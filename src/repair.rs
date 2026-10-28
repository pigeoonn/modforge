use crate::engine::{GameEntry, RigEngine};
use crate::fsutil::{self, atomic_write};
use regex::{NoExpand, Regex};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::fs::{self, File};
use crate::fsutil::symlink;
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, Mutex};
use walkdir::WalkDir;

pub const KNOWN_MOD_DLLS: &[&str] = &[
    "dwmapi.dll",
    "dxgi.dll",
    "dinput8.dll",
    "version.dll",
    "winmm.dll",
    "d3d11.dll",
    "d3d9.dll",
    "d3d12.dll",
    "xinput1_3.dll",
    "xinput1_4.dll",
    "winhttp.dll",
];

pub const UNSAFE_UE4SS_DEBUG_MODS: &[&str] = &[
    "kismetdebuggermod",
    "eventviewermod",
    "linetracemod",
    "actordumpermod",
    "splitscreenmod",
    "jsbluaprofilermod",
];

const SYSTEM_MODS: &[&str] = &[
    "bpml_genericfunctions",
    "bpmodloadermod",
    "cheatmanagerenablermod",
    "consolecommandsmod",
    "consoleenablermod",
    "keybinds",
];

const LOG_TAIL_BYTES: u64 = 512 * 1024;

static RE_MODS_TXT_LINE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?m)^\s*([^:\r\n;#]+?)\s*:\s*([01])").unwrap());

/// Serialises every read-modify-write of a `mods.txt` so concurrent toggles cannot lose updates.
static MODS_TXT_LOCK: Mutex<()> = Mutex::new(());

fn lock_mods_txt() -> std::sync::MutexGuard<'static, ()> {
    MODS_TXT_LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

/// A file or folder moved to the trash, with where it came from.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrashedItem {
    pub original: PathBuf,
    pub stored: PathBuf,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ModEntry {
    pub id: String,
    pub name: String,
    pub mod_type: String,
    pub path: PathBuf,
    pub enabled: bool,
    pub is_system: bool,
    pub has_config: bool,
    pub size_label: String,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct DoctorReport {
    pub has_ue: bool,
    pub layout: String,
    pub dwmapi_active: bool,
    pub settings_ok: bool,
    pub vtable_ok: bool,
    pub detected_version: String,
    pub crash_dumps: usize,
    pub issues: Vec<String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Ue4ssSettingsDto {
    pub gui_console: bool,
    pub regular_console: bool,
    pub disable_uobject_cache: bool,
    pub major_version: String,
    pub minor_version: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct LaunchOpts {
    #[serde(default)]
    pub launch_args: String,
    #[serde(default)]
    pub env_vars: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct RigConfigData {
    #[serde(default)]
    pub trainers: HashMap<String, String>,
    /// Where the user said mods go, per game: the look of a mod (`Preview::shape`) → "guess" (where Gantry guessed)
    /// or "dir:<folder>".
    #[serde(default)]
    pub place_rules: HashMap<String, HashMap<String, String>>,
    /// The build of each game's executable the mods last worked with (version, size and date), to notice a game update.
    #[serde(default)]
    pub game_builds: HashMap<String, String>,
    /// The mods Crash Detective found breaking each game (names), to point at them when they are on again.
    #[serde(default)]
    pub culprits: HashMap<String, Vec<String>>,
    /// How many runs of each game crashed in a row.
    #[serde(default)]
    pub crash_streak: HashMap<String, u32>,
    /// Every trainer kept for a game (paths), the attached one among them.
    #[serde(default)]
    pub trainer_shelf: HashMap<String, Vec<String>>,
    #[serde(default)]
    pub launch_options: HashMap<String, LaunchOpts>,
    #[serde(default)]
    pub profiles: HashMap<String, HashMap<String, Vec<String>>>,
    /// Steam launch options rig wrote last, per appid (tells rig's own options from the user's).
    #[serde(default)]
    pub steam_applied: HashMap<String, String>,
    /// Additional Steam library folders to scan (beyond `libraryfolders.vdf`), e.g. `["/mnt/games"]`.
    #[serde(default)]
    pub extra_libraries: Vec<String>,
    /// Interface language code (see `i18n::LANGS`); empty = English.
    #[serde(default)]
    pub language: String,
    /// SteamID64 of the Steam account whose library is shown; empty = the newest login.
    #[serde(default)]
    pub steam_account: String,
    /// When Gantry last launched each game (ms since the epoch), by appid.
    #[serde(default)]
    pub last_launched: HashMap<String, u64>,
    /// Unreal games whose new mods go to the Mod Library instead of the UE4SS/PAK folders.
    #[serde(default)]
    pub library_first: Vec<String>,
    /// Games the user removed from the list (appid → name); nothing on disk is touched.
    #[serde(default)]
    pub hidden_games: HashMap<String, String>,
    /// Games started without their mods (appids): the mods come back when the game closes, even
    /// if Gantry was closed in between.
    #[serde(default)]
    pub vanilla_sessions: Vec<String>,
    /// The first-run tour has been seen.
    #[serde(default)]
    pub tour_done: bool,
    /// Keep Gantry running after its window is closed (update notices keep coming).
    #[serde(default)]
    pub keep_background: bool,
    /// Install mods straight away even when Gantry had to guess where their files go.
    #[serde(default)]
    pub no_install_preview: bool,
    /// Size of the interface in percent (0 = the normal 100).
    #[serde(default)]
    pub ui_scale: u32,
    /// No animations at all.
    #[serde(default)]
    pub reduce_motion: bool,
    /// Stronger text and borders.
    #[serde(default)]
    pub high_contrast: bool,
    /// "dark" (default), "light" or "auto" (follow the system).
    #[serde(default)]
    pub theme: String,
    /// Settings of the "custom" theme as JSON: `{ hue, light, radius }`.
    #[serde(default)]
    pub custom_theme: String,
    /// The accent colour is taken from the selected game's cover art.
    #[serde(default)]
    pub accent_from_game: bool,
    /// Accent colour name (see `ui.html`); empty = green.
    #[serde(default)]
    pub accent: String,
    /// Gentler motion: shorter, no bouncing or lifting (`reduce_motion` turns it off entirely).
    #[serde(default)]
    pub calm_motion: bool,
    /// Modpacks that were imported but still wait for mods from Nexus Mods, by the library they belong to.
    #[serde(default)]
    pub pending_packs: std::collections::BTreeMap<String, crate::deploy::ProfileSpec>,
    /// What the game page shows, as JSON: `{ hero, stats, ring, badges, tab }`.
    #[serde(default)]
    pub layout: String,
    /// The version whose "What's new" the user has seen.
    #[serde(default)]
    pub seen_version: String,
    /// The newest crash report of Gantry itself that the person has been told about (milliseconds).
    #[serde(default)]
    pub crash_seen_ms: u64,
    /// Mods set aside in the store with the heart, by game: the cards as the store showed them.
    #[serde(default)]
    pub saved_mods: HashMap<String, Vec<serde_json::Value>>,
    /// A folder that something else keeps in step between computers (Syncthing, Dropbox, a USB stick): Gantry puts a copy
    /// of the portable part of its setup there and can bring one in.
    #[serde(default)]
    pub sync_folder: String,
    /// The size of the window when it was last closed or resized (0 = the default size).
    #[serde(default)]
    pub window_w: u32,
    #[serde(default)]
    pub window_h: u32,
    /// Speed cap per download in KB/s (0 = no cap).
    #[serde(default)]
    pub download_limit_kb: u64,
    /// Hold new downloads while a game is running.
    #[serde(default)]
    pub downloads_wait: bool,
    /// Let the background checks (health, updates, covers, backups) go on while a game runs; they wait by default.
    #[serde(default)]
    pub checks_while_playing: bool,
    /// Soft interface sounds (off by default).
    #[serde(default)]
    pub sounds: bool,
    /// A short rumble in a gamepad when something is pressed or goes wrong (off by default).
    #[serde(default)]
    pub haptics: bool,
    /// Ask VirusTotal about a mod's program files (their SHA-256 only). Off until the user switches it on.
    #[serde(default)]
    pub virustotal: bool,
    /// Switch off the weekly automatic backup of settings and mod lists.
    #[serde(default)]
    pub no_auto_backup: bool,
    /// Show mods marked as adult in the mod store (hidden by default).
    #[serde(default)]
    pub show_adult: bool,
    /// Tighter rows and cards.
    #[serde(default)]
    pub compact: bool,
    /// Bigger controls and gamepad navigation (also switched on by `rig --deck`).
    #[serde(default)]
    pub deck_mode: bool,
    /// Turned off on purpose on a Steam Deck, where controller mode is on by itself.
    #[serde(default)]
    pub deck_declined: bool,
    /// Games the user pinned to the top of the list (appids).
    #[serde(default)]
    pub favorites: Vec<String>,
    /// The user's own notes and tags per game.
    #[serde(default)]
    pub game_notes: HashMap<String, String>,
    #[serde(default)]
    pub game_tags: HashMap<String, Vec<String>>,
    /// Seconds played that Gantry saw, per game.
    #[serde(default)]
    pub playtime_s: HashMap<String, u64>,
    /// Steam app found by name for the artwork of games from other launchers ("0" = nothing found).
    #[serde(default)]
    pub cover_appid: HashMap<String, String>,
    /// Nexus Mods game domain chosen by hand per game (appid → domain).
    #[serde(default)]
    pub nexus_domain: HashMap<String, String>,
    /// Proton chosen per game that Gantry starts itself (appid → path of the `proton` script).
    #[serde(default)]
    pub proton_choice: HashMap<String, String>,
    /// Thunderstore community chosen by hand per game (appid → identifier), when names do not match.
    #[serde(default)]
    pub thunder_community: HashMap<String, String>,
    /// mod.io game chosen per game (appid → mod.io game id).
    #[serde(default)]
    pub modio_game: HashMap<String, u64>,
}

// ───────────────────────────── INI helpers ─────────────────────────────

fn ini_key_re(key: &str, commented: bool) -> Regex {
    let lead = if commented { r"[ \t]*;[ \t]*" } else { r"[ \t]*" };
    Regex::new(&format!(r"(?im)^{}{}[ \t]*=.*$", lead, regex::escape(key))).unwrap()
}

/// Value of the first *active* (uncommented) `key = value` line, trailing `; comment` stripped.
pub fn ini_get(txt: &str, key: &str) -> Option<String> {
    let re = Regex::new(&format!(
        r"(?im)^[ \t]*{}[ \t]*=[ \t]*([^\r\n;]*)",
        regex::escape(key)
    ))
    .unwrap();
    re.captures(txt).map(|c| c[1].trim().to_string())
}

fn ini_truthy(v: Option<String>) -> bool {
    matches!(v.map(|s| s.to_lowercase()).as_deref(), Some("1") | Some("true"))
}

fn ini_falsy(v: Option<String>) -> bool {
    matches!(v.map(|s| s.to_lowercase()).as_deref(), Some("0") | Some("false"))
}

/// Set `key = val`. Active lines are rewritten; otherwise the first commented
/// occurrence is revived; otherwise the key is inserted into `[section]`
/// (created when missing). Keys are therefore never silently dropped.
pub fn ini_set(txt: &str, section: &str, key: &str, val: &str) -> String {
    let line = format!("{} = {}", key, val);
    let active = ini_key_re(key, false);
    if active.is_match(txt) {
        return active.replace_all(txt, NoExpand(&line)).into_owned();
    }
    let commented = ini_key_re(key, true);
    if commented.is_match(txt) {
        return commented.replacen(txt, 1, NoExpand(&line)).into_owned();
    }
    let sec_re = Regex::new(&format!(r"(?im)^[ \t]*\[{}\][ \t]*$", regex::escape(section))).unwrap();
    if let Some(m) = sec_re.find(txt) {
        let mut out = String::with_capacity(txt.len() + line.len() + 2);
        out.push_str(&txt[..m.end()]);
        out.push('\n');
        out.push_str(&line);
        out.push_str(&txt[m.end()..]);
        out
    } else {
        let mut out = txt.to_string();
        if !out.is_empty() && !out.ends_with('\n') {
            out.push('\n');
        }
        out.push_str(&format!("\n[{}]\n{}\n", section, line));
        out
    }
}

fn valid_ue_version_part(s: &str, max_len: usize) -> bool {
    s.len() <= max_len && s.bytes().all(|b| b.is_ascii_digit())
}

/// Insert/replace `"dwmapi"="native,builtin"` inside the `[Software\\Wine\\DllOverrides]`
/// block of a Wine `user.reg`, preserving the header's trailing timestamp.
/// Returns `None` when the registry already has the right value.
pub fn patch_user_reg(reg: &str, now_secs: u64) -> Option<String> {
    const HEADER: &str = r"[Software\\Wine\\DllOverrides]";
    const WANT: &str = r#""dwmapi"="native,builtin""#;
    let had_nl = reg.ends_with('\n');
    let mut lines: Vec<String> = reg.lines().map(str::to_string).collect();

    let Some(h) = lines.iter().position(|l| l.starts_with(HEADER)) else {
        let mut out = reg.to_string();
        if !out.is_empty() && !out.ends_with('\n') {
            out.push('\n');
        }
        out.push_str(&format!("\n{} {}\n{}\n", HEADER, now_secs, WANT));
        return Some(out);
    };

    let block_end = lines[h + 1..]
        .iter()
        .position(|l| l.starts_with('['))
        .map(|i| h + 1 + i)
        .unwrap_or(lines.len());
    let existing = (h + 1..block_end).find(|&i| {
        let l = lines[i].to_lowercase();
        l.starts_with("\"dwmapi\"=") || l.starts_with("\"dwmapi\" =")
    });
    match existing {
        Some(i) if lines[i].trim() == WANT => return None,
        Some(i) => lines[i] = WANT.to_string(),
        None => {
            let mut at = h + 1;
            while at < block_end && lines[at].starts_with('#') {
                at += 1; // keep `#time=` style metadata directly under the header
            }
            lines.insert(at, WANT.to_string());
        }
    }
    let mut out = lines.join("\n");
    if had_nl {
        out.push('\n');
    }
    Some(out)
}

// ───────────────────────────── misc helpers ─────────────────────────────

fn pak_base_stem(path: &Path) -> Option<(PathBuf, String)> {
    let parent = path.parent()?.to_path_buf();
    let fname = path.file_name()?.to_string_lossy().to_string();
    let without_dis = fname
        .strip_suffix(".disabled")
        .or_else(|| fname.strip_suffix(".DISABLED"))
        .unwrap_or(&fname);
    let stem = without_dis
        .strip_suffix(".pak")
        .or_else(|| without_dis.strip_suffix(".PAK"))?;
    Some((parent, stem.to_string()))
}

fn compute_path_size_label(p: &Path) -> String {
    let bytes: u64 = if p.is_file() {
        fs::metadata(p).map(|m| m.len()).unwrap_or(0)
    } else if p.is_dir() {
        WalkDir::new(p)
            .max_depth(3)
            .into_iter()
            .flatten()
            .filter_map(|e| e.metadata().ok())
            .filter(|m| m.is_file())
            .map(|m| m.len())
            .sum()
    } else {
        0
    };

    if bytes >= 1_048_576 {
        format!("{:.1} MB", bytes as f64 / 1_048_576.0)
    } else if bytes >= 1024 {
        format!("{} KB", bytes / 1024)
    } else {
        format!("{} B", bytes)
    }
}

fn dir_name_lower(p: &Path) -> String {
    p.file_name()
        .map(|n| n.to_string_lossy().to_lowercase())
        .unwrap_or_default()
}

/// Restrict an identifier that becomes a path component.
fn safe_id(raw: &str) -> String {
    let s: String = raw
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '-')
        .collect();
    if s.is_empty() {
        "game".to_string()
    } else {
        s
    }
}

/// Directory walker used by every "find something under the game dir" scan: files are
/// never yielded, `Content/*` other than `Paks` and our own staging dirs are pruned.
fn game_dir_walker(gdir: &Path, max_depth: usize) -> impl Iterator<Item = walkdir::DirEntry> {
    WalkDir::new(gdir)
        .min_depth(1)
        .max_depth(max_depth)
        .sort_by_file_name()
        .into_iter()
        .filter_entry(|e| {
            if !e.file_type().is_dir() {
                return false;
            }
            let n = e.file_name().to_string_lossy().to_lowercase();
            if n.starts_with(fsutil::TMP_MARK) || n.starts_with(".rig_stage") {
                return false;
            }
            let parent = e.path().parent().map(dir_name_lower).unwrap_or_default();
            if parent == "content" && n != "paks" {
                return false;
            }
            // Mod containers are scanned on their own; never descend into them here.
            if parent == "~mods" || parent == "logicmods" {
                return false;
            }
            true
        })
        .flatten()
}

pub struct ModManager;

impl ModManager {
    pub fn get_ue4ss_root(ue_bin: &Path) -> PathBuf {
        let sub = ue_bin.join("ue4ss");
        if sub.is_dir() && sub.join("UE4SS.dll").is_file() {
            sub
        } else {
            ue_bin.to_path_buf()
        }
    }

    pub fn find_or_create_paks_dir(gdir: &Path) -> PathBuf {
        for entry in game_dir_walker(gdir, 4) {
            if !entry.file_name().to_string_lossy().eq_ignore_ascii_case("paks") {
                continue;
            }
            let rel = entry.path().strip_prefix(gdir).unwrap_or(entry.path());
            let in_engine = rel
                .components()
                .any(|c| c.as_os_str().to_string_lossy().eq_ignore_ascii_case("engine"));
            if !in_engine {
                return entry.path().to_path_buf();
            }
        }
        gdir.join("Content").join("Paks")
    }

    pub fn find_mod_config_file(mod_entry: &ModEntry) -> Option<PathBuf> {
        if mod_entry.mod_type != "LUA" {
            return None;
        }
        [
            "config.ini",
            "settings.ini",
            "Scripts/main.lua",
            "scripts/main.lua",
            "main.lua",
        ]
        .iter()
        .map(|rel| mod_entry.path.join(rel))
        .find(|c| c.is_file())
    }

    pub fn watched_mod_dirs(game: &GameEntry) -> Vec<PathBuf> {
        let mut dirs = Vec::new();
        let gdir = PathBuf::from(&game.game_dir);
        if !gdir.is_dir() {
            return dirs;
        }
        if let Some(ub) = RigEngine::find_ue_bin_dir(&gdir) {
            let mdir = Self::get_ue4ss_root(&ub).join("Mods");
            if mdir.is_dir() {
                dirs.push(mdir);
            }
            dirs.push(ub);
        }
        let paks = Self::find_or_create_paks_dir(&gdir);
        if paks.is_dir() {
            dirs.push(paks);
        }
        dirs
    }

    fn lua_state_map(mods_dir: &Path) -> HashMap<String, String> {
        let mods_txt = fs::read_to_string(mods_dir.join("mods.txt"))
            .unwrap_or_default()
            .replace("\r\n", "\n");
        let mut state_map = HashMap::new();
        for cap in RE_MODS_TXT_LINE.captures_iter(&mods_txt) {
            state_map.insert(cap[1].trim().to_lowercase(), cap[2].to_string());
        }
        state_map
    }

    pub fn list_mods(game: &GameEntry) -> Vec<ModEntry> {
        let mut mods = Vec::new();
        let gdir = PathBuf::from(&game.game_dir);
        if !gdir.is_dir() {
            return mods;
        }
        let ue_bin = RigEngine::find_ue_bin_dir(&gdir);

        if let Some(ref ub) = ue_bin {
            let mods_dir = Self::get_ue4ss_root(ub).join("Mods");
            if mods_dir.is_dir() {
                let state_map = Self::lua_state_map(&mods_dir);
                let mut entries: Vec<_> = fs::read_dir(&mods_dir)
                    .ok()
                    .into_iter()
                    .flatten()
                    .flatten()
                    .collect();
                entries.sort_by_key(|e| e.file_name());

                for item in entries {
                    let path = item.path();
                    let name = item.file_name().to_string_lossy().to_string();
                    if !path.is_dir()
                        || name.starts_with('.')
                        || matches!(name.as_str(), "shared" | "UEHelpers" | "__pycache__")
                    {
                        continue;
                    }
                    let low = name.to_lowercase();
                    let enabled = match state_map.get(&low).map(|s| s.as_str()) {
                        Some("1") => true,
                        Some("0") => false,
                        _ => {
                            path.join("enabled.txt").is_file()
                                && !path.join("enabled.txt.disabled").is_file()
                        }
                    };
                    let is_system = UNSAFE_UE4SS_DEBUG_MODS.contains(&low.as_str())
                        || SYSTEM_MODS.contains(&low.as_str());
                    let has_config = path.join("config.ini").is_file()
                        || path.join("settings.ini").is_file()
                        || path.join("Scripts/main.lua").is_file()
                        || path.join("scripts/main.lua").is_file()
                        || path.join("main.lua").is_file();
                    let size_label = compute_path_size_label(&path);

                    mods.push(ModEntry {
                        id: format!("LUA:{}", name),
                        name,
                        mod_type: "LUA".into(),
                        path,
                        enabled,
                        is_system,
                        has_config,
                        size_label,
                    });
                }
            }
        }

        for entry in game_dir_walker(&gdir, 5) {
            let dname = entry.file_name().to_string_lossy().to_lowercase();
            if dname != "~mods" && dname != "logicmods" {
                continue;
            }
            let prefix = if dname == "logicmods" { "[Logic] " } else { "" };
            let mut pak_files: Vec<PathBuf> = WalkDir::new(entry.path())
                .min_depth(1)
                .max_depth(3)
                .into_iter()
                .flatten()
                .filter(|f| f.file_type().is_file())
                .map(|f| f.path().to_path_buf())
                .collect();
            pak_files.sort();

            for fpath in pak_files {
                let fname = fpath.file_name().unwrap_or_default().to_string_lossy().to_string();
                let low = fname.to_lowercase();
                let (clean, enabled) = if low.ends_with(".pak") {
                    (fname[..fname.len() - 4].to_string(), true)
                } else if low.ends_with(".pak.disabled") {
                    (fname[..fname.len() - 13].to_string(), false)
                } else {
                    continue;
                };
                let disp = format!("{}{}", prefix, clean);
                let size_label = compute_path_size_label(&fpath);
                mods.push(ModEntry {
                    id: format!("PAK:{}", disp),
                    name: disp,
                    mod_type: "PAK".into(),
                    path: fpath,
                    enabled,
                    is_system: false,
                    has_config: false,
                    size_label,
                });
            }
        }

        let mut seen_dlls = HashSet::new();
        for d in [ue_bin, Some(gdir)].into_iter().flatten() {
            if let Ok(rd) = fs::read_dir(d) {
                let mut files: Vec<_> = rd.flatten().collect();
                files.sort_by_key(|f| f.file_name());
                for f in files {
                    let fname = f.file_name().to_string_lossy().to_lowercase();
                    let clean = fname.trim_end_matches(".disabled").to_string();
                    if KNOWN_MOD_DLLS.contains(&clean.as_str()) && seen_dlls.insert(clean.clone()) {
                        let size_label = compute_path_size_label(&f.path());
                        mods.push(ModEntry {
                            id: format!("DLL:{}", clean),
                            name: clean.clone(),
                            mod_type: "DLL".into(),
                            path: f.path(),
                            enabled: !fname.ends_with(".disabled"),
                            is_system: clean == "dwmapi.dll",
                            has_config: false,
                            size_label,
                        });
                    }
                }
            }
        }
        mods
    }

    /// Update (or append) `name : 0|1` inside a mods.txt, atomically.
    fn set_mods_txt_state(mods_txt: &Path, name: &str, enable: bool) -> bool {
        let _guard = lock_mods_txt();
        let mut content = fs::read_to_string(mods_txt)
            .unwrap_or_default()
            .replace("\r\n", "\n");
        let val = if enable { "1" } else { "0" };
        let pat = format!(r"(?im)^([ \t]*{}[ \t]*:[ \t]*)[01]", regex::escape(name));
        let Ok(re) = Regex::new(&pat) else {
            return false;
        };
        if re.is_match(&content) {
            content = re.replace_all(&content, format!("${{1}}{}", val)).into_owned();
        } else {
            if !content.is_empty() && !content.ends_with('\n') {
                content.push('\n');
            }
            content.push_str(&format!("{} : {}\n", name, val));
        }
        atomic_write(mods_txt, content).is_ok()
    }

    pub fn toggle_mod(game: &GameEntry, mod_entry: &ModEntry, enable: bool) -> bool {
        match mod_entry.mod_type.as_str() {
            "LUA" => {
                let Some(ue_bin) = RigEngine::find_ue_bin_dir(Path::new(&game.game_dir)) else {
                    return false;
                };
                let mods_dir = Self::get_ue4ss_root(&ue_bin).join("Mods");
                let on = mod_entry.path.join("enabled.txt");
                let off = mod_entry.path.join("enabled.txt.disabled");
                let markers_ok = if enable {
                    File::create(&on).is_ok() && {
                        let _ = fs::remove_file(&off);
                        true
                    }
                } else {
                    let _ = fs::remove_file(&on);
                    File::create(&off).is_ok()
                };
                markers_ok && Self::set_mods_txt_state(&mods_dir.join("mods.txt"), &mod_entry.name, enable)
            }
            "PAK" => {
                let Some((parent, stem)) = pak_base_stem(&mod_entry.path) else {
                    return false;
                };
                let stem_low = stem.to_lowercase();
                let mut pak_ok = false;
                let Ok(rd) = fs::read_dir(&parent) else {
                    return false;
                };
                for entry in rd.flatten() {
                    let fname = entry.file_name().to_string_lossy().to_string();
                    let low = fname.to_lowercase();
                    for ext in ["pak", "utoc", "ucas", "sig"] {
                        let target_act = format!("{}.{}", stem_low, ext);
                        let target_dis = format!("{}.{}.disabled", stem_low, ext);
                        if enable && low == target_dis {
                            let new_name = &fname[..fname.len() - 9];
                            if fs::rename(entry.path(), parent.join(new_name)).is_ok() && ext == "pak" {
                                pak_ok = true;
                            }
                        } else if !enable && low == target_act {
                            let new_name = format!("{}.disabled", fname);
                            if fs::rename(entry.path(), parent.join(new_name)).is_ok() && ext == "pak" {
                                pak_ok = true;
                            }
                        }
                    }
                }
                pak_ok
            }
            _ => {
                let p_str = mod_entry.path.to_string_lossy().to_string();
                if enable && p_str.ends_with(".disabled") {
                    let new_p = PathBuf::from(p_str.trim_end_matches(".disabled"));
                    fs::rename(&mod_entry.path, &new_p).is_ok()
                } else if !enable && !p_str.ends_with(".disabled") {
                    let new_p = PathBuf::from(format!("{}.disabled", p_str));
                    fs::rename(&mod_entry.path, &new_p).is_ok()
                } else {
                    false
                }
            }
        }
    }

    pub fn bulk_toggle_user_mods(game: &GameEntry, enable: bool) -> usize {
        Self::list_mods(game)
            .into_iter()
            .filter(|m| !m.is_system && m.enabled != enable)
            .filter(|m| Self::toggle_mod(game, m, enable))
            .count()
    }

    pub fn apply_profile(game: &GameEntry, enabled_ids: &[String]) -> usize {
        let set: HashSet<&String> = enabled_ids.iter().collect();
        Self::list_mods(game)
            .into_iter()
            .filter(|m| !m.is_system)
            .filter(|m| m.enabled != set.contains(&m.id))
            .filter(|m| Self::toggle_mod(game, m, set.contains(&m.id)))
            .count()
    }

    /// Remove a mod without destroying it: its files move into `trash_root` so the removal can be
    /// undone with `restore_trashed`. Refuses system mods and anything outside the game folder.
    pub fn trash_mod(game: &GameEntry, mod_entry: &ModEntry, trash_root: &Path) -> Result<Vec<TrashedItem>, String> {
        if mod_entry.is_system {
            return Err(format!("'{}' is a system mod and cannot be removed.", mod_entry.name));
        }
        if !mod_entry.path.starts_with(&game.game_dir) {
            return Err("That mod is outside the game folder.".into());
        }
        let files: Vec<PathBuf> = match mod_entry.mod_type.as_str() {
            "PAK" => {
                let (parent, stem) = pak_base_stem(&mod_entry.path).ok_or("Unrecognised PAK file name.")?;
                let stem_low = stem.to_lowercase();
                let mut v: Vec<PathBuf> = fs::read_dir(&parent)
                    .map_err(|e| e.to_string())?
                    .flatten()
                    .filter(|e| {
                        let low = e.file_name().to_string_lossy().to_lowercase();
                        ["pak", "utoc", "ucas", "sig"]
                            .iter()
                            .any(|x| low == format!("{}.{}", stem_low, x) || low == format!("{}.{}.disabled", stem_low, x))
                    })
                    .map(|e| e.path())
                    .collect();
                v.sort();
                v
            }
            _ => vec![mod_entry.path.clone()],
        };
        if files.is_empty() {
            return Err("Nothing to remove: the files are already gone.".into());
        }
        let bundle = trash_root.join(fsutil::unique_token());
        let mut moved: Vec<TrashedItem> = Vec::new();
        for f in files {
            let stored = bundle.join(f.file_name().unwrap_or_default());
            let res = if f.is_dir() { fsutil::move_dir(&f, &stored) } else { fsutil::move_file(&f, &stored) };
            match res {
                Ok(()) => moved.push(TrashedItem { original: f, stored }),
                Err(e) => {
                    let _ = Self::restore_trashed(&moved);
                    return Err(format!("Could not remove '{}': {}", mod_entry.name, e));
                }
            }
        }
        if mod_entry.mod_type == "LUA" {
            if let Some(ue_bin) = RigEngine::find_ue_bin_dir(Path::new(&game.game_dir)) {
                let mods_txt = Self::get_ue4ss_root(&ue_bin).join("Mods").join("mods.txt");
                let _guard = lock_mods_txt();
                if let Ok(txt) = fs::read_to_string(&mods_txt) {
                    let clean_txt = txt.replace("\r\n", "\n");
                    let pat = format!(r"(?im)^[ \t]*{}[ \t]*:.*\n?", regex::escape(&mod_entry.name));
                    if let Ok(re) = Regex::new(&pat) {
                        let _ = atomic_write(&mods_txt, re.replace_all(&clean_txt, "").as_bytes());
                    }
                }
            }
        }
        Ok(moved)
    }

    /// Put trashed files back where they came from. Never overwrites: a file that has appeared
    /// at the original place in the meantime is reported and left in the trash.
    pub fn restore_trashed(items: &[TrashedItem]) -> (usize, Vec<String>) {
        let (mut restored, mut conflicts) = (0, Vec::new());
        for it in items {
            if fs::symlink_metadata(&it.original).is_ok() {
                conflicts.push(it.original.display().to_string());
                continue;
            }
            let res = if it.stored.is_dir() { fsutil::move_dir(&it.stored, &it.original) } else { fsutil::move_file(&it.stored, &it.original) };
            match res {
                Ok(()) => {
                    restored += 1;
                    if let Some(bundle) = it.stored.parent() {
                        let _ = fs::remove_dir(bundle); // only succeeds when empty
                    }
                }
                Err(e) => conflicts.push(format!("{} ({})", it.original.display(), e)),
            }
        }
        (restored, conflicts)
    }

    /// Delete trash bundles older than `max_age`.
    pub fn purge_trash(trash_root: &Path, max_age: std::time::Duration) -> usize {
        let now = std::time::SystemTime::now();
        fs::read_dir(trash_root)
            .into_iter()
            .flatten()
            .flatten()
            .filter(|e| {
                e.metadata()
                    .and_then(|m| m.modified())
                    .ok()
                    .and_then(|t| now.duration_since(t).ok())
                    .map(|age| age >= max_age)
                    .unwrap_or(false)
            })
            .filter(|e| fs::remove_dir_all(e.path()).is_ok())
            .count()
    }

    pub fn get_active_dll_overrides(game: &GameEntry) -> String {
        let mods = Self::list_mods(game);
        let mut active_dlls = HashSet::new();
        let mut dwmapi_explicitly_disabled = false;

        for m in mods {
            if m.mod_type == "DLL" {
                let stem = m.name.trim_end_matches(".dll").to_string();
                if m.enabled {
                    active_dlls.insert(stem);
                } else if stem == "dwmapi" {
                    dwmapi_explicitly_disabled = true;
                }
            }
        }

        if active_dlls.is_empty() {
            if dwmapi_explicitly_disabled {
                "dwmapi=b".to_string()
            } else {
                "dwmapi=n,b".to_string()
            }
        } else {
            let mut list: Vec<_> = active_dlls.into_iter().collect();
            list.sort();
            format!("{}=n,b", list.join(","))
        }
    }

    pub fn read_ue4ss_log_tail(game: &GameEntry, max_lines: usize) -> String {
        let gdir = PathBuf::from(&game.game_dir);
        let Some(ue_bin) = RigEngine::find_ue_bin_dir(&gdir) else {
            return "Unreal Engine Win64 folder not found.".to_string();
        };
        let ue_root = Self::get_ue4ss_root(&ue_bin);
        let log_p = if ue_root.join("UE4SS.log").is_file() {
            ue_root.join("UE4SS.log")
        } else {
            ue_bin.join("UE4SS.log")
        };
        let Ok(txt) = fsutil::read_tail(&log_p, LOG_TAIL_BYTES) else {
            return "UE4SS.log not found yet (launch the game once to generate it).".to_string();
        };
        let lines: Vec<&str> = txt.lines().collect();
        let start = lines.len().saturating_sub(max_lines);
        lines[start..].join("\n")
    }

    fn settings_ini_path(game: &GameEntry) -> Option<(PathBuf, PathBuf)> {
        let ue_bin = RigEngine::find_ue_bin_dir(Path::new(&game.game_dir))?;
        let ini = Self::get_ue4ss_root(&ue_bin).join("UE4SS-settings.ini");
        Some((ue_bin, ini))
    }

    pub fn get_ue4ss_settings(game: &GameEntry) -> Ue4ssSettingsDto {
        let Some((ue_bin, ini_p)) = Self::settings_ini_path(game) else {
            return Ue4ssSettingsDto::default();
        };
        let detected = RepairEngine::engine_version(game, &ue_bin);
        let (maj, min) = detected.unwrap_or_default();
        let txt = fs::read_to_string(&ini_p)
            .unwrap_or_default()
            .replace("\r\n", "\n");

        Ue4ssSettingsDto {
            gui_console: ini_truthy(ini_get(&txt, "GuiConsoleEnabled")),
            regular_console: ini_truthy(ini_get(&txt, "ConsoleEnabled")),
            disable_uobject_cache: ini_falsy(ini_get(&txt, "bUseUObjectArrayCache")),
            major_version: ini_get(&txt, "MajorVersion").filter(|v| !v.is_empty()).unwrap_or(maj),
            minor_version: ini_get(&txt, "MinorVersion").filter(|v| !v.is_empty()).unwrap_or(min),
        }
    }

    pub fn save_ue4ss_settings(game: &GameEntry, dto: &Ue4ssSettingsDto) -> Result<(), String> {
        let (maj, min) = (dto.major_version.trim(), dto.minor_version.trim());
        if !valid_ue_version_part(maj, 1) || !valid_ue_version_part(min, 2) || maj.is_empty() != min.is_empty() {
            return Err(format!("Invalid engine version '{}.{}'.", maj, min));
        }
        let Some((_, ini_p)) = Self::settings_ini_path(game) else {
            return Err("Unreal Engine Win64 folder not found.".into());
        };
        let raw_txt = fs::read_to_string(&ini_p)
            .map_err(|_| "UE4SS-settings.ini not found (run Setup UE4SS first).".to_string())?;
        let mut t = raw_txt.replace("\r\n", "\n");

        let gui_val = if dto.gui_console { "1" } else { "0" };
        let con_val = if dto.regular_console { "1" } else { "0" };
        let cache_val = if dto.disable_uobject_cache { "false" } else { "true" };

        t = ini_set(&t, "Debug", "GuiConsoleEnabled", gui_val);
        t = ini_set(&t, "Debug", "GuiConsoleVisible", gui_val);
        t = ini_set(&t, "Debug", "ConsoleEnabled", con_val);
        t = ini_set(&t, "Debug", "GraphicsAPI", "dx11");
        t = ini_set(&t, "General", "bUseUObjectArrayCache", cache_val);
        t = ini_set(&t, "EngineVersionOverride", "MajorVersion", maj);
        t = ini_set(&t, "EngineVersionOverride", "MinorVersion", min);

        atomic_write(&ini_p, t).map_err(|e| format!("Could not write UE4SS-settings.ini: {}", e))
    }
}

pub struct RepairEngine;

impl RepairEngine {
    pub fn diagnose(game: &GameEntry) -> DoctorReport {
        let mut rep = DoctorReport::default();
        let gdir = PathBuf::from(&game.game_dir);
        let Some(ue_bin) = RigEngine::find_ue_bin_dir(&gdir) else {
            rep.issues.push("Unreal Engine Win64 directory not found.".into());
            return rep;
        };
        rep.has_ue = true;
        let version = Self::engine_version(game, &ue_bin);
        rep.detected_version = version.as_ref().map(|(a, b)| format!("{}.{}", a, b)).unwrap_or_default();
        let ue_root = ModManager::get_ue4ss_root(&ue_bin);
        rep.layout = if ue_root.ends_with("ue4ss") {
            "ue4ss/ (Nested)".into()
        } else {
            "Win64/ (Root)".into()
        };
        rep.dwmapi_active = ue_bin.join("dwmapi.dll").is_file();
        if !rep.dwmapi_active {
            rep.issues.push("dwmapi.dll proxy is missing or disabled in Win64.".into());
        }
        rep.vtable_ok = ue_root.join("VTableLayout.ini").is_file()
            && ue_root.join("MemberVariableLayout.ini").is_file();
        if !rep.vtable_ok {
            match &version {
                Some((maj, min)) => rep.issues.push(format!("Missing VTableLayout.ini templates for UE {}.{}.", maj, min)),
                // nothing to deploy for an unknown version: UE4SS falls back to its built-in layouts
                None => rep.vtable_ok = true,
            }
        }
        if let Ok(txt) = fs::read_to_string(ue_root.join("UE4SS-settings.ini")) {
            let txt = txt.replace("\r\n", "\n");
            let cache_ok = ini_falsy(ini_get(&txt, "bUseUObjectArrayCache"));
            let forced = (ini_get(&txt, "MajorVersion").unwrap_or_default(), ini_get(&txt, "MinorVersion").unwrap_or_default());
            let forced_set = !forced.0.is_empty() || !forced.1.is_empty();
            let version_ok = match &version {
                Some((maj, min)) => forced.0 == *maj && forced.1 == *min,
                None => !forced_set, // unknown: only automatic detection is safe
            };
            rep.settings_ok = cache_ok && version_ok;
            if !cache_ok {
                rep.issues.push("UE4SS-settings.ini needs the memory cache patch (bUseUObjectArrayCache = false).".into());
            }
            if !version_ok {
                rep.issues.push(match &version {
                    Some((maj, min)) if forced_set => format!(
                        "UE4SS is set for Unreal Engine {}.{} but this game is {}.{}. A wrong version corrupts memory and crashes the game at startup: press Self-Heal.",
                        forced.0, forced.1, maj, min
                    ),
                    Some((maj, min)) => format!("UE4SS-settings.ini does not state the engine version ({}.{}).", maj, min),
                    None => format!(
                        "UE4SS-settings.ini forces version {}.{} but Gantry cannot confirm it for this game. Press Self-Heal to switch back to automatic detection.",
                        forced.0, forced.1
                    ),
                });
            }
        } else {
            rep.issues.push("UE4SS-settings.ini missing.".into());
        }

        let mods = ModManager::list_mods(game);
        let hstm_lua_on = mods
            .iter()
            .any(|m| m.mod_type == "LUA" && m.name.to_lowercase().contains("trainer") && m.enabled);
        let hstm_pak_off = mods
            .iter()
            .any(|m| m.mod_type == "PAK" && m.name.to_lowercase().contains("hstm") && !m.enabled);
        if hstm_lua_on && hstm_pak_off {
            rep.issues.push(
                "Mod Dependency Warning: HalfSwordTrainerMod (LUA) is enabled, but its companion [Logic] HSTM_UI.pak is disabled!"
                    .into(),
            );
        }

        rep.crash_dumps = Self::crash_dumps(&ue_bin).len();
        rep
    }

    fn crash_dumps(ue_bin: &Path) -> Vec<PathBuf> {
        WalkDir::new(ue_bin)
            .max_depth(2)
            .into_iter()
            .flatten()
            .filter(|e| e.path().extension().map(|x| x == "dmp").unwrap_or(false))
            .map(|e| e.path().to_path_buf())
            .collect()
    }

    pub fn clear_shader_cache(game: &GameEntry) -> usize {
        let mut roots = vec![PathBuf::from(&game.compatdata), PathBuf::from(&game.game_dir)];
        if !game.launches_directly() {
            // Steam keeps DXVK/VKD3D/Fossilize caches next to compatdata: <lib>/shadercache/<appid>
            if let Some(lib) = Path::new(&game.compatdata).parent().and_then(|p| p.parent()) {
                roots.push(lib.join("shadercache").join(safe_id(&game.appid)));
            }
        }

        let mut removed = 0;
        for root in roots {
            if !root.is_dir() {
                continue;
            }
            for entry in WalkDir::new(root).max_depth(6).into_iter().flatten() {
                let p = entry.path();
                if !p.is_file() {
                    continue;
                }
                let fname = p.file_name().unwrap_or_default().to_string_lossy().to_lowercase();
                if (fname.ends_with(".dxvk-cache")
                    || fname.contains("vkd3d-proton.cache")
                    || fname.ends_with(".ushaderprecache"))
                    && fs::remove_file(p).is_ok()
                {
                    removed += 1;
                }
            }
        }
        removed
    }

    /// All `SaveGames` directories below `root` (outermost only).
    pub(crate) fn find_save_dirs(root: &Path) -> Vec<PathBuf> {
        WalkDir::new(root)
            .min_depth(1)
            .max_depth(4)
            .sort_by_file_name()
            .into_iter()
            .filter_entry(|e| {
                // do not descend into a SaveGames dir once found
                e.file_type().is_dir()
                    && !e
                        .path()
                        .parent()
                        .map(|p| dir_name_lower(p) == "savegames")
                        .unwrap_or(false)
            })
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().eq_ignore_ascii_case("SaveGames"))
            .map(|e| e.path().to_path_buf())
            .collect()
    }

    /// The game's Unreal Engine version: what the game itself last printed in its log beats what
    /// can be read from the executable; `None` when neither knows (never a guess).
    pub fn engine_version(game: &GameEntry, ue_bin: &Path) -> Option<(String, String)> {
        crate::crash::logged_engine_version(game).or_else(|| RigEngine::detect_ue_version_opt(ue_bin))
    }

    /// The UE4SS-curated folder in `CustomGameConfigs/` for `game_name`, if there is one: an
    /// exact name match wins, otherwise the longest folder name the game's name contains.
    pub fn custom_game_config(ue_root: &Path, game_name: &str) -> Option<PathBuf> {
        let norm = |s: &str| s.chars().filter(|c| c.is_alphanumeric()).collect::<String>().to_lowercase();
        let want = norm(game_name);
        let mut best: Option<(usize, PathBuf)> = None;
        for e in fs::read_dir(ue_root.join("CustomGameConfigs")).ok()?.flatten() {
            if !e.path().is_dir() {
                continue;
            }
            let n = norm(&e.file_name().to_string_lossy());
            let score = if n == want {
                usize::MAX
            } else if n.len() >= 5 && want.contains(&n) {
                n.len()
            } else {
                continue;
            };
            if best.as_ref().map(|(s, _)| score > *s).unwrap_or(true) {
                best = Some((score, e.path()));
            }
        }
        best.map(|(_, p)| p)
    }

    pub fn repair(game: &GameEntry) -> Vec<String> {
        let mut actions = Vec::new();
        let gdir = PathBuf::from(&game.game_dir);
        let Some(ue_bin) = RigEngine::find_ue_bin_dir(&gdir) else {
            return vec!["Win64 directory not found.".into()];
        };
        let version = Self::engine_version(game, &ue_bin);
        let ue_root = ModManager::get_ue4ss_root(&ue_bin);
        match &version {
            Some((a, b)) => actions.push(format!("Engine version: Unreal Engine {}.{}.", a, b)),
            None => actions.push("Engine version could not be determined; UE4SS will detect it itself.".into()),
        }

        // 1. Unify duplicate case-sensitive folder on Linux ext4
        let wrong_case = gdir.join("HalfSwordUE5");
        let right_case = gdir.join("HalfswordUE5");
        let distinct = match (fs::canonicalize(&wrong_case), fs::canonicalize(&right_case)) {
            (Ok(a), Ok(b)) => a != b,
            _ => false,
        };
        if wrong_case.is_dir() && right_case.is_dir() && distinct {
            match fsutil::copy_dir_all(&wrong_case, &right_case) {
                Ok(_) => {
                    let _ = fs::remove_dir_all(&wrong_case);
                    actions.push(
                        "Merged duplicate case-sensitive folder HalfSwordUE5 -> HalfswordUE5.".into(),
                    );
                }
                Err(e) => actions.push(format!("Could not merge HalfSwordUE5 duplicate: {}", e)),
            }
        }

        // 2. Ensure Content/Paks/~mods and Content/Paks/LogicMods exist
        let paks_dir = ModManager::find_or_create_paks_dir(&gdir);
        let _ = fs::create_dir_all(paks_dir.join("~mods"));
        let _ = fs::create_dir_all(paks_dir.join("LogicMods"));

        // 3. Purge stale crash dumps
        let purged = Self::crash_dumps(&ue_bin)
            .into_iter()
            .filter(|p| fs::remove_file(p).is_ok())
            .count();
        if purged > 0 {
            actions.push(format!("Purged {} stale crash dump(s).", purged));
        }

        // 4. Restore dwmapi.dll & UE4SS.dll symlink
        if !ue_bin.join("dwmapi.dll").is_file()
            && !ue_bin.join("dwmapi.dll.disabled").is_file()
            && ue_root.join("dwmapi.dll").is_file()
            && fsutil::atomic_copy(&ue_root.join("dwmapi.dll"), &ue_bin.join("dwmapi.dll")).is_ok()
        {
            actions.push("Restored dwmapi.dll to Win64 root.".into());
        }
        if ue_root.ends_with("ue4ss")
            && ue_root.join("UE4SS.dll").is_file()
            && fs::symlink_metadata(ue_bin.join("UE4SS.dll")).is_err()
            && symlink("ue4ss/UE4SS.dll", ue_bin.join("UE4SS.dll")).is_ok()
        {
            actions.push("Created Win64/UE4SS.dll symlink.".into());
        }

        // 5. Deploy VTableLayout & MemberVariableLayout templates for the detected engine
        let ver_tag = version
            .as_ref()
            .map(|(maj, min)| format!("{}_{:02}", maj, min.parse::<u32>().unwrap_or(0)))
            .unwrap_or_default();
        let vt_tpl = ue_root
            .join("VTableLayoutTemplates")
            .join(format!("VTableLayout_{}_Template.ini", ver_tag));
        if vt_tpl.is_file() && fsutil::atomic_copy(&vt_tpl, &ue_root.join("VTableLayout.ini")).is_ok() {
            actions.push(format!(
                "Deployed VTableLayout_{}_Template.ini -> VTableLayout.ini",
                ver_tag
            ));
        }
        let mv_tpl = ue_root
            .join("MemberVarLayoutTemplates")
            .join(format!("MemberVariableLayout_{}_Template.ini", ver_tag));
        if mv_tpl.is_file()
            && fsutil::atomic_copy(&mv_tpl, &ue_root.join("MemberVariableLayout.ini")).is_ok()
        {
            actions.push(format!(
                "Deployed MemberVariableLayout_{}_Template.ini -> MemberVariableLayout.ini",
                ver_tag
            ));
        }

        // 5b. A configuration curated for this specific game beats the generic template
        if let Some(dir) = Self::custom_game_config(&ue_root, &game.name) {
            let mut copied = Vec::new();
            for f in ["VTableLayout.ini", "MemberVariableLayout.ini"] {
                if dir.join(f).is_file() && fsutil::atomic_copy(&dir.join(f), &ue_root.join(f)).is_ok() {
                    copied.push(f);
                }
            }
            if !copied.is_empty() {
                actions.push(format!(
                    "Applied UE4SS's curated config for '{}' ({}).",
                    dir.file_name().unwrap_or_default().to_string_lossy(),
                    copied.join(", ")
                ));
            }
        }

        // 6. Patch UE4SS-settings.ini
        let ini_p = ue_root.join("UE4SS-settings.ini");
        if let Ok(raw_txt) = fs::read_to_string(&ini_p) {
            let mut t = raw_txt.replace("\r\n", "\n");
            t = ini_set(&t, "General", "bUseUObjectArrayCache", "false");
            // known version: pin it; unknown: clear any earlier (possibly wrong) pin so UE4SS detects it
            let (vmaj, vmin) = version.clone().unwrap_or_default();
            t = ini_set(&t, "EngineVersionOverride", "MajorVersion", &vmaj);
            t = ini_set(&t, "EngineVersionOverride", "MinorVersion", &vmin);
            t = ini_set(&t, "General", "SecondsToScanBeforeGivingUp", "60");
            t = ini_set(&t, "Debug", "GraphicsAPI", "dx11");
            match atomic_write(&ini_p, t) {
                Ok(()) => actions.push(match &version {
                    Some((maj, min)) => format!("Configured UE4SS-settings.ini for UE {}.{}, GraphicsAPI=dx11 & bUseUObjectArrayCache=false.", maj, min),
                    None => "Configured UE4SS-settings.ini (engine version left to UE4SS), GraphicsAPI=dx11 & bUseUObjectArrayCache=false.".into(),
                }),
                Err(e) => actions.push(format!("Could not write UE4SS-settings.ini: {}", e)),
            }
        }

        // 7. Reconcile mods.txt with strict load-order priority
        let has_active_logic_paks = WalkDir::new(paks_dir.join("LogicMods"))
            .min_depth(1)
            .max_depth(3)
            .into_iter()
            .flatten()
            .any(|f| {
                f.file_type().is_file() && f.file_name().to_string_lossy().to_lowercase().ends_with(".pak")
            });

        let mods_dir = ue_root.join("Mods");
        if mods_dir.is_dir() {
            let _guard = lock_mods_txt();
            let mods_txt_p = mods_dir.join("mods.txt");
            let state_map = ModManager::lua_state_map(&mods_dir);

            if let Ok(rd) = fs::read_dir(&mods_dir) {
                let mut entries: Vec<_> = rd.flatten().collect();
                entries.sort_by_key(|e| {
                    let low = e.file_name().to_string_lossy().to_lowercase();
                    let priority = match low.as_str() {
                        "bpml_genericfunctions" => 0,
                        "bpmodloadermod" => 1,
                        "keybinds" => 2,
                        _ => 10,
                    };
                    (priority, low)
                });

                let mut out_lines = Vec::new();
                for f in entries {
                    let path = f.path();
                    let name = f.file_name().to_string_lossy().to_string();
                    if !path.is_dir()
                        || name.starts_with('.')
                        || matches!(name.as_str(), "shared" | "UEHelpers" | "__pycache__")
                    {
                        continue;
                    }
                    if path.join("Scripts").is_dir() && !path.join("scripts").exists() {
                        let _ = symlink("Scripts", path.join("scripts"));
                    } else if path.join("scripts").is_dir() && !path.join("Scripts").exists() {
                        let _ = symlink("scripts", path.join("Scripts"));
                    }

                    let low = name.to_lowercase();
                    if low == "halfwolverine" {
                        let cfg_p = path.join("config.ini");
                        if let Ok(ct) = fs::read_to_string(&cfg_p) {
                            if (ct.contains("ClawKey = T") || ct.contains("ClawKey=T"))
                                && atomic_write(
                                    &cfg_p,
                                    ct.replace("ClawKey = T", "ClawKey = G")
                                        .replace("ClawKey=T", "ClawKey=G"),
                                )
                                .is_ok()
                            {
                                actions.push(
                                    "Set HalfWolverine ClawKey = G (prevents Trainer key conflict)."
                                        .into(),
                                );
                            }
                        }
                    }

                    let val = if UNSAFE_UE4SS_DEBUG_MODS.contains(&low.as_str()) {
                        "0".to_string()
                    } else if low == "keybinds"
                        || (has_active_logic_paks
                            && (low == "bpmodloadermod" || low == "bpml_genericfunctions"))
                    {
                        "1".to_string()
                    } else if let Some(v) = state_map.get(&low) {
                        v.clone()
                    } else if path.join("enabled.txt.disabled").is_file() {
                        "0".to_string()
                    } else {
                        "1".to_string()
                    };

                    if val == "1" {
                        let _ = File::create(path.join("enabled.txt"));
                        let _ = fs::remove_file(path.join("enabled.txt.disabled"));
                    } else {
                        let _ = fs::remove_file(path.join("enabled.txt"));
                        let _ = File::create(path.join("enabled.txt.disabled"));
                    }
                    out_lines.push(format!("{} : {}", name, val));
                }
                match atomic_write(&mods_txt_p, out_lines.join("\n") + "\n") {
                    Ok(()) => actions.push(
                        "Ordered mods.txt (Keybinds & BPModLoader first) & disabled crashing debug hooks."
                            .into(),
                    ),
                    Err(e) => actions.push(format!("Could not write mods.txt: {}", e)),
                }
            }
        }

        // 8. Sync Wine prefix user.reg DllOverrides (wineserver rewrites it on exit, so never while running)
        let user_reg = PathBuf::from(&game.pfx).join("user.reg");
        if crate::procs::is_game_running(game) {
            actions.push("Skipped Wine registry sync: the game is running (close it and run Self-Heal again).".into());
        } else if user_reg.is_file() {
            if let Ok(reg_txt) = fs::read_to_string(&user_reg) {
                if let Some(patched) = patch_user_reg(&reg_txt, fsutil::now_ms() / 1000) {
                    if atomic_write(&user_reg, patched).is_ok() {
                        actions.push(
                            "Synced \"dwmapi\"=\"native,builtin\" into Wine prefix user.reg.".into(),
                        );
                    }
                }
            }
        }
        actions
    }
}

#[cfg(test)]
pub mod fixtures {
    use super::*;
    use crate::fsutil::testutil::TempDir;

    pub const SHIPPING: &str = "Demo/Binaries/Win64/Demo-Win64-Shipping.exe";

    /// A fake Unreal game on disk with a nested UE4SS install, two Lua mods and PAKs.
    pub fn make_game(tag: &str) -> (TempDir, GameEntry) {
        let d = TempDir::new(tag);
        // The install folder name deliberately contains "Engine" (regression for path filter).
        let gdir = d.join("steamapps/common/Unreal Engine Demo");
        let win64 = gdir.join("Demo/Binaries/Win64");
        fs::create_dir_all(&win64).unwrap();
        let mut exe = vec![0u8; 4096];
        exe.extend_from_slice(b"..++UE5+Release-5.3..");
        exe.extend_from_slice(&[1u8; 4096]);
        fs::write(win64.join("Demo-Win64-Shipping.exe"), exe).unwrap();

        let root = win64.join("ue4ss");
        fs::create_dir_all(root.join("Mods/Keybinds")).unwrap();
        fs::create_dir_all(root.join("Mods/CoolMod/Scripts")).unwrap();
        fs::create_dir_all(root.join("Mods/ActorDumperMod")).unwrap();
        fs::write(root.join("UE4SS.dll"), b"dll").unwrap();
        fs::write(root.join("dwmapi.dll"), b"proxy").unwrap();
        fs::write(root.join("Mods/CoolMod/Scripts/main.lua"), "print('hi')").unwrap();
        fs::write(root.join("Mods/Keybinds/enabled.txt"), "").unwrap();
        fs::write(root.join("Mods/mods.txt"), "CoolMod : 1\r\nKeybinds : 1\r\n").unwrap();
        fs::write(
            root.join("UE4SS-settings.ini"),
            "[General]\r\nbUseUObjectArrayCache = true\r\n\r\n[Debug]\r\n; GraphicsAPI = opengl\r\nConsoleEnabled = 0\r\n",
        )
        .unwrap();

        let paks = gdir.join("Demo/Content/Paks");
        fs::create_dir_all(paks.join("~mods")).unwrap();
        fs::create_dir_all(paks.join("LogicMods")).unwrap();
        fs::write(paks.join("~mods/Skin_P.pak"), vec![9u8; 2048]).unwrap();
        fs::write(paks.join("~mods/Skin_P.utoc"), b"u").unwrap();
        fs::write(paks.join("LogicMods/UI.pak.disabled"), b"u").unwrap();

        let compat = d.join("steamapps/compatdata/42");
        let pfx = compat.join("pfx");
        fs::create_dir_all(pfx.join("drive_c/users/steamuser/AppData/Local/Demo/Saved/SaveGames")).unwrap();

        let game = GameEntry {
            appid: "42".into(),
            name: "Demo".into(),
            game_dir: gdir.to_string_lossy().into(),
            exe_path: String::new(),
            shipping_exe: win64.join("Demo-Win64-Shipping.exe").to_string_lossy().into(),
            bin_dir: win64.to_string_lossy().into(),
            engine: "Unreal Engine 5.3".into(),
            compatdata: compat.to_string_lossy().into(),
            pfx: pfx.to_string_lossy().into(),
            has_pfx: true,
            is_custom: false,
            is_installed: true,
            steam_appid: String::new(),
            launch_args: String::new(),
            env_vars: String::new(),
            trainer_name: String::new(),
            mod_count: 0, source: String::new(), proton_path: String::new(), ue4ss: false, loader: String::new(), cover_id: String::new(),
        };
        (d, game)
    }
}

#[cfg(test)]
mod tests {
    use super::fixtures::*;
    use super::*;

    fn by_id<'a>(mods: &'a [ModEntry], id: &str) -> &'a ModEntry {
        mods.iter().find(|m| m.id == id).unwrap_or_else(|| panic!("missing {id}: {:?}", mods.iter().map(|m| &m.id).collect::<Vec<_>>()))
    }

    #[test]
    fn ini_set_covers_active_commented_and_missing() {
        let t = "[General]\nbUseUObjectArrayCache = true\n; MinorVersion = 1\n[Debug]\n";
        let t = ini_set(t, "General", "bUseUObjectArrayCache", "false");
        assert_eq!(ini_get(&t, "bUseUObjectArrayCache").as_deref(), Some("false"));
        let t = ini_set(&t, "EngineVersionOverride", "MinorVersion", "4");
        assert_eq!(ini_get(&t, "MinorVersion").as_deref(), Some("4"));
        assert!(!t.contains("; MinorVersion"), "commented line revived in place");
        let t = ini_set(&t, "Debug", "GraphicsAPI", "dx11");
        assert!(t.contains("[Debug]\nGraphicsAPI = dx11"));
        let t = ini_set(&t, "NewSection", "X", "1");
        assert!(t.contains("[NewSection]\nX = 1"));
        assert_eq!(ini_get("A = 5 ; note", "A").as_deref(), Some("5"));
    }

    #[test]
    fn user_reg_patch_keeps_header_timestamp() {
        let reg = "WINE REGISTRY Version 2\n\n[Software\\\\Wine\\\\DllOverrides] 1700000000\n#time=1da\n\"foo\"=\"builtin\"\n\n[Other] 1\n";
        let out = patch_user_reg(reg, 5).unwrap();
        assert!(out.contains("[Software\\\\Wine\\\\DllOverrides] 1700000000\n#time=1da\n\"dwmapi\"=\"native,builtin\"\n\"foo\""));
        assert!(patch_user_reg(&out, 5).is_none(), "idempotent");
        let wrong = out.replace("native,builtin", "builtin");
        let fixed = patch_user_reg(&wrong, 5).unwrap();
        assert_eq!(fixed.matches("dwmapi").count(), 1);
        assert!(fixed.contains("\"native,builtin\""));
        let none = patch_user_reg("WINE REGISTRY Version 2\n", 7).unwrap();
        assert!(none.contains("[Software\\\\Wine\\\\DllOverrides] 7\n\"dwmapi\"=\"native,builtin\"\n"));
    }

    #[test]
    fn lists_all_mod_types_and_survives_engine_in_install_path() {
        let (_d, game) = make_game("list");
        let mods = ModManager::list_mods(&game);
        assert!(by_id(&mods, "LUA:CoolMod").enabled);
        assert!(by_id(&mods, "LUA:CoolMod").has_config);
        assert!(by_id(&mods, "LUA:Keybinds").is_system);
        assert!(by_id(&mods, "LUA:ActorDumperMod").is_system);
        assert!(by_id(&mods, "PAK:Skin_P").enabled);
        assert!(!by_id(&mods, "PAK:[Logic] UI").enabled);
    }

    #[test]
    fn toggle_roundtrips_for_every_mod_type() {
        let (_d, game) = make_game("toggle");
        let get = |id: &str| by_id(&ModManager::list_mods(&game), id).clone();

        for id in ["LUA:CoolMod", "PAK:Skin_P", "PAK:[Logic] UI"] {
            let m = get(id);
            let target = !m.enabled;
            assert!(ModManager::toggle_mod(&game, &m, target), "{id}");
            assert_eq!(get(id).enabled, target, "{id} flipped");
            assert!(ModManager::toggle_mod(&game, &get(id), !target), "{id} back");
            assert_eq!(get(id).enabled, !target);
        }
        // companion files follow the .pak
        let paks = PathBuf::from(&game.game_dir).join("Demo/Content/Paks/~mods");
        assert!(ModManager::toggle_mod(&game, &get("PAK:Skin_P"), false));
        assert!(paks.join("Skin_P.utoc.disabled").is_file());
        assert!(!paks.join("Skin_P.utoc").exists());
        // mods.txt keeps CRLF-free, deduplicated state
        let txt = fs::read_to_string(
            PathBuf::from(&game.bin_dir).join("ue4ss/Mods/mods.txt"),
        )
        .unwrap();
        assert_eq!(txt.matches("CoolMod").count(), 1);
    }

    #[test]
    fn bulk_and_profiles_skip_system_mods() {
        let (_d, game) = make_game("bulk");
        let n = ModManager::bulk_toggle_user_mods(&game, false);
        assert_eq!(n, 2, "CoolMod + Skin_P (UI.pak already off)");
        let mods = ModManager::list_mods(&game);
        assert!(by_id(&mods, "LUA:Keybinds").enabled, "system mod untouched");
        let changed = ModManager::apply_profile(&game, &["PAK:Skin_P".into()]);
        assert_eq!(changed, 1);
        assert!(by_id(&ModManager::list_mods(&game), "PAK:Skin_P").enabled);
    }

    #[test]
    fn removal_goes_to_the_trash_and_can_be_undone() {
        let (d, game) = make_game("del");
        let trash = d.join("trash");
        let mods = ModManager::list_mods(&game);
        assert!(ModManager::trash_mod(&game, by_id(&mods, "LUA:Keybinds"), &trash).is_err(), "system mods are protected");
        let mut evil = by_id(&mods, "LUA:CoolMod").clone();
        evil.path = PathBuf::from("/tmp");
        assert!(ModManager::trash_mod(&game, &evil, &trash).is_err());
        assert!(!trash.exists(), "refusals touch nothing");

        let lua = ModManager::trash_mod(&game, by_id(&mods, "LUA:CoolMod"), &trash).unwrap();
        let pak = ModManager::trash_mod(&game, by_id(&mods, "PAK:Skin_P"), &trash).unwrap();
        assert_eq!((lua.len(), pak.len()), (1, 2), "the .utoc companion travels with the .pak");
        let after = ModManager::list_mods(&game);
        assert!(after.iter().all(|m| m.id != "LUA:CoolMod" && m.id != "PAK:Skin_P"));
        let txt = fs::read_to_string(PathBuf::from(&game.bin_dir).join("ue4ss/Mods/mods.txt")).unwrap();
        assert!(!txt.contains("CoolMod"));
        assert!(lua[0].stored.join("Scripts/main.lua").is_file(), "content is kept, not destroyed");

        assert_eq!(ModManager::restore_trashed(&lua), (1, vec![]));
        assert_eq!(ModManager::restore_trashed(&pak), (2, vec![]));
        let back = ModManager::list_mods(&game);
        assert!(by_id(&back, "LUA:CoolMod").has_config && by_id(&back, "PAK:Skin_P").enabled);
        assert!(!lua[0].stored.exists(), "trash bundle cleaned up after restoring");

        // restoring never overwrites something that appeared meanwhile
        let again = ModManager::trash_mod(&game, by_id(&back, "PAK:Skin_P"), &trash).unwrap();
        fs::write(&again[0].original, b"newer file").unwrap();
        let (n, conflicts) = ModManager::restore_trashed(&again);
        assert_eq!((n, conflicts.len()), (1, 1), "{conflicts:?}");
        assert_eq!(fs::read(&again[0].original).unwrap(), b"newer file");
        assert!(again[0].stored.is_file(), "the conflicting file stays in the trash");

        // old trash is purged, recent trash is not
        assert_eq!(ModManager::purge_trash(&trash, std::time::Duration::from_secs(3600)), 0);
        assert!(ModManager::purge_trash(&trash, std::time::Duration::ZERO) >= 1);
        assert_eq!(fs::read_dir(&trash).unwrap().count(), 0);
    }

    #[test]
    fn ue4ss_tuner_roundtrip_and_validation() {
        let (_d, game) = make_game("tuner");
        let mut dto = ModManager::get_ue4ss_settings(&game);
        assert!(!dto.gui_console);
        assert_eq!((dto.major_version.as_str(), dto.minor_version.as_str()), ("5", "3"));
        dto.gui_console = true;
        dto.disable_uobject_cache = true;
        dto.minor_version = "4".into();
        ModManager::save_ue4ss_settings(&game, &dto).unwrap();
        let back = ModManager::get_ue4ss_settings(&game);
        assert!(back.gui_console && back.disable_uobject_cache && !back.regular_console);
        assert_eq!(back.minor_version, "4");

        dto.minor_version = "4\nEvil = 1".into();
        assert!(ModManager::save_ue4ss_settings(&game, &dto).is_err(), "newline injection rejected");
        dto.minor_version = "".into();
        assert!(ModManager::save_ue4ss_settings(&game, &dto).is_err());
    }

    #[test]
    fn repair_is_idempotent_and_diagnose_goes_green() {
        let (_d, game) = make_game("repair");
        let bin = PathBuf::from(&game.bin_dir);
        fs::write(bin.join("crash.dmp"), b"x").unwrap();
        fs::write(
            PathBuf::from(&game.pfx).join("user.reg"),
            "WINE REGISTRY Version 2\n\n[Software\\\\Wine\\\\DllOverrides] 17\n",
        )
        .unwrap();
        assert!(RepairEngine::diagnose(&game).crash_dumps == 1);

        let first = RepairEngine::repair(&game);
        assert!(first.iter().any(|a| a.contains("crash dump")));
        assert!(first.iter().any(|a| a.contains("user.reg")));
        assert!(bin.join("dwmapi.dll").is_file(), "proxy restored");
        assert!(fs::symlink_metadata(bin.join("UE4SS.dll")).is_ok());

        let rep = RepairEngine::diagnose(&game);
        assert!(rep.dwmapi_active && rep.settings_ok, "{:?}", rep.issues);
        assert_eq!(rep.crash_dumps, 0);
        assert_eq!(rep.detected_version, "5.3");

        // mods.txt: system mods first, crashing debug mod forced off
        let txt = fs::read_to_string(bin.join("ue4ss/Mods/mods.txt")).unwrap();
        let lines: Vec<_> = txt.lines().collect();
        assert_eq!(lines[0], "Keybinds : 1");
        assert!(lines.contains(&"ActorDumperMod : 0"));
        assert!(lines.contains(&"CoolMod : 1"));

        let second = RepairEngine::repair(&game);
        assert!(!second.iter().any(|a| a.contains("user.reg") || a.contains("crash dump")));
        assert_eq!(
            fs::read_to_string(bin.join("ue4ss/Mods/mods.txt")).unwrap(),
            txt,
            "stable across runs"
        );
    }

    #[test]
    fn curated_per_game_ue4ss_configs_override_the_generic_templates() {
        let (_d, mut game) = make_game("curated");
        let root = PathBuf::from(&game.bin_dir).join("ue4ss");
        for (dir, tag) in [("Grounded", "g1"), ("Grounded2", "g2"), ("Wukong", "wk"), ("Gate", "tiny")] {
            fs::create_dir_all(root.join("CustomGameConfigs").join(dir)).unwrap();
            fs::write(root.join("CustomGameConfigs").join(dir).join("VTableLayout.ini"), format!("[{tag}]")).unwrap();
            fs::write(root.join("CustomGameConfigs").join(dir).join("MemberVariableLayout.ini"), format!("[m{tag}]")).unwrap();
        }
        let pick = |name: &str| RepairEngine::custom_game_config(&root, name).map(|p| p.file_name().unwrap().to_string_lossy().to_string());
        assert_eq!(pick("Grounded 2").as_deref(), Some("Grounded2"), "exact match beats the shorter containing one");
        assert_eq!(pick("Grounded").as_deref(), Some("Grounded"));
        assert_eq!(pick("Black Myth: Wukong").as_deref(), Some("Wukong"));
        assert_eq!(pick("Aggregate Demo"), None, "very short folder names never match by containment");
        assert_eq!(pick("Half Sword"), None);

        game.name = "Black Myth: Wukong".into();
        let actions = RepairEngine::repair(&game);
        assert!(actions.iter().any(|a| a.contains("curated config for 'Wukong'")), "{actions:?}");
        assert_eq!(fs::read_to_string(root.join("VTableLayout.ini")).unwrap(), "[wk]");
        assert_eq!(fs::read_to_string(root.join("MemberVariableLayout.ini")).unwrap(), "[mwk]");
        // games without a curated entry are left with whatever the generic step produced
        game.name = "Demo".into();
        assert!(!RepairEngine::repair(&game).iter().any(|a| a.contains("curated")));
    }

    fn exe_path(game: &GameEntry) -> PathBuf {
        PathBuf::from(&game.shipping_exe)
    }

    fn pe_only_exe(minor: u32) -> Vec<u8> {
        let mut v = vec![0x90u8; 3000];
        v.extend_from_slice(&[0xBD, 0x04, 0xEF, 0xFE]);
        v.extend_from_slice(&0x0001_0000u32.to_le_bytes());
        v.extend_from_slice(&((5u32 << 16) | minor).to_le_bytes());
        v.extend_from_slice(&[0u8; 20]);
        v
    }

    #[test]
    fn an_unknown_engine_version_is_never_guessed() {
        let (_d, game) = make_game("unkver");
        fs::write(exe_path(&game), vec![0u8; 4000]).unwrap(); // no version anywhere
        let ini = PathBuf::from(&game.bin_dir).join("ue4ss/UE4SS-settings.ini");
        // a wrong pin left by an earlier guess
        fs::write(&ini, "[General]\nbUseUObjectArrayCache = false\n[EngineVersionOverride]\nMajorVersion = 5\nMinorVersion = 4\n").unwrap();

        let rep = RepairEngine::diagnose(&game);
        assert_eq!(rep.detected_version, "");
        assert!(rep.issues.iter().any(|i| i.contains("cannot confirm") && i.contains("5.4")), "{:?}", rep.issues);
        let actions = RepairEngine::repair(&game);
        assert!(actions.iter().any(|a| a.contains("could not be determined")), "{actions:?}");
        let text = fs::read_to_string(&ini).unwrap();
        assert_eq!(ini_get(&text, "MajorVersion").as_deref(), Some(""), "{text}");
        assert_eq!(ini_get(&text, "MinorVersion").as_deref(), Some(""));
        assert!(RepairEngine::diagnose(&game).issues.is_empty(), "{:?}", RepairEngine::diagnose(&game).issues);
        // and the tuner shows "automatic" (blank), not an invented 5.4
        let dto = ModManager::get_ue4ss_settings(&game);
        assert_eq!((dto.major_version.as_str(), dto.minor_version.as_str()), ("", ""));
        let mut set = dto.clone();
        set.minor_version = "3".into();
        assert!(ModManager::save_ue4ss_settings(&game, &set).is_err(), "half a version is refused");
        set.minor_version.clear();
        assert!(ModManager::save_ue4ss_settings(&game, &set).is_ok(), "blank = automatic is allowed");
    }

    #[test]
    #[cfg(not(windows))] // the game's log of this test lives in a Wine prefix; on Windows it is in the real AppData
    fn pe_resource_and_the_games_own_log_decide_the_engine_version() {
        let (_d, game) = make_game("logver");
        let root = PathBuf::from(&game.bin_dir).join("ue4ss");
        for v in ["5_05", "5_06"] {
            fs::write(root.join("VTableLayoutTemplates").join(format!("VTableLayout_{v}_Template.ini")), format!("[v{v}]")).unwrap_or_else(|_| {
                fs::create_dir_all(root.join("VTableLayoutTemplates")).unwrap();
                fs::create_dir_all(root.join("MemberVarLayoutTemplates")).unwrap();
                fs::write(root.join("VTableLayoutTemplates").join(format!("VTableLayout_{v}_Template.ini")), format!("[v{v}]")).unwrap();
            });
            fs::create_dir_all(root.join("MemberVarLayoutTemplates")).unwrap();
            fs::write(root.join("MemberVarLayoutTemplates").join(format!("MemberVariableLayout_{v}_Template.ini")), format!("[m{v}]")).unwrap();
        }
        // a project branch name hides the version from strings; only the PE resource knows
        fs::write(exe_path(&game), pe_only_exe(6)).unwrap();
        let actions = RepairEngine::repair(&game);
        assert!(actions.iter().any(|a| a.contains("Unreal Engine 5.6")), "{actions:?}");
        assert_eq!(fs::read_to_string(root.join("VTableLayout.ini")).unwrap(), "[v5_06]");
        let ini = root.join("UE4SS-settings.ini");
        assert_eq!(ini_get(&fs::read_to_string(&ini).unwrap(), "MinorVersion").as_deref(), Some("6"));
        assert!(RepairEngine::diagnose(&game).issues.is_empty());

        // the game then prints its real version in its log, and that wins over the executable
        std::thread::sleep(std::time::Duration::from_millis(30));
        let logs = PathBuf::from(&game.pfx).join("drive_c/users/steamuser/AppData/Local/Demo/Saved/Logs");
        fs::create_dir_all(&logs).unwrap();
        fs::write(logs.join("Demo.log"), "LogInit: Build: x\nLogInit: Engine Version: 5.5.2-1+++Project+Demo-Release\n").unwrap();
        let rep = RepairEngine::diagnose(&game);
        assert_eq!(rep.detected_version, "5.5");
        assert!(rep.issues.iter().any(|i| i.contains("set for Unreal Engine 5.6") && i.contains("is 5.5") && i.contains("Self-Heal")), "{:?}", rep.issues);
        assert!(!rep.settings_ok);
        RepairEngine::repair(&game);
        assert_eq!(fs::read_to_string(root.join("VTableLayout.ini")).unwrap(), "[v5_05]", "layouts follow the corrected version");
        assert!(RepairEngine::diagnose(&game).issues.is_empty());

        // a log older than the executable (the game was updated since) is ignored
        std::thread::sleep(std::time::Duration::from_millis(1100));
        fs::write(exe_path(&game), pe_only_exe(6)).unwrap();
        assert_eq!(RepairEngine::engine_version(&game, Path::new(&game.bin_dir)).unwrap().1, "6");
    }

    #[test]
    fn shader_cache_purge_counts_files() {
        let (d, game) = make_game("shader");
        let sc = d.join("steamapps/shadercache/42");
        fs::create_dir_all(&sc).unwrap();
        fs::write(sc.join("DXVK.dxvk-cache"), b"x").unwrap();
        fs::write(sc.join("keep.txt"), b"x").unwrap();
        fs::write(PathBuf::from(&game.compatdata).join("vkd3d-proton.cache"), b"x").unwrap();
        assert_eq!(RepairEngine::clear_shader_cache(&game), 2);
        assert!(sc.join("keep.txt").is_file());
    }

    #[test]
    fn dll_overrides_follow_enabled_state() {
        let (_d, game) = make_game("dll");
        let bin = PathBuf::from(&game.bin_dir);
        assert_eq!(ModManager::get_active_dll_overrides(&game), "dwmapi=n,b");
        fs::write(bin.join("dwmapi.dll"), b"x").unwrap();
        fs::write(bin.join("dxgi.dll.disabled"), b"x").unwrap();
        let m = ModManager::list_mods(&game);
        assert!(by_id(&m, "DLL:dwmapi.dll").enabled);
        assert!(!by_id(&m, "DLL:dxgi.dll").enabled);
        assert!(ModManager::toggle_mod(&game, by_id(&m, "DLL:dxgi.dll"), true));
        assert_eq!(ModManager::get_active_dll_overrides(&game), "dwmapi,dxgi=n,b");
    }

    #[test]
    fn log_tail_reads_last_lines() {
        let (_d, game) = make_game("log");
        let log = PathBuf::from(&game.bin_dir).join("ue4ss/UE4SS.log");
        let body: String = (0..3000).map(|i| format!("L{}\n", i)).collect();
        fs::write(&log, body).unwrap();
        let tail = ModManager::read_ue4ss_log_tail(&game, 3);
        assert_eq!(tail, "L2997\nL2998\nL2999");
    }
}
