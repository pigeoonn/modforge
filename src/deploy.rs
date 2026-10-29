//! Mod library for any game, Vortex-style: mods are kept untouched in a staging folder and
//! *deployed* into the game folder as hard links (copies when a link is impossible). The game
//! folder is never the source of truth, so disabling, reordering or removing a mod is always a
//! clean, reversible operation:
//!
//! * every game file a mod would overwrite is moved to `backups/` first and restored on purge;
//! * a journal (`deployed.json`) records exactly what was placed, so purge removes only what
//!   Gantry itself put there and leaves files the game or the user replaced since;
//! * later mods win conflicts (the order is the priority), and paths match the game folder's
//!   existing case, because Windows games are case-insensitive but Linux file systems are not;
//! * deployment is refused for games that ship an anti-cheat unless the user insists.
//!
//! Staging lives next to the game (`<library>/.gantry/<game folder>/`) so hard links work and
//! survive reinstalling the game; when that folder is not writable it falls back to the user's
//! data directory.

use crate::fsutil::{self, move_file};
use crate::nexus::NexusRef;
use crate::thunderstore::ThunderRef;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::sync::{Mutex, MutexGuard};
use walkdir::WalkDir;

/// Library operations are rare and quick; one process-wide lock keeps them strictly ordered.
static LOCK: Mutex<()> = Mutex::new(());

fn lock() -> MutexGuard<'static, ()> {
    LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

/// DLL names Windows games load from their own folder and that mods commonly impersonate
/// (ASI loaders, BepInEx's winhttp, ReShade's dxgi, ...). Wine only prefers a game-folder copy
/// over its built-in one when told to, so these get a `WINEDLLOVERRIDES` entry.
const PROXY_DLLS: &[&str] = &[
    "dinput8", "version", "winmm", "winhttp", "d3d8", "d3d9", "d3d10", "d3d11", "dxgi", "xinput1_3",
    "xinput1_4", "xinput9_1_0", "dsound", "dwmapi", "opengl32", "ddraw", "wininet", "msimg32", "iphlpapi",
    "bink2w64", "binkw64", "binkw32",
];

const ANTI_CHEAT: &[(&str, &str)] = &[
    ("easyanticheat", "Easy Anti-Cheat"),
    ("easyanticheat_eos", "Easy Anti-Cheat"),
    ("easyanticheat_eos_setup.exe", "Easy Anti-Cheat"),
    ("start_protected_game.exe", "Easy Anti-Cheat"),
    ("battleye", "BattlEye"),
    ("beservice.exe", "BattlEye"),
    ("beservice_x64.exe", "BattlEye"),
];

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct LibMod {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub version: String,
    pub enabled: bool,
    /// Folder inside the game folder the mod's files are laid onto ("" = the game root).
    #[serde(default)]
    pub target: String,
    /// Where it came from (archive name, Nexus link...).
    #[serde(default)]
    pub source: String,
    /// Set when the mod was downloaded from Nexus Mods (enables update checks).
    #[serde(default)]
    pub nexus: Option<NexusRef>,
    /// Set when the mod came from Thunderstore (enables update checks and dependency handling).
    #[serde(default)]
    pub thunderstore: Option<ThunderRef>,
    /// Set when the mod came from a GitHub release (enables update checks).
    #[serde(default)]
    pub github: Option<crate::github::GithubRef>,
    /// Set when the mod came from mod.io.
    #[serde(default)]
    pub modio: Option<crate::modio::ModioRef>,
    #[serde(default)]
    pub added_ms: u64,
    #[serde(default)]
    pub size: u64,
    /// Paths relative to the mod's own folder, `/`-separated.
    pub files: Vec<String>,
    /// Readmes and install instructions that came with the mod (kept in `notes/`, never deployed).
    #[serde(default)]
    pub notes: Vec<String>,
    /// How the download was checked against what the site published: "md5", or "" when it was not.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub verified: String,
    /// Program files inside the mod (.exe, .bat, ...): worth knowing before you trust it.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub programs: Vec<String>,
    /// "" = tell me about updates, "auto" = install them when the game is closed, "never" = keep this version.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub update_rule: String,
    /// The answers given to the mod's own installer script (FOMOD), so an update can reuse them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fomod: Option<serde_json::Value>,
    /// How its place in the game was decided when it was added: "names" (the game has files of the
    /// same names there), "readme" (its own instructions), or empty (its folder layout fitted).
    #[serde(default)]
    pub placed_by: String,
    /// The user's own remark about this mod.
    #[serde(default)]
    pub note: String,
    /// Thunderstore packages (`Owner-Name`) this mod needs, as its author listed them.
    #[serde(default)]
    pub depends_on: Vec<String>,
    /// A label the user gave to sort the list ("Visual", "Gameplay"...); it does not change the load order.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub group: String,
}

#[derive(Default, Serialize, Deserialize)]
struct LibFile {
    /// Load order: later mods win file conflicts.
    #[serde(default)]
    mods: Vec<LibMod>,
    /// Changes were made that are not deployed yet.
    #[serde(default)]
    dirty: bool,
    /// Named sets of mods: `(id, enabled)` in load order.
    #[serde(default)]
    profiles: BTreeMap<String, Vec<(String, bool)>>,
    /// How the game starts with each profile (kept apart so older files and versions still read `profiles`).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    profile_launch: BTreeMap<String, ProfileLaunch>,
    /// How things were before the last change in the list (one step back).
    #[serde(default)]
    undo: Option<Snapshot>,
    /// Earlier states of the list, oldest first.
    #[serde(default)]
    history: Vec<Snapshot>,
    /// Files whose winner the user chose: lowercase game path → mod id.
    #[serde(default)]
    pins: BTreeMap<String, String>,
    /// Each profile keeps its own saves (see `profsaves`).
    #[serde(default)]
    separate_saves: bool,
    /// The profile whose saves are in the game's folders now ("" = not known yet).
    #[serde(default)]
    saves_of: String,
    /// Where the saves go back to after a session without mods.
    #[serde(default)]
    saves_return: String,
}

/// The state of the list at one moment: `(id, enabled, target)` in load order.
#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
struct Snapshot {
    label: String,
    mods: Vec<(String, bool, String)>,
    /// When it was taken (ms since the epoch); 0 for ones saved before history existed.
    #[serde(default)]
    ts: u64,
}

/// Copies of replaced saves are kept this long (30 days).
const SAVE_BACKUP_KEEP_MS: u64 = 30 * 86_400_000;

/// What a check of a library found.
#[derive(Debug, Default, Serialize, PartialEq, Eq)]
pub struct Verify {
    pub mods: usize,
    /// "Mod name: path" of staged files that are gone.
    pub missing: Vec<String>,
    /// Folders in the staging area that belong to no mod of the library.
    pub orphans: Vec<String>,
    pub mods_bytes: u64,
    pub trash_bytes: u64,
    /// Size of the deployed files that are hard links: space the game folder does not take twice.
    pub linked_bytes: u64,
    /// Profile saves and the copies of what they replaced.
    pub saves_bytes: u64,
    /// Part of `saves_bytes` that is older than the keep time (see `clean_old_save_backups`).
    pub old_saves_bytes: u64,
}

fn dir_bytes(p: &Path) -> u64 {
    WalkDir::new(p).follow_links(false).into_iter().flatten().filter(|e| e.file_type().is_file()).filter_map(|e| e.metadata().ok()).map(|m| m.len()).sum()
}

/// What a profile remembers besides its mods: how the game is started with it.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ProfileLaunch {
    #[serde(default)]
    pub launch_args: String,
    #[serde(default)]
    pub env_vars: String,
    /// The compatibility tool (for games Gantry starts itself; Steam keeps its own choice).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub tool: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub trainer: String,
}

/// One mod of a shared setup: what it is and where to get it again, never its files.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct SpecMod {
    pub name: String,
    #[serde(default)]
    pub version: String,
    pub enabled: bool,
    #[serde(default)]
    pub target: String,
    #[serde(default)]
    pub source: String,
    #[serde(default)]
    pub nexus: Option<NexusRef>,
    #[serde(default)]
    pub thunderstore: Option<ThunderRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub github: Option<crate::github::GithubRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub modio: Option<crate::modio::ModioRef>,
    /// Folder inside a modpack file that holds this mod's own files (only for mods that cannot be fetched again).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub files: String,
    /// The group the sender gave it, and the sender's note.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub group: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub note: String,
}

/// A whole mod setup as a small file that can be shared and imported again.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ProfileSpec {
    pub gantry_profile: u32,
    pub game: String,
    pub mods: Vec<SpecMod>,
    /// A modpack's own name and a few words from its maker.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub name: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub note: String,
}

fn same_mod(m: &LibMod, s: &SpecMod) -> bool {
    if let (Some(a), Some(b)) = (&m.thunderstore, &s.thunderstore) {
        return a.namespace.eq_ignore_ascii_case(&b.namespace) && a.name.eq_ignore_ascii_case(&b.name);
    }
    if let (Some(a), Some(b)) = (&m.nexus, &s.nexus) {
        return a.game == b.game && a.mod_id == b.mod_id;
    }
    if let (Some(a), Some(b)) = (&m.github, &s.github) {
        return a.same_file_as(b);
    }
    if let (Some(a), Some(b)) = (&m.modio, &s.modio) {
        return a.mod_id == b.mod_id;
    }
    // no source in common: the same name is the same mod when one side does not know where it came from
    let local = m.thunderstore.is_none() && m.nexus.is_none() && m.github.is_none() && m.modio.is_none();
    (local || !s.fetchable()) && m.name.eq_ignore_ascii_case(&s.name)
}

impl SpecMod {
    /// Can it be downloaded again from where it came from?
    pub fn fetchable(&self) -> bool {
        self.thunderstore.is_some() || self.nexus.is_some() || self.github.is_some() || self.modio.is_some()
    }
}

/// A library's mods as a shared setup.
fn spec_of(lf: &LibFile, game: &str) -> ProfileSpec {
    ProfileSpec {
        gantry_profile: 1,
        game: game.to_string(),
        mods: lf
            .mods
            .iter()
            .map(|m| SpecMod {
                name: m.name.clone(),
                version: m.version.clone(),
                enabled: m.enabled,
                target: m.target.clone(),
                source: m.source.clone(),
                nexus: m.nexus.clone(),
                thunderstore: m.thunderstore.clone(),
                github: m.github.clone(),
                modio: m.modio.clone(),
                group: m.group.clone(),
                note: m.note.clone(),
                ..Default::default()
            })
            .collect(),
        name: String::new(),
        note: String::new(),
    }
}

/// The setup a backed-up mod list (`export_meta`, as a backup or the sync folder keeps it) stands for.
pub fn spec_from_meta(v: &Value, game: &str) -> Option<ProfileSpec> {
    serde_json::from_value::<LibFile>(v.clone()).ok().map(|lf| spec_of(&lf, game))
}

/// How many earlier states of the list are kept to go back to.
const HISTORY_KEEP: usize = 25;

impl LibFile {
    /// Remember how things were before a change: for one-step undo and for the history list.
    fn snapshot(&mut self, label: String) {
        let snap = Snapshot { label, mods: self.mods.iter().map(|m| (m.id.clone(), m.enabled, m.target.clone())).collect(), ts: fsutil::now_ms() };
        if self.history.last().map(|h| h.mods != snap.mods).unwrap_or(true) {
            self.history.push(snap.clone());
            if self.history.len() > HISTORY_KEEP {
                self.history.remove(0);
            }
        }
        self.undo = Some(snap);
    }

    /// Put the list back as it was in `snap`: mods that still exist take its enabled state, folder and place.
    fn apply(&mut self, snap: &Snapshot) {
        let pos: HashMap<&str, usize> = snap.mods.iter().enumerate().map(|(i, (id, ..))| (id.as_str(), i)).collect();
        for m in &mut self.mods {
            if let Some((_, on, target)) = snap.mods.iter().find(|(id, ..)| *id == m.id) {
                m.enabled = *on;
                m.target = target.clone();
            }
        }
        self.mods.sort_by_key(|m| pos.get(m.id.as_str()).copied().unwrap_or(usize::MAX));
        self.dirty = true;
    }
}

#[derive(Clone, Serialize, Deserialize)]
struct Placed {
    /// Destination relative to the game folder (canonical case).
    rel: String,
    mod_id: String,
    /// The file inside the mod's staging folder.
    file: String,
    /// `hardlink`, `copy`, or `pending` (journalled, not placed yet).
    kind: String,
    backup: bool,
    #[serde(default)]
    size: u64,
    #[serde(default)]
    mtime_ns: i64,
}

#[derive(Default, Serialize, Deserialize)]
struct Journal {
    entries: Vec<Placed>,
    /// Directories Gantry created for the deployment, parents first.
    created_dirs: Vec<String>,
}

/// A mod that was updated lately and can still go back to the version before.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct RecentUpdate {
    pub id: String,
    pub name: String,
    pub version: String,
    pub old_version: String,
    pub dir: String,
    pub at_ms: u64,
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct Conflict {
    pub path: String,
    /// Names of every mod providing the file, in load order.
    pub mods: Vec<String>,
    /// Their ids, in the same order.
    pub ids: Vec<String>,
    pub winner: String,
    pub winner_id: String,
    /// The user picked the winner (instead of the load order deciding).
    pub pinned: bool,
}

#[derive(Debug, Default, Serialize, PartialEq, Eq)]
pub struct DeployReport {
    pub linked: usize,
    pub copied: usize,
    pub backed_up: usize,
    pub conflicts: usize,
    pub skipped: Vec<String>,
    pub overrides: Vec<String>,
    /// Enabled mods whose files sit where the game has nothing of those names, but do elsewhere.
    pub misplaced: usize,
}

#[derive(Debug, Default, Serialize, PartialEq, Eq)]
pub struct PurgeReport {
    pub removed: usize,
    pub restored: usize,
    /// Files that changed since deployment (game update, user edit) and were left alone.
    pub kept: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ConflictResolutionReport {
    pub resolved_conflicts: usize,
    pub reordered_mods: Vec<String>,
    pub auto_pinned_files: Vec<(String, String)>,
    pub strategy: String,
}

struct Analysis {
    files: Vec<PathBuf>,
    cleaned: Vec<Option<String>>,
    note_idx: Vec<usize>,
    content_idx: Vec<usize>,
    smart: Option<Vec<Option<String>>>,
    target: String,
    placed_by: &'static str,
}

/// What adding a mod would do, shown before it happens when Gantry had to guess.
#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct Preview {
    /// "names", "readme", or empty when the mod's own folders fit the game.
    pub placed_by: String,
    pub target: String,
    pub files: usize,
    /// Game folder → how many files go there.
    pub folders: Vec<(String, usize)>,
    pub notes: usize,
    /// What the mod looks like inside (its top folders and kinds of files), so an answer about where it goes can be
    /// reused for the next mod that looks the same.
    pub shape: String,
}

struct Planned {
    dest: String,
    mod_idx: usize,
    file: String,
}

struct Plan {
    files: Vec<Planned>,
    conflicts: Vec<Conflict>,
}

pub struct Library {
    root: PathBuf,
    game_dir: PathBuf,
    /// Text from the mod's web page, kept as a note of the next mod added (and read for install hints).
    page_note: Option<String>,
    /// What the game's support entry says about where files of a kind go: `(extensions, folder)`.
    rules: Vec<(Vec<String>, String)>,
}

/// Documents and pictures that come with mods; they are not game files.
const DOC_EXT: &[&str] = &["txt", "md", "pdf", "rtf", "doc", "docx", "url", "html", "htm", "nfo", "jpg", "jpeg", "png", "gif"];
/// Programs and libraries: placed exactly where the mod author put them.
const CODE_EXT: &[&str] = &["dll", "exe", "asi", "so", "bat", "cmd", "sh"];

/// Files that run on their own (a mod rarely needs one; a loader or trainer may).
pub fn is_program(rel: &str) -> bool {
    Path::new(rel).extension().map(|e| ["exe", "bat", "cmd", "ps1", "vbs", "msi", "scr", "com", "jar"].contains(&e.to_string_lossy().to_lowercase().as_str())).unwrap_or(false)
}

/// A top-level text that explains the mod (readme, install instructions...) rather than being part of it.
fn is_notes_name(rel: &str) -> bool {
    let n = rel.to_lowercase();
    let ext = Path::new(&n).extension().map(|e| e.to_string_lossy().to_string()).unwrap_or_default();
    !rel.contains('/')
        && ["txt", "md", "nfo", "rtf", "html", "htm", "pdf"].contains(&ext.as_str())
        && ["readme", "read me", "read_me", "install", "instruction", "how to", "howto", "changelog", "credits", "licen", "notes"].iter().any(|k| n.contains(k))
}

fn safe_name(s: &str) -> String {
    let t: String = s.chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' { c } else { '_' }).collect();
    if t.is_empty() { "game".into() } else { t }
}

fn slug(name: &str) -> String {
    let s: String = name
        .chars()
        .map(|c| if c.is_alphanumeric() { c.to_ascii_lowercase() } else { '-' })
        .collect::<String>()
        .split('-')
        .filter(|p| !p.is_empty())
        .collect::<Vec<_>>()
        .join("-");
    let s: String = s.chars().take(40).collect();
    if s.is_empty() { "mod".into() } else { s }
}

/// `Cool Mod-123-1-2-1700000000.zip` (Nexus naming) → (`Cool Mod`, `1.2`).
pub fn friendly_name(file_name: &str) -> (String, String) {
    if let Some(n) = crate::nexus::parse_download_name(file_name) {
        return (if n.name.is_empty() { "Mod".into() } else { n.name }, n.version);
    }
    let stem = Path::new(file_name).file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
    let stem = stem.trim_end_matches(".tar").to_string();
    let name = if stem.contains(' ') { stem } else { stem.replace('_', " ") };
    let name = name.trim().to_string();
    (if name.is_empty() { "Mod".into() } else { name }, String::new())
}

/// `mods/<name>` for a REDmod, the name coming from its `info.json`.
fn redmod_folder(info_json: &[u8]) -> String {
    let named = serde_json::from_slice::<Value>(info_json).ok().and_then(|v| v.get("name")?.as_str().map(str::to_string)).unwrap_or_default();
    let name: String = named.chars().filter(|c| c.is_alphanumeric() || " -_.".contains(*c)).collect::<String>().trim().trim_matches('.').to_string();
    format!("mods/{}", if name.is_empty() { "REDmod" } else { name.as_str() })
}

fn is_junk(rel: &Path) -> bool {
    let comp_is = |c: Component, n: &str| matches!(c, Component::Normal(x) if x.to_string_lossy().eq_ignore_ascii_case(n));
    rel.components().any(|c| comp_is(c, "__MACOSX"))
        || rel.file_name().map(|n| n.to_string_lossy().to_lowercase()).is_none_or(|n| n == ".ds_store" || n == "thumbs.db" || n == "desktop.ini")
}

fn clean_rel(s: &str) -> Option<String> {
    let mut parts = Vec::new();
    for c in Path::new(s).components() {
        match c {
            Component::Normal(n) => parts.push(n.to_string_lossy().to_string()),
            Component::CurDir | Component::RootDir => {}
            _ => return None,
        }
    }
    Some(parts.join("/"))
}

/// Which anti-cheat the game ships, if any.
pub fn anti_cheat(game_dir: &Path) -> Option<&'static str> {
    let skip = |n: &str| matches!(n, ".gantry" | "content" | "saved" | "paks" | "movies" | "localization");
    WalkDir::new(game_dir)
        .max_depth(4)
        .into_iter()
        .filter_entry(|e| !(e.depth() > 0 && e.file_type().is_dir() && skip(&e.file_name().to_string_lossy().to_lowercase())))
        .flatten()
        .find_map(|e| {
            let n = e.file_name().to_string_lossy().to_lowercase();
            ANTI_CHEAT.iter().find(|(k, _)| *k == n).map(|(_, v)| *v)
        })
}

/// Remove `dir` and every folder below it that is empty.
fn prune_empty(dir: &Path) {
    if let Ok(rd) = fs::read_dir(dir) {
        for e in rd.flatten() {
            if e.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                prune_empty(&e.path());
            }
        }
    }
    let _ = fs::remove_dir(dir);
}


/// Case-insensitive path resolution against the game folder, remembering what the plan has
/// already decided so two mods spelling a folder differently land in the same one.
struct Resolver<'a> {
    root: &'a Path,
    canon: HashMap<String, String>,
    dirs: HashMap<PathBuf, (HashMap<String, String>, HashSet<String>)>,
}

impl<'a> Resolver<'a> {
    fn new(root: &'a Path) -> Self {
        Self { root, canon: HashMap::new(), dirs: HashMap::new() }
    }

    fn lookup(&mut self, parent: &Path, name: &str) -> Option<String> {
        let abs = self.root.join(parent);
        let (lower, exact) = self.dirs.entry(parent.to_path_buf()).or_insert_with(|| {
            let mut lower = HashMap::new();
            let mut exact = HashSet::new();
            if let Ok(rd) = fs::read_dir(&abs) {
                for e in rd.flatten() {
                    let n = e.file_name().to_string_lossy().to_string();
                    lower.entry(n.to_lowercase()).or_insert_with(|| n.clone());
                    exact.insert(n);
                }
            }
            (lower, exact)
        });
        if exact.contains(name) {
            return Some(name.to_string());
        }
        lower.get(&name.to_lowercase()).cloned()
    }

    fn resolve(&mut self, rel: &Path) -> PathBuf {
        let mut cur = PathBuf::new();
        let mut key = String::new();
        for c in rel.components() {
            let Component::Normal(n) = c else { continue };
            let n = n.to_string_lossy().to_string();
            key.push('/');
            key.push_str(&n.to_lowercase());
            let name = match self.canon.get(&key) {
                Some(k) => k.clone(),
                None => {
                    let found = self.lookup(&cur, &n).unwrap_or_else(|| n.clone());
                    self.canon.insert(key.clone(), found.clone());
                    found
                }
            };
            cur.push(name);
        }
        cur
    }
}

impl Library {
    /// Where the library of `game_dir` lives (an existing one wins, so moving never forks it).
    pub fn locate(game_dir: &Path, data_dir: &Path, appid: &str) -> Library {
        let fallback = data_dir.join("library").join(safe_name(appid));
        let preferred = match (game_dir.parent(), game_dir.file_name()) {
            (Some(p), Some(n)) => Some(p.join(".gantry").join(n)),
            _ => None,
        };
        let root = match preferred {
            Some(pr) if pr.is_dir() => pr,
            _ if fallback.is_dir() => fallback,
            Some(pr) if game_dir.parent().map(fsutil::can_write).unwrap_or(false) => pr,
            _ => fallback,
        };
        Library { root, game_dir: game_dir.to_path_buf(), page_note: None, rules: Vec::new() }
    }

    /// Where the game's support entry says files of a kind go (used when nothing else decides).
    pub fn with_rules(mut self, rules: Vec<(Vec<String>, String)>) -> Library {
        self.rules = rules;
        self
    }

    /// The next mod added also gets this text as a note named "Web page".
    pub fn with_page_note(mut self, text: String) -> Library {
        self.page_note = Some(text).filter(|t| !t.trim().is_empty());
        self
    }

    pub fn open(root: PathBuf, game_dir: PathBuf) -> Library {
        Library { root, game_dir, page_note: None, rules: Vec::new() }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    fn lib_path(&self) -> PathBuf {
        self.root.join("library.json")
    }

    fn journal_path(&self) -> PathBuf {
        self.root.join("deployed.json")
    }

    fn notes_dir(&self, id: &str) -> PathBuf {
        self.root.join("mods").join(id).join("notes")
    }

    /// The folder (spelled the way the game does) a mod's instructions point to, when the game has it.
    fn resolve_hint(&self, hint: &str) -> Option<String> {
        let rel = Resolver::new(&self.game_dir).resolve(Path::new(hint));
        if !rel.as_os_str().is_empty() && self.game_dir.join(&rel).is_dir() {
            return Some(rel.to_string_lossy().replace('\\', "/"));
        }
        if !hint.contains('/') {
            let parent = self.find_parent_of(hint)?;
            let rel = Resolver::new(&self.game_dir).resolve(&Path::new(&parent).join(hint));
            return self.game_dir.join(&rel).is_dir().then(|| rel.to_string_lossy().replace('\\', "/"));
        }
        None
    }

    /// Readmes kept with a mod: `(file name, text)`, markup removed.
    pub fn notes(&self, id: &str) -> Vec<(String, String)> {
        let Some(m) = self.load().mods.into_iter().find(|m| m.id == id) else { return Vec::new() };
        m.notes
            .iter()
            .filter_map(|n| {
                let path = self.notes_dir(id).join(n);
                let bytes = fs::read(path).ok()?;
                if n.to_lowercase().ends_with(".pdf") {
                    return None;
                }
                Some((n.clone(), crate::readme::plain(&String::from_utf8_lossy(&bytes[..bytes.len().min(200_000)]))))
            })
            .collect()
    }

    /// SHA-256 of each program file of a mod (at most the 12 the mod lists), as `(path in the mod, fingerprint)`.
    /// Files that cannot be read are left out.
    pub fn program_hashes(&self, id: &str) -> Result<Vec<(String, String)>, String> {
        let m = self.load().mods.into_iter().find(|m| m.id == id).ok_or("Unknown mod.")?;
        Ok(m.programs.iter().filter_map(|f| Some((f.clone(), crate::hashing::sha256_file(&self.mod_files_dir(id).join(f)).ok()?))).collect())
    }

    pub fn mod_files_dir(&self, id: &str) -> PathBuf {
        self.root.join("mods").join(id).join("files")
    }

    /// Every file of the enabled mods: which mod, where it lands in the game, where it is kept.
    pub fn enabled_mod_files(&self) -> Vec<crate::clashes::ModFile> {
        self.load()
            .mods
            .into_iter()
            .filter(|m| m.enabled)
            .flat_map(|m| {
                let dir = self.mod_files_dir(&m.id);
                m.files
                    .iter()
                    .map(|f| crate::clashes::ModFile { mod_name: m.name.clone(), dest: if m.target.is_empty() { f.clone() } else { format!("{}/{}", m.target, f) }, src: dir.join(f) })
                    .collect::<Vec<_>>()
            })
            .collect()
    }

    fn load(&self) -> LibFile {
        // never silently start over: what could not be read is kept, and the copy from before the last change is used
        fsutil::read_json_kept(&self.lib_path()).value().unwrap_or_default()
    }

    fn save(&self, f: &LibFile) -> Result<(), String> {
        let json = serde_json::to_string_pretty(f).map_err(|e| e.to_string())?;
        fsutil::atomic_write_kept(&self.lib_path(), json).map_err(|e| format!("Could not save the mod library: {e}"))
    }

    /// Read the list and the journal once, so a damaged one is found (and recovered) now rather than in the middle of
    /// something. Returns how many mods the list has.
    pub fn self_check(&self) -> usize {
        let _ = self.journal();
        self.load().mods.len()
    }

    fn journal(&self) -> Journal {
        fsutil::read_json_kept(&self.journal_path()).value().unwrap_or_default()
    }

    fn save_journal(&self, j: &Journal) -> Result<(), String> {
        let json = serde_json::to_string(j).map_err(|e| e.to_string())?;
        fsutil::atomic_write_kept(&self.journal_path(), json).map_err(|e| format!("Could not write the deploy journal: {e}"))
    }

    pub fn mods(&self) -> Vec<LibMod> {
        self.load().mods
    }

    pub fn is_dirty(&self) -> bool {
        self.load().dirty
    }

    // ───────────────────────────── adding mods ─────────────────────────────

    fn exists_ci(&self, rel_dir: &Path, name: &str) -> bool {
        let want = name.to_lowercase();
        fs::read_dir(self.game_dir.join(rel_dir))
            .map(|rd| rd.flatten().any(|e| e.file_name().to_string_lossy().to_lowercase() == want))
            .unwrap_or(false)
    }

    /// The shallowest game sub-folder (up to two levels down) that contains a folder called
    /// `name`, e.g. `Binaries` → `Subnautica2` when the game keeps it in `Subnautica2/Binaries`.
    fn find_parent_of(&self, name: &str) -> Option<String> {
        let mut level: Vec<PathBuf> = vec![PathBuf::new()];
        let mut visited = 0usize;
        for depth in 0..3 {
            let mut next = Vec::new();
            for rel in &level {
                let Ok(rd) = fs::read_dir(self.game_dir.join(rel)) else { continue };
                let mut dirs: Vec<String> = rd
                    .flatten()
                    .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
                    .map(|e| e.file_name().to_string_lossy().to_string())
                    .filter(|n| !n.starts_with('.'))
                    .collect();
                dirs.sort();
                if depth > 0 && dirs.iter().any(|d| d.eq_ignore_ascii_case(name)) {
                    return Some(rel.to_string_lossy().replace('\\', "/"));
                }
                for d in dirs {
                    visited += 1;
                    if visited > 4000 {
                        return None;
                    }
                    next.push(rel.join(d));
                }
            }
            level = next;
        }
        None
    }

    /// The game's `Paks` folder (relative, `/`-separated) when it is an Unreal game, preferring the
    /// one under `Content/`.
    pub fn find_paks_rel(&self) -> Option<String> {
        let mut found: Vec<String> = Vec::new();
        let mut level: Vec<PathBuf> = vec![PathBuf::new()];
        for _ in 0..6 {
            let mut next = Vec::new();
            for rel in &level {
                let Ok(rd) = fs::read_dir(self.game_dir.join(rel)) else { continue };
                for e in rd.flatten() {
                    if !e.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                        continue;
                    }
                    let name = e.file_name().to_string_lossy().to_string();
                    if name.starts_with('.') {
                        continue;
                    }
                    let child = rel.join(&name);
                    if name.eq_ignore_ascii_case("paks") {
                        found.push(child.to_string_lossy().replace('\\', "/"));
                    } else {
                        next.push(child);
                    }
                }
            }
            level = next;
        }
        found.sort_by_key(|p| (!p.to_lowercase().contains("content"), p.len(), p.clone()));
        found.into_iter().next()
    }

    /// How many leading folders of the archive to drop and which game folder to lay it onto.
    /// A REDmod is an `info.json` next to `archives/`, `tweaks/`, `scripts/` or similar. The game only looks for it in
    /// `mods/<its name>/`, so a mod zipped without that folder is given it. Only for games that have Cyberpunk's folders.
    fn redmod_target(&self, stage: &Path, files: &[PathBuf], cleaned: &[Option<String>]) -> Option<String> {
        if !(self.exists_ci(Path::new(""), "r6") || self.exists_ci(Path::new(""), "REDprelauncher.exe")) {
            return None;
        }
        let info = cleaned.iter().position(|c| c.as_deref().is_some_and(|c| c.eq_ignore_ascii_case("info.json")))?;
        let has_content = cleaned.iter().flatten().any(|c| {
            let top = c.split('/').next().unwrap_or("").to_lowercase();
            c.contains('/') && ["archives", "tweaks", "scripts", "customsounds", "localization"].contains(&top.as_str())
        });
        if !has_content {
            return None;
        }
        Some(redmod_folder(&fs::read(stage.join(&files[info])).unwrap_or_default()))
    }

    fn detect_layout(&self, files: &[PathBuf]) -> (usize, String) {
        // BepInEx (a Unity mod loader) installed or about to be: its folder names are never wrappers
        let has_bepinex = self.exists_ci(Path::new(""), "BepInEx") || self.load().mods.iter().any(|m| m.files.iter().any(|f| f.starts_with("BepInEx/")));
        let bepinex_dir = |n: &str| ["bepinex", "plugins", "patchers", "monomod", "config", "core"].contains(&n.to_lowercase().as_str());
        // Cyberpunk 2077: the folders its mods are laid out from, even before the first mod made them (a RED4ext plugin
        // zipped as `red4ext/plugins/...` must not lose its `red4ext`)
        let cyberpunk = self.exists_ci(Path::new("bin/x64"), "Cyberpunk2077.exe");
        let cyberpunk_dir = |n: &str| ["red4ext", "r6", "engine", "bin", "archive", "mods"].contains(&n.to_lowercase().as_str());

        // Bethesda games (Skyrim, Fallout, Starfield, Oblivion, etc.)
        let bethesda = crate::loaders::detect_bethesda(&self.game_dir).is_some() || self.exists_ci(Path::new(""), "Data");
        let bethesda_dir = |n: &str| ["data", "meshes", "textures", "scripts", "sound", "music", "interface", "skse", "f4se", "sfse", "strings", "seq", "shadersfx", "materials"].contains(&n.to_lowercase().as_str());

        // Godot games
        let godot = crate::loaders::detect_godot(&self.game_dir).is_some();
        let godot_dir = |n: &str| ["gdweave", "mods"].contains(&n.to_lowercase().as_str());

        // Source / GoldSrc games
        let source_info = crate::loaders::detect_source(&self.game_dir);
        let has_source = source_info.is_some();
        let source_dir = |n: &str| {
            let low = n.to_lowercase();
            ["custom", "addons", "materials", "models", "sound", "maps", "resource", "scripts", "cfg"].contains(&low.as_str())
                || source_info.as_ref().map(|s| s.game_dir_name.eq_ignore_ascii_case(n)).unwrap_or(false)
        };

        // RE Engine games (Resident Evil, Monster Hunter, Dragon's Dogma 2)
        let re_engine = crate::loaders::detect_re_engine(&self.game_dir).is_some() || self.exists_ci(Path::new(""), "reframework") || self.exists_ci(Path::new(""), "natives");
        let re_dir = |n: &str| ["reframework", "natives", "autorun", "plugins"].contains(&n.to_lowercase().as_str());

        // Witcher 3 games (REDengine 3)
        let witcher = crate::loaders::detect_witcher3(&self.game_dir).is_some();
        let witcher_dir = |n: &str| ["mods", "dlc", "content", "bin"].contains(&n.to_lowercase().as_str());

        let mut strip = 0usize;
        loop {
            let mut tops: BTreeSet<String> = BTreeSet::new();
            let mut loose = false;
            for f in files {
                let comps: Vec<_> = f.components().collect();
                if comps.len() > strip + 1 {
                    tops.insert(comps[strip].as_os_str().to_string_lossy().to_string());
                } else {
                    loose = true;
                }
            }
            if strip < 4 && tops.len() == 1 && !loose {
                let top = tops.iter().next().expect("one element").clone();
                if self.exists_ci(Path::new(""), &top)
                    || self.find_parent_of(&top).is_some()
                    || (has_bepinex && bepinex_dir(&top))
                    || (cyberpunk && cyberpunk_dir(&top))
                    || (bethesda && bethesda_dir(&top))
                    || (godot && godot_dir(&top))
                    || (has_source && source_dir(&top))
                    || (re_engine && re_dir(&top))
                    || (witcher && witcher_dir(&top))
                {
                    break; // a real game folder, keep it
                }
                strip += 1; // a wrapper folder named after the mod
                continue;
            }
            break;
        }
        // none of the top-level folders exist at the game root: maybe they live one level down
        let mut tops: BTreeSet<String> = BTreeSet::new();
        for f in files {
            let comps: Vec<_> = f.components().collect();
            if comps.len() > strip + 1 {
                tops.insert(comps[strip].as_os_str().to_string_lossy().to_string());
            }
        }
        let mut target = String::new();
        if !tops.is_empty() && !tops.iter().any(|t| self.exists_ci(Path::new(""), t)) {
            let mut votes: BTreeMap<String, usize> = BTreeMap::new();
            for t in &tops {
                if let Some(p) = self.find_parent_of(t) {
                    *votes.entry(p).or_default() += 1;
                }
            }
            if let Some((p, _)) = votes.into_iter().max_by(|a, b| a.1.cmp(&b.1).then_with(|| b.0.len().cmp(&a.0.len()))) {
                target = p;
            }
        }
        // BepInEx games (Unity): plugins belong in BepInEx/plugins, however the author zipped them
        if target.is_empty() && has_bepinex {
            let lower = |c: &std::path::Component| c.as_os_str().to_string_lossy().to_lowercase();
            let top_dirs: BTreeSet<String> = files.iter().filter_map(|f| f.components().nth(strip + 1).map(|_| lower(&f.components().nth(strip).unwrap()))).collect();
            let loose_dll = files.iter().any(|f| f.components().count() == strip + 1 && f.extension().map(|e| e.eq_ignore_ascii_case("dll")).unwrap_or(false));
            if top_dirs.contains("bepinex") {
                // already laid out from the game folder
            } else if ["plugins", "patchers", "monomod", "config", "core"].iter().any(|d| top_dirs.contains(*d)) {
                target = "BepInEx".into();
            } else if top_dirs.is_empty() && loose_dll {
                target = "BepInEx/plugins".into();
            }
        }
        // Bethesda games: loose plugins or asset folders belong in Data/
        if target.is_empty() && bethesda {
            let lower = |c: &std::path::Component| c.as_os_str().to_string_lossy().to_lowercase();
            let top_dirs: BTreeSet<String> = files.iter().filter_map(|f| f.components().nth(strip + 1).map(|_| lower(&f.components().nth(strip).unwrap()))).collect();
            let ext = |f: &PathBuf| f.extension().map(|e| e.to_string_lossy().to_lowercase()).unwrap_or_default();
            let has_bethesda_plugins = files.iter().any(|f| f.components().count() == strip + 1 && ["esp", "esm", "esl", "ba2", "bsa"].contains(&ext(f).as_str()));
            let has_bethesda_subdirs = ["meshes", "textures", "scripts", "sound", "music", "interface", "skse", "f4se", "sfse", "strings", "seq", "shadersfx", "materials"].iter().any(|d| top_dirs.contains(*d));
            if top_dirs.contains("data") {
                // already laid out from root with Data/
            } else if has_bethesda_plugins || has_bethesda_subdirs {
                target = "Data".into();
            }
        }
        // Godot games: loose .pck files or GDWeave mods
        if target.is_empty() && godot {
            let lower = |c: &std::path::Component| c.as_os_str().to_string_lossy().to_lowercase();
            let top_dirs: BTreeSet<String> = files.iter().filter_map(|f| f.components().nth(strip + 1).map(|_| lower(&f.components().nth(strip).unwrap()))).collect();
            let ext = |f: &PathBuf| f.extension().map(|e| e.to_string_lossy().to_lowercase()).unwrap_or_default();
            let loose_pck = files.iter().any(|f| f.components().count() == strip + 1 && ext(f) == "pck");
            if top_dirs.contains("gdweave") || top_dirs.contains("mods") {
                // already laid out
            } else if loose_pck {
                if self.exists_ci(Path::new(""), "mods") {
                    target = "mods".into();
                }
            }
        }
        // Source games: .vpk files or custom assets belong in <game>/custom (or <game>/addons for L4D2)
        if target.is_empty() {
            if let Some(ref s) = source_info {
                let gname = &s.game_dir_name;
                if !gname.is_empty() {
                    let ext = |f: &PathBuf| f.extension().map(|e| e.to_string_lossy().to_lowercase()).unwrap_or_default();
                    let loose_vpk = files.iter().any(|f| f.components().count() == strip + 1 && ext(f) == "vpk");
                    let lower = |c: &std::path::Component| c.as_os_str().to_string_lossy().to_lowercase();
                    let top_dirs: BTreeSet<String> = files.iter().filter_map(|f| f.components().nth(strip + 1).map(|_| lower(&f.components().nth(strip).unwrap()))).collect();
                    let is_custom_assets = ["materials", "models", "sound", "maps", "resource", "scripts"].iter().any(|d| top_dirs.contains(*d));

                    let subtarget = if gname.eq_ignore_ascii_case("left4dead2") { "addons" } else { "custom" };
                    if top_dirs.contains(&gname.to_lowercase()) {
                        // already rooted with game directory
                    } else if loose_vpk || is_custom_assets {
                        target = format!("{}/{}", gname, subtarget);
                    }
                }
            }
        }
        // Unreal games: loose PAK files belong in Content/Paks/~mods (LogicMods for UI/logic paks)
        if target.is_empty() {
            let loose: Vec<&PathBuf> = files.iter().filter(|f| f.components().count() == strip + 1).collect();
            let ext = |f: &PathBuf| f.extension().map(|e| e.to_string_lossy().to_lowercase()).unwrap_or_default();
            let paks_only = !loose.is_empty()
                && loose.len() == files.len()
                && loose.iter().all(|f| ["pak", "utoc", "ucas", "sig"].contains(&ext(f).as_str()))
                && loose.iter().any(|f| ext(f) == "pak");
            if paks_only {
                if let Some(paks) = self.find_paks_rel() {
                    let logic = loose.iter().find(|f| ext(f) == "pak").map(|f| crate::pak_goes_to_logic_mods(f)).unwrap_or(false);
                    target = format!("{}/{}", paks, if logic { "LogicMods" } else { "~mods" });
                }
            }
        }
        (strip, target)
    }

    /// Where the game keeps files named like these: folder (as the game spells it) per lowercase name,
    /// and every file path in lowercase. Files Gantry itself deployed do not count.
    fn name_index(&self) -> (HashMap<String, BTreeSet<String>>, HashSet<String>) {
        // files Gantry created do not count; files it put over an original do (the game has one there)
        let ours: HashSet<String> = self.journal().entries.iter().filter(|e| !e.backup).map(|e| e.rel.to_lowercase()).collect();
        let mut by_name: HashMap<String, BTreeSet<String>> = HashMap::new();
        let mut all = HashSet::new();
        let mut seen = 0usize;
        for e in WalkDir::new(&self.game_dir)
            .max_depth(4)
            .follow_links(false)
            .into_iter()
            .filter_entry(|e| e.depth() == 0 || !e.file_name().to_string_lossy().starts_with('.'))
            .flatten()
        {
            if !e.file_type().is_file() {
                continue;
            }
            seen += 1;
            if seen > 150_000 {
                break;
            }
            let Ok(rel) = e.path().strip_prefix(&self.game_dir) else { continue };
            let rel_s = rel.to_string_lossy().replace('\\', "/");
            if ours.contains(&rel_s.to_lowercase()) {
                continue;
            }
            let dir = rel.parent().map(|d| d.to_string_lossy().replace('\\', "/")).unwrap_or_default();
            by_name.entry(e.file_name().to_string_lossy().to_lowercase()).or_default().insert(dir);
            all.insert(rel_s.to_lowercase());
        }
        (by_name, all)
    }

    /// Mods that ship loose files (a pair of car models, a texture) often do not say where they go.
    /// When the game already has files with the same names, they belong beside them. Returns the new
    /// path of every file (`None` = a readme or picture that is not part of the game), or `None` when
    /// the paths already fit the game.
    fn smart_plan(&self, files: &[String]) -> Option<Vec<Option<String>>> {
        let ext = |f: &str| Path::new(f).extension().map(|e| e.to_string_lossy().to_lowercase()).unwrap_or_default();
        let is_doc = |f: &str| DOC_EXT.contains(&ext(f).as_str());
        // programs and libraries are placed where their author says: a proxy DLL at the root is a new file
        let is_code = |f: &str| CODE_EXT.contains(&ext(f).as_str());
        let content: Vec<&String> = files.iter().filter(|f| !is_doc(f)).collect();
        if content.is_empty() || content.iter().any(|f| is_code(f)) {
            return None;
        }
        let (by_name, all) = self.name_index();
        // Instructions that ship with a mod are not part of the game, wherever the game's files go
        let is_notes = |f: &str| is_notes_name(f);
        if content.iter().all(|f| all.contains(&f.to_lowercase())) {
            // every file already sits where the game has it
            return files.iter().any(|f| is_notes(f)).then(|| files.iter().map(|f| (!is_notes(f)).then(|| f.clone())).collect());
        }
        let mut dest: Vec<Option<String>> = files.iter().map(|f| Some(f.clone())).collect();
        let mut matched = 0usize;
        let mut votes: BTreeMap<String, usize> = BTreeMap::new();
        for (i, f) in files.iter().enumerate() {
            if is_doc(f) {
                continue;
            }
            let (dir, name) = match f.rsplit_once('/') {
                Some((d, n)) => (d.to_string(), n.to_string()),
                None => (String::new(), f.clone()),
            };
            let Some(dirs) = by_name.get(&name.to_lowercase()) else { continue };
            let pick = dirs.iter().find(|d| d.eq_ignore_ascii_case(&dir)).or(if dirs.len() == 1 { dirs.iter().next() } else { None });
            if let Some(d) = pick {
                matched += 1;
                *votes.entry(d.clone()).or_default() += 1;
                dest[i] = Some(if d.is_empty() { name } else { format!("{d}/{name}") });
            }
        }
        if matched == 0 || matched * 2 < content.len() {
            return None;
        }
        // newcomers of the same kind as the matched files (an extra car model) go to the same folder
        let main_dir = votes.into_iter().max_by(|a, b| a.1.cmp(&b.1).then_with(|| b.0.cmp(&a.0))).map(|v| v.0).unwrap_or_default();
        let kinds: HashSet<String> = files
            .iter()
            .enumerate()
            .filter(|(i, f)| !is_doc(f) && dest[*i].as_deref() != Some(f.as_str()))
            .map(|(_, f)| ext(f))
            .collect();
        for (i, f) in files.iter().enumerate() {
            let moved = dest[i].as_deref() != Some(f.as_str());
            if !is_doc(f) && !moved && !all.contains(&f.to_lowercase()) && kinds.contains(&ext(f)) {
                let name = f.rsplit('/').next().unwrap_or(f);
                dest[i] = Some(if main_dir.is_empty() { name.to_string() } else { format!("{main_dir}/{name}") });
            }
        }
        // two files may never land on one path
        let mut used = HashSet::new();
        for (i, f) in files.iter().enumerate() {
            if let Some(d) = dest[i].clone() {
                if !used.insert(d.to_lowercase()) {
                    dest[i] = Some(f.clone());
                    used.insert(f.to_lowercase());
                }
            }
        }
        let moved = files.iter().zip(&dest).any(|(f, d)| d.as_deref() != Some(f.as_str()));
        if !moved {
            return None;
        }
        // a readme or picture at the top of the mod is not part of the game
        for (i, f) in files.iter().enumerate() {
            if is_doc(f) && !f.contains('/') {
                dest[i] = None;
            }
        }
        Some(dest)
    }

    /// Is everything the library promises still on disk? Also how much room it takes.
    pub fn verify(&self) -> Verify {
        let lf = self.load();
        let mut v = Verify { mods: lf.mods.len(), trash_bytes: dir_bytes(&self.root.join("trash")), ..Default::default() };
        for m in &lf.mods {
            let dir = self.mod_files_dir(&m.id);
            for f in &m.files {
                if !dir.join(f).is_file() {
                    v.missing.push(format!("{}: {}", m.name, f));
                }
            }
        }
        v.missing.truncate(200);
        let known: HashSet<String> = lf.mods.iter().map(|m| m.id.clone()).collect();
        for e in fs::read_dir(self.root.join("mods")).into_iter().flatten().flatten() {
            let name = e.file_name().to_string_lossy().to_string();
            if e.path().is_dir() && !known.contains(&name) {
                v.orphans.push(name);
            }
        }
        v.orphans.sort();
        v.mods_bytes = dir_bytes(&self.root.join("mods"));
        v.linked_bytes = self.journal().entries.iter().filter(|e| e.kind == "hardlink").map(|e| e.size).sum();
        v.saves_bytes = dir_bytes(&self.root.join("profile_saves"));
        v.old_saves_bytes = self.old_save_backups(SAVE_BACKUP_KEEP_MS).iter().map(|p| dir_bytes(p)).sum();
        v
    }

    /// Copies of saves that a profile switch replaced, older than `keep_ms`.
    fn old_save_backups(&self, keep_ms: u64) -> Vec<PathBuf> {
        let now = fsutil::now_ms();
        fs::read_dir(self.root.join("profile_saves/_replaced"))
            .into_iter()
            .flatten()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().parse::<u64>().map(|t| now.saturating_sub(t) > keep_ms).unwrap_or(false))
            .map(|e| e.path())
            .collect()
    }

    /// Delete the copies of replaced saves that are older than the keep time. Returns the bytes freed.
    pub fn clean_old_save_backups(&self) -> u64 {
        let _g = lock();
        self.old_save_backups(SAVE_BACKUP_KEEP_MS).into_iter().map(|p| {
            let n = dir_bytes(&p);
            let _ = fs::remove_dir_all(&p);
            n
        }).sum()
    }

    /// Move staging folders of no known mod to the trash. Returns how many.
    pub fn clean_orphans(&self) -> Result<usize, String> {
        let _g = lock();
        let known: HashSet<String> = self.load().mods.iter().map(|m| m.id.clone()).collect();
        let mut n = 0;
        for e in fs::read_dir(self.root.join("mods")).into_iter().flatten().flatten() {
            let name = e.file_name().to_string_lossy().to_string();
            if e.path().is_dir() && !known.contains(&name) {
                let to = self.root.join("trash").join(format!("{name}-{}", fsutil::unique_token()));
                fs::create_dir_all(self.root.join("trash")).map_err(|e| e.to_string())?;
                fs::rename(e.path(), to).map_err(|e| format!("Could not move {name} to the trash: {e}"))?;
                n += 1;
            }
        }
        Ok(n)
    }

    /// Mods removed earlier that can still be brought back: `(trash folder, name, version, size, removed_ms)`.
    pub fn trash_list(&self) -> Vec<(String, String, String, u64, u64)> {
        let mut out: Vec<(String, String, String, u64, u64)> = fs::read_dir(self.root.join("trash"))
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(|e| {
                let v: Value = serde_json::from_slice(&fs::read(e.path().join("mod.json")).ok()?).ok()?;
                let m: LibMod = serde_json::from_value(v["mod"].clone()).ok()?;
                e.path().join("files").is_dir().then(|| (e.file_name().to_string_lossy().to_string(), m.name, m.version, m.size, v["removed_ms"].as_u64().unwrap_or(0)))
            })
            .collect();
        out.sort_by_key(|t| std::cmp::Reverse(t.4));
        out
    }

    /// Bring a removed mod back (switched on, last in the order). Returns its name.
    pub fn restore_trash(&self, dir: &str) -> Result<String, String> {
        let _g = lock();
        self.restore_trash_inner(dir).map(|m| m.name)
    }

    fn restore_trash_inner(&self, dir: &str) -> Result<LibMod, String> {
        if dir.is_empty() || dir.contains('/') || dir.contains("..") || dir.contains('\\') {
            return Err("That is not a removed mod.".into());
        }
        let from = self.root.join("trash").join(dir);
        let v: Value = serde_json::from_slice(&fs::read(from.join("mod.json")).map_err(|_| "That mod can no longer be brought back.".to_string())?).map_err(|_| "That mod can no longer be brought back.".to_string())?;
        let mut m: LibMod = serde_json::from_value(v["mod"].clone()).map_err(|_| "That mod can no longer be brought back.".to_string())?;
        if !from.join("files").is_dir() {
            return Err("That mod can no longer be brought back.".into());
        }
        let mut lf = self.load();
        let base = slug(&m.name);
        let mut id = if lf.mods.iter().any(|x| x.id == m.id) || self.root.join("mods").join(&m.id).exists() { base.clone() } else { m.id.clone() };
        let mut n = 2;
        while lf.mods.iter().any(|x| x.id == id) || self.root.join("mods").join(&id).exists() {
            id = format!("{base}-{n}");
            n += 1;
        }
        fs::create_dir_all(self.root.join("mods")).map_err(|e| e.to_string())?;
        fs::rename(&from, self.root.join("mods").join(&id)).map_err(|e| format!("Could not bring the mod back: {e}"))?;
        let _ = fs::remove_file(self.root.join("mods").join(&id).join("mod.json"));
        m.id = id;
        m.enabled = true;
        lf.mods.push(m.clone());
        lf.dirty = true;
        self.save(&lf)?;
        Ok(m)
    }

    /// The earlier versions of a mod that are still in the trash (from updates): `(trash folder,
    /// version, when it was put there)`, newest first.
    pub fn previous_versions(&self, id: &str) -> Vec<(String, String, u64)> {
        let Some(cur) = self.load().mods.into_iter().find(|m| m.id == id) else { return Vec::new() };
        let mut out: Vec<(String, String, u64)> = fs::read_dir(self.root.join("trash"))
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(|e| {
                let v: Value = serde_json::from_slice(&fs::read(e.path().join("mod.json")).ok()?).ok()?;
                let m: LibMod = serde_json::from_value(v["mod"].clone()).ok()?;
                let same = match (&m.nexus, &cur.nexus, &m.thunderstore, &cur.thunderstore) {
                    (Some(a), Some(b), _, _) => a.game == b.game && a.mod_id == b.mod_id,
                    (_, _, Some(a), Some(b)) => a.full_name().eq_ignore_ascii_case(&b.full_name()),
                    _ => m.name == cur.name,
                };
                (same && e.path().join("files").is_dir() && m.version != cur.version).then(|| (e.file_name().to_string_lossy().to_string(), m.version, v["removed_ms"].as_u64().unwrap_or(0)))
            })
            .collect();
        out.sort_by_key(|t| std::cmp::Reverse(t.2));
        out
    }

    /// For every mod with an earlier version in the trash, the newest one: mod id → (trash folder, version). Reads the
    /// trash once (the list shows it on every row).
    pub fn newest_previous(&self, mods: &[LibMod]) -> HashMap<String, (String, String)> {
        let mut best: HashMap<String, (String, String, u64)> = HashMap::new();
        for e in fs::read_dir(self.root.join("trash")).into_iter().flatten().flatten() {
            let Some(v) = fs::read(e.path().join("mod.json")).ok().and_then(|b| serde_json::from_slice::<Value>(&b).ok()) else { continue };
            let Ok(m) = serde_json::from_value::<LibMod>(v["mod"].clone()) else { continue };
            if !e.path().join("files").is_dir() {
                continue;
            }
            let at = v["removed_ms"].as_u64().unwrap_or(0);
            for cur in mods {
                let same = match (&m.nexus, &cur.nexus, &m.thunderstore, &cur.thunderstore) {
                    (Some(a), Some(b), _, _) => a.game == b.game && a.mod_id == b.mod_id,
                    (_, _, Some(a), Some(b)) => a.full_name().eq_ignore_ascii_case(&b.full_name()),
                    _ => m.name == cur.name,
                };
                if same && m.version != cur.version && best.get(&cur.id).is_none_or(|b| b.2 < at) {
                    best.insert(cur.id.clone(), (e.file_name().to_string_lossy().to_string(), m.version.clone(), at));
                }
            }
        }
        best.into_iter().map(|(k, (d, v, _))| (k, (d, v))).collect()
    }

    /// Mods whose version changed at or after `since_ms`, with the version they could go back to:
    /// `(mod id, name, version now, earlier version, trash folder of the earlier one, when it changed)`.
    pub fn recent_updates(&self, since_ms: u64) -> Vec<RecentUpdate> {
        self.load()
            .mods
            .iter()
            .filter(|m| m.enabled)
            .filter_map(|m| {
                let (dir, old, at) = self.previous_versions(&m.id).into_iter().next()?;
                (at >= since_ms).then(|| RecentUpdate { id: m.id.clone(), name: m.name.clone(), version: m.version.clone(), old_version: old, dir, at_ms: at })
            })
            .collect()
    }

    /// Go back to an earlier version: it takes the place (and the on/off state and note) of the
    /// current one, which goes to the trash itself so it can be brought back too.
    pub fn rollback(&self, current_id: &str, trash_dir: &str) -> Result<String, String> {
        let _g = lock();
        let old = self.load().mods.into_iter().find(|m| m.id == current_id).ok_or("That mod is not in the library any more.")?;
        let restored = self.restore_trash_inner(trash_dir)?;
        let mut lf = self.load();
        let from = lf.mods.iter().position(|m| m.id == restored.id).ok_or("The earlier version could not be put back.")?;
        let mut m = lf.mods.remove(from);
        m.enabled = old.enabled;
        m.note = old.note.clone();
        m.update_rule = old.update_rule.clone();
        let at = lf.mods.iter().position(|x| x.id == current_id).unwrap_or(lf.mods.len());
        lf.mods.insert(at, m.clone());
        self.save(&lf)?;
        self.remove_inner(current_id)?;
        Ok(if m.version.is_empty() { m.name } else { format!("{} v{}", m.name, m.version) })
    }

    /// How updates of a mod are handled: "" (tell me), "auto" (update when the game is closed) or "never".
    pub fn set_update_rule(&self, id: &str, rule: &str) -> Result<(), String> {
        if !["", "auto", "never"].contains(&rule) {
            return Err("Unknown update rule.".into());
        }
        let _g = lock();
        let mut lf = self.load();
        let m = lf.mods.iter_mut().find(|m| m.id == id).ok_or("Unknown mod.")?;
        m.update_rule = rule.to_string();
        self.save(&lf)
    }

    /// Empty the library's trash (what was removed from the library earlier). Returns the bytes freed.
    pub fn empty_trash(&self) -> u64 {
        let _g = lock();
        let bytes = dir_bytes(&self.root.join("trash"));
        let _ = fs::remove_dir_all(self.root.join("trash"));
        bytes
    }

    /// The library's list of mods, profiles and choices as JSON, for a backup.
    pub fn export_meta(&self) -> Value {
        serde_json::to_value(self.load()).unwrap_or(Value::Null)
    }

    /// Put a backed-up list back. Mods whose files are no longer in the library are left out;
    /// the current list is kept next to it. Returns `(mods restored, mods left out)`.
    pub fn import_meta(&self, v: &Value) -> Result<(usize, usize), String> {
        let _g = lock();
        let mut lf: LibFile = serde_json::from_value(v.clone()).map_err(|_| "That backup of the mod library is not readable.".to_string())?;
        let before = lf.mods.len();
        lf.mods.retain(|m| self.mod_files_dir(&m.id).is_dir());
        let kept: HashSet<String> = lf.mods.iter().map(|m| m.id.clone()).collect();
        lf.pins.retain(|_, id| kept.contains(id));
        lf.profiles.values_mut().for_each(|p| p.retain(|(id, _)| kept.contains(id)));
        lf.undo = None;
        lf.dirty = true;
        let current = self.lib_path();
        if current.is_file() {
            let _ = fs::copy(&current, current.with_extension(format!("json.before-restore-{}", fsutil::now_ms())));
        }
        self.save(&lf)?;
        Ok((lf.mods.len(), before - lf.mods.len()))
    }

    /// Put the files of mods (all of them, or just `only`) beside the game's own files of the same
    /// names, as `smart_plan` decides. Returns how many mods were changed.
    pub fn relayout(&self, only: Option<&str>) -> Result<usize, String> {
        let _g = lock();
        let mut lf = self.load();
        let mut changed = 0;
        let mut purged = false;
        for m in lf.mods.iter_mut().filter(|m| only.map(|o| o == m.id).unwrap_or(true) && m.target.is_empty()) {
            let Some(plan) = self.smart_plan(&m.files) else { continue };
            if !purged {
                // links are tied to the staged paths: take them out (originals come back) before anything moves
                self.purge_inner()?;
                purged = true;
            }
            let dir = self.mod_files_dir(&m.id);
            let mut now = Vec::new();
            for (old, new) in m.files.iter().zip(&plan) {
                match new {
                    None if is_notes_name(old) => {
                        let _ = move_file(&dir.join(old), &self.notes_dir(&m.id).join(old));
                        if !m.notes.contains(old) {
                            m.notes.push(old.clone());
                        }
                    }
                    None => {
                        let _ = fs::remove_file(dir.join(old));
                    }
                    Some(n) if n == old => now.push(old.clone()),
                    Some(n) => {
                        move_file(&dir.join(old), &dir.join(n)).map_err(|e| format!("Could not move {old}: {e}"))?;
                        now.push(n.clone());
                    }
                }
            }
            prune_empty(&dir);
            now.sort();
            m.files = now;
            changed += 1;
        }
        if changed > 0 {
            lf.dirty = true;
            self.save(&lf)?;
        }
        Ok(changed)
    }

    /// Take a freshly extracted mod (consuming `stage`'s files) into the library. New mods are
    /// enabled and go last, so they win conflicts until the user reorders them.
    pub fn add_from_dir(&self, stage: &Path, name: &str, source: &str) -> Result<LibMod, String> {
        self.add_with(stage, name, source, None, "")
    }

    /// `add_from_dir` with the origin and version filled in.
    pub fn add_with(&self, stage: &Path, name: &str, source: &str, nexus: Option<NexusRef>, version: &str) -> Result<LibMod, String> {
        let _g = lock();
        self.add_inner(stage, name, source, nexus, version, None)
    }

    /// `add_with`, with the folder chosen by the user instead of Gantry's guess.
    pub fn add_with_target(&self, stage: &Path, name: &str, source: &str, nexus: Option<NexusRef>, version: &str, target: Option<&str>) -> Result<LibMod, String> {
        let _g = lock();
        self.add_inner(stage, name, source, nexus, version, target)
    }

    /// Replace `old_id` by a new version of the same mod: the newcomer takes over the old one's
    /// place in the load order, its enabled state and (when it has none of its own) its target
    /// folder; the old files go to the trash. Nothing changes when the new files cannot be added.
    pub fn replace_with(
        &self,
        old_id: &str,
        stage: &Path,
        name: &str,
        source: &str,
        nexus: Option<NexusRef>,
        version: &str,
    ) -> Result<LibMod, String> {
        let _g = lock();
        let old = self.load().mods.into_iter().find(|m| m.id == old_id).ok_or("The mod to update is no longer in the library.")?;
        let mut added = self.add_inner(stage, name, source, nexus, version, None)?;
        let mut lf = self.load();
        let from = lf.mods.iter().position(|m| m.id == added.id).expect("just added");
        let mut m = lf.mods.remove(from);
        m.enabled = old.enabled;
        m.note = old.note.clone();
        m.update_rule = old.update_rule.clone();
        if m.target.is_empty() {
            m.target = old.target.clone();
        }
        let at = lf.mods.iter().position(|x| x.id == old_id).unwrap_or(lf.mods.len());
        lf.mods.insert(at, m.clone());
        self.save(&lf)?;
        added = m;
        self.remove_inner(old_id)?;
        // keep the last three earlier versions for a rollback, not every one there ever was
        for (dir, _, _) in self.previous_versions(&added.id).into_iter().skip(3) {
            let _ = fs::remove_dir_all(self.root.join("trash").join(dir));
        }
        Ok(added)
    }

    /// Look at an extracted mod and decide where each of its files goes, without moving anything.
    fn analyze(&self, stage: &Path, forced_target: Option<&str>) -> Result<Analysis, String> {
        let mut files: Vec<PathBuf> = Vec::new();
        for e in WalkDir::new(stage).follow_links(false) {
            let e = e.map_err(|e| format!("Could not read the extracted mod: {e}"))?;
            if !e.file_type().is_file() {
                continue; // folders are implied; symlinks are never followed or installed
            }
            let rel = e.path().strip_prefix(stage).map_err(|e| e.to_string())?.to_path_buf();
            if !is_junk(&rel) {
                files.push(rel);
            }
        }
        // Thunderstore packages carry store metadata next to the mod; it is not part of the mod
        if files.iter().any(|f| f == Path::new("manifest.json")) {
            files.retain(|f| {
                f.components().count() > 1
                    || !["manifest.json", "icon.png", "readme.md", "changelog.md", "license", "license.txt", "license.md"]
                        .contains(&f.to_string_lossy().to_lowercase().as_str())
            });
        }
        if files.is_empty() {
            return Err("The mod contains no files.".into());
        }
        files.sort();
        let (strip, target) = self.detect_layout(&files);
        let cleaned: Vec<Option<String>> = files
            .iter()
            .map(|f| clean_rel(&f.components().skip(strip).collect::<PathBuf>().to_string_lossy()).filter(|r| !r.is_empty()))
            .collect();
        // instructions that came with the mod are kept to read, not laid into the game
        let note_idx: Vec<usize> = (0..files.len()).filter(|&i| cleaned[i].as_deref().map(is_notes_name).unwrap_or(false)).collect();
        let content_idx: Vec<usize> = (0..files.len()).filter(|i| cleaned[*i].is_some() && !note_idx.contains(i)).collect();
        let mut target = target;
        let content_rels: Vec<String> = content_idx.iter().filter_map(|&i| cleaned[i].clone()).collect();
        // a folder the user chose beats every guess
        if let Some(t) = forced_target {
            return Ok(Analysis { files, cleaned, note_idx, content_idx, smart: None, target: clean_rel(t.trim()).ok_or("The folder may not contain '..'.")?, placed_by: "manual" });
        }
        // a REDmod (Cyberpunk 2077's own mod format) belongs in the game's mods folder, in a folder of its own
        if target.is_empty() {
            if let Some(t) = self.redmod_target(stage, &files, &cleaned) {
                target = t;
            }
        }
        // no folder structure that fits the game: look for the game's own files of the same names
        let smart = if target.is_empty() && !content_rels.is_empty() { self.smart_plan(&content_rels) } else { None };
        // ...or follow what the mod's own instructions say, when that folder exists in the game
        let mut hinted = false;
        if smart.is_none() && target.is_empty() && !content_rels.is_empty() && content_rels.iter().all(|r| !r.contains('/')) {
            let mut text: String = note_idx
                .iter()
                .filter(|&&i| !cleaned[i].as_deref().unwrap_or("").to_lowercase().ends_with(".pdf"))
                .filter_map(|&i| fs::read(stage.join(&files[i])).ok())
                .map(|b| String::from_utf8_lossy(&b[..b.len().min(200_000)]).to_string())
                .collect::<Vec<_>>()
                .join("\n");
            if let Some(page) = &self.page_note {
                text.push('\n');
                text.push_str(page);
            }
            if let Some(dir) = crate::readme::folder_hints(&text).iter().find_map(|h| self.resolve_hint(h)) {
                target = dir;
                hinted = true;
            }
        }
        // the game's support entry: every loose file is of a kind it has a folder for, and all agree
        let mut by_rule = false;
        if smart.is_none() && !hinted && target.is_empty() && !self.rules.is_empty() && !content_rels.is_empty() && content_rels.iter().all(|r| !r.contains('/')) {
            let folder_of = |rel: &str| {
                let ext = Path::new(rel).extension().map(|e| e.to_string_lossy().to_lowercase()).unwrap_or_default();
                self.rules.iter().find(|(exts, _)| exts.contains(&ext)).map(|(_, f)| f.clone())
            };
            let folders: Vec<Option<String>> = content_rels.iter().map(|r| folder_of(r)).collect();
            if let Some(Some(first)) = folders.first() {
                if folders.iter().all(|f| f.as_ref() == Some(first)) {
                    target = first.clone();
                    by_rule = true;
                }
            }
        }
        let placed_by = if smart.is_some() {
            "names"
        } else if hinted {
            "readme"
        } else if by_rule {
            "pack"
        } else {
            ""
        };
        Ok(Analysis { files, cleaned, note_idx, content_idx, smart, target, placed_by })
    }

    /// Where the files of an extracted mod would go, for the user to approve first.
    pub fn preview(&self, stage: &Path) -> Result<Preview, String> {
        let a = self.analyze(stage, None)?;
        let mut folders: BTreeMap<String, usize> = BTreeMap::new();
        for (k, &i) in a.content_idx.iter().enumerate() {
            let Some(rel) = a.cleaned[i].clone() else { continue };
            let rel = match &a.smart {
                Some(plan) => match &plan[k] {
                    Some(n) => n.clone(),
                    None => continue,
                },
                None => rel,
            };
            let full = if a.target.is_empty() { rel } else { format!("{}/{}", a.target, rel) };
            let dir = full.rsplit_once('/').map(|d| d.0.to_string()).unwrap_or_default();
            *folders.entry(dir).or_default() += 1;
        }
        let files = folders.values().sum();
        let shape: BTreeSet<String> = a
            .content_idx
            .iter()
            .filter_map(|&i| a.cleaned[i].as_deref())
            .map(|rel| {
                let low = rel.to_lowercase();
                // a top folder the game has says where things go; any other is the mod's own name and says nothing
                let top = match low.split_once('/') {
                    Some((t, _)) if self.exists_ci(Path::new(""), t) => t.to_string(),
                    Some(_) => "*".to_string(),
                    None => String::new(),
                };
                let ext = low.rsplit_once('.').map(|(_, e)| e).filter(|e| !e.contains('/') && e.len() <= 8).unwrap_or("").to_string();
                format!("{top}/*.{ext}")
            })
            .collect();
        let shape = shape.into_iter().take(16).collect::<Vec<_>>().join(",");
        Ok(Preview { placed_by: a.placed_by.to_string(), target: a.target, files, folders: folders.into_iter().collect(), notes: a.note_idx.len(), shape })
    }

    fn add_inner(&self, stage: &Path, name: &str, source: &str, nexus: Option<NexusRef>, version: &str, forced_target: Option<&str>) -> Result<LibMod, String> {
        let Analysis { files, cleaned, note_idx, content_idx, smart, target, placed_by } = self.analyze(stage, forced_target)?;

        let mut lf = self.load();
        let base = slug(name);
        let mut id = base.clone();
        let mut n = 2;
        while lf.mods.iter().any(|m| m.id == id) || self.root.join("mods").join(&id).exists() {
            id = format!("{base}-{n}");
            n += 1;
        }
        let dest_root = self.mod_files_dir(&id);
        fs::create_dir_all(&dest_root).map_err(|e| format!("Could not create the staging folder: {e}"))?;

        let mut listed = Vec::new();
        let mut size = 0u64;
        let mut notes: Vec<String> = Vec::new();
        let result = (|| -> Result<(), String> {
            for &i in &note_idx {
                let rel = cleaned[i].clone().unwrap_or_default();
                let src = stage.join(&files[i]);
                move_file(&src, &self.notes_dir(&id).join(&rel)).map_err(|e| format!("Could not stage {rel}: {e}"))?;
                notes.push(rel);
            }
            if let Some(page) = &self.page_note {
                let rel = "Web page.txt".to_string();
                let path = self.notes_dir(&id).join(&rel);
                fs::create_dir_all(path.parent().expect("has parent")).map_err(|e| format!("Could not stage {rel}: {e}"))?;
                fs::write(&path, page).map_err(|e| format!("Could not stage {rel}: {e}"))?;
                notes.push(rel);
            }
            for (k, &i) in content_idx.iter().enumerate() {
                let f = &files[i];
                let Some(mut rel) = cleaned[i].clone() else { continue };
                if let Some(plan) = &smart {
                    match &plan[k] {
                        Some(n) => rel = n.clone(),
                        None => continue,
                    }
                }
                let src = stage.join(f);
                size += fs::metadata(&src).map(|m| m.len()).unwrap_or(0);
                move_file(&src, &dest_root.join(&rel)).map_err(|e| format!("Could not stage {rel}: {e}"))?;
                listed.push(rel);
            }
            Ok(())
        })();
        if let Err(e) = result {
            let _ = fs::remove_dir_all(self.root.join("mods").join(&id));
            return Err(e);
        }
        listed.sort();

        let m = LibMod {
            id,
            name: name.trim().to_string(),
            version: version.trim().to_string(),
            enabled: true,
            target,
            source: source.to_string(),
            nexus,
            thunderstore: None,
            github: None,
            modio: None,
            fomod: None,
            update_rule: String::new(),
            verified: String::new(),
            programs: listed.iter().filter(|f| is_program(f)).take(12).cloned().collect(),
            added_ms: fsutil::now_ms(),
            size,
            files: listed,
            notes,
            placed_by: placed_by.to_string(),
            note: String::new(),
            depends_on: Vec::new(),
            group: String::new(),
        };
        lf.mods.push(m.clone());
        lf.dirty = true;
        self.save(&lf)?;
        Ok(m)
    }

    /// The user's own remark about a mod.
    pub fn set_note(&self, id: &str, text: &str) -> Result<(), String> {
        let _g = lock();
        let mut lf = self.load();
        let m = lf.mods.iter_mut().find(|m| m.id == id).ok_or("Unknown mod.")?;
        m.note = text.trim().chars().take(500).collect();
        self.save(&lf)
    }

    /// Which Thunderstore packages a mod needs (versions are dropped, names kept).
    pub fn set_depends(&self, id: &str, deps: Vec<String>) -> Result<(), String> {
        let _g = lock();
        let mut lf = self.load();
        let m = lf.mods.iter_mut().find(|m| m.id == id).ok_or("Unknown mod.")?;
        m.depends_on = deps;
        self.save(&lf)
    }

    /// Mods that list `id`'s package among their dependencies.
    pub fn needed_by(&self, id: &str) -> Vec<String> {
        let lf = self.load();
        let Some(me) = lf.mods.iter().find(|m| m.id == id).and_then(|m| m.thunderstore.as_ref()).map(|t| t.full_name().to_lowercase()) else { return Vec::new() };
        lf.mods.iter().filter(|m| m.id != id && m.depends_on.iter().any(|d| d.to_lowercase() == me)).map(|m| m.name.clone()).collect()
    }

    /// Remember that a mod came from Thunderstore.
    pub fn set_thunderstore(&self, id: &str, r: ThunderRef) -> Result<(), String> {
        let _g = lock();
        let mut lf = self.load();
        let m = lf.mods.iter_mut().find(|m| m.id == id).ok_or("Unknown mod.")?;
        m.thunderstore = Some(r);
        self.save(&lf)
    }

    /// REDmods added before Gantry knew about them sit in the game's root; give them their `mods/<name>` folder (the
    /// change can be undone and takes effect at the next deploy). Returns the names of the mods that were changed.
    pub fn fix_redmod_targets(&self) -> Vec<String> {
        if !(self.exists_ci(Path::new(""), "r6") || self.exists_ci(Path::new(""), "REDprelauncher.exe")) {
            return Vec::new();
        }
        let _g = lock();
        let mut lf = self.load();
        let mut fixed: Vec<(String, String)> = Vec::new();
        for m in lf.mods.iter().filter(|m| m.target.is_empty()) {
            let has_info = m.files.iter().any(|f| f.eq_ignore_ascii_case("info.json"));
            let has_content = m.files.iter().any(|f| {
                let top = f.split('/').next().unwrap_or("").to_lowercase();
                f.contains('/') && ["archives", "tweaks", "scripts", "customsounds", "localization"].contains(&top.as_str())
            });
            if has_info && has_content {
                let info = m.files.iter().find(|f| f.eq_ignore_ascii_case("info.json")).cloned().unwrap_or_default();
                let folder = redmod_folder(&fs::read(self.mod_files_dir(&m.id).join(info)).unwrap_or_default());
                fixed.push((m.id.clone(), folder));
            }
        }
        if fixed.is_empty() {
            return Vec::new();
        }
        let names: Vec<String> = fixed.iter().filter_map(|(id, _)| lf.mods.iter().find(|m| &m.id == id).map(|m| m.name.clone())).collect();
        lf.snapshot(format!("Moved {} into its REDmod folder", names.join(", ")));
        for (id, folder) in &fixed {
            if let Some(m) = lf.mods.iter_mut().find(|m| &m.id == id) {
                m.target = folder.clone();
            }
        }
        lf.dirty = true;
        if self.save(&lf).is_err() {
            return Vec::new();
        }
        names
    }

    /// A mod that was added from a Nexus-named file keeps the file's whole name when the name was never changed: give it
    /// the real one (and its version). Returns how many were fixed; nothing is written when none were.
    pub fn tidy_names(&self) -> usize {
        let _g = lock();
        let mut lf = self.load();
        let mut n = 0;
        for m in lf.mods.iter_mut() {
            let own = Path::new(&m.source).file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
            if m.name.is_empty() || m.name != own {
                continue;
            }
            if let Some(d) = crate::nexus::parse_download_name(&m.source).filter(|d| !d.name.is_empty() && d.name != m.name) {
                m.name = d.name;
                if m.version.is_empty() {
                    m.version = d.version;
                }
                n += 1;
            }
        }
        if n > 0 && self.save(&lf).is_err() {
            return 0;
        }
        n
    }

    /// Remember which Nexus Mods file a mod came from (found from its file name); a name that is still the file's own
    /// is replaced by the clean one, and a missing version is filled in.
    pub fn set_nexus(&self, id: &str, r: crate::nexus::NexusRef, clean_name: &str, version: &str) -> Result<(), String> {
        let _g = lock();
        let mut lf = self.load();
        let m = lf.mods.iter_mut().find(|m| m.id == id).ok_or("Unknown mod.")?;
        let own = Path::new(&m.source).file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
        if !clean_name.is_empty() && (m.name == own || m.name == m.source) {
            m.name = clean_name.to_string();
        }
        if m.version.is_empty() {
            m.version = version.to_string();
        }
        m.nexus = Some(r);
        self.save(&lf)
    }

    /// Remember that a mod came from mod.io.
    pub fn set_modio(&self, id: &str, r: crate::modio::ModioRef) -> Result<(), String> {
        let _g = lock();
        let mut lf = self.load();
        let m = lf.mods.iter_mut().find(|m| m.id == id).ok_or("Unknown mod.")?;
        m.modio = Some(r);
        self.save(&lf)
    }

    /// Remember that a mod came from a GitHub release.
    pub fn set_github(&self, id: &str, r: crate::github::GithubRef) -> Result<(), String> {
        let _g = lock();
        let mut lf = self.load();
        let m = lf.mods.iter_mut().find(|m| m.id == id).ok_or("Unknown mod.")?;
        m.github = Some(r);
        self.save(&lf)
    }

    /// Remember that the download was checked against the site's checksum.
    pub fn set_verified(&self, id: &str, how: &str) -> Result<(), String> {
        let _g = lock();
        let mut lf = self.load();
        let m = lf.mods.iter_mut().find(|m| m.id == id).ok_or("Unknown mod.")?;
        m.verified = how.to_string();
        self.save(&lf)
    }

    /// Keep the answers given to the mod's installer script.
    pub fn set_fomod(&self, id: &str, answers: serde_json::Value) -> Result<(), String> {
        let _g = lock();
        let mut lf = self.load();
        let m = lf.mods.iter_mut().find(|m| m.id == id).ok_or("Unknown mod.")?;
        m.fomod = Some(answers);
        self.save(&lf)
    }

    /// Like `add_from_dir`, with a version attached (from the file name or the user).
    pub fn set_version(&self, id: &str, version: &str) -> Result<(), String> {
        let _g = lock();
        let mut lf = self.load();
        let m = lf.mods.iter_mut().find(|m| m.id == id).ok_or("Unknown mod.")?;
        m.version = version.trim().to_string();
        self.save(&lf)
    }

    // ───────────────────────────── editing ─────────────────────────────

    pub fn set_enabled(&self, id: &str, enabled: bool) -> Result<(), String> {
        let _g = lock();
        let mut lf = self.load();
        let name = lf.mods.iter().find(|m| m.id == id).ok_or("Unknown mod.")?.name.clone();
        if lf.mods.iter().any(|m| m.id == id && m.enabled != enabled) {
            lf.snapshot(if enabled { format!("Enabled {}", name) } else { format!("Disabled {}", name) });
            if let Some(m) = lf.mods.iter_mut().find(|m| m.id == id) {
                m.enabled = enabled;
            }
            lf.dirty = true;
            self.save(&lf)?;
        }
        Ok(())
    }

    /// Put a mod at a given place in the load order (0 = first); used by drag and drop.
    pub fn move_to(&self, id: &str, to: usize) -> Result<(), String> {
        let _g = lock();
        let mut lf = self.load();
        let i = lf.mods.iter().position(|m| m.id == id).ok_or("Unknown mod.")?;
        let j = to.min(lf.mods.len() - 1);
        if i != j {
            let name = lf.mods[i].name.clone();
            lf.snapshot(format!("Moved {}", name));
            let m = lf.mods.remove(i);
            lf.mods.insert(j, m);
            lf.dirty = true;
            self.save(&lf)?;
        }
        Ok(())
    }

    /// Turn several mods on or off as one step (one entry in the history). Returns how many changed.
    pub fn set_enabled_many(&self, ids: &[String], enabled: bool) -> Result<usize, String> {
        let _g = lock();
        let mut lf = self.load();
        let changing = lf.mods.iter().filter(|m| ids.contains(&m.id) && m.enabled != enabled).count();
        if changing == 0 {
            return Ok(0);
        }
        lf.snapshot(if enabled { format!("Enabled {} mods", changing) } else { format!("Disabled {} mods", changing) });
        for m in lf.mods.iter_mut().filter(|m| ids.contains(&m.id)) {
            m.enabled = enabled;
        }
        lf.dirty = true;
        self.save(&lf)?;
        Ok(changing)
    }

    /// Automatically optimize the load order based on mod dependencies, core frameworks,
    /// script loaders, and asset types. Returns the total count of mods in the sorted library.
    pub fn optimize_load_order(&self) -> Result<usize, String> {
        let _g = lock();
        let mut lf = self.load();
        if lf.mods.len() <= 1 {
            return Ok(lf.mods.len());
        }
        let total = lf.mods.len();
        lf.snapshot("Auto-sorted load order".into());

        fn tier_score(m: &LibMod) -> u32 {
            let n = m.name.to_lowercase();
            let grp = m.group.to_lowercase();
            // Tier 10: Engine Loaders & Script Extenders
            if n.contains("red4ext")
                || n.contains("ue4ss")
                || n.contains("bepinex")
                || n.contains("melonloader")
                || n.contains("reframework")
                || n.contains("skse")
                || n.contains("f4se")
                || n.contains("nvse")
                || n.contains("obse")
                || n.contains("gdweave")
                || grp == "loader"
                || grp == "core"
                || m.files.iter().any(|f| {
                    let fl = f.to_lowercase();
                    fl.ends_with("dinput8.dll") || fl.ends_with("version.dll") || fl.ends_with("dxgi.dll") || fl.ends_with("winmm.dll")
                })
            {
                return 10;
            }
            // Tier 20: Core Frameworks, Address Libraries, Script Compilers
            if n.contains("archivexl")
                || n.contains("tweakxl")
                || n.contains("codeware")
                || n.contains("redscript")
                || n.contains("cyber engine tweaks")
                || n.contains("cet")
                || n.contains("address library")
                || n.contains("skyui")
                || n.contains("mod configuration menu")
                || n.contains("mcm")
                || grp == "framework"
                || grp == "library"
            {
                return 20;
            }
            // Tier 30: Gameplay, Logic, Scripts, Tweaks
            if grp == "gameplay"
                || grp == "scripts"
                || m.files.iter().any(|f| {
                    let fl = f.to_lowercase();
                    fl.ends_with(".reds") || fl.ends_with(".lua") || fl.ends_with(".asi") || fl.starts_with("scripts/")
                })
            {
                return 30;
            }
            // Tier 70: Visual, Textures, Meshes, Sounds
            if grp == "visual"
                || grp == "textures"
                || grp == "audio"
                || m.files.iter().any(|f| {
                    let fl = f.to_lowercase();
                    fl.ends_with(".pak") || fl.ends_with(".archive") || fl.ends_with(".ba2") || fl.ends_with(".dds")
                })
            {
                return 70;
            }
            // Tier 90: UI Overrides, HUD, Patches, Translations (load late to win conflicts)
            if n.contains("patch")
                || n.contains("fix")
                || n.contains("translation")
                || n.contains("override")
                || n.contains("ui")
                || n.contains("hud")
                || grp == "ui"
                || grp == "patch"
            {
                return 90;
            }
            // Tier 50: General / Default Mods
            50
        }

        lf.mods.sort_by_key(tier_score);

        // Topological / dependency resolution pass:
        // If mod A depends on mod B, ensure B is positioned before A
        for _ in 0..lf.mods.len() {
            let mut swapped = false;
            for i in 0..lf.mods.len() {
                let deps = lf.mods[i].depends_on.clone();
                for dep in deps {
                    let dep_lower = dep.to_lowercase();
                    if let Some(dep_pos) = lf.mods.iter().skip(i + 1).position(|m| {
                        m.name.to_lowercase().contains(&dep_lower) || m.id.to_lowercase().contains(&dep_lower)
                    }) {
                        let actual_dep_idx = i + 1 + dep_pos;
                        let dep_mod = lf.mods.remove(actual_dep_idx);
                        lf.mods.insert(i, dep_mod);
                        swapped = true;
                        break;
                    }
                }
                if swapped {
                    break;
                }
            }
            if !swapped {
                break;
            }
        }

        lf.dirty = true;
        self.save(&lf)?;
        Ok(total)
    }

    /// Give several mods the same group label ("" removes it).
    pub fn set_group(&self, ids: &[String], group: &str) -> Result<usize, String> {
        let _g = lock();
        let group: String = group.trim().chars().take(40).collect();
        let mut lf = self.load();
        let mut n = 0;
        for m in lf.mods.iter_mut().filter(|m| ids.contains(&m.id)) {
            if m.group != group {
                m.group = group.clone();
                n += 1;
            }
        }
        if n > 0 {
            self.save(&lf)?;
        }
        Ok(n)
    }

    /// Remove several mods (each goes to the trash, so each can come back). Returns how many went.
    pub fn remove_many(&self, ids: &[String]) -> Result<usize, String> {
        let _g = lock();
        let mut n = 0;
        for id in ids {
            if self.load().mods.iter().any(|m| m.id == *id) {
                self.remove_inner(id)?;
                n += 1;
            }
        }
        Ok(n)
    }

    /// Move a mod up (negative) or down (positive) the load order.
    pub fn move_mod(&self, id: &str, delta: i32) -> Result<(), String> {
        let _g = lock();
        let mut lf = self.load();
        let i = lf.mods.iter().position(|m| m.id == id).ok_or("Unknown mod.")?;
        let j = (i as i64 + delta as i64).clamp(0, lf.mods.len() as i64 - 1) as usize;
        if i != j {
            let name = lf.mods[i].name.clone();
            lf.snapshot(format!("Moved {}", name));
            let m = lf.mods.remove(i);
            lf.mods.insert(j, m);
            lf.dirty = true;
            self.save(&lf)?;
        }
        Ok(())
    }

    pub fn set_target(&self, id: &str, target: &str) -> Result<(), String> {
        let _g = lock();
        let target = clean_rel(target.trim()).ok_or("The folder may not contain '..'.")?;
        let mut lf = self.load();
        let name = lf.mods.iter().find(|m| m.id == id).ok_or("Unknown mod.")?.name.clone();
        if lf.mods.iter().any(|m| m.id == id && m.target != target) {
            lf.snapshot(format!("Changed the folder of {}", name));
            if let Some(m) = lf.mods.iter_mut().find(|m| m.id == id) {
                m.target = target;
            }
            lf.dirty = true;
            self.save(&lf)?;
        }
        Ok(())
    }

    // ───────────────────────────── profiles and undo ─────────────────────────────

    fn clean_profile_name(name: &str) -> Result<String, String> {
        let n = name.trim();
        if n.is_empty() {
            return Err("Give the profile a name.".into());
        }
        if n.chars().count() > 40 || n.chars().any(char::is_control) {
            return Err("Profile names can be at most 40 characters.".into());
        }
        Ok(n.to_string())
    }

    /// Remember which mods are on, and in what order, under `name` (replacing an older one).
    pub fn save_profile(&self, name: &str) -> Result<String, String> {
        let _g = lock();
        let name = Self::clean_profile_name(name)?;
        let mut lf = self.load();
        let set = lf.mods.iter().map(|m| (m.id.clone(), m.enabled)).collect();
        lf.profiles.insert(name.clone(), set);
        self.save(&lf)?;
        Ok(name)
    }

    /// Switch to a saved profile: its mods in its order and state; mods it does not know are off.
    /// Returns how many mods changed their enabled state.
    pub fn load_profile(&self, name: &str) -> Result<usize, String> {
        let _g = lock();
        let mut lf = self.load();
        let profile = lf.profiles.get(name).cloned().ok_or("No such profile.")?;
        lf.snapshot(format!("Loaded profile '{}'", name));
        let wanted: HashMap<&str, bool> = profile.iter().map(|(id, on)| (id.as_str(), *on)).collect();
        let mut changed = 0;
        for m in &mut lf.mods {
            let on = wanted.get(m.id.as_str()).copied().unwrap_or(false);
            if m.enabled != on {
                m.enabled = on;
                changed += 1;
            }
        }
        // profile mods first, in the profile's order; the rest keep their relative order after them
        let pos: HashMap<&str, usize> = profile.iter().enumerate().map(|(i, (id, _))| (id.as_str(), i)).collect();
        lf.mods.sort_by_key(|m| pos.get(m.id.as_str()).copied().unwrap_or(usize::MAX));
        lf.dirty = true;
        self.save(&lf)?;
        Ok(changed)
    }

    /// The saved profile whose mods on are exactly the ones on now, if there is one.
    pub fn matching_profile(&self) -> Option<String> {
        let lf = self.load();
        let have: HashSet<&str> = lf.mods.iter().map(|m| m.id.as_str()).collect();
        let on: HashSet<&str> = lf.mods.iter().filter(|m| m.enabled).map(|m| m.id.as_str()).collect();
        // a mod removed since the profile was saved does not count against it
        lf.profiles.iter().find(|(_, set)| set.iter().filter(|(id, e)| *e && have.contains(id.as_str())).map(|(id, _)| id.as_str()).collect::<HashSet<_>>() == on).map(|(n, _)| n.clone())
    }

    pub fn separate_saves(&self) -> bool {
        self.load().separate_saves
    }

    pub fn set_separate_saves(&self, on: bool) -> Result<(), String> {
        let _g = lock();
        let mut lf = self.load();
        lf.separate_saves = on;
        self.save(&lf)
    }

    /// The profile whose saves are in the game's folders now.
    pub fn saves_of(&self) -> String {
        self.load().saves_of
    }

    pub fn saves_return(&self) -> String {
        self.load().saves_return
    }

    pub fn set_saves(&self, of: &str, back_to: Option<&str>) -> Result<(), String> {
        let _g = lock();
        let mut lf = self.load();
        lf.saves_of = of.to_string();
        if let Some(b) = back_to {
            lf.saves_return = b.to_string();
        }
        self.save(&lf)
    }

    pub fn delete_profile(&self, name: &str) -> Result<(), String> {
        let _g = lock();
        let mut lf = self.load();
        lf.profiles.remove(name).ok_or("No such profile.")?;
        lf.profile_launch.remove(name);
        self.save(&lf)
    }

    /// Remember how the game starts with a saved profile.
    pub fn set_profile_launch(&self, name: &str, launch: ProfileLaunch) -> Result<(), String> {
        let _g = lock();
        let mut lf = self.load();
        if !lf.profiles.contains_key(name) {
            return Err("No such profile.".into());
        }
        lf.profile_launch.insert(name.to_string(), launch);
        self.save(&lf)
    }

    pub fn profile_launch(&self, name: &str) -> Option<ProfileLaunch> {
        self.load().profile_launch.get(name).cloned()
    }

    /// Take back the last change of the list (enabled state, order or target folder). Returns its label.
    pub fn undo(&self) -> Result<String, String> {
        let _g = lock();
        let mut lf = self.load();
        let snap = lf.undo.take().ok_or("Nothing to undo for this game.")?;
        lf.apply(&snap);
        self.save(&lf)?;
        Ok(snap.label)
    }

    /// Go back to an earlier state of the list (`index` counts from the newest, 0 = most recent).
    /// The state before going back is kept, so this can be undone too.
    pub fn restore_history(&self, index: usize) -> Result<String, String> {
        let _g = lock();
        let mut lf = self.load();
        let n = lf.history.len();
        let snap = n.checked_sub(index + 1).and_then(|i| lf.history.get(i).cloned()).ok_or("That moment is no longer in the history.")?;
        lf.snapshot(format!("Went back to before \"{}\"", snap.label));
        lf.apply(&snap);
        self.save(&lf)?;
        Ok(snap.label)
    }

    /// Disables all content mods, keeping only core loaders and framework mods enabled.
    /// Returns the number of content mods disabled.
    pub fn stage_safe_mode(&self) -> Result<usize, String> {
        let _g = lock();
        let mut lf = self.load();
        lf.snapshot("Before Safe Mode launch".into());
        let mut disabled = 0;
        let is_loader = |m: &LibMod| -> bool {
            let n = m.name.to_lowercase();
            let is_hook = m.files.iter().any(|f| {
                let l = f.to_lowercase();
                l.ends_with(".dll") || l.ends_with(".asi") || l.ends_with(".so")
                    || crate::deploy::PROXY_DLLS.iter().any(|p| l.contains(p))
            });
            is_hook || n.contains("ue4ss") || n.contains("bepinex") || n.contains("melonloader")
                || n.contains("loader") || n.contains("framework") || n.contains("extender")
                || n.contains("red4ext") || n.contains("cet") || n.contains("gdweave")
        };
        for m in &mut lf.mods {
            if m.enabled && !is_loader(m) {
                m.enabled = false;
                disabled += 1;
            }
        }
        if disabled > 0 {
            lf.dirty = true;
            self.save(&lf)?;
        }
        Ok(disabled)
    }

    /// Restore the mods state that was snapshotted before Safe Mode launch.
    pub fn restore_safe_mode(&self) -> Result<(), String> {
        let _g = lock();
        let mut lf = self.load();
        let n = lf.history.len();
        if let Some(snap) = n.checked_sub(1).and_then(|i| lf.history.get(i).cloned()) {
            lf.apply(&snap);
            self.save(&lf)?;
        }
        Ok(())
    }

    /// Remove a mod: purge first (so none of its files stay in the game), then move its files
    /// to the library trash. The caller redeploys afterwards.
    pub fn remove(&self, id: &str) -> Result<(), String> {
        let _g = lock();
        self.remove_inner(id)
    }

    fn remove_inner(&self, id: &str) -> Result<(), String> {
        let mut lf = self.load();
        if !lf.mods.iter().any(|m| m.id == id) {
            return Err("Unknown mod.".into());
        }
        self.purge_inner()?;
        let from = self.root.join("mods").join(id);
        if from.exists() {
            // what the mod was, so it can be brought back from the trash
            if let Some(m) = lf.mods.iter().find(|m| m.id == id) {
                let mut meta = m.clone();
                meta.enabled = true;
                let _ = fs::write(from.join("mod.json"), serde_json::to_string(&json!({ "mod": meta, "removed_ms": fsutil::now_ms() })).unwrap_or_default());
            }
            let to = self.root.join("trash").join(format!("{id}-{}", fsutil::unique_token()));
            if let Some(p) = to.parent() {
                fs::create_dir_all(p).map_err(|e| e.to_string())?;
            }
            fs::rename(&from, &to).map_err(|e| format!("Could not move the mod to the trash: {e}"))?;
        }
        lf.mods.retain(|m| m.id != id);
        lf.dirty = true;
        self.save(&lf)
    }

    // ───────────────────────────── planning ─────────────────────────────

    fn build_plan(&self, mods: &[LibMod], pins: &BTreeMap<String, String>) -> Plan {
        let mut res = Resolver::new(&self.game_dir);
        let mut winners: BTreeMap<String, Planned> = BTreeMap::new();
        let mut owners: HashMap<String, Vec<usize>> = HashMap::new();
        let mut files_of: HashMap<(String, usize), String> = HashMap::new();
        for (idx, m) in mods.iter().enumerate() {
            if !m.enabled {
                continue;
            }
            for f in &m.files {
                let rel = Path::new(&m.target).join(f);
                let dest = fsutil::slashed(&res.resolve(&rel));
                let key = dest.to_lowercase();
                let o = owners.entry(key.clone()).or_default();
                if o.last() != Some(&idx) {
                    o.push(idx);
                }
                files_of.insert((key.clone(), idx), f.clone());
                winners.insert(key, Planned { dest, mod_idx: idx, file: f.clone() });
            }
        }
        // the user's choices beat the load order, as long as that mod still provides the file
        let mut pinned: HashSet<String> = HashSet::new();
        for (key, id) in pins {
            let Some(idx) = mods.iter().position(|m| &m.id == id && m.enabled) else { continue };
            if let (Some(file), Some(w)) = (files_of.get(&(key.clone(), idx)), winners.get_mut(key)) {
                w.mod_idx = idx;
                w.file = file.clone();
                pinned.insert(key.clone());
            }
        }
        let mut conflicts: Vec<Conflict> = owners
            .iter()
            .filter(|(_, v)| v.len() > 1)
            .map(|(k, v)| {
                let w = &winners[k];
                Conflict {
                    path: w.dest.clone(),
                    mods: v.iter().map(|i| mods[*i].name.clone()).collect(),
                    ids: v.iter().map(|i| mods[*i].id.clone()).collect(),
                    winner: mods[w.mod_idx].name.clone(),
                    winner_id: mods[w.mod_idx].id.clone(),
                    pinned: pinned.contains(k),
                }
            })
            .collect();
        conflicts.sort_by(|a, b| a.path.cmp(&b.path));
        Plan { files: winners.into_values().collect(), conflicts }
    }

    /// Conflicting files between enabled mods, for the UI.
    pub fn conflicts(&self) -> Vec<Conflict> {
        let lf = self.load();
        self.build_plan(&lf.mods, &lf.pins).conflicts
    }

    /// The conflict screen: how each mod fares, who overwrites whom, and the files (filtered by `query`,
    /// at most `limit` of them) with the choice made for each.
    pub fn conflict_view(&self, query: &str, limit: usize) -> Value {
        let lf = self.load();
        let conflicts = self.build_plan(&lf.mods, &lf.pins).conflicts;
        let q = query.trim().to_lowercase();
        let mut stat: HashMap<&str, (usize, usize)> = HashMap::new();
        let mut pairs: HashMap<(&str, &str), usize> = HashMap::new();
        for c in &conflicts {
            for id in &c.ids {
                let e = stat.entry(id.as_str()).or_default();
                if *id == c.winner_id {
                    e.0 += 1;
                } else {
                    e.1 += 1;
                    *pairs.entry((c.winner_id.as_str(), id.as_str())).or_default() += 1;
                }
            }
        }
        let name_of = |id: &str| lf.mods.iter().find(|m| m.id == id).map(|m| m.name.clone()).unwrap_or_default();
        let mut mods: Vec<Value> = lf
            .mods
            .iter()
            .filter(|m| stat.contains_key(m.id.as_str()))
            .map(|m| {
                let (wins, loses) = stat[m.id.as_str()];
                json!({ "id": m.id, "name": m.name, "wins": wins, "loses": loses, "files": m.files.len(), "dead": wins == 0 && loses >= m.files.len() })
            })
            .collect();
        mods.sort_by_key(|m| std::cmp::Reverse(m["loses"].as_u64().unwrap_or(0)));
        let mut pairs: Vec<((&str, &str), usize)> = pairs.into_iter().collect();
        pairs.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
        let matching: Vec<&Conflict> = conflicts.iter().filter(|c| q.is_empty() || c.path.to_lowercase().contains(&q) || c.mods.iter().any(|n| n.to_lowercase().contains(&q))).collect();
        json!({
            "total": conflicts.len(),
            "matching": matching.len(),
            "pinned": conflicts.iter().filter(|c| c.pinned).count(),
            "mods": mods,
            "pairs": pairs.iter().map(|((w, l), n)| json!({ "winner_id": w, "winner": name_of(w), "loser_id": l, "loser": name_of(l), "files": n })).collect::<Vec<_>>(),
            "files": matching.iter().take(limit).collect::<Vec<_>>(),
        })
    }

    /// Make `winner` the mod whose copy is used for every file it shares with `over` (an empty `over` means
    /// every file it shares with anyone). Returns how many files were pinned.
    pub fn prefer(&self, winner: &str, over: &str) -> Result<usize, String> {
        let _g = lock();
        let mut lf = self.load();
        if !lf.mods.iter().any(|m| m.id == winner) {
            return Err("Unknown mod.".into());
        }
        let conflicts = self.build_plan(&lf.mods, &lf.pins).conflicts;
        let mut n = 0;
        for c in conflicts.iter().filter(|c| c.ids.iter().any(|i| i == winner) && (over.is_empty() || c.ids.iter().any(|i| i == over))) {
            if lf.pins.insert(c.path.to_lowercase(), winner.to_string()).as_deref() != Some(winner) {
                n += 1;
            }
        }
        if n > 0 {
            lf.dirty = true;
            self.save(&lf)?;
        }
        Ok(n)
    }

    /// Forget every pinned file: the load order decides again.
    pub fn clear_pins(&self) -> Result<usize, String> {
        let _g = lock();
        let mut lf = self.load();
        let n = lf.pins.len();
        if n > 0 {
            lf.pins.clear();
            lf.dirty = true;
            self.save(&lf)?;
        }
        Ok(n)
    }

    /// Automatically resolve file conflicts between enabled mods using smart topological priority:
    /// - Specific patches and compatibility fixes win over large base texture packs.
    /// - Base overhauls and community patches load first so sub-mods can overwrite them cleanly.
    pub fn auto_resolve_conflicts(&self, strategy: &str) -> Result<ConflictResolutionReport, String> {
        let _g = lock();
        let mut lf = self.load();
        let initial_conflicts = self.build_plan(&lf.mods, &lf.pins).conflicts;
        if initial_conflicts.is_empty() {
            return Ok(ConflictResolutionReport {
                resolved_conflicts: 0,
                reordered_mods: Vec::new(),
                auto_pinned_files: Vec::new(),
                strategy: strategy.to_string(),
            });
        }

        lf.snapshot("Auto-resolved mod conflicts".into());

        let is_patch = |name: &str| {
            let n = name.to_lowercase();
            n.contains("patch") || n.contains("hotfix") || n.contains("fix") || n.contains("compat") || n.contains("update")
        };

        let is_base = |name: &str| {
            let n = name.to_lowercase();
            n.contains("unofficial") || n.contains("base") || n.contains("overhaul") || n.contains("core") || n.contains("framework")
        };

        let mut auto_pinned = Vec::new();

        for c in &initial_conflicts {
            if c.pinned { continue; }
            if let Some(patch_id) = c.ids.iter().find(|id| {
                lf.mods.iter().find(|m| &m.id == *id).map(|m| is_patch(&m.name)).unwrap_or(false)
            }) {
                if patch_id != &c.winner_id {
                    let patch_mod = lf.mods.iter().find(|m| &m.id == patch_id).unwrap();
                    lf.pins.insert(c.path.to_lowercase(), patch_id.clone());
                    auto_pinned.push((c.path.clone(), patch_mod.name.clone()));
                }
            }
        }

        let mut reordered = Vec::new();
        let mut new_mods = lf.mods.clone();
        new_mods.sort_by(|a, b| {
            let a_base = is_base(&a.name);
            let b_base = is_base(&b.name);
            let a_patch = is_patch(&a.name);
            let b_patch = is_patch(&b.name);

            if a_base && !b_base {
                std::cmp::Ordering::Less
            } else if !a_base && b_base {
                std::cmp::Ordering::Greater
            } else if a_patch && !b_patch {
                std::cmp::Ordering::Greater
            } else if !a_patch && b_patch {
                std::cmp::Ordering::Less
            } else {
                std::cmp::Ordering::Equal
            }
        });

        if new_mods.iter().map(|m| &m.id).ne(lf.mods.iter().map(|m| &m.id)) {
            for (idx, m) in new_mods.iter().enumerate() {
                if m.id != lf.mods[idx].id {
                    reordered.push(m.name.clone());
                }
            }
            lf.mods = new_mods;
        }

        let post_plan = self.build_plan(&lf.mods, &lf.pins);
        let resolved = initial_conflicts.len().saturating_sub(
            post_plan.conflicts.iter().filter(|c| !c.pinned).count()
        ) + auto_pinned.len();

        lf.dirty = true;
        self.save(&lf)?;

        Ok(ConflictResolutionReport {
            resolved_conflicts: resolved,
            reordered_mods: reordered,
            auto_pinned_files: auto_pinned,
            strategy: strategy.to_string(),
        })
    }

    // ───────────────────────────── deploy / purge ─────────────────────────────

    fn is_ours(&self, e: &Placed) -> bool {
        let dest = self.game_dir.join(&e.rel);
        let Ok(dm) = fs::symlink_metadata(&dest) else { return false };
        if !dm.is_file() {
            return false;
        }
        if fsutil::is_same_file(&self.mod_files_dir(&e.mod_id).join(&e.file), &dest) {
            return true;
        }
        e.kind == "copy" && dm.len() == e.size && fsutil::mtime_ns(&dm) == e.mtime_ns
    }

    fn backup_path(&self, rel: &str) -> PathBuf {
        self.root.join("backups").join(rel)
    }

    fn purge_inner(&self) -> Result<PurgeReport, String> {
        let j = self.journal();
        let mut rep = PurgeReport::default();
        for e in j.entries.iter().rev() {
            let dest = self.game_dir.join(&e.rel);
            let mut present = fs::symlink_metadata(&dest).is_ok();
            if present {
                if self.is_ours(e) {
                    fs::remove_file(&dest).map_err(|er| format!("Could not remove {}: {er}", e.rel))?;
                    rep.removed += 1;
                    present = false;
                } else {
                    rep.kept.push(e.rel.clone());
                }
            }
            if e.backup && !present {
                let bak = self.backup_path(&e.rel);
                if bak.is_file() {
                    move_file(&bak, &dest).map_err(|er| format!("Could not restore {}: {er}", e.rel))?;
                    rep.restored += 1;
                }
            }
        }
        for d in j.created_dirs.iter().rev() {
            let _ = fs::remove_dir(self.game_dir.join(d)); // only succeeds when empty
        }
        // backups of files left alone stay; everything else has been restored
        prune_empty(&self.root.join("backups"));
        self.save_journal(&Journal::default())?;
        Ok(rep)
    }

    /// The mod list as a shareable setup (no files, only what each mod is and where it came from).
    pub fn export_spec(&self, game: &str) -> ProfileSpec {
        spec_of(&self.load(), game)
    }

    /// The setup as a modpack: `export_spec`, with the files of every mod that cannot be fetched again (one that
    /// came from neither Nexus Mods nor Thunderstore) copied under `dest/mods/<n>`. Returns how many were copied.
    pub fn export_pack(&self, game: &str, name: &str, note: &str, dest: &Path) -> Result<(ProfileSpec, usize), String> {
        self.export_pack_opt(game, name, note, dest, false)
    }

    /// Export setup as a modpack, optionally copying files of all mods when `full_bundle` is true.
    pub fn export_pack_opt(&self, game: &str, name: &str, note: &str, dest: &Path, full_bundle: bool) -> Result<(ProfileSpec, usize), String> {
        let _g = lock();
        let lf = self.load();
        let mut spec = self.export_spec(game);
        spec.name = name.chars().take(80).collect();
        spec.note = note.chars().take(400).collect();
        let mut copied = 0;
        for (i, m) in lf.mods.iter().enumerate() {
            if !full_bundle && spec.mods[i].fetchable() {
                continue;
            }
            let from = self.mod_files_dir(&m.id);
            if !from.is_dir() {
                continue;
            }
            let to = dest.join("mods").join(i.to_string());
            fsutil::copy_dir_all(&from, &to).map_err(|e| format!("Could not copy {}: {}", m.name, e))?;
            spec.mods[i].files = format!("mods/{i}");
            copied += 1;
        }
        Ok((spec, copied))
    }

    /// The mods of a shared setup that this library does not have yet.
    pub fn missing_of(&self, spec: &ProfileSpec) -> Vec<SpecMod> {
        let lf = self.load();
        let mut used: Vec<usize> = Vec::new();
        let mut missing = Vec::new();
        for s in &spec.mods {
            match (0..lf.mods.len()).find(|i| !used.contains(i) && same_mod(&lf.mods[*i], s)) {
                Some(i) => used.push(i),
                None => missing.push(s.clone()),
            }
        }
        missing
    }

    /// Apply a shared setup to the mods that are already here (order, on/off, folder) and return
    /// the ones that are not in the library yet. Mods the setup does not mention stay as they are, last.
    pub fn apply_spec(&self, spec: &ProfileSpec) -> Result<Vec<SpecMod>, String> {
        let _g = lock();
        if spec.gantry_profile != 1 {
            return Err("This file is not a ModForge setup.".into());
        }
        let mut lf = self.load();
        let mut missing = Vec::new();
        let mut order: Vec<usize> = Vec::new();
        let before = lf.mods.clone();
        for s in &spec.mods {
            match (0..lf.mods.len()).find(|i| !order.contains(i) && same_mod(&lf.mods[*i], s)) {
                Some(i) => {
                    if lf.mods[i].group.is_empty() {
                        lf.mods[i].group = s.group.clone();
                    }
                    if lf.mods[i].note.is_empty() {
                        lf.mods[i].note = s.note.clone();
                    }
                    lf.mods[i].enabled = s.enabled;
                    if !s.target.is_empty() || lf.mods[i].target.is_empty() {
                        lf.mods[i].target = s.target.clone();
                    }
                    order.push(i);
                }
                None => missing.push(s.clone()),
            }
        }
        let mut sorted: Vec<LibMod> = order.iter().map(|&i| lf.mods[i].clone()).collect();
        sorted.extend(lf.mods.iter().enumerate().filter(|(i, _)| !order.contains(i)).map(|(_, m)| m.clone()));
        let changed = sorted.iter().zip(&before).any(|(a, b)| a.id != b.id || a.enabled != b.enabled || a.target != b.target || a.group != b.group || a.note != b.note);
        if changed {
            lf.snapshot("Imported a setup".to_string());
            lf.mods = sorted;
            lf.dirty = true;
            self.save(&lf)?;
        }
        Ok(missing)
    }

    /// Switch exactly the mods in `on` on and every other one off (Crash Detective rounds).
    pub fn set_enabled_set(&self, on: &[String]) -> Result<usize, String> {
        let _g = lock();
        let mut lf = self.load();
        let mut changed = 0;
        for m in &mut lf.mods {
            let want = on.contains(&m.id);
            if m.enabled != want {
                m.enabled = want;
                changed += 1;
            }
        }
        if changed > 0 {
            lf.dirty = true;
            self.save(&lf)?;
        }
        Ok(changed)
    }

    /// Make `id` the mod whose version of `path` is used (an empty id goes back to the load order).
    pub fn pin_file(&self, path: &str, id: &str) -> Result<(), String> {
        let _g = lock();
        let mut lf = self.load();
        let key = path.to_lowercase();
        if id.is_empty() {
            lf.pins.remove(&key);
        } else {
            if !lf.mods.iter().any(|m| m.id == id) {
                return Err("Unknown mod.".into());
            }
            lf.pins.insert(key, id.to_string());
        }
        lf.dirty = true;
        self.save(&lf)
    }

    /// Deployed files that are no longer what Gantry put there: the game, a launcher or the user
    /// replaced or deleted them (a game update does this). `(path, "missing" | "changed")`.
    pub fn drift(&self) -> Vec<(String, &'static str)> {
        let known_ids: HashSet<String> = self.load().mods.into_iter().filter(|m| m.enabled).map(|m| m.id).collect();
        if known_ids.is_empty() {
            return Vec::new();
        }
        self.journal()
            .entries
            .iter()
            .filter(|e| known_ids.contains(&e.mod_id))
            .filter_map(|e| match fs::symlink_metadata(self.game_dir.join(&e.rel)) {
                Err(_) => Some((e.rel.clone(), "missing")),
                Ok(_) if self.is_ours(e) => None,
                Ok(_) => Some((e.rel.clone(), "changed")),
            })
            .collect()
    }

    /// Remove everything Gantry deployed and put the original game files back.
    pub fn purge(&self) -> Result<PurgeReport, String> {
        let _g = lock();
        let rep = self.purge_inner()?;
        let mut lf = self.load();
        if lf.mods.iter().any(|m| m.enabled) {
            lf.dirty = true;
            self.save(&lf)?;
        }
        Ok(rep)
    }

    /// Remove every deployed mod from the game directory (restoring original backups) and wipe all staged mods.
    pub fn wipe_all(&self) -> Result<(), String> {
        let _g = lock();
        let _ = self.purge_inner();
        let _ = fs::remove_dir_all(self.root.join("mods"));
        let _ = fs::remove_dir_all(self.root.join("backups"));
        let _ = fs::remove_dir_all(self.root.join("trash"));
        let _ = self.save_journal(&Journal::default());
        let _ = fs::remove_file(fsutil::kept_copy(&self.journal_path()));
        let mut lf = self.load();
        lf.mods.clear();
        lf.pins.clear();
        lf.undo = None;
        lf.history.clear();
        lf.dirty = false;
        let _ = self.save(&lf);
        let _ = fs::remove_file(fsutil::kept_copy(&self.lib_path()));
        Ok(())
    }

    /// Make the game folder match the library: purge, then lay every enabled mod over it.
    pub fn deploy(&self, force: bool) -> Result<DeployReport, String> {
        let _g = lock();
        if !self.game_dir.is_dir() {
            return Err("The game folder does not exist (is the game installed?).".into());
        }
        if !force {
            if let Some(ac) = anti_cheat(&self.game_dir) {
                return Err(format!(
                    "This game uses {ac}. Modded files can get your account banned, so ModForge will not deploy unless you confirm."
                ));
            }
        }
        self.purge_inner()?;
        let mut lf = self.load();
        let plan = self.build_plan(&lf.mods, &lf.pins);
        let mut rep = DeployReport { conflicts: plan.conflicts.len(), ..Default::default() };

        // Phase 1: decide, create folders, and back up every original that would be overwritten.
        let mut journal = Journal::default();
        let mut made: HashSet<String> = HashSet::new();
        let mut todo: Vec<(&Planned, usize)> = Vec::new();
        for p in &plan.files {
            let dest = self.game_dir.join(&p.dest);
            let backup = match fs::symlink_metadata(&dest) {
                Ok(m) if m.is_dir() => {
                    rep.skipped.push(format!("{} (a folder of the game is in the way)", p.dest));
                    continue;
                }
                Ok(_) => true,
                Err(_) => false,
            };
            // folders that do not exist yet, parents first
            let mut acc = PathBuf::new();
            let comps: Vec<_> = Path::new(&p.dest).components().collect();
            for c in &comps[..comps.len().saturating_sub(1)] {
                acc.push(c);
                let s = fsutil::slashed(&acc);
                if !self.game_dir.join(&acc).exists() && made.insert(s.clone()) {
                    journal.created_dirs.push(s);
                }
            }
            if backup {
                let bak = self.backup_path(&p.dest);
                if bak.exists() {
                    let _ = fs::rename(&bak, bak.with_extension(format!("old-{}", fsutil::unique_token())));
                }
                move_file(&dest, &bak).map_err(|e| format!("Could not back up {}: {e}", p.dest))?;
                rep.backed_up += 1;
            }
            journal.entries.push(Placed {
                rel: p.dest.clone(),
                mod_id: lf.mods[p.mod_idx].id.clone(),
                file: p.file.clone(),
                kind: "pending".into(),
                backup,
                size: 0,
                mtime_ns: 0,
            });
            todo.push((p, journal.entries.len() - 1));
        }
        // the journal exists before the first link, so a crash can always be purged
        self.save_journal(&journal)?;

        // Phase 2: place the files.
        let mut failed: Vec<usize> = Vec::new();
        for (p, ji) in todo {
            let m = &lf.mods[p.mod_idx];
            let src = self.mod_files_dir(&m.id).join(&p.file);
            let dest = self.game_dir.join(&p.dest);
            if let Some(parent) = dest.parent() {
                if let Err(e) = fs::create_dir_all(parent) {
                    rep.skipped.push(format!("{} ({e})", p.dest));
                    failed.push(ji);
                    continue;
                }
            }
            let tmp = dest.with_file_name(format!(".gantry_{}", fsutil::unique_token()));
            let kind = match fs::hard_link(&src, &tmp) {
                Ok(()) => "hardlink",
                Err(_) => match fs::copy(&src, &tmp) {
                    Ok(_) => "copy",
                    Err(e) => {
                        let _ = fs::remove_file(&tmp);
                        rep.skipped.push(format!("{} ({e})", p.dest));
                        failed.push(ji);
                        continue;
                    }
                },
            };
            if let Err(e) = fs::rename(&tmp, &dest) {
                let _ = fs::remove_file(&tmp);
                rep.skipped.push(format!("{} ({e})", p.dest));
                failed.push(ji);
                continue;
            }
            let e = &mut journal.entries[ji];
            e.kind = kind.into();
            if let Ok(md) = fs::metadata(&dest) {
                e.size = md.len();
                e.mtime_ns = fsutil::mtime_ns(&md);
            }
            if kind == "hardlink" {
                rep.linked += 1;
            } else {
                rep.copied += 1;
            }
        }
        // put originals back for files that could not be placed
        for ji in failed.iter().rev() {
            let e = journal.entries.remove(*ji);
            if e.backup {
                let _ = move_file(&self.backup_path(&e.rel), &self.game_dir.join(&e.rel));
            }
        }
        self.save_journal(&journal)?;
        rep.overrides = self.overrides_from(&journal);
        lf.dirty = false;
        self.save(&lf)?;
        rep.misplaced = lf.mods.iter().filter(|m| m.enabled && m.target.is_empty() && self.smart_plan(&m.files).is_some()).count();
        Ok(rep)
    }

    fn overrides_from(&self, j: &Journal) -> Vec<String> {
        let mut set = BTreeSet::new();
        for e in &j.entries {
            let p = Path::new(&e.rel);
            let is_dll = p.extension().map(|x| x.eq_ignore_ascii_case("dll")).unwrap_or(false);
            if let (true, Some(stem)) = (is_dll, p.file_stem().map(|s| s.to_string_lossy().to_lowercase())) {
                if PROXY_DLLS.contains(&stem.as_str()) {
                    set.insert(stem);
                }
            }
        }
        set.into_iter().collect()
    }

    /// DLL names that currently need a Wine `native,builtin` override because a deployed mod
    /// provides them (e.g. `winhttp` for BepInEx).
    pub fn wine_overrides(&self) -> Vec<String> {
        self.overrides_from(&self.journal())
    }

    /// Everything the UI needs to show.
    pub fn status_json(&self) -> Value {
        let lf = self.load();
        let mut j = self.journal();
        if lf.mods.is_empty() && !j.entries.is_empty() {
            let _ = self.save_journal(&Journal::default());
            let _ = fs::remove_file(fsutil::kept_copy(&self.journal_path()));
            j = Journal::default();
        }
        let plan = self.build_plan(&lf.mods, &lf.pins);
        let drift = {
            let d = self.drift();
            let first: Vec<Value> = d.iter().take(5).map(|(p, why)| json!({ "path": p, "why": why })).collect();
            json!({ "count": d.len(), "files": first })
        };
        let in_conflict: HashSet<&String> = plan.conflicts.iter().flat_map(|c| c.mods.iter()).collect();
        // per mod: over whom it wins files and to whom it loses them, the most first
        let mut wins: HashMap<&str, BTreeMap<&str, usize>> = HashMap::new();
        let mut loses: HashMap<&str, BTreeMap<&str, usize>> = HashMap::new();
        for c in &plan.conflicts {
            for (id, name) in c.ids.iter().zip(&c.mods) {
                if *id != c.winner_id {
                    *wins.entry(c.winner_id.as_str()).or_default().entry(name.as_str()).or_default() += 1;
                    *loses.entry(id.as_str()).or_default().entry(c.winner.as_str()).or_default() += 1;
                }
            }
        }
        let top = |m: Option<&BTreeMap<&str, usize>>| -> Vec<Value> {
            let mut v: Vec<(&str, usize)> = m.map(|m| m.iter().map(|(k, n)| (*k, *n)).collect()).unwrap_or_default();
            v.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(b.0)));
            v.into_iter().take(2).map(|(k, n)| json!({ "mod": k, "files": n })).collect()
        };
        let known_ids: HashSet<String> = lf.mods.iter().map(|m| m.id.clone()).collect();
        let deployed_count = j.entries.iter().filter(|e| known_ids.contains(&e.mod_id)).count();
        let ours: HashSet<String> = j.entries.iter().map(|e| e.rel.to_lowercase()).collect();
        let overwrites = plan
            .files
            .iter()
            .filter(|p| fs::symlink_metadata(self.game_dir.join(&p.dest)).is_ok() && !ours.contains(&p.dest.to_lowercase()))
            .count();
        json!({
            "root": self.root.to_string_lossy(),
            "dirty": lf.dirty,
            "deployed": deployed_count,
            "overwrites": overwrites,
            "overrides": self.overrides_from(&j),
            "anti_cheat": if lf.mods.is_empty() { None } else { anti_cheat(&self.game_dir) },
            "conflicts": plan.conflicts,
            "profiles": lf.profiles.keys().collect::<Vec<_>>(),
            "undo": lf.undo.as_ref().map(|u| u.label.clone()),
            "drift": drift,
            "history": lf.history.iter().rev().map(|h| json!({ "label": h.label, "ts": h.ts })).collect::<Vec<_>>(),
            "trash": self.trash_list().into_iter().map(|(dir, name, version, size, removed_ms)| json!({ "dir": dir, "name": name, "version": version, "size": size, "removed_ms": removed_ms })).collect::<Vec<_>>(),
            "previous": self.newest_previous(&lf.mods).into_iter().map(|(id, (dir, version))| (id, json!({ "dir": dir, "version": version }))).collect::<serde_json::Map<String, Value>>(),
            "mods": lf.mods.iter().map(|m| json!({
                "id": m.id, "name": m.name, "version": m.version, "enabled": m.enabled, "target": m.target,
                "nexus": m.nexus, "thunderstore": m.thunderstore, "modio": m.modio, "github": m.github.is_some(),
                "source": m.source, "files": m.files.len(), "size": m.size, "added_ms": m.added_ms, "notes": m.notes.len(), "placed_by": m.placed_by, "note": m.note, "group": m.group, "depends_on": m.depends_on, "update_rule": m.update_rule, "verified": m.verified, "programs": m.programs,
                "needed_by": m.thunderstore.as_ref().map(|t| lf.mods.iter().filter(|o| o.id != m.id && o.depends_on.iter().any(|d| d.eq_ignore_ascii_case(&t.full_name()))).map(|o| o.name.clone()).collect::<Vec<_>>()).unwrap_or_default(),
                "conflicts": in_conflict.contains(&m.name),
                "wins": top(wins.get(m.id.as_str())),
                "loses": top(loses.get(m.id.as_str())),
            })).collect::<Vec<_>>(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fsutil::testutil::TempDir;

    fn write(p: &Path, s: &str) {
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, s).unwrap();
    }

    fn read(p: &Path) -> String {
        fs::read_to_string(p).unwrap_or_else(|_| "<missing>".into())
    }

    /// A game with a `Data` folder and a `Game/Binaries/Win64` folder.
    fn setup(tag: &str) -> (TempDir, PathBuf, Library) {
        let d = TempDir::new(tag);
        let game = d.join("lib/common/Cool Game");
        write(&game.join("Data/base.esp"), "ORIGINAL");
        write(&game.join("Game/Binaries/Win64/game.exe"), "MZ");
        let lib = Library::locate(&game, &d.join("data"), "1");
        (d, game, lib)
    }

    fn stage(d: &TempDir, tag: &str, files: &[(&str, &str)]) -> PathBuf {
        let s = d.join(&format!("stage-{tag}"));
        for (p, c) in files {
            write(&s.join(p), c);
        }
        s
    }

    #[test]
    fn staging_sits_beside_the_game_and_survives_the_game_folder() {
        let (_d, game, lib) = setup("loc");
        assert_eq!(lib.root(), game.parent().unwrap().join(".gantry/Cool Game"));
    }

    #[test]
    fn nexus_style_names_become_a_name_and_a_version() {
        assert_eq!(friendly_name("Better Lighting-123-1-2-1700000000.zip"), ("Better Lighting".into(), "1.2".into()));
        assert_eq!(friendly_name("plain_mod.7z"), ("plain mod".into(), String::new()));
        assert_eq!(friendly_name("My Mod v2.zip"), ("My Mod v2".into(), String::new()));
    }

    #[test]
    fn a_wrapper_folder_is_stripped_but_real_game_folders_are_kept() {
        let (d, game, lib) = setup("wrap");
        let s = stage(&d, "a", &[("Cool Mod 1.2/Data/new.esp", "NEW"), ("Cool Mod 1.2/readme.txt", "hi")]);
        // two top-level entries after the wrapper: Data (exists) + loose file → wrapper stripped once
        let m = lib.add_from_dir(&s, "Cool Mod", "cool.zip").unwrap();
        assert_eq!(m.files, vec!["Data/new.esp"]);
        assert_eq!(m.notes, vec!["readme.txt"], "the readme is kept for reading, not laid into the game");
        assert_eq!(m.target, "");
        let s = stage(&d, "b", &[("Data/other.esp", "O")]);
        let m2 = lib.add_from_dir(&s, "Other", "o.zip").unwrap();
        assert_eq!(m2.files, vec!["Data/other.esp"], "Data is a real game folder, not a wrapper");
        assert!(game.join("Data/base.esp").is_file());
    }

    #[test]
    fn mods_that_belong_one_level_down_get_the_right_target() {
        let (d, _game, lib) = setup("target");
        let s = stage(&d, "a", &[("Binaries/Win64/mod.dll", "DLL")]);
        let m = lib.add_from_dir(&s, "Deep", "d.zip").unwrap();
        assert_eq!(m.target, "Game", "Binaries lives in Game/");
        assert_eq!(m.files, vec!["Binaries/Win64/mod.dll"]);
        // wrapper + deep folder
        let s = stage(&d, "b", &[("Wrapper/Binaries/Win64/b.dll", "B")]);
        let m = lib.add_from_dir(&s, "Deep2", "d2.zip").unwrap();
        assert_eq!((m.target.as_str(), m.files[0].as_str()), ("Game", "Binaries/Win64/b.dll"));
    }

    #[test]
    fn unreal_pak_mods_go_to_the_paks_mods_folder() {
        let (d, game, lib) = setup("unrealpak");
        write(&game.join("Game/Content/Paks/pakchunk0-Windows.pak"), "BASE");
        assert_eq!(lib.find_paks_rel().as_deref(), Some("Game/Content/Paks"));
        let m = lib.add_from_dir(&stage(&d, "a", &[("Cool_P.pak", "P"), ("Cool_P.utoc", "T"), ("Cool_P.ucas", "U")]), "Cool", "c.zip").unwrap();
        assert_eq!(m.target, "Game/Content/Paks/~mods");
        let ui = lib.add_from_dir(&stage(&d, "b", &[("HSTM_UI.pak", "U")]), "Ui", "u.zip").unwrap();
        assert_eq!(ui.target, "Game/Content/Paks/LogicMods");
        // paks together with other files are not guessed at
        let mixed = lib.add_from_dir(&stage(&d, "c", &[("x.pak", "X"), ("readme.txt", "r")]), "Mixed", "m.zip").unwrap();
        assert_eq!(mixed.target, "");
        // a pak already inside its folders is left where its author put it
        let full = lib.add_from_dir(&stage(&d, "d", &[("Content/Paks/~mods/F.pak", "F"), ("Content/Paks/~mods/F.ucas", "F")]), "Full", "f.zip").unwrap();
        assert_eq!(full.target, "Game");
        lib.deploy(false).unwrap();
        for f in ["Game/Content/Paks/~mods/Cool_P.pak", "Game/Content/Paks/~mods/Cool_P.ucas", "Game/Content/Paks/LogicMods/HSTM_UI.pak", "Game/Content/Paks/~mods/F.pak"] {
            assert!(game.join(f).is_file(), "{f}");
        }
        lib.purge().unwrap();
        assert!(!game.join("Game/Content/Paks/~mods").exists() && game.join("Game/Content/Paks/pakchunk0-Windows.pak").is_file());
        // a game without a Paks folder gets nothing special
        let (d2, _g2, lib2) = setup("nopak");
        assert!(lib2.find_paks_rel().is_none());
        assert_eq!(lib2.add_from_dir(&stage(&d2, "a", &[("Cool_P.pak", "P")]), "Cool", "c.zip").unwrap().target, "");
    }

    #[test]
    fn bethesda_plugins_and_assets_target_data_folder() {
        let d = TempDir::new("bethesda_deploy");
        let game = d.join("Skyrim Special Edition");
        write(&game.join("SkyrimSE.exe"), "MZ");
        write(&game.join("Data/Skyrim.esm"), "BASE");
        let lib = Library::locate(&game, &d.join("data"), "skyrim");

        // Loose .esp mod should target Data
        let s1 = stage(&d, "esp_mod", &[("MySword.esp", "ESP"), ("MySword.bsa", "BSA")]);
        let m1 = lib.add_from_dir(&s1, "MySword", "sword.zip").unwrap();
        assert_eq!(m1.target, "Data");

        // Mod with meshes/textures folders should target Data
        let s2 = stage(&d, "texture_mod", &[("textures/weapons/sword.dds", "DDS"), ("meshes/weapons/sword.nif", "NIF")]);
        let m2 = lib.add_from_dir(&s2, "SwordTextures", "textures.zip").unwrap();
        assert_eq!(m2.target, "Data");

        // Mod already rooted in Data should keep target empty (relative to root)
        let s3 = stage(&d, "full_mod", &[("Data/Another.esp", "ESP")]);
        let m3 = lib.add_from_dir(&s3, "FullMod", "full.zip").unwrap();
        assert_eq!(m3.target, "");
    }

    #[test]
    fn godot_pck_mods_target_mods_folder() {
        let d = TempDir::new("godot_deploy");
        let game = d.join("Brotato");
        write(&game.join("Brotato.exe"), "MZ");
        write(&game.join("Brotato.pck"), "PCK");
        write(&game.join("mods/base.txt"), "BASE");
        let lib = Library::locate(&game, &d.join("data"), "brotato");

        let s1 = stage(&d, "pck_mod", &[("CoolMod.pck", "PCK")]);
        let m1 = lib.add_from_dir(&s1, "CoolMod", "cool.zip").unwrap();
        assert_eq!(m1.target, "mods");
    }

    #[test]
    fn source_vpk_and_custom_assets_target_custom_or_addons() {
        let d = TempDir::new("source_deploy");
        let hl2 = d.join("Half-Life 2");
        write(&hl2.join("hl2.exe"), "MZ");
        write(&hl2.join("hl2/gameinfo.txt"), "GameInfo");
        write(&hl2.join("hl2/custom/readme.txt"), "Custom");
        let lib_hl2 = Library::locate(&hl2, &d.join("data"), "hl2");

        let s1 = stage(&d, "vpk_mod", &[("skins.vpk", "VPK")]);
        let m1 = lib_hl2.add_from_dir(&s1, "Skins", "skins.zip").unwrap();
        assert_eq!(m1.target, "hl2/custom");

        let l4d2 = d.join("Left 4 Dead 2");
        write(&l4d2.join("left4dead2.exe"), "MZ");
        write(&l4d2.join("left4dead2/gameinfo.txt"), "GameInfo");
        let lib_l4d2 = Library::locate(&l4d2, &d.join("data"), "l4d2");

        let s2 = stage(&d, "campaign_vpk", &[("campaign.vpk", "VPK")]);
        let m2 = lib_l4d2.add_from_dir(&s2, "Campaign", "camp.zip").unwrap();
        assert_eq!(m2.target, "left4dead2/addons");
    }

    #[test]
    fn a_big_library_is_listed_and_deployed_quickly() {
        let (d, game, lib) = setup("big");
        let t0 = std::time::Instant::now();
        for i in 0..400 {
            let name = format!("Mod {i}");
            lib.add_from_dir(&stage(&d, &format!("m{i}"), &[(&format!("Data/m{i}.esp"), "x"), (&format!("Data/m{i}/a.txt"), "y")]), &name, "m.zip").unwrap();
        }
        let added = t0.elapsed();
        let t1 = std::time::Instant::now();
        let status = lib.status_json();
        let listed = t1.elapsed();
        let t2 = std::time::Instant::now();
        lib.deploy(false).unwrap();
        let deployed = t2.elapsed();
        eprintln!("400 mods: add {added:?}, status {listed:?}, deploy {deployed:?}");
        assert_eq!(status["mods"].as_array().unwrap().len(), 400);
        assert!(game.join("Data/m399.esp").exists());
        // file systems of other systems (and their virus scanners) are much slower: only Linux and macOS are held to a time
        assert!(cfg!(windows) || listed < std::time::Duration::from_secs(2), "listing took {listed:?}");
        assert!(cfg!(windows) || deployed < std::time::Duration::from_secs(10), "deploying took {deployed:?}");
    }

    #[test]
    fn the_disk_check_counts_hard_links_and_old_save_copies() {
        let (d, _game, lib) = setup("diskcheck");
        lib.add_from_dir(&stage(&d, "a", &[("Data/a.esp", "AAAA")]), "A", "a.zip").unwrap();
        lib.deploy(false).unwrap();
        let v = lib.verify();
        assert_eq!(v.linked_bytes, 4, "one hard-linked file of 4 bytes is not stored twice");
        // two copies of replaced saves: one from long ago, one from now
        let replaced = lib.root().join("profile_saves/_replaced");
        write(&replaced.join("1000/slot.sav"), "old save");
        write(&replaced.join(format!("{}/slot.sav", fsutil::now_ms())), "new save");
        let v = lib.verify();
        assert_eq!(v.old_saves_bytes, 8);
        assert!(v.saves_bytes >= 16);
        assert_eq!(lib.clean_old_save_backups(), 8);
        assert!(!replaced.join("1000").exists() && replaced.read_dir().unwrap().count() == 1, "only the old copy went");
    }

    #[test]
    fn mods_can_be_dragged_switched_grouped_and_removed_in_bulk() {
        let (d, game, lib) = setup("bulk");
        let a = lib.add_from_dir(&stage(&d, "a", &[("Data/a.esp", "A")]), "A", "a.zip").unwrap();
        let b = lib.add_from_dir(&stage(&d, "b", &[("Data/b.esp", "B")]), "B", "b.zip").unwrap();
        let c = lib.add_from_dir(&stage(&d, "c", &[("Data/c.esp", "C")]), "C", "c.zip").unwrap();
        let names = |lib: &Library| lib.mods().iter().map(|m| m.name.clone()).collect::<Vec<_>>();
        lib.move_to(&c.id, 0).unwrap();
        assert_eq!(names(&lib), vec!["C", "A", "B"]);
        lib.move_to(&c.id, 99).unwrap();
        assert_eq!(names(&lib), vec!["A", "B", "C"], "a place past the end means last");
        assert!(lib.move_to("nope", 0).is_err());

        // two switches in one step: one change in the history, one undo takes both back
        assert_eq!(lib.set_enabled_many(&[a.id.clone(), b.id.clone()], false).unwrap(), 2);
        assert_eq!(lib.set_enabled_many(&[a.id.clone(), b.id.clone()], false).unwrap(), 0, "nothing to do the second time");
        assert!(lib.mods().iter().filter(|m| !m.enabled).count() == 2);
        assert_eq!(lib.undo().unwrap(), "Disabled 2 mods");
        assert!(lib.mods().iter().all(|m| m.enabled));

        assert_eq!(lib.set_group(&[a.id.clone(), c.id.clone()], "  Visual ").unwrap(), 2);
        let m = lib.mods();
        assert_eq!(m.iter().filter(|m| m.group == "Visual").count(), 2);
        assert_eq!(lib.set_group(std::slice::from_ref(&a.id), "").unwrap(), 1, "an empty label clears the group");

        lib.deploy(false).unwrap();
        assert_eq!(lib.remove_many(&[a.id.clone(), b.id.clone(), "ghost".into()]).unwrap(), 2);
        assert_eq!(names(&lib), vec!["C"]);
        assert_eq!(lib.trash_list().len(), 2, "both went to the trash and can come back");
        lib.deploy(false).unwrap();
        assert!(!game.join("Data/a.esp").exists() && game.join("Data/c.esp").exists());
    }

    #[test]
    fn profiles_switch_whole_sets_of_mods_and_the_last_change_can_be_undone() {
        let (d, game, lib) = setup("profiles");
        let a = lib.add_from_dir(&stage(&d, "a", &[("Data/a.esp", "A")]), "A", "a.zip").unwrap();
        let b = lib.add_from_dir(&stage(&d, "b", &[("Data/b.esp", "B")]), "B", "b.zip").unwrap();
        let c = lib.add_from_dir(&stage(&d, "c", &[("Data/c.esp", "C")]), "C", "c.zip").unwrap();
        lib.deploy(false).unwrap();
        assert_eq!(lib.save_profile("  Everything ").unwrap(), "Everything");

        lib.set_enabled(&a.id, false).unwrap();
        lib.set_enabled(&c.id, false).unwrap();
        lib.move_mod(&b.id, -1).unwrap();
        lib.save_profile("Only B").unwrap();
        // a mod added after the profiles were saved is unknown to them
        let late = lib.add_from_dir(&stage(&d, "late", &[("Data/late.esp", "L")]), "Late", "l.zip").unwrap();

        assert_eq!(lib.load_profile("Everything").unwrap(), 3, "A and C come back on; Late goes off, as the profile does not know it");
        let m = lib.mods();
        let state = |id: &str| m.iter().find(|x| x.id == id).unwrap().enabled;
        assert!(state(&a.id) && state(&b.id) && state(&c.id) && !state(&late.id));
        assert_eq!(m.iter().map(|x| x.name.as_str()).collect::<Vec<_>>(), vec!["A", "B", "C", "Late"], "the profile's order is restored too");
        assert_eq!(lib.matching_profile().as_deref(), Some("Everything"), "what is on is exactly that profile");
        lib.deploy(false).unwrap();
        assert!(game.join("Data/a.esp").exists() && !game.join("Data/late.esp").exists());

        // back to the previous situation with one undo
        assert_eq!(lib.undo().unwrap(), "Loaded profile 'Everything'");
        let m = lib.mods();
        assert_eq!(m.iter().map(|x| (x.name.as_str(), x.enabled)).collect::<Vec<_>>(), vec![("B", true), ("A", false), ("C", false), ("Late", true)]);
        assert!(lib.undo().is_err(), "one step back only");
        assert_eq!(lib.status_json()["profiles"], json!(["Everything", "Only B"]));

        // single changes are undoable too
        lib.set_enabled(&b.id, false).unwrap();
        assert_eq!(lib.status_json()["undo"], "Disabled B");
        assert_eq!(lib.undo().unwrap(), "Disabled B");
        assert!(lib.mods()[0].enabled);
        lib.set_target(&a.id, "Sub").unwrap();
        assert_eq!(lib.undo().unwrap(), "Changed the folder of A");
        assert_eq!(lib.mods().iter().find(|x| x.id == a.id).unwrap().target, "");

        // errors are explained
        assert!(lib.load_profile("nope").is_err() && lib.delete_profile("nope").is_err());
        assert!(lib.save_profile("   ").is_err() && lib.save_profile(&"x".repeat(41)).is_err() && lib.save_profile("bad\nname").is_err());
        lib.load_profile("Only B").unwrap();
        lib.set_enabled(&c.id, true).unwrap();
        assert_eq!(lib.matching_profile(), None);
        lib.set_enabled(&c.id, false).unwrap();
        lib.remove_many(std::slice::from_ref(&a.id)).unwrap();
        assert_eq!(lib.matching_profile().as_deref(), Some("Only B"), "a removed mod does not count against a profile");
        lib.delete_profile("Only B").unwrap();
        assert_eq!(lib.status_json()["profiles"], json!(["Everything"]));
    }

    #[test]
    fn safe_mode_disables_content_mods_while_preserving_loaders_and_restores_them() {
        let (d, _game, lib) = setup("safemode");
        let loader = lib.add_from_dir(&stage(&d, "loader", &[("winhttp.dll", "hook"), ("loader.ini", "cfg")]), "UE4SS Loader", "loader.zip").unwrap();
        let content1 = lib.add_from_dir(&stage(&d, "c1", &[("Data/textures.pak", "tex")]), "HD Textures", "c1.zip").unwrap();
        let content2 = lib.add_from_dir(&stage(&d, "c2", &[("Data/weapons.esp", "esp")]), "Big Swords", "c2.zip").unwrap();

        assert_eq!(lib.stage_safe_mode().unwrap(), 2);
        let m = lib.mods();
        assert!(m.iter().find(|x| x.id == loader.id).unwrap().enabled, "loader remains enabled");
        assert!(!m.iter().find(|x| x.id == content1.id).unwrap().enabled, "content mod disabled");
        assert!(!m.iter().find(|x| x.id == content2.id).unwrap().enabled, "content mod disabled");

        lib.restore_safe_mode().unwrap();
        let m2 = lib.mods();
        assert!(m2.iter().find(|x| x.id == loader.id).unwrap().enabled);
        assert!(m2.iter().find(|x| x.id == content1.id).unwrap().enabled);
        assert!(m2.iter().find(|x| x.id == content2.id).unwrap().enabled);
    }

    #[test]
    fn bepinex_plugins_land_in_bepinex_plugins_and_store_metadata_stays_out() {
        let (d, game, lib) = setup("bepinex");
        // no BepInEx yet: a loose dll is just a file in the game folder
        let plain = lib.add_from_dir(&stage(&d, "p", &[("helper.dll", "H"), ("notes.txt", "n")]), "Plain", "p.zip").unwrap();
        assert_eq!(plain.target, "");
        lib.remove(&plain.id).unwrap();

        // BepInEx itself (a Thunderstore "pack": wrapper folder + its own BepInEx folder)
        let core = lib
            .add_from_dir(
                &stage(&d, "c", &[("BepInExPack/winhttp.dll", "W"), ("BepInExPack/doorstop_config.ini", "I"), ("BepInExPack/BepInEx/core/BepInEx.dll", "B"), ("BepInExPack/BepInEx/plugins/.keep", "")]),
                "BepInExPack",
                "pack.zip",
            )
            .unwrap();
        assert_eq!((core.target.as_str(), core.files.contains(&"winhttp.dll".to_string())), ("", true));
        lib.deploy(false).unwrap();
        assert!(game.join("BepInEx/core/BepInEx.dll").is_file());

        // a plugin: loose dll + Thunderstore metadata → BepInEx/plugins, metadata dropped
        let plugin = lib
            .add_from_dir(&stage(&d, "pl", &[("manifest.json", "{}"), ("icon.png", "PNG"), ("README.md", "r"), ("CoolMod.dll", "C"), ("CoolMod.pdb", "P")]), "CoolMod", "cool.zip")
            .unwrap();
        assert_eq!(plugin.target, "BepInEx/plugins");
        assert_eq!(plugin.files, vec!["CoolMod.dll", "CoolMod.pdb"]);
        // a plugin shipped as plugins/<Name>/..., and patchers
        let nested = lib.add_from_dir(&stage(&d, "n", &[("plugins/Other/Other.dll", "O"), ("patchers/Fix.dll", "F")]), "Other", "o.zip").unwrap();
        assert_eq!(nested.target, "BepInEx", "plugins/ and patchers/ are laid onto BepInEx/");
        // one that already carries BepInEx/ is left alone
        let full = lib.add_from_dir(&stage(&d, "f", &[("BepInEx/plugins/Full.dll", "F"), ("readme.txt", "r")]), "Full", "f.zip").unwrap();
        assert_eq!(full.target, "");
        lib.deploy(false).unwrap();
        for f in ["BepInEx/plugins/CoolMod.dll", "BepInEx/plugins/Other/Other.dll", "BepInEx/patchers/Fix.dll", "BepInEx/plugins/Full.dll", "winhttp.dll"] {
            assert!(game.join(f).is_file(), "{f}");
        }
        assert!(!game.join("BepInEx/plugins/manifest.json").exists() && !game.join("icon.png").exists());
        assert_eq!(lib.wine_overrides(), vec!["winhttp"]);
        lib.purge().unwrap();
        assert!(!game.join("BepInEx").exists(), "everything Gantry created is gone again");
    }

    #[test]
    fn the_real_thunderstore_layouts_of_bepinexpack_and_r2api_are_understood_even_before_a_deploy() {
        // file lists of bbepis-BepInExPack-5.4.2122 and RiskofThunder-R2API_Core as published on Thunderstore
        let (d, game, lib) = setup("realts");
        let pack = lib
            .add_from_dir(
                &stage(&d, "p", &[
                    ("CHANGELOG.md", "c"), ("icon.png", "i"), ("manifest.json", "{}"), ("README.md", "r"),
                    ("BepInExPack/.doorstop_version", "4"), ("BepInExPack/doorstop_config.ini", "I"), ("BepInExPack/winhttp.dll", "W"),
                    ("BepInExPack/BepInEx/core/BepInEx.dll", "B"), ("BepInExPack/BepInEx/core/0Harmony.dll", "H"),
                ]),
                "BepInExPack",
                "pack.zip",
            )
            .unwrap();
        assert_eq!(pack.target, "");
        assert!(pack.files.contains(&"winhttp.dll".to_string()) && pack.files.contains(&"BepInEx/core/BepInEx.dll".to_string()));
        assert!(!pack.files.iter().any(|f| f == "manifest.json" || f == "icon.png" || f == "README.md" || f == "CHANGELOG.md"));
        // not deployed yet (e.g. the game is running): the library still knows BepInEx is coming
        let r2api = lib
            .add_from_dir(
                &stage(&d, "r", &[("icon.png", "i"), ("README.md", "r"), ("manifest.json", "{}"), ("plugins/R2API.Core/R2API.Core.dll", "D"), ("plugins/R2API.Core/R2API.Core.xml", "X")]),
                "R2API Core",
                "r2.zip",
            )
            .unwrap();
        assert_eq!(r2api.target, "BepInEx");
        lib.deploy(false).unwrap();
        assert!(game.join("BepInEx/plugins/R2API.Core/R2API.Core.dll").is_file() && game.join("winhttp.dll").is_file() && game.join(".doorstop_version").is_file());
        assert!(!game.join("manifest.json").exists());
    }

    #[test]
    fn junk_and_empty_mods_are_handled() {
        let (d, _g, lib) = setup("junk");
        let s = stage(&d, "a", &[("__MACOSX/x", "1"), (".DS_Store", "1"), ("Thumbs.db", "1")]);
        assert!(lib.add_from_dir(&s, "Nothing", "n.zip").is_err());
        assert!(lib.mods().is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn symlinks_in_an_archive_are_never_followed_or_installed() {
        let (d, _g, lib) = setup("symlink");
        let s = stage(&d, "a", &[("Data/ok.esp", "OK")]);
        std::os::unix::fs::symlink("/etc/passwd", s.join("Data/evil.esp")).unwrap();
        let m = lib.add_from_dir(&s, "Linked", "l.zip").unwrap();
        assert_eq!(m.files, vec!["Data/ok.esp"]);
    }

    /// A racing game that keeps its cars in `VEHICLES/`, where mod authors ship them in whatever folder they like.
    fn racing_setup(tag: &str) -> (TempDir, PathBuf, Library) {
        let d = TempDir::new(tag);
        let game = d.join("lib/common/Racer");
        for n in ["VEH_1_HI.BNDL", "VEH_1_LO.BNDL", "VEH_2_HI.BNDL", "VEH_2_LO.BNDL", "VEH_3_HI.BNDL", "VEH_3_LO.BNDL"] {
            write(&game.join("VEHICLES").join(n), "ORIGINAL");
        }
        write(&game.join("Readme.txt"), "game readme");
        write(&game.join("Core/dinput8.dll"), "dll");
        write(&game.join("Racer.exe"), "MZ");
        let lib = Library::locate(&game, &d.join("data"), "1");
        (d, game, lib)
    }

    #[test]
    fn a_redmod_that_was_placed_in_the_root_before_is_moved_into_its_folder_at_the_next_deploy() {
        let d = TempDir::new("redmod-fix");
        let game = d.join("lib/common/Cyberpunk 2077");
        write(&game.join("bin/x64/Cyberpunk2077.exe"), "MZ");
        let lib = Library::locate(&game, &d.join("data"), "1091500");
        // an older Gantry did not know the game: the mod landed in the root
        let s = stage(&d, "npc", &[("info.json", "{\"name\": \"NPCs Gone Wild\"}"), ("archives/a.archive", "X")]);
        lib.add_with(&s, "NPCs Gone Wild", "n.zip", None, "").unwrap();
        lib.deploy(false).unwrap();
        assert!(game.join("archives/a.archive").is_file());
        assert!(lib.fix_redmod_targets().is_empty(), "a game without Cyberpunk's folders is left alone");
        write(&game.join("r6/config/settings.json"), "{}");
        assert_eq!(lib.fix_redmod_targets(), vec!["NPCs Gone Wild".to_string()]);
        assert_eq!(lib.mods()[0].target, "mods/NPCs Gone Wild");
        assert!(lib.fix_redmod_targets().is_empty(), "once is enough");
        lib.deploy(false).unwrap();
        assert!(game.join("mods/NPCs Gone Wild/archives/a.archive").is_file());
        assert!(!game.join("archives").exists() && !game.join("info.json").exists(), "the root is clean again");
        lib.undo().unwrap();
        assert_eq!(lib.mods()[0].target, "", "and it can be undone");
    }

    #[test]
    fn mods_named_after_their_whole_download_get_their_real_name_once() {
        let (d, _game, lib) = racing_setup("tidy");
        let s = stage(&d, "t", &[("VEHICLES/VEH_3_HI.BNDL", "A")]);
        let raw = "Fast Cars 790 2.14 2026-10-21T20-49Z psSjsvvdX";
        let m = lib.add_with(&s, raw, "Fast Cars 790 2.14 2026-10-21T20-49Z psSjsvvdX.zip", None, "").unwrap();
        let s2 = stage(&d, "t2", &[("VEHICLES/VEH_3_LO.BNDL", "B")]);
        let kept = lib.add_with(&s2, "My own name", "Slow Cars 5 1.0 2026-10-21T20-49Z abc.zip", None, "").unwrap();
        assert_eq!(lib.tidy_names(), 1);
        let after = lib.mods();
        let a = after.iter().find(|x| x.id == m.id).unwrap();
        assert_eq!((a.name.as_str(), a.version.as_str()), ("Fast Cars", "2.14"));
        assert_eq!(after.iter().find(|x| x.id == kept.id).unwrap().name, "My own name", "a name the user chose stays");
        assert_eq!(lib.tidy_names(), 0, "done once");
    }

    #[test]
    fn a_redmod_goes_into_its_own_folder_under_the_games_mods_folder() {
        let d = TempDir::new("redmod");
        let game = d.join("lib/common/Cyberpunk 2077");
        write(&game.join("r6/config/settings.json"), "{}");
        write(&game.join("archive/pc/content/base.archive"), "A");
        write(&game.join("bin/x64/Cyberpunk2077.exe"), "MZ");
        let lib = Library::locate(&game, &d.join("data"), "1091500");
        // as the author zipped it: the manifest and the archives folder, no mods/<name> around them
        let s = stage(&d, "npc", &[("info.json", "{\"name\": \"NPCs Gone Wild\", \"version\": \"4.3.2\"}"), ("archives/00NPC_GW.archive", "X")]);
        let m = lib.add_with(&s, "NPCs Gone Wild", "NPCs Gone Wild-1436-4-3-2-1671831997.zip", None, "4.3.2").unwrap();
        assert_eq!(m.target, "mods/NPCs Gone Wild");
        lib.deploy(false).unwrap();
        assert!(game.join("mods/NPCs Gone Wild/archives/00NPC_GW.archive").is_file() && game.join("mods/NPCs Gone Wild/info.json").is_file());
        assert!(!game.join("info.json").exists() && !game.join("archives").exists(), "nothing lands in the game's root");
        // a wrapper folder is still removed, and a plain mod with an info.json and no REDmod folders is left alone
        let wrapped = stage(&d, "w", &[("Cool Cars/info.json", "{\"name\": \"Cool Cars\"}"), ("Cool Cars/tweaks/cars.yaml", "t")]);
        assert_eq!(lib.preview(&wrapped).unwrap().target, "mods/Cool Cars");
        let plain = stage(&d, "p", &[("info.json", "{}"), ("readme.txt", "r")]);
        assert_eq!(lib.preview(&plain).unwrap().target, "");
        // a game without Cyberpunk's folders never gets this treatment
        let other = d.join("lib/common/Other");
        write(&other.join("Other.exe"), "MZ");
        let olib = Library::locate(&other, &d.join("data"), "2");
        assert_eq!(olib.preview(&stage(&d, "o", &[("info.json", "{\"name\": \"x\"}"), ("archives/a.archive", "x")])).unwrap().target, "");
    }

    #[test]
    fn cyberpunk_mods_keep_their_top_folder_even_before_the_game_has_it() {
        let d = TempDir::new("cp-top");
        let game = d.join("lib/common/Cyberpunk 2077");
        write(&game.join("bin/x64/Cyberpunk2077.exe"), "MZ");
        let lib = Library::locate(&game, &d.join("data"), "1091500");
        // a RED4ext plugin before RED4ext itself made the folder, and Redscript into a game without `engine`
        let plugin = stage(&d, "pl", &[("red4ext/plugins/Thing/Thing.dll", "x")]);
        let m = lib.add_with(&plugin, "Thing", "Thing-1.zip", None, "1").unwrap();
        assert_eq!(m.files, vec!["red4ext/plugins/Thing/Thing.dll"]);
        let rs = stage(&d, "rs", &[("engine/tools/scc.exe", "s")]);
        assert_eq!(lib.add_with(&rs, "Redscript", "redscript.zip", None, "1").unwrap().files, vec!["engine/tools/scc.exe"]);
        // a real wrapper folder is still taken off
        let wrapped = stage(&d, "wr", &[("Thing-1.2/red4ext/plugins/Other/Other.dll", "x")]);
        assert_eq!(lib.add_with(&wrapped, "Other", "Other.zip", None, "1").unwrap().files, vec!["red4ext/plugins/Other/Other.dll"]);
        // other games keep the old rule: one unknown folder is a wrapper
        let other = d.join("lib/common/Other");
        write(&other.join("Other.exe"), "MZ");
        let olib = Library::locate(&other, &d.join("data"), "2");
        assert_eq!(olib.add_with(&stage(&d, "o", &[("engine/a.dll", "x")]), "A", "a.zip", None, "1").unwrap().files, vec!["a.dll"]);
    }

    #[test]
    fn a_mod_that_clashes_with_the_games_folders_fails_cleanly_and_leaves_the_game_as_it_was() {
        let d = TempDir::new("clash");
        let game = d.join("lib/common/Clash");
        write(&game.join("Data"), "I am a file, not a folder");
        write(&game.join("Folder/inner.txt"), "ORIGINAL");
        write(&game.join("Clash.exe"), "MZ");
        let lib = Library::locate(&game, &d.join("data"), "7");
        // a mod that needs a folder where the game has a file, and a file where the game has a folder
        let s = stage(&d, "c", &[("Data/x.esp", "X"), ("Folder", "now a file"), ("ok.txt", "fine")]);
        lib.add_with_target(&s, "Clash", "c.zip", None, "", Some("")).unwrap();
        let r = lib.deploy(false);
        // whatever it decided, the game's own things are intact and nothing is half done
        assert_eq!(fs::read_to_string(game.join("Data")).unwrap(), "I am a file, not a folder");
        assert_eq!(fs::read_to_string(game.join("Folder/inner.txt")).unwrap(), "ORIGINAL");
        if r.is_err() {
            assert!(!game.join("ok.txt").exists(), "a failed deploy leaves nothing behind");
        }
        lib.purge().unwrap();
        assert_eq!(fs::read_to_string(game.join("Data")).unwrap(), "I am a file, not a folder");
        assert_eq!(fs::read_to_string(game.join("Folder/inner.txt")).unwrap(), "ORIGINAL");
        assert!(!game.join("ok.txt").exists());
    }

    #[test]
    fn a_preview_says_where_files_would_go_and_a_chosen_folder_beats_the_guess() {
        let (d, game, lib) = racing_setup("preview");
        let s = stage(&d, "p", &[("Replace/VEH_2_HI.BNDL", "H"), ("Replace/VEH_2_LO.BNDL", "L"), ("Readme.txt", "r")]);
        let pv = lib.preview(&s).unwrap();
        assert_eq!((pv.placed_by.as_str(), pv.files, pv.notes, pv.folders.clone()), ("names", 2, 1, vec![("VEHICLES".to_string(), 2)]));
        assert!(s.join("Replace/VEH_2_HI.BNDL").exists(), "a preview moves nothing");
        // the user says otherwise
        let m = lib.add_with_target(&s, "Pack", "p.zip", None, "", Some("Custom/Cars")).unwrap();
        assert_eq!((m.target.as_str(), m.placed_by.as_str()), ("Custom/Cars", "manual"));
        assert_eq!(m.files, vec!["Replace/VEH_2_HI.BNDL", "Replace/VEH_2_LO.BNDL"], "files keep the author's layout under the chosen folder");
        lib.deploy(false).unwrap();
        assert!(game.join("Custom/Cars/Replace/VEH_2_HI.BNDL").is_file());
        // a mod whose own folders fit the game needs no question
        let ok = stage(&d, "ok", &[("VEHICLES/VEH_3_HI.BNDL", "A"), ("VEHICLES/VEH_3_LO.BNDL", "B")]);
        assert_eq!(lib.preview(&ok).unwrap().placed_by, "");
        assert!(lib.add_with_target(&ok, "Bad", "b.zip", None, "", Some("../out")).is_err());
    }

    #[test]
    fn loose_files_go_beside_the_game_files_they_replace() {
        let (d, game, lib) = racing_setup("smartplace");
        // car mods as authors ship them: at the top, in invented folders, with readmes and pictures
        let loose = stage(&d, "loose", &[("VEH_1_HI.BNDL", "ONE_HI"), ("VEH_1_LO.BNDL", "ONE_LO"), ("Readme.txt", "by me"), ("preview.jpg", "img")]);
        let m = lib.add_from_dir(&loose, "Loose Car", "l.zip").unwrap();
        assert_eq!(m.files, vec!["VEHICLES/VEH_1_HI.BNDL", "VEHICLES/VEH_1_LO.BNDL"], "documents are not game files");
        assert_eq!(m.placed_by, "names");
        let pack = stage(&d, "pack", &[("Replace/VEH_2_HI.BNDL", "TWO_HI"), ("Replace/VEH_2_LO.BNDL", "TWO_LO"), ("Add/VEH_9_HI.BNDL", "NEW_HI"), ("Add/VEH_9_LO.BNDL", "NEW_LO"), ("ReadMe.txt", "x")]);
        let m = lib.add_from_dir(&pack, "Pack", "p.zip").unwrap();
        assert_eq!(m.files, vec!["VEHICLES/VEH_2_HI.BNDL", "VEHICLES/VEH_2_LO.BNDL", "VEHICLES/VEH_9_HI.BNDL", "VEHICLES/VEH_9_LO.BNDL"], "new cars of the same kind sit with the others");
        let named = stage(&d, "named", &[("Car One/VEH_3_HI.BNDL", "THREE_HI"), ("Car One/VEH_3_LO.BNDL", "THREE_LO")]);
        let m = lib.add_from_dir(&named, "Named", "n.rar").unwrap();
        assert_eq!(m.files, vec!["VEHICLES/VEH_3_HI.BNDL", "VEHICLES/VEH_3_LO.BNDL"]);

        lib.deploy(false).unwrap();
        assert_eq!(read(&game.join("VEHICLES/VEH_1_HI.BNDL")), "ONE_HI");
        assert_eq!(read(&game.join("VEHICLES/VEH_2_LO.BNDL")), "TWO_LO");
        assert_eq!(read(&game.join("VEHICLES/VEH_3_HI.BNDL")), "THREE_HI");
        assert_eq!(read(&game.join("VEHICLES/VEH_9_HI.BNDL")), "NEW_HI");
        assert!(!game.join("VEH_1_HI.BNDL").exists() && !game.join("Replace").exists() && !game.join("Car One").exists());
        assert_eq!(read(&game.join("Readme.txt")), "game readme", "a mod's readme never overwrites the game's");
        lib.purge().unwrap();
        assert_eq!(read(&game.join("VEHICLES/VEH_1_HI.BNDL")), "ORIGINAL");
        assert!(!game.join("VEHICLES/VEH_9_HI.BNDL").exists());
    }

    #[test]
    fn placement_by_name_leaves_programs_and_correct_layouts_alone() {
        let (d, game, lib) = racing_setup("smartkeep");
        // a proxy DLL at the root is a new file even though another folder has a DLL of that name
        let dll = stage(&d, "dll", &[("dinput8.dll", "PROXY")]);
        assert_eq!(lib.add_from_dir(&dll, "Proxy", "p.zip").unwrap().files, vec!["dinput8.dll"]);
        // already laid out from the game folder: untouched (its notes are not game files)
        let ok = stage(&d, "ok", &[("VEHICLES/VEH_1_HI.BNDL", "A"), ("VEHICLES/VEH_1_LO.BNDL", "B"), ("Readme.txt", "r")]);
        assert_eq!(lib.add_from_dir(&ok, "Right", "r.zip").unwrap().files, vec!["VEHICLES/VEH_1_HI.BNDL", "VEHICLES/VEH_1_LO.BNDL"], "already in place; only the notes go");
        // names the game does not know: the author's layout stands
        let unknown = stage(&d, "unk", &[("Stuff/new.bin", "N"), ("other.bin", "O")]);
        assert_eq!(lib.add_from_dir(&unknown, "Unknown", "u.zip").unwrap().files, vec!["Stuff/new.bin", "other.bin"]);
        assert!(game.join("VEHICLES").is_dir());
    }

    #[test]
    fn a_removed_mod_can_be_brought_back_and_notes_and_needs_are_kept() {
        let (d, game, lib) = setup("trashback");
        lib.add_from_dir(&stage(&d, "a", &[("Data/a.esp", "AAA")]), "Alpha", "a.zip").unwrap();
        lib.add_from_dir(&stage(&d, "b", &[("Data/b.esp", "BBB")]), "Beta", "b.zip").unwrap();
        lib.set_note("alpha", "  the good one  ").unwrap();
        lib.set_thunderstore("alpha", ThunderRef { namespace: "Own".into(), name: "Alpha_Mod".into(), version: "1.0.0".into() }).unwrap();
        lib.set_depends("beta", vec!["own-alpha_mod".into()]).unwrap();
        let st = lib.status_json();
        assert_eq!(st["mods"][0]["needed_by"], json!(["Beta"]), "Alpha is needed by Beta");
        assert_eq!(st["mods"][0]["note"], "the good one");
        assert_eq!(lib.needed_by("alpha"), vec!["Beta".to_string()]);

        // an update keeps the user's note
        let up = lib.replace_with("alpha", &stage(&d, "a2", &[("Data/a.esp", "NEW")]), "Alpha", "a2.zip", None, "2.0").unwrap();
        assert_eq!(up.note, "the good one");

        lib.deploy(false).unwrap();
        lib.remove("beta").unwrap();
        assert!(!game.join("Data/b.esp").exists());
        let t = lib.trash_list();
        assert_eq!(t.len(), 2, "the old Alpha (replaced by the update) and Beta");
        let beta = t.iter().find(|x| x.1 == "Beta").unwrap();
        let name = lib.restore_trash(&beta.0).unwrap();
        assert_eq!(name, "Beta");
        assert_eq!(lib.trash_list().len(), 1);
        assert!(lib.mods().iter().any(|m| m.name == "Beta" && m.enabled));
        lib.deploy(false).unwrap();
        assert_eq!(read(&game.join("Data/b.esp")), "BBB");
        assert!(lib.verify().orphans.is_empty());
        for bad in ["", "../x", "a/b", "nothing-here"] {
            assert!(lib.restore_trash(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn a_check_finds_missing_files_and_stray_folders_and_cleaning_only_touches_what_is_ours() {
        let (d, _game, lib) = setup("verify");
        let s = stage(&d, "a", &[("Data/a.esp", "AAAA"), ("Data/b.esp", "BB")]);
        lib.add_from_dir(&s, "Alpha", "a.zip").unwrap();
        let v = lib.verify();
        assert_eq!((v.mods, v.missing.len(), v.orphans.len(), v.mods_bytes), (1, 0, 0, 6));
        // a staged file disappears and a folder nobody owns shows up
        fs::remove_file(lib.mod_files_dir("alpha").join("Data/b.esp")).unwrap();
        fs::create_dir_all(lib.root().join("mods/stray/files")).unwrap();
        fs::write(lib.root().join("mods/stray/files/x"), "XXX").unwrap();
        let v = lib.verify();
        assert_eq!((v.missing.clone(), v.orphans.clone()), (vec!["Alpha: Data/b.esp".to_string()], vec!["stray".to_string()]));
        assert_eq!(lib.clean_orphans().unwrap(), 1);
        assert!(!lib.root().join("mods/stray").exists() && lib.verify().orphans.is_empty());
        assert!(lib.verify().trash_bytes >= 3, "it went to the trash, not into thin air");
        assert!(lib.empty_trash() >= 3);
        assert!(lib.mod_files_dir("alpha").join("Data/a.esp").is_file(), "the real mod is untouched");
    }

    #[test]
    fn a_backed_up_mod_list_comes_back_for_mods_whose_files_are_still_there() {
        let (d, _game, lib) = setup("meta");
        for (t, n) in [("a", "Alpha"), ("b", "Beta")] {
            lib.add_from_dir(&stage(&d, t, &[(&format!("Data/{t}.esp"), t)]), n, "x.zip").unwrap();
        }
        lib.save_profile("Both").unwrap();
        lib.pin_file("Data/a.esp", "alpha").unwrap();
        let saved = lib.export_meta();
        lib.set_enabled("alpha", false).unwrap();
        lib.remove("beta").unwrap(); // its files go to the trash
        assert_eq!(lib.mods().len(), 1);
        let (back, out) = lib.import_meta(&saved).unwrap();
        assert_eq!((back, out), (1, 1), "Alpha is back, Beta's files are gone");
        let m = lib.mods();
        assert_eq!((m[0].id.as_str(), m[0].enabled), ("alpha", true), "the saved on/off state returns");
        assert!(lib.status_json()["profiles"].as_array().unwrap().iter().any(|p| p == "Both"));
        assert!(lib.import_meta(&json!("nonsense")).is_err());
    }

    #[test]
    fn an_update_can_be_rolled_back_and_only_the_last_three_old_versions_are_kept() {
        let (d, game, lib) = setup("rollback");
        let nx = |file_id| NexusRef { game: "g".into(), mod_id: 5, file_id };
        let v1 = lib.add_with(&stage(&d, "v1", &[("Data/new.esp", "V1")]), "Cool", "c1.zip", Some(nx(1)), "1.0").unwrap();
        lib.set_note(&v1.id, "my note").unwrap();
        lib.set_update_rule(&v1.id, "auto").unwrap();
        let v2 = lib.replace_with(&v1.id, &stage(&d, "v2", &[("Data/new.esp", "V2")]), "Cool", "c2.zip", Some(nx(2)), "2.0").unwrap();
        lib.deploy(false).unwrap();
        assert_eq!(read(&game.join("Data/new.esp")), "V2");
        assert_eq!(lib.mods()[0].update_rule, "auto", "the rule follows the mod through an update");
        let prev = lib.previous_versions(&v2.id);
        assert_eq!(prev.iter().map(|p| p.1.as_str()).collect::<Vec<_>>(), vec!["1.0"]);

        let back = lib.rollback(&v2.id, &prev[0].0).unwrap();
        assert_eq!(back, "Cool v1.0");
        lib.deploy(false).unwrap();
        assert_eq!(read(&game.join("Data/new.esp")), "V1");
        let mods = lib.mods();
        assert_eq!((mods.len(), mods[0].version.as_str(), mods[0].note.as_str(), mods[0].update_rule.as_str(), mods[0].enabled), (1, "1.0", "my note", "auto", true));
        // and forward again: 2.0 is now the earlier version
        let fwd = lib.previous_versions(&mods[0].id);
        assert_eq!(fwd.iter().map(|p| p.1.as_str()).collect::<Vec<_>>(), vec!["2.0"]);
        lib.rollback(&mods[0].id, &fwd[0].0).unwrap();
        lib.deploy(false).unwrap();
        assert_eq!(read(&game.join("Data/new.esp")), "V2");
        // nonsense is refused and changes nothing
        let id = lib.mods()[0].id.clone();
        assert!(lib.rollback(&id, "../x").is_err() && lib.rollback(&id, "nope").is_err() && lib.rollback("ghost", "x").is_err());
        assert!(lib.set_update_rule(&id, "sometimes").is_err());
        assert_eq!(lib.mods().len(), 1);

        // after more updates only the last three old versions are kept
        let mut cur = id;
        for (i, ver) in ["3.0", "4.0", "5.0"].iter().enumerate() {
            let m = lib.replace_with(&cur, &stage(&d, &format!("v{ver}"), &[("Data/new.esp", ver)]), "Cool", "c.zip", Some(nx(10 + i as u64)), ver).unwrap();
            cur = m.id;
        }
        let kept: Vec<String> = lib.previous_versions(&cur).into_iter().map(|p| p.1).collect();
        assert_eq!(kept.len(), 3, "{kept:?}");
        assert!(kept.contains(&"4.0".to_string()) && kept.contains(&"3.0".to_string()), "the newest three: {kept:?}");
        // the list shows the newest one on the mod's row
        let st = lib.status_json();
        assert_eq!(st["previous"][&cur]["version"], "4.0", "{}", st["previous"]);
        assert_eq!(st["previous"].as_object().unwrap().len(), 1);
        // what was updated lately is listed with the version to go back to
        let recent = lib.recent_updates(0);
        assert_eq!((recent.len(), recent[0].version.as_str()), (1, "5.0"));
        assert!(!recent[0].old_version.is_empty() && !recent[0].dir.is_empty());
        assert!(lib.recent_updates(crate::fsutil::now_ms() + 60_000).is_empty(), "nothing changed after that moment");
        lib.set_enabled(&cur, false).unwrap();
        assert!(lib.recent_updates(0).is_empty(), "a mod that is switched off cannot be the cause");
        // a mod of another name or origin is never offered as an earlier version
        let other = lib.add_with(&stage(&d, "other", &[("Data/o.esp", "O")]), "Other", "o.zip", None, "9.0").unwrap();
        assert!(lib.previous_versions(&other.id).is_empty());
    }

    #[test]
    fn a_games_support_entry_decides_where_loose_files_go_when_nothing_else_does() {
        let (d, game, lib) = setup("rules");
        let lib = lib.with_rules(vec![(vec!["bndl".into(), "bin".into()], "VEHICLES".into()), (vec!["lua".into()], "Scripts/mods".into())]);
        // all files are of one kind: that kind's folder
        let a = lib.add_from_dir(&stage(&d, "cars", &[("CAR_1.BNDL", "A"), ("car_2.bin", "B")]), "Cars", "c.zip").unwrap();
        assert_eq!((a.target.as_str(), a.placed_by.as_str()), ("VEHICLES", "pack"));
        // mixed kinds that point to different folders: no guess
        let b = lib.add_from_dir(&stage(&d, "mixed", &[("x.bndl", "A"), ("y.lua", "B")]), "Mixed", "m.zip").unwrap();
        assert_eq!(b.target, "");
        // a kind the entry does not know: no guess
        let c = lib.add_from_dir(&stage(&d, "other", &[("x.xyz", "A")]), "Other", "o.zip").unwrap();
        assert_eq!(c.target, "");
        // the mod's own folders always win
        let e = lib.add_from_dir(&stage(&d, "own", &[("Data/z.bndl", "A")]), "Own", "z.zip").unwrap();
        assert_eq!(e.target, "");
        assert!(e.files.contains(&"Data/z.bndl".to_string()));
        // and it deploys where the entry said
        lib.deploy(false).unwrap();
        assert_eq!(read(&game.join("VEHICLES/CAR_1.BNDL")), "A");
        // the preview tells the user who decided
        let pv = lib.preview(&stage(&d, "p", &[("q.bndl", "A")])).unwrap();
        assert_eq!((pv.placed_by.as_str(), pv.target.as_str()), ("pack", "VEHICLES"));
    }

    #[test]
    fn the_user_can_pick_which_mod_wins_a_single_file() {
        let (d, game, lib) = setup("pins");
        let a = stage(&d, "a", &[("Data/shared.esp", "FROM_A"), ("Data/only_a.esp", "A")]);
        let b = stage(&d, "b", &[("Data/shared.esp", "FROM_B"), ("Data/only_b.esp", "B")]);
        lib.add_from_dir(&a, "Alpha", "a.zip").unwrap();
        lib.add_from_dir(&b, "Beta", "b.zip").unwrap();
        lib.deploy(false).unwrap();
        assert_eq!(read(&game.join("Data/shared.esp")), "FROM_B", "the later mod wins by default");
        let c = &lib.conflicts()[0];
        assert_eq!((c.winner_id.as_str(), c.pinned, c.ids.clone()), ("beta", false, vec!["alpha".to_string(), "beta".to_string()]));

        lib.pin_file("Data/shared.esp", "alpha").unwrap();
        assert!(lib.is_dirty());
        lib.deploy(false).unwrap();
        assert_eq!(read(&game.join("Data/shared.esp")), "FROM_A", "this one file now comes from Alpha");
        assert_eq!(read(&game.join("Data/only_b.esp")), "B", "everything else still follows the load order");
        assert!(lib.conflicts()[0].pinned);
        // a pin on a mod that is switched off falls back to the load order
        lib.set_enabled("alpha", false).unwrap();
        lib.deploy(false).unwrap();
        assert_eq!(read(&game.join("Data/shared.esp")), "FROM_B");
        lib.set_enabled("alpha", true).unwrap();
        lib.pin_file("data/shared.esp", "").unwrap();
        lib.deploy(false).unwrap();
        assert_eq!(read(&game.join("Data/shared.esp")), "FROM_B");
        assert!(lib.pin_file("Data/x", "nobody").is_err());
    }

    #[test]
    fn the_library_remembers_earlier_states_and_can_go_back_to_any_of_them() {
        let (d, game, lib) = setup("history");
        for (tag, name) in [("a", "Alpha"), ("b", "Beta")] {
            let s = stage(&d, tag, &[(&format!("Data/{tag}.esp"), tag)]);
            lib.add_from_dir(&s, name, "x.zip").unwrap();
        }
        lib.deploy(false).unwrap();
        lib.set_enabled("alpha", false).unwrap(); // history: [both on]
        lib.set_enabled("beta", false).unwrap(); // history: [both on, only beta on]
        lib.move_mod("alpha", 1).unwrap(); // order change, nothing on
        let h = lib.status_json()["history"].clone();
        assert_eq!(h.as_array().unwrap().len(), 3, "{h}");
        assert_eq!(h[0]["label"], "Moved Alpha", "newest first");
        // two steps back: both mods on again, in their first order
        let label = lib.restore_history(2).unwrap();
        assert!(label.starts_with("Disabled Alpha") || label.starts_with("Enabled") || !label.is_empty());
        let m = lib.mods();
        assert_eq!((m[0].id.as_str(), m[0].enabled, m[1].enabled), ("alpha", true, true));
        lib.deploy(false).unwrap();
        assert!(game.join("Data/a.esp").is_file() && game.join("Data/b.esp").is_file());
        // going back is itself a step that can be undone
        assert!(lib.undo().unwrap().starts_with("Went back"));
        assert!(lib.restore_history(99).is_err());
    }

    #[test]
    fn files_changed_behind_gantrys_back_are_noticed_and_a_redeploy_repairs_them() {
        let (d, game, lib) = setup("drift");
        let s = stage(&d, "a", &[("Data/base.esp", "MODDED"), ("Data/extra.esp", "EXTRA")]);
        lib.add_from_dir(&s, "Overhaul", "o.zip").unwrap();
        lib.deploy(false).unwrap();
        assert!(lib.drift().is_empty());
        // a game update writes a new base.esp (a fresh file, not our link) and removes extra.esp
        fs::remove_file(game.join("Data/base.esp")).unwrap();
        write(&game.join("Data/base.esp"), "UPDATED ORIGINAL");
        fs::remove_file(game.join("Data/extra.esp")).unwrap();
        let drift = lib.drift();
        assert_eq!(drift, vec![("Data/base.esp".to_string(), "changed"), ("Data/extra.esp".to_string(), "missing")]);
        assert_eq!(lib.status_json()["drift"]["count"], 2);
        // deploying again puts the mods back and keeps the game's new file as the thing to restore
        lib.deploy(false).unwrap();
        assert!(lib.drift().is_empty());
        assert_eq!(read(&game.join("Data/base.esp")), "MODDED");
        lib.purge().unwrap();
        assert_eq!(read(&game.join("Data/base.esp")), "UPDATED ORIGINAL", "the original that is restored is the updated one");
    }

    #[test]
    fn a_deploy_says_when_a_mod_looks_misplaced() {
        let (d, game, lib) = racing_setup("misplaced");
        let away = d.join("away");
        fs::rename(game.join("VEHICLES"), &away).unwrap();
        let s = stage(&d, "x", &[("VEH_1_HI.BNDL", "A"), ("VEH_1_LO.BNDL", "B")]);
        lib.add_from_dir(&s, "Late", "l.zip").unwrap();
        assert_eq!(lib.deploy(false).unwrap().misplaced, 0, "nothing in the game tells otherwise yet");
        fs::rename(&away, game.join("VEHICLES")).unwrap();
        assert_eq!(lib.deploy(false).unwrap().misplaced, 1);
        lib.relayout(None).unwrap();
        assert_eq!(lib.deploy(false).unwrap().misplaced, 0);
    }

    #[test]
    fn mods_added_before_the_game_files_were_known_can_be_re_placed() {
        let (d, game, lib) = racing_setup("relayout");
        // the car files are not in the game yet when the mod arrives, so it is kept as shipped...
        let hidden = d.join("away");
        fs::rename(game.join("VEHICLES"), &hidden).unwrap();
        let s = stage(&d, "x", &[("VEH_1_HI.BNDL", "ONE_HI"), ("VEH_1_LO.BNDL", "ONE_LO"), ("How To Install.txt", "t")]);
        let m = lib.add_from_dir(&s, "Late", "l.zip").unwrap();
        assert_eq!(m.files, vec!["VEH_1_HI.BNDL", "VEH_1_LO.BNDL"]);
        lib.deploy(false).unwrap();
        assert!(game.join("VEH_1_HI.BNDL").exists(), "misplaced, as it used to be");
        // ...and once the game has them, fixing the placement moves it and the next deploy cleans up
        fs::rename(&hidden, game.join("VEHICLES")).unwrap();
        assert_eq!(lib.relayout(None).unwrap(), 1);
        assert_eq!(lib.relayout(None).unwrap(), 0, "nothing left to fix");
        assert!(lib.is_dirty());
        assert!(!game.join("VEH_1_HI.BNDL").exists(), "the old, wrong place is cleaned up at once");
        lib.deploy(false).unwrap();
        assert_eq!(read(&game.join("VEHICLES/VEH_1_HI.BNDL")), "ONE_HI");
        assert!(!game.join("How To Install.txt").exists(), "stale readme");
        assert!(game.join("VEHICLES/VEH_1_LO.BNDL").is_file());
    }

    #[test]
    fn a_mods_install_notes_decide_the_folder_when_the_files_do_not_say_and_stay_readable() {
        let (d, game, lib) = setup("notes");
        write(&game.join("Scripts/old.lua"), "OLD");
        let s = stage(&d, "a", &[("cool.lua", "COOL"), ("Readme.txt", "Thanks!\nCopy cool.lua to the Scripts folder of the game.\n")]);
        let m = lib.add_from_dir(&s, "Cool Script", "c.zip").unwrap();
        assert_eq!((m.target.as_str(), m.files.clone(), m.notes.clone()), ("Scripts", vec!["cool.lua".to_string()], vec!["Readme.txt".to_string()]));
        assert_eq!(m.placed_by, "readme");
        lib.deploy(false).unwrap();
        assert_eq!(read(&game.join("Scripts/cool.lua")), "COOL");
        assert!(!game.join("Readme.txt").exists(), "notes are never put into the game");
        let notes = lib.notes(&m.id);
        assert_eq!(notes.len(), 1);
        assert!(notes[0].1.contains("Scripts folder"));
        assert_eq!(lib.status_json()["mods"][0]["notes"], 1);

        // instructions that point at a folder the game does not have are not followed
        let s = stage(&d, "b", &[("other.lua", "X"), ("HowTo.txt", "Put it into the Nowhere folder.")]);
        let m = lib.add_from_dir(&s, "Other", "o.zip").unwrap();
        assert_eq!(m.target, "");
        // a single-name folder is found one level down too
        write(&game.join("Game/Binaries/Win64/game.exe"), "MZ");
        let s = stage(&d, "c", &[("tweak.ini", "T"), ("Install.txt", "Drop tweak.ini in the Win64 directory.")]);
        let m = lib.add_from_dir(&s, "Tweak", "t.zip").unwrap();
        assert_eq!(m.target, "Game/Binaries/Win64");
    }

    #[test]
    fn deploy_links_files_backs_up_originals_and_purge_restores_everything() {
        let (d, game, lib) = setup("deploy");
        let s = stage(&d, "a", &[("Data/base.esp", "MODDED"), ("Data/extra.esp", "EXTRA"), ("Data/sub/deep.txt", "DEEP")]);
        lib.add_from_dir(&s, "Overhaul", "o.zip").unwrap();
        let rep = lib.deploy(false).unwrap();
        assert_eq!((rep.linked, rep.backed_up), (3, 1));
        assert_eq!(read(&game.join("Data/base.esp")), "MODDED");
        assert_eq!(read(&game.join("Data/sub/deep.txt")), "DEEP");
        // hard links: same inode as the staged file
        let staged = lib.mod_files_dir("overhaul").join("Data/extra.esp");
        assert!(fsutil::is_same_file(&staged, &game.join("Data/extra.esp")));
        assert!(!lib.is_dirty());

        let p = lib.purge().unwrap();
        assert_eq!((p.removed, p.restored), (3, 1));
        assert_eq!(read(&game.join("Data/base.esp")), "ORIGINAL");
        assert!(!game.join("Data/extra.esp").exists());
        assert!(!game.join("Data/sub").exists(), "folders Gantry created are removed again");
        assert!(staged.is_file(), "the library keeps its files");
        // and a deploy after a purge works again
        lib.deploy(false).unwrap();
        assert_eq!(read(&game.join("Data/extra.esp")), "EXTRA");
    }

    #[test]
    fn the_conflict_view_says_who_overwrites_whom_and_can_prefer_a_mod() {
        let (d, game, lib) = setup("cview");
        let a = lib.add_from_dir(&stage(&d, "a", &[("Data/one.esp", "A1"), ("Data/two.esp", "A2")]), "Alpha", "a.zip").unwrap();
        let b = lib.add_from_dir(&stage(&d, "b", &[("Data/one.esp", "B1"), ("Data/two.esp", "B2"), ("Data/three.esp", "B3")]), "Beta", "b.zip").unwrap();
        let c = lib.add_from_dir(&stage(&d, "c", &[("Data/three.esp", "C3")]), "Gamma", "c.zip").unwrap();
        lib.deploy(false).unwrap();
        let v = lib.conflict_view("", 100);
        assert_eq!((v["total"].as_u64(), v["matching"].as_u64(), v["pinned"].as_u64()), (Some(3), Some(3), Some(0)));
        let mod_of = |v: &Value, id: &str| v["mods"].as_array().unwrap().iter().find(|m| m["id"] == id).cloned().unwrap();
        assert_eq!((mod_of(&v, &a.id)["loses"].as_u64(), mod_of(&v, &a.id)["dead"].as_bool()), (Some(2), Some(true)), "Alpha loses everything it has");
        assert_eq!((mod_of(&v, &b.id)["wins"].as_u64(), mod_of(&v, &b.id)["loses"].as_u64(), mod_of(&v, &b.id)["dead"].as_bool()), (Some(2), Some(1), Some(false)));
        assert_eq!(mod_of(&v, &c.id)["dead"], false);
        assert_eq!((v["pairs"][0]["winner"].as_str(), v["pairs"][0]["loser"].as_str(), v["pairs"][0]["files"].as_u64()), (Some("Beta"), Some("Alpha"), Some(2)));

        // search by file name, by mod name, and the cap on how many files come back
        assert_eq!(lib.conflict_view("TWO", 100)["matching"], 1);
        assert_eq!(lib.conflict_view("gamma", 100)["matching"], 1);
        assert_eq!(lib.conflict_view("nothing like it", 100)["files"].as_array().unwrap().len(), 0);
        let capped = lib.conflict_view("", 1);
        assert_eq!((capped["files"].as_array().unwrap().len(), capped["matching"].as_u64()), (1, Some(3)));

        // Alpha wins against Beta everywhere they meet; Gamma still follows the load order
        assert_eq!(lib.prefer(&a.id, &b.id).unwrap(), 2);
        assert_eq!(lib.prefer(&a.id, &b.id).unwrap(), 0, "nothing changes the second time");
        lib.deploy(false).unwrap();
        assert_eq!((read(&game.join("Data/one.esp")).as_str(), read(&game.join("Data/two.esp")).as_str(), read(&game.join("Data/three.esp")).as_str()), ("A1", "A2", "C3"));
        let v = lib.conflict_view("", 100);
        assert_eq!(v["pinned"], 2);
        assert_eq!(mod_of(&v, &a.id)["dead"], false);
        assert!(lib.prefer("nobody", "").is_err());

        assert_eq!(lib.clear_pins().unwrap(), 2);
        lib.deploy(false).unwrap();
        assert_eq!(read(&game.join("Data/one.esp")), "B1", "the load order decides again");
        assert_eq!(lib.clear_pins().unwrap(), 0);
        // one mod over everyone it meets
        assert_eq!(lib.prefer(&a.id, "").unwrap(), 2);
    }

    /// Every file under `root` with its bytes, relative paths, for comparing a folder before and after.
    fn snapshot_of(root: &Path) -> BTreeMap<String, Vec<u8>> {
        WalkDir::new(root).into_iter().flatten().filter(|e| e.file_type().is_file()).map(|e| (e.path().strip_prefix(root).unwrap().to_string_lossy().replace('\\', "/"), fs::read(e.path()).unwrap())).collect()
    }

    #[test]
    fn random_sequences_of_changes_always_deploy_the_winners_and_purge_back_to_the_original() {
        // a small deterministic generator: the same run every time, a different one per seed
        struct Rng(u64);
        impl Rng {
            fn next(&mut self) -> u64 {
                self.0 ^= self.0 << 13;
                self.0 ^= self.0 >> 7;
                self.0 ^= self.0 << 17;
                self.0
            }
            fn below(&mut self, n: usize) -> usize {
                (self.next() % n as u64) as usize
            }
        }
        let paths = ["Data/a.esp", "Data/b.esp", "Data/sub/c.esp", "Data/base.esp", "Data/Textures/t.dds", "bin/x.dll", "Data/BASE.esp"];
        for seed in 1..=60u64 {
            let (d, game, lib) = setup(&format!("rand{seed}"));
            let original = snapshot_of(&game);
            let mut rng = Rng(seed * 7919 + 13);
            let mut ids: Vec<String> = Vec::new();
            for step in 0..40 {
                match rng.below(7) {
                    0 | 1 => {
                        // a new mod with a random few of the files
                        let n = 1 + rng.below(4);
                        let files: Vec<(String, String)> = (0..n).map(|_| (paths[rng.below(paths.len())].to_string(), format!("m{seed}-{step}"))).collect();
                        let refs: Vec<(&str, &str)> = files.iter().map(|(a, b)| (a.as_str(), b.as_str())).collect();
                        if let Ok(m) = lib.add_from_dir(&stage(&d, &format!("s{step}"), &refs), &format!("Mod {step}"), "x.zip") {
                            ids.push(m.id);
                        }
                    }
                    2 if !ids.is_empty() => {
                        let id = &ids[rng.below(ids.len())];
                        let on = lib.mods().iter().find(|m| &m.id == id).map(|m| m.enabled).unwrap_or(true);
                        let _ = lib.set_enabled(id, !on);
                    }
                    3 if !ids.is_empty() => {
                        let _ = lib.move_mod(&ids[rng.below(ids.len())], rng.below(5) as i32 - 2);
                    }
                    4 if !ids.is_empty() => {
                        let _ = lib.pin_file(paths[rng.below(paths.len())], &ids[rng.below(ids.len())]);
                    }
                    5 if ids.len() > 2 => {
                        let i = rng.below(ids.len());
                        let _ = lib.remove(&ids.remove(i));
                    }
                    _ => {
                        lib.deploy(false).unwrap();
                    }
                }
                if rng.below(3) == 0 {
                    lib.deploy(false).unwrap();
                    // what is in the game now: the original files, plus each file as its winner has it
                    let plan = lib.build_plan(&lib.load().mods, &lib.load().pins);
                    let now = snapshot_of(&game);
                    for w in &plan.files {
                        let m = &lib.load().mods[w.mod_idx];
                        let want = fs::read(lib.mod_files_dir(&m.id).join(&w.file)).unwrap();
                        let got = now.iter().find(|(k, _)| k.to_lowercase() == w.dest.to_lowercase()).map(|(_, v)| v.clone());
                        assert_eq!(got.as_deref(), Some(want.as_slice()), "seed {seed} step {step}: {} should come from {}", w.dest, m.name);
                    }
                    let known: Vec<String> = plan.files.iter().map(|w| w.dest.to_lowercase()).collect();
                    for k in now.keys().filter(|k| !original.contains_key(*k)) {
                        assert!(known.contains(&k.to_lowercase()), "seed {seed} step {step}: {k} is in the game but no mod provides it");
                    }
                }
            }
            lib.purge().unwrap();
            assert_eq!(snapshot_of(&game), original, "seed {seed}: the game folder is not as it was after purge");
        }
    }

    #[test]
    fn the_conflict_view_copes_with_thousands_of_shared_files() {
        let (d, _game, lib) = setup("cbig");
        let names: Vec<String> = (0..5000).map(|i| format!("Data/Meshes/set{}/part{}.nif", i % 50, i)).collect();
        let files: Vec<(&str, &str)> = names.iter().map(|n| (n.as_str(), "x")).collect();
        lib.add_from_dir(&stage(&d, "a", &files), "Alpha", "a.zip").unwrap();
        lib.add_from_dir(&stage(&d, "b", &files), "Beta", "b.zip").unwrap();
        let t = std::time::Instant::now();
        let v = lib.conflict_view("set7/", 300);
        assert_eq!((v["total"].as_u64(), v["matching"].as_u64(), v["files"].as_array().unwrap().len()), (Some(5000), Some(100), 100));
        assert_eq!(lib.conflict_view("", 300)["files"].as_array().unwrap().len(), 300);
        assert_eq!(lib.prefer("alpha", "beta").unwrap(), 5000);
        if cfg!(any(target_os = "linux", target_os = "macos")) {
            assert!(t.elapsed() < std::time::Duration::from_secs(5), "took {:?}", t.elapsed());
        }
    }

    #[test]
    fn later_mods_win_conflicts_and_reordering_changes_the_winner() {
        let (d, game, lib) = setup("order");
        let a = lib.add_from_dir(&stage(&d, "a", &[("Data/base.esp", "FROM A")]), "A", "a.zip").unwrap();
        let b = lib.add_from_dir(&stage(&d, "b", &[("Data/base.esp", "FROM B")]), "B", "b.zip").unwrap();
        let rep = lib.deploy(false).unwrap();
        assert_eq!(rep.conflicts, 1);
        assert_eq!(read(&game.join("Data/base.esp")), "FROM B");
        let c = lib.conflicts();
        assert_eq!((c[0].winner.as_str(), c[0].mods.clone()), ("B", vec!["A".to_string(), "B".to_string()]));

        lib.move_mod(&b.id, -1).unwrap();
        assert!(lib.is_dirty());
        lib.deploy(false).unwrap();
        assert_eq!(read(&game.join("Data/base.esp")), "FROM A");

        lib.set_enabled(&a.id, false).unwrap();
        lib.deploy(false).unwrap();
        assert_eq!(read(&game.join("Data/base.esp")), "FROM B");
        lib.set_enabled(&b.id, false).unwrap();
        lib.deploy(false).unwrap();
        assert_eq!(read(&game.join("Data/base.esp")), "ORIGINAL", "everything off = vanilla again");
        assert_eq!(fs::read_dir(lib.root().join("backups")).map(|r| r.count()).unwrap_or(0), 0);
    }

    #[test]
    fn paths_follow_the_games_case_and_mods_agree_with_each_other() {
        let (d, game, lib) = setup("case");
        // "data/BASE.ESP" is the same file as the game's Data/base.esp on a case-insensitive system
        lib.add_from_dir(&stage(&d, "a", &[("data/BASE.ESP", "LOWER")]), "Lower", "l.zip").unwrap();
        // two mods spelling a brand-new folder differently share one folder
        lib.add_from_dir(&stage(&d, "b", &[("Data/NewDir/one.txt", "1")]), "One", "1.zip").unwrap();
        lib.add_from_dir(&stage(&d, "c", &[("data/newdir/two.txt", "2")]), "Two", "2.zip").unwrap();
        lib.deploy(false).unwrap();
        assert_eq!(read(&game.join("Data/base.esp")), "LOWER");
        let spellings = fs::read_dir(&game).unwrap().flatten().filter(|e| e.file_name().to_string_lossy().eq_ignore_ascii_case("data")).count();
        assert_eq!(spellings, 1, "no second, differently-cased folder");
        assert_eq!(read(&game.join("Data/NewDir/one.txt")), "1");
        assert_eq!(read(&game.join("Data/NewDir/two.txt")), "2");
        lib.purge().unwrap();
        assert!(!game.join("Data/NewDir").exists());
        assert_eq!(read(&game.join("Data/base.esp")), "ORIGINAL");
    }

    #[test]
    fn purge_leaves_files_the_game_replaced_since_and_keeps_their_originals_safe() {
        let (d, game, lib) = setup("update");
        lib.add_from_dir(&stage(&d, "a", &[("Data/base.esp", "MODDED")]), "M", "m.zip").unwrap();
        lib.deploy(false).unwrap();
        // a game update replaces the file (new inode, new content)
        fs::remove_file(game.join("Data/base.esp")).unwrap();
        write(&game.join("Data/base.esp"), "UPDATED BY STEAM");
        let p = lib.purge().unwrap();
        assert_eq!(p.kept, vec!["Data/base.esp"]);
        assert_eq!(read(&game.join("Data/base.esp")), "UPDATED BY STEAM", "never clobber what we did not place");
    }

    #[test]
    fn a_crash_between_backup_and_link_is_recoverable() {
        let (d, game, lib) = setup("crash");
        lib.add_from_dir(&stage(&d, "a", &[("Data/base.esp", "MODDED")]), "M", "m.zip").unwrap();
        // simulate: original moved to backups, journal written, link never made
        let bak = lib.backup_path("Data/base.esp");
        move_file(&game.join("Data/base.esp"), &bak).unwrap();
        lib.save_journal(&Journal {
            entries: vec![Placed {
                rel: "Data/base.esp".into(), mod_id: "m".into(), file: "Data/base.esp".into(),
                kind: "pending".into(), backup: true, size: 0, mtime_ns: 0,
            }],
            created_dirs: vec![],
        })
        .unwrap();
        assert!(!game.join("Data/base.esp").exists());
        lib.purge().unwrap();
        assert_eq!(read(&game.join("Data/base.esp")), "ORIGINAL");
    }

    #[test]
    fn removing_a_mod_takes_its_files_out_of_the_game_but_keeps_them_in_the_trash() {
        let (d, game, lib) = setup("remove");
        let m = lib.add_from_dir(&stage(&d, "a", &[("Data/gone.esp", "G")]), "Gone", "g.zip").unwrap();
        lib.deploy(false).unwrap();
        assert!(game.join("Data/gone.esp").exists());
        lib.remove(&m.id).unwrap();
        assert!(!game.join("Data/gone.esp").exists());
        assert!(lib.mods().is_empty());
        let trash: Vec<_> = fs::read_dir(lib.root().join("trash")).unwrap().flatten().collect();
        assert_eq!(trash.len(), 1, "recoverable by hand");
        assert!(lib.remove("nope").is_err());
    }

    #[test]
    fn mod_ids_stay_unique_and_targets_cannot_escape() {
        let (d, game, lib) = setup("ids");
        let a = lib.add_from_dir(&stage(&d, "a", &[("Data/a.esp", "A")]), "Same Name", "1.zip").unwrap();
        let b = lib.add_from_dir(&stage(&d, "b", &[("Data/b.esp", "B")]), "Same Name", "2.zip").unwrap();
        assert_ne!(a.id, b.id);
        assert!(lib.set_target(&a.id, "../../etc").is_err());
        assert!(lib.set_target(&a.id, "/abs/path").is_ok(), "a leading slash is read as relative to the game");
        assert_eq!(lib.mods()[0].target, "abs/path");
        lib.set_target(&a.id, "").unwrap();
        lib.deploy(false).unwrap();
        assert!(game.join("Data/a.esp").exists());
        // a hand-edited library.json cannot make a mod write outside the game folder
        let mut lf = lib.load();
        lf.mods[0].files = vec!["../../escape.txt".into()];
        lib.save(&lf).unwrap();
        let rep = lib.deploy(false);
        assert!(!d.join("escape.txt").exists() && !game.parent().unwrap().join("escape.txt").exists(), "{rep:?}");
    }

    #[test]
    fn games_with_an_anti_cheat_are_refused_unless_forced() {
        let (d, game, lib) = setup("ac");
        write(&game.join("EasyAntiCheat/settings.json"), "{}");
        lib.add_from_dir(&stage(&d, "a", &[("Data/x.esp", "X")]), "X", "x.zip").unwrap();
        let e = lib.deploy(false).unwrap_err();
        assert!(e.contains("Easy Anti-Cheat"), "{e}");
        assert!(!game.join("Data/x.esp").exists());
        assert_eq!(lib.status_json()["anti_cheat"], "Easy Anti-Cheat");
        lib.deploy(true).unwrap();
        assert!(game.join("Data/x.esp").exists());
    }

    #[test]
    fn proxy_dlls_ask_for_wine_overrides_and_only_while_deployed() {
        let (d, _g, lib) = setup("proxy");
        let m = lib.add_from_dir(&stage(&d, "a", &[("winhttp.dll", "DLL"), ("BepInEx/core/x.dll", "X"), ("doorstop_config.ini", "I")]), "BepInEx", "b.zip").unwrap();
        lib.deploy(false).unwrap();
        assert_eq!(lib.wine_overrides(), vec!["winhttp"]);
        assert_eq!(lib.status_json()["overrides"], json!(["winhttp"]));
        lib.set_enabled(&m.id, false).unwrap();
        lib.deploy(false).unwrap();
        assert!(lib.wine_overrides().is_empty());
    }

    #[test]
    fn updating_a_mod_keeps_its_place_state_and_target_and_trashes_the_old_files() {
        let (d, game, lib) = setup("update-mod");
        let nx = |f| Some(NexusRef { game: "coolgame".into(), mod_id: 5, file_id: f });
        let a = lib.add_with(&stage(&d, "a", &[("Data/base.esp", "V1"), ("Data/old_only.esp", "OLD")]), "Cool", "cool-1.zip", nx(10), "1.0").unwrap();
        let b = lib.add_from_dir(&stage(&d, "b", &[("Data/base.esp", "B")]), "B", "b.zip").unwrap();
        lib.set_target(&a.id, "").unwrap();
        lib.set_enabled(&a.id, false).unwrap();
        lib.deploy(false).unwrap();
        assert_eq!(read(&game.join("Data/base.esp")), "B");

        let new = lib.replace_with(&a.id, &stage(&d, "a2", &[("Data/base.esp", "V2"), ("Data/new_only.esp", "NEW")]), "Cool", "cool-2.zip", nx(12), "2.0").unwrap();
        let ids: Vec<String> = lib.mods().into_iter().map(|m| m.id).collect();
        assert_eq!(ids, vec![new.id.clone(), b.id.clone()], "same slot, old one gone");
        let m = &lib.mods()[0];
        assert_eq!((m.version.as_str(), m.enabled, m.nexus.as_ref().map(|n| n.file_id)), ("2.0", false, Some(12)), "the user's disabled state survives an update");
        assert_eq!(fs::read_dir(lib.root().join("trash")).unwrap().count(), 1);

        lib.set_enabled(&new.id, true).unwrap();
        lib.move_mod(&b.id, -1).unwrap();
        lib.deploy(false).unwrap();
        assert_eq!(read(&game.join("Data/base.esp")), "V2");
        assert!(game.join("Data/new_only.esp").exists() && !game.join("Data/old_only.esp").exists());

        assert!(lib.replace_with("missing", &stage(&d, "a3", &[("Data/x.esp", "X")]), "X", "x.zip", None, "").is_err());
        assert_eq!(lib.mods().len(), 2, "a failed update adds nothing");
        // an update that brings no files leaves the old version alone
        let empty = d.join("stage-empty");
        fs::create_dir_all(&empty).unwrap();
        assert!(lib.replace_with(&new.id, &empty, "Cool", "c.zip", None, "").is_err());
        assert_eq!(lib.mods().len(), 2);
        assert!(game.join("Data/new_only.esp").exists());
    }

    #[test]
    fn a_corrupt_library_file_is_set_aside_not_overwritten() {
        let (d, _g, lib) = setup("corrupt");
        lib.add_from_dir(&stage(&d, "a", &[("Data/a.esp", "A")]), "A", "a.zip").unwrap();
        let _ = fs::remove_file(fsutil::kept_copy(&lib.lib_path()));
        fs::write(lib.lib_path(), b"{ not json").unwrap();
        assert!(lib.mods().is_empty());
        let kept: Vec<_> = fs::read_dir(lib.root()).unwrap().flatten().filter(|e| e.file_name().to_string_lossy().contains("corrupt")).collect();
        assert_eq!(kept.len(), 1);
    }

    #[test]
    fn a_damaged_library_or_journal_comes_back_from_the_copy_before_the_last_change() {
        let (d, g, lib) = setup("recover");
        lib.add_from_dir(&stage(&d, "a", &[("Data/a.esp", "A")]), "A", "a.zip").unwrap();
        lib.add_from_dir(&stage(&d, "b", &[("Data/b.esp", "B")]), "B", "b.zip").unwrap();
        lib.deploy(false).unwrap();
        lib.deploy(false).unwrap();
        let _ = fsutil::take_repaired();
        fs::write(lib.lib_path(), b"\0\0\0 half written").unwrap();
        fs::write(lib.journal_path(), b"{").unwrap();
        assert_eq!(lib.mods().len(), 2, "both mods are still known");
        let repaired = fsutil::take_repaired();
        assert!(repaired.iter().any(|(p, ok)| *ok && p.ends_with("library.json")), "{repaired:?}");
        // the journal still knows what was placed, so going back to vanilla removes it
        lib.purge().unwrap();
        assert!(!g.join("Data/a.esp").exists() && !g.join("Data/b.esp").exists());
        assert!(fs::read_dir(lib.root()).unwrap().flatten().any(|e| e.file_name().to_string_lossy().starts_with("library.json.corrupt-")), "the damaged file is kept");
    }

    #[test]
    fn status_reports_overwrites_dirty_state_and_conflict_flags() {
        let (d, _g, lib) = setup("status");
        lib.add_from_dir(&stage(&d, "a", &[("Data/base.esp", "A")]), "A", "a.zip").unwrap();
        lib.add_from_dir(&stage(&d, "b", &[("Data/base.esp", "B")]), "B", "b.zip").unwrap();
        let s = lib.status_json();
        assert_eq!((s["dirty"].as_bool(), s["overwrites"].as_u64(), s["deployed"].as_u64()), (Some(true), Some(1), Some(0)));
        assert_eq!(s["mods"][0]["conflicts"], true);
        lib.deploy(false).unwrap();
        let s = lib.status_json();
        assert_eq!((s["dirty"].as_bool(), s["overwrites"].as_u64(), s["deployed"].as_u64()), (Some(false), Some(0), Some(1)));
    }

    #[test]
    fn auto_resolve_conflicts_reorders_patches_after_base_and_auto_pins() {
        let (d, _g, lib) = setup("auto-resolve");
        lib.add_from_dir(&stage(&d, "patch", &[("Data/config.ini", "Patch Config")]), "Balance Compatibility Patch", "patch.zip").unwrap();
        lib.add_from_dir(&stage(&d, "base", &[("Data/config.ini", "Base Config"), ("Data/main.esm", "Base ESM")]), "Total Overhaul Base", "base.zip").unwrap();

        let report = lib.auto_resolve_conflicts("topological_patches_last").unwrap();
        assert!(report.resolved_conflicts > 0);

        let mods = lib.mods();
        assert_eq!(mods[0].name, "Total Overhaul Base");
        assert_eq!(mods[1].name, "Balance Compatibility Patch");
    }

    #[test]
    fn optimize_load_order_sequences_loaders_frameworks_and_patches_correctly() {
        let (d, _g, lib) = setup("opt-order");
        lib.add_from_dir(&stage(&d, "patch", &[("Data/ui.xml", "UI Patch")]), "My Custom UI Patch", "p.zip").unwrap();
        lib.add_from_dir(&stage(&d, "tex", &[("Data/tex.dds", "DDS")]), "4K High Res Textures", "t.zip").unwrap();
        lib.add_from_dir(&stage(&d, "loader", &[("dinput8.dll", "MZ")]), "REFramework Core", "l.zip").unwrap();
        lib.add_from_dir(&stage(&d, "frame", &[("scripts/lib.lua", "lua")]), "Address Library Framework", "f.zip").unwrap();

        let count = lib.optimize_load_order().unwrap();
        assert_eq!(count, 4);

        let mods = lib.mods();
        assert_eq!(mods[0].name, "REFramework Core"); // Tier 10
        assert_eq!(mods[1].name, "Address Library Framework"); // Tier 20
        assert_eq!(mods[2].name, "4K High Res Textures"); // Tier 70
        assert_eq!(mods[3].name, "My Custom UI Patch"); // Tier 90
    }
}
