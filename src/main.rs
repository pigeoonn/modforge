#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]
#![recursion_limit = "256"]
pub mod bisect;
pub mod compat;
pub mod covers;
pub mod crashreport;
pub mod configs;
pub mod crash;
pub mod deploy;
pub mod engine;
pub mod fomod;
pub mod clashes;
pub mod frameworks;
pub mod fsutil;
pub mod github;
pub mod gamelogs;
pub mod hashing;
pub mod hydra;
pub mod i18n;
pub mod install;
pub mod kb;
pub mod launchers;
pub mod leveldb;
pub mod downloads;
pub mod profilecode;
pub mod notes;
pub mod score;
pub mod stats;
pub mod welcome;
pub mod loaders;
pub mod modinfo;
pub mod modio;
pub mod logdoctor;
pub mod nexus;
pub mod pak;
pub mod plugins;
pub mod procs;
pub mod profsaves;
pub mod platform;
pub mod readme;
pub mod richtext;
pub mod autobackup;
pub mod background;
pub mod perf;
pub mod recipes;
pub mod repair;
pub mod saves;
pub mod selfupdate;
pub mod screenshots;
pub mod steam;
pub mod steamlib;
pub mod steamshortcut;
pub mod workshop;
pub mod thunderstore;
pub mod trainer;
pub mod ue4ss;
pub mod virustotal;

use bisect::{Bisect, Phase};
use crash::CrashReport;
use engine::{is_auxiliary_exe, GameEntry, RigEngine, RigPaths};
use fsutil::{atomic_write, home_dir, now_ms, place_file};
use notify::{Config, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use repair::{
    LaunchOpts, ModEntry, ModManager, RepairEngine, RigConfigData, TrashedItem, Ue4ssSettingsDto, KNOWN_MOD_DLLS,
};
use serde::Serialize;
use serde_json::{json, Value};
use std::collections::{HashSet, VecDeque};
use std::fs::{self, File};
use std::io::Read;
#[cfg(unix)]
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, RwLock, RwLockReadGuard, RwLockWriteGuard, Weak};
use std::thread;
use std::time::{Duration, Instant};
use tiny_http::{Header, Method, Request, Response, Server};
use walkdir::WalkDir;

const EMBEDDED_UI: &str = include_str!("ui.html");
const MAX_LOG_HISTORY: usize = 300;
const DEFAULT_PORT: u16 = 19450;

/// The port Gantry listens on: 19450, or `RIG_PORT` (for tests and side-by-side runs).
fn server_port() -> u16 {
    std::env::var("RIG_PORT").ok().and_then(|v| v.parse::<u16>().ok()).filter(|p| *p >= 1024).unwrap_or(DEFAULT_PORT)
}
const MAX_JSON_BODY: u64 = 4 * 1024 * 1024;
const SUBSCRIBE_TIMEOUT: Duration = Duration::from_secs(25);

#[derive(Clone, Debug, Serialize)]
pub struct LogEntry {
    pub timestamp: String,
    pub level: String,
    pub message: String,
}

// ───────────────────────────── event fan-out ─────────────────────────────

/// Version counter + condvar: every state change bumps the version and wakes all
/// long-polling `/api/subscribe` requests.
pub struct EventNotifier {
    pub version: AtomicU64,
    pub last_client_ping_ms: AtomicU64,
    pub lock: Mutex<u64>,
    pub cvar: Condvar,
}

impl Default for EventNotifier {
    fn default() -> Self {
        Self::new()
    }
}

impl EventNotifier {
    pub fn new() -> Self {
        Self {
            version: AtomicU64::new(1),
            last_client_ping_ms: AtomicU64::new(now_ms()),
            lock: Mutex::new(1),
            cvar: Condvar::new(),
        }
    }

    pub fn touch_client(&self) {
        self.last_client_ping_ms.store(now_ms(), Ordering::SeqCst);
    }

    pub fn bump(&self) {
        let next = self.version.fetch_add(1, Ordering::SeqCst) + 1;
        let mut g = self.lock.lock().unwrap_or_else(|e| e.into_inner());
        // Concurrent bumps may reach the lock out of order; never move the mirror backwards.
        *g = (*g).max(next);
        self.cvar.notify_all();
    }

    /// Block until the version differs from `client_ver` or `timeout` elapses.
    pub fn wait_for_change(&self, client_ver: u64, timeout: Duration) -> u64 {
        self.touch_client();
        let deadline = Instant::now() + timeout;
        let mut guard = self.lock.lock().unwrap_or_else(|e| e.into_inner());
        loop {
            let cur = self.version.load(Ordering::SeqCst);
            if cur != client_ver {
                return cur;
            }
            let now = Instant::now();
            if now >= deadline {
                return cur;
            }
            guard = self
                .cvar
                .wait_timeout(guard, deadline - now)
                .unwrap_or_else(|e| e.into_inner())
                .0;
        }
    }
}

// ───────────────────────────── shared state ─────────────────────────────

pub struct AppState {
    /// Mod-library status of the selected game (see `deploy::Library::status_json`).
    pub library: Value,
    pub paths: RigPaths,
    pub config: RigConfigData,
    pub games: Vec<GameEntry>,
    pub protons: Vec<(String, PathBuf)>,
    pub selected_idx: usize,
    pub selected_proton_idx: usize,
    pub mods: Vec<ModEntry>,
    pub logs: VecDeque<LogEntry>,
    pub last_toast: String,
    pub gamemode: bool,
    pub mangohud: bool,
    pub auto_trainer: bool,
    pub is_running: Arc<AtomicBool>,
    pub running_pid: Arc<AtomicU32>,
    pub notifier: Arc<EventNotifier>,
    pub notes: notes::Notes,
    /// A MangoHud log the user opened by hand: its file name and numbers.
    pub perf_manual: Option<(String, perf::FrameStats)>,
    pub steam_env: SteamEnv,
    /// Set while a Full Setup runs so two cannot interleave on one machine.
    pub setup_busy: Arc<AtomicBool>,
    /// The checklist of the last "Make this game moddable" run, ticked off as it goes.
    pub setup_run: Option<SetupRun>,
    /// What came of the last trainer start (one trainer at a time).
    pub trainer_run: Option<TrainerRun>,
    /// Mods waiting to be installed, one after another, and how the current batch is going.
    install_jobs: VecDeque<InstallJob>,
    install_worker: bool,
    pub batch: Option<Batch>,
    /// Counts every log line ever written (the log itself keeps only the newest).
    pub log_seq: u64,
    /// Latest crash analysis (any game); the UI shows it for the game it belongs to.
    pub crash: Option<CrashReport>,
    /// Open "find the culprit" session, if any (one at a time).
    pub bisect: Option<Bisect>,
    pub last_run: Option<LastRun>,
    /// Undo stack for mod changes (newest last), across games.
    pub undo: Vec<UndoEntry>,
    pub nexus: NexusState,
    pub modio: ModioState,
    /// Where new downloads are looked for (tests point it at a temporary folder).
    pub downloads_dir: Option<PathBuf>,
    pub vt: VtState,
    /// Where the UE4SS release is looked up (tests point this at a local fake).
    pub ue4ss_source: ue4ss::Source,
    pub loader_sources: loaders::Sources,
    pub ge_source: compat::GeSource,
    /// Where cover artwork is looked up (tests point it at a local server).
    pub covers_base: String,
    /// A newer Gantry release than this one, when GitHub has said so.
    pub new_version: Option<String>,
    /// The file Gantry could replace by itself (the AppImage, the Windows build), when this install is one of those.
    pub self_target: Option<PathBuf>,
    /// The version that was put in place and waits for a restart.
    pub update_ready: Option<String>,
    /// How many reports of Gantry's own failures are kept on this computer.
    pub crash_reports: usize,
    /// What is known about particular games (built-in list plus the user's `recipes.json`).
    pub recipes: Vec<recipes::Recipe>,
    /// GitHub's API address and, in tests, the address its downloads come from.
    pub github_api: String,
    pub github_allow: Option<String>,
    pub covers_allow_http: bool,
    /// Tests point the tool search at temporary folders (`compatibilitytools.d` style).
    pub tools_dirs: Option<Vec<PathBuf>>,
    pub pending_install: Option<PendingInstall>,
    /// More mods whose place had to be guessed, waiting their turn for the same question.
    pub pending_queue: std::collections::VecDeque<PendingInstall>,
    /// Installer scripts waiting for answers, oldest first.
    pub pending_fomod: Vec<PendingFomod>,
    /// A shared setup (file or code) shown to the user before anything of it is fetched.
    pub import_preview: Option<ImportPreview>,
    /// Where game shortcuts are written (tests point it at a temporary folder; None = the real home).
    pub shortcut_home: Option<PathBuf>,
    /// Games with trouble (appid → "warn" or "bad"), from the background watch.
    pub health: std::collections::BTreeMap<String, String>,
    /// Steam accounts and everything they own.
    pub steam_lib: SteamLibState,
    /// The home page's "trending for your games" shelf and when it was made (it is kept for a few minutes).
    pub home_trending: Option<(Instant, Value)>,
    pub github_updates: Vec<GithubUpdate>,
    pub modio_updates: Vec<ModioUpdate>,
    /// `LastPlayed` per Steam appid (seconds), read from Steam's own config.
    pub steam_played: std::collections::HashMap<String, u64>,
    pub thunder: ThunderState,
}

/// Thunderstore: where to ask, and which installed packages have a newer version.
#[derive(Clone)]
pub struct ThunderState {
    pub base: String,
    /// Tests talk to a local fake over plain http; production never does.
    pub allow_http: bool,
    pub updates: Vec<ThunderUpdate>,
}

impl Default for ThunderState {
    fn default() -> Self {
        Self { base: thunderstore::DEFAULT_BASE.into(), allow_http: false, updates: Vec::new() }
    }
}

/// A newer release of a mod that came from GitHub.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct GithubUpdate {
    pub appid: String,
    pub game: String,
    pub lib_id: String,
    pub name: String,
    pub installed_version: String,
    pub new_version: String,
    pub kind: &'static str,
    pub repo: String,
    /// Where the new file is (empty when the release has several files and none is clearly the same one).
    #[serde(skip)]
    pub url: String,
    /// What the mod will be remembered as once the new file is in.
    #[serde(skip)]
    pub new_ref: Option<github::GithubRef>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ThunderUpdate {
    pub appid: String,
    pub game: String,
    pub lib_id: String,
    pub name: String,
    pub installed_version: String,
    pub new_version: String,
    pub pkg: thunderstore::ThunderRef,
}

/// Your Steam library: the Web API key, the account shown and the owned games last fetched.
#[derive(Clone)]
pub struct SteamLibState {
    pub key: Option<String>,
    pub base: String,
    /// Tests talk to a local fake over plain http; production never does.
    pub allow_http: bool,
    /// Steam installs to read accounts from (`None` = the real ones).
    pub roots: Option<Vec<PathBuf>>,
    pub owned: Vec<steamlib::Owned>,
    /// SteamID64 the `owned` list belongs to.
    pub owned_for: String,
    pub fetched_ms: u64,
    pub fetching: bool,
    pub error: Option<String>,
}

impl Default for SteamLibState {
    fn default() -> Self {
        Self {
            key: None,
            base: steamlib::DEFAULT_BASE.into(),
            allow_http: false,
            roots: None,
            owned: Vec::new(),
            owned_for: String::new(),
            fetched_ms: 0,
            fetching: false,
            error: None,
        }
    }
}

/// What VirusTotal said about the program files of one mod.
#[derive(Clone, Debug, Default, Serialize)]
pub struct VtReport {
    pub at_ms: u64,
    /// Files still waiting for their turn (free keys may ask four times a minute).
    pub waiting: usize,
    pub files: Vec<VtFile>,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct VtFile {
    pub file: String,
    pub sha256: String,
    pub verdict: Option<virustotal::Verdict>,
    pub error: String,
}

/// The user's own mod.io key (read-only, from mod.io/me/access); it stays on this computer.
#[derive(Clone)]
pub struct ModioState {
    pub key: Option<String>,
    pub base: String,
    /// Tests talk to a local fake server over plain http; production never does.
    pub allow_http: bool,
}

impl Default for ModioState {
    fn default() -> Self {
        Self { key: None, base: modio::DEFAULT_BASE.into(), allow_http: false }
    }
}

/// VirusTotal: the user's own key (never in the UI JSON) and the answers of this run.
#[derive(Clone)]
pub struct VtState {
    pub key: Option<String>,
    pub base: String,
    /// Tests talk to a local fake server over plain http; production never does.
    pub allow_http: bool,
    /// "appid:mod id" -> answers
    pub reports: std::collections::BTreeMap<String, VtReport>,
    pub busy: std::collections::BTreeSet<String>,
}

impl Default for VtState {
    fn default() -> Self {
        Self { key: None, base: virustotal::BASE.into(), allow_http: false, reports: Default::default(), busy: Default::default() }
    }
}

/// Nexus Mods account state. The API key lives here and in a private file, never in the UI JSON.
#[derive(Clone)]
pub struct NexusState {
    pub key: Option<String>,
    pub user: Option<nexus::UserInfo>,
    pub base: String,
    /// Tests talk to a local fake server over plain http; production never does.
    pub allow_http: bool,
    /// A link from the website waiting for the user's confirmation.
    pub pending: Option<nexus::NxmLink>,
    /// When the waiting link is the update the user asked for, which library mod it replaces.
    pub pending_for: Option<UpdateTarget>,
    /// Mods with a newer file on Nexus (all games), from the last check.
    pub updates: Vec<UpdateRow>,
    /// Library mods that need other mods which are not in the library.
    pub needs: Vec<NeedRow>,
    pub checked_ms: u64,
    pub checking: bool,
    pub check_error: Option<String>,
    /// Free accounts update through the website: the file whose link we are waiting for.
    pub expect: Option<(UpdateTarget, nexus::NexusRef)>,
    /// Mods whose Nexus page Gantry opened and whose download link it is waiting for; that link needs no confirmation.
    pub wants: Vec<Want>,
    /// Mods still to be opened, one at a time, as each download arrives.
    pub queue: std::collections::VecDeque<Want>,
    /// How many the current run of the queue has had and got through, for "3 of 7".
    pub queue_total: usize,
    pub queue_done: usize,
}

/// A mod the user asked for on Nexus Mods with a free account (the website must start its download).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Want {
    pub appid: String,
    pub game: String,
    pub mod_id: u64,
    pub name: String,
    pub asked_ms: u64,
    /// The file to open the page at (0: the mod's main file).
    pub file_id: u64,
}

/// How long Gantry keeps waiting for the click on the website.
const WANT_FOR_MS: u64 = 30 * 60_000;

impl Default for NexusState {
    fn default() -> Self {
        Self {
            key: None,
            user: None,
            base: nexus::DEFAULT_BASE.into(),
            allow_http: false,
            pending: None,
            pending_for: None,
            updates: Vec::new(),
            needs: Vec::new(),
            checked_ms: 0,
            checking: false,
            check_error: None,
            expect: None,
            wants: Vec::new(),
            queue: std::collections::VecDeque::new(),
            queue_total: 0,
            queue_done: 0,
        }
    }
}

/// The library mod an update is meant for.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct UpdateTarget {
    pub appid: String,
    pub lib_id: String,
    pub name: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct UpdateRow {
    pub appid: String,
    pub game: String,
    pub lib_id: String,
    pub name: String,
    pub installed_version: String,
    pub new_version: String,
    pub new_file_id: u64,
    pub new_file_name: String,
    /// What the author says changed in the new file (plain text, shortened).
    pub changes: String,
    pub nexus: nexus::NexusRef,
    pub page_url: String,
}

/// A mod that is extracted and waiting for the user's OK on where its files go.
#[derive(Clone)]
pub struct PendingInstall {
    pub appid: String,
    pub stage: PathBuf,
    pub name: String,
    pub fname: String,
    pub version: String,
    pub opts_nexus: Option<nexus::NexusRef>,
    pub preview: deploy::Preview,
}

/// A shared setup waiting for the user's yes: what it is, for which game, the unpacked files of a modpack (removed
/// when it is cancelled) and what the window shows.
pub struct ImportPreview {
    pub appid: String,
    pub spec: deploy::ProfileSpec,
    pub files: Option<PathBuf>,
    pub view: Value,
}

/// A mod with an installer script (FOMOD) that is extracted and waiting for the user's answers.
#[derive(Clone)]
pub struct PendingFomod {
    pub appid: String,
    /// The extracted archive (removed when the installer is done or cancelled).
    pub stage: PathBuf,
    /// The folder the script's paths start from.
    pub root: PathBuf,
    pub module: fomod::Module,
    pub fname: String,
    opts: InstallOpts,
}

/// Mods a library mod needs that are not in the library (yet), as its Nexus page lists them.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct NeedRow {
    pub appid: String,
    pub lib_id: String,
    pub name: String,
    pub missing: Vec<MissingReq>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct MissingReq {
    pub name: String,
    pub page_url: String,
    pub notes: String,
}

/// The requirements of `nx` that no mod of `have` was installed from.
fn missing_requirements(reqs: &[nexus::Requirement], have: &[deploy::LibMod]) -> Vec<MissingReq> {
    reqs.iter()
        .filter(|r| !r.external && r.mod_id > 0 && !r.game.is_empty())
        .filter(|r| !have.iter().any(|m| m.nexus.as_ref().map(|n| n.game == r.game && n.mod_id == r.mod_id).unwrap_or(false)))
        .map(|r| MissingReq { name: r.name.clone(), page_url: r.page(), notes: r.notes.clone() })
        .collect()
}

const UNDO_DEPTH: usize = 20;
const TRASH_MAX_AGE: Duration = Duration::from_secs(14 * 86_400);

/// What it takes to undo one change: the enabled set from before it, plus anything moved to the trash.
#[derive(Clone, Debug)]
pub struct UndoEntry {
    pub appid: String,
    pub label: String,
    pub enabled_before: Vec<String>,
    pub trashed: Vec<TrashedItem>,
}

/// How the most recent observed run of a game ended.
#[derive(Clone, Debug, Serialize)]
pub struct LastRun {
    pub appid: String,
    pub ended_ms: u64,
    pub crashed: bool,
}

/// Where Steam's per-user config lives and whether Steam is open. `None` = detect for real;
/// tests pin both so they never touch (or wait on) the developer's actual Steam.
#[derive(Clone, Default)]
pub struct SteamEnv {
    pub localconfig: Option<PathBuf>,
    /// Steam's `config.vdf` (tests point it at a temporary copy).
    pub config_vdf: Option<PathBuf>,
    pub running: Option<bool>,
    /// `steamapps` folders to scan; `None` = discover the real ones.
    pub libraries: Option<Vec<PathBuf>>,
}

impl SteamEnv {
    fn localconfig(&self) -> Option<PathBuf> {
        self.localconfig.clone().or_else(steam::find_localconfig)
    }
    /// Every account's file (the logged-in one first); tests pin a single file.
    fn localconfigs(&self) -> Vec<PathBuf> {
        match &self.localconfig {
            Some(p) => vec![p.clone()],
            None => steam::find_all_localconfigs(),
        }
    }
    fn config_vdf(&self) -> Option<PathBuf> {
        self.config_vdf.clone().or_else(steam::find_config_vdf)
    }
    fn running(&self) -> bool {
        self.running.unwrap_or_else(procs::steam_running)
    }
}

type SharedState = Arc<RwLock<AppState>>;

fn read_state(state: &SharedState) -> RwLockReadGuard<'_, AppState> {
    state.read().unwrap_or_else(|e| e.into_inner())
}

fn write_state(state: &SharedState) -> RwLockWriteGuard<'_, AppState> {
    state.write().unwrap_or_else(|e| e.into_inner())
}

/// Log without keeping the lock around any longer than the push itself.
/// Put a line in the notification centre (the bell in the header).
fn add_note(state: &SharedState, kind: &str, text: impl Into<String>, appid: &str) {
    let mut st = write_state(state);
    st.notes.add(now_ms(), kind, &text.into(), appid);
    st.notifier.bump();
}

fn log(state: &SharedState, level: &str, msg: impl Into<String>) {
    write_state(state).log(level, msg);
}

fn selected_game(state: &SharedState) -> Option<GameEntry> {
    let st = read_state(state);
    st.games.get(st.selected_idx).cloned()
}

/// The settings. A file that cannot be read is kept aside and the copy from before its last change is used; when even
/// that is gone, the newest weekly backup's settings. Never silently the defaults over a damaged file.
fn load_config(paths: &RigPaths) -> RigConfigData {
    match fsutil::read_json_kept::<RigConfigData>(&paths.config_file) {
        fsutil::Loaded::Fine(c) | fsutil::Loaded::Recovered(c) => c,
        fsutil::Loaded::Missing => RigConfigData::default(),
        fsutil::Loaded::Lost(_) => {
            let dir = paths.cache_dir.parent().unwrap_or(&paths.cache_dir).join("auto-backups");
            let from_backup = autobackup::list(&dir).iter().find_map(|e| autobackup::read(&dir, &e.name).ok().and_then(|v| serde_json::from_value::<RigConfigData>(v["config"].clone()).ok()));
            match from_backup {
                Some(c) => {
                    if let Ok(json) = serde_json::to_string_pretty(&c) {
                        let _ = fsutil::atomic_write_kept(&paths.config_file, json);
                    }
                    c
                }
                None => RigConfigData::default(),
            }
        }
    }
}

/// Right after start: every game's mod list and deploy journal is read once, so a damaged one is found and recovered
/// before anything works with it (and the person is told).
fn spawn_self_check(state: Weak<RwLock<AppState>>) {
    thread::spawn(move || {
        let Some(st) = state.upgrade() else { return };
        let games: Vec<GameEntry> = read_state(&st).games.iter().filter(|g| g.is_installed).cloned().collect();
        for g in &games {
            if let Some(lib) = library_of(g) {
                lib.self_check();
            }
        }
        report_repaired(&st);
    });
}

/// Tell about files that were found damaged and what was done (once each).
fn report_repaired(state: &SharedState) {
    for (path, ok) in fsutil::take_repaired() {
        let name = path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
        let text = if ok {
            format!("{} was damaged and could not be read. ModForge used the copy from before its last change; the damaged file is kept next to it.", name)
        } else {
            format!("{} was damaged and there was no good copy of it. The damaged file is kept next to it (in {}).", name, path.parent().map(|p| p.display().to_string()).unwrap_or_default())
        };
        log(state, "WARN", text.clone());
        add_note(state, "repair", text, "");
    }
}

impl AppState {
    pub fn new(notifier: Arc<EventNotifier>) -> Self {
        let t0 = Instant::now();
        let paths = RigPaths::new();
        let config = load_config(&paths);
        let games = RigEngine::scan_all_games(&paths, &config);
        let protons = RigEngine::find_protons();
        let mut st = Self::with(paths, config, games, protons, notifier);
        let elapsed_ms = t0.elapsed().as_secs_f64() * 1000.0;
        st.log(
            "INFO",
            format!(
                "ModForge ready in {:.2}ms ({} games indexed).",
                elapsed_ms,
                st.games.len()
            ),
        );
        st
    }

    pub fn with(
        paths: RigPaths,
        config: RigConfigData,
        games: Vec<GameEntry>,
        protons: Vec<(String, PathBuf)>,
        notifier: Arc<EventNotifier>,
    ) -> Self {
        let selected_idx = games
            .iter()
            .position(|g| g.name.to_lowercase().contains("half sword"))
            .unwrap_or(0);
        let mods = games.get(selected_idx).map(ModManager::list_mods).unwrap_or_default();
        Self {
            library: Value::Null,
            paths,
            config,
            games,
            protons,
            selected_idx,
            selected_proton_idx: 0,
            mods,
            logs: VecDeque::new(),
            last_toast: String::new(),
            gamemode: true,
            mangohud: false,
            auto_trainer: true,
            is_running: Arc::new(AtomicBool::new(false)),
            running_pid: Arc::new(AtomicU32::new(0)),
            notifier,
            notes: notes::Notes::default(),
            perf_manual: None,
            steam_env: SteamEnv::default(),
            setup_busy: Arc::new(AtomicBool::new(false)),
            setup_run: None,
            trainer_run: None,
            install_jobs: VecDeque::new(),
            install_worker: false,
            batch: None,
            log_seq: 0,
            crash: None,
            bisect: None,
            last_run: None,
            undo: Vec::new(),
            nexus: NexusState::default(),
            modio: ModioState::default(),
            downloads_dir: None,
            vt: VtState::default(),
            ue4ss_source: ue4ss::Source::default(),
            loader_sources: loaders::Sources::default(),
            ge_source: compat::GeSource::default(),
            covers_base: covers::STORE_SEARCH.into(),
            new_version: None,
            self_target: selfupdate::target(),
            update_ready: None,
            crash_reports: 0,
            recipes: Vec::new(),
            github_api: github::API.into(),
            github_allow: None,
            covers_allow_http: false,
            tools_dirs: None,
            pending_install: None,
            pending_queue: std::collections::VecDeque::new(),
            pending_fomod: Vec::new(),
            import_preview: None,
            shortcut_home: None,
            health: Default::default(),
            steam_lib: SteamLibState::default(),
            home_trending: None,
            github_updates: Vec::new(),
            modio_updates: Vec::new(),
            steam_played: Default::default(),
            thunder: ThunderState::default(),
        }
    }

    pub fn log(&mut self, level: &str, msg: impl Into<String>) {
        let m = msg.into();
        self.last_toast = m.clone();
        while self.logs.len() >= MAX_LOG_HISTORY {
            self.logs.pop_front();
        }
        self.logs.push_back(LogEntry {
            timestamp: fsutil::local_hms(),
            level: level.to_string(),
            message: m,
        });
        self.log_seq += 1;
        self.notifier.bump();
    }

    /// Proton script for `g`: the build its own launcher chose (Hydra), else rig's selection.
    pub fn proton_for(&self, g: &GameEntry) -> Option<PathBuf> {
        hydra::proton_script(g)
            .or_else(|| self.config.proton_choice.get(&g.appid).map(PathBuf::from).filter(|p| p.is_file()))
            .or_else(|| self.protons.get(self.selected_proton_idx).map(|(_, p)| p.clone()))
    }

    pub fn cache_root(&self) -> PathBuf {
        self.paths.cache_dir.parent().unwrap_or(&self.paths.cache_dir).to_path_buf()
    }

    pub fn save_custom_games(&self) {
        let custom: Vec<&GameEntry> = self.games.iter().filter(|g| g.is_custom).collect();
        if let Ok(json) = serde_json::to_string_pretty(&custom) {
            let _ = fsutil::atomic_write_kept(&self.paths.custom_games_file, json);
        }
    }

    pub fn save_config(&self) {
        if let Ok(json) = serde_json::to_string_pretty(&self.config) {
            let _ = fsutil::atomic_write_kept(&self.paths.config_file, json);
        }
    }

    /// Commit a freshly computed mod list for the game it was computed for.
    /// Returns false (and drops the list) when the selection moved meanwhile.
    fn apply_mods(&mut self, appid: &str, list: Vec<ModEntry>) -> bool {
        let idx = self.selected_idx;
        match self.games.get_mut(idx) {
            Some(g) if g.appid == appid => {
                g.mod_count = list.len();
                g.refresh_flags(); // e.g. UE4SS was just installed
                self.mods = list;
                self.notifier.bump();
                true
            }
            _ => false,
        }
    }

    pub fn select_by_appid(&mut self, appid: &str) -> bool {
        match self.games.iter().position(|g| g.appid == appid) {
            Some(i) => {
                self.selected_idx = i;
                true
            }
            None => {
                self.selected_idx = self.selected_idx.min(self.games.len().saturating_sub(1));
                false
            }
        }
    }

    /// What the user keeps about their games: pins, notes, tags and the time played.
    fn meta_json(&self) -> Value {
        let mut ids: std::collections::BTreeSet<&String> = self.config.favorites.iter().collect();
        ids.extend(self.config.game_notes.keys());
        ids.extend(self.config.game_tags.keys());
        ids.extend(self.config.playtime_s.keys());
        let mut m = serde_json::Map::new();
        for id in ids {
            m.insert(
                id.clone(),
                json!({
                    "fav": self.config.favorites.contains(id),
                    "notes": self.config.game_notes.get(id).cloned().unwrap_or_default(),
                    "tags": self.config.game_tags.get(id).cloned().unwrap_or_default(),
                    "playtime_s": self.config.playtime_s.get(id).copied().unwrap_or(0),
                }),
            );
        }
        Value::Object(m)
    }

    fn hidden_games_json(&self) -> Vec<Value> {
        let mut hidden: Vec<(&String, &String)> = self.config.hidden_games.iter().collect();
        hidden.sort_by_key(|(_, n)| n.to_lowercase());
        hidden.into_iter().map(|(a, n)| json!({ "appid": a, "name": n })).collect()
    }

    /// Controller mode: chosen, asked for with `rig --deck`, or the machine is a Steam Deck and nobody said no.
    pub fn deck_on(&self) -> bool {
        self.config.deck_mode || std::env::var_os("RIG_DECK").is_some() || (platform::is_steam_deck() && !self.config.deck_declined)
    }

    pub fn to_json(&self) -> Value {
        let active_profiles: Vec<String> = self
            .games
            .get(self.selected_idx)
            .and_then(|g| self.config.profiles.get(&g.appid))
            .map(|map| {
                let mut keys: Vec<String> = map.keys().cloned().collect();
                keys.sort();
                keys
            })
            .unwrap_or_default();

        let mut v = json!({
            "version": self.notifier.version.load(Ordering::SeqCst),
            "games": self.games,
            "hidden_games": self.hidden_games_json(),
            "mods": self.mods,
            "library": self.library,
            "vanilla": self.config.vanilla_sessions,
            "safe_mode": self.config.safe_mode_sessions,
            "app_version": env!("CARGO_PKG_VERSION"), "os": platform::OS, "new_version": self.new_version, "can_self_update": self.self_target.is_some(), "crash_reports": self.crash_reports, "update_ready": self.update_ready, "saved_mods": self.games.get(self.selected_idx).and_then(|g| self.config.saved_mods.get(&g.appid)).map(|v| v.iter().filter_map(|c| Some(format!("{}:{}", c["source"].as_str()?, c["id"].as_str()?))).collect::<Vec<_>>()).unwrap_or_default(), "settings": { "tour_done": self.config.tour_done, "keep_background": self.config.keep_background, "install_preview": !self.config.no_install_preview, "ui_scale": if self.config.ui_scale == 0 { 100 } else { self.config.ui_scale }, "reduce_motion": self.config.reduce_motion, "motion": if self.config.reduce_motion { "off" } else if self.config.calm_motion { "calm" } else { "full" }, "high_contrast": self.config.high_contrast, "deck": self.deck_on(), "is_deck": platform::is_steam_deck(), "theme": if self.config.theme.is_empty() { "dark" } else { self.config.theme.as_str() }, "custom_theme": serde_json::from_str::<Value>(&self.config.custom_theme).unwrap_or(Value::Null), "accent_from_game": self.config.accent_from_game, "accent": self.config.accent, "compact": self.config.compact, "show_adult": self.config.show_adult, "layout": serde_json::from_str::<Value>(&self.config.layout).unwrap_or(Value::Null), "seen_version": self.config.seen_version, "auto_backup": !self.config.no_auto_backup, "sounds": self.config.sounds, "haptics": self.config.haptics, "sync_folder": self.config.sync_folder, "download_limit_kb": self.config.download_limit_kb, "downloads_wait": self.config.downloads_wait, "checks_while_playing": self.config.checks_while_playing, "virustotal": self.config.virustotal, "vt_key": self.vt.key.is_some(), "vt_key_hint": self.vt.key.as_ref().map(|k| format!("…{}", &k[k.len().saturating_sub(4)..])) },
            "notes": self.notes.json(),
            "meta": self.meta_json(),
            "health": self.health.iter().filter(|(appid, _)| self.games.iter().any(|g| &g.appid == *appid && g.is_installed)).map(|(k, v)| (k.clone(), v.clone())).collect::<std::collections::BTreeMap<_, _>>(),
            "pending_fomod": self.pending_fomod.first().map(|p| json!({ "name": if p.module.name.is_empty() { p.fname.clone() } else { p.module.name.clone() }, "image": p.module.image, "queue": self.pending_fomod.len() })),
            "pending_install": self.pending_install.as_ref().map(|p| json!({ "name": p.name, "placed_by": p.preview.placed_by, "target": p.preview.target, "files": p.preview.files, "folders": p.preview.folders.iter().take(6).collect::<Vec<_>>(), "notes": p.preview.notes, "more": self.pending_queue.len() })),
            "unity": self.games.get(self.selected_idx).filter(|g| g.is_installed).and_then(|g| loaders::detect(Path::new(&g.game_dir)).map(|u| json!({ "il2cpp": u.il2cpp, "x64": u.x64, "loader": u.loader, "recommended": recipes::find(&self.recipes, &g.appid, &g.steam_appid).map(|r| r.loader.clone()).unwrap_or_default() }))),
            "language": if i18n::valid(&self.config.language) { self.config.language.as_str() } else { i18n::DEFAULT_LANG },
            "logs": self.logs,
            "last_toast": self.last_toast,
            "profiles": active_profiles,
            "protons": self.protons.iter().map(|(n, _)| n.clone()).collect::<Vec<_>>(),
            "selected_idx": self.selected_idx,
            "selected_proton_idx": self.selected_proton_idx,
            "gamemode": self.gamemode,
            "mangohud": self.mangohud,
            "auto_trainer": self.auto_trainer,
            "is_running": self.is_running.load(Ordering::SeqCst),
            "crash": self.crash,
            "bisect": self.bisect_json(),
            "last_run": self.last_run,
            "undo": self.undo_json(),
            "nexus": self.nexus_json(),
            "steam_lib": self.steam_lib_json(),
            "last_played": self.last_played_json(),
            "modio": json!({ "configured": self.modio.key.is_some(), "key_hint": self.modio.key.as_ref().map(|k| format!("…{}", &k[k.len().saturating_sub(4)..])) }),
            "thunder_updates": self.thunder.updates.iter().filter(|u| self.games.iter().any(|g| g.appid == u.appid && g.is_installed)).map(|u| serde_json::to_value(u).unwrap_or(Value::Null)).chain(self.github_updates.iter().filter(|u| self.games.iter().any(|g| g.appid == u.appid && g.is_installed)).map(|u| serde_json::to_value(u).unwrap_or(Value::Null))).chain(self.modio_updates.iter().filter(|u| self.games.iter().any(|g| g.appid == u.appid && g.is_installed)).map(|u| serde_json::to_value(u).unwrap_or(Value::Null))).collect::<Vec<_>>(),
        });
        v["trainer_shelf"] = self.trainer_shelf_json();
        v["trainer_run"] = self.trainer_run_json();
        v["batch"] = serde_json::to_value(&self.batch).unwrap_or(Value::Null);
        v["import_preview"] = self.import_preview.as_ref().map(|p| p.view.clone()).unwrap_or(Value::Null);
        v["place_rules"] = json!(self.games.get(self.selected_idx).and_then(|g| self.config.place_rules.get(&g.appid)).map(|r| r.len()).unwrap_or(0));
        v
    }

    /// The trainers kept for the selected game.
    fn trainer_shelf_json(&self) -> Value {
        let Some(g) = self.games.get(self.selected_idx) else { return json!([]) };
        let current = self.config.trainers.get(&g.appid).map(PathBuf::from);
        trainer_shelf(self, &g.appid)
            .iter()
            .map(|p| {
                let name = trainer_display(&g.appid, p);
                let made_for = trainer::made_for(&name).map(|(a, b)| if a == b { trainer::version_text(&a) } else { format!("{}–{}", trainer::version_text(&a), trainer::version_text(&b)) });
                json!({ "id": p.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default(), "name": name, "made_for": made_for, "current": current.as_deref() == Some(p.as_path()) })
            })
            .collect()
    }

    fn trainer_run_json(&self) -> Value {
        self.trainer_run.as_ref().map(|r| json!({ "appid": r.appid, "state": r.state, "file": r.file, "alive": r.pid != 0 && procs::alive(r.pid), "secs": r.secs })).unwrap_or(Value::Null)
    }

    /// When each installed game was last played (ms): Steam's own record or Gantry's launch, the newer.
    fn last_played_json(&self) -> Value {
        let mut m = serde_json::Map::new();
        for g in self.games.iter().filter(|g| g.is_installed) {
            let steam = self.steam_played.get(&g.appid).or_else(|| self.steam_played.get(&g.steam_appid)).copied().unwrap_or(0) * 1000;
            let own = self.config.last_launched.get(&g.appid).copied().unwrap_or(0);
            if steam.max(own) > 0 {
                m.insert(g.appid.clone(), json!(steam.max(own)));
            }
        }
        Value::Object(m)
    }

    fn steam_lib_json(&self) -> Value {
        let roots = self.steam_lib.roots.clone().unwrap_or_else(RigPaths::steam_roots);
        let accounts = steamlib::accounts(&roots);
        let active = steamlib::pick(&accounts, &self.config.steam_account).map(|a| a.steamid.clone()).unwrap_or_default();
        json!({
            "configured": self.steam_lib.key.is_some(),
            "key_hint": self.steam_lib.key.as_ref().map(|k| format!("…{}", &k[k.len().saturating_sub(4)..])),
            "accounts": accounts,
            "active": active,
            "owned_count": self.steam_lib.owned.len(),
            "owned_for": self.steam_lib.owned_for,
            "fetched_ms": self.steam_lib.fetched_ms,
            "fetching": self.steam_lib.fetching,
            "error": self.steam_lib.error,
        })
    }

    fn nexus_json(&self) -> Value {
        let target = self.games.get(self.selected_idx).map(|g| g.name.clone()).unwrap_or_default();
        json!({
            "configured": self.nexus.key.is_some(),
            "key_hint": self.nexus.key.as_ref().map(|k| format!("…{}", &k[k.len().saturating_sub(4)..])),
            "user": self.nexus.user.as_ref().map(|u| u.name.clone()),
            "premium": self.nexus.user.as_ref().map(|u| u.premium).unwrap_or(false),
            "pending": self.nexus.pending.as_ref().map(|l| json!({
                "game": l.game, "mod_id": l.mod_id, "file_id": l.file_id,
                "free_key": l.key.is_some(),
                "target": self.nexus.pending_for.as_ref().map(|t| t.name.clone()).unwrap_or(target.clone()),
                "update": self.nexus.pending_for.is_some(),
            })),
            "updates": self.nexus.updates.iter().filter(|u| self.games.iter().any(|g| g.appid == u.appid && g.is_installed)).cloned().collect::<Vec<_>>(),
            "needs": self.nexus.needs.iter().filter(|n| self.games.iter().any(|g| g.appid == n.appid && g.is_installed)).cloned().collect::<Vec<_>>(),
            "checked_ms": self.nexus.checked_ms,
            "checking": self.nexus.checking,
            "check_error": self.nexus.check_error,
            "waiting_for": self.nexus.expect.as_ref().map(|(t, _)| json!({ "appid": t.appid, "lib_id": t.lib_id, "name": t.name }))
                .or_else(|| self.nexus.wants.first().map(|w| json!({ "appid": w.appid, "lib_id": "", "name": w.name }))),
            "queued": self.nexus.queue.len(),
            "queue": (self.nexus.queue_total > 0).then(|| json!({ "total": self.nexus.queue_total, "done": self.nexus.queue_done, "next": self.nexus.queue.iter().take(5).map(|w| w.name.clone()).collect::<Vec<_>>() })),
        })
    }

    fn undo_json(&self) -> Value {
        let appid = self.games.get(self.selected_idx).map(|g| g.appid.as_str()).unwrap_or("");
        match self.undo.iter().rev().find(|u| u.appid == appid) {
            Some(u) => json!({ "label": u.label, "count": self.undo.iter().filter(|x| x.appid == appid).count() }),
            None => Value::Null,
        }
    }

    fn mod_display_name(&self, id: &str) -> String {
        if let Some(n) = self.bisect.as_ref().and_then(|b| b.names.get(id)) {
            return n.clone();
        }
        self.mods
            .iter()
            .find(|m| m.id == id)
            .map(|m| m.name.clone())
            .unwrap_or_else(|| id.split_once(':').map(|(_, n)| n).unwrap_or(id).to_string())
    }

    fn bisect_json(&self) -> Value {
        let Some(b) = &self.bisect else { return Value::Null };
        let names = |ids: &[String]| ids.iter().map(|i| self.mod_display_name(i)).collect::<Vec<_>>();
        json!({
            "appid": b.appid,
            "phase": b.phase,
            "round": b.round,
            "max_rounds": b.max_rounds,
            "rounds_left": b.rounds_left(),
            "testing": names(&b.testing),
            "pool": names(&b.pool),
            "found": names(&b.found),
            "originals": names(&b.originals),
            "note": b.note,
            "round_started_ms": b.round_started_ms,
            "finished": b.is_finished(),
        })
    }
}

/// Recompute the selected game's mod list *without* holding the state lock (the
/// filesystem walk can be slow), then commit it under a short write lock.
fn refresh_mods_shared(state: &SharedState) {
    let Some(game) = selected_game(state) else {
        let mut st = write_state(state);
        st.mods.clear();
        st.notifier.bump();
        return;
    };
    prune_updates(state);
    if let Some(lib) = library_of(&game) {
        lib.tidy_names();
        let moved = lib.fix_redmod_targets();
        if !moved.is_empty() {
            log(state, "INFO", format!("{} is a REDmod, so it now goes into the game's mods folder. Deploy to apply it.", moved.join(", ")));
        }
    }
    let list = ModManager::list_mods(&game);
    let first = read_state(state).config.library_first.contains(&game.appid);
    let mut lib = library_status(&game, first);
    if lib.is_object() {
        lib["pending_pack"] = pending_pack_json(state, &game);
    }
    let mut st = write_state(state);
    if !game.is_installed {
        st.health.remove(&game.appid);
    }
    let applied = st.apply_mods(&game.appid, list);
    if applied || st.games.get(st.selected_idx).map(|g| &g.appid) == Some(&game.appid) {
        st.library = lib;
    }
}

/// Forget listed updates whose mod was updated, replaced or removed since.
fn prune_updates(state: &SharedState) {
    prune_thunder_updates(state);
    prune_github_updates(state);
    prune_modio_updates(state);
    prune_nexus_needs(state);
    let (rows, games) = {
        let st = read_state(state);
        if st.nexus.updates.is_empty() {
            return;
        }
        (st.nexus.updates.clone(), st.games.clone())
    };
    let keep: Vec<UpdateRow> = rows
        .iter()
        .filter(|r| {
            games
                .iter()
                .find(|g| g.appid == r.appid)
                .and_then(library_of)
                .map(|l| l.mods().iter().any(|m| m.id == r.lib_id && m.nexus.as_ref() == Some(&r.nexus)))
                .unwrap_or(false)
        })
        .cloned()
        .collect();
    if keep.len() != rows.len() {
        let mut st = write_state(state);
        st.nexus.updates = keep;
        st.notifier.bump();
    }
}

/// Forget requirements for mods that have been removed, disabled, or whose requirements are satisfied.
fn prune_nexus_needs(state: &SharedState) {
    let (rows, games) = {
        let st = read_state(state);
        if st.nexus.needs.is_empty() {
            return;
        }
        (st.nexus.needs.clone(), st.games.clone())
    };
    let orig_len = rows.len();
    let keep: Vec<NeedRow> = rows
        .into_iter()
        .filter(|r| {
            games
                .iter()
                .find(|g| g.appid == r.appid && g.is_installed)
                .and_then(library_of)
                .map(|l| l.mods().iter().any(|m| m.id == r.lib_id && m.enabled))
                .unwrap_or(false)
        })
        .collect();
    if keep.len() != orig_len {
        let mut st = write_state(state);
        st.nexus.needs = keep;
        st.notifier.bump();
    }
}

/// Forget Thunderstore updates whose package was updated or removed since.
fn prune_thunder_updates(state: &SharedState) {
    let (rows, games) = {
        let st = read_state(state);
        if st.thunder.updates.is_empty() {
            return;
        }
        (st.thunder.updates.clone(), st.games.clone())
    };
    let keep: Vec<ThunderUpdate> = rows
        .iter()
        .filter(|r| {
            games
                .iter()
                .find(|g| g.appid == r.appid)
                .and_then(library_of)
                .map(|l| l.mods().iter().any(|m| m.id == r.lib_id && m.thunderstore.as_ref() == Some(&r.pkg)))
                .unwrap_or(false)
        })
        .cloned()
        .collect();
    if keep.len() != rows.len() {
        let mut st = write_state(state);
        st.thunder.updates = keep;
        st.notifier.bump();
    }
}

/// Forget GitHub updates whose mod was updated or removed since.
fn prune_github_updates(state: &SharedState) {
    let (rows, games) = {
        let st = read_state(state);
        if st.github_updates.is_empty() {
            return;
        }
        (st.github_updates.clone(), st.games.clone())
    };
    let keep: Vec<GithubUpdate> = rows
        .iter()
        .filter(|r| {
            games.iter().find(|g| g.appid == r.appid).and_then(library_of).map(|l| l.mods().iter().any(|m| m.id == r.lib_id && m.github.as_ref().map(|g| g.tag.trim_start_matches('v') == r.installed_version).unwrap_or(false))).unwrap_or(false)
        })
        .cloned()
        .collect();
    if keep.len() != rows.len() {
        let mut st = write_state(state);
        st.github_updates = keep;
        st.notifier.bump();
    }
}

/// Data folder used when a game's own folder cannot hold the mod library.
fn library_fallback_dir() -> PathBuf {
    platform::data_dir()
}

fn library_of(g: &GameEntry) -> Option<deploy::Library> {
    (g.is_installed && !g.game_dir.is_empty()).then(|| {
        let rules = recipes::rules_for(&g.appid, &g.steam_appid).into_iter().map(|r| (r.ext, r.folder)).collect();
        deploy::Library::locate(Path::new(&g.game_dir), &library_fallback_dir(), &g.appid).with_rules(rules)
    })
}

/// How many save folders Gantry can see for a game (looked up at most every 30 seconds).
fn save_folder_count(g: &GameEntry) -> usize {
    static CACHE: std::sync::LazyLock<Mutex<std::collections::HashMap<String, (Instant, usize)>>> = std::sync::LazyLock::new(|| Mutex::new(std::collections::HashMap::new()));
    let key = format!("{}|{}|{}", g.appid, g.game_dir, g.pfx);
    if let Some((at, n)) = CACHE.lock().unwrap_or_else(|e| e.into_inner()).get(&key) {
        if at.elapsed() < Duration::from_secs(30) {
            return *n;
        }
    }
    let custom = recipes::rules_saves(&g.appid, &g.steam_appid);
    let n = profsaves::roots(g, &custom).len();
    CACHE.lock().unwrap_or_else(|e| e.into_inner()).insert(key, (Instant::now(), n));
    n
}

/// The Steam screenshots of a game, each told apart by the session (with or without mods) it was taken in.
fn shots_for(state: &SharedState, g: &GameEntry) -> Vec<screenshots::Shot> {
    let (roots, sessions_file) = {
        let st = read_state(state);
        (st.steam_lib.roots.clone().unwrap_or_else(RigPaths::steam_roots), st.paths.config_file.with_file_name("sessions.jsonl"))
    };
    let id = g.steam_id();
    if id.is_empty() {
        return Vec::new();
    }
    let sessions: Vec<stats::Session> = stats::read(&sessions_file).into_iter().filter(|s| s.appid == g.appid).collect();
    screenshots::list(&roots, id, &sessions)
}

/// How many screenshots the game has (looked up at most every 30 seconds).
fn screenshot_count(g: &GameEntry) -> usize {
    if g.steam_id().is_empty() {
        return 0;
    }
    static CACHE: std::sync::LazyLock<Mutex<std::collections::HashMap<String, (Instant, usize)>>> = std::sync::LazyLock::new(|| Mutex::new(std::collections::HashMap::new()));
    let key = format!("{}|{}", g.steam_id(), g.game_dir);
    if let Some((at, n)) = CACHE.lock().unwrap_or_else(|e| e.into_inner()).get(&key) {
        if at.elapsed() < Duration::from_secs(30) {
            return *n;
        }
    }
    let n = screenshots::list(&RigPaths::steam_roots(), g.steam_id(), &[]).len();
    CACHE.lock().unwrap_or_else(|e| e.into_inner()).insert(key, (Instant::now(), n));
    n
}

/// How many Workshop items the game has installed (looked up at most every 30 seconds).
fn workshop_count(g: &GameEntry) -> usize {
    if g.steam_id().is_empty() || g.game_dir.is_empty() {
        return 0;
    }
    static CACHE: std::sync::LazyLock<Mutex<std::collections::HashMap<String, (Instant, usize)>>> = std::sync::LazyLock::new(|| Mutex::new(std::collections::HashMap::new()));
    let key = format!("{}|{}", g.steam_id(), g.game_dir);
    if let Some((at, n)) = CACHE.lock().unwrap_or_else(|e| e.into_inner()).get(&key) {
        if at.elapsed() < Duration::from_secs(30) {
            return *n;
        }
    }
    let n = workshop::installed(Path::new(&g.game_dir), g.steam_id()).len();
    CACHE.lock().unwrap_or_else(|e| e.into_inner()).insert(key, (Instant::now(), n));
    n
}

/// A card the person set aside, cleaned: only the fields the store shows, short, and only web pictures.
fn saved_card(body: &Value) -> Option<Value> {
    let text = |k: &str, max: usize| body.get(k).and_then(Value::as_str).unwrap_or("").chars().filter(|c| !c.is_control()).take(max).collect::<String>();
    let num = |k: &str| body.get(k).and_then(Value::as_u64).unwrap_or(0);
    let (source, id) = (text("source", 20), text("id", 120));
    let id_ok = match source.as_str() {
        "nexus" => !id.is_empty() && id.len() <= 12 && id.bytes().all(|b| b.is_ascii_digit()),
        "thunderstore" => !id.is_empty() && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_' || b == b'.'),
        _ => false,
    };
    let name = text("name", 120);
    if !id_ok || name.is_empty() {
        return None;
    }
    let picture = text("picture", 400);
    let page = text("page_url", 400);
    let page_ok = (source == "nexus" && page.starts_with("https://www.nexusmods.com/")) || (source == "thunderstore" && page.starts_with("https://thunderstore.io/"));
    Some(json!({
        "source": source, "id": id, "name": name, "summary": text("summary", 300), "author": text("author", 80), "version": text("version", 40),
        "downloads": num("downloads"), "endorsements": num("endorsements"), "updated_ms": num("updated_ms"), "size_kb": num("size_kb"),
        "picture": if picture.starts_with("https://") { picture } else { String::new() }, "category": text("category", 60),
        "page_url": if page_ok && !page.contains(char::is_whitespace) { page } else { String::new() },
    }))
}

/// The "Saved" view of the store: what the person set aside for this game, with what is in the library marked.
fn saved_page(state: &SharedState, g: &GameEntry) -> Value {
    let (have_nexus, have_thunder): (HashSet<u64>, HashSet<String>) = library_of(g)
        .map(|l| {
            let mods = l.mods();
            (mods.iter().filter_map(|m| m.nexus.as_ref().map(|n| n.mod_id)).collect(), mods.iter().filter_map(|m| m.thunderstore.as_ref().map(|t| t.full_name().to_lowercase())).collect())
        })
        .unwrap_or_default();
    let cards: Vec<Value> = read_state(state)
        .config
        .saved_mods
        .get(&g.appid)
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .map(|mut c| {
            let installed = match c["source"].as_str() {
                Some("nexus") => c["id"].as_str().and_then(|i| i.parse::<u64>().ok()).map(|i| have_nexus.contains(&i)).unwrap_or(false),
                _ => c["id"].as_str().map(|i| have_thunder.contains(&i.to_lowercase())).unwrap_or(false),
            };
            c["installed"] = json!(installed);
            c
        })
        .collect();
    json!({ "cards": cards, "next": { "n": 0, "t": 0 }, "more": false, "categories": [], "notes": [], "premium": read_state(state).nexus.user.as_ref().map(|u| u.premium).unwrap_or(false), "adult": false, "saved_view": true })
}

/// The folder a quick button opens: the game's own (the default), where its saves are, its Wine prefix, or Gantry's cache
/// (logs, backups). Only folders that exist; anything else gives nothing.
fn folder_target(g: &GameEntry, which: &str, cache_root: &Path, save_places: &[String]) -> Option<PathBuf> {
    let p = match which {
        "saves" => profsaves::roots(g, save_places).first().map(|r| r.live.clone()),
        "prefix" if !g.pfx.is_empty() => Some(PathBuf::from(&g.pfx)),
        "logs" => Some(cache_root.to_path_buf()),
        "" | "game" => Some(PathBuf::from(&g.game_dir)),
        _ => None,
    };
    p.filter(|p| p.is_dir())
}

/// The home page's shelf: for the games played last, the mods people download most among those changed lately, leaving
/// out what is already in the library. Kept for ten minutes (the sites are asked for up to three games).
fn trending_for_home(state: &SharedState, refresh: bool) -> Value {
    const KEEP: Duration = Duration::from_secs(600);
    if !refresh {
        if let Some((at, v)) = &read_state(state).home_trending {
            if at.elapsed() < KEEP {
                return v.clone();
            }
        }
    }
    let games: Vec<GameEntry> = {
        let st = read_state(state);
        let lp = st.last_played_json();
        let mut played: Vec<(u64, GameEntry)> = st.games.iter().filter(|g| g.is_installed && !g.game_dir.is_empty()).filter_map(|g| Some((lp.get(&g.appid)?.as_u64()?, g.clone()))).collect();
        played.sort_by_key(|(t, _)| std::cmp::Reverse(*t));
        played.into_iter().take(3).map(|(_, g)| g).collect()
    };
    let mut shelves: Vec<Value> = Vec::new();
    for g in games {
        let ask = StoreAsk { source: "both".into(), sort: "lately".into(), query: String::new(), category: String::new(), off_nexus: 0, off_thunder: 0, off_modio: 0, per: 8, hide_libs: true };
        let page = store_page(state, &g, &ask);
        let cards: Vec<Value> = page["cards"].as_array().cloned().unwrap_or_default().into_iter().filter(|c| c["installed"] != true).take(4).collect();
        if !cards.is_empty() {
            shelves.push(json!({ "appid": g.appid, "name": g.name, "cards": cards }));
        }
    }
    let v = json!({ "games": shelves });
    // an empty shelf (no key yet, no connection) is not worth remembering
    write_state(state).home_trending = (!shelves.is_empty()).then(|| (Instant::now(), v.clone()));
    v
}

/// Status the UI shows for the mod library; an empty library when the game has none yet.
/// Per mod: the core mods its files need that the game does not have, with where to get each.
fn core_needs_json(g: &GameEntry, lib: &deploy::Library) -> Value {
    let dir = PathBuf::from(&g.game_dir);
    let mods = lib.mods();
    // most games need nothing: one look for all mods first
    if frameworks::missing(&dir, &files_in_game(&mods)).is_empty() {
        return json!({});
    }
    let out: serde_json::Map<String, Value> = mods
        .iter()
        .filter(|m| m.enabled)
        .filter_map(|m| {
            let fw = frameworks::missing(&dir, &files_in_game(std::slice::from_ref(m)));
            (!fw.is_empty()).then(|| (m.id.clone(), json!(fw.iter().map(|x| json!({ "name": x.name, "url": x.page })).collect::<Vec<_>>())))
        })
        .collect();
    Value::Object(out)
}

fn library_status(g: &GameEntry, first: bool) -> Value {
    match library_of(g) {
        Some(lib) => {
            let mut v = lib.status_json();
            v["unreal"] = json!(g.engine.starts_with("Unreal"));
            v["first"] = json!(first);
            v["separate_saves"] = json!(lib.separate_saves());
            v["saves_of"] = json!(lib.saves_of());
            v["save_folders"] = json!(save_folder_count(g));
            v["workshop"] = json!(workshop_count(g));
            v["screenshots"] = json!(screenshot_count(g));
            // Steam syncs the copy it installed itself, not one that came from another launcher
            v["steam_cloud"] = json!(steamlib::manifest_for(Path::new(&g.game_dir), g.steam_id()).is_some() && steamlib::cloud_synced(&RigPaths::steam_roots(), g.steam_id()));
            v["plugins"] = json!(has_plugins(g));
            v["core_needs"] = core_needs_json(g, &lib);
            // the mods everything else stands on: the game's core mods and the mod loaders
            let mut core_names: Vec<String> = frameworks::cores(Path::new(&g.game_dir)).iter().map(|c| c.name.to_lowercase()).collect();
            core_names.extend(["bepinex", "melonloader", "ue4ss"].map(String::from));
            v["core_ids"] = json!(lib.mods().iter().filter(|m| core_names.iter().any(|c| m.name.to_lowercase().contains(c.as_str()))).map(|m| m.id.clone()).collect::<Vec<_>>());
            v
        }
        None => Value::Null,
    }
}

/// `WINEDLLOVERRIDES` value for `base` plus the proxy DLLs the mod library has deployed.
fn merge_overrides(base: Option<String>, extra: &[String]) -> Option<String> {
    if extra.is_empty() {
        return base;
    }
    let e = format!("{}=n,b", extra.join(","));
    Some(match base {
        Some(b) => format!("{b};{e}"),
        None => e,
    })
}

fn library_overrides(g: &GameEntry) -> Vec<String> {
    library_of(g).map(|l| l.wine_overrides()).unwrap_or_default()
}

// ───────────────────────────── process helpers ─────────────────────────────

/// Spawn and reap in the background so fire-and-forget children never become zombies.
fn spawn_reaped(mut cmd: Command) -> bool {
    cmd.stdin(Stdio::null());
    match cmd.spawn() {
        Ok(mut child) => {
            thread::spawn(move || {
                let _ = child.wait();
            });
            true
        }
        Err(_) => false,
    }
}

fn xdg_open(target: impl AsRef<std::ffi::OsStr>) -> bool {
    if cfg!(test) {
        return true; // tests never start Steam or a browser
    }
    platform::open(target)
}

fn run_quiet(cmd: &mut Command) -> bool {
    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// The compatibility folder and Wine prefix a game runs in.
fn compat_and_pfx(g: &GameEntry) -> (PathBuf, String) {
    let compat = if g.compatdata.is_empty() {
        home_dir().join(".local/share/Steam/steamapps/compatdata/protonrig_custom")
    } else {
        PathBuf::from(&g.compatdata)
    };
    let pfx = if platform::IS_WINDOWS {
        String::new()
    } else if g.pfx.is_empty() {
        format!("{}/pfx", compat.to_string_lossy())
    } else {
        g.pfx.clone()
    };
    (compat, pfx)
}

/// The Proton the game itself runs with: for a Steam game what Steam is set to use for it (a trainer started with
/// another build cannot see the game), otherwise Gantry's own choice.
fn game_proton(state: &SharedState, g: &GameEntry) -> Option<PathBuf> {
    if !g.launches_directly() {
        let env = read_state(state).steam_env.clone();
        let name = env.config_vdf().and_then(|p| fs::read_to_string(p).ok()).and_then(|t| steam::get_compat_tool(&t, &g.appid).ok().flatten());
        if let Some(name) = name {
            if let Some(t) = all_tools(state).into_iter().find(|t| t.internal == name) {
                return Some(t.script);
            }
        }
    }
    read_state(state).proton_for(g)
}

/// Starts a trainer for a game that is running (or about to) and says in the activity log what happened.
/// What came of starting a trainer. `state` is "starting", "attached" (seen running in the game's Wine prefix),
/// "elsewhere" (running, but the game runs in another prefix), "closed" (it ended; `secs` after the start) or
/// "unseen" (started, but Gantry could not see it).
#[derive(Clone, Debug)]
pub struct TrainerRun {
    pub appid: String,
    pub file: String,
    pub state: &'static str,
    /// The trainer's own process once seen (0 before).
    pub pid: u32,
    pub secs: u64,
}

fn set_trainer_run(state: &SharedState, g: &GameEntry, file: &str, run_state: &'static str, pid: u32, secs: u64) {
    let mut st = write_state(state);
    st.trainer_run = Some(TrainerRun { appid: g.appid.clone(), file: file.to_string(), state: run_state, pid, secs });
    st.notifier.bump();
}

/// The game's own executable, for its version.
fn game_exe_file(g: &GameEntry) -> Option<PathBuf> {
    [&g.shipping_exe, &g.exe_path].into_iter().filter(|p| !p.is_empty()).map(|p| {
        let p = PathBuf::from(p);
        if p.is_absolute() { p } else { Path::new(&g.game_dir).join(p) }
    }).find(|p| p.is_file())
}

/// The product version the game's executable declares, read once per file version on disk.
fn game_version(g: &GameEntry) -> Option<Vec<u32>> {
    type Seen = std::collections::HashMap<PathBuf, (u64, Option<std::time::SystemTime>, Option<Vec<u32>>)>;
    static SEEN: std::sync::LazyLock<Mutex<Seen>> = std::sync::LazyLock::new(Default::default);
    let exe = game_exe_file(g)?;
    let meta = fs::metadata(&exe).ok()?;
    let key = (meta.len(), meta.modified().ok());
    if let Some((len, at, v)) = SEEN.lock().unwrap_or_else(|e| e.into_inner()).get(&exe) {
        if (*len, *at) == key {
            return v.clone();
        }
    }
    let v = fs::read(&exe).ok().and_then(|d| trainer::product_version(&d));
    SEEN.lock().unwrap_or_else(|e| e.into_inner()).insert(exe, (key.0, key.1, v.clone()));
    v
}

/// A warning when the trainer's name says it is made for other game versions than the one installed.
fn trainer_version_warning(state: &SharedState, g: &GameEntry, file: &str) {
    let Some(range) = trainer::made_for(file) else { return };
    let Some(have) = game_version(g) else { return };
    let span = if range.0 == range.1 { trainer::version_text(&range.0) } else { format!("{}–{}", trainer::version_text(&range.0), trainer::version_text(&range.1)) };
    match trainer::fits(&have, &range) {
        Some(std::cmp::Ordering::Greater) => log(state, "WARN", format!("This trainer is made for {} {}, and the game is {} now. Its cheats may not work or may crash the game; look for a newer trainer.", g.name, span, trainer::version_text(&have))),
        Some(std::cmp::Ordering::Less) => log(state, "WARN", format!("This trainer is made for {} {}, and the game is the older {}. Its cheats may not work or may crash the game.", g.name, span, trainer::version_text(&have))),
        _ => {}
    }
}

/// How long the trainer gets to show up in the game's Wine session.
fn attach_wait() -> Duration {
    Duration::from_millis(if cfg!(test) { 2500 } else { 12_000 })
}

/// After the start: is the trainer really running, and in the game's Wine session? Says so in the log either way.
fn watch_trainer(state: &SharedState, g: &GameEntry, trainer: &Path, started: &trainer::Started, log_file: &Path) {
    let file = trainer.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
    let needle = file.to_lowercase();
    let t0 = Instant::now();
    set_trainer_run(state, g, &file, "starting", 0, 0);
    let direct = started.how == "directly";
    let ended_for = || started.ended.lock().unwrap_or_else(|e| e.into_inner()).as_ref().map(|(at, status)| (at.duration_since(t0), status.clone()));
    let seen = loop {
        // the process Gantry started is the trainer itself only on Windows; elsewhere it is Proton or Wine starting it
        let pids: Vec<u32> = procs::pids_with_cmdline(&needle).into_iter().filter(|&p| direct || p != started.pid).collect();
        if let Some(&pid) = pids.first() {
            break Some((pid, pids));
        }
        let gone = ended_for().is_some_and(|(at, _)| t0.elapsed().saturating_sub(at) > Duration::from_millis(if cfg!(test) { 300 } else { 2000 }));
        if gone || t0.elapsed() > attach_wait() {
            break None;
        }
        thread::sleep(Duration::from_millis(if cfg!(test) { 50 } else { 400 }));
    };
    let secs = t0.elapsed().as_secs();
    match seen {
        Some((pid, pids)) => {
            let trainer_pfx = pids.iter().find_map(|&p| procs::wine_prefix_of(p));
            let game_pfx = procs::game_pids(g).into_iter().find_map(procs::wine_prefix_of);
            match (trainer_pfx, game_pfx) {
                (Some(t), Some(gp)) if t != gp => {
                    set_trainer_run(state, g, &file, "elsewhere", pid, secs);
                    log(state, "WARN", format!("The trainer runs, but in another Wine prefix than the game ({} instead of {}), so it cannot reach the game. Start the game through ModForge or Steam, then Run Trainer again.", t, gp));
                }
                _ => {
                    set_trainer_run(state, g, &file, "attached", pid, secs);
                    if platform::IS_WINDOWS || procs::wine_prefix_of(pid).is_none() {
                        log(state, "OK", "The trainer is running. Use its hotkeys inside the game.");
                    } else {
                        log(state, "OK", format!("The trainer is running in {}'s Wine session. Use its hotkeys inside the game.", g.name));
                    }
                }
            }
        }
        None => match ended_for() {
            Some((at, status)) => {
                set_trainer_run(state, g, &file, "closed", 0, at.as_secs());
                let said = trainer::tail(log_file, 3);
                if said.is_empty() {
                    log(state, "WARN", format!("The trainer closed {} s after it started ({}).", at.as_secs(), status));
                } else {
                    log(state, "WARN", format!("The trainer closed {} s after it started ({}). It said: {}", at.as_secs(), status, said));
                }
                trainer_cause_hint(state, g, log_file);
            }
            None => {
                set_trainer_run(state, g, &file, "unseen", 0, secs);
                log(state, "INFO", "The trainer was started, but ModForge cannot see it among the game's processes yet. If its window does not show up, see the trainer log under Logs.");
            }
        },
    }
}

fn trainer_cause_hint(state: &SharedState, g: &GameEntry, log_file: &Path) {
    let app_id = compat_app_id(g);
    let said = fs::read_to_string(log_file).unwrap_or_default();
    match trainer::cause_of(&said) {
        Some(trainer::Cause::CppRuntime) => log(state, "WARN", format!("The trainer needs the Visual C++ runtime in the game's prefix. Install it with protontricks: protontricks {} vcrun2022", app_id)),
        Some(trainer::Cause::DotNet) => log(state, "WARN", format!("The trainer needs .NET in the game's prefix. Install it with protontricks: protontricks {} dotnet48", app_id)),
        Some(trainer::Cause::OtherBuild) => log(state, "WARN", "The game and the trainer run with different compatibility tools. Pick the same one for both under Compatibility."),
        None => {}
    }
}

fn start_trainer(state: &SharedState, g: &GameEntry, trainer: &Path) -> bool {
    if !trainer.is_file() {
        log(state, "ERR", format!("The trainer file is missing: {}", trainer.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default()));
        return false;
    }
    let (compat, pfx) = compat_and_pfx(g);
    let proton = game_proton(state, g);
    let app_id = compat_app_id(g);
    let steam_root = home_dir().join(".local/share/Steam");
    let log_file = trainer::log_path(&read_state(state).cache_root());
    let launch = trainer::Launch { trainer, pfx: &pfx, compat: &compat, proton: proton.as_deref(), steam_root: &steam_root, app_id: &app_id, env_lines: &g.env_vars, log_file: &log_file };
    let file = trainer.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
    trainer_version_warning(state, g, &file);
    match trainer::start(&launch, Duration::from_secs(if cfg!(test) { 1 } else { 6 })) {
        Ok(started) => {
            log(state, "OK", "Trainer started. Checking that it runs next to the game…");
            watch_trainer(state, g, trainer, &started, &log_file);
            true
        }
        Err(why) => {
            log(state, "ERR", format!("The trainer did not start: {}", why));
            set_trainer_run(state, g, &file, "closed", 0, 0);
            trainer_cause_hint(state, g, &log_file);
            false
        }
    }
}

fn wineserver_for(proton_script: Option<&Path>) -> PathBuf {
    let wine = RigEngine::resolve_proton_wine_bin(proton_script);
    let ws = wine.with_file_name("wineserver");
    if ws.is_file() {
        ws
    } else {
        PathBuf::from("wineserver")
    }
}

/// Terminate the running game: process group, the prefix's wineserver, and any leftover
/// process carrying this game's shipping executable name.
fn stop_game_processes(game: Option<&GameEntry>, pid: u32, proton: Option<&Path>) {
    fsutil::kill_process_tree(pid);
    if let Some(g) = game {
        if platform::IS_WINDOWS {
            let exe_name = Path::new(&g.shipping_exe).file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
            if !exe_name.is_empty() {
                let _ = run_quiet(Command::new("taskkill").args(["/F", "/T", "/IM", &exe_name]));
            }
            return;
        }
        if !g.pfx.is_empty() {
            let _ = run_quiet(Command::new(wineserver_for(proton)).arg("-k").env("WINEPREFIX", &g.pfx));
        }
        let exe_name = Path::new(&g.shipping_exe)
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();
        if !exe_name.is_empty() {
            let pat = regex::escape(&exe_name);
            let _ = run_quiet(Command::new("pkill").args(["-9", "-i", "-f", &pat]));
        }
    }
}

/// Room an archive needs once unpacked (twice its size, plus a margin, is a safe guess for game files) before it is
/// unpacked, so a nearly full drive is told so instead of being filled to the brim.
fn ensure_room(archive: &Path, dest_dir: &Path) -> Result<(), String> {
    let size = fs::metadata(archive).map(|m| m.len()).unwrap_or(0);
    let need = size.saturating_mul(2).saturating_add(256 << 20);
    match fsutil::free_bytes(dest_dir) {
        Some(free) if free < need => Err(format!(
            "Not enough free space for {}: it needs about {} and only {} is free there. Free some space and try again.",
            archive.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default(),
            fsutil::size_text(need),
            fsutil::size_text(free)
        )),
        _ => Ok(()),
    }
}

fn extract_archive(archive_path: &Path, dest_dir: &Path) -> bool {
    // 7z handles zip/7z/rar; bsdtar (tar.exe on Windows) handles zip/7z/rar/tar.*;
    // unzip / unrar are last-resort fallbacks.
    platform::extract_commands(archive_path, dest_dir).iter_mut().any(run_quiet)
}

// ───────────────────────────── install pipeline ─────────────────────────────

fn sanitize_mod_folder_name(raw: &str) -> String {
    let cleaned: String = raw
        .chars()
        .filter(|c| c.is_alphanumeric() || *c == '_' || *c == '-')
        .collect();
    if cleaned.is_empty() {
        "CustomLuaMod".to_string()
    } else {
        cleaned
    }
}

/// UI-ish PAKs (`HSTM_UI.pak`, `Logic_Foo.pak`) belong in LogicMods; match whole name tokens
/// so "Quick_Build.pak" or "Suit.pak" are not misrouted by a substring hit.
fn pak_goes_to_logic_mods(path: &Path) -> bool {
    if path.to_string_lossy().to_lowercase().contains("logicmods") {
        return true;
    }
    let stem = path.file_stem().map(|s| s.to_string_lossy().to_lowercase()).unwrap_or_default();
    stem.split(|c: char| !c.is_alphanumeric()).any(|t| t == "ui" || t == "logic")
}

/// Removes a directory tree when dropped (staging areas, drop buffers) even on early return.
struct DirGuard(PathBuf);
impl Drop for DirGuard {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// Staging directory on the *same partition* as the game when possible so moving staged
/// files into place is an instant atomic `rename(2)`; falls back to the cache partition.
fn make_stage_dir(gdir: &Path, cache_root: &Path) -> Option<DirGuard> {
    let token = fsutil::unique_token();
    let on_game = gdir.join(format!(".rig_stage_{}", token));
    if gdir.is_dir() && fs::create_dir_all(&on_game).is_ok() {
        return Some(DirGuard(on_game));
    }
    let fallback = cache_root.join("extract").join(&token);
    fs::create_dir_all(&fallback).ok()?;
    Some(DirGuard(fallback))
}

fn trainers_dir(cache_dir: &Path) -> PathBuf {
    cache_dir.parent().unwrap_or(cache_dir).join("trainers")
}

/// The trainers kept for a game: the ones attached before (still on disk) and the copies in Gantry's trainer folder.
fn trainer_shelf(st: &AppState, appid: &str) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = st.config.trainers.get(appid).into_iter().chain(st.config.trainer_shelf.get(appid).into_iter().flatten()).map(PathBuf::from).collect();
    let prefix = format!("{}_", appid);
    let mut kept: Vec<PathBuf> = fs::read_dir(trainers_dir(&st.paths.cache_dir))
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.file_name().map(|n| n.to_string_lossy()).is_some_and(|n| n.starts_with(&prefix) && n.to_lowercase().ends_with(".exe")))
        .collect();
    kept.sort();
    out.extend(kept);
    let mut seen = HashSet::new();
    out.retain(|p| p.is_file() && seen.insert(p.clone()));
    out
}

/// The name a kept trainer is shown with (Gantry's copies carry the game's id in front).
fn trainer_display(appid: &str, p: &Path) -> String {
    let n = p.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
    n.strip_prefix(&format!("{}_", appid)).map(str::to_string).unwrap_or(n)
}

fn attach_trainer(state: &SharedState, appid: &str, trainer: &Path, display: &str) {
    let mut st = write_state(state);
    st.config.trainers.insert(appid.to_string(), trainer.to_string_lossy().to_string());
    let shelf = st.config.trainer_shelf.entry(appid.to_string()).or_default();
    let path = trainer.to_string_lossy().to_string();
    if !shelf.contains(&path) {
        shelf.push(path);
    }
    st.save_config();
    if let Some(g) = st.games.iter_mut().find(|g| g.appid == appid) {
        g.trainer_name = display.to_string();
    }
    st.log("OK", format!("Attached persistent trainer: {}", display));
    let low = display.to_lowercase();
    if ["setup", "installer", "install "].iter().any(|w| low.contains(w)) {
        st.log("WARN", format!("{} looks like an installer, not a trainer. Running it would install a program inside the game's prefix.", display));
    }
    // reading the game's executable takes a moment, so not under the lock
    if let Some(g) = st.games.iter().find(|g| g.appid == appid).cloned() {
        drop(st);
        let (state, display) = (Arc::clone(state), display.to_string());
        thread::spawn(move || trainer_version_warning(&state, &g, &display));
    }
}

/// Detect a full UE4SS runtime inside an extracted archive and install it into `ue_bin`.
/// The payload is consumed (removed from the stage) so later passes don't re-handle it.
fn install_ue4ss_payload(stage: &Path, ue_bin: &Path) -> Option<usize> {
    let dll = WalkDir::new(stage)
        .into_iter()
        .flatten()
        .find(|e| e.file_type().is_file() && e.file_name().to_string_lossy().eq_ignore_ascii_case("ue4ss.dll"))?;
    let mut root = dll.path().parent()?.to_path_buf();
    let in_nested = root.file_name().map(|n| n.to_string_lossy().eq_ignore_ascii_case("ue4ss")).unwrap_or(false);
    if in_nested {
        if let Some(up) = root.parent() {
            if up.join("dwmapi.dll").is_file() || up != stage {
                root = up.to_path_buf();
            }
        }
    }
    let n = fsutil::copy_dir_all(&root, ue_bin).ok()?;
    if root == stage {
        if let Ok(rd) = fs::read_dir(stage) {
            for e in rd.flatten() {
                let p = e.path();
                let _ = if p.is_dir() { fs::remove_dir_all(&p) } else { fs::remove_file(&p) };
            }
        }
    } else {
        let _ = fs::remove_dir_all(&root);
    }
    Some(n)
}

fn install_dir(src: &Path, dest: &Path) -> std::io::Result<()> {
    if dest.exists() {
        fsutil::copy_dir_all(src, dest)?;
        fs::remove_dir_all(src)
    } else {
        fsutil::move_dir(src, dest)
    }
}

fn install_mod_or_trainer_from_path(p: PathBuf, is_temp_drop: bool, state: SharedState) {
    install_routed(p, is_temp_drop, state, InstallOpts::default());
}

/// Install into the mod library even for Unreal games (which otherwise use the UE4SS/PAK routing).
fn install_to_library_from_path(p: PathBuf, is_temp_drop: bool, state: SharedState) {
    install_routed(p, is_temp_drop, state, InstallOpts { to_library: true, ..Default::default() });
}

/// What the caller knows about a file being installed.
#[derive(Default, Clone)]
struct InstallOpts {
    /// Use the mod library even for Unreal games.
    to_library: bool,
    /// Install for this game instead of the selected one (updates run in the background).
    game: Option<GameEntry>,
    /// Where the file came from on Nexus Mods.
    nexus: Option<nexus::NexusRef>,
    version: String,
    /// Library mod this file is a newer version of.
    replaces: Option<String>,
    /// Unreal game with "install new mods here": hand mods that need UE4SS back to the usual routing.
    ue_guard: bool,
    /// The download was checked against the site's checksum ("md5").
    verified: String,
    /// Where the file came from on Thunderstore; also names the mod.
    thunder: Option<thunderstore::ThunderRef>,
    /// Where the file came from on GitHub.
    github: Option<github::GithubRef>,
    /// Where the file came from on mod.io.
    modio: Option<modio::ModioRef>,
    name: Option<String>,
    /// Thunderstore packages the mod needs (`Owner-Name`).
    depends: Vec<String>,
    /// The answers given to the mod's installer script, kept with the mod.
    fomod: Option<Value>,
    /// Install into this folder of the game (instead of guessing).
    target: Option<String>,
}

struct InstallJob {
    path: PathBuf,
    temp: bool,
    opts: InstallOpts,
}

/// Several mods added at once (dropped together, "Add all" from Downloads): installed one after another, with one
/// progress line and one summary. A single mod is a batch of one and shows nothing extra.
#[derive(Clone, Debug, Default, serde::Serialize)]
pub struct Batch {
    pub total: usize,
    pub done: usize,
    pub added: usize,
    /// Name and the reason, for each one that could not be added.
    pub failed: Vec<(String, String)>,
    /// Waiting for an answer (where its files go, or its installer's choices).
    pub waiting: usize,
    pub current: String,
    pub finished: bool,
}

/// Queues a file to be installed; the files are installed one at a time, in the order they came.
fn queue_install(state: &SharedState, path: PathBuf, temp: bool, opts: InstallOpts) {
    let start = {
        let mut st = write_state(state);
        if st.batch.as_ref().is_none_or(|b| b.finished) {
            st.batch = Some(Batch::default());
        }
        if let Some(b) = st.batch.as_mut() {
            b.total += 1;
        }
        st.install_jobs.push_back(InstallJob { path, temp, opts });
        st.notifier.bump();
        !std::mem::replace(&mut st.install_worker, true)
    };
    if start {
        let state = Arc::clone(state);
        thread::spawn(move || install_worker(&state));
    }
}

fn questions_waiting(st: &AppState) -> usize {
    st.pending_install.is_some() as usize + st.pending_queue.len() + st.pending_fomod.len()
}

fn install_worker(state: &SharedState) {
    loop {
        let (job, seq, asked) = {
            let mut st = write_state(state);
            let Some(job) = st.install_jobs.pop_front() else {
                st.install_worker = false;
                let summary = st.batch.as_mut().filter(|b| !b.finished).map(|b| {
                    b.finished = true;
                    b.current.clear();
                    b.clone()
                });
                st.notifier.bump();
                drop(st);
                if let Some(b) = summary.filter(|b| b.total > 1) {
                    let level = if b.failed.is_empty() { "OK" } else { "WARN" };
                    log(state, level, format!("Mods added: {} of {}.", b.added, b.total));
                    if b.waiting > 0 {
                        log(state, "INFO", format!("Waiting for your answer: {}.", b.waiting));
                    }
                    for (name, why) in &b.failed {
                        log(state, "WARN", format!("Not added: {} ({})", name, why));
                    }
                }
                return;
            };
            let name = job.path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
            if let Some(b) = st.batch.as_mut() {
                b.current = name;
            }
            st.notifier.bump();
            let (seq, asked) = (st.log_seq, questions_waiting(&st));
            (job, seq, asked)
        };
        let name = job.path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
        install_routed(job.path, job.temp, Arc::clone(state), job.opts);
        let mut st = write_state(state);
        // what happened to it: an error said while it was installing, a new question, or it went in
        let new = st.log_seq.saturating_sub(seq).min(st.logs.len() as u64) as usize;
        let err = st.logs.iter().rev().take(new).find(|l| l.level == "ERR").map(|l| l.message.clone());
        let now_asked = questions_waiting(&st);
        if let Some(b) = st.batch.as_mut() {
            b.done += 1;
            match err {
                Some(why) => b.failed.push((name, why)),
                None if now_asked > asked => b.waiting += 1,
                None => b.added += 1,
            }
        }
        st.notifier.bump();
    }
}

fn install_routed(p: PathBuf, is_temp_drop: bool, state: SharedState, opts: InstallOpts) {
    // The drop buffer lives in its own directory; remove it whatever happens below.
    let _drop_cleanup = if is_temp_drop { p.parent().map(|d| DirGuard(d.to_path_buf())) } else { None };

    let (game_opt, cache_dir) = {
        let st = read_state(&state);
        (opts.game.clone().or_else(|| st.games.get(st.selected_idx).cloned()), st.paths.cache_dir.clone())
    };
    let Some(g) = game_opt else {
        log(&state, "WARN", "No game selected; nothing was installed.");
        return;
    };

    let ext = p.extension().map(|e| e.to_string_lossy().to_lowercase()).unwrap_or_default();
    let fname = p.file_name().unwrap_or_default().to_string_lossy().to_string();
    let gdir = PathBuf::from(&g.game_dir);
    let ue_bin = RigEngine::find_ue_bin_dir(&gdir);

    // Every game except Unreal ones (UE4SS/PAK routing) uses the mod library; trainers stay trainers.
    // Unreal games use it when asked, or when the user chose "install new mods here": then archives
    // and PAKs go in, while anything that needs UE4SS (Lua, DLL hooks) keeps its own routing.
    let unreal = g.engine.starts_with("Unreal");
    let library_first = unreal && read_state(&state).config.library_first.contains(&g.appid) && !["dll", "lua"].contains(&ext.as_str());
    if ext != "exe" && g.is_installed && (opts.to_library || opts.replaces.is_some() || !unreal || library_first) {
        let guarded = InstallOpts { ue_guard: library_first && !opts.to_library && opts.replaces.is_none(), ..opts.clone() };
        if add_to_library(&state, &g, &p, is_temp_drop, &fname, &guarded) {
            return;
        }
        log(&state, "INFO", "This mod needs UE4SS, so it is installed the usual way.");
    }

    match ext.as_str() {
        "exe" => {
            let final_exe = if is_temp_drop {
                let dir = trainers_dir(&cache_dir);
                let perm = dir.join(format!("{}_{}", g.appid, fname));
                if let Err(e) = place_file(&p, &perm, true) {
                    log(&state, "ERR", format!("Could not store trainer '{}': {}", fname, e));
                    return;
                }
                perm
            } else {
                p.clone()
            };
            attach_trainer(&state, &g.appid, &final_exe, &fname);
        }
        "dll" => {
            let dest = ue_bin.clone().unwrap_or_else(|| gdir.clone());
            match place_file(&p, &dest.join(&fname), is_temp_drop) {
                Ok(()) => log(&state, "OK", format!("Installed DLL hook: {}", fname)),
                Err(e) => log(&state, "ERR", format!("Could not install '{}': {}", fname, e)),
            }
        }
        "pak" => {
            let sub = if pak_goes_to_logic_mods(&p) { "LogicMods" } else { "~mods" };
            let dest = ModManager::find_or_create_paks_dir(&gdir).join(sub);
            let mut res = place_file(&p, &dest.join(&fname), is_temp_drop);
            if res.is_ok() {
                for c_ext in ["utoc", "ucas", "sig"] {
                    let comp = p.with_extension(c_ext);
                    if comp.is_file() {
                        let name = comp.file_name().unwrap_or_default().to_os_string();
                        res = res.and(place_file(&comp, &dest.join(name), is_temp_drop));
                    }
                }
            }
            match res {
                Ok(()) => log(&state, "OK", format!("Routed PAK to Content/Paks/{}: {}", sub, fname)),
                Err(e) => log(&state, "ERR", format!("Could not install '{}': {}", fname, e)),
            }
        }
        _ => install_archive(&p, &fname, &g, &gdir, ue_bin.as_deref(), &cache_dir, &state),
    }

    RigEngine::invalidate_ue_bin_memo();
    refresh_mods_shared(&state);
}

/// The Thunderstore community of a game: chosen by hand, else matched by name.
fn thunder_community_for(state: &SharedState, g: &GameEntry) -> Result<String, String> {
    if let Some(c) = read_state(state).config.thunder_community.get(&g.appid) {
        return Ok(c.clone());
    }
    if let Some(c) = recipes::find(&read_state(state).recipes, &g.appid, g.recipe_steam_id()).map(|r| r.thunderstore.clone()).filter(|c| !c.is_empty()) {
        return Ok(c);
    }
    let (cache, client) = (read_state(state).cache_root().join("thunder"), thunder_client(state));
    let file = cache.join("communities.json");
    let fresh = fs::metadata(&file).and_then(|m| m.modified()).ok().and_then(|t| t.elapsed().ok()).map(|a| a < Duration::from_secs(86_400)).unwrap_or(false);
    let list: Vec<(String, String)> = if fresh {
        fs::read(&file).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
    } else {
        match client.communities() {
            Ok(l) => {
                let _ = fs::create_dir_all(&cache);
                let _ = fsutil::atomic_write(&file, serde_json::to_string(&l).unwrap_or_default());
                l
            }
            Err(e) => match fs::read(&file).ok().and_then(|b| serde_json::from_slice(&b).ok()) {
                Some(old) => old,
                None => return Err(e),
            },
        }
    };
    thunderstore::match_community(&g.name, &list).ok_or_else(|| format!("ModForge could not tell which Thunderstore community {} belongs to.", g.name))
}

/// All packages of a community, kept on disk for a few hours (the list is several MB).
fn thunder_listing(state: &SharedState, community: &str) -> Result<Vec<thunderstore::Listing>, String> {
    let (cache, client) = (read_state(state).cache_root().join("thunder"), thunder_client(state));
    let file = cache.join(format!("{community}.json"));
    let read = || fs::read(&file).ok().and_then(|b| serde_json::from_slice::<Vec<thunderstore::Listing>>(&b).ok());
    let fresh = fs::metadata(&file).and_then(|m| m.modified()).ok().and_then(|t| t.elapsed().ok()).map(|a| a < Duration::from_secs(6 * 3600)).unwrap_or(false);
    if fresh {
        if let Some(l) = read() {
            return Ok(l);
        }
    }
    match client.community_packages(community) {
        Ok(l) => {
            let _ = fs::create_dir_all(&cache);
            let _ = fsutil::atomic_write(&file, serde_json::to_string(&l).unwrap_or_default());
            Ok(l)
        }
        Err(e) => read().ok_or(e),
    }
}

// ───────────────────────────── the mod store ─────────────────────────────

struct StoreAsk {
    /// "both", "nexus" or "thunder"
    source: String,
    sort: String,
    query: String,
    category: String,
    off_nexus: usize,
    off_thunder: usize,
    off_modio: usize,
    per: usize,
    /// Leave out the libraries and tools of Thunderstore (BepInEx, mod managers...): they are needed, not browsed.
    hide_libs: bool,
}

/// Libraries and tools (BepInEx, mod managers...): packages other mods and the player need, not mods to browse.
fn is_support_package(l: &thunderstore::Listing) -> bool {
    l.categories.iter().any(|c| c.eq_ignore_ascii_case("libraries") || c.eq_ignore_ascii_case("tools"))
}

/// The starter pack of a game: the mods its support entry names, or (for a game without such a list) the
/// most downloaded ones that people also liked.
/// Games with anti-cheat get no store: the page says why instead.
fn store_blocked(g: &GameEntry) -> Option<Value> {
    let ac = deploy::anti_cheat(Path::new(&g.game_dir))?;
    Some(json!({ "cards": [], "next": { "n": 0, "t": 0 }, "more": false, "categories": [], "notes": [{ "source": "game", "error": format!("This game uses {}. Mods can get your account banned, so Gantry will not install any here.", ac) }], "premium": false, "adult": false }))
}

fn starter_page(state: &SharedState, g: &GameEntry) -> Value {
    if let Some(v) = store_blocked(g) {
        return v;
    }
    let picks: Vec<recipes::StarterPick> = recipes::find(&read_state(state).recipes, &g.appid, g.recipe_steam_id()).map(|r| r.starter.clone()).unwrap_or_default();
    let (have_nexus, have_thunder): (HashSet<u64>, HashSet<String>) = library_of(g)
        .map(|l| {
            let mods = l.mods();
            (mods.iter().filter_map(|m| m.nexus.as_ref().map(|n| n.mod_id)).collect(), mods.iter().filter_map(|m| m.thunderstore.as_ref().map(|t| t.full_name().to_lowercase())).collect())
        })
        .unwrap_or_default();
    let mut cards: Vec<Value> = Vec::new();
    let mut notes: Vec<Value> = Vec::new();
    let nexus = nexus_domain_for(state, g).and_then(|d| nexus_client(state).map(|c| (d, c)));
    let thunder = thunder_community_for(state, g).and_then(|c| thunder_listing(state, &c).map(|l| (c, l)));
    let nexus_card = |domain: &str, m: &nexus::Listing, why: &str| {
        json!({
            "source": "nexus", "id": m.mod_id.to_string(), "name": m.name, "summary": m.summary, "author": m.author, "version": m.version,
            "downloads": m.downloads, "endorsements": m.endorsements, "updated_ms": m.updated_ms, "size_kb": m.size_kb, "picture": m.picture,
            "category": m.category, "installed": have_nexus.contains(&m.mod_id), "why": why, "page_url": format!("https://www.nexusmods.com/{domain}/mods/{}", m.mod_id),
        })
    };
    let thunder_card = |community: &str, l: &thunderstore::Listing, why: &str| {
        json!({
            "source": "thunderstore", "id": l.full_name, "name": l.name.replace('_', " "), "summary": l.description, "author": l.owner, "version": l.version,
            "downloads": l.downloads, "endorsements": l.rating.max(0), "updated_ms": fsutil::iso_ms(&l.updated), "size_kb": 0, "picture": l.icon,
            "category": l.categories.first().cloned().unwrap_or_default(), "installed": have_thunder.contains(&l.full_name.to_lowercase()), "why": why,
            "page_url": format!("https://thunderstore.io/c/{community}/p/{}/{}/", l.owner, l.name),
        })
    };
    if !picks.is_empty() {
        for p in &picks {
            match p.source.as_str() {
                "nexus" => match &nexus {
                    Ok((domain, client)) => match (p.id.parse::<u64>(), client.mod_raw(domain, p.id.parse().unwrap_or(0))) {
                        (Ok(id), Ok(raw)) => {
                            let s = |k: &str| raw.get(k).and_then(Value::as_str).unwrap_or("").to_string();
                            let m = nexus::Listing {
                                mod_id: id,
                                name: s("name"),
                                summary: readme::plain(&s("summary")).chars().take(220).collect(),
                                version: s("version"),
                                author: s("author"),
                                endorsements: raw.get("endorsement_count").and_then(Value::as_u64).unwrap_or(0),
                                picture: s("picture_url"),
                                downloads: raw.get("mod_downloads").and_then(Value::as_u64).unwrap_or(0),
                                updated_ms: raw.get("updated_timestamp").and_then(Value::as_u64).unwrap_or(0) * 1000,
                                size_kb: 0,
                                category: String::new(),
                            };
                            if !m.name.is_empty() {
                                cards.push(nexus_card(domain, &m, &p.why));
                            }
                        }
                        (_, Err(e)) => notes.push(json!({ "source": "nexus", "error": e })),
                        _ => {}
                    },
                    Err(e) => {
                        if !notes.iter().any(|n| n["source"] == "nexus") {
                            notes.push(json!({ "source": "nexus", "error": e, "need_domain": !e.contains("API key") }));
                        }
                    }
                },
                _ => {
                    if let Ok((community, all)) = &thunder {
                        if let Some(l) = all.iter().find(|l| l.full_name.eq_ignore_ascii_case(&p.id)) {
                            cards.push(thunder_card(community, l, &p.why));
                        }
                    }
                }
            }
        }
    } else {
        // no list written for this game: the best liked of the most downloaded
        if let Ok((domain, client)) = &nexus {
            let q = nexus::StoreQuery { sort: "popular".into(), adult: false, count: 24, ..Default::default() };
            match client.store(domain, &q) {
                Ok((list, _)) => {
                    for m in list.iter().filter(|m| m.downloads >= 200 && m.endorsements * 100 >= m.downloads).take(6) {
                        cards.push(nexus_card(domain, m, "Many people downloaded it and liked it."));
                    }
                }
                Err(e) => notes.push(json!({ "source": "nexus", "error": e })),
            }
        } else if let Err(e) = &nexus {
            notes.push(json!({ "source": "nexus", "error": e, "need_domain": !e.contains("API key") }));
        }
        if let Ok((community, all)) = &thunder {
            let mut best: Vec<&thunderstore::Listing> = all.iter().filter(|l| !l.nsfw && l.rating > 0 && !is_support_package(l)).collect();
            best.sort_by_key(|l| std::cmp::Reverse(l.downloads));
            for l in best.into_iter().take(6) {
                cards.push(thunder_card(community, l, "Many people downloaded it and liked it."));
            }
        }
    }
    json!({ "cards": cards, "next": { "n": 0, "t": 0 }, "more": false, "categories": [], "notes": notes, "premium": read_state(state).nexus.user.as_ref().map(|u| u.premium).unwrap_or(false), "adult": false, "curated": !picks.is_empty() })
}

/// One page of the mod store: Nexus Mods and Thunderstore as the same kind of card, sorted the same way.
fn store_page(state: &SharedState, g: &GameEntry, ask: &StoreAsk) -> Value {
    if let Some(v) = store_blocked(g) {
        return v;
    }
    if ask.sort == "starter" && ask.query.trim().is_empty() {
        return starter_page(state, g);
    }
    if ask.sort == "saved" {
        return saved_page(state, g);
    }
    let adult = read_state(state).config.show_adult;
    let premium = read_state(state).nexus.user.as_ref().map(|u| u.premium).unwrap_or(false);
    let (have_nexus, have_thunder): (HashSet<u64>, HashSet<String>) = library_of(g)
        .map(|l| {
            let mods = l.mods();
            (mods.iter().filter_map(|m| m.nexus.as_ref().map(|n| n.mod_id)).collect(), mods.iter().filter_map(|m| m.thunderstore.as_ref().map(|t| t.full_name().to_lowercase())).collect())
        })
        .unwrap_or_default();
    let mut cards: Vec<Value> = Vec::new();
    let mut notes: Vec<Value> = Vec::new();
    let mut categories: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    let (mut next_n, mut next_t, mut next_m) = (ask.off_nexus, ask.off_thunder, ask.off_modio);
    let (mut more_n, mut more_t, mut more_m) = (false, false, false);
    let want_nexus = !matches!(ask.source.as_str(), "thunder" | "modio");
    let want_thunder = !matches!(ask.source.as_str(), "nexus" | "modio");
    let want_modio = matches!(ask.source.as_str(), "modio" | "both" | "all");
    let have_modio: HashSet<u64> = library_of(g).map(|l| l.mods().iter().filter_map(|m| m.modio.as_ref().map(|r| r.mod_id)).collect()).unwrap_or_default();

    if want_nexus {
        match nexus_domain_for(state, g).and_then(|d| nexus_client(state).map(|c| (d, c))) {
            Err(e) => notes.push(json!({ "source": "nexus", "error": e, "need_domain": !e.contains("API key") })),
            Ok((domain, client)) => {
                let q = nexus::StoreQuery { sort: ask.sort.clone(), query: ask.query.clone(), category: ask.category.clone(), adult, count: ask.per, offset: ask.off_nexus };
                match client.store(&domain, &q) {
                    Err(e) => notes.push(json!({ "source": "nexus", "error": e })),
                    Ok((list, total)) => {
                        next_n = ask.off_nexus + list.len();
                        more_n = list.len() >= ask.per && (total == 0 || (next_n as u64) < total);
                        if ask.off_nexus == 0 {
                            categories.extend(client.categories(&domain));
                        }
                        for m in list {
                            cards.push(json!({
                                "source": "nexus", "id": m.mod_id.to_string(), "name": m.name, "summary": m.summary, "author": m.author, "version": m.version,
                                "downloads": m.downloads, "endorsements": m.endorsements, "updated_ms": m.updated_ms, "size_kb": m.size_kb,
                                "picture": m.picture, "category": m.category, "installed": have_nexus.contains(&m.mod_id),
                                "page_url": format!("https://www.nexusmods.com/{domain}/mods/{}", m.mod_id),
                            }));
                        }
                    }
                }
            }
        }
    }
    if want_thunder {
        match thunder_community_for(state, g).and_then(|c| thunder_listing(state, &c).map(|l| (c, l))) {
            Err(e) => {
                // a game that has no Thunderstore community is normal, not a problem worth a notice
                if ask.source == "thunder" {
                    notes.push(json!({ "source": "thunderstore", "error": e, "need_community": true }));
                }
            }
            Ok((community, all)) => {
                let now = now_ms();
                let mut hits: Vec<&thunderstore::Listing> = if ask.query.trim().chars().count() >= 2 {
                    thunderstore::search(&all, &ask.query, 2000)
                } else {
                    all.iter().filter(|l| adult || !l.nsfw).collect()
                };
                let cat = ask.category.to_lowercase();
                if ask.hide_libs && cat.is_empty() {
                    hits.retain(|l| !is_support_package(l));
                }
                if !cat.is_empty() {
                    hits.retain(|l| l.categories.iter().any(|c| c.to_lowercase() == cat));
                }
                if ask.off_thunder == 0 {
                    categories.extend(all.iter().flat_map(|l| l.categories.iter().cloned()).filter(|c| !c.is_empty()).take(400));
                }
                let when = |l: &thunderstore::Listing| fsutil::iso_ms(&l.updated);
                if ask.query.trim().is_empty() || ask.sort != "relevance" {
                    match ask.sort.as_str() {
                        "endorsed" => hits.sort_by_key(|l| std::cmp::Reverse(l.rating)),
                        "new" | "updated" => hits.sort_by_key(|l| std::cmp::Reverse(when(l))),
                        "lately" => {
                            hits.retain(|l| now.saturating_sub(when(l)) < 180 * 86_400_000);
                            hits.sort_by_key(|l| std::cmp::Reverse(l.downloads));
                        }
                        _ => hits.sort_by_key(|l| std::cmp::Reverse(l.downloads)),
                    }
                }
                let page: Vec<&thunderstore::Listing> = hits.iter().skip(ask.off_thunder).take(ask.per).copied().collect();
                next_t = ask.off_thunder + page.len();
                more_t = next_t < hits.len();
                for l in page {
                    cards.push(json!({
                        "source": "thunderstore", "id": l.full_name, "name": l.name.replace('_', " "), "summary": l.description, "author": l.owner, "version": l.version,
                        "downloads": l.downloads, "endorsements": l.rating.max(0), "updated_ms": when(l), "size_kb": 0,
                        "picture": l.icon, "category": l.categories.first().cloned().unwrap_or_default(), "installed": have_thunder.contains(&l.full_name.to_lowercase()),
                        "page_url": format!("https://thunderstore.io/c/{community}/p/{}/{}/", l.owner, l.name),
                    }));
                }
            }
        }
    }
    if want_modio {
        match modio_game_for(state, g).and_then(|id| modio_client(state).map(|c| (id, c))) {
            Err(e) => {
                let has_key = read_state(state).modio.key.is_some();
                let choose = if has_key && e.contains("has no game called") { modio_candidates(state, g) } else { Vec::new() };
                notes.push(json!({ "source": "modio", "error": e, "need_key": !has_key, "choose": choose }));
            }
            Ok((game_id, client)) => match client.mods(game_id, &modio::Query { text: ask.query.clone(), sort: ask.sort.clone(), offset: ask.off_modio, limit: ask.per }) {
                Err(e) => notes.push(json!({ "source": "modio", "error": e })),
                Ok((list, total)) => {
                    next_m = ask.off_modio + list.len();
                    more_m = list.len() >= ask.per && (next_m as u64) < total;
                    for c in list.into_iter().filter(|c| adult || !c.adult) {
                        cards.push(json!({
                            "source": "modio", "id": c.id.to_string(), "name": c.name, "summary": c.summary, "author": c.author, "version": c.version,
                            "downloads": c.downloads, "endorsements": 0, "updated_ms": c.updated_ms, "size_kb": c.size_kb,
                            "picture": richtext::image_url(&c.picture).unwrap_or_default(), "category": "", "installed": have_modio.contains(&c.id), "page_url": c.page_url, "has_deps": c.has_deps,
                        }));
                    }
                }
            },
        }
    }
    // both sources in one list, ordered by what was asked for
    match ask.sort.as_str() {
        "endorsed" => cards.sort_by_key(|c| std::cmp::Reverse(c["endorsements"].as_u64().unwrap_or(0))),
        "new" | "updated" => cards.sort_by_key(|c| std::cmp::Reverse(c["updated_ms"].as_u64().unwrap_or(0))),
        _ => cards.sort_by_key(|c| std::cmp::Reverse(c["downloads"].as_u64().unwrap_or(0))),
    }
    // a game that is not on Nexus Mods is no problem when the other site has mods for it
    if (ask.source == "both" || ask.source == "all") && !cards.is_empty() {
        notes.retain(|n| !(n["source"] == "nexus" && n["need_domain"].as_bool().unwrap_or(false)));
    }
    json!({ "cards": cards, "next": { "n": next_n, "t": next_t, "m": next_m }, "more": more_n || more_t || more_m, "categories": categories, "notes": notes, "premium": premium, "adult": adult })
}

// ───────────────────────────── backup, disk use, cleanup ─────────────────────────────

fn dir_size(p: &Path) -> u64 {
    WalkDir::new(p).follow_links(false).into_iter().flatten().filter(|e| e.file_type().is_file()).filter_map(|e| e.metadata().ok()).map(|m| m.len()).sum()
}

/// Caches Gantry keeps that can be thrown away: `(id, label, folder name inside the cache root)`.
const CLEANABLE_CACHES: &[(&str, &str, &str)] = &[
    ("loaders", "Downloaded mod loaders", "loaders"),
    ("thunder", "Thunderstore mod lists", "thunder"),
    ("downloads", "Unfinished downloads", "downloads"),
    ("drops", "Files dropped on the window", "drops"),
];

/// How much room Gantry's data takes and what is wrong with it.
fn disk_report(state: &SharedState) -> Value {
    let games: Vec<GameEntry> = read_state(state).games.iter().filter(|g| g.is_installed).cloned().collect();
    let rows: Vec<Value> = games
        .iter()
        .filter_map(|g| {
            let lib = library_of(g)?;
            if !lib.root().is_dir() {
                return None;
            }
            let v = lib.verify();
            Some(json!({ "appid": g.appid, "name": g.name, "mods": v.mods, "mods_bytes": v.mods_bytes, "trash_bytes": v.trash_bytes, "linked_bytes": v.linked_bytes, "saves_bytes": v.saves_bytes, "old_saves_bytes": v.old_saves_bytes, "missing": v.missing, "orphans": v.orphans }))
        })
        .collect();
    let root = read_state(state).cache_root();
    let caches: Vec<Value> = CLEANABLE_CACHES.iter().map(|(id, label, dir)| json!({ "id": id, "label": label, "bytes": dir_size(&root.join(dir)) })).collect();
    json!({ "games": rows, "caches": caches, "backups_bytes": dir_size(&root.join("steam-backups")) })
}

/// Clean one kind of leftover. Only places Gantry owns are ever touched.
fn disk_clean(state: &SharedState, what: &str) -> String {
    let games: Vec<GameEntry> = read_state(state).games.iter().filter(|g| g.is_installed).cloned().collect();
    match what {
        "trash" => {
            let freed: u64 = games.iter().filter_map(library_of).map(|l| l.empty_trash()).sum();
            format!("Emptied the trash of the mod libraries ({} MB freed).", freed / 1_048_576)
        }
        "oldsaves" => {
            let freed: u64 = games.iter().filter_map(library_of).map(|l| l.clean_old_save_backups()).sum();
            format!("Deleted the old copies of replaced saves ({} MB freed).", freed / 1_048_576)
        }
        "orphans" => {
            let n: usize = games.iter().filter_map(library_of).filter_map(|l| l.clean_orphans().ok()).sum();
            format!("Moved {} leftover folder(s) to the trash.", n)
        }
        id => match CLEANABLE_CACHES.iter().find(|c| c.0 == id) {
            Some((_, label, dir)) => {
                let path = read_state(state).cache_root().join(dir);
                let freed = dir_size(&path);
                let _ = fs::remove_dir_all(&path);
                format!("Cleared “{}” ({} MB freed).", label, freed / 1_048_576)
            }
            None => "Nothing to clean there.".to_string(),
        },
    }
}

/// The "Why is it slow" screen: the last measured session of the selected game, how modded and plain
/// sessions compare, the mods most likely to cost something, and what the numbers suggest.
fn perf_json(state: &SharedState) -> Value {
    let (path, game, manual) = {
        let st = read_state(state);
        (st.paths.config_file.with_file_name("sessions.jsonl"), st.games.get(st.selected_idx).cloned(), st.perf_manual.clone())
    };
    let Some(g) = game else { return json!({ "sessions": 0 }) };
    let mine: Vec<stats::Session> = stats::read(&path).into_iter().filter(|s| s.appid == g.appid && s.perf.is_some()).collect();
    let last = mine.last().map(|s| json!({ "end_ms": s.end_ms, "secs": s.secs, "mods": s.mods, "perf": s.perf, "tips": perf::tips(s.perf.as_ref().unwrap_or(&Default::default())) }));
    let pairs: Vec<(usize, perf::Summary)> = mine.iter().filter_map(|s| Some((s.mods, s.perf.clone()?))).collect();
    let (modded, plain) = perf::compare(&pairs);
    let heavy = library_of(&g).map(|l| perf::heavy(&l.mods())).unwrap_or_default();
    let manual = manual.map(|(name, f)| json!({ "name": name, "frames": f, "tips": perf::tips(&perf::Summary { frames: Some(f), ..Default::default() }) }));
    json!({
        "sessions": mine.len(), "last": last, "modded": modded, "plain": plain, "heavy": heavy, "manual": manual,
        "mangohud": which_mangohud(),
    })
}

fn which_mangohud() -> bool {
    std::env::var_os("PATH").map(|p| std::env::split_paths(&p).any(|d| d.join("mangohud").is_file())).unwrap_or(false)
}

fn auto_backup_dir(st: &AppState) -> PathBuf {
    st.cache_root().join("auto-backups")
}

/// Write a backup of settings and mod lists into the automatic-backup folder.
fn run_auto_backup(state: &SharedState) -> Result<String, String> {
    let json = serde_json::to_string(&backup_bundle(state)).map_err(|e| e.to_string())?;
    let dir = auto_backup_dir(&read_state(state));
    autobackup::store(&dir, now_ms(), &json)
}

/// Back up once a week, checking now and then; off when the user switched it off.
fn spawn_auto_backup(state: Weak<RwLock<AppState>>) {
    spawn_background(state, "weekly_backup", "Weekly backup of settings and mod lists", Duration::from_secs(45), Duration::from_secs(6 * 3600), Box::new(|st, asked| {
        let (on, dir) = {
            let s = read_state(st);
            (!s.config.no_auto_backup, auto_backup_dir(&s))
        };
        if !on && !asked {
            return Ok(false);
        }
        if asked || autobackup::due(&dir, now_ms()) {
            match run_auto_backup(st) {
                Ok(_) => log(st, "INFO", "Weekly backup of ModForge's settings and mod lists made."),
                Err(e) => {
                    log(st, "WARN", format!("Weekly backup failed: {}", e));
                    return Err(e);
                }
            }
        }
        Ok(true)
    }));
}

/// How often a background task looks whether its time has come (and whether the game has closed).
const BG_LOOK: Duration = Duration::from_secs(3);

type BgWork = Box<dyn FnMut(&SharedState, bool) -> Result<bool, String> + Send>;

/// Run `work` now and then as the background task `name`, listed in the window. It waits while a game runs
/// (unless the user lets checks go on during play) and starts at once on Run now; `work` gets whether the user
/// asked and answers `Ok(false)` when it is switched off.
fn spawn_background(state: Weak<RwLock<AppState>>, name: &'static str, label: &'static str, first: Duration, every: Duration, mut work: BgWork) {
    if cfg!(test) {
        return;
    }
    background::register(name, label, first.as_millis() as u64, every.as_millis() as u64, now_ms());
    thread::spawn(move || loop {
        thread::sleep(BG_LOOK);
        let Some(st) = state.upgrade() else { return };
        let hold = background::hold_for(read_state(&st).is_running.load(Ordering::SeqCst));
        if let Some(asked) = background::due(name, now_ms(), hold) {
            let res = work(&st, asked);
            background::finished(name, now_ms(), res);
        }
    });
}

/// Everything Gantry knows about the user's games, as one file (no keys, no mod files).
fn backup_bundle(state: &SharedState) -> Value {
    let (config, custom, games) = {
        let st = read_state(state);
        (
            serde_json::to_value(&st.config).unwrap_or(Value::Null),
            fs::read_to_string(&st.paths.custom_games_file).ok().and_then(|t| serde_json::from_str::<Value>(&t).ok()).unwrap_or(json!([])),
            st.games.iter().filter(|g| g.is_installed).cloned().collect::<Vec<_>>(),
        )
    };
    let libs: Vec<Value> = games
        .iter()
        .filter_map(|g| {
            let lib = library_of(g)?;
            lib.root().is_dir().then(|| json!({ "appid": g.appid, "name": g.name, "library": lib.export_meta() }))
        })
        .collect();
    json!({ "gantry_backup": 1, "created_ms": now_ms(), "version": env!("CARGO_PKG_VERSION"), "config": config, "custom_games": custom, "libraries": libs })
}

/// Each game's mod list from a bundle, wherever its mods are still in the library: for the games already known first,
/// then again once a rescan has found the rest. `(mods put back, mods left out, games done)`.
fn restore_libraries(state: &SharedState, v: &Value) -> (usize, usize, usize) {
    let (mut restored, mut left_out, mut games_done) = (0, 0, 0);
    let mut done: std::collections::HashSet<String> = Default::default();
    for round in 0..2 {
        for entry in v["libraries"].as_array().into_iter().flatten() {
            let id = entry["appid"].as_str().unwrap_or("").to_string();
            if done.contains(&id) {
                continue;
            }
            let Some(g) = game_by_appid(state, &id) else { continue };
            let Some(lib) = library_of(&g) else { continue };
            done.insert(id);
            match lib.import_meta(&entry["library"]) {
                Ok((r, l)) => {
                    restored += r;
                    left_out += l;
                    games_done += 1;
                }
                Err(e) => log(state, "WARN", format!("{}: {}", g.name, e)),
            }
        }
        if round == 0 {
            rescan(state);
        }
    }
    (restored, left_out, games_done)
}

/// Put a backup back: settings and custom games (the current ones are kept first), then each
/// game's mod list wherever its mods are still in the library.
fn restore_bundle(state: &SharedState, v: &Value) -> Result<String, String> {
    if v.get("gantry_backup").and_then(Value::as_u64) != Some(1) {
        return Err("This file is not a ModForge backup.".into());
    }
    running_guard(state)?;
    let cfg: RigConfigData = serde_json::from_value(v["config"].clone()).map_err(|_| "The settings in this backup are not readable.".to_string())?;
    let stamp = now_ms();
    {
        let mut st = write_state(state);
        let keep = st.cache_root().join("backups");
        let _ = fs::create_dir_all(&keep);
        let _ = fs::copy(&st.paths.config_file, keep.join(format!("rig_config-{stamp}.json")));
        let _ = fs::copy(&st.paths.custom_games_file, keep.join(format!("custom_games-{stamp}.json")));
        st.config = cfg;
        st.save_config();
        if let Ok(j) = serde_json::to_string_pretty(&v["custom_games"]) {
            let _ = atomic_write(&st.paths.custom_games_file, j);
        }
    }
    let (restored, left_out, games_done) = restore_libraries(state, v);
    refresh_mods_shared(state);
    Ok(format!("Backup restored: settings, {} game(s), {} mod(s) back in their lists, {} left out because their files are gone.", games_done, restored, left_out))
}

// ───────────────────────────── sync through a folder ─────────────────────────────

/// What travels between computers: taste and lists, never paths, trainers, launch options, play time or keys.
const SYNCED_KEYS: &[&str] = &[
    "profiles", "library_first", "hidden_games", "theme", "custom_theme", "accent_from_game", "accent", "calm_motion", "reduce_motion", "high_contrast",
    "layout", "saved_mods", "show_adult", "compact", "favorites", "game_notes", "game_tags", "cover_appid", "nexus_domain", "thunder_community", "no_install_preview",
];

/// The folder inside the chosen one where the copies go.
fn sync_dir(folder: &str) -> PathBuf {
    Path::new(folder).join("Gantry-sync")
}

/// The portable part of the setup, ready to put in the sync folder.
fn sync_bundle(state: &SharedState) -> Value {
    let mut b = backup_bundle(state);
    let all = b["config"].as_object().cloned().unwrap_or_default();
    b["config"] = Value::Object(all.into_iter().filter(|(k, _)| SYNCED_KEYS.contains(&k.as_str())).collect());
    b["custom_games"] = json!([]);
    // the tool and trainer of a profile are files on this computer: their paths stay here
    for entry in b["libraries"].as_array_mut().into_iter().flatten() {
        for l in entry["library"]["profile_launch"].as_object_mut().into_iter().flat_map(|o| o.values_mut()) {
            if let Some(o) = l.as_object_mut() {
                o.remove("tool");
                o.remove("trainer");
            }
        }
    }
    b
}

/// The same bundle with the moment it was made taken out, to see whether anything changed since the last copy.
fn without_time(v: &Value) -> String {
    let mut c = v.clone();
    if let Some(o) = c.as_object_mut() {
        o.remove("created_ms");
    }
    c.to_string()
}

/// A folder the person typed: it has to exist, be written as a full path and not be one of Gantry's own.
fn sync_folder_ok(path: &str, own: &[PathBuf]) -> Result<String, String> {
    let p = Path::new(path.trim());
    if path.trim().is_empty() || !p.is_absolute() || p.components().count() < 3 {
        return Err("Use the full path of a folder, for example /home/you/Sync.".into());
    }
    if !p.is_dir() {
        return Err("That folder does not exist.".into());
    }
    let canon = fsutil::canonical(p).map_err(|_| "That folder cannot be read.".to_string())?;
    if own.iter().any(|o| fsutil::canonical(o).map(|o| canon.starts_with(&o) || o.starts_with(&canon)).unwrap_or(false)) {
        return Err("That folder is one of Gantry's own; choose another.".into());
    }
    Ok(canon.to_string_lossy().to_string())
}

/// Put a copy of the portable setup in the sync folder, unless the newest copy there is the same.
fn sync_push(state: &SharedState) -> Result<String, String> {
    let folder = read_state(state).config.sync_folder.clone();
    if folder.is_empty() {
        return Err("Choose a sync folder first.".into());
    }
    let dir = sync_dir(&folder);
    let bundle = sync_bundle(state);
    if let Some(newest) = autobackup::list(&dir).first() {
        if autobackup::read(&dir, &newest.name).map(|old| without_time(&old) == without_time(&bundle)).unwrap_or(false) {
            return Ok("unchanged".into());
        }
    }
    autobackup::store(&dir, now_ms(), &bundle.to_string())
}

/// Bring in the newest copy from the sync folder: the portable settings and each game's mod list; what belongs to this
/// computer stays. The settings it replaces are kept first.
fn sync_pull(state: &SharedState) -> Result<String, String> {
    let folder = read_state(state).config.sync_folder.clone();
    if folder.is_empty() {
        return Err("Choose a sync folder first.".into());
    }
    let dir = sync_dir(&folder);
    let newest = autobackup::list(&dir).into_iter().next().ok_or("There is nothing in the sync folder yet.")?;
    let v = autobackup::read(&dir, &newest.name)?;
    if v.get("gantry_backup").and_then(Value::as_u64) != Some(1) {
        return Err("This file is not a ModForge backup.".into());
    }
    running_guard(state)?;
    {
        let mut st = write_state(state);
        let keep = st.cache_root().join("backups");
        let _ = fs::create_dir_all(&keep);
        let _ = fs::copy(&st.paths.config_file, keep.join(format!("rig_config-{}.json", now_ms())));
        let mut cur = serde_json::to_value(&st.config).map_err(|e| e.to_string())?;
        if let (Some(c), Some(theirs)) = (cur.as_object_mut(), v["config"].as_object()) {
            for k in SYNCED_KEYS {
                if let Some(x) = theirs.get(*k) {
                    c.insert((*k).to_string(), x.clone());
                }
            }
        }
        st.config = serde_json::from_value(cur).map_err(|_| "The settings in the sync folder are not readable.".to_string())?;
        st.save_config();
        st.notifier.bump();
    }
    let (restored, left_out, games) = restore_libraries(state, &v);
    refresh_mods_shared(state);
    let msg = format!("Brought in the setup from the sync folder: {} game(s), {} mod(s) back in their lists, {} left out because their files are not here.", games, restored, left_out);
    if left_out > 0 {
        log(state, "OK", msg);
        return Ok(fetch_left_out(state, &v));
    }
    Ok(msg)
}

/// Bring my setup here: the mods of a pulled setup whose files are not on this computer are downloaded again
/// (Thunderstore, GitHub, mod.io by themselves; Nexus Mods ones wait in the game's "still needs" list).
fn fetch_left_out(state: &SharedState, v: &Value) -> String {
    let mut done = 0;
    for entry in v["libraries"].as_array().into_iter().flatten() {
        let Some(g) = game_by_appid(state, entry["appid"].as_str().unwrap_or("")) else { continue };
        let (Some(lib), Some(spec)) = (library_of(&g), deploy::spec_from_meta(&entry["library"], &g.name)) else { continue };
        if lib.missing_of(&spec).is_empty() {
            continue;
        }
        log(state, "INFO", format!("Getting the mods of {} that are not on this computer...", g.name));
        match import_spec(state, &g, spec) {
            Ok(m) => {
                done += 1;
                log(state, "OK", format!("{}: {}", g.name, m));
            }
            Err(e) => log(state, "WARN", format!("{}: {}", g.name, e)),
        }
    }
    refresh_mods_shared(state);
    format!("Your setup is here: the missing mods of {} game(s) were fetched or are listed to get.", done)
}

/// Keep the sync folder up to date: look every twenty minutes and copy when something changed.
fn spawn_sync_push(state: Weak<RwLock<AppState>>) {
    spawn_background(state, "sync", "Copying your setup to the sync folder", Duration::from_secs(90), Duration::from_secs(20 * 60), Box::new(|st, _| {
        if read_state(st).config.sync_folder.is_empty() {
            return Ok(false);
        }
        match sync_push(st) {
            Ok(n) if n != "unchanged" => log(st, "INFO", "Your setup was copied to the sync folder."),
            Err(e) => {
                log(st, "WARN", e.clone());
                return Err(e);
            }
            _ => {}
        }
        Ok(true)
    }));
}

/// Recreate a shared setup: use what the library already has, fetch the Thunderstore packages it
/// lacks, and say where to get the rest.
fn import_spec(state: &SharedState, g: &GameEntry, spec: deploy::ProfileSpec) -> Result<String, String> {
    running_guard(state)?;
    let lib = library_of(g).ok_or_else(|| format!("{} is not installed.", g.name))?;
    let missing = lib.apply_spec(&spec)?;
    let (mut fetched, mut manual) = (0, 0);
    for m in &missing {
        if let Some(res) = fetch_spec_mod(state, g, m) {
            match res {
                Ok(msg) => {
                    fetched += 1;
                    log(state, "OK", msg);
                }
                Err(e) => {
                    manual += 1;
                    log(state, "ERR", format!("{}: {}", m.name, e));
                }
            }
        } else {
            manual += 1;
            let whence = match &m.nexus {
                Some(n) => format!("https://www.nexusmods.com/{}/mods/{}", n.game, n.mod_id),
                None => m.source.clone(),
            };
            log(state, "WARN", format!("\"{}\" is part of this setup but not in your library yet ({}).", m.name, whence));
        }
    }
    if fetched > 0 {
        lib.apply_spec(&spec)?; // newcomers take their place in the order and their on/off state
    }
    // what has to be fetched by hand is remembered, so each mod takes its place when it arrives
    {
        let key = lib.root().to_string_lossy().to_string();
        let mut st = write_state(state);
        if lib.missing_of(&spec).is_empty() {
            st.config.pending_packs.remove(&key);
        } else {
            st.config.pending_packs.insert(key, spec.clone());
        }
        st.save_config();
    }
    let applied = library_apply(state, &lib, false)?;
    Ok(format!("Imported the setup: {} fetched, {} still to get yourself. {}", fetched, manual, applied))
}

/// Download one mod of a shared setup from where it came from, when that needs no clicks: Thunderstore, a GitHub
/// release (the same release when it still exists, else the newest one's matching file) or mod.io. `None` for the
/// rest (Nexus Mods, files that were never online).
fn fetch_spec_mod(state: &SharedState, g: &GameEntry, m: &deploy::SpecMod) -> Option<Result<String, String>> {
    if let Some(t) = &m.thunderstore {
        return Some(thunder_install(state, g, &format!("{}-{}-{}", t.namespace, t.name, t.version)));
    }
    if let Some(r) = &m.github {
        return Some((|| {
            let (owner, repo) = github::parse_repo(&r.repo_name()).ok_or("That does not look like a GitHub repository.")?;
            let (api, allow) = {
                let st = read_state(state);
                (st.github_api.clone(), st.github_allow.clone())
            };
            let rel = github::release_or_latest(&owner, &repo, &r.tag, &api, allow.as_deref())?;
            let asset = rel.assets.iter().find(|a| a.name == r.asset).cloned().or_else(|| github::matching_asset(&r.asset, &rel.assets)).ok_or("That release no longer has a file Gantry recognises.")?;
            let before = library_of(g).map(|l| l.mods().len()).unwrap_or(0);
            let reference = github::GithubRef { owner: owner.clone(), repo: repo.clone(), tag: rel.tag.clone(), asset: asset.name.clone() };
            github_install_named(state, g, &r.repo_name(), &asset.url, Some(reference), Some(&m.name));
            let after = library_of(g).map(|l| l.mods().len()).unwrap_or(0);
            if after > before {
                Ok(format!("Added {} from {}/{}.", m.name, owner, repo))
            } else {
                Err("The download did not work (see the log).".to_string())
            }
        })());
    }
    if let Some(r) = &m.modio {
        return Some(modio_install(state, g, r.mod_id));
    }
    None
}

/// What a modpack imported earlier still waits for in this game's library (null when nothing).
fn pending_pack_json(state: &SharedState, g: &GameEntry) -> Value {
    let Some(lib) = library_of(g) else { return Value::Null };
    let Some(spec) = read_state(state).config.pending_packs.get(&lib.root().to_string_lossy().to_string()).cloned() else { return Value::Null };
    let missing = lib.missing_of(&spec);
    if missing.is_empty() {
        return Value::Null;
    }
    json!({
        "name": spec.name,
        "mods": missing.iter().map(|m| json!({
            "name": m.name, "version": m.version, "source": m.source,
            "nexus": m.nexus.as_ref().map(|n| json!({ "game": n.game, "mod_id": n.mod_id, "url": format!("https://www.nexusmods.com/{}/mods/{}", n.game, n.mod_id) })),
            "thunderstore": m.thunderstore.is_some(),
        })).collect::<Vec<_>>(),
    })
}

/// A mod arrived in a library that waits for a modpack: give it its place and its on/off state.
fn reapply_pending(state: &SharedState, lib: &deploy::Library) {
    let key = lib.root().to_string_lossy().to_string();
    let Some(spec) = read_state(state).config.pending_packs.get(&key).cloned() else { return };
    let missing = lib.apply_spec(&spec).unwrap_or_else(|_| spec.mods.clone());
    if missing.is_empty() {
        let mut st = write_state(state);
        st.config.pending_packs.remove(&key);
        st.save_config();
        drop(st);
        if spec.name.is_empty() {
            log(state, "OK", "The modpack you imported is complete.");
            add_note(state, "pack", "The modpack you imported is complete.", "");
        } else {
            log(state, "OK", format!("The modpack “{}” is complete.", spec.name));
            add_note(state, "pack", format!("The modpack “{}” is complete.", spec.name), "");
        }
    }
}

/// Save the selected game's mods as a modpack: a zip with the list (`pack.json`) and the files of the mods.
/// When `full_bundle` is true, all enabled mods are packed with their full files for offline/friend sharing.
fn export_pack_file(state: &SharedState, g: &GameEntry, name: &str, path: &Path, full_bundle: bool) -> Result<String, String> {
    let lib = library_of(g).ok_or_else(|| format!("{} is not installed.", g.name))?;
    let tmp = read_state(state).cache_root().join("pack").join(fsutil::unique_token());
    fs::create_dir_all(&tmp).map_err(|e| e.to_string())?;
    let done = (|| {
        let (spec, copied) = lib.export_pack_opt(&g.name, name, "", &tmp, full_bundle)?;
        fs::write(tmp.join("pack.json"), serde_json::to_string_pretty(&spec).map_err(|e| e.to_string())?).map_err(|e| e.to_string())?;
        let tool = ["bsdtar", "tar"].iter().find(|t| fsutil::find_in_path(t).is_some()).ok_or("Making a modpack needs bsdtar (or tar).")?;
        let _ = fs::remove_file(path);
        let ok = Command::new(tool).args(["--format", "zip", "-cf"]).arg(path).arg("-C").arg(&tmp).arg(".").stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).status().map(|s| s.success()).unwrap_or(false);
        if !ok || !path.is_file() {
            return Err("Could not write the modpack file.".to_string());
        }
        Ok(format!("Saved the modpack of {} mod(s) to {} ({} with their files).", spec.mods.len(), path.display(), copied))
    })();
    let _ = fs::remove_dir_all(&tmp);
    done
}

/// A modpack (or an older plain setup file) read from disk: the list, and the folder its files were unpacked to.
fn read_pack_file(state: &SharedState, path: &Path) -> Result<(deploy::ProfileSpec, Option<PathBuf>), String> {
    let head = {
        let mut f = fs::File::open(path).map_err(|e| e.to_string())?;
        let mut b = [0u8; 4];
        let n = std::io::Read::read(&mut f, &mut b).map_err(|e| e.to_string())?;
        b[..n].to_vec()
    };
    if head.starts_with(b"PK") {
        let dir = read_state(state).cache_root().join("pack").join(fsutil::unique_token());
        fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
        let extracted = platform::extract_commands(path, &dir).into_iter().any(|mut c| c.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).status().map(|s| s.success()).unwrap_or(false));
        let spec = extracted
            .then(|| fs::read(dir.join("pack.json")).ok())
            .flatten()
            .filter(|b| b.len() <= 2_000_000)
            .and_then(|b| serde_json::from_slice::<deploy::ProfileSpec>(&b).ok());
        match spec {
            Some(s) => Ok((s, Some(dir))),
            None => {
                let _ = fs::remove_dir_all(&dir);
                Err("This file is not a ModForge modpack.".into())
            }
        }
    } else {
        let spec = fs::metadata(path)
            .map_err(|e| e.to_string())
            .and_then(|m| if m.len() > 1_000_000 { Err("That file is too large to be a setup.".to_string()) } else { Ok(()) })
            .and_then(|_| fs::read(path).map_err(|e| e.to_string()))
            .and_then(|b| serde_json::from_slice::<deploy::ProfileSpec>(&b).map_err(|_| "This file is not a Gantry setup.".to_string()))?;
        Ok((spec, None))
    }
}

/// The mods of a Thunderstore profile code (the code r2modman and Gale give), as a setup for `game`.
fn profile_from_code(state: &SharedState, code: &str, game: &str) -> Result<deploy::ProfileSpec, String> {
    let code = profilecode::clean_code(code).ok_or("That does not look like a profile code.")?;
    let text = thunder_client(state).profile_file(&code)?;
    let zip = profilecode::unwrap_profile(&text).ok_or("Thunderstore sent something that is not a profile.")?;
    let dir = read_state(state).cache_root().join("pack").join(fsutil::unique_token());
    fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let zip_path = dir.join("profile.r2z");
    let done = (|| {
        fs::write(&zip_path, &zip).map_err(|e| e.to_string())?;
        let out = dir.join("x");
        fs::create_dir_all(&out).map_err(|e| e.to_string())?;
        let ok = platform::extract_commands(&zip_path, &out).into_iter().any(|mut c| c.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).status().map(|s| s.success()).unwrap_or(false));
        let text = ok.then(|| fs::read_to_string(out.join("export.r2x")).ok()).flatten().filter(|t| t.len() <= 2_000_000);
        let r2x = profilecode::parse_r2x(&text.ok_or("The profile could not be read (is bsdtar, 7z or unzip installed?).")?)?;
        let spec = profilecode::to_spec(&r2x, game);
        if spec.mods.is_empty() {
            return Err("This profile has no mods Gantry can fetch.".into());
        }
        Ok(spec)
    })();
    let _ = fs::remove_dir_all(&dir);
    done
}

/// Show a shared setup before anything is fetched: every mod and how it would arrive ("have": already here,
/// "included": its files are in the modpack, "auto": downloads by itself, "nexus": one click on Nexus Mods each with a
/// free account, "manual": it was never online). Replaces a preview that was still open.
fn preview_import(state: &SharedState, g: &GameEntry, spec: deploy::ProfileSpec, files: Option<PathBuf>) -> Result<(), String> {
    let lib = library_of(g).ok_or_else(|| format!("{} is not installed.", g.name))?;
    if spec.gantry_profile != 1 {
        return Err("This file is not a Gantry setup.".into());
    }
    let missing = lib.missing_of(&spec);
    let mut left: Vec<&deploy::SpecMod> = missing.iter().collect();
    let mut counts: std::collections::BTreeMap<&str, usize> = Default::default();
    let rows: Vec<Value> = spec
        .mods
        .iter()
        .map(|m| {
            let how = match left.iter().position(|x| *x == m) {
                None => "have",
                Some(i) => {
                    left.remove(i);
                    if !m.files.is_empty() && files.as_ref().map(|d| d.join(&m.files).is_dir()).unwrap_or(false) {
                        "included"
                    } else if m.thunderstore.is_some() || m.github.is_some() || m.modio.is_some() {
                        "auto"
                    } else if m.nexus.is_some() {
                        "nexus"
                    } else {
                        "manual"
                    }
                }
            };
            *counts.entry(how).or_default() += 1;
            json!({ "name": m.name, "version": m.version, "enabled": m.enabled, "how": how })
        })
        .collect();
    let same_game = spec.game.is_empty() || spec.game.eq_ignore_ascii_case(&g.name);
    let view = json!({ "name": spec.name, "note": spec.note, "game": spec.game, "here": g.name, "same_game": same_game, "mods": rows, "counts": counts });
    let old = write_state(state).import_preview.replace(ImportPreview { appid: g.appid.clone(), spec, files, view });
    if let Some(dir) = old.and_then(|o| o.files) {
        let _ = fs::remove_dir_all(dir);
    }
    read_state(state).notifier.bump();
    Ok(())
}

/// The user said yes (or no) to the setup in the preview.
fn finish_import(state: &SharedState, accept: bool) {
    let Some(p) = write_state(state).import_preview.take() else { return };
    read_state(state).notifier.bump();
    if !accept {
        if let Some(dir) = p.files {
            let _ = fs::remove_dir_all(dir);
        }
        return;
    }
    let Some(g) = game_by_appid(state, &p.appid) else {
        return log(state, "WARN", "That game is no longer in the list.");
    };
    match import_pack(state, &g, p.spec, p.files) {
        Ok(m) => log(state, "OK", m),
        Err(e) => log(state, "ERR", e),
    }
    refresh_mods_shared(state);
}

/// Bring a modpack into the game's library: its own files first, then the Thunderstore mods, and what is left
/// (Nexus Mods) is listed so it can be fetched one by one.
fn import_pack(state: &SharedState, g: &GameEntry, spec: deploy::ProfileSpec, files: Option<PathBuf>) -> Result<String, String> {
    running_guard(state)?;
    let lib = library_of(g).ok_or_else(|| format!("{} is not installed.", g.name))?;
    let mut added = 0;
    if let Some(dir) = &files {
        for m in lib.missing_of(&spec).iter().filter(|m| !m.files.is_empty()) {
            // only `mods/<number>` inside the unpacked folder is ever read
            let ok = m.files.strip_prefix("mods/").map(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit())).unwrap_or(false);
            let stage = dir.join(&m.files);
            if !ok || !stage.is_dir() {
                continue;
            }
            match lib.add_with(&stage, &m.name, "modpack", None, &m.version) {
                Ok(_) => added += 1,
                Err(e) => log(state, "ERR", e),
            }
        }
    }
    let result = import_spec(state, g, spec);
    if let Some(dir) = files {
        let _ = fs::remove_dir_all(dir);
    }
    result.map(|m| if added > 0 { format!("{} Mods that came with their files: {}.", m, added) } else { m })
}

/// One thing worth telling the user about a game's mods.
fn issue(code: &'static str, level: &'static str, fix: &'static str, args: Vec<String>) -> Value {
    json!({ "code": code, "level": level, "fix": fix, "args": args })
}

/// What Steam is doing with this game's files (idle for games that do not come from Steam).
fn steam_update_of(g: &GameEntry) -> steamlib::UpdateState {
    if cfg!(test) {
        if let Some(running) = TEST_STEAM_RUNNING.with(|c| c.get()) {
            return steam_update_with(g, running);
        }
    }
    steam_update_with(g, procs::steam_running())
}

// Tests say whether Steam "runs" (the real answer depends on the machine they run on). Not behind cfg(test): the
// checks that split this file into program and tests look for the first such marker.
thread_local! {
    static TEST_STEAM_RUNNING: std::cell::Cell<Option<bool>> = const { std::cell::Cell::new(None) };
}

/// Steam can only be updating a game while it runs: a record left behind by a crash must not hold the game back for good.
fn steam_update_with(g: &GameEntry, steam_running: bool) -> steamlib::UpdateState {
    if g.steam_id().is_empty() || g.game_dir.is_empty() || !steam_running {
        return steamlib::UpdateState::IDLE;
    }
    steamlib::game_update(Path::new(&g.game_dir), g.steam_id())
}

fn level_rank(l: &str) -> u8 {
    match l {
        "bad" => 3,
        "warn" => 2,
        "info" => 1,
        _ => 0,
    }
}

/// What is wrong with a game's mods right now, worst first. `deep` also measures the running
/// game's CPU (about a second), which only the open game page needs.
/// Known problems the game's logs name right now (the list in data/errors.json, plus the user's own).
fn kb_findings(state: &SharedState, g: &GameEntry) -> Vec<kb::Finding> {
    let prefixes: Vec<PathBuf> = [g.pfx.clone(), format!("{}/pfx", g.compatdata)].into_iter().filter(|p| !p.is_empty() && p != "/pfx").map(PathBuf::from).collect();
    let ue_log = ModManager::read_ue4ss_log_tail(g, 400);
    let proton_log = home_dir().join(format!("steam-{}.log", if g.steam_appid.is_empty() { &g.appid } else { &g.steam_appid }));
    let logs = kb::collect_logs(Path::new(&g.game_dir), &prefixes, &ue_log, Some(proton_log.as_path()).filter(|p| p.is_file()), crash::unreal_log_files(g).1.as_deref(), &home_dir());
    if logs.is_empty() {
        return Vec::new();
    }
    let user = read_state(state).paths.config_file.with_file_name("errors.json");
    kb::scan(&kb::patterns(Some(&user)), &logs)
}

fn game_issues(state: &SharedState, g: &GameEntry, deep: bool) -> (Vec<Value>, Value) {
    let dir = PathBuf::from(&g.game_dir);
    let unity = loaders::detect(&dir);
    let loader = unity.as_ref().map(|u| u.loader.clone()).unwrap_or_default();
    let mut out: Vec<Value> = Vec::new();
    let lib = library_of(g);
    let (mods, status) = match &lib {
        Some(l) => (l.mods(), l.status_json()),
        None => (Vec::new(), Value::Null),
    };
    let deployed = status["deployed"].as_u64().unwrap_or(0);
    // Steam is busy with the game's files: nothing should be played or changed until it is done
    let upd = steam_update_of(g);
    match upd.phase {
        steamlib::Phase::Updating => out.push(issue("steam_updating", "warn", "", vec![upd.percent.map(|p| p.to_string()).unwrap_or_default()])),
        steamlib::Phase::Pending | steamlib::Phase::Paused => out.push(issue("steam_update_waiting", "info", "", vec![])),
        steamlib::Phase::Idle => {}
    }
    // modified files and a game that bans for them
    if let Some(ac) = status["anti_cheat"].as_str() {
        if deployed > 0 {
            out.push(issue("anti_cheat", "bad", "vanilla", vec![ac.to_string()]));
        }
    }
    // the loader: logs, CPU, and the launch option of the account that is logged in
    let running = procs::is_game_running(g);
    let mut primary = json!({ "code": "none", "level": "info", "fix": "", "count": 0, "idle_mins": 0, "last_error": "" });
    if !loader.is_empty() {
        let cpu = if deep && running { procs::cpu_percent(g, 800) } else { None };
        let prefixes: Vec<PathBuf> = [g.pfx.clone(), format!("{}/pfx", g.compatdata)].into_iter().filter(|p| !p.is_empty() && p != "/pfx").map(PathBuf::from).collect();
        let h = logdoctor::diagnose(&logdoctor::Input { game_dir: &dir, prefixes, loader: &loader, running, cpu_percent: if deep { cpu } else { Some(100.0) }, now_ms: now_ms() });
        primary = serde_json::to_value(&h).unwrap_or(primary);
        let steam = if !g.launches_directly() { steam_info(state, g).status } else { String::new() };
        let launch_missing = matches!(steam.as_str(), "missing" | "stale");
        if launch_missing && h.code != "ok" {
            out.push(issue("launch_option", "bad", "launch_option", vec![]));
        } else if matches!(h.code, "not_loaded" | "frozen" | "incomplete" | "errors") {
            let mut i = issue(h.code, h.level, h.fix, vec![h.idle_mins.to_string(), h.last_error.clone()]);
            i["args"] = json!([h.idle_mins.to_string(), h.last_error]);
            out.push(i);
        }
    } else if unity.is_some() && mods.iter().any(|m| m.enabled) {
        out.push(issue("needs_loader", "warn", "install_loader", vec![]));
    }
    // a mod that loads through a proxy DLL (Cyber Engine Tweaks' version.dll, ...) only loads when Steam's launch options carry Wine's override
    if !platform::IS_WINDOWS && !platform::IS_MAC && loader.is_empty() && !g.launches_directly() && deployed > 0 && !library_overrides(g).is_empty() && matches!(steam_info(state, g).status.as_str(), "missing" | "stale") {
        out.push(issue("launch_option", "bad", "launch_option", vec!["mods".to_string()]));
    }
    if let Some(note) = recipes::find(&read_state(state).recipes, &g.appid, g.recipe_steam_id()).map(|r| r.notes.trim().to_string()).filter(|n| !n.is_empty()) {
        out.push(issue("pack_note", "info", "", vec![note]));
    }
    for f in kb_findings(state, g) {
        let level = match f.level.as_str() {
            "bad" => "bad",
            "info" => "info",
            _ => "warn",
        };
        out.push(json!({ "code": "known", "level": level, "fix": "", "args": [], "kb": f }));
    }
    let drift = status["drift"]["count"].as_u64().unwrap_or(0);
    if drift > 0 {
        out.push(issue("drift", "warn", "redeploy", vec![drift.to_string()]));
    }
    let enabled_lib_ids: std::collections::HashSet<String> = mods.iter().filter(|m| m.enabled).map(|m| m.id.clone()).collect();
    let needs: Vec<String> = read_state(state).nexus.needs.iter()
        .filter(|n| n.appid == g.appid && enabled_lib_ids.contains(&n.lib_id))
        .flat_map(|n| n.missing.iter().map(|m| m.name.clone()))
        .collect();
    if !needs.is_empty() {
        out.push(issue("needs", "warn", "", vec![needs.join(", ")]));
    }
    out.extend(duplicate_issues(&mods));
    out.extend(game_update_issue(state, g, &mods));
    out.extend(framework_issue(&dir, &mods));
    if let Some(l) = &lib {
        out.extend(clash_issues(&dir, l));
    }
    if status["dirty"].as_bool().unwrap_or(false) && !mods.is_empty() && drift == 0 {
        out.push(issue("dirty", "info", "deploy", vec![]));
    }
    out.sort_by_key(|i| std::cmp::Reverse(level_rank(i["level"].as_str().unwrap_or(""))));
    let mut info = primary;
    info["running"] = json!(running);
    info["loader"] = json!(loader);
    (out, info)
}

/// The same mod twice (two versions of one Nexus page, say) is a common reason for scripts that do not compile.
fn duplicate_issues(mods: &[deploy::LibMod]) -> Vec<Value> {
    let mut groups: std::collections::BTreeMap<String, Vec<String>> = Default::default();
    for m in mods.iter().filter(|m| m.enabled) {
        // one Nexus page can offer several files at once (a head and a body, say): only the same name twice is a duplicate
        let norm = |s: &str| s.to_lowercase().chars().filter(|c| c.is_alphanumeric()).collect::<String>();
        let key = m.nexus.as_ref().filter(|n| n.valid()).map(|n| format!("nx:{}:{}:{}", n.game, n.mod_id, norm(&m.name))).or_else(|| m.thunderstore.as_ref().map(|t| format!("ts:{}", t.full_name().to_lowercase()))).or_else(|| m.modio.as_ref().filter(|r| r.valid()).map(|r| format!("mio:{}:{}", r.game_id, r.mod_id)));
        if let Some(k) = key {
            groups.entry(k).or_default().push(if m.version.is_empty() { m.name.clone() } else { format!("{} v{}", m.name, m.version) });
        }
    }
    groups.values().filter(|v| v.len() > 1).take(3).map(|names| issue("duplicate_mod", "warn", "", vec![names.join(", ")])).collect()
}

/// Core mods (RED4ext, ArchiveXL, a script extender ...) that the enabled mods ask for and the game does not have.
fn framework_issue(dir: &Path, mods: &[deploy::LibMod]) -> Option<Value> {
    let fw = frameworks::missing(dir, &files_in_game(mods));
    if fw.is_empty() {
        return None;
    }
    let mut i = issue("needs_framework", "warn", "setup", fw.iter().map(|x| x.name.to_string()).collect());
    i["links"] = json!(fw.iter().map(|x| json!({ "name": x.name, "url": x.page })).collect::<Vec<_>>());
    Some(i)
}

/// Conflicts that file names do not show: Cyberpunk archives bringing the same game resource, and Redscript
/// declarations made by two mods.
fn clash_view(dir: &Path, lib: &deploy::Library) -> (Vec<clashes::ArchiveClash>, Vec<clashes::ScriptClash>) {
    let files = lib.enabled_mod_files();
    let has_modlist = dir.join("archive/pc/mod/modlist.txt").is_file();
    (clashes::archive_clashes(&files, has_modlist), clashes::script_clashes(&files))
}

fn clash_issues(dir: &Path, lib: &deploy::Library) -> Vec<Value> {
    // only games whose mods have archives or scripts are read at all
    if !dir.join("archive/pc").is_dir() && !dir.join("r6").is_dir() {
        return Vec::new();
    }
    let (archives, scripts) = clash_view(dir, lib);
    let mut out = Vec::new();
    if !archives.is_empty() {
        out.push(issue("archive_clash", "warn", "conflicts", archives.iter().take(4).map(|c| format!("{} / {}", c.winner_mod, c.loser_mod)).collect()));
    }
    if !scripts.is_empty() {
        out.push(issue("script_clash", "warn", "conflicts", scripts.iter().take(4).map(|c| format!("{} ({})", c.what, c.mods.join(", "))).collect()));
    }
    out
}

/// The build of the game's executable: its declared version (may be empty), size and date, joined by `|`.
fn game_build(g: &GameEntry) -> Option<String> {
    let exe = game_exe_file(g)?;
    let meta = fs::metadata(&exe).ok()?;
    let secs = meta.modified().ok().and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map(|d| d.as_secs()).unwrap_or(0);
    Some(format!("{}|{}|{}", game_version(g).map(|v| trainer::version_text(&v)).unwrap_or_default(), meta.len(), secs))
}

/// Mods that run code inside the game (plugins, script mods), the ones a game patch tends to break.
fn hooks_into_game(m: &deploy::LibMod) -> bool {
    m.files.iter().any(|f| {
        let f = f.to_lowercase();
        [".dll", ".asi", ".reds", ".lua"].iter().any(|e| f.ends_with(e)) || f.contains("ue4ss/mods/")
    })
}

/// A warning when the game's executable changed since the mods last worked and some of them hook into it. The first
/// build Gantry sees, and a change with no such mod on, are taken as they are.
fn game_update_issue(state: &SharedState, g: &GameEntry, mods: &[deploy::LibMod]) -> Option<Value> {
    let now = game_build(g)?;
    let before = read_state(state).config.game_builds.get(&g.appid).cloned();
    let risky: Vec<&str> = mods.iter().filter(|m| m.enabled && hooks_into_game(m)).map(|m| m.name.as_str()).collect();
    match before {
        Some(b) if b == now => None,
        Some(b) if !risky.is_empty() => {
            let ver = |x: &str| x.split('|').next().unwrap_or("").to_string();
            let (old, new) = (ver(&b), ver(&now));
            let span = if !old.is_empty() && !new.is_empty() && old != new { [old.as_str(), new.as_str()].join(" → ") } else { String::new() };
            let mut names = risky.iter().take(4).map(|n| n.to_string()).collect::<Vec<_>>().join(", ");
            if risky.len() > 4 {
                names.push_str(&format!(" (+{})", risky.len() - 4));
            }
            Some(issue("game_updated", "warn", "updates", vec![span, names]))
        }
        _ => {
            accept_game_build(state, g);
            None
        }
    }
}

/// The current build of the game is the one its mods work with.
fn accept_game_build(state: &SharedState, g: &GameEntry) {
    let Some(now) = game_build(g) else { return };
    let mut st = write_state(state);
    if st.config.game_builds.get(&g.appid) != Some(&now) {
        st.config.game_builds.insert(g.appid.clone(), now);
        st.save_config();
    }
}

/// Less free space than this on the game's drive is worth a word before playing (saves, shader caches, logs).
const LOW_DISK: u64 = 1 << 30;

/// What would make pressing Play go wrong, checked quickly (no logs are read): Steam updating the game, mods in a game
/// with anti-cheat, the launch option mods need while Steam is open (Gantry can only write it with Steam closed),
/// missing core mods or loader, the same mod twice, a nearly full drive. Each has a `fix` when Gantry can fix it.
fn preflight(state: &SharedState, g: &GameEntry) -> Vec<Value> {
    let dir = PathBuf::from(&g.game_dir);
    let mut out: Vec<Value> = Vec::new();
    let upd = steam_update_of(g);
    if upd.phase == steamlib::Phase::Updating {
        out.push(issue("steam_updating", "bad", "", vec![upd.percent.map(|p| p.to_string()).unwrap_or_default()]));
    }
    let lib = library_of(g);
    let mods = lib.as_ref().map(|l| l.mods()).unwrap_or_default();
    let enabled = mods.iter().filter(|m| m.enabled).count();
    let ac = deploy::anti_cheat(&dir);
    if let Some(ac) = ac.filter(|_| enabled > 0) {
        out.push(issue("anti_cheat", "bad", "vanilla", vec![ac.to_string()]));
    }
    // with Steam closed, Play writes the launch option itself; with Steam open it cannot, and the mods would not load
    if !g.launches_directly() && enabled > 0 && steam_wanted(state, g) != "%command%" {
        let info = steam_info(state, g);
        if matches!(info.status.as_str(), "missing" | "stale") && info.steam_running {
            out.push(issue("launch_option", "bad", "steam", vec![]));
        }
    }
    if ac.is_none() {
        if let Some(i) = framework_issue(&dir, &mods) {
            out.push(i);
        }
        if enabled > 0 && loader_wanted(state, g).map(|w| !w.1).unwrap_or(false) {
            out.push(issue("needs_loader", "warn", "setup", vec![]));
        }
    }
    out.extend(duplicate_issues(&mods));
    out.extend(game_update_issue(state, g, &mods));
    if let Some(free) = fsutil::free_bytes(&dir).filter(|f| *f < LOW_DISK) {
        out.push(issue("low_disk", "warn", "", vec![fsutil::size_text(free)]));
    }
    let enabled_lib_ids: std::collections::HashSet<String> = mods.iter().filter(|m| m.enabled).map(|m| m.id.clone()).collect();
    let needs: Vec<String> = read_state(state).nexus.needs.iter()
        .filter(|n| n.appid == g.appid && enabled_lib_ids.contains(&n.lib_id))
        .flat_map(|n| n.missing.iter().map(|m| m.name.clone()))
        .collect();
    if !needs.is_empty() {
        out.push(issue("needs", "info", "", vec![needs.join(", ")]));
    }
    out.sort_by_key(|i| std::cmp::Reverse(level_rank(i["level"].as_str().unwrap_or(""))));
    out
}

/// Fix what the pre-flight found (as many of `fixes` as apply), then play. "vanilla" plays without mods instead.
/// The files of these mods as they land in the game.
fn files_in_game(mods: &[deploy::LibMod]) -> Vec<String> {
    mods.iter().filter(|m| m.enabled).flat_map(|m| m.files.iter().map(|f| if m.target.is_empty() { f.clone() } else { format!("{}/{}", m.target, f) })).collect()
}

/// Installs the core mods the enabled mods need and the game lacks, or only `first` (and then what it needs itself).
/// A core mod can need another (ArchiveXL lives in RED4ext's plugin folder), so it asks again after each round.
fn install_missing_cores(state: &SharedState, g: &GameEntry, first: Option<&str>) {
    let dir = PathBuf::from(&g.game_dir);
    // what the mods need; with `only`, what the core mods installed here need
    let wanted_by = |only: Option<&HashSet<&str>>| -> Vec<&'static str> {
        let mods: Vec<deploy::LibMod> = library_of(g).map(|l| l.mods()).unwrap_or_default().into_iter().filter(|m| only.is_none_or(|o| o.iter().any(|c| m.name.to_lowercase().contains(&c.to_lowercase())))).collect();
        frameworks::missing(&dir, &files_in_game(&mods)).iter().map(|m| m.name).collect()
    };
    let mut tried: HashSet<&str> = HashSet::new();
    for round in 0..3 {
        // with `first`, other core mods that were already missing are left for the user to decide
        let wanted: Vec<&str> = match first {
            Some(f) if round == 0 => wanted_by(None).into_iter().filter(|n| *n == f).collect(),
            Some(_) => wanted_by(Some(&tried)),
            None => wanted_by(None),
        };
        let wanted: Vec<&str> = wanted.into_iter().filter(|n| !tried.contains(n)).collect();
        if wanted.is_empty() {
            break;
        }
        for core in frameworks::cores(&dir).into_iter().filter(|c| wanted.contains(&c.name)) {
            tried.insert(core.name);
            match install_core(state, g, &core) {
                Ok(v) => log(state, "OK", format!("Installed {} {}.", core.name, v)),
                Err(e) => log(state, "WARN", e),
            }
        }
    }
    refresh_mods_shared(state);
}

fn fix_and_play(state: &SharedState, fixes: &[String]) {
    let Some(g) = selected_game(state).filter(|g| g.is_installed) else {
        return log(state, "WARN", "Select an installed game first.");
    };
    if fixes.iter().any(|f| f == "vanilla") {
        return play_without_mods(state);
    }
    if fixes.iter().any(|f| f == "setup") {
        install_missing_cores(state, &g, None);
        if let Some((kind, false)) = loader_wanted(state, &g) {
            loader_exclusive(state, &g, |st| {
                install_loader_inner(st, &g, kind, false);
            });
        }
    }
    if fixes.iter().any(|f| f == "steam") {
        if let Err(e) = apply_steam_options(state, &g, false, true) {
            // the game still starts; the line above the Play button keeps saying what is missing
            log(state, "WARN", e);
        }
    }
    play_game(state);
}

/// The answer for the game page: every issue, plus the loader's own state.
fn game_health(state: &SharedState, g: &GameEntry) -> Value {
    let (issues, mut v) = game_issues(state, g, true);
    if let Some(first) = issues.first() {
        v["code"] = first["code"].clone();
        v["level"] = first["level"].clone();
        v["fix"] = first["fix"].clone();
    } else if v["code"] == "none" {
        v["level"] = json!("none");
    }
    let updates = read_state(state).nexus.updates.iter().filter(|u| u.appid == g.appid).count();
    v["health"] = score::summary(&issues, updates);
    v["issues"] = json!(issues);
    v
}

/// Last `n` lines of a text file ("" when it is missing).
fn tail_text(p: &Path, n: usize) -> String {
    let Ok(b) = fs::read(p) else { return String::new() };
    let t = String::from_utf8_lossy(&b[b.len().saturating_sub(60_000)..]).to_string();
    let lines: Vec<&str> = t.lines().collect();
    lines[lines.len().saturating_sub(n)..].join("\n")
}

/// Everything someone helping with a problem would ask for, as plain text. Keys and tokens are
/// never included, and the user's home folder is shortened to `~`.
fn problem_report(state: &SharedState, g: &GameEntry) -> String {
    let (issues, health) = game_issues(state, g, true);
    let lib = library_of(g);
    let status = lib.as_ref().map(|l| l.status_json()).unwrap_or(Value::Null);
    let os = fs::read_to_string("/etc/os-release").ok().and_then(|t| t.lines().find_map(|l| l.strip_prefix("PRETTY_NAME=").map(|v| v.trim_matches('"').to_string()))).unwrap_or_default();
    let (proton, logs, nexus) = {
        let st = read_state(state);
        (
            st.protons.get(st.selected_proton_idx).map(|(n, _)| n.clone()).unwrap_or_default(),
            st.logs.iter().rev().take(40).map(|l| format!("[{}] {}", l.level, l.message)).collect::<Vec<_>>().into_iter().rev().collect::<Vec<_>>(),
            st.nexus.key.is_some(),
        )
    };
    let mut r = String::new();
    let mut line = |s: String| {
        r.push_str(&s);
        r.push('\n');
    };
    line(format!("Gantry {} on {} (kernel {})", env!("CARGO_PKG_VERSION"), os, fs::read_to_string("/proc/sys/kernel/osrelease").unwrap_or_default().trim()));
    line(format!("Proton: {}   Nexus connected: {}", proton, nexus));
    line(format!("Game: {} [{}] engine: {}  source: {}", g.name, g.appid, g.engine, if g.source.is_empty() { "steam/custom" } else { &g.source }));
    line(format!("Folder: {}", g.game_dir));
    line(format!("Loader: {}   running: {}", health["loader"].as_str().unwrap_or(""), health["running"]));
    let steam = if g.launches_directly() { "launched by Gantry".to_string() } else { let i = steam_info(state, g); format!("{} (current: '{}', wanted: '{}')", i.status, i.current, i.wanted) };
    line(format!("Steam launch options: {}", steam));
    line(format!("Library: {} file(s) deployed, drift {}, dirty {}", status["deployed"], status["drift"]["count"], status["dirty"]));
    line("Mods:".to_string());
    for m in lib.as_ref().map(|l| l.mods()).unwrap_or_default() {
        line(format!("  {} {} v{} {}{}", if m.enabled { "[on] " } else { "[off]" }, m.name, m.version, if m.target.is_empty() { String::new() } else { format!("-> {}/ ", m.target) }, if m.placed_by.is_empty() { String::new() } else { format!("(placed by {})", m.placed_by) }));
    }
    line("Issues:".to_string());
    for i in &issues {
        line(format!("  {} {} {}", i["level"].as_str().unwrap_or(""), i["code"].as_str().unwrap_or(""), i["args"]));
    }
    let loader_log = match health["loader"].as_str().unwrap_or("") {
        "melonloader" => dir_join(&g.game_dir, "MelonLoader/Latest.log"),
        "bepinex" => dir_join(&g.game_dir, "BepInEx/LogOutput.log"),
        _ => PathBuf::new(),
    };
    if !loader_log.as_os_str().is_empty() {
        line(format!("--- {} (last 30 lines)\n{}", loader_log.file_name().unwrap_or_default().to_string_lossy(), tail_text(&loader_log, 30)));
    }
    line("--- Gantry log (last 40)".to_string());
    for l in logs {
        line(l);
    }
    let home = home_dir().to_string_lossy().to_string();
    r.replace(&home, "~")
}

fn dir_join(dir: &str, rel: &str) -> PathBuf {
    Path::new(dir).join(rel)
}

/// Keep a quiet watch on every installed game: the sidebar shows a chip for the ones with trouble.
fn spawn_health_monitor(state: Weak<RwLock<AppState>>) {
    spawn_background(state, "health", "Health check of your games", Duration::from_secs(6), Duration::from_secs(25), Box::new(|st, _| {
        let games: Vec<GameEntry> = read_state(st).games.iter().filter(|g| g.is_installed).cloned().collect();
        let mut levels: std::collections::BTreeMap<String, String> = Default::default();
        for g in &games {
            let (issues, _) = game_issues(st, g, false);
            if let Some(w) = issues.iter().map(|i| i["level"].as_str().unwrap_or("")).filter(|l| matches!(*l, "bad" | "warn")).max_by_key(|l| level_rank(l)) {
                levels.insert(g.appid.clone(), w.to_string());
            }
        }
        {
            let mut s = write_state(st);
            if s.health != levels {
                s.health = levels;
                s.notifier.bump();
            }
        }
        report_repaired(st);
        Ok(true)
    }));
}

/// Apply the pin / notes / tags fields of a `/api/game_meta` request.
fn game_meta_apply(st: &mut AppState, appid: &str, body: &Value) {
    let appid = appid.to_string();
    if let Some(fav) = body.get("favorite").and_then(Value::as_bool) {
                st.config.favorites.retain(|a| a != &appid);
                if fav {
                    st.config.favorites.push(appid.clone());
                }
            }
            if body.get("notes").is_some() {
                let notes: String = body_str(body, "notes").chars().take(4000).collect();
                if notes.trim().is_empty() {
                    st.config.game_notes.remove(&appid);
                } else {
                    st.config.game_notes.insert(appid.clone(), notes);
                }
            }
            if body.get("tags").is_some() {
                let tags: Vec<String> = body_str(body, "tags")
                    .split(',')
                    .map(|t| t.trim().chars().take(30).collect::<String>())
                    .filter(|t| !t.is_empty())
                    .fold(Vec::new(), |mut v, t| {
                        if !v.iter().any(|x: &String| x.eq_ignore_ascii_case(&t)) {
                            v.push(t);
                        }
                        v
                    })
                    .into_iter()
                    .take(12)
                    .collect();
                if tags.is_empty() {
                    st.config.game_tags.remove(&appid);
                } else {
                    st.config.game_tags.insert(appid.clone(), tags);
                }
            }
            st.save_config();
            st.notifier.bump();
}

/// Find artwork by name for games that have none (Hydra, Heroic, custom...). Returns how many were found.
fn lookup_missing_covers(state: &SharedState) -> usize {
    let (todo, base, http) = {
        let st = read_state(state);
        let todo: Vec<(String, String)> = st
            .games
            .iter()
            .filter(|g| {
                let steam_like = !g.is_custom && g.source.is_empty() && g.appid.bytes().all(|b| b.is_ascii_digit());
                g.steam_appid.is_empty() && g.cover_id.is_empty() && !steam_like && !st.config.cover_appid.contains_key(&g.appid)
            })
            .map(|g| (g.appid.clone(), g.name.clone()))
            .collect();
        (todo, st.covers_base.clone(), st.covers_allow_http)
    };
    let mut found = 0;
    for (appid, name) in todo {
        match covers::lookup(&name, &base, http) {
            Ok(hit) => {
                let mut st = write_state(state);
                st.config.cover_appid.insert(appid.clone(), hit.map(|i| i.to_string()).unwrap_or_else(|| "0".into()));
                if let (Some(id), Some(g)) = (hit, st.games.iter_mut().find(|g| g.appid == appid)) {
                    g.cover_id = id.to_string();
                    found += 1;
                }
                st.save_config();
                st.notifier.bump();
            }
            Err(_) => break, // no connection: try again later
        }
        if !cfg!(test) {
            thread::sleep(Duration::from_millis(800));
        }
    }
    found
}

fn spawn_cover_lookup(state: Weak<RwLock<AppState>>) {
    spawn_background(state, "covers", "Looking up game covers", Duration::from_secs(10), Duration::from_secs(1800), Box::new(|st, _| {
        lookup_missing_covers(st);
        Ok(true)
    }));
}

/// The Nexus game domain of a game: chosen by hand, else matched by name against Nexus's game list.
fn nexus_domain_for(state: &SharedState, g: &GameEntry) -> Result<String, String> {
    if let Some(d) = read_state(state).config.nexus_domain.get(&g.appid) {
        return Ok(d.clone());
    }
    if let Some(d) = recipes::find(&read_state(state).recipes, &g.appid, g.recipe_steam_id()).map(|r| r.nexus.clone()).filter(|d| !d.is_empty()) {
        return Ok(d);
    }
    let client = nexus_client(state)?;
    let file = read_state(state).cache_root().join("nexus").join("games.json");
    let fresh = fs::metadata(&file).and_then(|m| m.modified()).ok().and_then(|t| t.elapsed().ok()).map(|a| a < Duration::from_secs(86_400)).unwrap_or(false);
    let list: Vec<(String, String)> = match fresh.then(|| fs::read(&file).ok().and_then(|b| serde_json::from_slice(&b).ok())).flatten() {
        Some(l) => l,
        None => match client.games() {
            Ok(l) => {
                if let Some(dir) = file.parent() {
                    let _ = fs::create_dir_all(dir);
                }
                let _ = fsutil::atomic_write(&file, serde_json::to_string(&l).unwrap_or_default());
                l
            }
            Err(e) => match fs::read(&file).ok().and_then(|b| serde_json::from_slice(&b).ok()) {
                Some(old) => old,
                None => return Err(e),
            },
        },
    };
    nexus::match_domain(&g.name, &list).ok_or_else(|| format!("Gantry could not tell which Nexus Mods game {} is.", g.name))
}

/// The file to take for "install this mod": the newest MAIN file, else the newest file.
fn nexus_main_file(client: &nexus::Nexus, game: &str, mod_id: u64) -> Result<u64, String> {
    let mf = client.mod_files(game, mod_id)?;
    mf.files.iter().filter(|f| f.category.eq_ignore_ascii_case("main")).max_by_key(|f| f.uploaded).or_else(|| mf.files.iter().max_by_key(|f| f.uploaded)).map(|f| f.file_id).ok_or_else(|| "That mod has no files to download.".to_string())
}

/// "Get this mod" from the browse list: installed for Premium accounts, otherwise its page opens.
fn nexus_get_mod(state: &SharedState, mod_id: u64, name: &str) {
    let name = if name.trim().is_empty() { format!("mod {}", mod_id) } else { name.trim().to_string() };
    let Some(g) = selected_game(state).filter(|g| g.is_installed) else { return };
    let domain = match nexus_domain_for(state, &g) {
        Ok(d) => d,
        Err(e) => return log(state, "ERR", e),
    };
    let client = match nexus_client(state) {
        Ok(c) => c,
        Err(e) => return log(state, "ERR", e),
    };
    let premium = read_state(state).nexus.user.as_ref().map(|u| u.premium).unwrap_or(false);
    if !premium {
        // a free account downloads through the website: Gantry opens the exact page and installs what arrives
        {
            let mut st = write_state(state);
            st.nexus.wants.retain(|w| !(w.game == domain && w.mod_id == mod_id));
            st.nexus.wants.insert(0, Want { appid: g.appid.clone(), game: domain.clone(), mod_id, name: name.clone(), asked_ms: now_ms(), file_id: 0 });
            st.notifier.bump();
        }
        open_nexus_files_page(state, &domain, mod_id);
        return log(state, "INFO", format!("Opened the Nexus page of {}. Press its Mod Manager Download button; ModForge installs it by itself.", name));
    }
    match nexus_main_file(&client, &domain, mod_id) {
        Ok(file_id) => nexus_fetch_and_install(state, &g, &nexus::NxmLink { game: domain, mod_id, file_id, key: None, expires: None }, None),
        Err(e) => log(state, "ERR", e),
    }
}

/// Premium: fetch and install every mod a library mod needs that is still missing.
fn nexus_install_needs(state: &SharedState, appid: &str, lib_id: &str) {
    let (row, premium) = {
        let st = read_state(state);
        (
            st.nexus.needs.iter().find(|n| n.appid == appid && n.lib_id == lib_id).cloned(),
            st.nexus.user.as_ref().map(|u| u.premium).unwrap_or(false),
        )
    };
    let (Some(row), Some(g)) = (row, game_by_appid(state, appid)) else { return };
    let parse = |url: &str| {
        let stripped = url
            .strip_prefix("https://www.nexusmods.com/")
            .or_else(|| url.strip_prefix("https://nexusmods.com/"))
            .or_else(|| url.strip_prefix("http://www.nexusmods.com/"))
            .or_else(|| url.strip_prefix("http://nexusmods.com/"))?;
        let (g, i) = stripped.split_once("/mods/")?;
        Some((g.to_string(), i.split(['?', '/']).next()?.parse::<u64>().ok()?))
    };
    if !premium {
        // a free account cannot download without the website: the pages open one after the other and each download is installed on arrival
        let mut queued = 0;
        {
            let mut st = write_state(state);
            for r in &row.missing {
                let Some((game, mod_id)) = parse(&r.page_url) else { continue };
                if st.nexus.queue.iter().chain(st.nexus.wants.iter()).any(|w| w.game == game && w.mod_id == mod_id) {
                    continue;
                }
                st.nexus.queue.push_back(Want { appid: appid.to_string(), game, mod_id, name: r.name.clone(), asked_ms: 0, file_id: 0 });
                queued += 1;
            }
            queue_counted(&mut st, queued);
        }
        if queued == 0 {
            return log(state, "WARN", "Nothing to open: what is missing has no Nexus page, or is already being waited for.");
        }
        return nexus_queue_next(state);
    }
    let client = match nexus_client(state) {
        Ok(c) => c,
        Err(e) => return log(state, "ERR", e),
    };
    for r in &row.missing {
        // https://www.nexusmods.com/<game>/mods/<id>
        let Some((game, id)) = r.page_url.strip_prefix("https://www.nexusmods.com/").and_then(|p| p.split_once("/mods/")).and_then(|(g, i)| Some((g.to_string(), i.split(['?', '/']).next()?.parse::<u64>().ok()?))) else { continue };
        match nexus_main_file(&client, &game, id) {
            Ok(file_id) => nexus_fetch_and_install(state, &g, &nexus::NxmLink { game, mod_id: id, file_id, key: None, expires: None }, None),
            Err(e) => log(state, "ERR", format!("{}: {}", r.name, e)),
        }
    }
}

/// Gantry's own home on GitHub.
const SELF_REPO: (&str, &str) = ("pigeoonn", "gantry");

/// One of Gantry's own help pages on GitHub. Only these few names exist; anything else opens nothing.
fn docs_url(page: &str) -> Option<String> {
    let file = match page {
        "guide" => "GUIDE.md",
        "faq" => "FAQ.md",
        "reference" => "REFERENCE.md",
        "tutorial" => "TUTORIAL.md",
        "install" => "INSTALL.md",
        _ => return None,
    };
    Some(format!("https://github.com/{}/{}/blob/main/docs/{}", SELF_REPO.0, SELF_REPO.1, file))
}

/// The address of Gantry's issue form with what the form asks first already filled in: the version and system for a
/// bug, the game for a game request. Nothing else leaves the computer, and the person still has to press Submit.
fn issue_url(game_request: bool, system: &str, game: &str) -> String {
    let (template, field, value) = if game_request { ("game.yml", "game", game) } else { ("bug.yml", "version", system) };
    format!("https://github.com/{}/{}/issues/new?template={}&{}={}", SELF_REPO.0, SELF_REPO.1, template, field, fsutil::url_encode(value))
}

/// Is there a newer Gantry than the one running? `(newest tag, its page)` when there is.
/// Betas are marked as pre-releases, and GitHub's "latest release" ignores those, so the whole list is read.
fn newer_gantry(state: &SharedState) -> Option<(String, String)> {
    let api = read_state(state).github_api.clone();
    let get = |path: String| -> Option<Value> {
        let out = Command::new("curl")
            .args(["-sSL", "--max-time", "20", "-H", "Accept: application/vnd.github+json", "-H", "User-Agent: Gantry"])
            .arg(format!("{}/repos/{}/{}/{}", api, SELF_REPO.0, SELF_REPO.1, path))
            .stdin(Stdio::null())
            .output()
            .ok()?;
        serde_json::from_slice::<Value>(&out.stdout).ok()
    };
    let mut releases: Vec<Value> = match get("releases?per_page=10".into()) {
        Some(Value::Array(a)) => a,
        _ => Vec::new(),
    };
    if releases.is_empty() {
        releases.extend(get("releases/latest".into()));
    }
    let mut best: Option<(String, String)> = None;
    for r in releases.iter().filter(|r| r.get("draft").and_then(Value::as_bool) != Some(true)) {
        let Some(tag) = r.get("tag_name").and_then(Value::as_str) else { continue };
        let url = r.get("html_url").and_then(Value::as_str).unwrap_or("");
        let v = tag.trim_start_matches('v');
        if url.starts_with("https://github.com/") && thunderstore::version_newer(v, env!("CARGO_PKG_VERSION")) && best.as_ref().map(|(b, _)| thunderstore::version_newer(v, b.trim_start_matches('v'))).unwrap_or(true) {
            best = Some((tag.to_string(), url.to_string()));
        }
    }
    best
}

/// Put the newest Gantry in place of this one (AppImage or Windows build), after the person said yes. The file comes
/// from Gantry's own release, its checksum has to match, and the running copy is only swapped when all of it is in hand.
fn self_update(state: &SharedState) {
    let (target, tag, api, allow) = {
        let st = read_state(state);
        (st.self_target.clone(), st.new_version.clone(), st.github_api.clone(), st.github_allow.clone())
    };
    let Some(target) = target else {
        return log(state, "WARN", "This copy of ModForge cannot update itself. Update it the way you installed it, or download the new version.");
    };
    let Some(tag) = tag.filter(|t| !t.is_empty() && t.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'-')) else {
        return log(state, "INFO", "There is no newer ModForge to put in place. Use Check for a newer ModForge first.");
    };
    let curl = |url: &str| -> Option<Vec<u8>> {
        let out = Command::new("curl").args(["-fsSL", "--max-time", "60", "-H", "Accept: application/vnd.github+json", "-H", "User-Agent: ModForge"]).arg(url).stdin(Stdio::null()).output().ok()?;
        out.status.success().then_some(out.stdout)
    };
    let release = curl(&format!("{}/repos/{}/{}/releases/tags/{}", api, SELF_REPO.0, SELF_REPO.1, tag)).and_then(|b| serde_json::from_slice::<Value>(&b).ok());
    let picked = release.as_ref().and_then(|r| selfupdate::pick_asset(&r["assets"], cfg!(windows), std::env::consts::ARCH));
    let Some((name, url, sum_url)) = picked else {
        return log(state, "WARN", format!("ModForge {} has no file for this kind of install (or no checksum), so it cannot be put in place. Download it from the release page.", tag));
    };
    if ![&url, &sum_url].iter().all(|u| github::download_ok(u, SELF_REPO.0, SELF_REPO.1, allow.as_deref())) || name.contains("..") || name.contains('/') || name.contains('\\') {
        return log(state, "ERR", "Refusing a download that is not from ModForge's own releases.");
    }
    let dir = read_state(state).cache_root().join("drops").join(fsutil::unique_token());
    let dest = dir.join(&name);
    log(state, "INFO", format!("Downloading ModForge {}...", tag));
    let got = fs::create_dir_all(&dir).is_ok()
        && Command::new("curl")
            .args(["-fL", "--retry", "2", "--connect-timeout", "15", "--max-time", "1800", "-sS", "-o"])
            .arg(&dest)
            .arg(&url)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
    let expected = curl(&sum_url).and_then(|b| selfupdate::expected_sum(&String::from_utf8_lossy(&b), &name));
    let finish = |msg: &str| {
        let _ = fs::remove_dir_all(&dir);
        log(state, "ERR", msg.to_string());
    };
    if !got || fs::metadata(&dest).map(|m| m.len()).unwrap_or(0) == 0 {
        return finish("The download failed (check your connection).");
    }
    let Some(expected) = expected else {
        return finish("The checksum file of that release could not be read, so nothing was changed.");
    };
    if hashing::sha256_file(&dest).map(|h| h != expected).unwrap_or(true) {
        return finish("The downloaded file does not match its checksum, so nothing was changed.");
    }
    // the Windows build comes in a zip with the program inside
    let new = if cfg!(windows) {
        let out = dir.join("unpacked");
        let unpacked = fs::create_dir_all(&out).is_ok() && platform::extract_commands(&dest, &out).into_iter().any(|mut c| c.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).status().map(|s| s.success()).unwrap_or(false));
        if unpacked { selfupdate::find_named(&out, "rig.exe") } else { None }
    } else {
        Some(dest.clone())
    };
    let result = match new {
        Some(n) => selfupdate::swap_in(&target, &n).map_err(|e| e.to_string()),
        None => Err("the program was not in the download".to_string()),
    };
    let _ = fs::remove_dir_all(&dir);
    match result {
        Ok(()) => {
            {
                let mut st = write_state(state);
                st.update_ready = Some(tag.clone());
                st.notifier.bump();
            }
            log(state, "OK", format!("ModForge {} is in place. Close ModForge and open it again to use it.", tag));
        }
        Err(e) => log(state, "ERR", format!("Could not replace this copy of ModForge: {}", e)),
    }
}

/// Check for a newer Gantry once a day and say so once per version. Quiet when GitHub does not answer.
fn spawn_self_update_check(state: Weak<RwLock<AppState>>) {
    let mut told = String::new();
    spawn_background(state, "gantry_update", "Looking for a newer ModForge", Duration::from_secs(40), Duration::from_secs(24 * 3600), Box::new(move |st, _| {
        if let Some((tag, _)) = newer_gantry(st) {
            write_state(st).new_version = Some(tag.clone());
            if tag != told {
                told = tag.clone();
                let lang = read_state(st).config.language.clone();
                platform::notify(&i18n::tr(&lang, "A new version of Gantry is available"), &tag);
            }
            write_state(st).notifier.bump();
        }
        Ok(true)
    }));
}

/// Download one file of a GitHub release and add it to the selected game's library.
fn github_install(state: &SharedState, repo: &str, url: &str) {
    let Some(g) = selected_game(state).filter(|g| g.is_installed) else {
        return log(state, "WARN", "Select an installed game first.");
    };
    github_install_named(state, &g, repo, url, github::GithubRef::from_download(url), None);
}

/// Download one file of a GitHub release into `g`'s library. A file of the same repository that is already there is
/// replaced (that is an update), and the mod remembers where it came from so it can be checked for newer releases.
fn github_install_for(state: &SharedState, g: &GameEntry, repo: &str, url: &str, reference: Option<github::GithubRef>) {
    github_install_named(state, g, repo, url, reference, None)
}

/// The same, under a name of Gantry's choosing (the core mods of a game: "RED4ext", not "red4ext-1.30.0").
fn github_install_named(state: &SharedState, g: &GameEntry, repo: &str, url: &str, reference: Option<github::GithubRef>, label: Option<&str>) {
    let Some((o, r)) = github::parse_repo(repo) else {
        return log(state, "ERR", "That does not look like a GitHub repository.");
    };
    let allow = read_state(state).github_allow.clone();
    if !github::download_ok(url, &o, &r, allow.as_deref()) {
        return log(state, "ERR", "Refusing a download that is not from that repository's releases.");
    }
    let raw_name = url.split('?').next().unwrap_or(url).rsplit('/').next().unwrap_or("download.zip");
    let name_decoded = fsutil::url_decode(raw_name);
    let name = name_decoded.as_str();
    if name.is_empty() || name.contains("..") || name.contains('\\') {
        return log(state, "ERR", "That file name is not allowed.");
    }
    let dir = read_state(state).cache_root().join("drops").join(fsutil::unique_token());
    let dest = dir.join(name);
    log(state, "INFO", format!("Downloading {} from {}/{}...", name, o, r));
    let ok = fs::create_dir_all(&dir).is_ok()
        && Command::new("curl")
            .args(["-fL", "--retry", "2", "--connect-timeout", "15", "--max-time", "1800", "-sS", "-o"])
            .arg(&dest)
            .arg(url)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
    if !ok || fs::metadata(&dest).map(|m| m.len()).unwrap_or(0) == 0 {
        let _ = fs::remove_dir_all(&dir);
        return log(state, "ERR", "The download failed (check your connection).");
    }
    // a file that is not under github.com/<owner>/<repo>/releases/download (a mirror) simply is not tracked
    let replaces = reference.as_ref().and_then(|new| library_of(g)?.mods().into_iter().find(|m| m.github.as_ref().map(|old| old.same_file_as(new)).unwrap_or(false)).map(|m| m.id));
    let version = reference.as_ref().map(|r| r.tag.trim_start_matches('v').to_string()).unwrap_or_default();
    install_routed(dest, true, Arc::clone(state), InstallOpts { game: Some(g.clone()), to_library: true, github: reference, replaces, version, name: label.map(str::to_string), ..Default::default() });
}

/// What the compatibility window shows for the selected game.
fn all_tools(state: &SharedState) -> Vec<compat::Tool> {
    match read_state(state).tools_dirs.clone() {
        Some(dirs) => compat::tools_in(&[], &dirs),
        None => compat::tools(),
    }
}

fn compat_info(state: &SharedState, g: &GameEntry) -> Value {
    let tools = all_tools(state);
    let steam_game = !g.launches_directly();
    let (current, steam_running) = {
        let env = read_state(state).steam_env.clone();
        let running = env.running();
        if steam_game {
            let name = env.config_vdf().and_then(|p| fs::read_to_string(p).ok()).and_then(|t| steam::get_compat_tool(&t, &g.appid).ok().flatten()).unwrap_or_default();
            (name, running)
        } else {
            let st = read_state(state);
            (st.proton_for(g).map(|p| p.to_string_lossy().to_string()).unwrap_or_default(), running)
        }
    };
    let presets: Vec<Value> = compat::PRESETS.iter().map(|(id, line, text)| json!({ "id": id, "line": line, "text": text, "on": compat::preset_on(&g.env_vars, line) })).collect();
    json!({ "kind": if steam_game { "steam" } else { "direct" }, "steam_running": steam_running, "tools": tools, "current": current, "presets": presets })
}

/// Give the selected game another Proton: through Steam's settings for Steam games (Steam closed),
/// or Gantry's own choice for games it starts itself.
fn set_proton(state: &SharedState, tool: &str) {
    let Some(g) = selected_game(state) else { return };
    let tools = all_tools(state);
    if g.launches_directly() {
        let Some(t) = tools.iter().find(|t| t.script.to_string_lossy() == tool) else {
            return log(state, "WARN", "That Proton is no longer installed.");
        };
        guard_saves(state, &g, "before-tool");
        let mut st = write_state(state);
        st.config.proton_choice.insert(g.appid.clone(), t.script.to_string_lossy().to_string());
        st.save_config();
        st.notifier.bump();
        st.log("OK", format!("{} will start with {}.", g.name, t.display));
        return;
    }
    let Some(t) = tools.iter().find(|t| t.internal == tool) else {
        return log(state, "WARN", "That Proton is no longer installed.");
    };
    let env = read_state(state).steam_env.clone();
    if env.running() {
        return log(state, "WARN", "Steam is running, so its settings could not be changed. Close Steam and try again.");
    }
    let Some(cfg) = env.config_vdf() else {
        return log(state, "ERR", "Steam's config.vdf was not found.");
    };
    guard_saves(state, &g, "before-tool");
    let backups = read_state(state).cache_root().join("steam-backups");
    match steam::apply_compat_tool(&cfg, &g.appid, &t.internal, false, &backups) {
        Ok(()) => log(state, "OK", format!("{} will start with {} the next time Steam runs it.", g.name, t.display)),
        Err(e) => log(state, "ERR", e),
    }
}

/// Fetch the newest GE-Proton into Steam's `compatibilitytools.d` and refresh the Proton list.
fn download_ge(state: &SharedState) {
    let (src, cache) = {
        let st = read_state(state);
        (st.ge_source.clone(), st.cache_root().join("downloads"))
    };
    let rel = match compat::latest_ge(&src) {
        Ok(r) => r,
        Err(e) => return log(state, "ERR", e),
    };
    let dir = read_state(state).tools_dirs.as_ref().and_then(|d| d.first().cloned()).unwrap_or_else(|| home_dir().join(".local/share/Steam/compatibilitytools.d"));
    match compat::install_ge(&rel, &dir, &cache, &|m| log(state, "INFO", m)) {
        Ok(_) => {
            let protons = RigEngine::find_protons();
            let mut st = write_state(state);
            st.protons = protons;
            st.notifier.bump();
            st.log("OK", format!("{} is installed. Choose it for a game in Compatibility; Steam must be restarted to list it.", rel.tag));
        }
        Err(e) => log(state, "ERR", e),
    }
}

/// Every game back to its original files, in one go (mods stay in their libraries).
fn purge_all(state: &SharedState) {
    if read_state(state).is_running.load(Ordering::SeqCst) {
        return log(state, "WARN", "Close the game first.");
    }
    let games: Vec<GameEntry> = read_state(state).games.iter().filter(|g| g.is_installed).cloned().collect();
    let (mut games_done, mut files) = (0, 0);
    for g in &games {
        let Some(lib) = library_of(g) else { continue };
        match lib.purge() {
            Ok(r) if r.removed + r.restored > 0 => {
                games_done += 1;
                files += r.removed;
                log(state, "INFO", format!("{} is back to its original files.", g.name));
            }
            Ok(_) => {}
            Err(e) => log(state, "ERR", format!("{}: {}", g.name, e)),
        }
    }
    log(state, "OK", format!("{} game(s) restored, {} mod file(s) removed.", games_done, files));
    refresh_mods_shared(state);
}

/// Download a Unity mod loader that fits the selected game and put it in through the mod library,
/// so "Back to vanilla" removes it again.
fn install_loader(state: &SharedState, kind: &str) {
    let Some(g) = selected_game(state).filter(|g| g.is_installed) else {
        return log(state, "WARN", "Select an installed game first.");
    };
    loader_exclusive(state, &g, |s| {
        install_loader_inner(s, &g, kind, false);
    });
}

/// A second click while a loader is being installed or switched must not start a second one
/// (that put the same loader into one library twice).
/// Libraries (by folder) whose mod loader is being installed or swapped right now.
static LOADER_BUSY: std::sync::LazyLock<Mutex<HashSet<String>>> = std::sync::LazyLock::new(|| Mutex::new(HashSet::new()));

fn loader_exclusive(state: &SharedState, g: &GameEntry, f: impl FnOnce(&SharedState)) {
    use LOADER_BUSY as BUSY;
    // one at a time per library (the library's folder tells libraries apart, game ids can repeat)
    let Some(appid) = library_of(g).map(|l| l.root().to_string_lossy().to_string()) else { return };
    if !BUSY.lock().unwrap_or_else(|e| e.into_inner()).insert(appid.clone()) {
        return log(state, "INFO", "A loader change is already running for this game.");
    }
    struct Done(String);
    impl Drop for Done {
        fn drop(&mut self) {
            BUSY.lock().unwrap_or_else(|e| e.into_inner()).remove(&self.0);
        }
    }
    let _done = Done(appid);
    f(state);
}

/// Replace the mod loader of the selected game by another one: the old one goes out through the
/// library (and its generated `MelonLoader` folder, which belongs to nobody else), then the new one in.
fn switch_loader(state: &SharedState, kind: &str) {
    let Some(g) = selected_game(state).filter(|g| g.is_installed) else {
        return log(state, "WARN", "Select an installed game first.");
    };
    loader_exclusive(state, &g, |s| switch_loader_inner(s, &g, kind));
}

fn switch_loader_inner(state: &SharedState, g: &GameEntry, kind: &str) {
    if read_state(state).is_running.load(Ordering::SeqCst) {
        return log(state, "WARN", "Close the game first.");
    }
    let Some(lib) = library_of(g) else { return };
    let labels = ["BepInEx 5", "BepInEx 6", "MelonLoader"];
    for m in lib.mods().into_iter().filter(|m| labels.contains(&m.name.as_str())) {
        if let Err(e) = lib.remove(&m.id) {
            return log(state, "ERR", e);
        }
    }
    let ml = Path::new(&g.game_dir).join("MelonLoader");
    if ml.is_dir() && fs::symlink_metadata(&ml).map(|m| !m.file_type().is_symlink()).unwrap_or(false) {
        let _ = fs::remove_dir_all(&ml);
    }
    install_loader_inner(state, g, kind, true);
}

/// Returns whether the loader went in.
fn install_loader_inner(state: &SharedState, g: &GameEntry, kind: &str, switching: bool) -> bool {
    let Some(loader) = loaders::Loader::parse(kind) else { return false };
    let Some(info) = loaders::detect(Path::new(&g.game_dir)) else {
        log(state, "WARN", format!("{} does not look like a Unity game.", g.name));
        return false;
    };
    if !info.loader.is_empty() && !switching {
        log(state, "INFO", format!("{} already has a mod loader.", g.name));
        return true;
    }
    if let Some(ac) = deploy::anti_cheat(Path::new(&g.game_dir)) {
        log(state, "WARN", format!("This game uses {ac}. Mod loaders can get your account banned, so ModForge will not install one."));
        return false;
    }
    if read_state(state).is_running.load(Ordering::SeqCst) {
        log(state, "WARN", "Close the game first.");
        return false;
    }
    let (cache, src) = {
        let st = read_state(state);
        (st.paths.cache_dir.join("loaders"), st.loader_sources.clone())
    };
    let asset = match loaders::find_asset(loader, &info, &src) {
        Ok(a) => a,
        Err(e) => {
            log(state, "ERR", e);
            return false;
        }
    };
    let zip = match loaders::ensure_zip(&cache, &asset, &|m| log(state, "INFO", m)) {
        Ok(z) => z,
        Err(e) => {
            log(state, "ERR", e);
            return false;
        }
    };
    // the same loader in the same version is never added twice
    if library_of(g).map(|l| l.mods().iter().any(|m| m.name == loader.label() && m.version == asset.version)).unwrap_or(false) {
        log(state, "INFO", format!("{} {} is already in the library.", loader.label(), asset.version));
        return true;
    }
    let opts = InstallOpts { game: Some(g.clone()), to_library: true, name: Some(loader.label().to_string()), version: asset.version.clone(), ..Default::default() };
    install_routed(zip, false, Arc::clone(state), opts);
    let ok = loaders::detect(Path::new(&g.game_dir)).map(|i| !i.loader.is_empty()).unwrap_or(false);
    if ok {
        log(state, "OK", format!("{} is installed. Start the game once so it can finish setting itself up, then add mods.", loader.label()));
    }
    ok
}

/// After a Nexus mod went in: tell the user what it needs that the library does not have, and
/// forget needs this mod just satisfied. Network trouble is not worth an error here.
fn note_requirements(state: &SharedState, g: &GameEntry, lib: &deploy::Library, m: &deploy::LibMod, nx: &nexus::NexusRef) {
    let Ok(client) = nexus_client(state) else { return };
    let reqs = client.requirements(&nx.game, nx.mod_id).unwrap_or_default();
    let missing = missing_requirements(&reqs, &lib.mods());
    let this_page = format!("https://www.nexusmods.com/{}/mods/{}", nx.game, nx.mod_id);
    let mut st = write_state(state);
    for n in st.nexus.needs.iter_mut().filter(|n| n.appid == g.appid) {
        n.missing.retain(|r| r.page_url != this_page);
    }
    st.nexus.needs.retain(|n| !n.missing.is_empty() && !(n.appid == g.appid && n.lib_id == m.id));
    if !missing.is_empty() {
        let names = missing.iter().map(|r| r.name.clone()).collect::<Vec<_>>().join(", ");
        st.log("WARN", format!("{} needs {} (not in your library yet).", m.name, names));
        st.nexus.needs.push(NeedRow { appid: g.appid.clone(), lib_id: m.id.clone(), name: m.name.clone(), missing });
    }
    st.notifier.bump();
}

const ARCHIVE_EXTS: &[&str] = &["zip", "7z", "rar", "tar", "gz", "tgz", "xz", "bz2", "zst"];

/// Extract (or wrap) a dropped file and add it to the game's mod library, then deploy.
/// Returns false (and touches nothing) when the mod turned out to need UE4SS routing instead.
fn add_to_library(state: &SharedState, g: &GameEntry, p: &Path, consume: bool, fname: &str, opts: &InstallOpts) -> bool {
    let Some(lib) = library_of(g) else {
        log(state, "WARN", format!("{} is not installed; nothing was added.", g.name));
        return true;
    };
    let stage = lib.root().join("incoming").join(fsutil::unique_token());
    if let Err(e) = fs::create_dir_all(&stage) {
        log(state, "ERR", format!("Could not create a staging folder: {e}"));
        return true;
    }
    let _guard = DirGuard(stage.clone());
    let ext = p.extension().map(|e| e.to_string_lossy().to_lowercase()).unwrap_or_default();
    let is_archive = ARCHIVE_EXTS.contains(&ext.as_str());
    if is_archive {
        log(state, "INFO", format!("Extracting archive: {}...", fname));
        if let Err(e) = ensure_room(p, &stage) {
            log(state, "ERR", e);
            return true;
        }
        if !extract_archive(p, &stage) {
            log(state, "ERR", format!("Failed to extract '{}' (need 7z, bsdtar, unzip or unrar installed).", fname));
            return true;
        }
        if opts.ue_guard && needs_ue4ss(&stage) {
            return false;
        }
    } else if let Err(e) = place_file(p, &stage.join(fname), consume) {
        log(state, "ERR", format!("Could not read '{}': {}", fname, e));
        return true;
    }
    // an installer script asks its questions before anything else happens to the files
    if is_archive {
        if let Some((script, root)) = fomod::find_module(&stage) {
            match fomod::load(&script) {
                Ok(module) => return start_fomod(state, g, lib, stage, root, module, _guard, fname, opts),
                Err(e) => log(state, "WARN", format!("'{}' has an installer script Gantry cannot follow ({}); adding its files as they are.", fname, e)),
            }
        }
    }
    add_staged(state, g, lib, stage, _guard, fname, opts)
}

/// The rest of adding a mod once its files are in a staging folder.
fn add_staged(state: &SharedState, g: &GameEntry, lib: deploy::Library, stage: PathBuf, guard: DirGuard, fname: &str, opts: &InstallOpts) -> bool {
    let _guard = guard;
    // what the author wrote on the Nexus page is kept (and read for install hints) like a readme
    let mut lib = lib;
    if let Some(nx) = opts.nexus.as_ref().filter(|n| n.valid()) {
        if let Ok(client) = nexus_client(state) {
            if let Ok(page) = client.mod_page(&nx.game, nx.mod_id) {
                let file = client.mod_files(&nx.game, nx.mod_id).ok().and_then(|mf| mf.files.into_iter().find(|f| f.file_id == nx.file_id));
                lib = lib.with_page_note(nexus::page_notes(&page, file.as_ref()));
            }
        }
    }
    let (mut name, file_version) = deploy::friendly_name(fname);
    if let Some(n) = &opts.name {
        name = n.clone();
    }
    let version = if opts.version.is_empty() { file_version } else { opts.version.clone() };
    // an update keeps the name it had; if the old one vanished meanwhile it is added as a new mod
    let old = opts.replaces.as_ref().and_then(|id| lib.mods().into_iter().find(|m| &m.id == id));
    if let Some(o) = &old {
        name = o.name.clone();
    }
    // when Gantry has to guess where the files go, a mod the user added by hand waits for an OK
    let by_hand = opts.nexus.is_none() && opts.thunder.is_none() && opts.replaces.is_none() && opts.name.is_none() && opts.target.is_none();
    if by_hand && !read_state(state).config.no_install_preview {
        if let Ok(pv) = lib.preview(&stage) {
            // the user already said where a mod that looks like this one goes
            let rule = (!pv.placed_by.is_empty() && !pv.shape.is_empty()).then(|| read_state(state).config.place_rules.get(&g.appid).and_then(|r| r.get(&pv.shape)).cloned()).flatten();
            if let Some(rule) = rule {
                let target = rule.strip_prefix("dir:").map(str::to_string);
                let shown = match target.as_deref() {
                    Some("") => ".".to_string(),
                    Some(t) => t.to_string(),
                    None if pv.target.is_empty() => ".".to_string(),
                    None => pv.target.clone(),
                };
                log(state, "INFO", format!("'{}' goes where you put a mod like it before ({}).", name, shown));
                finish_library_add(state, g, &lib, &stage, &name, fname, &version, opts, old, target.as_deref().or(opts.target.as_deref()));
                return true;
            }
            if !pv.placed_by.is_empty() {
                log(state, "INFO", format!("'{}' is waiting for your OK on where its files go.", name));
                let mut st = write_state(state);
                let item = PendingInstall { appid: g.appid.clone(), stage: stage.clone(), name, fname: fname.to_string(), version, opts_nexus: opts.nexus.clone(), preview: pv };
                // one question at a time: the others wait behind it
                if st.pending_install.is_none() {
                    st.pending_install = Some(item);
                } else {
                    st.pending_queue.push_back(item);
                }
                st.notifier.bump();
                std::mem::forget(_guard); // the files stay until the user answers
                return true;
            }
        }
    }
    finish_library_add(state, g, &lib, &stage, &name, fname, &version, opts, old, opts.target.as_deref());
    true
}

/// What an installer script may ask about the game: is this file there (in the game or in a mod)?
fn fomod_world(g: &GameEntry, lib: &deploy::Library) -> impl Fn(&str) -> bool {
    let game = PathBuf::from(&g.game_dir);
    let from_mods: HashSet<String> = lib.mods().into_iter().filter(|m| m.enabled).flat_map(|m| m.files.into_iter().map(|f| f.to_lowercase())).collect();
    move |path: &str| {
        let p = path.trim_start_matches('/');
        fomod::resolve_ci(&game, p).map(|x| x.exists()).unwrap_or(false)
            || fomod::resolve_ci(&game.join("Data"), p).map(|x| x.exists()).unwrap_or(false)
            || from_mods.contains(&p.to_lowercase())
            || from_mods.iter().any(|f| f.ends_with(&format!("/{}", p.to_lowercase())))
    }
}

/// An archive with an installer script: use the answers kept from an earlier install when there are
/// some, otherwise wait for the user's.
#[allow(clippy::too_many_arguments)]
fn start_fomod(state: &SharedState, g: &GameEntry, lib: deploy::Library, stage: PathBuf, root: PathBuf, module: fomod::Module, guard: DirGuard, fname: &str, opts: &InstallOpts) -> bool {
    let world = fomod_world(g, &lib);
    let remembered: Option<fomod::Selection> = opts
        .replaces
        .as_ref()
        .and_then(|id| lib.mods().into_iter().find(|m| &m.id == id))
        .and_then(|m| m.fomod)
        .filter(|v| v.get("module").and_then(Value::as_str) == Some(module.name.as_str()))
        .and_then(|v| serde_json::from_value(v.get("selection").cloned()?).ok());
    let pending = PendingFomod { appid: g.appid.clone(), stage: stage.clone(), root, module, fname: fname.to_string(), opts: opts.clone() };
    if let Some(sel) = remembered {
        if fomod::complete(&fomod::resolve(&pending.module, &sel, &world)) {
            log(state, "INFO", format!("Using the answers you gave the installer of '{}' last time.", fname));
            std::mem::forget(guard);
            fomod_continue(state, pending, sel);
            return true;
        }
    }
    // no questions at all: the script only installs fixed files
    if pending.module.steps.is_empty() {
        std::mem::forget(guard);
        fomod_continue(state, pending, fomod::Selection::new());
        return true;
    }
    log(state, "INFO", format!("'{}' has an installer with choices: it is waiting for your answers.", fname));
    {
        let mut st = write_state(state);
        st.pending_fomod.push(pending);
        st.notifier.bump();
    }
    std::mem::forget(guard); // the files stay until the user answers
    true
}

/// Installer scripts of Bethesda-style games name their files relative to the game's `Data` folder.
/// When the game has one and what the script produced looks like Data content, that is where it goes.
fn data_relative_target(g: &GameEntry, out: &Path) -> Option<String> {
    let data = fs::read_dir(&g.game_dir).ok()?.flatten().find(|e| e.path().is_dir() && e.file_name().to_string_lossy().eq_ignore_ascii_case("data"))?.file_name().to_string_lossy().to_string();
    const FOLDERS: &[&str] = &["meshes", "textures", "sound", "scripts", "interface", "music", "strings", "skse", "f4se", "grass", "video", "facegen", "lodsettings", "shadersfx", "seq", "source", "sky", "calientetools", "netscriptframework", "fose", "nvse", "obse"];
    const FILES: &[&str] = &["esp", "esm", "esl", "bsa", "ba2", "bsl"];
    let looks_like_data = fs::read_dir(out).ok()?.flatten().any(|e| {
        let name = e.file_name().to_string_lossy().to_lowercase();
        if e.path().is_dir() {
            FOLDERS.contains(&name.as_str())
        } else {
            name.rsplit('.').next().map(|x| FILES.contains(&x)).unwrap_or(false)
        }
    });
    let already_inside = fs::read_dir(out).ok()?.flatten().any(|e| e.path().is_dir() && e.file_name().to_string_lossy().eq_ignore_ascii_case("data"));
    (looks_like_data && !already_inside).then_some(data)
}

/// Lay out the files the answers lead to and add them to the library like any other mod.
fn fomod_continue(state: &SharedState, p: PendingFomod, selection: fomod::Selection) {
    let _extracted = DirGuard(p.stage.clone());
    let Some(g) = game_by_appid(state, &p.appid) else { return };
    let Some(lib) = library_of(&g) else { return };
    let world = fomod_world(&g, &lib);
    let resolved = fomod::resolve(&p.module, &selection, &world);
    if !fomod::complete(&resolved) {
        return log(state, "ERR", format!("The installer of '{}' still needs answers; nothing was added.", p.fname));
    }
    let ops = fomod::install_files(&p.module, &resolved, &world);
    let out = lib.root().join("incoming").join(fsutil::unique_token());
    if let Err(e) = fs::create_dir_all(&out) {
        return log(state, "ERR", format!("Could not create a staging folder: {e}"));
    }
    let guard = DirGuard(out.clone());
    match fomod::materialize(&ops, &p.root, &out) {
        Ok((0, _)) => return log(state, "ERR", format!("The installer of '{}' chose no files; nothing was added.", p.fname)),
        Ok((n, missing)) => {
            log(state, "INFO", format!("Installer: {} file(s) chosen.", n));
            if !missing.is_empty() {
                log(state, "WARN", format!("The installer named {} file(s) the archive does not contain (first: {}).", missing.len(), missing[0]));
            }
        }
        Err(e) => return log(state, "ERR", e),
    }
    let mut opts = p.opts.clone();
    opts.target = opts.target.or_else(|| data_relative_target(&g, &out));
    opts.fomod = Some(json!({ "module": p.module.name, "selection": resolved.selection }));
    if opts.name.is_none() && !p.module.name.trim().is_empty() && opts.nexus.is_none() && opts.thunder.is_none() {
        opts.name = Some(p.module.name.trim().to_string());
    }
    add_staged(state, &g, lib, out, guard, &p.fname, &opts);
}

/// Put an extracted mod into the library (as asked, or where the user said), deploy, and report.
#[allow(clippy::too_many_arguments)]
fn finish_library_add(state: &SharedState, g: &GameEntry, lib: &deploy::Library, stage: &Path, name: &str, fname: &str, version: &str, opts: &InstallOpts, old: Option<deploy::LibMod>, target: Option<&str>) {
    if old.is_some() {
        guard_saves(state, g, "before-update");
    }
    let added = match &old {
        Some(o) => lib.replace_with(&o.id, stage, name, fname, opts.nexus.clone(), version),
        None => lib.add_with_target(stage, name, fname, opts.nexus.clone(), version, target),
    };
    match added {
        Ok(m) => {
            if let Some(r) = &opts.thunder {
                let _ = lib.set_thunderstore(&m.id, r.clone());
                let _ = lib.set_depends(&m.id, opts.depends.clone());
            }
            if let Some(r) = &opts.github {
                let _ = lib.set_github(&m.id, r.clone());
            }
            if let Some(r) = &opts.modio {
                let _ = lib.set_modio(&m.id, r.clone());
            }
            if let Some(answers) = &opts.fomod {
                let _ = lib.set_fomod(&m.id, answers.clone());
            }
            if !opts.verified.is_empty() {
                let _ = lib.set_verified(&m.id, &opts.verified);
            }
            if !m.programs.is_empty() {
                log(state, "WARN", format!("'{}' contains program files ({}). Only keep it if you trust where it came from.", m.name, m.programs.iter().take(4).cloned().collect::<Vec<_>>().join(", ")));
            }
            if old.is_some() {
                if m.version.is_empty() {
                    log(state, "OK", format!("Updated '{}' ({} files).", m.name, m.files.len()));
                } else {
                    log(state, "OK", format!("Updated '{}' to v{} ({} files).", m.name, m.version, m.files.len()));
                }
            } else {
                log(state, "OK", format!("Added '{}' to the mod library ({} files).", m.name, m.files.len()));
            }
            if !m.target.is_empty() {
                log(state, "INFO", format!("Placed under {}/ to match the game's layout.", m.target));
            }
            match library_apply(state, lib, false) {
                Ok(msg) => log(state, "OK", msg),
                Err(e) => log(state, "ERR", e),
            }
            if let Some(nx) = opts.nexus.as_ref().filter(|n| n.valid()) {
                note_requirements(state, g, lib, &m, nx);
            }
        }
        Err(e) => log(state, "ERR", format!("Could not add '{}': {}", fname, e)),
    }
    let _ = fs::remove_dir(lib.root().join("incoming")); // only when empty
    refresh_mods_shared(state);
}

/// The user's answer to "is this the right place?": install there, install into a folder of their
/// choosing, or drop the mod.
fn install_confirm(state: &SharedState, accept: bool, target: Option<&str>) {
    let p = {
        let mut st = write_state(state);
        let Some(p) = st.pending_install.take() else { return };
        st.pending_install = st.pending_queue.pop_front();
        st.notifier.bump();
        p
    };
    let _cleanup = DirGuard(p.stage.clone());
    if !accept {
        log(state, "INFO", format!("'{}' was not added.", p.name));
        return;
    }
    let Some(g) = game_by_appid(state, &p.appid) else { return };
    let Some(lib) = library_of(&g) else { return };
    let opts = InstallOpts { game: Some(g.clone()), nexus: p.opts_nexus.clone(), ..Default::default() };
    // the answer holds for the next mod that looks the same
    if !p.preview.shape.is_empty() {
        let mut st = write_state(state);
        let rule = target.map(|t| format!("dir:{t}")).unwrap_or_else(|| "guess".to_string());
        st.config.place_rules.entry(p.appid.clone()).or_default().insert(p.preview.shape.clone(), rule);
        st.save_config();
    }
    finish_library_add(state, &g, &lib, &p.stage, &p.name, &p.fname, &p.version, &opts, None, target);
}

/// Does an extracted mod carry Lua scripts, DLL hooks or UE4SS itself (things the library does not route)?
fn needs_ue4ss(stage: &Path) -> bool {
    WalkDir::new(stage).into_iter().flatten().filter(|e| e.file_type().is_file()).any(|e| {
        let n = e.file_name().to_string_lossy().to_lowercase();
        n.ends_with(".lua") || n.ends_with(".dll") || n.contains("ue4ss")
    })
}

/// Deploy the library unless the game is running (then the change waits for the next Deploy).
fn library_apply(state: &SharedState, lib: &deploy::Library, force: bool) -> Result<String, String> {
    if read_state(state).is_running.load(Ordering::SeqCst) {
        return Ok("Saved. Close the game, then press Deploy to apply it.".into());
    }
    reapply_pending(state, lib);
    let r = lib.deploy(force)?;
    let mut msg = format!("Deployed {} file(s) ({} linked, {} copied).", r.linked + r.copied, r.linked, r.copied);
    if r.backed_up > 0 {
        msg.push_str(&format!(" {} original(s) backed up.", r.backed_up));
    }
    if r.conflicts > 0 {
        msg.push_str(&format!(" {} conflict(s): later mods win.", r.conflicts));
    }
    if !r.skipped.is_empty() {
        msg.push_str(&format!(" Skipped: {}.", r.skipped.join("; ")));
    }
    if !r.overrides.is_empty() {
        msg.push_str(&format!(" Wine will load {} from the game folder.", r.overrides.join(", ")));
    }
    if r.misplaced > 0 {
        msg.push_str(&format!(" {} mod(s) may be in the wrong folder: press Fix placement.", r.misplaced));
    }
    Ok(msg)
}

/// Run a mod-library action for the selected game off the request thread and report the result.
fn library_task(state: &SharedState, f: impl FnOnce(&SharedState, &deploy::Library, &GameEntry) -> Result<String, String> + Send + 'static) {
    let state_bg = Arc::clone(state);
    thread::spawn(move || {
        let Some(g) = selected_game(&state_bg) else { return };
        let Some(lib) = library_of(&g) else {
            return log(&state_bg, "WARN", format!("{} is not installed.", g.name));
        };
        match f(&state_bg, &lib, &g) {
            Ok(m) if !m.is_empty() => log(&state_bg, "OK", m),
            Ok(_) => {}
            Err(e) => log(&state_bg, "ERR", e),
        }
        refresh_mods_shared(&state_bg);
    });
}

fn running_guard(state: &SharedState) -> Result<(), String> {
    if read_state(state).is_running.load(Ordering::SeqCst) {
        Err("Close the game first.".into())
    } else {
        Ok(())
    }
}

fn install_archive(
    p: &Path,
    fname: &str,
    g: &GameEntry,
    gdir: &Path,
    ue_bin: Option<&Path>,
    cache_dir: &Path,
    state: &SharedState,
) {
    log(state, "INFO", format!("Extracting archive: {}...", fname));
    let cache_root = cache_dir.parent().unwrap_or(cache_dir);
    let Some(stage_guard) = make_stage_dir(gdir, cache_root) else {
        log(state, "ERR", "Could not create a staging directory for extraction.");
        return;
    };
    let stage = stage_guard.0.clone();

    if let Err(e) = ensure_room(p, &stage) {
        return log(state, "ERR", e);
    }
    if !extract_archive(p, &stage) {
        log(
            state,
            "ERR",
            format!("Failed to extract '{}' (need 7z, bsdtar, unzip or unrar installed).", fname),
        );
        return;
    }

    if let Some(ub) = ue_bin {
        if let Some(n) = install_ue4ss_payload(&stage, ub) {
            log(state, "OK", format!("Installed UE4SS runtime ({} files).", n));
        }
    }

    // PAKs (+ IoStore companions) → Content/Paks/{~mods,LogicMods}
    let paks_root = ModManager::find_or_create_paks_dir(gdir);
    let pak_files: Vec<PathBuf> = WalkDir::new(&stage)
        .into_iter()
        .flatten()
        .filter(|e| e.file_type().is_file())
        .map(|e| e.path().to_path_buf())
        .filter(|ep| ep.extension().map(|e| e.eq_ignore_ascii_case("pak")).unwrap_or(false))
        .collect();
    for ep in pak_files {
        let sub = if pak_goes_to_logic_mods(&ep) { "LogicMods" } else { "~mods" };
        let dest = paks_root.join(sub);
        let Some(name) = ep.file_name().map(|n| n.to_os_string()) else { continue };
        let mut res = place_file(&ep, &dest.join(&name), true);
        for c_ext in ["utoc", "ucas", "sig"] {
            let comp = ep.with_extension(c_ext);
            if comp.is_file() {
                let cname = comp.file_name().unwrap_or_default().to_os_string();
                res = res.and(place_file(&comp, &dest.join(cname), true));
            }
        }
        match res {
            Ok(()) => log(state, "OK", format!("Routed PAK to Content/Paks/{}", sub)),
            Err(e) => log(state, "ERR", format!("Could not install PAK {}: {}", name.to_string_lossy(), e)),
        }
    }

    // Lua mods → UE4SS/Mods/<Name>
    if let Some(ub) = ue_bin {
        let mods_dir = ModManager::get_ue4ss_root(ub).join("Mods");
        let _ = fs::create_dir_all(&mods_dir);
        let mut lua_roots = HashSet::new();
        for entry in WalkDir::new(&stage).into_iter().flatten() {
            if entry.file_name().to_string_lossy().eq_ignore_ascii_case("main.lua") {
                if let Some(parent) = entry.path().parent() {
                    let pname = parent.file_name().unwrap_or_default().to_string_lossy().to_lowercase();
                    let r = if pname == "scripts" || pname == "src" {
                        parent.parent().unwrap_or(parent)
                    } else {
                        parent
                    };
                    lua_roots.insert(r.to_path_buf());
                }
            }
        }
        let mut roots: Vec<PathBuf> = lua_roots.into_iter().collect();
        roots.sort(); // ancestors sort before their descendants
        let mut done: Vec<PathBuf> = Vec::new();
        for r in roots {
            if done.iter().any(|d| r.starts_with(d)) || !r.exists() {
                continue;
            }
            let raw_mname = if r == stage {
                p.file_stem().unwrap_or_default().to_string_lossy().to_string()
            } else {
                r.file_name().unwrap_or_default().to_string_lossy().to_string()
            };
            let mname = sanitize_mod_folder_name(&raw_mname);
            let dest = mods_dir.join(&mname);
            let res = if r == stage {
                fsutil::copy_dir_all(&r, &dest).map(|_| ())
            } else {
                install_dir(&r, &dest)
            };
            match res {
                Ok(()) => {
                    let _ = File::create(dest.join("enabled.txt"));
                    let _ = fs::remove_file(dest.join("enabled.txt.disabled"));
                    log(state, "OK", format!("Installed Lua mod: {}", mname));
                }
                Err(e) => log(state, "ERR", format!("Could not install Lua mod {}: {}", mname, e)),
            }
            if r != stage {
                done.push(r);
            }
        }
    }

    // Loose DLL hooks and trainers
    let dll_dest = ue_bin.map(Path::to_path_buf).unwrap_or_else(|| gdir.to_path_buf());
    let loose: Vec<PathBuf> = WalkDir::new(&stage)
        .into_iter()
        .flatten()
        .filter(|e| e.file_type().is_file())
        .map(|e| e.path().to_path_buf())
        .collect();
    for ep in loose {
        let en_name = ep.file_name().unwrap_or_default().to_string_lossy().to_string();
        let low = en_name.to_lowercase();
        if KNOWN_MOD_DLLS.contains(&low.as_str()) {
            match place_file(&ep, &dll_dest.join(&en_name), true) {
                Ok(()) => log(state, "OK", format!("Installed DLL hook from archive: {}", en_name)),
                Err(e) => log(state, "ERR", format!("Could not install {}: {}", en_name, e)),
            }
        } else if low.ends_with(".exe") && !low.contains("ue4ss") && !is_auxiliary_exe(&low) {
            let perm = trainers_dir(cache_dir).join(format!("{}_{}", g.appid, en_name));
            if place_file(&ep, &perm, true).is_ok() {
                attach_trainer(state, &g.appid, &perm, &en_name);
            }
        }
    }

    if ue_bin.is_some() {
        RigEngine::invalidate_ue_bin_memo();
        for a in RepairEngine::repair(g) {
            log(state, "UE5", a);
        }
    }
}

// ───────────────────────────── file watcher ─────────────────────────────

fn is_relevant_fs_event(ev: &notify::Event) -> bool {
    if matches!(ev.kind, EventKind::Access(_)) {
        return false;
    }
    ev.paths.iter().any(|p| {
        if fsutil::is_tmp_path(p) {
            return false;
        }
        // Windows reports a folder as modified whenever a file inside it is written; the file's own
        // event says whether that matters
        if matches!(ev.kind, EventKind::Modify(_)) && p.is_dir() {
            return false;
        }
        !matches!(
            p.extension().map(|e| e.to_string_lossy().to_lowercase()).as_deref(),
            Some("log") | Some("dmp")
        )
    })
}

/// Watches the selected game's mod folders and refreshes the mod list (debounced, off-lock).
/// Holds only a `Weak` reference so it ends together with the application state.
fn spawn_inotify_watcher(state: Weak<RwLock<AppState>>) {
    thread::spawn(move || {
        let (tx, rx) = std::sync::mpsc::channel();
        let Ok(mut watcher) = RecommendedWatcher::new(
            move |res| {
                if let Ok(event) = res {
                    let _ = tx.send(event);
                }
            },
            Config::default(),
        ) else {
            if let Some(s) = state.upgrade() {
                log(&s, "WARN", "File watcher unavailable; mod list updates only on actions.");
            }
            return;
        };

        let mut watched: Vec<(PathBuf, RecursiveMode)> = Vec::new();
        let mut watched_key = String::new();
        let mut last_resolve = Instant::now() - Duration::from_secs(60);
        let mut dirty_since: Option<Instant> = None;
        let mut last_refresh = Instant::now() - Duration::from_secs(60);

        loop {
            let Some(st_arc) = state.upgrade() else { return };

            let (key, game) = {
                let st = read_state(&st_arc);
                let g = st.games.get(st.selected_idx).cloned();
                (g.as_ref().map(|g| g.appid.clone()).unwrap_or_default(), g)
            };
            // Re-resolve directories when the selection changes, and periodically so folders
            // created later (UE4SS install, first PAK) get picked up.
            if key != watched_key || last_resolve.elapsed() > Duration::from_secs(8) {
                last_resolve = Instant::now();
                let gdir_ok = game.as_ref().map(|g| Path::new(&g.game_dir).is_dir()).unwrap_or(false);
                let mut want: Vec<(PathBuf, RecursiveMode)> = Vec::new();
                if let (Some(g), true) = (game.as_ref(), gdir_ok) {
                    for d in ModManager::watched_mod_dirs(g) {
                        let mode = if d.ends_with("Win64") { RecursiveMode::NonRecursive } else { RecursiveMode::Recursive };
                        want.push((d, mode));
                    }
                }
                if want != watched {
                    for (old, _) in &watched {
                        let _ = watcher.unwatch(old);
                    }
                    for (new_d, mode) in &want {
                        let _ = watcher.watch(new_d, *mode);
                    }
                    watched = want;
                }
                watched_key = key;
            }
            drop(st_arc);

            match rx.recv_timeout(Duration::from_millis(400)) {
                Ok(ev) => {
                    if is_relevant_fs_event(&ev) {
                        dirty_since.get_or_insert_with(Instant::now);
                    }
                }
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => return,
            }

            // Debounce: settle for 150ms of quiet (or 1s of continuous churn) before refreshing.
            if let Some(since) = dirty_since {
                let quiet = rx.recv_timeout(Duration::from_millis(150));
                if let Ok(ev) = &quiet {
                    if !is_relevant_fs_event(ev) {
                        continue;
                    }
                    if since.elapsed() < Duration::from_secs(1) {
                        continue;
                    }
                }
                while rx.try_recv().is_ok() {}
                if last_refresh.elapsed() >= Duration::from_millis(300) {
                    if let Some(s) = state.upgrade() {
                        refresh_mods_shared(&s);
                    }
                    last_refresh = Instant::now();
                    dirty_since = None;
                }
            }
        }
    });
}

// ───────────────────────────── window launcher ─────────────────────────────

/// The window size to open with: the one remembered from last time when it is sensible, else the default.
fn window_size_or_default(w: u32, h: u32) -> (u32, u32) {
    if (640..=7680).contains(&w) && (480..=4320).contains(&h) {
        (w, h)
    } else {
        (1280, 825)
    }
}

fn configured_window_size() -> (u32, u32) {
    let c = fs::read(RigPaths::new().config_file).ok().and_then(|b| serde_json::from_slice::<RigConfigData>(&b).ok()).unwrap_or_default();
    window_size_or_default(c.window_w, c.window_h)
}

fn launch_native_app_window(url: &str, profile_dir: &Path) {
    let (win_w, win_h) = configured_window_size();
    // `RIG_NO_WINDOW=1`: run only the server (automated checks, headless machines)
    if std::env::var_os("RIG_NO_WINDOW").is_some() {
        return;
    }
    // Launch native Qt desktop shell if available
    let py_app = std::env::current_exe().ok().and_then(|e| e.parent().map(|p| p.join("modforge_app.py")));
    if let Some(app_path) = py_app.filter(|p| p.exists()) {
        let mut py = Command::new("python3");
        py.arg(app_path).stdout(Stdio::null()).stderr(Stdio::null());
        if spawn_reaped(py) {
            return;
        }
    }
    for browser in platform::browsers() {
        let mut c = Command::new(browser);
        c.arg(format!("--app={}", url))
            .arg(format!("--user-data-dir={}", profile_dir.display()))
            .arg(format!("--window-size={win_w},{win_h}"))
            .args(std::env::var_os("RIG_DECK").map(|_| "--start-fullscreen"))
            .arg("--disable-features=Translate,TranslateUI")
            .arg("--overscroll-history-navigation=0")
            .arg("--disable-session-crashed-bubble")
            .arg("--enable-gpu-rasterization")
            .arg("--disable-extensions")
            .arg("--disable-sync")
            .arg("--no-first-run")
            .arg("--no-default-browser-check")
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        if !platform::IS_WINDOWS && !platform::IS_MAC {
            // its own window class (taskbar grouping, icon) and Wayland
            c.arg("--class=ModForge").arg("--name=ModForge").arg("--ozone-platform-hint=auto").arg("--enable-zero-copy");
        }
        if spawn_reaped(c) {
            return;
        }
    }
    xdg_open(url);
}

// ───────────────────────────── HTTP layer ─────────────────────────────

pub struct ServerCtx {
    /// Identifies this exact binary so a newer launch can tell it apart from an older instance.
    pub build: String,
    pub token: String,
    pub hosts: Vec<String>,
    pub origins: Vec<String>,
}

impl ServerCtx {
    pub fn new(port: u16, token: String, build: String) -> Self {
        let hosts: Vec<String> = ["127.0.0.1", "localhost"].iter().map(|h| format!("{}:{}", h, port)).collect();
        let origins = hosts.iter().map(|h| format!("http://{}", h)).collect();
        Self { build, token, hosts, origins }
    }
}

fn header_value(req: &Request, name: &'static str) -> Option<String> {
    req.headers()
        .iter()
        .find(|h| h.field.equiv(name))
        .map(|h| h.value.as_str().to_string())
}

fn split_url(url: &str) -> (&str, &str) {
    url.split_once('?').unwrap_or((url, ""))
}

fn query_param(query: &str, key: &str) -> Option<String> {
    query.split('&').find_map(|kv| {
        let (k, v) = kv.split_once('=').unwrap_or((kv, ""));
        (k == key).then(|| fsutil::url_decode(v))
    })
}

fn constant_time_eq(a: &str, b: &str) -> bool {
    a.len() == b.len() && a.bytes().zip(b.bytes()).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// Host must be our own loopback name (defeats DNS rebinding), Origin — when sent — must be
/// ourselves (defeats cross-site fetches) and API calls must present the per-launch token.
fn host_and_origin_ok(req: &Request, ctx: &ServerCtx) -> bool {
    let host_ok = header_value(req, "Host").map(|h| ctx.hosts.contains(&h)).unwrap_or(false);
    let origin_ok = header_value(req, "Origin")
        .map(|o| ctx.origins.contains(&o))
        .unwrap_or(true);
    host_ok && origin_ok
}

fn token_ok(req: &Request, query: &str, ctx: &ServerCtx) -> bool {
    let presented = header_value(req, "X-Rig-Token").or_else(|| query_param(query, "t")).unwrap_or_default();
    constant_time_eq(&presented, &ctx.token)
}

fn json_header() -> Header {
    Header::from_bytes(&b"Content-Type"[..], &b"application/json"[..]).unwrap()
}

fn nocache_header() -> Header {
    Header::from_bytes(&b"Cache-Control"[..], &b"no-store"[..]).unwrap()
}

fn respond_json(req: Request, status: u16, body: String) {
    let r = Response::from_string(body)
        .with_status_code(status)
        .with_header(json_header())
        .with_header(nocache_header());
    let _ = req.respond(r);
}

fn respond_state(req: Request, state: &SharedState) {
    let body = read_state(state).to_json().to_string();
    respond_json(req, 200, body);
}

fn serve_index(req: Request, ctx: &ServerCtx, state: &SharedState) {
    #[cfg(debug_assertions)]
    let html = fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/src/ui.html"))
        .unwrap_or_else(|_| EMBEDDED_UI.to_string());
    #[cfg(not(debug_assertions))]
    let html = EMBEDDED_UI.to_string();
    let lang = read_state(state).config.language.clone();
    let html = html
        .replace("__RIG_TOKEN__", &ctx.token)
        .replace("__RIG_LANG__", if i18n::valid(&lang) { &lang } else { i18n::DEFAULT_LANG })
        .replace("__RIG_DICT__", &i18n::dict_for_page(&lang));
    let csp = "default-src 'self'; script-src 'self' 'unsafe-inline'; style-src 'self' 'unsafe-inline'; \
               frame-ancestors 'none'; base-uri 'none'; form-action 'none'";
    let hosts: String = richtext::IMAGE_HOSTS.iter().map(|h| format!(" https://{h}")).collect();
    let csp = format!("{csp}; img-src 'self' data: https://shared.fastly.steamstatic.com{hosts}; connect-src 'self'");
    let r = Response::from_string(html)
        .with_header(Header::from_bytes(&b"Content-Type"[..], &b"text/html; charset=utf-8"[..]).unwrap())
        .with_header(Header::from_bytes(&b"Content-Security-Policy"[..], csp.as_bytes()).unwrap())
        .with_header(Header::from_bytes(&b"X-Content-Type-Options"[..], &b"nosniff"[..]).unwrap())
        .with_header(Header::from_bytes(&b"X-Frame-Options"[..], &b"DENY"[..]).unwrap())
        .with_header(Header::from_bytes(&b"X-Rig-Build"[..], ctx.build.as_bytes()).unwrap())
        .with_header(nocache_header());
    let _ = req.respond(r);
}

fn handle_request(mut req: Request, state: SharedState, notifier: Arc<EventNotifier>, ctx: Arc<ServerCtx>) {
    let full_url = req.url().to_string();
    let (path, query) = split_url(&full_url);
    let (path, query) = (path.to_string(), query.to_string());

    if !host_and_origin_ok(&req, &ctx) {
        return respond_json(req, 403, json!({ "error": "forbidden" }).to_string());
    }
    if path == "/" || path == "/index.html" {
        notifier.touch_client();
        return serve_index(req, &ctx, &state);
    }
    if !path.starts_with("/api/") || !token_ok(&req, &query, &ctx) {
        let (code, msg) = if path.starts_with("/api/") { (403, "forbidden") } else { (404, "not found") };
        return respond_json(req, code, json!({ "error": msg }).to_string());
    }
    notifier.touch_client();

    match path.as_str() {
        "/api/subscribe" => {
            let client_ver = query_param(&query, "v").and_then(|s| s.parse::<u64>().ok()).unwrap_or(0);
            notifier.wait_for_change(client_ver, SUBSCRIBE_TIMEOUT);
            return respond_state(req, &state);
        }
        "/api/state" => return respond_state(req, &state),
        "/api/drop_upload" => return handle_drop_upload(req, &query, &state),
        "/api/doctor" => {
            let game = selected_game(&state);
            let rep = game.as_ref().map(RepairEngine::diagnose).unwrap_or_default();
            let mut v = serde_json::to_value(&rep).unwrap_or_else(|_| json!({}));
            if let (Some(g), Some(obj)) = (game.as_ref(), v.as_object_mut()) {
                obj.insert("steam".into(), serde_json::to_value(steam_info(&state, g)).unwrap_or(Value::Null));
            }
            return respond_json(req, 200, v.to_string());
        }
        "/api/saves_list" => {
            let (game, cache_root) = {
                let st = read_state(&state);
                (st.games.get(st.selected_idx).cloned(), st.cache_root())
            };
            let snaps = game.map(|g| saves::list(&g, &cache_root)).unwrap_or_default();
            return respond_json(req, 200, json!({ "snapshots": snaps }).to_string());
        }
        "/api/share_code" => {
            let body = match selected_game(&state).filter(|g| g.is_installed).and_then(|g| library_of(&g).map(|l| l.export_spec(&g.name))) {
                Some(spec) => {
                    let (code, left_out) = profilecode::share_code(&spec);
                    json!({ "code": code, "mods": spec.mods.len() - left_out, "left_out": left_out })
                }
                None => json!({ "error": "Select an installed game first." }),
            };
            return respond_json(req, 200, body.to_string());
        }
        "/api/saves_files" => {
            let (game, cache_root, sessions_file) = {
                let st = read_state(&state);
                (st.games.get(st.selected_idx).cloned(), st.cache_root(), st.paths.config_file.with_file_name("sessions.jsonl"))
            };
            let Some(g) = game else { return respond_json(req, 200, json!({ "files": [] }).to_string()) };
            let custom = save_places(&state, &g);
            let (a, b, id) = (query_param(&query, "a").unwrap_or_default(), query_param(&query, "b").unwrap_or_default(), query_param(&query, "id").unwrap_or_default());
            let body = if !a.is_empty() && !b.is_empty() {
                match saves::compare(&g, &cache_root, &a, &b, &custom) {
                    Ok(d) => json!({ "diff": d }),
                    Err(e) => json!({ "error": e }),
                }
            } else if !id.is_empty() {
                match saves::files(&g, &cache_root, &id) {
                    Ok(f) => json!({ "id": id, "files": f }),
                    Err(e) => json!({ "error": e }),
                }
            } else {
                saves_made_with(&g, saves::live_files(&g, &custom), &stats::read(&sessions_file))
            };
            return respond_json(req, 200, body.to_string());
        }
        "/api/game_logs" => {
            let (src, q) = (query_param(&query, "src").unwrap_or_default(), query_param(&query, "q").unwrap_or_default());
            let body = match selected_game(&state) {
                None => json!({ "sources": [] }),
                Some(g) => {
                    let proton = home_dir().join(format!("steam-{}.log", if g.steam_appid.is_empty() { &g.appid } else { &g.steam_appid }));
                    let files = gamelogs::files(&g, Some(proton));
                    let chosen = files.iter().find(|f| f.id == src).or_else(|| files.first());
                    let (lines, total) = chosen.and_then(|f| gamelogs::read(&f.path, &q).ok()).unwrap_or_default();
                    json!({ "sources": files, "src": chosen.map(|f| f.id.clone()), "lines": lines, "total": total })
                }
            };
            return respond_json(req, 200, body.to_string());
        }
        "/api/home_trending" => return respond_json(req, 200, trending_for_home(&state, query_param(&query, "refresh").as_deref() == Some("1")).to_string()),
        "/api/inbox" => return respond_json(req, 200, inbox_json(&state).to_string()),
        "/api/screenshots" => {
            let shots = selected_game(&state).map(|g| shots_for(&state, &g)).unwrap_or_default();
            let count = |k: &str| shots.iter().filter(|s| s.kind == k).count();
            let items: Vec<Value> = shots.iter().enumerate().map(|(n, s)| json!({ "n": n, "name": s.name, "at_ms": s.at_ms, "kind": s.kind })).collect();
            return respond_json(req, 200, json!({ "items": items, "modded": count("modded"), "plain": count("plain") }).to_string());
        }
        "/api/screenshot" => {
            // the browser names a place in the list, never a file
            let n = query_param(&query, "n").and_then(|v| v.parse::<usize>().ok());
            let found = selected_game(&state).and_then(|g| {
                let shots = shots_for(&state, &g);
                screenshots::bytes(shots.get(n?)?, query_param(&query, "thumb").as_deref() == Some("1"))
            });
            return match found {
                Some((bytes, ctype)) => {
                    let _ = req.respond(Response::from_data(bytes).with_header(Header::from_bytes(&b"Content-Type"[..], ctype.as_bytes()).unwrap()).with_header(nocache_header()));
                }
                None => respond_json(req, 404, json!({ "error": "no such picture" }).to_string()),
            };
        }
        "/api/sync_info" => {
            let folder = read_state(&state).config.sync_folder.clone();
            let copies = if folder.is_empty() { Vec::new() } else { autobackup::list(&sync_dir(&folder)) };
            return respond_json(req, 200, json!({ "folder": folder, "newest_ms": copies.first().map(|e| e.at_ms), "count": copies.len() }).to_string());
        }
        "/api/doctor_all" => return respond_json(req, 200, doctor_json(&state).to_string()),
        "/api/workshop" => {
            let (game, cache, base, http) = {
                let st = read_state(&state);
                (st.games.get(st.selected_idx).cloned(), st.cache_root().join("workshop_items.json"), st.steam_lib.base.clone(), st.steam_lib.allow_http)
            };
            let body = match game.filter(|g| g.is_installed && !g.steam_id().is_empty()) {
                Some(g) => workshop::listing(Path::new(&g.game_dir), g.steam_id(), &cache, &base, http),
                None => json!({ "items": [], "error": null }),
            };
            return respond_json(req, 200, body.to_string());
        }
        "/api/perf" => return respond_json(req, 200, perf_json(&state).to_string()),
        "/api/startup_benchmark" => {
            let res = selected_game(&state)
                .map(|g| {
                    let lib = library_of(&g);
                    let mods = lib.as_ref().map(|l| l.mods()).unwrap_or_default();
                    perf::benchmark_startup(g.steam_id(), &g.name, &mods)
                })
                .unwrap_or_default();
            return respond_json(req, 200, serde_json::to_string(&res).unwrap_or_else(|_| "{}".into()));
        }
        "/api/conflict_view" => {
            let q = query_param(&query, "q").unwrap_or_default();
            let body = selected_game(&state)
                .and_then(|g| library_of(&g))
                .map(|lib| {
                    let mut v = lib.conflict_view(&q, 300);
                    let dir = selected_game(&state).map(|g| PathBuf::from(g.game_dir)).unwrap_or_default();
                    let (archives, scripts) = clash_view(&dir, &lib);
                    v["archives"] = json!(archives);
                    v["scripts"] = json!(scripts);
                    v
                })
                .unwrap_or_else(|| json!({ "total": 0, "matching": 0, "pinned": 0, "mods": [], "pairs": [], "files": [] }));
            return respond_json(req, 200, body.to_string());
        }
        "/api/conflicts" => {
            let report = selected_game(&state)
                .map(|g| {
                    let paks: Vec<pak::PakMod> = ModManager::list_mods(&g)
                        .into_iter()
                        .filter(|m| m.mod_type == "PAK" && m.enabled)
                        .map(|m| pak::PakMod { name: m.name, path: m.path })
                        .collect();
                    pak::find_conflicts(&paks)
                })
                .unwrap_or_default();
            return respond_json(req, 200, serde_json::to_string(&report).unwrap_or_else(|_| "{}".into()));
        }
        "/api/ue4ss_tuner_get" => {
            let dto = selected_game(&state).map(|g| ModManager::get_ue4ss_settings(&g)).unwrap_or_default();
            return respond_json(req, 200, serde_json::to_string(&dto).unwrap_or_else(|_| "{}".into()));
        }
        "/api/i18n" => {
            let lang = query_param(&query, "lang").unwrap_or_default();
            if !i18n::valid(&lang) {
                return respond_json(req, 404, json!({ "error": "unknown language" }).to_string());
            }
            return respond_json(req, 200, i18n::dict(&lang).to_string());
        }
        "/api/steam_owned" => {
            let list = read_state(&state).steam_lib.owned.clone();
            return respond_json(req, 200, serde_json::to_string(&list).unwrap_or_else(|_| "[]".into()));
        }
        "/api/downloads" => {
            let mut v = downloads::status();
            v["background"] = background::status();
            return respond_json(req, 200, v.to_string());
        }
        "/api/auto_backups" => {
            let dir = auto_backup_dir(&read_state(&state));
            let list: Vec<Value> = autobackup::list(&dir).iter().map(|e| json!({ "name": e.name, "at_ms": e.at_ms, "size": e.size })).collect();
            return respond_json(req, 200, json!({ "backups": list, "keep": autobackup::KEEP }).to_string());
        }
        "/api/stats" => {
            let tz = query_param(&query, "tz").and_then(|v| v.parse::<i64>().ok()).unwrap_or(0).clamp(-840, 840);
            let which = query_param(&query, "all").is_none();
            let (path, appid) = {
                let st = read_state(&state);
                (st.paths.config_file.with_file_name("sessions.jsonl"), st.games.get(st.selected_idx).map(|g| g.appid.clone()))
            };
            let all = stats::read(&path);
            let body = stats::summary(&all, if which { appid.as_deref() } else { None }, now_ms(), tz);
            return respond_json(req, 200, body.to_string());
        }
        "/api/store_page" => {
            let (source, id) = (query_param(&query, "source").unwrap_or_default(), query_param(&query, "id").unwrap_or_default());
            let body = store_page_json(&state, &source, &id).unwrap_or_else(|e| json!({ "error": e }));
            return respond_json(req, 200, body.to_string());
        }
        "/api/store" => {
            let Some(g) = selected_game(&state).filter(|g| g.is_installed) else {
                return respond_json(req, 200, json!({ "error": "Select an installed game first." }).to_string());
            };
            let n = |k: &str| query_param(&query, k).and_then(|v| v.parse::<usize>().ok()).unwrap_or(0);
            let ask = StoreAsk {
                source: query_param(&query, "source").unwrap_or_else(|| "all".into()),
                sort: query_param(&query, "sort").unwrap_or_else(|| "popular".into()),
                query: query_param(&query, "q").unwrap_or_default(),
                category: query_param(&query, "category").unwrap_or_default(),
                off_nexus: n("off_n"),
                off_thunder: n("off_t"),
                off_modio: n("off_m"),
                hide_libs: query_param(&query, "libs").as_deref() != Some("1"),
                per: match n("per") {
                    0 => 18,
                    v => v.clamp(1, 40),
                },
            };
            return respond_json(req, 200, store_page(&state, &g, &ask).to_string());
        }
        "/api/preflight" => {
            let Some(g) = selected_game(&state).filter(|g| g.is_installed) else { return respond_json(req, 200, json!({ "problems": [] }).to_string()) };
            let t0 = Instant::now();
            let problems = preflight(&state, &g);
            return respond_json(req, 200, json!({ "appid": g.appid, "problems": problems, "ms": t0.elapsed().as_millis() as u64 }).to_string());
        }
        "/api/setup_plan" => {
            let g = match query_param(&query, "appid") {
                Some(id) => game_by_appid(&state, &id),
                None => selected_game(&state),
            };
            let Some(g) = g.filter(|g| g.is_installed) else { return respond_json(req, 200, json!({ "steps": [], "not_installed": true }).to_string()) };
            return respond_json(req, 200, setup_plan_json(&state, &g).to_string());
        }
        "/api/welcome" => {
            let (games, recipes_list) = {
                let st = read_state(&state);
                (st.games.clone(), st.recipes.clone())
            };
            let rows: Vec<Value> = games
                .iter()
                .filter(|g| g.is_installed)
                .map(|g| {
                    let dir = PathBuf::from(&g.game_dir);
                    let rec = recipes::find(&recipes_list, &g.appid, g.recipe_steam_id()).map(|r| r.loader.clone()).unwrap_or_default();
                    let ac = deploy::anti_cheat(&dir);
                    let missing = frameworks::cores(&dir).iter().filter(|c| !c.installed).count();
                    let v = welcome::verdict_with_cores(loaders::detect(&dir).as_ref(), &g.engine, g.ue4ss, &rec, ac, missing);
                    welcome::row(&g.appid, &g.name, &g.engine, g.mod_count, &v)
                })
                .collect();
            return respond_json(req, 200, json!({ "games": rows }).to_string());
        }
        "/api/health" => {
            let Some(g) = selected_game(&state).filter(|g| g.is_installed) else {
                return respond_json(req, 200, json!({ "code": "none", "level": "info" }).to_string());
            };
            return respond_json(req, 200, game_health(&state, &g).to_string());
        }
        "/api/nexus_browse" => {
            let list = query_param(&query, "list").unwrap_or_else(|| "trending".into());
            let Some(g) = selected_game(&state).filter(|g| g.is_installed) else {
                return respond_json(req, 200, json!({ "error": "Select an installed game first." }).to_string());
            };
            let body = match nexus_domain_for(&state, &g) {
                Err(e) => json!({ "error": e, "need_domain": !e.contains("API key") }),
                Ok(domain) => {
                    let premium = read_state(&state).nexus.user.as_ref().map(|u| u.premium).unwrap_or(false);
                    let q = query_param(&query, "q").unwrap_or_default();
                    let listing = nexus_client(&state).and_then(|c| {
                        if q.trim().chars().count() >= 2 {
                            c.search(&domain, &q, 30).map(|v| json!(v))
                        } else if list == "tracked" {
                            tracked_listing(&state, &c, &domain).map(|v| json!(v))
                        } else {
                            c.browse(&domain, &list).map(|v| json!(v))
                        }
                    });
                    match listing {
                        Ok(mods) => json!({ "domain": domain, "mods": mods, "premium": premium }),
                        Err(e) => json!({ "error": e, "domain": domain }),
                    }
                }
            };
            return respond_json(req, 200, body.to_string());
        }
        "/api/github" => {
            let repo = query_param(&query, "repo").unwrap_or_default();
            let (api, allow) = {
                let st = read_state(&state);
                (st.github_api.clone(), st.github_allow.clone())
            };
            let body = match github::parse_repo(&repo) {
                None => json!({ "error": "That does not look like a GitHub repository." }),
                Some((o, r)) => match github::latest(&o, &r, &api, allow.as_deref()) {
                    Ok(rel) => json!({ "repo": format!("{o}/{r}"), "tag": rel.tag, "title": rel.title, "files": github::mod_files(&rel.assets) }),
                    Err(e) => json!({ "error": e }),
                },
            };
            return respond_json(req, 200, body.to_string());
        }
        "/api/compat" => {
            let Some(g) = selected_game(&state) else { return respond_json(req, 200, json!({}).to_string()) };
            return respond_json(req, 200, compat_info(&state, &g).to_string());
        }
        "/api/disk" => {
            return respond_json(req, 200, disk_report(&state).to_string());
        }
        "/api/report" => {
            let text = selected_game(&state).map(|g| problem_report(&state, &g)).unwrap_or_default();
            return respond_json(req, 200, json!({ "text": text }).to_string());
        }
        "/api/thunder_search" => {
            let q = query_param(&query, "q").unwrap_or_default();
            let Some(g) = selected_game(&state).filter(|g| g.is_installed) else {
                return respond_json(req, 200, json!({ "error": "Select an installed game first." }).to_string());
            };
            let body = match thunder_community_for(&state, &g) {
                Err(e) => json!({ "error": e, "need_community": true }),
                Ok(c) => match thunder_listing(&state, &c) {
                    Err(e) => json!({ "error": e, "community": c }),
                    Ok(all) => {
                        let found: Vec<&thunderstore::Listing> = if q.trim().is_empty() {
                            let mut top: Vec<&thunderstore::Listing> = all.iter().filter(|l| !l.nsfw).collect();
                            top.sort_by_key(|l| std::cmp::Reverse(l.downloads));
                            top.truncate(40);
                            top
                        } else {
                            thunderstore::search(&all, &q, 40)
                        };
                        let installed: std::collections::HashSet<String> =
                            library_of(&g).map(|l| l.mods().into_iter().filter_map(|m| m.thunderstore.map(|t| t.full_name())).collect()).unwrap_or_default();
                        let rows: Vec<Value> = found
                            .iter()
                            .map(|l| json!({ "full_name": l.full_name, "name": l.name.replace('_', " "), "owner": l.owner, "description": l.description, "version": l.version, "downloads": l.downloads, "rating": l.rating, "icon": l.icon, "installed": installed.contains(&l.full_name) }))
                            .collect();
                        json!({ "community": c, "total": all.len(), "results": rows })
                    }
                },
            };
            return respond_json(req, 200, body.to_string());
        }
        "/api/configs" => {
            let files = selected_game(&state).filter(|g| g.is_installed).map(|g| configs::list(Path::new(&g.game_dir))).unwrap_or_default();
            return respond_json(req, 200, json!({ "files": files }).to_string());
        }
        "/api/config_read" => {
            let path = query_param(&query, "path").unwrap_or_default();
            let r = selected_game(&state).filter(|g| g.is_installed).ok_or_else(|| "Select an installed game first.".to_string()).and_then(|g| configs::read(Path::new(&g.game_dir), &path));
            return match r {
                Ok(text) => respond_json(req, 200, json!({ "text": text }).to_string()),
                Err(e) => respond_json(req, 400, json!({ "error": e }).to_string()),
            };
        }
        "/api/lib_notes" => {
            let id = query_param(&query, "id").unwrap_or_default();
            let notes = selected_game(&state).and_then(|g| library_of(&g)).map(|l| l.notes(&id)).unwrap_or_default();
            let v: Vec<Value> = notes.into_iter().map(|(name, text)| json!({ "name": name, "text": text })).collect();
            return respond_json(req, 200, json!({ "notes": v }).to_string());
        }
        "/api/fomod_view" => {
            let sel: fomod::Selection = query_param(&query, "sel").and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default();
            let Some(p) = read_state(&state).pending_fomod.first().cloned() else { return respond_json(req, 200, json!({ "error": "There is no installer waiting." }).to_string()) };
            let Some(g) = game_by_appid(&state, &p.appid) else { return respond_json(req, 200, json!({ "error": "The game is gone." }).to_string()) };
            let Some(lib) = library_of(&g) else { return respond_json(req, 200, json!({ "error": "This game is not installed." }).to_string()) };
            let world = fomod_world(&g, &lib);
            let r = fomod::resolve(&p.module, &sel, &world);
            let files = fomod::install_files(&p.module, &r, &world).len();
            let body = json!({ "name": p.module.name, "fname": p.fname, "steps": r.steps, "selection": r.selection, "complete": fomod::complete(&r), "files": files });
            return respond_json(req, 200, body.to_string());
        }
        "/api/fomod_image" => {
            let rel = query_param(&query, "p").unwrap_or_default();
            let root = read_state(&state).pending_fomod.first().map(|p| p.root.clone());
            let found = root.and_then(|root| {
                let path = fomod::resolve_ci(&root, &rel)?;
                let ext = path.extension()?.to_string_lossy().to_lowercase();
                let ctype = match ext.as_str() {
                    "png" => "image/png",
                    "jpg" | "jpeg" => "image/jpeg",
                    "gif" => "image/gif",
                    "bmp" => "image/bmp",
                    "webp" => "image/webp",
                    _ => return None,
                };
                let inside = path.canonicalize().ok()?.starts_with(root.canonicalize().ok()?);
                if inside && fs::metadata(&path).ok()?.len() <= 8_000_000 { Some((fs::read(&path).ok()?, ctype)) } else { None }
            });
            return match found {
                Some((bytes, ctype)) => {
                    let _ = req.respond(Response::from_data(bytes).with_header(Header::from_bytes(&b"Content-Type"[..], ctype.as_bytes()).unwrap()).with_header(nocache_header()));
                }
                None => respond_json(req, 404, json!({ "error": "no such picture" }).to_string()),
            };
        }
        "/api/plugins" => {
            let sort = query_param(&query, "sort").is_some();
            return respond_json(req, 200, plugins_json(&state, sort).to_string());
        }
        "/api/pack" => {
            let Some(g) = selected_game(&state) else { return respond_json(req, 200, json!({ "error": "Pick a game first." }).to_string()) };
            return respond_json(req, 200, pack_draft(&state, &g).to_string());
        }
        "/api/find_all" => {
            let q = query_param(&query, "q").unwrap_or_default();
            return respond_json(req, 200, find_everywhere(&state, &q).to_string());
        }
        "/api/nexus_social" => {
            let (id, act) = (query_param(&query, "id").unwrap_or_default(), query_param(&query, "act").unwrap_or_default());
            return respond_json(req, 200, nexus_social_act(&state, &id, &act).to_string());
        }
        "/api/collection" => {
            let link = query_param(&query, "url").unwrap_or_default();
            let body = collection_json(&state, &link).unwrap_or_else(|e| json!({ "error": e }));
            return respond_json(req, 200, body.to_string());
        }
        "/api/mod_page" => {
            let id = query_param(&query, "id").unwrap_or_default();
            return match mod_page_json(&state, &id) {
                Ok(v) => respond_json(req, 200, v.to_string()),
                Err(e) => respond_json(req, 200, json!({ "error": e }).to_string()),
            };
        }
        "/api/ue4ss_log" => {
            let txt = selected_game(&state).map(|g| ModManager::read_ue4ss_log_tail(&g, 300)).unwrap_or_default();
            return respond_json(req, 200, json!({ "log": txt }).to_string());
        }
        "/api/quit_now" => {
            let _ = req.respond(Response::from_string("{}"));
            std::process::exit(0);
        }
        "/api/quit" => {
            let _ = req.respond(Response::from_string("{}"));
            let exit_ts = now_ms();
            let keep = read_state(&state).config.keep_background;
            thread::spawn(move || {
                // A page reload fires `pagehide` too; only exit if no client came back.
                thread::sleep(Duration::from_millis(1500));
                if !keep && notifier.last_client_ping_ms.load(Ordering::SeqCst) <= exit_ts {
                    std::process::exit(0);
                }
            });
            return;
        }
        _ => {}
    }

    let mut body_str = String::new();
    let _ = req.as_reader().take(MAX_JSON_BODY).read_to_string(&mut body_str);
    let body: Value = serde_json::from_str(&body_str).unwrap_or_else(|_| json!({}));

    if path == "/api/mod_config_get" {
        let payload = find_mod(&state, &body)
            .and_then(|(_, m)| {
                let cfg_p = ModManager::find_mod_config_file(&m)?;
                let content = fs::read_to_string(&cfg_p).ok()?;
                Some(json!({
                    "mod_name": m.name,
                    "file_name": cfg_p.file_name().unwrap_or_default().to_string_lossy(),
                    "content": content
                }))
            })
            .unwrap_or_else(|| json!({ "error": "No config file found for this mod." }));
        return respond_json(req, 200, payload.to_string());
    }

    if !dispatch_action(&path, &body, &state) {
        return respond_json(req, 404, json!({ "error": "unknown endpoint" }).to_string());
    }
    respond_state(req, &state);
}

fn handle_drop_upload(mut req: Request, query: &str, state: &SharedState) {
    if *req.method() != Method::Post {
        return respond_json(req, 405, json!({ "error": "POST required" }).to_string());
    }
    let decoded = query_param(query, "name").unwrap_or_else(|| "dropped_mod.zip".to_string());
    let safe_name = Path::new(&decoded)
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .filter(|n| !n.is_empty())
        .unwrap_or_else(|| "dropped_mod.zip".to_string());

    let cache_root = read_state(state).cache_root();
    let drop_dir = cache_root.join("drops").join(fsutil::unique_token());
    let target = drop_dir.join(&safe_name);

    // a file that cannot fit is refused before any of it is written
    if let (Some(len), Some(free)) = (req.body_length(), fsutil::free_bytes(&cache_root)) {
        if (len as u64).saturating_add(256 << 20) > free {
            let msg = format!("Not enough free space for {}: it needs about {} and only {} is free there. Free some space and try again.", safe_name, fsutil::size_text((len as u64).saturating_add(256 << 20)), fsutil::size_text(free));
            log(state, "ERR", msg.clone());
            return respond_json(req, 413, json!({ "error": msg }).to_string());
        }
    }
    // Stream the body straight to disk; large mods are never buffered in memory.
    let saved = fs::create_dir_all(&drop_dir)
        .and_then(|_| File::create(&target))
        .and_then(|mut f| {
            let n = std::io::copy(req.as_reader(), &mut f)?;
            f.sync_all()?;
            Ok(n)
        });
    match saved {
        Ok(n) if n > 0 && is_setup_file(&safe_name, &target) => {
            // a shared setup dropped on the window: show what it holds, fetch nothing yet
            let state_bg = Arc::clone(state);
            thread::spawn(move || {
                let res = selected_game(&state_bg).filter(|g| g.is_installed).ok_or_else(|| "Select an installed game first.".to_string()).and_then(|g| read_pack_file(&state_bg, &target).and_then(|(spec, files)| preview_import(&state_bg, &g, spec, files)));
                let _ = fs::remove_dir_all(&drop_dir);
                if let Err(e) = res {
                    log(&state_bg, "ERR", e);
                }
            });
            respond_state(req, state);
        }
        Ok(n) if n > 0 => {
            let state_bg = Arc::clone(state);
            let to_lib = query_param(query, "lib").as_deref() == Some("1");
            queue_install(&state_bg, target, true, InstallOpts { to_library: to_lib, ..Default::default() });
            respond_state(req, state);
        }
        other => {
            let _ = fs::remove_dir_all(&drop_dir);
            let msg = match other {
                Ok(_) => "Empty upload.".to_string(),
                Err(e) => format!("Upload failed: {}", e),
            };
            log(state, "ERR", msg.clone());
            respond_json(req, 400, json!({ "error": msg }).to_string());
        }
    }
}

/// A modpack or setup file of Gantry's, by its name or archive contents.
fn is_setup_file(name: &str, path: &Path) -> bool {
    let n = name.to_lowercase();
    if n.ends_with(".gantrypack") || n.ends_with(".gantry.json") || n.ends_with(".modpack.zip") || n.ends_with(".modpack") {
        return true;
    }
    if n.ends_with(".zip") {
        if let Ok(mut f) = fs::File::open(path) {
            let mut b = [0u8; 4];
            if std::io::Read::read(&mut f, &mut b).unwrap_or(0) >= 4 && &b == b"PK\x03\x04" {
                let tool = if platform::IS_WINDOWS { "tar" } else { "bsdtar" };
                let alt = if platform::IS_WINDOWS { "bsdtar" } else { "tar" };
                let chosen = [tool, alt].into_iter().find(|t| fsutil::find_in_path(t).is_some());
                if let Some(t) = chosen {
                    if let Ok(out) = Command::new(t).arg("-tf").arg(path).output() {
                        let listing = String::from_utf8_lossy(&out.stdout);
                        if listing.lines().any(|l| l.trim().trim_start_matches("./") == "pack.json") {
                            return true;
                        }
                    }
                }
            }
        }
    }
    false
}

/// Resolve a mod from the request: stable `id` first, positional `index` as legacy fallback.
fn find_mod(state: &SharedState, body: &Value) -> Option<(GameEntry, ModEntry)> {
    let st = read_state(state);
    let g = st.games.get(st.selected_idx)?.clone();
    let m = if let Some(id) = body.get("id").and_then(|v| v.as_str()) {
        st.mods.iter().find(|m| m.id == id)?.clone()
    } else {
        let i = body.get("index").and_then(|v| v.as_u64())? as usize;
        st.mods.get(i)?.clone()
    };
    Some((g, m))
}

/// A list of strings from the request (at most 500, so a bad request cannot ask for the moon).
fn body_strings(body: &Value, key: &str) -> Vec<String> {
    body.get(key).and_then(Value::as_array).map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_string)).take(500).collect()).unwrap_or_default()
}

fn body_str(body: &Value, key: &str) -> String {
    body.get(key).and_then(|v| v.as_str()).unwrap_or("").trim().to_string()
}

/// Execute a state-changing action. Returns false for unknown endpoints.
/// Rule: never run filesystem walks, subprocesses or dialogs while holding the state lock.
fn dispatch_action(path: &str, body: &Value, state: &SharedState) -> bool {
    match path {
        "/api/select" => {
            if let Some(idx) = body.get("index").and_then(|v| v.as_u64()) {
                {
                    let mut st = write_state(state);
                    if (idx as usize) < st.games.len() {
                        st.selected_idx = idx as usize;
                        st.mods.clear();
                        st.notifier.bump();
                    }
                }
                refresh_mods_shared(state);
            }
        }
        "/api/select_proton" => {
            if let Some(idx) = body.get("index").and_then(|v| v.as_u64()) {
                let mut st = write_state(state);
                if (idx as usize) < st.protons.len() {
                    st.selected_proton_idx = idx as usize;
                }
                st.notifier.bump();
            }
        }
        "/api/set_steam_appid" => {
            let sid = body_str(body, "steam_appid");
            if sid.is_empty() || sid.bytes().all(|b| b.is_ascii_digit()) {
                let mut st = write_state(state);
                let idx = st.selected_idx;
                let appid_opt = st.games.get(idx).map(|g| g.appid.clone());
                if let Some(appid) = appid_opt {
                    if let Some(g) = st.games.get_mut(idx) {
                        g.steam_appid = sid.clone();
                    }
                    if sid.is_empty() {
                        st.config.cover_appid.remove(&appid);
                    } else {
                        st.config.cover_appid.insert(appid, sid.clone());
                    }
                    st.save_config();
                    st.save_custom_games();
                    st.notifier.bump();
                    st.log("OK", format!("Updated Cover Steam AppID to '{}'.", sid));
                }
            } else {
                log(state, "WARN", "Steam AppID must be numeric.");
            }
        }
        "/api/toggle_flag" => {
            if let Some(f) = body.get("flag").and_then(|v| v.as_str()) {
                let mut st = write_state(state);
                match f {
                    "gamemode" => st.gamemode = !st.gamemode,
                    "mangohud" => st.mangohud = !st.mangohud,
                    "auto_trainer" => st.auto_trainer = !st.auto_trainer,
                    _ => {}
                }
                st.notifier.bump();
            }
        }
        "/api/clear_logs" => {
            let mut st = write_state(state);
            st.logs.clear();
            st.last_toast.clear();
            st.notifier.bump();
        }
        "/api/toggle_mod" => {
            if let (Some((g, m)), Some(en)) = (find_mod(state, body), body.get("enable").and_then(|v| v.as_bool())) {
                let before = snapshot_enabled(state);
                if ModManager::toggle_mod(&g, &m, en) {
                    let (label, msg) = if en {
                        (format!("Enabled {}", m.name), format!("{} mod '{}' enabled.", m.mod_type, m.name))
                    } else {
                        (format!("Disabled {}", m.name), format!("{} mod '{}' disabled.", m.mod_type, m.name))
                    };
                    push_undo(state, &g.appid, label, before, vec![]);
                    log(state, "OK", msg);
                } else {
                    log(state, "WARN", format!("Could not change '{}' (already in that state or not writable).", m.name));
                }
                refresh_mods_shared(state);
            }
        }
        "/api/bulk_toggle_mods" => {
            let enable = body.get("enable").and_then(|v| v.as_bool()).unwrap_or(true);
            if let Some(g) = selected_game(state) {
                let before = snapshot_enabled(state);
                let changed = ModManager::bulk_toggle_user_mods(&g, enable);
                log(
                    state,
                    "OK",
                    if enable { format!("Enabled {} user mod(s).", changed) } else { format!("Disabled {} user mod(s).", changed) },
                );
                if changed > 0 {
                    push_undo(state, &g.appid, if enable { "Enabled all mods" } else { "Disabled all mods" }.into(), before, vec![]);
                }
                refresh_mods_shared(state);
            }
        }
        "/api/profile_save" => {
            let pname = body_str(body, "name");
            if pname.is_empty() {
                log(state, "WARN", "Profile name cannot be empty.");
            } else {
                let mut st = write_state(state);
                if let Some(g) = st.games.get(st.selected_idx).cloned() {
                    let enabled_ids: Vec<String> =
                        st.mods.iter().filter(|m| !m.is_system && m.enabled).map(|m| m.id.clone()).collect();
                    st.config.profiles.entry(g.appid).or_default().insert(pname.clone(), enabled_ids);
                    st.save_config();
                    st.notifier.bump();
                    st.log("OK", format!("Saved mod profile '{}'.", pname));
                }
            }
        }
        "/api/profile_load" => {
            let pname = body_str(body, "name");
            let (game_opt, ids_opt) = {
                let st = read_state(state);
                let g = st.games.get(st.selected_idx).cloned();
                let ids = g
                    .as_ref()
                    .and_then(|gm| st.config.profiles.get(&gm.appid))
                    .and_then(|m| m.get(&pname))
                    .cloned();
                (g, ids)
            };
            match (game_opt, ids_opt) {
                (Some(g), Some(ids)) => {
                    let before = snapshot_enabled(state);
                    let changed = ModManager::apply_profile(&g, &ids);
                    if changed > 0 {
                        push_undo(state, &g.appid, format!("Loaded profile '{}'", pname), before, vec![]);
                    }
                    refresh_mods_shared(state);
                    log(state, "OK", format!("Loaded profile '{}' ({} mods updated).", pname, changed));
                }
                _ => log(state, "WARN", format!("Profile '{}' not found.", pname)),
            }
        }
        "/api/profile_delete" => {
            let pname = body_str(body, "name");
            if pname.is_empty() {
                log(state, "WARN", "Profile name cannot be empty.");
            } else {
                let mut st = write_state(state);
                if let Some(g) = st.games.get(st.selected_idx).cloned() {
                    let removed = st.config.profiles.get_mut(&g.appid).map(|m| m.remove(&pname).is_some()).unwrap_or(false);
                    if removed {
                        st.save_config();
                        st.notifier.bump();
                        st.log("WARN", format!("Deleted mod profile '{}'.", pname));
                    }
                }
            }
        }
        "/api/backup_saves" | "/api/saves_backup" => {
            let (game_opt, cache_root) = {
                let st = read_state(state);
                (st.games.get(st.selected_idx).cloned(), st.cache_root())
            };
            if let Some(g) = game_opt {
                match saves::snapshot(&g, &cache_root, "manual", &save_places(state, &g)) {
                    Ok(Some(s)) => log(state, "OK", format!("Backed up {} save file(s) for '{}'.", s.files, g.name)),
                    Ok(None) => log(state, "INFO", "Your saves have not changed since the last backup."),
                    Err(e) => log(state, "WARN", e),
                }
            }
        }
        "/api/restore_saves" | "/api/saves_restore" => {
            let (game_opt, cache_root) = {
                let st = read_state(state);
                (st.games.get(st.selected_idx).cloned(), st.cache_root())
            };
            if let Some(g) = game_opt {
                // no id given (old button): the newest backup
                let id = body_str(body, "id");
                let id = if id.is_empty() { saves::list(&g, &cache_root).first().map(|s| s.id.clone()).unwrap_or_default() } else { id };
                match saves::restore(&g, &cache_root, &id, &save_places(state, &g)) {
                    Ok(m) => log(state, "OK", m),
                    Err(e) => log(state, "WARN", e),
                }
            }
        }
        "/api/saves_restore_file" => {
            let (game_opt, cache_root) = {
                let st = read_state(state);
                (st.games.get(st.selected_idx).cloned(), st.cache_root())
            };
            if let Some(g) = game_opt {
                let custom = save_places(state, &g);
                match saves::restore_file(&g, &cache_root, &body_str(body, "id"), &body_str(body, "key"), &body_str(body, "path"), &custom) {
                    Ok(m) => log(state, "OK", m),
                    Err(e) => log(state, "WARN", e),
                }
            }
        }
        "/api/clear_shaders" => {
            if let Some(g) = selected_game(state) {
                let count = RepairEngine::clear_shader_cache(&g);
                log(state, "OK", format!("Purged {} stale DXVK/VKD3D shader cache file(s).", count));
            }
        }
        "/api/clean_prefix_caches" => {
            if let Some(g) = selected_game(state) {
                let (count, freed) = RepairEngine::clean_prefix_caches(&g);
                if count > 0 {
                    log(state, "OK", format!("Cleaned {} prefix cache and dump file(s) ({}) freed.", count, fsutil::size_text(freed)));
                } else {
                    log(state, "INFO", "Prefix is already clean; no stale pipeline caches or crash dumps found.");
                }
            }
        }
        "/api/kill_wine" | "/api/stop" => {
            let (game, pid, proton, running) = {
                let st = read_state(state);
                let g = st.games.get(st.selected_idx).cloned();
                let proton = g.as_ref().and_then(|g| st.proton_for(g));
                (g, st.running_pid.swap(0, Ordering::SeqCst), proton, Arc::clone(&st.is_running))
            };
            stop_game_processes(game.as_ref(), pid, proton.as_deref());
            running.store(false, Ordering::SeqCst);
            if path == "/api/stop" {
                log(state, "WARN", "Stopped running game processes.");
            } else {
                log(state, "WARN", "Terminated Wine prefix server & background processes.");
            }
        }
        "/api/delete_mod" => {
            if let Some((g, m)) = find_mod(state, body) {
                let before = snapshot_enabled(state);
                let trash = read_state(state).cache_root().join("trash");
                match ModManager::trash_mod(&g, &m, &trash) {
                    Ok(items) => {
                        log(state, "WARN", format!("Deleted mod '{}'. Use Undo to bring it back.", m.name));
                        push_undo(state, &g.appid, format!("Deleted {}", m.name), before, items);
                    }
                    Err(e) => log(state, "WARN", e),
                }
                refresh_mods_shared(state);
            }
        }
        "/api/modio_key_set" => {
            let key = body_str(body, "key");
            let state_bg = Arc::clone(state);
            thread::spawn(move || match set_modio_key(&state_bg, &key) {
                Ok(m) => log(&state_bg, "OK", m),
                Err(e) => log(&state_bg, "ERR", e),
            });
        }
        "/api/modio_key_clear" => {
            clear_modio_key(state);
            log(state, "WARN", "mod.io API key removed from this computer.");
        }
        "/api/place_rules_clear" => {
            let mut st = write_state(state);
            if let Some(g) = st.games.get(st.selected_idx).cloned() {
                if st.config.place_rules.remove(&g.appid).is_some() {
                    st.save_config();
                    st.notifier.bump();
                    st.log("OK", format!("Gantry forgot where you said mods for {} go; it asks again.", g.name));
                }
            }
        }
        "/api/core_install" => {
            // one core mod (`name`) or several (`names`), one after the other
            let mut names: Vec<String> = body.get("names").and_then(Value::as_array).map(|a| a.iter().filter_map(|x| x.as_str().map(str::to_string)).take(16).collect()).unwrap_or_default();
            if names.is_empty() {
                names.push(body_str(body, "name"));
            }
            if let Some(g) = selected_game(state).filter(|g| g.is_installed) {
                let known = frameworks::cores(Path::new(&g.game_dir));
                if names.iter().all(|n| known.iter().any(|c| c.name == n)) {
                    let state_bg = Arc::clone(state);
                    thread::spawn(move || {
                        for n in names {
                            install_missing_cores(&state_bg, &g, Some(&n));
                        }
                    });
                } else {
                    log(state, "WARN", "Gantry does not know that core mod for this game.");
                }
            }
        }
        "/api/game_update_ok" => {
            if let Some(g) = selected_game(state) {
                accept_game_build(state, &g);
                write_state(state).notifier.bump();
            }
        }
        "/api/batch_dismiss" => {
            let mut st = write_state(state);
            if st.batch.as_ref().is_some_and(|b| b.finished) {
                st.batch = None;
                st.notifier.bump();
            }
        }
        "/api/inbox_add" | "/api/inbox_hide" => {
            let names: Vec<String> = body.get("names").and_then(Value::as_array).map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_string)).take(40).collect()).unwrap_or_default();
            let install = path == "/api/inbox_add";
            let state_bg = Arc::clone(state);
            thread::spawn(move || inbox_take(&state_bg, &names, install));
        }
        "/api/modio_game_set" => {
            let id: u64 = body.get("id").and_then(|v| v.as_u64().or_else(|| v.as_str().and_then(|s| s.parse().ok()))).unwrap_or(0);
            if id == 0 {
                log(state, "WARN", "Invalid mod.io game ID.");
            } else if let Some(g) = selected_game(state).filter(|g| g.is_installed) {
                let state_bg = Arc::clone(state);
                thread::spawn(move || match modio_game_set(&state_bg, &g, id) {
                    Ok(m) => log(&state_bg, "OK", m),
                    Err(e) => log(&state_bg, "ERR", e),
                });
            } else {
                log(state, "WARN", "Select an installed game first.");
            }
        }
        "/api/modio_install" => {
            let id: u64 = body.get("id").and_then(|v| v.as_u64().or_else(|| v.as_str().and_then(|s| s.parse().ok()))).unwrap_or(0);
            if id == 0 {
                log(state, "WARN", "Invalid mod.io mod ID.");
            } else if let Some(g) = selected_game(state).filter(|g| g.is_installed) {
                let state_bg = Arc::clone(state);
                thread::spawn(move || match modio_install(&state_bg, &g, id) {
                    Ok(m) => log(&state_bg, "OK", m),
                    Err(e) => log(&state_bg, "ERR", e),
                });
            } else {
                log(state, "WARN", "Select an installed game first.");
            }
        }
        "/api/nexus_key_set" => {
            let key = body_str(body, "key");
            let state_bg = Arc::clone(state);
            thread::spawn(move || match set_nexus_key(&state_bg, &key) {
                Ok(m) => log(&state_bg, "OK", m),
                Err(e) => log(&state_bg, "ERR", e),
            });
        }
        "/api/nexus_key_clear" => {
            clear_nexus_key(state);
            log(state, "WARN", "Nexus API key removed from this computer.");
        }
        "/api/nexus_check" => {
            let state_bg = Arc::clone(state);
            thread::spawn(move || {
                let has_key = read_state(&state_bg).nexus.key.is_some();
                let nexus = if has_key { Some(check_nexus_updates(&state_bg)) } else { None };
                let thunder = check_thunder_updates(&state_bg);
                let mut total = 0;
                for r in [nexus, Some(thunder)].into_iter().flatten() {
                    match r {
                        Ok(n) => total += n,
                        Err(e) => log(&state_bg, "ERR", e),
                    }
                }
                if total == 0 && !has_key_error(&state_bg) {
                    log(&state_bg, "OK", "All mods are up to date.");
                }
            });
        }
        "/api/thunder_install" => {
            let input = body_str(body, "input");
            let state_bg = Arc::clone(state);
            thread::spawn(move || {
                let Some(g) = selected_game(&state_bg) else { return };
                match thunder_install(&state_bg, &g, &input) {
                    Ok(m) => log(&state_bg, "OK", m),
                    Err(e) => log(&state_bg, "ERR", e),
                }
            });
        }
        "/api/thunder_update" => {
            let (appid, id) = (body_str(body, "appid"), body_str(body, "id"));
            let state_bg = Arc::clone(state);
            thread::spawn(move || {
                let row = read_state(&state_bg).thunder.updates.iter().find(|u| u.appid == appid && u.lib_id == id).cloned();
                let game = read_state(&state_bg).games.iter().find(|g| g.appid == appid).cloned();
                match (row, game) {
                    (Some(r), Some(g)) => match thunder_install(&state_bg, &g, &r.pkg.full_name()) {
                        Ok(m) => log(&state_bg, "OK", m),
                        Err(e) => log(&state_bg, "ERR", e),
                    },
                    (None, Some(g)) => {
                        // looked up first: a lock taken inside the `match` line would be held while the update runs
                        let found = read_state(&state_bg).github_updates.iter().find(|u| u.appid == appid && u.lib_id == id).cloned();
                        let from_modio = read_state(&state_bg).modio_updates.iter().find(|u| u.appid == appid && u.lib_id == id).cloned();
                        match (found, from_modio) {
                            (Some(u), _) => github_update_apply(&state_bg, &g, &u),
                            (None, Some(u)) => modio_update_apply(&state_bg, &g, &u),
                            (None, None) => log(&state_bg, "WARN", "That update is no longer listed. Check for updates again."),
                        }
                    }
                    _ => log(&state_bg, "WARN", "That update is no longer listed. Check for updates again."),
                }
            });
        }
        "/api/update_all" => {
            let state_bg = Arc::clone(state);
            thread::spawn(move || update_all(&state_bg));
        }
        "/api/nexus_update" => {
            let (appid, id) = (body_str(body, "appid"), body_str(body, "id"));
            let state_bg = Arc::clone(state);
            thread::spawn(move || nexus_update(&state_bg, &appid, &id));
        }
        "/api/nxm" => match nxm_request(state, &body_str(body, "url")) {
            Ok(m) => log(state, "INFO", m),
            Err(e) => log(state, "WARN", e),
        },
        "/api/nexus_queue" => {
            let action = body_str(body, "action");
            let state_bg = Arc::clone(state);
            thread::spawn(move || nexus_queue_action(&state_bg, &action));
        }
        "/api/nxm_confirm" => {
            let accept = body.get("accept").and_then(|v| v.as_bool()).unwrap_or(false);
            let state_bg = Arc::clone(state);
            thread::spawn(move || nxm_confirm(&state_bg, accept));
        }
        "/api/nxm_register" | "/api/nxm_unregister" => {
            let (home, cfg) = (home_dir(), read_state(state).paths.config_file.parent().map(Path::to_path_buf).unwrap_or_default());
            let exe = std::env::current_exe().unwrap_or_default();
            let res = if path == "/api/nxm_register" { install::register_nxm(&home, &cfg, &exe) } else { Ok(install::unregister_nxm(&home, &cfg)) };
            match res {
                Ok(m) => log(state, "OK", m),
                Err(e) => log(state, "ERR", e),
            }
        }
        "/api/undo" => match undo_last(state) {
            Ok(m) => log(state, "OK", m),
            Err(e) => log(state, "WARN", e),
        }
        "/api/ue4ss_tuner_save" => {
            let Some(g) = selected_game(state) else { return true; };
            match serde_json::from_value::<Ue4ssSettingsDto>(body.clone()) {
                Ok(dto) => match ModManager::save_ue4ss_settings(&g, &dto) {
                    Ok(()) => log(
                        state,
                        "UE5",
                        format!(
                            "Updated UE4SS Tuner (GUI Console={}, GraphicsAPI=dx11, UE={}.{}).",
                            dto.gui_console, dto.major_version, dto.minor_version
                        ),
                    ),
                    Err(e) => log(state, "ERR", e),
                },
                Err(e) => log(state, "WARN", format!("Invalid UE4SS settings: {}", e)),
            }
        }
        "/api/mod_config_save" => {
            let content = body.get("content").and_then(|v| v.as_str()).unwrap_or("").to_string();
            match find_mod(state, body) {
                Some((_, m)) => match ModManager::find_mod_config_file(&m) {
                    Some(cfg_p) => match atomic_write(&cfg_p, content) {
                        Ok(()) => {
                            let fname = cfg_p.file_name().unwrap_or_default().to_string_lossy().to_string();
                            log(state, "OK", format!("Saved config for '{}' ({}).", m.name, fname));
                        }
                        Err(e) => log(state, "ERR", format!("Could not save config for '{}': {}", m.name, e)),
                    },
                    None => log(state, "WARN", format!("'{}' has no editable config.", m.name)),
                },
                None => log(state, "WARN", "Could not find that mod to save config."),
            }
        }
        "/api/self_heal" => {
            let state_bg = Arc::clone(state);
            thread::spawn(move || {
                if let Some(g) = selected_game(&state_bg) {
                    for a in RepairEngine::repair(&g) {
                        log(&state_bg, "OK", a);
                    }
                    refresh_mods_shared(&state_bg);
                }
            });
        }
        "/api/setup_ue4ss" => {
            let state_bg = Arc::clone(state);
            thread::spawn(move || setup_ue4ss(&state_bg));
        }
        "/api/add_game_dialog" => {
            let state_bg = Arc::clone(state);
            let lang = read_state(state).config.language.clone();
            thread::spawn(move || {
                let Some(p) = rfd::FileDialog::new().add_filter(i18n::tr(&lang, "Windows Executable"), &["exe"]).pick_file() else {
                    return;
                };
                add_custom_game(&state_bg, &p);
            });
        }
        "/api/remove_game" => {
            // A custom game is forgotten; any other game is only hidden from the list (files are never touched).
            let removed = {
                let mut st = write_state(state);
                let wanted = body_str(body, "appid");
                let idx = if wanted.is_empty() { Some(st.selected_idx) } else { st.games.iter().position(|g| g.appid == wanted) };
                match idx.and_then(|i| st.games.get(i).cloned().map(|g| (i, g))) {
                    Some((idx, g)) => {
                        let selected = st.games.get(st.selected_idx).map(|s| s.appid.clone());
                        st.games.remove(idx);
                        if g.is_custom {
                            st.save_custom_games();
                            st.log("WARN", format!("Removed custom game '{}'.", g.name));
                        } else {
                            st.config.hidden_games.insert(g.appid.clone(), g.name.clone());
                            st.save_config();
                            st.log("WARN", format!("Removed '{}' from the list.", g.name));
                        }
                        match selected {
                            Some(id) if id != g.appid => {
                                st.select_by_appid(&id);
                            }
                            _ => st.selected_idx = st.selected_idx.min(st.games.len().saturating_sub(1)),
                        }
                        st.mods.clear();
                        st.notifier.bump();
                        true
                    }
                    None => false,
                }
            };
            if removed {
                refresh_mods_shared(state);
            }
        }
        "/api/restore_game" => {
            let appid = body_str(body, "appid");
            let name = {
                let mut st = write_state(state);
                let name = st.config.hidden_games.remove(&appid);
                if name.is_some() {
                    st.save_config();
                }
                name
            };
            if let Some(name) = name {
                write_state(state).log("INFO", format!("Restored '{}' to the list.", name));
                let state_bg = Arc::clone(state);
                thread::spawn(move || rescan(&state_bg));
            }
        }
        "/api/trainer_use" | "/api/trainer_forget" => {
            let id = body_str(body, "id");
            let found = {
                let st = read_state(state);
                st.games.get(st.selected_idx).cloned().and_then(|g| {
                    let p = trainer_shelf(&st, &g.appid).into_iter().find(|p| !id.is_empty() && p.file_name().is_some_and(|n| n.to_string_lossy() == id))?;
                    Some((g, p))
                })
            };
            match (found, path) {
                (None, _) => log(state, "WARN", "That trainer is not kept for this game any more."),
                (Some((g, p)), "/api/trainer_use") => attach_trainer(state, &g.appid, &p, &trainer_display(&g.appid, &p)),
                (Some((g, p)), _) => {
                    let mut st = write_state(state);
                    let key = p.to_string_lossy().to_string();
                    if let Some(shelf) = st.config.trainer_shelf.get_mut(&g.appid) {
                        shelf.retain(|s| *s != key);
                    }
                    if st.config.trainers.get(&g.appid) == Some(&key) {
                        st.config.trainers.remove(&g.appid);
                        if let Some(gm) = st.games.iter_mut().find(|x| x.appid == g.appid) {
                            gm.trainer_name.clear();
                        }
                    }
                    st.save_config();
                    // Gantry's own copy goes; a file elsewhere (the user's Downloads) is only forgotten
                    if p.parent() == Some(trainers_dir(&st.paths.cache_dir).as_path()) {
                        let _ = fs::remove_file(&p);
                    }
                    st.notifier.bump();
                    st.log("OK", format!("Removed the trainer {} from {}.", trainer_display(&g.appid, &p), g.name));
                }
            }
        }
        "/api/detach_trainer" => {
            let mut st = write_state(state);
            let idx = st.selected_idx;
            if let Some(g) = st.games.get(idx).cloned() {
                st.config.trainers.remove(&g.appid);
                st.save_config();
                st.games[idx].trainer_name.clear();
                st.notifier.bump();
                st.log("WARN", format!("Detached trainer from '{}'.", g.name));
            }
        }
        "/api/install_dialog" => {
            let state_bg = Arc::clone(state);
            let lang = read_state(state).config.language.clone();
            thread::spawn(move || {
                if let Some(p) = rfd::FileDialog::new()
                    .add_filter(i18n::tr(&lang, "Mods & Trainers"), &["zip", "7z", "rar", "pak", "dll", "exe"])
                    .pick_file()
                {
                    install_mod_or_trainer_from_path(p, false, state_bg);
                }
            });
        }
        "/api/steam_key_set" => {
            let key = body_str(body, "key");
            let state_bg = Arc::clone(state);
            thread::spawn(move || match set_steam_key(&state_bg, &key) {
                Ok(m) => log(&state_bg, "OK", m),
                Err(e) => log(&state_bg, "ERR", e),
            });
        }
        "/api/steam_key_clear" => {
            clear_steam_key(state);
            log(state, "WARN", "Steam Web API key removed from this computer.");
        }
        "/api/steam_refresh" => {
            let state_bg = Arc::clone(state);
            thread::spawn(move || match refresh_steam_owned(&state_bg) {
                Ok(n) => log(&state_bg, "OK", format!("Steam library updated: {} games.", n)),
                Err(e) if e.is_empty() => {}
                Err(e) => log(&state_bg, "ERR", e),
            });
        }
        "/api/steam_account" => {
            let id = body_str(body, "steamid");
            {
                let mut st = write_state(state);
                st.config.steam_account = id;
                st.save_config();
                st.notifier.bump();
            }
            if read_state(state).steam_lib.key.is_some() {
                let state_bg = Arc::clone(state);
                thread::spawn(move || match refresh_steam_owned(&state_bg) {
                    Ok(n) => log(&state_bg, "OK", format!("Steam library updated: {} games.", n)),
                    Err(e) if e.is_empty() => {}
                    Err(e) => log(&state_bg, "ERR", e),
                });
            }
        }
        "/api/steam_install" => {
            let appid = body_str(body, "appid");
            if !appid.is_empty() && appid.bytes().all(|b| b.is_ascii_digit()) && !cfg!(test) {
                xdg_open(format!("steam://install/{}", appid));
            }
        }
        "/api/sync_folder_set" => {
            let path = body_str(body, "path");
            let own: Vec<PathBuf> = {
                let st = read_state(state);
                vec![st.paths.config_file.parent().map(Path::to_path_buf).unwrap_or_default(), st.cache_root()]
            };
            if path.trim().is_empty() {
                let mut st = write_state(state);
                st.config.sync_folder = String::new();
                st.save_config();
                st.notifier.bump();
            } else {
                match sync_folder_ok(&path, &own) {
                    Ok(p) => {
                        let mut st = write_state(state);
                        st.config.sync_folder = p;
                        st.save_config();
                        st.notifier.bump();
                        drop(st);
                        log(state, "OK", "The sync folder is set.");
                    }
                    Err(e) => log(state, "WARN", e),
                }
            }
        }
        "/api/sync_push" => match sync_push(state) {
            Ok(n) if n == "unchanged" => log(state, "INFO", "The sync folder already has your current setup."),
            Ok(_) => log(state, "OK", "Your setup was copied to the sync folder."),
            Err(e) => log(state, "WARN", e),
        },
        "/api/sync_pull" => {
            let state_bg = Arc::clone(state);
            thread::spawn(move || match sync_pull(&state_bg) {
                Ok(m) => log(&state_bg, "OK", m),
                Err(e) => log(&state_bg, "ERR", e),
            });
        }
        "/api/saved_toggle" => {
            // set a mod aside (or take it back out) for the selected game; the list is kept to 200
            if let (Some(card), Some(g)) = (saved_card(body), selected_game(state)) {
                let mut st = write_state(state);
                let list = st.config.saved_mods.entry(g.appid.clone()).or_default();
                match list.iter().position(|c| c["source"] == card["source"] && c["id"] == card["id"]) {
                    Some(i) => {
                        list.remove(i);
                    }
                    None if list.len() < 200 => list.insert(0, card),
                    None => {}
                }
                if list.is_empty() {
                    st.config.saved_mods.remove(&g.appid);
                }
                st.save_config();
                st.notifier.bump();
            }
        }
        "/api/workshop_folder" => {
            if let Some(f) = selected_game(state).and_then(|g| workshop::item_folder(Path::new(&g.game_dir), g.steam_id(), &body_str(body, "id"))) {
                xdg_open(f);
            }
        }
        "/api/open_link" => {
            let page = body_str(body, "url");
            let url = match body_str(body, "which").as_str() {
                "workshop" if !body_str(body, "id").is_empty() && body_str(body, "id").bytes().all(|b| b.is_ascii_digit()) => Some(format!("https://steamcommunity.com/sharedfiles/filedetails/?id={}", body_str(body, "id"))),
                "steam_key" => Some("https://steamcommunity.com/dev/apikey".to_string()),
                "docs" => docs_url(&body_str(body, "page")),
                "nexus_key" => Some("https://www.nexusmods.com/users/myaccount?tab=api".to_string()),
                // a mod page, only ever on Nexus Mods itself
                "nexus_page" if page.starts_with("https://www.nexusmods.com/") && !page.contains(char::is_whitespace) => Some(page),
                "thunder_page" if page.starts_with("https://thunderstore.io/") && !page.contains(char::is_whitespace) => Some(page),
                "modio_page" if (page.starts_with("https://mod.io/") || page.starts_with("https://www.mod.io/")) && !page.contains(char::is_whitespace) => Some(page),
                "modio_key" => Some("https://mod.io/me/access".to_string()),
                // a link from a mod's own page (the page only ever offers http and https ones)
                "web" => richtext::safe_url(&page),
                // a pre-filled issue form (the person reads it and presses Submit themselves)
                "report" => {
                    let os = if cfg!(target_os = "linux") { fs::read_to_string("/etc/os-release").ok().and_then(|t| t.lines().find_map(|l| l.strip_prefix("PRETTY_NAME=").map(|v| v.trim_matches('"').to_string()))).unwrap_or_else(|| platform::OS.to_string()) } else { platform::OS.to_string() };
                    let game = selected_game(state).map(|g| format!("{} ({})", g.name, if g.source.is_empty() { "Steam or custom".to_string() } else { g.source.clone() })).unwrap_or_default();
                    Some(issue_url(body_str(body, "kind") == "game", &format!("Gantry {} on {}", env!("CARGO_PKG_VERSION"), os), &game))
                }
                _ => None,
            };
            if let (Some(u), false) = (url, cfg!(test)) {
                xdg_open(u);
            }
        }
        "/api/set_language" => {
            let lang = body_str(body, "lang");
            if i18n::valid(&lang) {
                let mut st = write_state(state);
                if st.config.language != lang {
                    st.config.language = lang;
                    st.save_config();
                    st.notifier.bump();
                }
            }
        }
        "/api/lib_install_dialog" => {
            let state_bg = Arc::clone(state);
            let lang = read_state(state).config.language.clone();
            thread::spawn(move || {
                if let Some(p) = rfd::FileDialog::new().set_title(i18n::tr(&lang, "Add a mod to the library")).pick_file() {
                    install_to_library_from_path(p, false, state_bg);
                }
            });
        }
        "/api/lib_toggle" => {
            let (id, on) = (body_str(body, "id"), body.get("enabled").and_then(|v| v.as_bool()).unwrap_or(true));
            library_task(state, move |st, lib, _| {
                lib.set_enabled(&id, on)?;
                library_apply(st, lib, false)
            });
        }
        "/api/lib_move" => {
            let (id, d) = (body_str(body, "id"), body.get("delta").and_then(|v| v.as_i64()).unwrap_or(0) as i32);
            library_task(state, move |st, lib, _| {
                lib.move_mod(&id, d)?;
                library_apply(st, lib, false)
            });
        }
        "/api/download_cancel" => {
            if let Some(id) = body.get("id").and_then(Value::as_u64) {
                downloads::cancel(id);
            }
        }
        "/api/downloads_clear" => downloads::clear_finished(),
        "/api/background_run" => {
            let name = body.get("name").and_then(Value::as_str).unwrap_or("");
            if !background::run_now(name) {
                log(state, "INFO", "That check is already running.");
            }
        }
        "/api/lib_move_to" => {
            let (id, to) = (body_str(body, "id"), body.get("to").and_then(|v| v.as_u64()).unwrap_or(0) as usize);
            library_task(state, move |st, lib, _| {
                lib.move_to(&id, to)?;
                library_apply(st, lib, false)
            });
        }
        "/api/lib_toggle_many" => {
            let (ids, on) = (body_strings(body, "ids"), body.get("enabled").and_then(|v| v.as_bool()).unwrap_or(true));
            library_task(state, move |st, lib, _| {
                let n = lib.set_enabled_many(&ids, on)?;
                log(st, "OK", if on { format!("{} mod(s) turned on.", n) } else { format!("{} mod(s) turned off.", n) });
                library_apply(st, lib, false)
            });
        }
        "/api/lib_group" => {
            let (ids, group) = (body_strings(body, "ids"), body_str(body, "group"));
            library_task(state, move |st, lib, _| {
                let n = lib.set_group(&ids, &group)?;
                log(st, "OK", format!("{} mod(s) tagged.", n));
                library_apply(st, lib, false)
            });
        }
        "/api/lib_move_many_to" => {
            let (ids, to_top) = (body_strings(body, "ids"), body.get("top").and_then(|v| v.as_bool()).unwrap_or(false));
            library_task(state, move |st, lib, _| {
                if to_top {
                    for (idx, id) in ids.iter().enumerate() {
                        let _ = lib.move_to(id, idx);
                    }
                } else {
                    let total = lib.mods().len();
                    for id in ids.iter().rev() {
                        let _ = lib.move_to(id, total.saturating_sub(1));
                    }
                }
                log(st, "OK", if to_top { "Moved selected mods to top of load order." } else { "Moved selected mods to bottom of load order." });
                library_apply(st, lib, false)
            });
        }
        "/api/lib_update_rule_many" => {
            let (ids, rule) = (body_strings(body, "ids"), body_str(body, "rule"));
            library_task(state, move |st, lib, _| {
                let mut count = 0;
                for id in &ids {
                    if lib.set_update_rule(id, &rule).is_ok() {
                        count += 1;
                    }
                }
                log(st, "OK", format!("Update rule set for {} mod(s).", count));
                library_apply(st, lib, false)
            });
        }
        "/api/lib_target_many" => {
            let (ids, target) = (body_strings(body, "ids"), body_str(body, "target"));
            library_task(state, move |st, lib, _| {
                let mut count = 0;
                for id in &ids {
                    if lib.set_target(id, &target).is_ok() {
                        count += 1;
                    }
                }
                log(st, "OK", format!("Target folder set for {} mod(s).", count));
                library_apply(st, lib, false)
            });
        }
        "/api/lib_remove_many" => {
            let ids = body_strings(body, "ids");
            library_task(state, move |st, lib, _| {
                running_guard(st)?;
                let n = lib.remove_many(&ids)?;
                log(st, "OK", format!("{} mod(s) removed.", n));
                library_apply(st, lib, false)
            });
        }
        "/api/lib_restore_many" => {
            let dirs = body_strings(body, "dirs");
            library_task(state, move |st, lib, _| {
                running_guard(st)?;
                let mut names = Vec::new();
                for d in &dirs {
                    names.push(lib.restore_trash(d)?);
                }
                log(st, "OK", format!("{} mod(s) are back in the mod library.", names.len()));
                library_apply(st, lib, false)
            });
        }
        "/api/lib_target" => {
            let (id, t) = (body_str(body, "id"), body_str(body, "target"));
            library_task(state, move |st, lib, _| {
                lib.set_target(&id, &t)?;
                library_apply(st, lib, false)
            });
        }
        "/api/loader_install" => {
            let kind = body_str(body, "kind");
            let state_bg = Arc::clone(state);
            thread::spawn(move || install_loader(&state_bg, &kind));
        }
        "/api/config_write" | "/api/config_restore" => {
            let (rel, text) = (body_str(body, "path"), body_str(body, "text"));
            let restore = path == "/api/config_restore";
            let r = selected_game(state).filter(|g| g.is_installed).ok_or_else(|| "Select an installed game first.".to_string()).and_then(|g| {
                let dir = PathBuf::from(&g.game_dir);
                if restore { configs::restore(&dir, &rel) } else { configs::write(&dir, &rel, &text) }
            });
            match r {
                Ok(()) => log(state, "OK", if restore { format!("Restored the original of {rel}.") } else { format!("Saved {rel}.") }),
                Err(e) => log(state, "ERR", e),
            }
        }
        "/api/thunder_community" => {
            let name = body_str(body, "name").trim().to_lowercase();
            let valid = !name.is_empty() && name.len() <= 64 && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
            if let (true, Some(g)) = (valid, selected_game(state)) {
                let mut st = write_state(state);
                st.config.thunder_community.insert(g.appid, name);
                st.save_config();
                st.notifier.bump();
                st.log("OK", "Saved Thunderstore community.");
            } else {
                log(state, "WARN", "That does not look like a Thunderstore community name.");
            }
        }
        "/api/install_confirm" => {
            let accept = body.get("accept").and_then(Value::as_bool).unwrap_or(false);
            // a folder given (even "", the game folder itself) beats Gantry's guess
            let target = body.get("target").and_then(Value::as_str).map(|t| t.trim().to_string());
            let state_bg = Arc::clone(state);
            thread::spawn(move || install_confirm(&state_bg, accept, target.as_deref()));
        }
        "/api/disk_clean" => {
            let what = body_str(body, "what");
            let state_bg = Arc::clone(state);
            thread::spawn(move || {
                let msg = disk_clean(&state_bg, &what);
                log(&state_bg, "OK", msg);
            });
        }
        "/api/vt_key_set" => {
            let key = body_str(body, "key").trim().to_string();
            let mut st = write_state(state);
            if !virustotal::key_ok(&key) {
                st.log("ERR", "That does not look like a VirusTotal key.");
            } else {
                match fsutil::atomic_write_private(&vt_key_file(&st), &key) {
                    Ok(()) => {
                        st.vt.key = Some(key);
                        st.log("OK", "VirusTotal key saved.");
                    }
                    Err(e) => st.log("ERR", format!("Could not save the key: {}", e)),
                }
            }
            st.notifier.bump();
        }
        "/api/vt_key_clear" => {
            let mut st = write_state(state);
            clear_key_file_with_migration(&vt_key_file(&st), "virustotal_api_key");
            st.vt.key = None;
            st.vt.reports.clear();
            st.notifier.bump();
        }
        "/api/vt_check" => {
            let id = body_str(body, "id");
            let state_bg = Arc::clone(state);
            thread::spawn(move || {
                let Some(g) = selected_game(&state_bg) else { return };
                match vt_check_mod(&state_bg, &g, &id) {
                    Ok(m) if !m.is_empty() => log(&state_bg, "OK", m),
                    Ok(_) => {}
                    Err(e) => log(&state_bg, "ERR", e),
                }
            });
        }
        "/api/perf_open" => {
            let state_bg = Arc::clone(state);
            let lang = read_state(state).config.language.clone();
            thread::spawn(move || {
                let Some(path) = rfd::FileDialog::new().set_title(i18n::tr(&lang, "Open a MangoHud log")).add_filter("MangoHud", &["csv"]).pick_file() else { return };
                match perf::parse_file(&path) {
                    Ok(f) => {
                        let mut st = write_state(&state_bg);
                        st.perf_manual = Some((path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default(), f));
                        st.notifier.bump();
                    }
                    Err(e) => log(&state_bg, "ERR", e),
                }
            });
        }
        "/api/perf_forget" => {
            let mut st = write_state(state);
            st.perf_manual = None;
            st.notifier.bump();
        }
        "/api/auto_backup_now" => {
            let state_bg = Arc::clone(state);
            thread::spawn(move || match run_auto_backup(&state_bg) {
                Ok(_) => log(&state_bg, "OK", "Backup made."),
                Err(e) => log(&state_bg, "ERR", e),
            });
        }
        "/api/auto_backup_restore" => {
            let name = body_str(body, "name");
            let state_bg = Arc::clone(state);
            thread::spawn(move || {
                let dir = auto_backup_dir(&read_state(&state_bg));
                match autobackup::read(&dir, &name).and_then(|v| restore_bundle(&state_bg, &v)) {
                    Ok(m) => log(&state_bg, "OK", m),
                    Err(e) => log(&state_bg, "ERR", e),
                }
            });
        }
        "/api/backup_save" => {
            let state_bg = Arc::clone(state);
            let lang = read_state(state).config.language.clone();
            thread::spawn(move || {
                let Some(path) = rfd::FileDialog::new().set_title(i18n::tr(&lang, "Save a backup of ModForge")).set_file_name("gantry-backup.json").save_file() else { return };
                match serde_json::to_string_pretty(&backup_bundle(&state_bg)).map_err(|e| e.to_string()).and_then(|j| fsutil::atomic_write(&path, j).map_err(|e| e.to_string())) {
                    Ok(()) => log(&state_bg, "OK", format!("Backup saved to {}.", path.display())),
                    Err(e) => log(&state_bg, "ERR", e),
                }
            });
        }
        "/api/backup_restore" => {
            let state_bg = Arc::clone(state);
            let lang = read_state(state).config.language.clone();
            thread::spawn(move || {
                let Some(path) = rfd::FileDialog::new().set_title(i18n::tr(&lang, "Open a backup of Gantry")).add_filter(i18n::tr(&lang, "Gantry backup"), &["json"]).pick_file() else { return };
                let parsed = fs::metadata(&path)
                    .map_err(|e| e.to_string())
                    .and_then(|m| if m.len() > 20_000_000 { Err("That file is too large to be a backup.".to_string()) } else { Ok(()) })
                    .and_then(|_| fs::read(&path).map_err(|e| e.to_string()))
                    .and_then(|b| serde_json::from_slice::<Value>(&b).map_err(|_| "This file is not a ModForge backup.".to_string()));
                match parsed.and_then(|v| restore_bundle(&state_bg, &v)) {
                    Ok(m) => log(&state_bg, "OK", m),
                    Err(e) => log(&state_bg, "ERR", e),
                }
            });
        }
        "/api/lib_export" => {
            let state_bg = Arc::clone(state);
            let lang = read_state(state).config.language.clone();
            thread::spawn(move || {
                let Some(g) = selected_game(&state_bg).filter(|g| g.is_installed) else { return };
                let Some(lib) = library_of(&g) else { return };
                let spec = lib.export_spec(&g.name);
                let Some(path) = rfd::FileDialog::new()
                    .set_title(i18n::tr(&lang, "Save this setup"))
                    .set_file_name(format!("{} setup.modforge.json", g.name))
                    .save_file()
                else {
                    return;
                };
                match serde_json::to_string_pretty(&spec).map_err(|e| e.to_string()).and_then(|j| fsutil::atomic_write(&path, j).map_err(|e| e.to_string())) {
                    Ok(()) => log(&state_bg, "OK", format!("Saved your setup of {} mod(s) to {}.", spec.mods.len(), path.display())),
                    Err(e) => log(&state_bg, "ERR", e),
                }
            });
        }
        "/api/lib_import" => {
            let state_bg = Arc::clone(state);
            let lang = read_state(state).config.language.clone();
            thread::spawn(move || {
                let Some(g) = selected_game(&state_bg).filter(|g| g.is_installed) else { return };
                let Some(path) = rfd::FileDialog::new().set_title(i18n::tr(&lang, "Open a modpack")).add_filter(i18n::tr(&lang, "ModForge modpack"), &["gantrypack", "zip", "json"]).pick_file() else { return };
                if let Err(e) = read_pack_file(&state_bg, &path).and_then(|(spec, files)| preview_import(&state_bg, &g, spec, files)) {
                    log(&state_bg, "ERR", e);
                }
            });
        }
        "/api/pack_export" => {
            let state_bg = Arc::clone(state);
            let lang = read_state(state).config.language.clone();
            let name = body_str(body, "name");
            let full = body.get("full").and_then(Value::as_bool).unwrap_or(false);
            let target_path = body_str(body, "path");
            thread::spawn(move || {
                let Some(g) = selected_game(&state_bg).filter(|g| g.is_installed) else { return };
                let label = if name.is_empty() { format!("{} modpack", g.name) } else { name.clone() };
                let path = if !target_path.is_empty() {
                    PathBuf::from(target_path)
                } else {
                    let ext = if full { "zip" } else { "gantrypack" };
                    let filter_desc = if full { "Modpack Archive (.zip)" } else { "ModForge modpack" };
                    let Some(p) = rfd::FileDialog::new().set_title(i18n::tr(&lang, "Save the modpack")).add_filter(filter_desc, &[ext]).set_file_name(format!("{label}.{ext}")).save_file() else { return };
                    p
                };
                match export_pack_file(&state_bg, &g, &label, &path, full) {
                    Ok(m) => log(&state_bg, "OK", m),
                    Err(e) => log(&state_bg, "ERR", e),
                }
            });
        }
        "/api/profile_code" => {
            let code = body_str(body, "code");
            let state_bg = Arc::clone(state);
            thread::spawn(move || {
                let Some(g) = selected_game(&state_bg).filter(|g| g.is_installed) else { return };
                let spec = if profilecode::is_share_code(&code) { profilecode::from_share_code(&code, &g.name) } else { profile_from_code(&state_bg, &code, &g.name) };
                if let Err(e) = spec.and_then(|spec| preview_import(&state_bg, &g, spec, None)) {
                    log(&state_bg, "ERR", e);
                }
            });
        }
        "/api/import_confirm" => {
            let accept = body.get("accept").and_then(Value::as_bool).unwrap_or(false);
            let state_bg = Arc::clone(state);
            thread::spawn(move || finish_import(&state_bg, accept));
        }
        "/api/notes_read" => {
            let id = body.get("id").and_then(Value::as_u64);
            let mut st = write_state(state);
            st.notes.mark_read(id);
            st.notifier.bump();
        }
        "/api/notes_clear" => {
            let mut st = write_state(state);
            st.notes.clear();
            st.notifier.bump();
        }
        "/api/pack_dismiss" => {
            if let Some(lib) = selected_game(state).and_then(|g| library_of(&g)) {
                let mut st = write_state(state);
                st.config.pending_packs.remove(&lib.root().to_string_lossy().to_string());
                st.save_config();
                st.notifier.bump();
            }
        }
        "/api/lib_pin" => {
            let (path, id) = (body_str(body, "path"), body_str(body, "id"));
            library_task(state, move |st, lib, _| {
                running_guard(st)?;
                lib.pin_file(&path, &id)?;
                library_apply(st, lib, false)
            });
        }
        "/api/lib_prefer" => {
            let (winner, over) = (body_str(body, "winner"), body_str(body, "over"));
            library_task(state, move |st, lib, _| {
                running_guard(st)?;
                let n = lib.prefer(&winner, &over)?;
                if n == 0 {
                    return Ok("Nothing to change: that mod already wins those files.".into());
                }
                library_apply(st, lib, false)
            });
        }
        "/api/lib_clear_pins" => {
            library_task(state, move |st, lib, _| {
                running_guard(st)?;
                if lib.clear_pins()? == 0 {
                    return Ok("No files are pinned.".into());
                }
                library_apply(st, lib, false)
            });
        }
        "/api/lib_auto_resolve" => {
            let strategy = body_str(body, "strategy");
            let strat = if strategy.is_empty() { "topological_patches_last".to_string() } else { strategy };
            library_task(state, move |st, lib, _| {
                running_guard(st)?;
                let report = lib.auto_resolve_conflicts(&strat)?;
                log(st, "OK", format!("Resolved {} conflict(s) across mods.", report.resolved_conflicts));
                library_apply(st, lib, false)
            });
        }
        "/api/lib_history" => {
            let index = body.get("index").and_then(Value::as_u64).unwrap_or(u64::MAX) as usize;
            library_task(state, move |st, lib, _| {
                running_guard(st)?;
                let label = lib.restore_history(index)?;
                log(st, "OK", format!("Went back to before \"{}\".", label));
                library_apply(st, lib, false)
            });
        }
        "/api/purge_all" => {
            let state_bg = Arc::clone(state);
            thread::spawn(move || purge_all(&state_bg));
        }
        "/api/self_update_check" => {
            let state_bg = Arc::clone(state);
            thread::spawn(move || match newer_gantry(&state_bg) {
                Some((tag, _)) => {
                    write_state(&state_bg).new_version = Some(tag.clone());
                    write_state(&state_bg).notifier.bump();
                    log(&state_bg, "OK", format!("A newer ModForge is available: {tag}."));
                }
                None => {
                    write_state(&state_bg).new_version = None;
                    write_state(&state_bg).notifier.bump();
                    log(&state_bg, "OK", "You have the newest ModForge.");
                }
            });
        }
        "/api/steam_shortcut" => {
            let close_steam = body.get("close_steam").and_then(Value::as_bool).unwrap_or(false);
            let appid = body.get("appid").and_then(Value::as_str).map(|s| s.to_string());
            let state_bg = Arc::clone(state);
            thread::spawn(move || match sync_to_steam(&state_bg, appid.as_deref(), close_steam) {
                Ok(m) => log(&state_bg, "OK", m),
                Err(e) => log(&state_bg, "WARN", e),
            });
        }
        "/api/steam_deck_sync" => {
            let close_steam = body.get("close_steam").and_then(Value::as_bool).unwrap_or(true);
            let appid = body.get("appid").and_then(Value::as_str).map(|s| s.to_string());
            let state_bg = Arc::clone(state);
            thread::spawn(move || match sync_to_steam(&state_bg, appid.as_deref(), close_steam) {
                Ok(m) => log(&state_bg, "OK", m),
                Err(e) => log(&state_bg, "WARN", e),
            });
        }
        "/api/open_crash_reports" => {
            let dir = crash_dir();
            if dir.is_dir() {
                xdg_open(dir);
            }
        }
        "/api/self_update" => {
            let state_bg = Arc::clone(state);
            thread::spawn(move || self_update(&state_bg));
        }
        "/api/settings" => {
            let mut st = write_state(state);
            if let Some(v) = body.get("tour_done").and_then(Value::as_bool) {
                st.config.tour_done = v;
            }
            if let Some(v) = body.get("keep_background").and_then(Value::as_bool) {
                st.config.keep_background = v;
            }
            if let Some(v) = body.get("install_preview").and_then(Value::as_bool) {
                st.config.no_install_preview = !v;
            }
            if let Some([w, h]) = body.get("window_size").and_then(Value::as_array).map(|a| [a.first().and_then(Value::as_u64), a.get(1).and_then(Value::as_u64)]) {
                // remembered so the next start opens the same size; nonsense is ignored
                if let (Some(w), Some(h)) = (w, h) {
                    if (640..=7680).contains(&w) && (480..=4320).contains(&h) {
                        st.config.window_w = w as u32;
                        st.config.window_h = h as u32;
                    }
                }
            }
            if let Some(v) = body.get("ui_scale").and_then(Value::as_u64) {
                st.config.ui_scale = (v as u32).clamp(80, 200);
            }
            if let Some(v) = body.get("motion").and_then(Value::as_str) {
                st.config.reduce_motion = v == "off";
                st.config.calm_motion = v == "calm";
            }
            if let Some(v) = body.get("reduce_motion").and_then(Value::as_bool) {
                st.config.reduce_motion = v;
            }
            if let Some(v) = body.get("high_contrast").and_then(Value::as_bool) {
                st.config.high_contrast = v;
            }
            if let Some(v) = body.get("theme").and_then(Value::as_str) {
                if matches!(v, "dark" | "light" | "auto" | "midnight" | "ember" | "forest" | "ocean" | "paper" | "mist" | "sand" | "custom") {
                    st.config.theme = v.to_string();
                }
            }
            if let Some(v) = body.get("accent").and_then(Value::as_str) {
                if matches!(v, "" | "green" | "blue" | "violet" | "pink" | "orange" | "red" | "cyan") {
                    st.config.accent = v.to_string();
                }
            }
            if let Some(c) = body.get("custom_theme").filter(|c| c.is_object()) {
                // only numbers in sensible ranges are kept
                let hue = c.get("hue").and_then(Value::as_f64).unwrap_or(250.0).clamp(0.0, 360.0).round();
                let radius = c.get("radius").and_then(Value::as_f64).unwrap_or(14.0).clamp(4.0, 24.0).round();
                let light = c.get("light").and_then(Value::as_bool).unwrap_or(false);
                st.config.custom_theme = json!({ "hue": hue, "radius": radius, "light": light }).to_string();
            }
            if let Some(v) = body.get("accent_from_game").and_then(Value::as_bool) {
                st.config.accent_from_game = v;
            }
            if let Some(l) = body.get("layout").filter(|l| l.is_object()) {
                // known words and yes/no only
                let word = |k: &str, ok: &[&str], default: &str| l.get(k).and_then(Value::as_str).filter(|v| ok.contains(v)).unwrap_or(default).to_string();
                let flag = |k: &str| l.get(k).and_then(Value::as_bool).unwrap_or(true);
                st.config.layout = json!({
                    "hero": word("hero", &["large", "compact", "bar"], "large"),
                    "tab": word("tab", &["mods", "tools", "activity"], "mods"),
                    "stats": flag("stats"), "ring": flag("ring"), "badges": flag("badges"),
                })
                .to_string();
            }
            if let Some(v) = body.get("seen_version").and_then(Value::as_str) {
                if v.len() <= 20 && v.bytes().all(|b| b.is_ascii_digit() || b == b'.') {
                    st.config.seen_version = v.to_string();
                }
            }
            if let Some(v) = body.get("show_adult").and_then(Value::as_bool) {
                st.config.show_adult = v;
            }
            if let Some(v) = body.get("sounds").and_then(Value::as_bool) {
                st.config.sounds = v;
            }
            if let Some(v) = body.get("haptics").and_then(Value::as_bool) {
                st.config.haptics = v;
            }
            if let Some(v) = body.get("download_limit_kb").and_then(Value::as_u64) {
                st.config.download_limit_kb = v.min(10_000_000);
            }
            if let Some(v) = body.get("downloads_wait").and_then(Value::as_bool) {
                st.config.downloads_wait = v;
            }
            if let Some(v) = body.get("checks_while_playing").and_then(Value::as_bool) {
                st.config.checks_while_playing = v;
            }
            background::set_while_playing(st.config.checks_while_playing);
            downloads::set_limits(downloads::Limits { kb_per_sec: st.config.download_limit_kb, wait_for_game: st.config.downloads_wait });
            if let Some(v) = body.get("virustotal").and_then(Value::as_bool) {
                st.config.virustotal = v;
            }
            if let Some(v) = body.get("auto_backup").and_then(Value::as_bool) {
                st.config.no_auto_backup = !v;
            }
            if let Some(v) = body.get("compact").and_then(Value::as_bool) {
                st.config.compact = v;
            }
            if let Some(v) = body.get("deck_mode").and_then(Value::as_bool) {
                st.config.deck_mode = v;
                // on a Steam Deck controller mode is on by itself; turning it off is remembered as a choice
                st.config.deck_declined = !v && platform::is_steam_deck();
            }
            st.save_config();
            st.notifier.bump();
        }
        "/api/game_meta" => {
            let mut st = write_state(state);
            let appid = {
                let a = body_str(body, "appid");
                if a.is_empty() { st.games.get(st.selected_idx).map(|g| g.appid.clone()).unwrap_or_default() } else { a }
            };
            let known = !appid.is_empty() && st.games.iter().any(|g| g.appid == appid);
            if known {
                game_meta_apply(&mut st, &appid, body);
            }
        }
        "/api/nexus_domain" => {
            let name = body_str(body, "name").trim().to_lowercase();
            if nexus::domain_ok(&name) {
                if let Some(g) = selected_game(state) {
                    let mut st = write_state(state);
                    st.config.nexus_domain.insert(g.appid, name.clone());
                    st.save_config();
                    st.notifier.bump();
                    st.log("OK", format!("Saved Nexus domain '{}'.", name));
                }
            } else {
                log(state, "WARN", "That does not look like a Nexus Mods game name.");
            }
        }
        "/api/collection_install" => {
            let (url, optional) = (body_str(body, "url"), body.get("optional").and_then(Value::as_bool).unwrap_or(false));
            let state_bg = Arc::clone(state);
            thread::spawn(move || collection_install(&state_bg, &url, optional));
        }
        "/api/pack_save" => {
            let entry = body.get("entry").cloned().unwrap_or(Value::Null);
            let path = read_state(state).paths.config_file.with_file_name("recipes.json");
            match serde_json::from_value::<recipes::Recipe>(entry).map_err(|_| "That is not a valid game entry.".to_string()).and_then(|r| recipes::save_user(&path, r)) {
                Ok(()) => {
                    let all = recipes::load(&path);
                    recipes::publish(&all);
                    write_state(state).recipes = all;
                    log(state, "OK", "Saved the support entry for this game.");
                    refresh_mods_shared(state);
                }
                Err(e) => log(state, "ERR", e),
            }
        }
        "/api/plugins_save" => {
            let order: Vec<(String, bool)> = body.get("order").and_then(Value::as_array).map(|a| a.iter().filter_map(|r| Some((r.get("name")?.as_str()?.to_string(), r.get("on").and_then(Value::as_bool).unwrap_or(false)))).collect()).unwrap_or_default();
            match plugins_save(state, &order) {
                Ok(m) => log(state, "OK", m),
                Err(e) => log(state, "ERR", e),
            }
        }
        "/api/game_shortcut" => {
            let (appid, desktop) = (body_str(body, "appid"), body.get("desktop").and_then(Value::as_bool).unwrap_or(false));
            if let Some(g) = game_by_appid(state, &appid) {
                let exe = std::env::current_exe().unwrap_or_default();
                let home = read_state(state).shortcut_home.clone().unwrap_or_else(home_dir);
                match install::game_shortcut(&home, &exe, &g.appid, &g.name, desktop) {
                    Ok(files) => log(state, "OK", format!("Shortcut created: {}", files.iter().map(|f| f.display().to_string()).collect::<Vec<_>>().join(", "))),
                    Err(e) => log(state, "ERR", e),
                }
            }
        }
        "/api/lib_update_rule" => {
            let (id, rule) = (body_str(body, "id"), body_str(body, "rule"));
            if let Some(lib) = selected_game(state).and_then(|g| library_of(&g)) {
                if let Err(e) = lib.set_update_rule(&id, &rule) {
                    log(state, "ERR", e);
                }
                let mut st = write_state(state);
                st.nexus.updates.retain(|u| !(rule == "never" && u.lib_id == id));
                st.thunder.updates.retain(|u| !(rule == "never" && u.lib_id == id));
                drop(st);
                refresh_mods_shared(state);
            }
        }
        "/api/lib_rollback" => {
            let (id, dir) = (body_str(body, "id"), body_str(body, "dir"));
            library_task(state, move |s, lib, g| {
                guard_saves(s, g, "before-rollback");
                let name = lib.rollback(&id, &dir)?;
                forget_updates_of(s, &id);
                let applied = library_apply(s, lib, false)?;
                refresh_mods_shared(s);
                Ok(format!("Went back to {}. {}", name, applied))
            });
        }
        "/api/fomod_finish" => {
            let sel: fomod::Selection = body.get("selection").and_then(|s| serde_json::from_value(s.clone()).ok()).unwrap_or_default();
            let pending = {
                let mut st = write_state(state);
                let p = (!st.pending_fomod.is_empty()).then(|| st.pending_fomod.remove(0));
                st.notifier.bump();
                p
            };
            if let Some(p) = pending {
                let state_bg = Arc::clone(state);
                thread::spawn(move || fomod_continue(&state_bg, p, sel));
            }
        }
        "/api/fomod_cancel" => {
            let pending = {
                let mut st = write_state(state);
                let p = (!st.pending_fomod.is_empty()).then(|| st.pending_fomod.remove(0));
                st.notifier.bump();
                p
            };
            if let Some(p) = pending {
                log(state, "INFO", format!("'{}' was not added.", p.fname));
                let _ = fs::remove_dir_all(&p.stage);
            }
        }
        "/api/nexus_get" => {
            let mod_id = body.get("mod_id").and_then(Value::as_u64).unwrap_or(0);
            let name = body_str(body, "name");
            let state_bg = Arc::clone(state);
            thread::spawn(move || nexus_get_mod(&state_bg, mod_id, &name));
        }
        "/api/nexus_install_needs" => {
            let (appid, lib_id) = (body_str(body, "appid"), body_str(body, "lib_id"));
            let state_bg = Arc::clone(state);
            thread::spawn(move || nexus_install_needs(&state_bg, &appid, &lib_id));
        }
        "/api/github_install" => {
            let (repo, url) = (body_str(body, "repo"), body_str(body, "url"));
            let state_bg = Arc::clone(state);
            thread::spawn(move || github_install(&state_bg, &repo, &url));
        }
        "/api/compat_proton" => {
            let tool = body_str(body, "tool");
            let state_bg = Arc::clone(state);
            thread::spawn(move || set_proton(&state_bg, &tool));
        }
        "/api/compat_preset" => {
            let (id, on) = (body_str(body, "id"), body.get("on").and_then(Value::as_bool).unwrap_or(false));
            if let Some((_, line, _)) = compat::PRESETS.iter().find(|p| p.0 == id) {
                let mut st = write_state(state);
                let idx = st.selected_idx;
                if let Some(g) = st.games.get_mut(idx) {
                    g.env_vars = compat::toggle_preset(&g.env_vars, line, on);
                    let (aid, args, envs) = (g.appid.clone(), g.launch_args.clone(), g.env_vars.clone());
                    st.config.launch_options.insert(aid, LaunchOpts { launch_args: args, env_vars: envs });
                    st.save_config();
                    st.save_custom_games();
                    st.log("OK", if on { format!("Turned on {line} for this game.") } else { format!("Turned off {line} for this game.") });
                }
            }
        }
        "/api/ge_download" => {
            let state_bg = Arc::clone(state);
            thread::spawn(move || download_ge(&state_bg));
        }
        "/api/loader_switch" => {
            let kind = body_str(body, "kind");
            let state_bg = Arc::clone(state);
            thread::spawn(move || switch_loader(&state_bg, &kind));
        }
        "/api/lib_relayout" => {
            library_task(state, move |st, lib, _| {
                running_guard(st)?;
                let n = lib.relayout(None)?;
                if n == 0 {
                    return Ok("Every mod already sits where the game expects it.".into());
                }
                log(st, "OK", format!("Moved the files of {} mod(s) next to the game's own files of the same name.", n));
                library_apply(st, lib, false)
            });
        }
        "/api/lib_note" => {
            let (id, text) = (body_str(body, "id"), body_str(body, "text"));
            library_task(state, move |_, lib, _| lib.set_note(&id, &text).map(|_| String::new()));
        }
        "/api/lib_restore" => {
            let dir = body_str(body, "dir");
            library_task(state, move |st, lib, _| {
                running_guard(st)?;
                let name = lib.restore_trash(&dir)?;
                log(st, "OK", format!("'{}' is back in the mod library.", name));
                library_apply(st, lib, false)
            });
        }
        "/api/lib_remove" => {
            let id = body_str(body, "id");
            library_task(state, move |st, lib, _| {
                running_guard(st)?;
                lib.remove(&id)?;
                log(st, "OK", "Mod removed.");
                library_apply(st, lib, false)
            });
        }
        "/api/lib_profile_save" => {
            let name = body_str(body, "name");
            library_task(state, move |st, lib, g| {
                let n = lib.save_profile(&name)?;
                let launch = {
                    let s = read_state(st);
                    let cur = s.games.iter().find(|x| x.appid == g.appid).unwrap_or(g);
                    deploy::ProfileLaunch {
                        launch_args: cur.launch_args.clone(),
                        env_vars: cur.env_vars.clone(),
                        tool: if g.launches_directly() { s.config.proton_choice.get(&g.appid).cloned().unwrap_or_default() } else { String::new() },
                        trainer: s.config.trainers.get(&g.appid).cloned().unwrap_or_default(),
                    }
                };
                lib.set_profile_launch(&n, launch)?;
                Ok(format!("Library profile '{}' saved.", n))
            });
        }
        "/api/lib_profile_load" => {
            let name = body_str(body, "name");
            library_task(state, move |st, lib, g| {
                guard_saves(st, g, "before-profile");
                let n = lib.load_profile(&name)?;
                if let Some(l) = lib.profile_launch(&name) {
                    apply_profile_launch(st, g, &l);
                }
                log(st, "OK", format!("Library profile '{}' loaded ({} mods changed).", name, n));
                switch_saves(st, g, lib, &name, false);
                library_apply(st, lib, false)
            });
        }
        "/api/lib_separate_saves" => {
            let on = body.get("enabled").and_then(Value::as_bool).unwrap_or(false);
            // the profile the saves in the game's folders belong to right now ("" = none yet)
            let current = body_str(body, "current");
            library_task(state, move |st, lib, _| {
                lib.set_separate_saves(on)?;
                if on && lib.saves_of().is_empty() && !current.trim().is_empty() {
                    lib.set_saves(current.trim(), None)?;
                }
                refresh_mods_shared(st);
                Ok(if on { "Each profile now keeps its own saves.".to_string() } else { "Profiles share the saves again.".to_string() })
            });
        }
        "/api/lib_profile_delete" => {
            let name = body_str(body, "name");
            library_task(state, move |_, lib, _| lib.delete_profile(&name).map(|_| format!("Library profile '{}' deleted.", name)));
        }
        "/api/lib_undo" => {
            library_task(state, |st, lib, _| {
                let label = lib.undo()?;
                log(st, "OK", format!("Undid: {}.", label));
                library_apply(st, lib, false)
            });
        }
        "/api/lib_first" => {
            let on = body.get("enabled").and_then(|v| v.as_bool()).unwrap_or(false);
            if let Some(g) = selected_game(state) {
                {
                    let mut st = write_state(state);
                    st.config.library_first.retain(|a| a != &g.appid);
                    if on {
                        st.config.library_first.push(g.appid.clone());
                    }
                    st.save_config();
                }
                refresh_mods_shared(state);
            }
        }
        "/api/lib_deploy" => {
            let force = body.get("force").and_then(|v| v.as_bool()).unwrap_or(false);
            library_task(state, move |st, lib, _| {
                running_guard(st)?;
                library_apply(st, lib, force)
            });
        }
        "/api/lib_purge" => {
            library_task(state, |st, lib, g| {
                running_guard(st)?;
                let r = lib.purge()?;
                Ok(format!("{} is vanilla again ({} file(s) removed, {} original(s) restored).", g.name, r.removed, r.restored))
            });
        }
        "/api/lib_open" => {
            if let Some(g) = selected_game(state) {
                if let Some(lib) = library_of(&g) {
                    let _ = fs::create_dir_all(lib.root());
                    xdg_open(lib.root());
                }
            }
        }
        "/api/play" => {
            let state_bg = Arc::clone(state);
            thread::spawn(move || play_game(&state_bg));
        }
        "/api/play_fixed" => {
            let fixes: Vec<String> = body.get("fixes").and_then(Value::as_array).map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_string)).take(8).collect()).unwrap_or_default();
            let state_bg = Arc::clone(state);
            thread::spawn(move || fix_and_play(&state_bg, &fixes));
        }
        "/api/play_vanilla" => {
            let state_bg = Arc::clone(state);
            thread::spawn(move || play_without_mods(&state_bg));
        }
        "/api/play_safe_mode" => {
            let state_bg = Arc::clone(state);
            thread::spawn(move || play_in_safe_mode(&state_bg));
        }
        "/api/play_profile" => {
            let name = body_str(body, "name");
            let state_bg = Arc::clone(state);
            thread::spawn(move || play_with_profile(&state_bg, &name));
        }
        "/api/trainer" => {
            let (game, tp) = {
                let st = read_state(state);
                let g = st.games.get(st.selected_idx).cloned();
                let tp = g.as_ref().and_then(|g| st.config.trainers.get(&g.appid).cloned());
                (g, tp)
            };
            if let Some(g) = game {
                match tp {
                    Some(tp) => {
                        if !procs::is_game_running(&g) {
                            log(state, "WARN", format!("{} is not running yet. A trainer only works once the game has started.", g.name));
                        }
                        let state_bg = Arc::clone(state);
                        thread::spawn(move || {
                            start_trainer(&state_bg, &g, Path::new(&tp));
                        });
                    }
                    None => log(state, "WARN", "No trainer attached to this game."),
                }
            }
        }
        "/api/wipe_mods" => {
            if let Some(g) = selected_game(state) {
                wipe_mods(&g);
                RigEngine::invalidate_ue_bin_memo();
                {
                    let mut st = write_state(state);
                    st.mods.clear();
                    st.nexus.needs.retain(|n| n.appid != g.appid);
                    st.nexus.updates.retain(|u| u.appid != g.appid);
                    st.thunder.updates.retain(|u| u.appid != g.appid);
                    st.github_updates.retain(|u| u.appid != g.appid);
                    st.modio_updates.retain(|u| u.appid != g.appid);
                    st.health.remove(&g.appid);
                    let idx = st.selected_idx;
                    if let Some(gm) = st.games.get_mut(idx) {
                        gm.mod_count = 0;
                        gm.refresh_flags();
                    }
                    st.notifier.bump();
                }
                refresh_mods_shared(state);
                log(state, "WARN", "Wiped UE4SS and all installed mods cleanly.");
            }
        }
        "/api/open_folder" => {
            if let Some(g) = selected_game(state) {
                let cache = read_state(state).cache_root();
                let places = save_places(state, &g);
                if let Some(p) = folder_target(&g, &body_str(body, "which"), &cache, &places) {
                    xdg_open(p);
                }
            }
        }
        "/api/open_mod_folder" => {
            let id = body_str(body, "id");
            if let Some(g) = selected_game(state) {
                if let Some(lib) = library_of(&g) {
                    let files_dir = lib.mod_files_dir(&id);
                    let target_dir = if files_dir.exists() {
                        files_dir
                    } else {
                        lib.root().join("mods").join(&id)
                    };
                    if target_dir.exists() {
                        xdg_open(&target_dir);
                    } else {
                        log(state, "WARN", "Mod staging folder does not exist.");
                    }
                }
            }
        }
        "/api/open_mods" => {
            if let Some(g) = selected_game(state) {
                let gdir = PathBuf::from(&g.game_dir);
                match RigEngine::find_ue_bin_dir(&gdir) {
                    Some(ub) => {
                        let mdir = ModManager::get_ue4ss_root(&ub).join("Mods");
                        let _ = fs::create_dir_all(&mdir);
                        xdg_open(mdir);
                    }
                    None => {
                        xdg_open(&g.game_dir);
                    }
                }
            }
        }
        "/api/nexus" => {
            if let Some(g) = selected_game(state) {
                let url = if g.name.to_lowercase().contains("half sword") {
                    "https://www.nexusmods.com/halfsword".to_string()
                } else {
                    format!("https://www.nexusmods.com/search/?gsearch={}", fsutil::url_encode(&g.name))
                };
                xdg_open(url);
            }
        }
        "/api/winecfg" => {
            let (game, pb) = {
                let st = read_state(state);
                let g = st.games.get(st.selected_idx).cloned();
                let pb = g.as_ref().and_then(|g| st.proton_for(g));
                (g, pb)
            };
            if let Some(g) = game {
                let wine = RigEngine::resolve_proton_wine_bin(pb.as_deref());
                let mut c = Command::new(wine);
                c.arg("winecfg").env("WINEPREFIX", &g.pfx);
                if !spawn_reaped(c) {
                    log(state, "ERR", "Could not start winecfg.");
                }
            }
        }
        "/api/launch_options" => {
            let args = body.get("launch_args").and_then(|v| v.as_str()).unwrap_or("").to_string();
            let envs = body.get("env_vars").and_then(|v| v.as_str()).unwrap_or("").to_string();
            let mut st = write_state(state);
            let idx = st.selected_idx;
            if let Some(g) = st.games.get_mut(idx) {
                g.launch_args = args.clone();
                g.env_vars = envs.clone();
                let aid = g.appid.clone();
                st.config.launch_options.insert(aid, LaunchOpts { launch_args: args, env_vars: envs });
                st.save_config();
                st.save_custom_games();
                st.log("OK", "Saved custom launch options.");
            }
        }
        "/api/crash_analyze" => {
            let state_bg = Arc::clone(state);
            thread::spawn(move || analyze_last_run(&state_bg));
        }
        "/api/crash_dismiss" => {
            let mut st = write_state(state);
            st.crash = None;
            st.notifier.bump();
        }
        "/api/bisect_start" => match bisect_start(state) {
            Ok(m) => log(state, "INFO", format!("Crash Detective started. {}", m)),
            Err(e) => log(state, "WARN", e),
        },
        "/api/bisect_answer" => {
            let crashed = body.get("crashed").and_then(|v| v.as_bool()).unwrap_or(false);
            match bisect_answer(state, crashed) {
                Ok(m) => log(state, "INFO", m),
                Err(e) => log(state, "WARN", e),
            }
        }
        "/api/bisect_finish" => {
            let restore_all = body.get("restore_all").and_then(|v| v.as_bool()).unwrap_or(true);
            match bisect_finish(state, restore_all) {
                Ok(m) => log(state, "OK", m),
                Err(e) => log(state, "WARN", e),
            }
        }
        "/api/full_setup" => {
            let close_steam = body.get("close_steam").and_then(|v| v.as_bool()).unwrap_or(false);
            // the welcome list sets up a game without selecting it
            let game = match body.get("appid").and_then(Value::as_str) {
                Some(id) => match game_by_appid(state, id).filter(|g| g.is_installed) {
                    Some(g) => Some(g),
                    None => return true,
                },
                None => None,
            };
            // the window waits for the new checklist, not the last one
            if !read_state(state).setup_busy.load(Ordering::SeqCst) {
                write_state(state).setup_run = None;
            }
            let state_bg = Arc::clone(state);
            thread::spawn(move || match game {
                Some(g) => full_setup_for(&state_bg, &g, close_steam),
                None => full_setup(&state_bg, close_steam),
            });
        }
        "/api/steam_launch_apply" => {
            let force = body.get("force").and_then(|v| v.as_bool()).unwrap_or(false);
            let close_steam = body.get("close_steam").and_then(|v| v.as_bool()).unwrap_or(false);
            let state_bg = Arc::clone(state);
            thread::spawn(move || {
                let Some(g) = selected_game(&state_bg) else { return };
                match apply_steam_options(&state_bg, &g, force, close_steam) {
                    Ok(msg) => log(&state_bg, "OK", msg),
                    Err(e) => log(&state_bg, "WARN", e),
                }
            });
        }
        "/api/rescan" => {
            let state_bg = Arc::clone(state);
            thread::spawn(move || rescan(&state_bg));
        }
        _ => return false,
    }
    true
}

/// Re-read when Steam says each game was last played.
fn load_steam_played(state: &SharedState) {
    let path = read_state(state).steam_env.localconfig();
    let played = path.and_then(|p| fs::read_to_string(p).ok()).map(|t| steamlib::last_played(&t)).unwrap_or_default();
    let mut st = write_state(state);
    if st.steam_played != played {
        st.steam_played = played;
        st.notifier.bump();
    }
}

/// Remember that a game was launched just now.
fn record_launch(state: &SharedState, appid: &str) {
    let mut st = write_state(state);
    st.config.last_launched.insert(appid.to_string(), now_ms());
    st.save_config();
    st.notifier.bump();
}

fn rescan(state: &SharedState) {
    let (paths, cfg, libraries, keep_appid, keep_proton) = {
        let st = read_state(state);
        (
            st.paths.clone(),
            st.config.clone(),
            st.steam_env.libraries.clone(),
            st.games.get(st.selected_idx).map(|g| g.appid.clone()),
            st.protons.get(st.selected_proton_idx).map(|(n, _)| n.clone()),
        )
    };
    let t0 = Instant::now();
    RigEngine::invalidate_ue_bin_memo();
    let games = match libraries {
        Some(libs) => RigEngine::scan_libraries(&libs, &paths, &cfg),
        None => RigEngine::scan_all_games(&paths, &cfg),
    };
    let protons = RigEngine::find_protons();
    let ms = t0.elapsed().as_secs_f64() * 1000.0;
    {
        let mut st = write_state(state);
        st.games = games;
        st.protons = protons;
        // Selection follows the game, not its (re-sorted) position.
        match keep_appid {
            Some(id) => {
                st.select_by_appid(&id);
            }
            None => st.selected_idx = 0,
        }
        st.selected_proton_idx = keep_proton
            .and_then(|n| st.protons.iter().position(|(pn, _)| *pn == n))
            .unwrap_or(0);
        st.mods.clear();
        st.log("INFO", format!("Parallel library rescan finished in {:.2}ms.", ms));
    }
    load_steam_played(state);
    refresh_mods_shared(state);
}

fn add_custom_game(state: &SharedState, exe: &Path) {
    let Some(entry) = RigEngine::resolve_executable(exe) else {
        log(state, "ERR", format!("Could not read '{}'.", exe.display()));
        return;
    };
    {
        let mut st = write_state(state);
        if let Some(existing) = st.games.iter().position(|g| g.is_custom && g.exe_path == entry.exe_path) {
            st.selected_idx = existing;
            let name = st.games[existing].name.clone();
            st.log("INFO", format!("'{}' is already in your library.", name));
        } else {
            st.log("OK", format!("Added custom game: {} ({})", entry.name, entry.engine));
            let appid = entry.appid.clone();
            st.games.push(entry);
            st.games.sort_by_cached_key(|g| g.name.to_lowercase());
            st.save_custom_games();
            st.select_by_appid(&appid);
        }
        st.mods.clear();
    }
    refresh_mods_shared(state);
}

fn wipe_mods(g: &GameEntry) {
    let gdir = PathBuf::from(&g.game_dir);
    if !gdir.is_dir() {
        return;
    }

    // 1. Remove every mod currently listed by ModManager::list_mods(g).
    // This directly removes discovered LUA folders, PAK files, and DLL mod files.
    for m in ModManager::list_mods(g) {
        if m.path.is_dir() {
            let _ = fs::remove_dir_all(&m.path);
        } else if m.path.is_file() {
            let _ = fs::remove_file(&m.path);
        }
        if let Some(parent) = m.path.parent() {
            let pname = parent.file_name().unwrap_or_default().to_string_lossy().to_lowercase();
            if pname == "~mods" || pname == "logicmods" {
                let _ = fs::remove_dir_all(parent);
            }
        }
    }

    // 2. Identify all candidate binary/root directories
    let mut bin_targets = vec![gdir.clone()];
    if let Some(ue_bin) = RigEngine::find_ue_bin_dir(&gdir) {
        if !bin_targets.contains(&ue_bin) {
            bin_targets.push(ue_bin);
        }
    }
    if !g.bin_dir.is_empty() {
        let bdir = PathBuf::from(&g.bin_dir);
        if bdir.is_dir() && !bin_targets.contains(&bdir) {
            bin_targets.push(bdir);
        }
    }
    for sub in ["bin", "bin/x64", "Binaries/Win64", "Binaries/WinGDK"] {
        let p = gdir.join(sub);
        if p.is_dir() && !bin_targets.contains(&p) {
            bin_targets.push(p);
        }
    }

    // 3. Remove proxy DLLs, loader configs, logs, and loader folders from all targets
    for b in &bin_targets {
        for f in [
            "dwmapi.dll", "dwmapi.dll.disabled",
            "dxgi.dll", "dxgi.dll.disabled",
            "dinput8.dll", "dinput8.dll.disabled",
            "version.dll", "version.dll.disabled",
            "winhttp.dll", "winhttp.dll.disabled",
            "winmm.dll", "winmm.dll.disabled",
            "bink2w64.dll", "bink2w64.dll.disabled",
            "xinput1_3.dll", "xinput1_3.dll.disabled",
            "xinput1_4.dll", "xinput1_4.dll.disabled",
            "UE4SS.dll", "UE4SS.log", "UE4SS-settings.ini",
            "VTableLayout.ini", "MemberVariableLayout.ini",
            "doorstop_config.ini", "doorstop_version",
            "ScriptHookV.dll", "ScriptHookV.log",
            "mods.txt",
        ] {
            let _ = fs::remove_file(b.join(f));
        }
        for d in [
            "ue4ss", "Mods", "VTableLayoutTemplates", "MemberVarLayoutTemplates",
            "BepInEx", "MelonLoader", "scripts", "Scripts", "plugins", "Plugins",
            "UserData", "UserLibs",
        ] {
            let _ = fs::remove_dir_all(b.join(d));
        }
    }

    // 4. Unreal & generic pak mods cleaning
    let paks = ModManager::find_or_create_paks_dir(&gdir);
    let _ = fs::remove_dir_all(paks.join("~mods"));
    let _ = fs::remove_dir_all(paks.join("LogicMods"));

    // Also walk gdir for any nested ~mods or logicmods directories
    for entry in WalkDir::new(&gdir).min_depth(1).max_depth(5).into_iter().flatten() {
        if entry.file_type().is_dir() {
            let name = entry.file_name().to_string_lossy().to_lowercase();
            if name == "~mods" || name == "logicmods" {
                let _ = fs::remove_dir_all(entry.path());
            }
        }
    }

    // 5. Game-specific mod ecosystems (e.g. Cyberpunk 2077)
    let _ = fs::remove_dir_all(gdir.join("archive/pc/mod"));
    let _ = fs::remove_dir_all(gdir.join("bin/x64/plugins"));
    let _ = fs::remove_dir_all(gdir.join("r6/scripts"));
    let _ = fs::remove_dir_all(gdir.join("r6/logs"));
    let _ = fs::remove_dir_all(gdir.join("r6/cache"));
    let _ = fs::remove_dir_all(gdir.join("r6/config"));
    let _ = fs::remove_dir_all(gdir.join("red4ext"));

    // 6. Purge and wipe the game's Mod Library (deploy::Library)
    if let Some(lib) = library_of(g) {
        let _ = lib.wipe_all();
    }
}

fn ue4ss_installed(ue_bin: &Path) -> bool {
    ModManager::get_ue4ss_root(ue_bin).join("UE4SS.dll").is_file() || ue_bin.join("UE4SS.dll").is_file()
}

/// Download (once, cached) and install the UE4SS runtime into the game's Win64 folder.
/// Returns the number of files installed.
fn install_ue4ss_runtime(state: &SharedState, g: &GameEntry, ue_bin: &Path) -> Result<usize, String> {
    let (cache_dir, source) = {
        let st = read_state(state);
        (st.paths.cache_dir.clone(), st.ue4ss_source.clone())
    };
    let gdir = PathBuf::from(&g.game_dir);
    let zip_p = ue4ss::ensure_zip(&cache_dir, &source, &|m| log(state, "UE5", m))?;
    let cache_root = cache_dir.parent().unwrap_or(&cache_dir).to_path_buf();
    let stage = make_stage_dir(&gdir, &cache_root).ok_or("Could not create a staging directory for UE4SS.")?;
    if !extract_archive(&zip_p, &stage.0) {
        let _ = fs::remove_file(&zip_p); // never keep a corrupt download
        return Err("Failed to extract the UE4SS archive (it was removed; retry to download it again).".into());
    }
    ue4ss::prune_stage(&stage.0);
    let installed = match install_ue4ss_payload(&stage.0, ue_bin) {
        Some(n) => n,
        None => fsutil::copy_dir_all(&stage.0, ue_bin).unwrap_or(0),
    };
    if installed == 0 {
        return Err("UE4SS archive contained no files.".into());
    }
    RigEngine::invalidate_ue_bin_memo();
    Ok(installed)
}

fn setup_ue4ss(state: &SharedState) {
    let Some(g) = selected_game(state) else { return };
    let Some(ue_bin) = RigEngine::find_ue_bin_dir(Path::new(&g.game_dir)) else {
        log(state, "ERR", "No Unreal Engine Win64 folder found for this game.");
        return;
    };
    if let Err(e) = install_ue4ss_runtime(state, &g, &ue_bin) {
        log(state, "ERR", e);
        return;
    }
    for a in RepairEngine::repair(&g) {
        log(state, "UE5", a);
    }
    refresh_mods_shared(state);
}

/// One line of the "Make this game moddable" checklist. `text` is an English phrase with at most one `{}`, filled with
/// `arg`; the window translates it.
#[derive(Clone, Debug, serde::Serialize)]
pub struct SetupStep {
    pub id: String,
    pub text: &'static str,
    pub arg: String,
    /// "todo", "doing", "done", "failed" or "skip".
    pub status: &'static str,
    pub detail: String,
}

#[derive(Clone, Debug, serde::Serialize)]
pub struct SetupRun {
    pub appid: String,
    pub steps: Vec<SetupStep>,
    pub finished: bool,
    /// Why nothing was done, when nothing was (an English phrase with at most one `{}`, filled with `note_arg`).
    pub note: &'static str,
    pub note_arg: String,
}

/// A run that ended before it began, with the reason.
fn setup_refused(state: &SharedState, g: &GameEntry, note: &'static str, arg: &str) {
    let mut st = write_state(state);
    st.setup_run = Some(SetupRun { appid: g.appid.clone(), steps: Vec::new(), finished: true, note, note_arg: arg.to_string() });
    st.notifier.bump();
}

fn step(id: &str, text: &'static str, arg: &str, done: bool) -> SetupStep {
    SetupStep { id: id.into(), text, arg: arg.into(), status: if done { "done" } else { "todo" }, detail: String::new() }
}

/// The loader that fits a Unity game without one (none when it has one or is not a Unity game).
fn loader_wanted(state: &SharedState, g: &GameEntry) -> Option<(&'static str, bool)> {
    let info = loaders::detect(Path::new(&g.game_dir))?;
    let rec = recipes::find(&read_state(state).recipes, &g.appid, g.recipe_steam_id()).map(|r| r.loader.clone()).unwrap_or_default();
    let kind = match welcome::verdict(Some(&info), &g.engine, g.ue4ss, &rec, None) {
        welcome::Verdict::NeedsLoader(k) => k,
        _ => return Some(("", true)),
    };
    Some((kind, false))
}

/// What "Make this game moddable" would do for `g`, each step already marked done when it is. The second value is why
/// nothing may be done at all (anti-cheat).
fn setup_steps(state: &SharedState, g: &GameEntry) -> (Vec<SetupStep>, Option<String>) {
    let dir = PathBuf::from(&g.game_dir);
    if let Some(ac) = deploy::anti_cheat(&dir) {
        return (Vec::new(), Some(ac.to_string()));
    }
    let mut steps = Vec::new();
    if let Some((kind, has)) = loader_wanted(state, g) {
        let label = loaders::Loader::parse(kind).map(|l| l.label()).unwrap_or("BepInEx");
        steps.push(step("loader", if has { "A mod loader is installed" } else { "Install the mod loader {}" }, if has { "" } else { label }, has));
    }
    if let Some(ue_bin) = RigEngine::find_ue_bin_dir(&dir) {
        let ok = ue4ss_installed(&ue_bin) && RepairEngine::diagnose(g).issues.is_empty();
        steps.push(step("ue4ss", "Install and configure UE4SS", "", ok));
    }
    for c in frameworks::cores(&dir) {
        steps.push(step(&format!("core:{}", c.name), if c.installed { "{} is installed" } else { "Install {} (from its official releases)" }, c.name, c.installed));
    }
    let installs = steps.iter().any(|s| s.status == "todo");
    steps.push(step("backup", "Back up your saves", "", false));
    if !g.launches_directly() {
        let info = steam_info(state, g);
        let linux = !platform::IS_WINDOWS && !platform::IS_MAC;
        let mut s = match info.status.as_str() {
            "ok" if !(installs && linux) => step("steam", "Steam launch options are set", "", true),
            "ok" | "missing" | "stale" => step("steam", "Set the Steam launch options", "", false),
            "custom" => SetupStep { status: "skip", ..step("steam", "Steam has launch options you set yourself; they stay", "", false) },
            _ => SetupStep { status: "skip", detail: info.detail.clone(), ..step("steam", "Steam launch options cannot be read", "", false) },
        };
        if s.status == "todo" && info.steam_running {
            s.detail = "steam_running".into();
        }
        steps.push(s);
    }
    (steps, None)
}

/// The checklist for the window: what will be done (a dry run) and, while or after it runs, how far it got.
fn setup_plan_json(state: &SharedState, g: &GameEntry) -> Value {
    let (steps, blocked) = setup_steps(state, g);
    let run = read_state(state).setup_run.clone().filter(|r| r.appid == g.appid);
    let busy = read_state(state).setup_busy.load(Ordering::SeqCst);
    let changes = steps.iter().filter(|s| s.status == "todo" && s.id != "backup").count();
    json!({ "appid": g.appid, "name": g.name, "blocked": blocked, "steps": steps, "changes": changes, "run": run, "busy": busy })
}

fn mark_step(state: &SharedState, id: &str, status: &'static str, detail: &str) {
    let mut st = write_state(state);
    if let Some(s) = st.setup_run.as_mut().and_then(|r| r.steps.iter_mut().find(|s| s.id == id)) {
        s.status = status;
        s.detail = detail.to_string();
    }
    st.notifier.bump();
}

/// Put one core mod in from the newest release of its repository, as a library mod that knows where it came from (so
/// it is checked for updates like any GitHub mod). Gives the version that went in.
fn install_core(state: &SharedState, g: &GameEntry, core: &frameworks::Core) -> Result<String, String> {
    // already in the library but switched off: switching it back on is all it takes
    if let Some(lib) = library_of(g) {
        let key = core.name.to_lowercase();
        if let Some(m) = lib.mods().into_iter().find(|m| !m.enabled && m.name.to_lowercase().contains(&key)) {
            lib.set_enabled(&m.id, true)?;
            library_apply(state, &lib, false)?;
            return Ok(m.version);
        }
    }
    let (api, allow) = {
        let st = read_state(state);
        (st.github_api.clone(), st.github_allow.clone())
    };
    let (owner, repo) = core.repo.split_once('/').ok_or("No release source is known.")?;
    let rel = github::latest(owner, repo, &api, allow.as_deref())?;
    let names: Vec<String> = rel.assets.iter().map(|a| a.name.clone()).collect();
    let file = core.pick(&names).ok_or_else(|| format!("The newest release of {} has no file ModForge recognises.", core.name))?;
    let url = rel.assets.iter().find(|a| &a.name == file).map(|a| a.url.clone()).unwrap_or_default();
    let reference = github::GithubRef::from_download(&url);
    github_install_named(state, g, core.repo, &url, reference, Some(core.name));
    let there = frameworks::cores(Path::new(&g.game_dir)).iter().any(|c| c.name == core.name && c.installed);
    if !there {
        return Err(format!("{} did not end up in the game folder (see the log).", core.name));
    }
    Ok(rel.tag.trim_start_matches('v').to_string())
}

/// One click: everything a game needs before mods work. Unity games get their loader, Unreal games UE4SS (installed,
/// then configured for the engine version), Cyberpunk, Skyrim and Fallout 4 their core mods from the official releases.
/// Then the saves are backed up, Steam's launch options are made to match, and it is all checked again. Each step is
/// ticked off in `setup_run` for the window.
fn full_setup(state: &SharedState, close_steam: bool) {
    let Some(g) = selected_game(state) else { return };
    full_setup_for(state, &g, close_steam)
}

fn full_setup_for(state: &SharedState, g: &GameEntry, close_steam: bool) {
    let busy_flag = Arc::clone(&read_state(state).setup_busy);
    if busy_flag.swap(true, Ordering::SeqCst) {
        log(state, "WARN", "Full setup is already running.");
        return;
    }
    struct Busy(Arc<AtomicBool>);
    impl Drop for Busy {
        fn drop(&mut self) {
            self.0.store(false, Ordering::SeqCst);
        }
    }
    let _busy = Busy(busy_flag);

    log(state, "INFO", format!("Full setup for {}...", g.name));
    if procs::is_game_running(g) {
        log(state, "WARN", format!("{} is running. Close it and run Full setup again.", g.name));
        return setup_refused(state, g, "{} is running. Close it and run Full setup again.", &g.name);
    }
    let (steps, blocked) = setup_steps(state, g);
    if let Some(ac) = blocked {
        log(state, "WARN", format!("This game uses {ac}. Mod loaders can get your account banned, so ModForge will not install one."));
        return setup_refused(state, g, "This game uses {}. Modified files can get your account banned, so play without mods.", &ac);
    }
    let installs = steps.iter().any(|s| ["loader", "ue4ss"].contains(&s.id.as_str()) || s.id.starts_with("core:"));
    if !installs && g.launches_directly() {
        // Nothing here applies: no loader, no UE4SS (Unreal only), no core mods and no Steam launch options to sync.
        log(
            state,
            "INFO",
            format!(
                "Nothing to set up for {}: it is not an Unreal Engine game, so UE4SS does not apply. DLL, PAK and archive mods can still be dragged onto ModForge.",
                g.name
            ),
        );
        return setup_refused(state, g, "Nothing to set up for this game. Mods can be dragged onto ModForge as they are.", "");
    }
    {
        let mut st = write_state(state);
        st.setup_run = Some(SetupRun { appid: g.appid.clone(), steps: steps.clone(), finished: false, note: "", note_arg: String::new() });
        st.notifier.bump();
    }
    let finish = |state: &SharedState| {
        let mut st = write_state(state);
        if let Some(r) = st.setup_run.as_mut() {
            r.finished = true;
            for s in r.steps.iter_mut().filter(|s| s.status == "todo" || s.status == "doing") {
                s.status = "skip";
            }
        }
        st.notifier.bump();
    };
    // the numbered lines of the log: Unreal games count UE4SS twice (installed, then configured), plus the check at the end
    let unreal = RigEngine::find_ue_bin_dir(Path::new(&g.game_dir));
    let total = steps.len() + usize::from(unreal.is_some()) + 1 + usize::from(g.launches_directly() && unreal.is_some());
    let mut n = 0;
    let mut next = || {
        n += 1;
        format!("{n}/{total}")
    };
    let mut problems: Vec<String> = Vec::new();
    if !installs {
        log(state, "INFO", "Not an Unreal Engine game, so UE4SS steps are skipped.");
    }

    for s in &steps {
        match s.id.as_str() {
            "loader" => {
                let k = next();
                if s.status == "done" {
                    log(state, "OK", format!("{} The mod loader is already installed.", k));
                    continue;
                }
                mark_step(state, "loader", "doing", "");
                let kind = loader_wanted(state, g).map(|w| w.0).unwrap_or("");
                let mut ok = false;
                loader_exclusive(state, g, |st| ok = install_loader_inner(st, g, kind, false));
                if ok {
                    mark_step(state, "loader", "done", "");
                    log(state, "OK", format!("{} Installed the mod loader.", k));
                } else {
                    mark_step(state, "loader", "failed", "");
                    log(state, "ERR", "Full setup stopped: the mod loader could not be installed (see above).");
                    return finish(state);
                }
            }
            "ue4ss" => {
                let Some(ue_bin) = unreal.as_ref() else { continue };
                mark_step(state, "ue4ss", "doing", "");
                // 1. runtime
                let k = next();
                if ue4ss_installed(ue_bin) {
                    log(state, "OK", format!("{} UE4SS is already installed.", k));
                } else {
                    match install_ue4ss_runtime(state, g, ue_bin) {
                        Ok(files) => log(state, "OK", format!("{} Installed UE4SS ({} files).", k, files)),
                        Err(e) => {
                            mark_step(state, "ue4ss", "failed", &e);
                            log(state, "ERR", format!("Full setup stopped: {}", e));
                            return finish(state);
                        }
                    }
                }
                // 2. repair / configure
                for a in RepairEngine::repair(g) {
                    log(state, "UE5", a);
                }
                log(state, "OK", format!("{} Configured UE4SS for the detected engine version.", next()));
                mark_step(state, "ue4ss", "done", "");
            }
            id if id.starts_with("core:") => {
                let k = next();
                let name = &id[5..];
                if s.status == "done" {
                    log(state, "OK", format!("{} {} is already installed.", k, name));
                    continue;
                }
                let Some(core) = frameworks::cores(Path::new(&g.game_dir)).into_iter().find(|c| c.name == name) else { continue };
                mark_step(state, id, "doing", "");
                match install_core(state, g, &core) {
                    Ok(v) => {
                        mark_step(state, id, "done", &v);
                        log(state, "OK", format!("{} Installed {} {}.", k, name, v));
                    }
                    Err(e) => {
                        // one core mod that cannot be had is no reason to stop: the others still help
                        mark_step(state, id, "failed", &e);
                        problems.push(format!("{}: {}", name, e));
                        log(state, "WARN", format!("{} {}", k, e));
                    }
                }
            }
            "backup" => {
                let k = next();
                mark_step(state, "backup", "doing", "");
                let cache_root = read_state(state).cache_root();
                match saves::snapshot(g, &cache_root, "setup", &save_places(state, g)) {
                    Ok(Some(snap)) => {
                        mark_step(state, "backup", "done", &snap.files.to_string());
                        log(state, "OK", format!("{} Backed up {} save file(s).", k, snap.files));
                    }
                    // silent when the game has no saves yet
                    _ => mark_step(state, "backup", "skip", ""),
                }
            }
            "steam" => {
                let k = next();
                mark_step(state, "steam", "doing", "");
                // asked again now: what went in above can change what the game needs
                let info = steam_info(state, g);
                match info.status.as_str() {
                    "ok" => {
                        mark_step(state, "steam", "done", "");
                        log(state, "OK", format!("{} Steam launch options are already in sync.", k));
                    }
                    "custom" => {
                        mark_step(state, "steam", "skip", "");
                        log(state, "INFO", format!("{} Steam has launch options you set yourself; left unchanged (Doctor can replace them).", k));
                    }
                    "missing" | "stale" => match apply_steam_options(state, g, false, close_steam) {
                        Ok(m) => {
                            mark_step(state, "steam", "done", "");
                            log(state, "OK", format!("{} {}", k, m));
                        }
                        Err(e) => {
                            mark_step(state, "steam", "failed", &e);
                            problems.push(format!("Steam launch options: {}", e));
                            log(state, "WARN", format!("{} {}", k, e));
                        }
                    },
                    _ => {
                        mark_step(state, "steam", "skip", &info.detail);
                        log(state, "INFO", format!("{} Steam launch options skipped: {}", k, info.detail));
                    }
                }
            }
            _ => {}
        }
    }
    if g.launches_directly() && unreal.is_some() {
        log(state, "OK", format!("{} This game is launched by rig directly; nothing to change in Steam.", next()));
    }

    // verify
    RigEngine::invalidate_ue_bin_memo();
    if unreal.is_some() {
        problems.extend(RepairEngine::diagnose(g).issues);
    }
    refresh_mods_shared(state);
    let k = next();
    if problems.is_empty() {
        log(state, "OK", format!("{} Setup complete: every check passed. Press Play.", k));
    } else {
        log(state, "WARN", format!("{} Setup finished with {} open item(s): {}", k, problems.len(), problems.join(" | ")));
    }
    finish(state);
}

// ───────────────────────────── Steam launch options ─────────────────────────────

fn steam_wanted(state: &SharedState, g: &GameEntry) -> String {
    let (gamemode, mangohud) = {
        let st = read_state(state);
        (st.gamemode, st.mangohud)
    };
    steam_wanted_with(g, gamemode, mangohud)
}

fn steam_wanted_with(g: &GameEntry, gamemode: bool, mangohud: bool) -> String {
    // Wine's DLL overrides, GameMode and MangoHud only exist on Linux
    let linux = !platform::IS_WINDOWS && !platform::IS_MAC;
    let (gamemode, mangohud) = (gamemode && linux, mangohud && linux);
    let base = (linux && g.engine.starts_with("Unreal")).then(|| ModManager::get_active_dll_overrides(g));
    let dll_overrides = if linux { merge_overrides(base, &library_overrides(g)) } else { None };
    steam::wanted_launch_options(&steam::LaunchInputs {
        game: g,
        gamemode,
        mangohud,
        dll_overrides,
        have_gamemoderun: fsutil::find_in_path("gamemoderun").is_some(),
    })
}

fn steam_info(state: &SharedState, g: &GameEntry) -> steam::SteamLaunchInfo {
    let wanted = steam_wanted(state, g);
    let (env, last) = {
        let st = read_state(state);
        (st.steam_env.clone(), st.config.steam_applied.get(&g.appid).cloned())
    };
    steam::inspect_file(env.localconfig().as_deref(), g, &wanted, last.as_deref(), env.running())
}

/// Close Steam (`steam -shutdown`) and wait until the process is really gone.
fn shutdown_steam(env: &SteamEnv) -> Result<(), String> {
    if env.running.is_some() {
        return Err("Steam is running (close it first).".into());
    }
    let steam_cmd = if platform::IS_WINDOWS {
        RigPaths::steam_roots().into_iter().map(|r| r.join("steam.exe")).find(|p| p.is_file())
    } else {
        fsutil::find_in_path("steam")
    };
    let Some(steam_cmd) = steam_cmd else {
        return Err("Steam is running and the `steam` command was not found; close Steam manually.".into());
    };
    let _ = run_quiet(Command::new(steam_cmd).arg("-shutdown"));
    for _ in 0..160 {
        if !procs::steam_running() {
            return Ok(());
        }
        thread::sleep(Duration::from_millis(250));
    }
    Err("Steam did not close within 40 seconds; nothing was changed.".into())
}

/// Put ModForge or a game into Steam's library as a non-Steam game (for Big Picture and the Steam Deck's game mode),
/// syncing shortcuts and artwork for every Steam account on this computer.
fn sync_to_steam(state: &SharedState, appid: Option<&str>, close_steam: bool) -> Result<String, String> {
    let (env, games) = {
        let st = read_state(state);
        (st.steam_env.clone(), st.games.clone())
    };
    let configs: Vec<PathBuf> = env.localconfigs().iter().filter_map(|p| steamshortcut::path_beside(p)).collect();
    if configs.is_empty() {
        return Err("Steam's account folder was not found, so games could not be added to Steam.".into());
    }

    let is_modforge = appid.is_none() || appid == Some("") || appid == Some("modforge");
    if is_modforge {
        let exe = selfupdate::target().or_else(|| std::env::current_exe().ok()).ok_or("ModForge could not tell where it is installed.")?;
        let (exe_s, dir_s) = (exe.to_string_lossy().to_string(), exe.parent().map(|p| p.to_string_lossy().to_string()).unwrap_or_default());
        let mut todo = Vec::new();
        for path in &configs {
            let old = fs::read(path).unwrap_or_default();
            match steamshortcut::has_gantry(&old, &exe_s) {
                None => return Err("Steam's shortcuts file has a form ModForge does not know, so it was left as it is.".into()),
                Some(true) => {}
                Some(false) => todo.push((path.clone(), old)),
            }
        }
        if todo.is_empty() {
            return Ok("ModForge is already in Steam's library.".into());
        }
        let mut restart = false;
        if env.running() {
            if !close_steam {
                return Err("Steam is running. Close it first, then try again.".into());
            }
            if games.iter().any(procs::is_game_running) {
                return Err("A game is running. Close it first: Steam would end it too.".into());
            }
            log(state, "INFO", "Closing Steam to add ModForge to its library...");
            shutdown_steam(&env)?;
            restart = true;
        }
        for (path, old) in todo {
            let new = steamshortcut::add_gantry(&old, &exe_s, &dir_s, "--deck")?;
            fsutil::atomic_write_kept(&path, new).map_err(|e| format!("Could not write Steam's shortcuts file: {}", e))?;
            if let Some(parent) = path.parent() {
                let grid_dir = parent.join("grid");
                let art = crate::install::ICON_PNG;
                let _ = steamshortcut::sync_grid_artwork(&grid_dir, &exe_s, steamshortcut::NAME, Some(art), Some(art), Some(art), Some(art), Some(art));
            }
        }
        if restart && env.running.is_none() {
            let mut c = Command::new("steam");
            c.arg("-silent").stdout(Stdio::null()).stderr(Stdio::null());
            spawn_reaped(c);
        }
        return Ok("ModForge is in Steam's library now. It opens in controller mode from Big Picture and on a Steam Deck.".into());
    }

    // Syncing a specific game (custom standalone game or modded Steam game shortcut)
    let target_id = appid.unwrap();
    let g = games.iter().find(|x| x.appid == target_id).ok_or_else(|| format!("Game '{}' not found.", target_id))?;
    let g_dir = Path::new(&g.game_dir);
    let (name, exe_s, dir_s, args_s, icon_s, art) = if g.is_custom {
        let full_exe = g_dir.join(&g.exe_path);
        let exe_s = full_exe.to_string_lossy().to_string();
        let dir_s = g.game_dir.clone();
        let icon_p = g_dir.join("icon.png");
        let icon_s = if icon_p.exists() { icon_p.to_string_lossy().to_string() } else { "".to_string() };
        let art = fs::read(g_dir.join("icon.png")).or_else(|_| fs::read(g_dir.join("cover.png"))).ok();
        (g.name.clone(), exe_s, dir_s, g.launch_args.clone(), icon_s, art)
    } else {
        let mf_exe = selfupdate::target().or_else(|| std::env::current_exe().ok()).ok_or("ModForge could not tell where it is installed.")?;
        let exe_s = mf_exe.to_string_lossy().to_string();
        let dir_s = mf_exe.parent().map(|p| p.to_string_lossy().to_string()).unwrap_or_default();
        (format!("{} (ModForge)", g.name), exe_s, dir_s, format!("--start {}", g.appid), "".to_string(), Some(crate::install::ICON_PNG.to_vec()))
    };

    let mut todo = Vec::new();
    for path in &configs {
        let old = fs::read(path).unwrap_or_default();
        if steamshortcut::has_shortcut(&old, &name, &exe_s).is_none() && !old.is_empty() {
            return Err("Steam's shortcuts file has a form ModForge does not know, so it was left as it is.".into());
        }
        todo.push((path.clone(), old));
    }

    let mut restart = false;
    if env.running() {
        if !close_steam {
            return Err("Steam is running. Close it first, then try again.".into());
        }
        if games.iter().any(procs::is_game_running) {
            return Err("A game is running. Close it first: Steam would end it too.".into());
        }
        log(state, "INFO", "Closing Steam to add ModForge to its library...");
        shutdown_steam(&env)?;
        restart = true;
    }

    let tags = vec!["ModForge".to_string(), "Modded".to_string()];
    for (path, old) in todo {
        let (new, _sid) = steamshortcut::add_shortcut(&old, &name, &exe_s, &dir_s, &args_s, &icon_s, &tags)?;
        fsutil::atomic_write_kept(&path, new).map_err(|e| format!("Could not write Steam's shortcuts file: {}", e))?;
        if let Some(parent) = path.parent() {
            let grid_dir = parent.join("grid");
            if let Some(ref b) = art {
                let _ = steamshortcut::sync_grid_artwork(&grid_dir, &exe_s, &name, Some(b.as_slice()), Some(b.as_slice()), Some(b.as_slice()), Some(b.as_slice()), Some(b.as_slice()));
            }
        }
    }

    if restart && env.running.is_none() {
        let mut c = Command::new("steam");
        c.arg("-silent").stdout(Stdio::null()).stderr(Stdio::null());
        spawn_reaped(c);
    }

    Ok(format!("'{}' is in Steam's library now for Steam Deck Game Mode.", name))
}

#[allow(dead_code)]
fn add_gantry_to_steam(state: &SharedState, close_steam: bool) -> Result<String, String> {
    sync_to_steam(state, None, close_steam)
}

/// Make Steam's launch options for `g` match rig's settings.
fn apply_steam_options(state: &SharedState, g: &GameEntry, force: bool, close_steam: bool) -> Result<String, String> {
    let info = steam_info(state, g);
    match info.status.as_str() {
        "ok" => return Ok("Steam launch options are already up to date.".into()),
        "n/a" | "unavailable" => return Err(info.detail),
        "custom" if !force => {
            return Err(format!(
                "Steam already has launch options you set yourself ({}). Use Replace to overwrite them.",
                info.current
            ))
        }
        _ => {}
    }
    let (env, backup_dir) = {
        let st = read_state(state);
        (st.steam_env.clone(), st.cache_root().join("steam-backups"))
    };
    let localconfig = env.localconfig().ok_or("Steam's localconfig.vdf was not found.")?;
    let mut restart = false;
    if env.running() {
        if !close_steam {
            return Err("Steam is running. Close Steam first, or use \"Close Steam & apply\".".into());
        }
        log(state, "INFO", "Closing Steam to update its launch options...");
        shutdown_steam(&env)?;
        restart = true;
    }
    steam::apply_to_file(&localconfig, g, &info.wanted, false, &backup_dir)?;
    // The same person usually has several Steam accounts (family sharing): launch options belong
    // to one account, so the game gets them in each of the others too, unless that account has
    // options of its own that Gantry did not write.
    let last = read_state(state).config.steam_applied.get(&g.appid).cloned();
    for other in env.localconfigs().into_iter().filter(|p| *p != localconfig) {
        let theirs = steam::inspect_file(Some(&other), g, &info.wanted, last.as_deref(), false);
        if matches!(theirs.status.as_str(), "missing" | "stale") || (theirs.status == "custom" && force) {
            if let Err(e) = steam::apply_to_file(&other, g, &info.wanted, false, &backup_dir) {
                log(state, "WARN", e);
            }
        }
    }
    {
        let mut st = write_state(state);
        st.config.steam_applied.insert(g.appid.clone(), info.wanted.clone());
        st.save_config();
    }
    if restart && env.running.is_none() {
        let mut c = Command::new("steam");
        c.arg("-silent").stdout(Stdio::null()).stderr(Stdio::null());
        spawn_reaped(c);
    }
    Ok(format!("Steam launch options updated: {}", info.wanted))
}

/// Before handing a launch to Steam: bring its launch options up to date when that is safe.
/// Pressing Play only edits Steam's config when the game *needs* it (UE4SS's DLL override, or
/// env vars/arguments the user set); the optional gamemode/MangoHud wrappers alone are applied
/// only when the user asks (Doctor or Full Setup), never silently on a launch.
fn sync_steam_before_launch(state: &SharedState, g: &GameEntry) {
    if steam_wanted_with(g, false, false) == "%command%" {
        return;
    }
    let info = steam_info(state, g);
    match (info.status.as_str(), info.steam_running) {
        ("missing" | "stale", false) => match apply_steam_options(state, g, false, false) {
            Ok(m) => log(state, "OK", m),
            Err(e) => log(state, "WARN", e),
        },
        ("missing" | "stale", true) => log(
            state,
            "WARN",
            "Steam is open, so its launch options could not be updated and mods may not load. Doctor → \"Close Steam & apply\" fixes it.",
        ),
        ("custom", _) => log(
            state,
            "INFO",
            "Steam has launch options you set yourself; rig left them as they are (Doctor shows what rig would use).",
        ),
        _ => {}
    }
}

// ───────────────────────────── Hydra library watcher ─────────────────────────────

const LIBRARY_POLL: Duration = if cfg!(test) { Duration::from_millis(200) } else { Duration::from_secs(5) };

/// What "the set of installed games" looks like right now, cheap enough to poll.
#[derive(Clone, PartialEq, Eq, Default)]
struct LibrarySig {
    hydra: Vec<(String, String)>,
    /// Heroic and Lutris installs.
    launchers: Vec<(String, String)>,
    /// (library folder, appmanifest file) for every Steam game that is installed or downloading
    steam: Vec<(String, String)>,
}

fn steam_manifests(state: &SharedState) -> Vec<(String, String)> {
    let (libs, extra) = {
        let st = read_state(state);
        (st.steam_env.libraries.clone(), st.config.extra_libraries.clone())
    };
    let libs = libs.unwrap_or_else(|| RigEngine::parse_libraries(RigEngine::find_steam_root().as_deref(), &extra));
    let mut out: Vec<(String, String)> = libs
        .iter()
        .flat_map(|lib| {
            fs::read_dir(lib).into_iter().flatten().flatten().filter_map(move |e| {
                let n = e.file_name().to_string_lossy().to_string();
                (n.starts_with("appmanifest_") && n.ends_with(".acf")).then(|| (lib.display().to_string(), n))
            })
        })
        .collect();
    out.sort();
    out
}

fn library_signature(state: &SharedState) -> LibrarySig {
    let (hydra_dir, launchers) = {
        let st = read_state(state);
        (st.paths.hydra_dir.clone(), st.paths.launchers.clone())
    };
    LibrarySig {
        hydra: hydra_dir.map(|d| hydra::installed_signature(&d)).unwrap_or_default(),
        launchers: launchers.signature(),
        steam: steam_manifests(state),
    }
}

/// Rescan when a game is installed or removed in Steam or Hydra, so the library is always current
/// without pressing Rescan. Hydra's constant playtime updates and Steam's download progress do not
/// change the signature, so they cause no rescans.
fn spawn_library_watcher(state: Weak<RwLock<AppState>>) {
    thread::spawn(move || {
        let mut last: Option<LibrarySig> = None;
        loop {
            let Some(st) = state.upgrade() else { return };
            load_steam_played(&st);
            let sig = library_signature(&st);
            match &last {
                None => last = Some(sig),
                Some(prev) if *prev != sig => {
                    last = Some(sig);
                    log(&st, "INFO", "Your game library changed; rescanning...");
                    rescan(&st);
                }
                _ => {}
            }
            drop(st);
            thread::sleep(LIBRARY_POLL);
        }
    });
}

// ───────────────────────────── running-game monitor ─────────────────────────────

const MONITOR_INTERVAL: Duration = if cfg!(test) { Duration::from_millis(150) } else { Duration::from_secs(2) };

/// Keeps `is_running` truthful no matter who started the game (rig, Steam, a terminal), so
/// Stop works and repairs can avoid a live prefix. Debounced so a launch in progress is not
/// mistaken for an exit.
fn spawn_process_monitor(state: Weak<RwLock<AppState>>) {
    thread::spawn(move || {
        let mut misses = 0u8;
        let mut run: Option<Run> = None;
        let mut run_misses = 0u8;
        loop {
            thread::sleep(MONITOR_INTERVAL);
            let Some(st_arc) = state.upgrade() else { return };
            let (game, flag, pid_cell) = {
                let st = read_state(&st_arc);
                (st.games.get(st.selected_idx).cloned(), Arc::clone(&st.is_running), Arc::clone(&st.running_pid))
            };
            let detected = game.as_ref().map(procs::is_game_running).unwrap_or(false);
            let owned = pid_cell.load(Ordering::SeqCst) > 0;
            let now = detected || owned;
            let was = flag.load(Ordering::SeqCst);
            if now {
                misses = 0;
                if !was {
                    flag.store(true, Ordering::SeqCst);
                    let name = game.as_ref().map(|g| g.name.clone()).unwrap_or_default();
                    log(&st_arc, "INFO", format!("{} is running.", name));
                }
            } else if was {
                misses += 1;
                if misses >= 3 {
                    misses = 0;
                    flag.store(false, Ordering::SeqCst);
                    let name = game.as_ref().map(|g| g.name.clone()).unwrap_or_default();
                    log(&st_arc, "INFO", format!("{} is no longer running.", name));
                }
            } else {
                misses = 0;
            }

            // One "run" per game session, tied to the game it started with (the selection may move on).
            match run.as_mut() {
                None => {
                    if let (true, Some(g)) = (now, game) {
                        // what the logs already said before this session is old news
                        let seen = kb_findings(&st_arc, &g).into_iter().map(|f| (f.id, f.evidence)).collect();
                        run = Some(Run { game: g, started_ms: now_ms(), perf: perf::Tracker::default(), seen, next_scan_ms: now_ms() + LIVE_SCAN_MS });
                        run_misses = 0;
                    }
                }
                Some(r) => {
                    let pids = procs::game_pids(&r.game);
                    if let Some((rss_kb, ticks)) = procs::usage(&pids) {
                        r.perf.add(now_ms(), rss_kb, ticks);
                    }
                    let alive = !pids.is_empty() || owned;
                    if alive {
                        live_log_watch(&st_arc, r);
                    }
                    run_misses = if alive { 0 } else { run_misses + 1 };
                    if run_misses >= 3 {
                        if let Some(done) = run.take() {
                            finish_run(&st_arc, done);
                        }
                        run_misses = 0;
                    }
                }
            }
        }
    });
}

// ───────────────────────────── Crash Detective ─────────────────────────────

/// A mod changed this long before a crashed run counts as recently updated.
const RECENT_UPDATE_MS: u64 = 48 * 3600 * 1000;

struct Run {
    game: GameEntry,
    started_ms: u64,
    perf: perf::Tracker,
    /// Known problems already in the logs (as `id`, line), so only new ones are told during play.
    seen: Vec<(String, String)>,
    next_scan_ms: u64,
}

/// How often the logs of a running game are read for known problems.
const LIVE_SCAN_MS: u64 = if cfg!(test) { 0 } else { 15_000 };

/// Read the running game's logs and tell about a known problem that was not there before, while the game is still on.
fn live_log_watch(state: &SharedState, r: &mut Run) {
    let now = now_ms();
    if now < r.next_scan_ms {
        return;
    }
    r.next_scan_ms = now + LIVE_SCAN_MS;
    let lang = read_state(state).config.language.clone();
    for f in kb_findings(state, &r.game) {
        let key = (f.id.clone(), f.evidence.clone());
        if r.seen.contains(&key) {
            continue;
        }
        r.seen.push(key);
        if f.level == "info" {
            continue;
        }
        let title = f.title.get(&lang).or_else(|| f.title.get("en")).and_then(Value::as_str).or(f.title.as_str()).unwrap_or(&f.id).to_string();
        log(state, "WARN", format!("{}: {} ({})", r.game.name, title, f.evidence));
        add_note(state, "known", format!("While {} runs, its log shows a known problem: {}", r.game.name, title), &r.game.appid);
    }
}

/// Frame statistics from the MangoHud log written during a session, if there is one.
fn session_frames(from_ms: u64, to_ms: u64) -> Option<perf::FrameStats> {
    perf::parse_file(&perf::find_log(&mangohud_dirs(), from_ms, to_ms)?).ok()
}

fn mangohud_dirs() -> Vec<PathBuf> {
    let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default();
    let cfg = fs::read_to_string(home.join(".config/MangoHud/MangoHud.conf")).ok();
    perf::log_dirs(&home, cfg.as_deref())
}

/// The three most promising things to try after a crash, in order: what a known problem in the logs says to do, a mod
/// Crash Detective caught before that is on again, going back from a fresh update, checking for updates after a game
/// update, switching off a strong suspect; else finding the culprit in rounds.
fn crash_tries(state: &SharedState, g: &GameEntry, report: &crash::CrashReport) -> Vec<Value> {
    let mut out: Vec<Value> = Vec::new();
    let (issues, _) = game_issues(state, g, true);
    for i in issues.iter().filter(|i| i["code"] == "known") {
        out.push(json!({ "kind": "kb", "kb": i["kb"] }));
    }
    let lib_mods = library_of(g).map(|l| l.mods()).unwrap_or_default();
    // every mod that is on: (id, name, from the library)
    let mut on: Vec<(String, String, bool)> = lib_mods.iter().filter(|m| m.enabled).map(|m| (m.id.clone(), m.name.clone(), true)).collect();
    on.extend(ModManager::list_mods(g).into_iter().filter(|m| m.enabled && !m.is_system).map(|m| (m.id, m.name, false)));
    let culprits = read_state(state).config.culprits.get(&g.appid).cloned().unwrap_or_default();
    for c in &culprits {
        if let Some((id, name, lib)) = on.iter().find(|x| &x.1 == c) {
            out.push(json!({ "kind": "culprit_again", "id": id, "name": name, "library": lib }));
        }
    }
    if let Some(u) = report.recent_updates.first() {
        out.push(json!({ "kind": "rollback", "id": u.id, "dir": u.dir, "name": u.name, "old_version": u.old_version }));
    }
    if issues.iter().any(|i| i["code"] == "game_updated") {
        out.push(json!({ "kind": "updates" }));
    }
    if let Some(s) = report.suspects.first().filter(|s| s.score >= 6) {
        if !out.iter().any(|t| t["id"] == s.id.as_str()) {
            out.push(json!({ "kind": "off", "id": s.id, "name": s.name, "library": lib_mods.iter().any(|m| m.id == s.id) }));
        }
    }
    if out.len() < 3 && on.len() >= 2 {
        out.push(json!({ "kind": "bisect" }));
    }
    out.truncate(3);
    out
}

/// A run just ended: look for crash evidence and record the result.
fn finish_run(state: &SharedState, run: Run) {
    let ended = now_ms();
    {
        // time played, as far as Gantry saw the game run
        let secs = ended.saturating_sub(run.started_ms) / 1000;
        if secs >= 10 {
            let mut st = write_state(state);
            *st.config.playtime_s.entry(run.game.appid.clone()).or_default() += secs;
            st.save_config();
        }
    }
    snapshot_after_session(state, &run.game);
    let mods = ModManager::list_mods(&run.game);
    let mut report = crash::analyze_run(&run.game, &mods, run.started_ms.saturating_sub(5_000), ended);
    let crashed = report.crashed;
    if crashed {
        // a mod updated in the two days before this run is a suspect of its own: offer the way back
        if let Some(lib) = library_of(&run.game) {
            report.recent_updates = lib.recent_updates(run.started_ms.saturating_sub(RECENT_UPDATE_MS));
            if let Some(u) = report.recent_updates.first() {
                add_note(state, "rollback", format!("{} crashed after {} was updated to v{}. You can go back to v{} (open Crash Detective).", run.game.name, u.name, u.version, u.old_version), &run.game.appid);
            }
        }
    }
    let headline = report.headline.clone();
    {
        let secs = ended.saturating_sub(run.started_ms) / 1000;
        if secs >= 10 {
            let path = read_state(state).paths.config_file.with_file_name("sessions.jsonl");
            let lib = library_of(&run.game);
            let enabled: Vec<String> = lib.as_ref().map(|l| l.mods().into_iter().filter(|m| m.enabled).map(|m| m.name).collect()).unwrap_or_default();
            let on = enabled.len();
            let profile = lib.and_then(|l| l.matching_profile()).unwrap_or_default();
            let perf = run.perf.summary().map(|mut p| {
                p.frames = session_frames(run.started_ms, ended);
                p
            });
            stats::record(&path, &stats::Session { appid: run.game.appid.clone(), name: run.game.name.clone(), start_ms: run.started_ms, end_ms: ended, secs, crashed, mods: on, perf, profile, mod_names: enabled.into_iter().take(stats::MAX_NAMES).collect() });
        }
    }
    {
        let mut st = write_state(state);
        let n = st.config.crash_streak.entry(run.game.appid.clone()).or_default();
        *n = if crashed { *n + 1 } else { 0 };
        report.streak = *n;
        st.save_config();
    }
    if crashed {
        report.tries = crash_tries(state, &run.game, &report);
    }
    let session_open = {
        let mut st = write_state(state);
        st.last_run = Some(LastRun { appid: run.game.appid.clone(), ended_ms: ended, crashed });
        if crashed {
            st.crash = Some(report);
        }
        st.bisect.as_ref().map(|b| b.appid == run.game.appid).unwrap_or(false)
    };
    // a minute of play without a crash: the mods work with this build of the game
    if !crashed && ended.saturating_sub(run.started_ms) >= 60_000 {
        accept_game_build(state, &run.game);
    }
    if crashed {
        log(state, "WARN", format!("{} crashed. {} (open Crash Detective)", run.game.name, headline));
        add_note(state, "crash", format!("{} crashed. {} (open Crash Detective)", run.game.name, headline), &run.game.appid);
    } else if session_open {
        log(state, "INFO", format!("{} ended without crash markers. Tell Crash Detective what happened.", run.game.name));
    } else {
        write_state(state).notifier.bump();
    }
}

// ───────────────────────────── Steam library ─────────────────────────────

fn read_key_with_migration(target_file: &Path, file_name: &str) -> Option<String> {
    if let Ok(k) = fs::read_to_string(target_file) {
        let trimmed = k.trim().to_string();
        if !trimmed.is_empty() {
            return Some(trimmed);
        }
    }
    let hp = home_dir();
    let candidates = [
        hp.join(".config/modforge").join(file_name),
        hp.join(".config/gantry").join(file_name),
        hp.join(".config/protonrig").join(file_name),
    ];
    for cand in &candidates {
        if cand != target_file {
            if let Ok(k) = fs::read_to_string(cand) {
                let trimmed = k.trim().to_string();
                if !trimmed.is_empty() {
                    let _ = fsutil::atomic_write_private(target_file, &trimmed);
                    return Some(trimmed);
                }
            }
        }
    }
    None
}

fn clear_key_file_with_migration(target_file: &Path, file_name: &str) {
    let _ = fs::remove_file(target_file);
    let hp = home_dir();
    let candidates = [
        hp.join(".config/modforge").join(file_name),
        hp.join(".config/gantry").join(file_name),
        hp.join(".config/protonrig").join(file_name),
    ];
    for cand in &candidates {
        if cand != target_file {
            let _ = fs::remove_file(cand);
        }
    }
}

fn steam_key_file(st: &AppState) -> PathBuf {
    st.paths.config_file.with_file_name("steam_web_api_key")
}

fn steam_owned_file(st: &AppState) -> PathBuf {
    st.cache_root().join("steam_owned.json")
}

#[derive(serde::Serialize, serde::Deserialize)]
struct OwnedCache {
    steamid: String,
    fetched_ms: u64,
    games: Vec<steamlib::Owned>,
}

/// Load the saved key and the last owned-games list (no network access at startup).
fn load_steam_lib(state: &SharedState) {
    let mut st = write_state(state);
    let keyfile = steam_key_file(&st);
    if let Some(k) = read_key_with_migration(&keyfile, "steam_web_api_key") {
        if steamlib::valid_key(&k) {
            st.steam_lib.key = Some(k);
        }
    }
    if let Some(c) = fs::read(steam_owned_file(&st)).ok().and_then(|b| serde_json::from_slice::<OwnedCache>(&b).ok()) {
        st.steam_lib.owned = c.games;
        st.steam_lib.owned_for = c.steamid;
        st.steam_lib.fetched_ms = c.fetched_ms;
    }
}

/// The account whose library is shown (the chosen one, else the newest login).
fn active_steam_account(state: &SharedState) -> Option<steamlib::Account> {
    let st = read_state(state);
    let roots = st.steam_lib.roots.clone().unwrap_or_else(RigPaths::steam_roots);
    let accounts = steamlib::accounts(&roots);
    steamlib::pick(&accounts, &st.config.steam_account).cloned()
}

/// Fetch the owned games of the active account with `key`; stores nothing but the result.
fn fetch_owned(state: &SharedState, key: &str) -> Result<(steamlib::Account, Vec<steamlib::Owned>), String> {
    let account = active_steam_account(state).ok_or("No Steam account found on this computer.")?;
    let (base, allow_http) = {
        let st = read_state(state);
        (st.steam_lib.base.clone(), st.steam_lib.allow_http)
    };
    let games = steamlib::Client::new(&base, key, allow_http)?.owned_games(&account.steamid)?;
    Ok((account, games))
}

fn store_owned(state: &SharedState, account: &steamlib::Account, games: Vec<steamlib::Owned>) {
    let mut st = write_state(state);
    let cache = OwnedCache { steamid: account.steamid.clone(), fetched_ms: now_ms(), games: games.clone() };
    if let Ok(json) = serde_json::to_string(&cache) {
        let _ = fsutil::atomic_write(&steam_owned_file(&st), json);
    }
    st.steam_lib.owned = games;
    st.steam_lib.owned_for = account.steamid.clone();
    st.steam_lib.fetched_ms = cache.fetched_ms;
    st.steam_lib.error = None;
    st.notifier.bump();
}

/// Re-read the library with the saved key.
fn refresh_steam_owned(state: &SharedState) -> Result<usize, String> {
    let key = read_state(state).steam_lib.key.clone().ok_or("Add your Steam Web API key first (🔑 in the library overview).")?;
    {
        let mut st = write_state(state);
        if st.steam_lib.fetching {
            return Err("A Steam library refresh is already running.".into());
        }
        st.steam_lib.fetching = true;
        st.steam_lib.error = None;
        st.notifier.bump();
    }
    let result = fetch_owned(state, &key);
    write_state(state).steam_lib.fetching = false;
    match result {
        // the key was removed while Steam answered: forget the answer, say nothing
        Ok(_) if read_state(state).steam_lib.key.is_none() => Err(String::new()),
        Ok((account, games)) => {
            let n = games.len();
            store_owned(state, &account, games);
            Ok(n)
        }
        Err(e) => {
            let mut st = write_state(state);
            st.steam_lib.error = Some(e.clone());
            st.notifier.bump();
            Err(e)
        }
    }
}

/// Check a key by using it; keep it (in a file only the user can read) only when it works.
fn set_steam_key(state: &SharedState, key: &str) -> Result<String, String> {
    let key = key.trim();
    write_state(state).steam_lib.fetching = true;
    let result = fetch_owned(state, key);
    write_state(state).steam_lib.fetching = false;
    let (account, games) = result?;
    {
        let mut st = write_state(state);
        fsutil::atomic_write_private(&steam_key_file(&st), key).map_err(|e| format!("Could not save the key: {}", e))?;
        st.steam_lib.key = Some(key.to_string());
    }
    let n = games.len();
    store_owned(state, &account, games);
    Ok(format!("Steam library connected: {} games found for {}.", n, account.persona))
}

fn clear_steam_key(state: &SharedState) {
    let mut st = write_state(state);
    clear_key_file_with_migration(&steam_key_file(&st), "steam_web_api_key");
    let _ = fs::remove_file(steam_owned_file(&st));
    st.steam_lib.key = None;
    st.steam_lib.owned.clear();
    st.steam_lib.owned_for.clear();
    st.steam_lib.fetched_ms = 0;
    st.steam_lib.error = None;
    st.notifier.bump();
}

/// Keep the owned-games list fresh: a few seconds after start and then twice a day.
fn spawn_steam_refresher(state: Weak<RwLock<AppState>>) {
    spawn_background(state, "steam_library", "Refreshing your Steam library", Duration::from_secs(10), Duration::from_secs(3600), Box::new(|st, asked| {
        let (key, stale) = {
            let r = read_state(st);
            (r.steam_lib.key.is_some(), now_ms().saturating_sub(r.steam_lib.fetched_ms) > 12 * 3_600_000)
        };
        if !key {
            return Ok(false);
        }
        if stale || asked {
            refresh_steam_owned(st).map(|_| true)
        } else {
            Ok(true)
        }
    }));
}

// ───────────────────────────── Nexus Mods ─────────────────────────────

fn nexus_key_file(st: &AppState) -> PathBuf {
    st.paths.config_file.with_file_name("nexus_api_key")
}

/// Load a previously saved key (no network access at startup).
fn load_nexus_key(state: &SharedState) {
    let mut st = write_state(state);
    let keyfile = nexus_key_file(&st);
    if let Some(k) = read_key_with_migration(&keyfile, "nexus_api_key") {
        if nexus::valid_api_key(&k) {
            st.nexus.key = Some(k);
        }
    }
}

fn modio_key_file(st: &AppState) -> PathBuf {
    st.paths.config_file.with_file_name("modio_api_key")
}

/// Load a previously saved mod.io key (no network access at startup).
fn load_modio_key(state: &SharedState) {
    let mut st = write_state(state);
    let keyfile = modio_key_file(&st);
    if let Some(k) = read_key_with_migration(&keyfile, "modio_api_key") {
        if modio::valid_key(&k) {
            st.modio.key = Some(k);
        }
    }
}

fn set_modio_key(state: &SharedState, key: &str) -> Result<String, String> {
    let (base, allow_http) = {
        let st = read_state(state);
        (st.modio.base.clone(), st.modio.allow_http)
    };
    modio::Modio::new(&base, key.trim(), allow_http)?.check()?;
    let mut st = write_state(state);
    fsutil::atomic_write_private(&modio_key_file(&st), key.trim()).map_err(|e| format!("Could not save the key: {}", e))?;
    st.modio.key = Some(key.trim().to_string());
    st.notifier.bump();
    Ok("mod.io connected.".to_string())
}

fn clear_modio_key(state: &SharedState) {
    let mut st = write_state(state);
    clear_key_file_with_migration(&modio_key_file(&st), "modio_api_key");
    st.modio.key = None;
    st.notifier.bump();
}

fn modio_client(state: &SharedState) -> Result<modio::Modio, String> {
    let st = read_state(state);
    let key = st.modio.key.clone().ok_or("Add your mod.io API key first.")?;
    modio::Modio::new(&st.modio.base, &key, st.modio.allow_http)
}

/// The mod.io game that is `g`: its remembered number, else the one named like it. Games whose makers do not let other
/// programs download their mods are reported as such.
fn modio_game_for(state: &SharedState, g: &GameEntry) -> Result<u64, String> {
    {
        let st = read_state(state);
        if let Some(id) = st.config.modio_game.get(&g.appid) {
            return Ok(*id);
        }
    }
    let file = read_state(state).cache_root().join("modio_games.json");
    let mut known: std::collections::HashMap<String, u64> = fs::read(&file).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
    if let Some(id) = known.get(&g.appid) {
        let mut st = write_state(state);
        st.config.modio_game.insert(g.appid.clone(), *id);
        st.save_config();
        return Ok(*id);
    }
    let client = modio_client(state)?;
    let norm = |s: &str| s.to_lowercase().chars().filter(|c| c.is_alphanumeric()).collect::<String>();
    let found = client.find_games(&g.name)?;
    let want = norm(&g.name);
    let hit = found.iter().find(|x| norm(&x.name) == want).ok_or_else(|| format!("mod.io has no game called {}.", g.name))?;
    if !hit.open {
        return Err(format!("The makers of {} only allow their mod.io mods to be used inside the game, so ModForge cannot download them.", hit.name));
    }
    known.insert(g.appid.clone(), hit.id);
    let _ = fsutil::atomic_write(&file, serde_json::to_string(&known).unwrap_or_default());
    {
        let mut st = write_state(state);
        st.config.modio_game.insert(g.appid.clone(), hit.id);
        st.save_config();
    }
    Ok(hit.id)
}

/// Games on mod.io that look like `g` (the first words of its name), for the person to pick from when the name does not match.
fn modio_candidates(state: &SharedState, g: &GameEntry) -> Vec<Value> {
    let Ok(client) = modio_client(state) else { return Vec::new() };
    let words: Vec<&str> = g.name.split_whitespace().collect();
    let mut out: Vec<Value> = Vec::new();
    for q in [g.name.clone(), words.iter().take(2).cloned().collect::<Vec<_>>().join(" ")] {
        if q.trim().len() < 3 {
            continue;
        }
        for x in client.find_games(&q).unwrap_or_default().into_iter().filter(|x| x.open) {
            if !out.iter().any(|o| o["id"] == x.id) && out.len() < 8 {
                out.push(json!({ "id": x.id, "name": x.name }));
            }
        }
    }
    out
}

/// The person chose which mod.io game is this one.
fn modio_game_set(state: &SharedState, g: &GameEntry, id: u64) -> Result<String, String> {
    let client = modio_client(state)?;
    let game = client.game(id)?.ok_or("mod.io has no such game or mod.")?;
    if !game.open {
        return Err(format!("The makers of {} only allow their mod.io mods to be used inside the game, so ModForge cannot download them.", game.name));
    }
    {
        let mut st = write_state(state);
        st.config.modio_game.insert(g.appid.clone(), game.id);
        st.save_config();
        st.notifier.bump();
    }
    let file = read_state(state).cache_root().join("modio_games.json");
    let mut known: std::collections::HashMap<String, u64> = fs::read(&file).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
    known.insert(g.appid.clone(), game.id);
    fsutil::atomic_write(&file, serde_json::to_string(&known).unwrap_or_default()).map_err(|e| e.to_string())?;
    Ok(format!("{} will use the mod.io game {}.", g.name, game.name))
}

// ───────────────────────────── new downloads in the Downloads folder ─────────────────────────────

/// The folder the browser saves to.
fn downloads_folder() -> PathBuf {
    if cfg!(target_os = "linux") {
        if let Ok(t) = fs::read_to_string(home_dir().join(".config/user-dirs.dirs")) {
            let found = t.lines().find_map(|l| l.trim().strip_prefix("XDG_DOWNLOAD_DIR=")).map(|v| v.trim().trim_matches('"').replace("$HOME", &home_dir().to_string_lossy()));
            if let Some(d) = found.filter(|d| d.starts_with('/') && !d.contains("..")) {
                return PathBuf::from(d);
            }
        }
    }
    home_dir().join("Downloads")
}

fn inbox_file(st: &AppState) -> PathBuf {
    st.cache_root().join("inbox.json")
}

/// "name|size" of the files that were added or hidden already.
fn inbox_handled(state: &SharedState) -> HashSet<String> {
    let file = inbox_file(&read_state(state));
    fs::read(file).ok().and_then(|b| serde_json::from_slice::<Vec<String>>(&b).ok()).unwrap_or_default().into_iter().collect()
}

fn inbox_mark(state: &SharedState, keys: impl IntoIterator<Item = String>) {
    let mut all = inbox_handled(state);
    all.extend(keys);
    let mut list: Vec<String> = all.into_iter().collect();
    list.sort();
    let skip = list.len().saturating_sub(500);
    let file = inbox_file(&read_state(state));
    let _ = fsutil::atomic_write(&file, serde_json::to_string(&list[skip..]).unwrap_or_default());
}

/// Mod archives that arrived in `dir` lately and are finished: `(path, size, age in seconds)`, newest first.
fn inbox_scan(dir: &Path, handled: &HashSet<String>, now: std::time::SystemTime) -> Vec<(PathBuf, u64, u64)> {
    let mut out: Vec<(PathBuf, u64, u64, std::time::SystemTime)> = Vec::new();
    for e in fs::read_dir(dir).into_iter().flatten().flatten() {
        let path = e.path();
        let ext = path.extension().map(|x| x.to_string_lossy().to_lowercase()).unwrap_or_default();
        let Ok(meta) = e.metadata() else { continue };
        if !meta.is_file() || !ARCHIVE_EXTS.contains(&ext.as_str()) || meta.len() < 64 || meta.len() > 10 << 30 {
            continue;
        }
        let Ok(modified) = meta.modified() else { continue };
        let age = now.duration_since(modified).map(|d| d.as_secs()).unwrap_or(0);
        // still being written, or too old to be what the person just downloaded
        if !(8..=14 * 86_400).contains(&age) {
            continue;
        }
        let name = e.file_name().to_string_lossy().to_string();
        if handled.contains(&format!("{}|{}", name, meta.len())) || handled.contains(&name) {
            continue;
        }
        out.push((path, meta.len(), age, modified));
    }
    out.sort_by_key(|x| std::cmp::Reverse(x.3));
    out.truncate(30);
    out.into_iter().map(|(p, s, a, _)| (p, s, a)).collect()
}

/// Files already in this game's library are not offered again: its mods remember the file they came from.
fn inbox_installed(g: &GameEntry) -> HashSet<String> {
    library_of(g).map(|l| l.mods().into_iter().map(|m| m.source).collect()).unwrap_or_default()
}

fn inbox_json(state: &SharedState) -> Value {
    let Some(g) = selected_game(state).filter(|g| g.is_installed) else { return json!({ "items": [] }) };
    let dir = read_state(state).downloads_dir.clone().unwrap_or_else(downloads_folder);
    let mut handled = inbox_handled(state);
    handled.extend(inbox_installed(&g));
    let items: Vec<Value> = inbox_scan(&dir, &handled, std::time::SystemTime::now())
        .into_iter()
        .map(|(p, size, age)| {
            let name = p.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
            let (clean, version) = deploy::friendly_name(&name);
            json!({ "name": name, "clean": clean, "version": version, "size": size, "age_s": age })
        })
        .collect();
    json!({ "items": items, "game": g.name })
}

/// Add (or just hide) archives from the Downloads folder. Only names the scan itself lists are taken.
fn inbox_take(state: &SharedState, names: &[String], install: bool) {
    let Some(g) = selected_game(state).filter(|g| g.is_installed) else { return };
    let dir = read_state(state).downloads_dir.clone().unwrap_or_else(downloads_folder);
    let mut handled = inbox_handled(state);
    handled.extend(inbox_installed(&g));
    let listed = inbox_scan(&dir, &handled, std::time::SystemTime::now());
    let mut keys = Vec::new();
    for want in names {
        let Some((path, size, _)) = listed.iter().find(|(p, _, _)| p.file_name().map(|n| n.to_string_lossy() == want.as_str()).unwrap_or(false)).cloned() else { continue };
        keys.push(format!("{}|{}", want, size));
        if install {
            queue_install(state, path, false, InstallOpts { to_library: true, ..Default::default() });
        }
    }
    inbox_mark(state, keys);
    write_state(state).notifier.bump();
}

/// One mod.io mod (and the mods it needs, deepest first) into `g`'s library; what is already there is skipped.
fn modio_install(state: &SharedState, g: &GameEntry, mod_id: u64) -> Result<String, String> {
    modio_install_with(state, g, mod_id, None)
}

/// `replaces` makes the mod an update of that library mod (the mods it needs that are missing are added first).
fn modio_install_with(state: &SharedState, g: &GameEntry, mod_id: u64, replaces: Option<String>) -> Result<String, String> {
    let game_id = modio_game_for(state, g)?;
    let client = modio_client(state)?;
    let lib = library_of(g).ok_or_else(|| format!("{} is not installed.", g.name))?;
    let have: HashSet<u64> = lib.mods().iter().filter_map(|m| m.modio.as_ref().map(|r| r.mod_id)).collect();
    let mut plan: Vec<(u64, Option<String>)> = client.dependencies(game_id, mod_id).unwrap_or_default().into_iter().map(|d| d.mod_id).filter(|id| !have.contains(id)).map(|id| (id, None)).collect();
    if replaces.is_some() || !have.contains(&mod_id) {
        plan.push((mod_id, replaces));
    }
    if plan.is_empty() {
        return Ok("That mod is already in the library.".to_string());
    }
    let drops = read_state(state).cache_root().join("drops");
    let mut done = 0;
    for (id, replaces) in plan {
        let card = client.one(game_id, id)?;
        if card.download_url.is_empty() {
            return Err(format!("{} has no file to download.", card.name));
        }
        log(state, "INFO", format!("Fetching mod.io mod {}...", card.name));
        let dir = drops.join(fsutil::unique_token());
        let name = nexus::safe_file_name(&card.file_name, &format!("modio_{}.zip", id));
        let dest = dir.join(&name);
        let fetched = fs::create_dir_all(&dir).map_err(|e| e.to_string()).and_then(|_| client.download(&card.download_url, &dest));
        if let Err(e) = fetched {
            let _ = fs::remove_dir_all(&dir);
            return Err(e);
        }
        let opts = InstallOpts {
            game: Some(g.clone()),
            to_library: true,
            version: card.version.clone(),
            name: Some(card.name.clone()),
            modio: Some(modio::ModioRef { game_id, mod_id: id, file_id: card.file_id }),
            replaces,
            ..Default::default()
        };
        install_routed(dest, true, Arc::clone(state), opts);
        done += 1;
    }
    refresh_mods_shared(state);
    Ok(if done == 1 { "mod.io: 1 mod installed.".to_string() } else { format!("mod.io: {} mods installed (with what they need).", done) })
}

/// A mod from mod.io with a newer file.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ModioUpdate {
    pub appid: String,
    pub game: String,
    pub lib_id: String,
    pub name: String,
    pub installed_version: String,
    pub new_version: String,
    pub kind: &'static str,
    #[serde(skip)]
    pub mod_id: u64,
    #[serde(skip)]
    pub old_file: u64,
}

/// Ask mod.io about every library mod that came from there; the answer replaces the list of known updates.
fn check_modio_updates(state: &SharedState) -> usize {
    let Ok(client) = modio_client(state) else {
        write_state(state).modio_updates.clear();
        return 0;
    };
    let games: Vec<GameEntry> = read_state(state).games.iter().filter(|g| g.is_installed).cloned().collect();
    let mut rows: Vec<ModioUpdate> = Vec::new();
    let mut seen: std::collections::HashMap<(u64, u64), Result<modio::Card, String>> = Default::default();
    'games: for g in &games {
        let Some(lib) = library_of(g) else { continue };
        for m in lib.mods() {
            let Some(r) = m.modio.clone().filter(|r| r.valid() && m.update_rule != "never") else { continue };
            match seen.entry((r.game_id, r.mod_id)).or_insert_with(|| client.one(r.game_id, r.mod_id)) {
                Ok(card) if card.file_id > r.file_id => rows.push(ModioUpdate {
                    appid: g.appid.clone(),
                    game: g.name.clone(),
                    lib_id: m.id.clone(),
                    name: m.name.clone(),
                    installed_version: m.version.clone(),
                    new_version: if card.version.is_empty() { format!("#{}", card.file_id) } else { card.version.clone() },
                    kind: "modio",
                    mod_id: r.mod_id,
                    old_file: r.file_id,
                }),
                Ok(_) => {}
                // the key was refused or the limit reached: asking on would only repeat it
                Err(e) if e.contains("rejected") || e.contains("rate limit") => {
                    let e = e.clone();
                    log(state, "WARN", e);
                    break 'games;
                }
                Err(_) => {}
            }
        }
    }
    let n = rows.len();
    let mut st = write_state(state);
    st.modio_updates = rows;
    st.notifier.bump();
    if n > 0 {
        st.log("INFO", if n == 1 { "1 mod.io update is available.".to_string() } else { format!("{} mod.io updates are available.", n) });
    }
    n
}

/// Install the newer file of one mod from mod.io.
fn modio_update_apply(state: &SharedState, g: &GameEntry, row: &ModioUpdate) {
    match modio_install_with(state, g, row.mod_id, Some(row.lib_id.clone())) {
        Ok(m) => log(state, "OK", m),
        Err(e) => log(state, "ERR", e),
    }
}

/// Forget mod.io updates whose mod was updated or removed since.
fn prune_modio_updates(state: &SharedState) {
    let (rows, games) = {
        let st = read_state(state);
        if st.modio_updates.is_empty() {
            return;
        }
        (st.modio_updates.clone(), st.games.clone())
    };
    let keep: Vec<ModioUpdate> = rows
        .iter()
        .filter(|r| games.iter().find(|g| g.appid == r.appid).and_then(library_of).map(|l| l.mods().iter().any(|m| m.id == r.lib_id && m.modio.as_ref().map(|x| x.file_id == r.old_file).unwrap_or(false))).unwrap_or(false))
        .cloned()
        .collect();
    if keep.len() != rows.len() {
        let mut st = write_state(state);
        st.modio_updates = keep;
        st.notifier.bump();
    }
}

fn vt_key_file(st: &AppState) -> PathBuf {
    st.paths.config_file.with_file_name("virustotal_api_key")
}

fn load_vt_key(state: &SharedState) {
    let mut st = write_state(state);
    let keyfile = vt_key_file(&st);
    if let Some(k) = read_key_with_migration(&keyfile, "virustotal_api_key") {
        if virustotal::key_ok(&k) {
            st.vt.key = Some(k);
        }
    }
}

/// Ask VirusTotal about the program files of one library mod. Nothing is sent unless the switch in Settings is on
/// and a key is saved; only SHA-256 fingerprints leave the computer. Answers are remembered for a few days, and
/// a free key's four requests a minute are respected by waiting between them.
fn vt_check_mod(state: &SharedState, g: &GameEntry, mod_id: &str) -> Result<String, String> {
    let (on, key, base, allow_http, cache_path) = {
        let st = read_state(state);
        (st.config.virustotal, st.vt.key.clone(), st.vt.base.clone(), st.vt.allow_http, st.cache_root().join("virustotal.json"))
    };
    if !on {
        return Err("VirusTotal checks are switched off. Turn them on in Settings first.".into());
    }
    let key = key.ok_or("Add your VirusTotal key in Settings first.")?;
    let client = virustotal::Client::new(&base, &key, allow_http)?;
    let lib = library_of(g).ok_or_else(|| format!("{} is not installed.", g.name))?;
    let name = lib.mods().into_iter().find(|m| m.id == mod_id).map(|m| m.name).ok_or("Unknown mod.")?;
    let hashes = lib.program_hashes(mod_id)?;
    if hashes.is_empty() {
        return Err("This mod has no program files to check.".into());
    }
    let slot = format!("{}:{}", g.appid, mod_id);
    if !write_state(state).vt.busy.insert(slot.clone()) {
        return Ok(String::new());
    }
    let mut cache = virustotal::Cache::load(&cache_path);
    let mut report = VtReport { at_ms: now_ms(), waiting: hashes.len(), files: Vec::new() };
    let mut asked = false;
    let mut failed: Option<String> = None;
    for (file, sha) in &hashes {
        let cached = cache.get(sha, now_ms());
        let result = match cached {
            Some(v) => Ok(v),
            None => {
                if asked && !cfg!(test) {
                    thread::sleep(Duration::from_millis(15_500));
                }
                asked = true;
                client.lookup(sha).inspect(|v| cache.put(sha, v.clone(), now_ms()))
            }
        };
        report.waiting -= 1;
        match result {
            Ok(v) => report.files.push(VtFile { file: file.clone(), sha256: sha.clone(), verdict: Some(v), error: String::new() }),
            Err(e) => {
                report.files.push(VtFile { file: file.clone(), sha256: sha.clone(), verdict: None, error: e.clone() });
                failed = Some(e);
                report.waiting = 0;
            }
        }
        {
            let mut st = write_state(state);
            st.vt.reports.insert(slot.clone(), report.clone());
            st.notifier.bump();
        }
        if failed.is_some() {
            break;
        }
    }
    cache.save(&cache_path);
    {
        let mut st = write_state(state);
        st.vt.busy.remove(&slot);
        st.notifier.bump();
    }
    if let Some(e) = failed {
        return Err(e);
    }
    let worst = report.files.iter().filter_map(|f| f.verdict.as_ref()).map(|v| (v.flagged(), v.engines())).max().unwrap_or((0, 0));
    if worst.0 > 0 {
        let msg = format!("VirusTotal flags a program file of {}: {} of {} engines.", name, worst.0, worst.1);
        add_note(state, "virustotal", msg.clone(), &g.appid);
        return Ok(msg);
    }
    Ok(format!("VirusTotal checked the program files of {}: nothing flagged.", name))
}

fn nexus_client(state: &SharedState) -> Result<nexus::Nexus, String> {
    let st = read_state(state);
    let key = st.nexus.key.clone().ok_or("Add your Nexus Mods API key first (🌐 Nexus Mods).")?;
    nexus::Nexus::new(&st.nexus.base, &key, st.nexus.allow_http)
}

/// Everything the mod page shows for one library mod: the author's page when it is known, what
/// Gantry has about the mod either way.
fn mod_page_json(state: &SharedState, mod_id: &str) -> Result<Value, String> {
    let g = selected_game(state).ok_or("Pick a game first.")?;
    let lib = library_of(&g).ok_or("This game is not installed.")?;
    let mods = lib.mods();
    let m = mods.iter().find(|m| m.id == mod_id).cloned().ok_or("That mod is not in the library any more.")?;
    let nexus_ids: HashSet<u64> = mods.iter().filter_map(|x| x.nexus.as_ref().map(|n| n.mod_id)).collect();
    let thunder_keys: HashSet<String> = mods.iter().filter_map(|x| x.thunderstore.as_ref().map(|t| t.full_name().to_lowercase())).collect();

    let online: Option<Result<(modinfo::Page, bool), String>> = if let Some(nx) = m.nexus.clone().filter(|n| n.valid()) {
        Some(nexus_client(state).and_then(|client| {
            modinfo::cached(&format!("nx:{}:{}", nx.game, nx.mod_id), || {
                let mut p = modinfo::nexus_page(&client.mod_raw(&nx.game, nx.mod_id)?, &nx.game, nx.mod_id)?;
                if let Ok(files) = client.mod_files(&nx.game, nx.mod_id) {
                    p.changes = modinfo::nexus_changes(&files);
                }
                if let Ok(reqs) = client.requirements(&nx.game, nx.mod_id) {
                    p.requirements = modinfo::nexus_requirements(&reqs);
                }
                Ok(p)
            })
        }))
    } else if let Some(t) = m.thunderstore.clone() {
        let client = thunder_client(state);
        Some(modinfo::cached(&format!("ts:{}", t.full_name().to_lowercase()), || {
            let pkg = client.package(&t.namespace, &t.name)?;
            let version = pkg.get("latest").and_then(|l| l.get("version_number")).and_then(Value::as_str).unwrap_or(&t.version).to_string();
            Ok(modinfo::thunder_page(&pkg, &client.document(&t.namespace, &t.name, &version, "readme"), &client.document(&t.namespace, &t.name, &version, "changelog")))
        }))
    } else {
        m.modio.clone().filter(|r| r.valid()).map(|r| {
            modio_client(state).and_then(|client| {
                modinfo::cached(&format!("mio:{}:{}", r.game_id, r.mod_id), || {
                    let card = client.one(r.game_id, r.mod_id)?;
                    let deps = client.dependencies(r.game_id, r.mod_id).unwrap_or_default();
                    Ok(modinfo::modio_page(&card, &deps))
                })
            })
        })
    };
    let (mut page, stale, online_error) = match online {
        Some(Ok((p, stale))) => (p, stale, String::new()),
        Some(Err(e)) => (modinfo::Page { source: "local".into(), name: m.name.clone(), version: m.version.clone(), ..Default::default() }, false, e),
        None => (modinfo::Page { source: "local".into(), name: m.name.clone(), version: m.version.clone(), ..Default::default() }, false, String::new()),
    };
    page.mark_installed(&nexus_ids, &thunder_keys);
    // the core mods its files ask for go with what its page lists; and the other mods that stand on this one
    let dir = PathBuf::from(&g.game_dir);
    let in_game = |x: &deploy::LibMod| -> Vec<String> { x.files.iter().map(|f| if x.target.is_empty() { f.clone() } else { format!("{}/{}", x.target, f) }).collect() };
    for c in frameworks::wanted(&dir, &in_game(&m)) {
        if !page.requirements.iter().any(|r| r.name.to_lowercase().contains(&c.name.to_lowercase())) {
            page.requirements.push(modinfo::Req { name: c.name.into(), page_url: c.page.into(), notes: "Found from its files.".into(), installed: c.installed, core: true, ..Default::default() });
        }
    }
    let provides = frameworks::provided(&dir, &in_game(&m));
    let ts_key = m.thunderstore.as_ref().map(|t| t.full_name().to_lowercase());
    let ts_deps: std::collections::HashMap<String, Vec<String>> = match &ts_key {
        Some(_) => thunder_community_for(state, &g)
            .and_then(|c| thunder_listing(state, &c))
            .map(|all| all.into_iter().map(|l| (l.full_name.to_lowercase(), l.deps.iter().map(|d| d.to_lowercase()).collect())).collect())
            .unwrap_or_default(),
        None => Default::default(),
    };
    let needed_by: Vec<Value> = mods
        .iter()
        .filter(|x| x.id != m.id)
        .filter_map(|x| {
            if let Some(c) = (!provides.is_empty()).then(|| frameworks::wanted(&dir, &in_game(x))).and_then(|w| w.into_iter().find(|c| provides.contains(&c.name))) {
                return Some(json!({ "id": x.id, "name": x.name, "enabled": x.enabled, "via": c.name }));
            }
            let (Some(k), Some(t)) = (&ts_key, &x.thunderstore) else { return None };
            ts_deps.get(&t.full_name().to_lowercase()).filter(|d| d.contains(k)).map(|_| json!({ "id": x.id, "name": x.name, "enabled": x.enabled, "via": "" }))
        })
        .collect();

    let update = {
        let st = read_state(state);
        st.nexus.updates.iter().find(|u| u.lib_id == m.id && u.appid == g.appid).map(|u| json!({ "version": u.new_version, "file": u.new_file_name, "changes": u.changes, "page_url": u.page_url }))
            .or_else(|| st.thunder.updates.iter().find(|u| u.lib_id == m.id && u.appid == g.appid).map(|u| json!({ "version": u.new_version, "file": "", "changes": "", "page_url": "" })))
            .or_else(|| st.modio_updates.iter().find(|u| u.lib_id == m.id && u.appid == g.appid).map(|u| json!({ "version": u.new_version, "file": "", "changes": "", "page_url": "" })))
    };
    let notes: Vec<Value> = lib.notes(&m.id).into_iter().map(|(name, text)| json!({ "name": name, "html": richtext::text_html(&text.chars().take(40_000).collect::<String>()) })).collect();
    let mut v = serde_json::to_value(&page).map_err(|e| e.to_string())?;
    v["stale"] = json!(stale);
    v["online_error"] = json!(online_error);
    v["update"] = update.unwrap_or(Value::Null);
    v["rule"] = json!(m.update_rule);
    v["trust"] = json!({ "verified": m.verified, "programs": m.programs });
    {
        let st = read_state(state);
        let slot = format!("{}:{}", g.appid, m.id);
        v["vt"] = json!({ "on": st.config.virustotal, "has_key": st.vt.key.is_some(), "busy": st.vt.busy.contains(&slot), "report": st.vt.reports.get(&slot) });
    }
    v["previous"] = json!(lib.previous_versions(&m.id).into_iter().map(|(dir, version, ms)| json!({ "dir": dir, "version": version, "removed_ms": ms })).collect::<Vec<_>>());
    v["nexus_user"] = m.nexus.as_ref().filter(|n| n.valid()).map(|n| nexus_user_json(state, n)).unwrap_or(Value::Null);
    v["notes"] = json!(notes);
    v["needed_by"] = json!(needed_by);
    v["installed"] = json!({
        "id": m.id, "name": m.name, "version": m.version, "enabled": m.enabled, "source": m.source, "target": m.target, "placed_by": m.placed_by, "size": m.size,
        "file_count": m.files.len(), "files": m.files.iter().take(300).collect::<Vec<_>>(), "added_ms": m.added_ms, "note": m.note,
    });
    Ok(v)
}

/// The page of a mod that is not in the library yet (opened from the store): the author's own description,
/// what changed, what it needs, and which of that the library already has.
fn store_page_json(state: &SharedState, source: &str, id: &str) -> Result<Value, String> {
    let g = selected_game(state).filter(|g| g.is_installed).ok_or("Select an installed game first.")?;
    let mods = library_of(&g).map(|l| l.mods()).unwrap_or_default();
    let nexus_ids: HashSet<u64> = mods.iter().filter_map(|x| x.nexus.as_ref().map(|n| n.mod_id)).collect();
    let thunder_keys: HashSet<String> = mods.iter().filter_map(|x| x.thunderstore.as_ref().map(|t| t.full_name().to_lowercase())).collect();
    let (mut page, stale) = match source {
        "nexus" => {
            let mod_id: u64 = id.parse().map_err(|_| "That is not a Nexus mod.".to_string())?;
            let domain = nexus_domain_for(state, &g)?;
            let client = nexus_client(state)?;
            modinfo::cached(&format!("nx:{domain}:{mod_id}"), || {
                let mut p = modinfo::nexus_page(&client.mod_raw(&domain, mod_id)?, &domain, mod_id)?;
                if let Ok(files) = client.mod_files(&domain, mod_id) {
                    p.changes = modinfo::nexus_changes(&files);
                }
                if let Ok(reqs) = client.requirements(&domain, mod_id) {
                    p.requirements = modinfo::nexus_requirements(&reqs);
                }
                Ok(p)
            })?
        }
        "thunderstore" => {
            let r = thunderstore::parse_ref(id)?;
            let client = thunder_client(state);
            modinfo::cached(&format!("ts:{}", format!("{}-{}", r.namespace, r.name).to_lowercase()), || {
                let pkg = client.package(&r.namespace, &r.name)?;
                let version = pkg.get("latest").and_then(|l| l.get("version_number")).and_then(Value::as_str).unwrap_or("").to_string();
                Ok(modinfo::thunder_page(&pkg, &client.document(&r.namespace, &r.name, &version, "readme"), &client.document(&r.namespace, &r.name, &version, "changelog")))
            })?
        }
        "modio" => {
            let mod_id: u64 = id.parse().map_err(|_| "That is not a mod.io mod.".to_string())?;
            let game_id = modio_game_for(state, &g)?;
            let client = modio_client(state)?;
            modinfo::cached(&format!("mio:{game_id}:{mod_id}"), || {
                let card = client.one(game_id, mod_id)?;
                let deps = client.dependencies(game_id, mod_id).unwrap_or_default();
                Ok(modinfo::modio_page(&card, &deps))
            })?
        }
        _ => return Err("Unknown mod site.".into()),
    };
    page.mark_installed(&nexus_ids, &thunder_keys);
    let modio_ids: HashSet<u64> = mods.iter().filter_map(|x| x.modio.as_ref().map(|r| r.mod_id)).collect();
    for r in page.requirements.iter_mut() {
        if let Some(n) = r.key.strip_prefix("modio:").and_then(|n| n.parse::<u64>().ok()) {
            r.installed = modio_ids.contains(&n);
        }
    }
    let already = match source {
        "nexus" => id.parse::<u64>().map(|n| nexus_ids.contains(&n)).unwrap_or(false),
        "modio" => id.parse::<u64>().map(|n| modio_ids.contains(&n)).unwrap_or(false),
        _ => thunder_keys.contains(&id.to_lowercase()),
    };
    let mut v = serde_json::to_value(&page).map_err(|e| e.to_string())?;
    v["stale"] = json!(stale);
    v["store"] = json!({ "source": source, "id": id, "already": already, "premium": read_state(state).nexus.user.as_ref().map(|u| u.premium).unwrap_or(false) });
    Ok(v)
}

/// The `Data` folder of a Bethesda style game and its layout, when the selected game is one.
fn plugin_game(state: &SharedState) -> Option<(GameEntry, plugins::Layout, PathBuf)> {
    let g = selected_game(state).filter(|g| g.is_installed && (cfg!(windows) || !g.pfx.is_empty()))?;
    let layout = plugins::layout_for(Path::new(&g.game_dir))?;
    let data = fs::read_dir(&g.game_dir).ok()?.flatten().find(|e| e.path().is_dir() && e.file_name().to_string_lossy().eq_ignore_ascii_case("data"))?.path();
    Some((g, layout, data))
}

/// Does the selected game keep its plugins in a `Data` folder (looked up at most once a minute per game)?
fn has_plugins(g: &GameEntry) -> bool {
    static CACHE: std::sync::LazyLock<Mutex<std::collections::HashMap<String, (Instant, bool)>>> = std::sync::LazyLock::new(|| Mutex::new(std::collections::HashMap::new()));
    if let Some((at, b)) = CACHE.lock().unwrap_or_else(|e| e.into_inner()).get(&g.appid) {
        if at.elapsed() < Duration::from_secs(60) {
            return *b;
        }
    }
    let b = plugins::layout_for(Path::new(&g.game_dir)).is_some();
    CACHE.lock().unwrap_or_else(|e| e.into_inner()).insert(g.appid.clone(), (Instant::now(), b));
    b
}

/// The plugins of the selected game in load order, with what is wrong with each. `sort` answers with the
/// order after sorting (nothing is written).
fn plugins_json(state: &SharedState, sort: bool) -> Value {
    let Some((g, layout, data)) = plugin_game(state) else { return json!({ "supported": false }) };
    let list = plugins::scan(&data, &layout);
    let path = plugins::plugins_txt_path(&profsaves::bases(&g).local, &layout);
    let saved = fs::read_to_string(&path).map(|t| plugins::parse_plugins_txt(&t)).unwrap_or_default();
    let mut order = plugins::merged_order(&list, &saved, &layout);
    if sort {
        order = plugins::sort(&order, &list, &layout);
    }
    let pos: std::collections::HashMap<String, usize> = order.iter().enumerate().map(|(i, (n, _))| (n.to_lowercase(), i)).collect();
    let rows: Vec<Value> = order
        .iter()
        .filter_map(|(name, on)| {
            let p = list.iter().find(|p| p.name.eq_ignore_ascii_case(name))?;
            let missing = plugins::missing_masters(p, &list);
            let late: Vec<&String> = p.masters.iter().filter(|m| pos.get(&m.to_lowercase()).map(|&i| i > pos[&p.name.to_lowercase()]).unwrap_or(false)).collect();
            Some(json!({ "name": p.name, "kind": p.kind, "on": *on || p.base, "base": p.base, "masters": p.masters, "missing": missing, "late": late, "description": p.description, "author": p.author }))
        })
        .collect();
    let problems = rows.iter().filter(|r| !r["missing"].as_array().map(|a| a.is_empty()).unwrap_or(true) || !r["late"].as_array().map(|a| a.is_empty()).unwrap_or(true)).count();
    json!({ "supported": true, "game": g.name, "path": path.to_string_lossy(), "exists": path.is_file(), "order": rows, "problems": problems })
}

/// Write `plugins.txt` for the selected game (the first version is kept next to it).
fn plugins_save(state: &SharedState, order: &[(String, bool)]) -> Result<String, String> {
    let (g, layout, data) = plugin_game(state).ok_or("This game does not have plugins ModForge can order.")?;
    if read_state(state).is_running.load(Ordering::SeqCst) {
        return Err("Close the game first: it reads the plugin list when it starts.".into());
    }
    let list = plugins::scan(&data, &layout);
    // only plugins that exist can be listed; nothing else ends up in the file
    let clean: Vec<(String, bool)> = order.iter().filter_map(|(n, on)| list.iter().find(|p| p.name.eq_ignore_ascii_case(n)).map(|p| (p.name.clone(), *on))).collect();
    let path = plugins::plugins_txt_path(&profsaves::bases(&g).local, &layout);
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    let keep = path.with_extension("txt.gantry-orig");
    if path.is_file() && !keep.exists() {
        let _ = fs::copy(&path, &keep);
    }
    fsutil::atomic_write(&path, plugins::write_plugins_txt(&clean, &layout)).map_err(|e| format!("Could not save the plugin list: {}", e))?;
    Ok(format!("Saved the load order of {} plugins.", clean.len()))
}

/// The support entry of a game as the editor shows it: the one Gantry has, or a draft made from what
/// it can see (loader, Nexus and Thunderstore names, folders that look like mod folders).
fn pack_draft(state: &SharedState, g: &GameEntry) -> Value {
    let existing = recipes::find(&read_state(state).recipes, &g.appid, g.recipe_steam_id()).cloned();
    let entry = match &existing {
        Some(r) => r.clone(),
        None => recipes::Recipe {
            appid: g.appid.clone(),
            name: g.name.clone(),
            loader: loaders::detect(Path::new(&g.game_dir)).map(|u| match (u.loader.as_str(), u.il2cpp) {
                ("melonloader", _) => "melonloader",
                (_, true) => "bepinex6",
                _ => "bepinex5",
            }.to_string()).unwrap_or_default(),
            thunderstore: read_state(state).config.thunder_community.get(&g.appid).cloned().unwrap_or_default(),
            nexus: read_state(state).config.nexus_domain.get(&g.appid).cloned().unwrap_or_default(),
            ..Default::default()
        },
    };
    let dirs: Vec<String> = fs::read_dir(&g.game_dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.path().is_dir())
        .map(|e| e.file_name().to_string_lossy().to_string())
        .filter(|n| !n.starts_with('.') && !n.ends_with("_Data") && !["MonoBleedingEdge", "Engine", "EasyAntiCheat", "__MACOSX"].contains(&n.as_str()))
        .take(14)
        .collect();
    json!({ "existing": existing.is_some(), "entry": entry, "folders_seen": dirs })
}

/// One search across the game's library, Nexus Mods and Thunderstore. A source that cannot answer
/// (no key, no community) is named in `notes` instead of failing the whole search.
fn find_everywhere(state: &SharedState, q: &str) -> Value {
    let q = q.trim();
    let Some(g) = selected_game(state).filter(|g| g.is_installed) else { return json!({ "error": "Select an installed game first." }) };
    if q.chars().count() < 2 {
        return json!({ "results": [], "notes": [] });
    }
    let lower = q.to_lowercase();
    let mut notes: Vec<String> = Vec::new();
    let mods = library_of(&g).map(|l| l.mods()).unwrap_or_default();
    let mine: Vec<Value> = mods
        .iter()
        .filter(|m| m.name.to_lowercase().contains(&lower))
        .map(|m| json!({ "source": "library", "id": m.id, "name": m.name, "summary": m.source, "version": m.version, "author": "", "popularity": 0, "picture": "", "installed": true }))
        .collect();
    let have_nexus: HashSet<u64> = mods.iter().filter_map(|m| m.nexus.as_ref().map(|n| n.mod_id)).collect();
    let have_ts: HashSet<String> = mods.iter().filter_map(|m| m.thunderstore.as_ref().map(|t| t.full_name().to_lowercase())).collect();
    let have_modio: HashSet<u64> = mods.iter().filter_map(|m| m.modio.as_ref().map(|r| r.mod_id)).collect();
    let has_modio_key = read_state(state).modio.key.is_some();
    // the three sites are asked at the same time
    let (nexus_r, thunder_r, modio_r) = thread::scope(|sc| {
        let nx = sc.spawn(|| nexus_domain_for(state, &g).and_then(|d| nexus_client(state).and_then(|c| c.search(&d, q, 20))));
        let ts = sc.spawn(|| thunder_community_for(state, &g).and_then(|c| thunder_listing(state, &c)).map(|all| thunderstore::search(&all, q, 20).into_iter().cloned().collect::<Vec<_>>()));
        let mio = sc.spawn(|| -> Result<Vec<modio::Card>, String> {
            if !has_modio_key {
                return Ok(Vec::new());
            }
            let id = modio_game_for(state, &g)?;
            modio_client(state)?.mods(id, &modio::Query { text: q.to_string(), sort: "downloads".into(), offset: 0, limit: 20 }).map(|(l, _)| l)
        });
        fn gone<T>() -> Result<T, String> {
            Err("ModForge could not finish this search.".to_string())
        }
        (nx.join().unwrap_or_else(|_| gone()), ts.join().unwrap_or_else(|_| gone()), mio.join().unwrap_or_else(|_| gone()))
    });
    let nexus_hits: Vec<Value> = match nexus_r {
        Ok(v) => v.into_iter().map(|m| json!({ "source": "nexus", "id": m.mod_id.to_string(), "name": m.name, "summary": m.summary, "version": m.version, "author": m.author, "popularity": m.endorsements, "picture": m.picture, "installed": have_nexus.contains(&m.mod_id) })).collect(),
        Err(e) => {
            notes.push(format!("Nexus Mods: {}", e));
            Vec::new()
        }
    };
    let thunder_hits: Vec<Value> = match thunder_r {
        Ok(all) => all
            .into_iter()
            .map(|l| json!({ "source": "thunderstore", "id": l.full_name, "name": l.name.replace('_', " "), "summary": l.description, "version": l.version, "author": l.owner, "popularity": l.downloads, "picture": l.icon, "installed": have_ts.contains(&l.full_name.to_lowercase()) }))
            .collect(),
        Err(e) => {
            notes.push(format!("Thunderstore: {}", e));
            Vec::new()
        }
    };
    let modio_hits: Vec<Value> = match modio_r {
        Ok(list) => list
            .into_iter()
            .filter(|c| !c.adult)
            .map(|c| json!({ "source": "modio", "id": c.id.to_string(), "name": c.name, "summary": c.summary, "version": c.version, "author": c.author, "popularity": c.downloads, "picture": richtext::image_url(&c.picture).unwrap_or_default(), "installed": have_modio.contains(&c.id) }))
            .collect(),
        // most games are not on mod.io at all; that is not worth a note
        Err(e) if e.contains("has no game called") => Vec::new(),
        Err(e) => {
            notes.push(format!("mod.io: {}", e));
            Vec::new()
        }
    };
    // the game's core mods come from their makers' GitHub releases
    let cores: Vec<Value> = frameworks::cores(Path::new(&g.game_dir))
        .into_iter()
        .filter(|c| c.name.to_lowercase().contains(&lower))
        .map(|c| json!({ "source": "github", "id": c.name, "name": c.name, "summary": c.repo, "version": "", "author": c.repo.split('/').next().unwrap_or(""), "popularity": 0, "picture": "", "installed": c.installed, "page_url": c.page }))
        .collect();
    // the library first, then the core mods, then the sites taking turns so none buries the others
    let mut results = mine;
    results.extend(cores);
    let mut sites = [nexus_hits.into_iter(), thunder_hits.into_iter(), modio_hits.into_iter()];
    let norm = |v: &Value| v["name"].as_str().unwrap_or("").to_lowercase().chars().filter(|c| c.is_alphanumeric()).collect::<String>();
    loop {
        let round: Vec<Value> = sites.iter_mut().filter_map(|it| it.next()).collect();
        if round.is_empty() {
            break;
        }
        for r in round {
            // the same mod on two sites is shown once, with the other site named
            let key = norm(&r);
            if let Some(first) = results.iter_mut().find(|x| x["source"] != "library" && x["source"] != r["source"] && !key.is_empty() && norm(x) == key) {
                let also = first.as_object_mut().map(|o| o.entry("also").or_insert_with(|| json!([])));
                if let Some(Value::Array(a)) = also {
                    a.push(json!({ "source": r["source"], "id": r["id"], "installed": r["installed"] }));
                }
                if r["installed"] == true {
                    first["installed"] = json!(true);
                }
                continue;
            }
            results.push(r);
        }
    }
    json!({ "results": results, "notes": notes })
}

/// Your endorsements and tracked mods, kept for two minutes (and forgotten after a change).
type Endorsements = std::collections::HashMap<(String, u64), String>;
type Tracking = Vec<(String, u64)>;
static NEXUS_SOCIAL: std::sync::LazyLock<Mutex<Option<(Instant, Endorsements, Tracking)>>> = std::sync::LazyLock::new(|| Mutex::new(None));

fn nexus_social(client: &nexus::Nexus) -> Option<(Endorsements, Tracking)> {
    if let Some((at, e, t)) = NEXUS_SOCIAL.lock().unwrap_or_else(|e| e.into_inner()).as_ref() {
        if at.elapsed() < Duration::from_secs(120) {
            return Some((e.clone(), t.clone()));
        }
    }
    let (e, t) = (client.endorsements().ok()?, client.tracked().ok()?);
    *NEXUS_SOCIAL.lock().unwrap_or_else(|e| e.into_inner()) = Some((Instant::now(), e.clone(), t.clone()));
    Some((e, t))
}

fn forget_nexus_social() {
    *NEXUS_SOCIAL.lock().unwrap_or_else(|e| e.into_inner()) = None;
}

/// `{ "endorsed": "Endorsed" | "Abstained" | "Undecided", "tracked": bool }` for a Nexus mod, or null when unknown.
fn nexus_user_json(state: &SharedState, nx: &nexus::NexusRef) -> Value {
    let Ok(client) = nexus_client(state) else { return Value::Null };
    match nexus_social(&client) {
        Some((e, t)) => json!({ "endorsed": e.get(&(nx.game.clone(), nx.mod_id)).cloned().unwrap_or_else(|| "Undecided".into()), "tracked": t.contains(&(nx.game.clone(), nx.mod_id)) }),
        None => Value::Null,
    }
}

/// Endorse / take back / track / untrack the Nexus mod of a library mod; answers what is true afterwards.
fn nexus_social_act(state: &SharedState, lib_id: &str, act: &str) -> Value {
    let run = || -> Result<Value, String> {
        let g = selected_game(state).ok_or("Pick a game first.")?;
        let lib = library_of(&g).ok_or("This game is not installed.")?;
        let m = lib.mods().into_iter().find(|m| m.id == lib_id).ok_or("That mod is not in the library any more.")?;
        let nx = m.nexus.clone().filter(|n| n.valid()).ok_or("This mod did not come from Nexus Mods.")?;
        let client = nexus_client(state)?;
        match act {
            "endorse" | "abstain" => client.endorse(&nx.game, nx.mod_id, &m.version, act == "endorse")?,
            "track" | "untrack" => client.track(&nx.game, nx.mod_id, act == "track")?,
            _ => return Err("Unknown action.".into()),
        }
        forget_nexus_social();
        log(state, "OK", match act {
            "endorse" => format!("Endorsed {} on Nexus Mods.", m.name),
            "abstain" => format!("Took back the endorsement of {}.", m.name),
            "track" => format!("Now tracking {} on Nexus Mods.", m.name),
            _ => format!("No longer tracking {}.", m.name),
        });
        Ok(json!({ "nexus_user": nexus_user_json(state, &nx) }))
    };
    run().unwrap_or_else(|e| json!({ "error": e }))
}

/// A Nexus collection as the screen shows it: every mod with whether the game's library already has it.
fn collection_json(state: &SharedState, link: &str) -> Result<Value, String> {
    let (domain, slug, rev) = nexus::parse_collection_link(link).ok_or("That does not look like a Nexus collection link.")?;
    let client = nexus_client(state)?;
    let c = client.collection(&domain, &slug, rev)?;
    let have: HashSet<u64> = selected_game(state).and_then(|g| library_of(&g)).map(|l| l.mods().iter().filter_map(|m| m.nexus.as_ref().filter(|n| n.game == c.domain).map(|n| n.mod_id)).collect()).unwrap_or_default();
    let (game_domain, premium) = {
        let g = selected_game(state);
        (g.and_then(|g| read_state(state).config.nexus_domain.get(&g.appid).cloned()), read_state(state).nexus.user.as_ref().map(|u| u.premium).unwrap_or(false))
    };
    let mods: Vec<Value> = c
        .mods
        .iter()
        .map(|m| json!({ "mod_id": m.mod_id, "file_id": m.file_id, "name": m.name, "version": m.version, "size": m.size, "optional": m.optional, "installed": have.contains(&m.mod_id),
            "page_url": format!("https://www.nexusmods.com/{}/mods/{}?tab=files&file_id={}&nmm=1", if m.domain.is_empty() { &c.domain } else { &m.domain }, m.mod_id, m.file_id) }))
        .collect();
    let free = selected_game(state).and_then(|g| fsutil::free_bytes(Path::new(&g.game_dir)));
    Ok(json!({ "name": c.name, "slug": c.slug, "domain": c.domain, "game_name": c.game_name, "revision": c.revision, "mods": mods, "premium": premium,
        "other_game": game_domain.map(|d| d != c.domain).unwrap_or(false), "free_bytes": free }))
}

/// Premium: install every mod of a collection the library does not have yet, one after the other.
fn collection_install(state: &SharedState, link: &str, optional: bool) {
    let run = || -> Result<(), String> {
        let g = selected_game(state).filter(|g| g.is_installed).ok_or("Select an installed game first.")?;
        let premium = read_state(state).nexus.user.as_ref().map(|u| u.premium).unwrap_or(false);
        let v = collection_json(state, link)?;
        let domain = v["domain"].as_str().unwrap_or("").to_string();
        if v["other_game"].as_bool().unwrap_or(false) {
            return Err(format!("This collection is for {}, not for {}.", v["game_name"].as_str().unwrap_or("another game"), g.name));
        }
        let todo: Vec<(u64, u64, String)> = v["mods"].as_array().into_iter().flatten().filter(|m| !m["installed"].as_bool().unwrap_or(false) && (optional || !m["optional"].as_bool().unwrap_or(false))).map(|m| (m["mod_id"].as_u64().unwrap_or(0), m["file_id"].as_u64().unwrap_or(0), m["name"].as_str().unwrap_or("").to_string())).collect();
        if todo.is_empty() {
            log(state, "OK", format!("Everything in “{}” is already in your library.", v["name"].as_str().unwrap_or("")));
            return Ok(());
        }
        let total = todo.len();
        if !premium {
            // a free account downloads through the website: each mod's page opens in turn, and each download is installed as it arrives
            let mut st = write_state(state);
            let mut queued = 0;
            for (mod_id, file_id, name) in todo {
                if st.nexus.queue.iter().chain(st.nexus.wants.iter()).any(|w| w.game == domain && w.mod_id == mod_id) {
                    continue;
                }
                st.nexus.queue.push_back(Want { appid: g.appid.clone(), game: domain.clone(), mod_id, name, asked_ms: 0, file_id });
                queued += 1;
            }
            queue_counted(&mut st, queued);
            drop(st);
            log(state, "INFO", format!("Collection “{}”: {} mods queued. Press Mod Manager Download on each page that opens; ModForge installs each one.", v["name"].as_str().unwrap_or(""), queued));
            nexus_queue_next(state);
            return Ok(());
        }
        for (i, (mod_id, file_id, name)) in todo.into_iter().enumerate() {
            log(state, "INFO", format!("Collection: {}/{} {}", i + 1, total, name));
            nexus_fetch_and_install(state, &g, &nexus::NxmLink { game: domain.clone(), mod_id, file_id, key: None, expires: None }, None);
        }
        log(state, "OK", format!("Collection “{}” done: {} mods processed.", v["name"].as_str().unwrap_or(""), total));
        Ok(())
    };
    if let Err(e) = run() {
        log(state, "ERR", e);
    }
}

/// Your tracked mods of one game, as the mod browser lists them.
fn tracked_listing(state: &SharedState, client: &nexus::Nexus, domain: &str) -> Result<Vec<Value>, String> {
    let ids: Vec<u64> = client.tracked()?.into_iter().filter(|(d, _)| d == domain).map(|(_, id)| id).take(25).collect();
    Ok(ids
        .into_iter()
        .filter_map(|id| {
            let (p, _) = modinfo::cached(&format!("nx:{domain}:{id}"), || modinfo::nexus_page(&client.mod_raw(domain, id)?, domain, id)).ok()?;
            let _ = state;
            Some(json!({ "mod_id": id, "name": p.name, "summary": p.summary.chars().take(220).collect::<String>(), "version": p.version, "author": p.author, "endorsements": p.endorsements, "picture": p.picture }))
        })
        .collect())
}

/// Check a key against Nexus; keep it (in a file only the user can read) only when it works.
fn set_nexus_key(state: &SharedState, key: &str) -> Result<String, String> {
    let (base, allow_http) = {
        let st = read_state(state);
        (st.nexus.base.clone(), st.nexus.allow_http)
    };
    let client = nexus::Nexus::new(&base, key.trim(), allow_http)?;
    let user = client.validate()?;
    {
        let mut st = write_state(state);
        fsutil::atomic_write_private(&nexus_key_file(&st), key.trim()).map_err(|e| format!("Could not save the key: {}", e))?;
        st.nexus.key = Some(key.trim().to_string());
        st.nexus.user = Some(user.clone());
        st.notifier.bump();
    }
    Ok(if user.premium {
        format!("Nexus Mods connected as {} (Premium).", user.name)
    } else {
        format!("Nexus Mods connected as {} (free account).", user.name)
    })
}

fn clear_nexus_key(state: &SharedState) {
    let mut st = write_state(state);
    clear_key_file_with_migration(&nexus_key_file(&st), "nexus_api_key");
    st.nexus.key = None;
    st.nexus.user = None;
    st.nexus.pending = None;
    st.nexus.pending_for = None;
    st.nexus.expect = None;
    st.nexus.updates.clear();
    st.nexus.checked_ms = 0;
    st.nexus.check_error = None;
    st.notifier.bump();
}

/// A "Mod Manager Download" click arrived. Nothing is downloaded until the user confirms.
fn nxm_request(state: &SharedState, url: &str) -> Result<String, String> {
    let link = nexus::parse_nxm(url)?;
    if read_state(state).nexus.key.is_none() {
        return Err("A Nexus link arrived, but no API key is set. Open 🌐 Nexus Mods and add your key first.".into());
    }
    let msg = format!("Nexus download waiting for your confirmation: {} mod {} file {}.", link.game, link.mod_id, link.file_id);
    {
        let mut st = write_state(state);
        // the update the user asked for on the website arrives here
        st.nexus.pending_for = match st.nexus.expect.take() {
            Some((t, r)) if r.game == link.game && r.mod_id == link.mod_id && r.file_id == link.file_id => Some(t),
            other => {
                st.nexus.expect = other;
                None
            }
        };
        // a link for a page Gantry opened itself needs no second question: the click on the website was the answer
        let now = now_ms();
        st.nexus.wants.retain(|w| now.saturating_sub(w.asked_ms) < WANT_FOR_MS);
        let asked = st.nexus.wants.iter().position(|w| w.game == link.game && w.mod_id == link.mod_id).map(|i| st.nexus.wants.remove(i));
        if let Some(w) = &asked {
            st.nexus.pending_for = Some(UpdateTarget { appid: w.appid.clone(), lib_id: String::new(), name: w.name.clone() });
            queue_one_done(&mut st);
        }
        st.nexus.pending = Some(link);
        st.notifier.bump();
        drop(st);
        if asked.is_some() {
            let state_bg = Arc::clone(state);
            thread::spawn(move || nxm_confirm(&state_bg, true));
            return Ok(msg);
        }
    }
    add_note(state, "waiting", "Open ModForge to confirm the Nexus download.", "");
    if !cfg!(test) {
        let lang = read_state(state).config.language.clone();
        platform::notify(&i18n::tr(&lang, "Mod download waiting"), &i18n::tr(&lang, "Open ModForge to confirm the Nexus download."));
    }
    Ok(msg)
}

fn nxm_confirm(state: &SharedState, accept: bool) {
    let (link, target) = {
        let mut st = write_state(state);
        let l = st.nexus.pending.take();
        let t = st.nexus.pending_for.take();
        st.notifier.bump();
        (l, t)
    };
    let Some(link) = link else {
        log(state, "WARN", "No Nexus download is waiting.");
        return;
    };
    if !accept {
        log(state, "INFO", "Nexus download cancelled.");
        return;
    }
    let game = match &target {
        Some(t) => read_state(state).games.iter().find(|g| g.appid == t.appid).cloned(),
        None => selected_game(state),
    };
    let Some(game) = game else {
        log(state, "WARN", "Select a game first, then confirm the download again.");
        return;
    };
    nexus_fetch_and_install(state, &game, &link, target.map(|t| t.lib_id).filter(|id| !id.is_empty()));
    nexus_queue_next(state);
}

/// `n` more mods joined the queue (a new run starts the count again).
fn queue_counted(st: &mut AppState, n: usize) {
    if st.nexus.queue_total > 0 && st.nexus.queue_done >= st.nexus.queue_total {
        st.nexus.queue_total = 0;
        st.nexus.queue_done = 0;
    }
    if st.nexus.queue_total == 0 {
        st.nexus.queue_done = 0;
        // a page that is already open counts too
        st.nexus.queue_total = st.nexus.wants.len();
    }
    st.nexus.queue_total += n;
    st.notifier.bump();
}

/// One mod of the queue is done (installed, or skipped). The count ends with the queue.
fn queue_one_done(st: &mut AppState) {
    if st.nexus.queue_total > 0 {
        st.nexus.queue_done = (st.nexus.queue_done + 1).min(st.nexus.queue_total);
    }
    if st.nexus.queue.is_empty() && st.nexus.wants.is_empty() {
        st.nexus.queue_total = 0;
        st.nexus.queue_done = 0;
    }
    st.notifier.bump();
}

/// The queue panel's buttons: "skip" the mod being waited for, "reopen" its page, or "stop" the whole queue.
fn nexus_queue_action(state: &SharedState, action: &str) {
    match action {
        "skip" => {
            let skipped = {
                let mut st = write_state(state);
                let w = (!st.nexus.wants.is_empty()).then(|| st.nexus.wants.remove(0));
                if w.is_some() {
                    queue_one_done(&mut st);
                }
                w
            };
            if let Some(w) = skipped {
                log(state, "INFO", format!("Skipped {}.", w.name));
            }
            nexus_queue_next(state);
        }
        "reopen" => {
            let w = read_state(state).nexus.wants.first().cloned();
            if let Some(w) = w {
                write_state(state).nexus.wants[0].asked_ms = now_ms();
                if w.file_id != 0 {
                    open_page(nexus::NexusRef { game: w.game.clone(), mod_id: w.mod_id, file_id: w.file_id }.files_page(w.file_id));
                } else {
                    open_nexus_files_page(state, &w.game, w.mod_id);
                }
            }
        }
        "stop" => {
            let n = {
                let mut st = write_state(state);
                let n = st.nexus.queue.len() + st.nexus.wants.len();
                st.nexus.queue.clear();
                st.nexus.wants.clear();
                st.nexus.queue_total = 0;
                st.nexus.queue_done = 0;
                st.notifier.bump();
                n
            };
            if n > 0 {
                log(state, "INFO", format!("Stopped waiting for {} Nexus download(s).", n));
            }
        }
        _ => {}
    }
}

/// A file saved with the website's "Manual download" (or by a browser that does not hand links over) is matched to a
/// mod Gantry is waiting for by the Nexus id in its name, and installed as if the link had come. Gives how many.
fn match_downloads_to_wants(state: &SharedState) -> usize {
    let wants: Vec<Want> = {
        let mut st = write_state(state);
        let now = now_ms();
        st.nexus.wants.retain(|w| now.saturating_sub(w.asked_ms) < WANT_FOR_MS);
        st.nexus.wants.clone()
    };
    if wants.is_empty() {
        return 0;
    }
    let dir = read_state(state).downloads_dir.clone().unwrap_or_else(downloads_folder);
    let handled = inbox_handled(state);
    let now = std::time::SystemTime::now();
    let mut n = 0;
    for (path, size, age) in inbox_scan(&dir, &handled, now) {
        let name = path.file_name().map(|x| x.to_string_lossy().to_string()).unwrap_or_default();
        let Some(dn) = nexus::parse_download_name(&name) else { continue };
        // only a file that arrived after the page was opened (with a minute to spare for clocks)
        let arrived_ms = now_ms().saturating_sub(age * 1000);
        let Some(w) = wants.iter().find(|w| w.mod_id == dn.mod_id && arrived_ms + 60_000 >= w.asked_ms) else { continue };
        let Some(g) = game_by_appid(state, &w.appid) else { continue };
        let file_id = if w.file_id != 0 { w.file_id } else { nexus_client(state).ok().and_then(|c| c.mod_files(&w.game, w.mod_id).ok()).and_then(|mf| mf.find_download(&dn).map(|f| f.file_id)).unwrap_or(0) };
        inbox_mark(state, [format!("{}|{}", name, size)]);
        {
            let mut st = write_state(state);
            st.nexus.wants.retain(|x| !(x.game == w.game && x.mod_id == w.mod_id));
            queue_one_done(&mut st);
        }
        log(state, "INFO", format!("{} arrived in your Downloads folder; installing it.", name));
        let nexus = (file_id != 0).then(|| nexus::NexusRef { game: w.game.clone(), mod_id: w.mod_id, file_id });
        install_routed(path, false, Arc::clone(state), InstallOpts { game: Some(g), to_library: true, nexus, ..Default::default() });
        n += 1;
        nexus_queue_next(state);
    }
    n
}

/// While Gantry waits for Nexus downloads, look in the Downloads folder every few seconds.
fn spawn_want_watcher(state: Weak<RwLock<AppState>>) {
    thread::spawn(move || loop {
        thread::sleep(if cfg!(test) { Duration::from_millis(200) } else { Duration::from_secs(3) });
        let Some(st) = state.upgrade() else { return };
        if !read_state(&st).nexus.wants.is_empty() {
            match_downloads_to_wants(&st);
        }
    });
}

/// Opens the Nexus page of the next mod in the queue, when nothing else is being waited for.
fn nexus_queue_next(state: &SharedState) {
    let (w, more) = {
        let mut st = write_state(state);
        let now = now_ms();
        st.nexus.wants.retain(|w| now.saturating_sub(w.asked_ms) < WANT_FOR_MS);
        if !st.nexus.wants.is_empty() {
            return;
        }
        let Some(mut w) = st.nexus.queue.pop_front() else { return };
        w.asked_ms = now;
        st.nexus.wants.push(w.clone());
        st.notifier.bump();
        (w, st.nexus.queue.len())
    };
    if w.file_id != 0 {
        open_page(nexus::NexusRef { game: w.game.clone(), mod_id: w.mod_id, file_id: w.file_id }.files_page(w.file_id));
    } else {
        open_nexus_files_page(state, &w.game, w.mod_id);
    }
    log(state, "INFO", format!("Opened the Nexus page of {}. Press its Mod Manager Download button; ModForge installs it by itself.", w.name));
    if more > 0 {
        log(state, "INFO", format!("{} more are waiting for it; each page opens when the one before is done.", more));
    }
}

/// Open a website page in the browser (never from tests).
fn open_page(url: String) {
    if !cfg!(test) {
        xdg_open(url);
    }
}

/// The Nexus page where the download button of a mod's main file is (the exact file when the key lets Gantry find it).
fn open_nexus_files_page(state: &SharedState, domain: &str, mod_id: u64) {
    let file = nexus_client(state).ok().and_then(|c| nexus_main_file(&c, domain, mod_id).ok());
    let url = match file {
        Some(file_id) => nexus::NexusRef { game: domain.to_string(), mod_id, file_id }.files_page(file_id),
        None => format!("https://www.nexusmods.com/{domain}/mods/{mod_id}?tab=files"),
    };
    if !cfg!(test) {
        xdg_open(url);
    }
}

/// Download one Nexus file and install it for `game`; `replaces` makes it an update of that library mod.
fn nexus_fetch_and_install(state: &SharedState, game: &GameEntry, link: &nexus::NxmLink, replaces: Option<String>) {
    let client = match nexus_client(state) {
        Ok(c) => c,
        Err(e) => return log(state, "ERR", e),
    };
    log(state, "INFO", format!("Fetching Nexus mod {} (file {}) for {}...", link.mod_id, link.file_id, game.name));
    let fallback = format!("nexus_{}_{}.zip", link.mod_id, link.file_id);
    let info = client.file_info(link);
    let (name, version) = info.as_ref().map(|i| (i.name.clone(), i.version.clone())).unwrap_or_else(|| (fallback.clone(), String::new()));
    let url = match client.download_url(link) {
        Ok(u) => u,
        Err(e) => return log(state, "ERR", e),
    };
    let dir = read_state(state).cache_root().join("drops").join(fsutil::unique_token());
    let dest = dir.join(&name);
    let result = fs::create_dir_all(&dir).map_err(|e| e.to_string()).and_then(|_| client.download(&url, &dest));
    if let Err(e) = result {
        let _ = fs::remove_dir_all(&dir);
        add_note(state, "download", e.clone(), &game.appid);
        return log(state, "ERR", e);
    }
    // the file must be the one Nexus published (when it says which that is)
    let mut verified = String::new();
    if let Some(want) = info.as_ref().and_then(|i| i.md5.clone()) {
        match hashing::md5_file(&dest) {
            Ok(got) if got == want => verified = "md5".into(),
            Ok(_) => {
                let _ = fs::remove_dir_all(&dir);
                return log(state, "ERR", format!("The download of {} does not match the checksum Nexus published, so it was thrown away. Try again.", name));
            }
            Err(e) => log(state, "WARN", format!("Could not check the download: {}", e)),
        }
    }
    log(state, "INFO", format!("Downloaded {}. Installing into {}...", name, game.name));
    let opts = InstallOpts { game: Some(game.clone()), nexus: Some(link.into()), version, replaces, verified, ..Default::default() };
    install_routed(dest, true, Arc::clone(state), opts);
}

// ───────────────────────────── Nexus update checks ─────────────────────────────

/// After a rollback or a rule change the list of waiting updates for that mod is stale.
fn forget_updates_of(state: &SharedState, lib_id: &str) {
    let mut st = write_state(state);
    st.nexus.updates.retain(|u| u.lib_id != lib_id);
    st.thunder.updates.retain(|u| u.lib_id != lib_id);
    st.notifier.bump();
}

/// Install the updates of mods the user set to update by themselves, when no game runs. Returns how many.
fn auto_update(state: &SharedState) -> usize {
    if read_state(state).is_running.load(Ordering::SeqCst) {
        return 0;
    }
    let (nexus_rows, thunder_rows, premium) = {
        let st = read_state(state);
        (st.nexus.updates.clone(), st.thunder.updates.clone(), st.nexus.user.as_ref().map(|u| u.premium).unwrap_or(false))
    };
    let rule_of = |appid: &str, id: &str| -> String {
        game_by_appid(state, appid).and_then(|g| library_of(&g)).and_then(|l| l.mods().into_iter().find(|m| m.id == id)).map(|m| m.update_rule).unwrap_or_default()
    };
    let mut done = 0;
    for u in &thunder_rows {
        if rule_of(&u.appid, &u.lib_id) != "auto" {
            continue;
        }
        let Some(g) = game_by_appid(state, &u.appid) else { continue };
        match thunder_install(state, &g, &u.pkg.full_name()) {
            Ok(m) => {
                done += 1;
                log(state, "OK", format!("Updated automatically: {}", m));
            }
            Err(e) => log(state, "ERR", e),
        }
    }
    let modio_rows = read_state(state).modio_updates.clone();
    for u in &modio_rows {
        if rule_of(&u.appid, &u.lib_id) != "auto" {
            continue;
        }
        let Some(g) = game_by_appid(state, &u.appid) else { continue };
        modio_update_apply(state, &g, u);
        done += 1;
    }
    for u in &nexus_rows {
        // free accounts cannot download without the website, so those stay as updates to click
        if !premium || rule_of(&u.appid, &u.lib_id) != "auto" {
            continue;
        }
        let Some(g) = game_by_appid(state, &u.appid) else { continue };
        let link = nexus::NxmLink { game: u.nexus.game.clone(), mod_id: u.nexus.mod_id, file_id: u.new_file_id, key: None, expires: None };
        nexus_fetch_and_install(state, &g, &link, Some(u.lib_id.clone()));
        done += 1;
        log(state, "OK", format!("Updated automatically: {} to v{}.", u.name, u.new_version));
    }
    if done > 0 {
        refresh_mods_shared(state);
        let _ = check_thunder_updates(state);
        if read_state(state).nexus.key.is_some() {
            let _ = check_nexus_updates(state);
        }
    }
    done
}

/// Network and account failures end a whole check; a single odd mod (deleted, hidden) does not.
fn is_fatal_nexus_error(e: &str) -> bool {
    e.contains("rejected the API key") || e.contains("Could not reach") || e.contains("rate limit") || e.contains("Could not run curl")
}

/// The Nexus file a hand-added mod came from, found from its archive's name (mod number, upload time, version), and
/// remembered in the library. `Ok(None)` when the name says nothing or the site has no such file.
fn link_by_file_name(
    state: &SharedState,
    client: &nexus::Nexus,
    g: &GameEntry,
    lib: &deploy::Library,
    m: &deploy::LibMod,
    domain: &mut Option<Option<String>>,
    cache: &mut std::collections::HashMap<(String, u64), Result<nexus::ModFiles, String>>,
) -> Result<Option<nexus::NexusRef>, String> {
    let Some(d) = nexus::parse_download_name(&m.source) else { return Ok(None) };
    let dom = domain
        .get_or_insert_with(|| {
            let from_recipe = recipes::find(&read_state(state).recipes, &g.appid, g.recipe_steam_id()).map(|r| r.nexus.clone()).filter(|n| !n.is_empty());
            from_recipe.or_else(|| nexus_domain_for(state, g).ok())
        })
        .clone();
    let Some(dom) = dom.filter(|d| nexus::domain_ok(d)) else { return Ok(None) };
    match cache.entry((dom.clone(), d.mod_id)).or_insert_with(|| client.mod_files(&dom, d.mod_id)) {
        Ok(mf) => {
            let Some(f) = mf.find_download(&d) else { return Ok(None) };
            let r = nexus::NexusRef { game: dom, mod_id: d.mod_id, file_id: f.file_id };
            let _ = lib.set_nexus(&m.id, r.clone(), &d.name, &f.version);
            Ok(Some(r))
        }
        Err(e) if is_fatal_nexus_error(e) => Err(e.clone()),
        Err(_) => Ok(None),
    }
}

/// Ask Nexus about every library mod that came from there. Replaces the list of known updates.
fn check_nexus_updates(state: &SharedState) -> Result<usize, String> {
    let client = nexus_client(state)?;
    {
        let mut st = write_state(state);
        if st.nexus.checking {
            return Err("An update check is already running.".into());
        }
        st.nexus.checking = true;
        st.nexus.check_error = None;
        st.notifier.bump();
    }
    let games: Vec<GameEntry> = read_state(state).games.iter().filter(|g| g.is_installed).cloned().collect();
    let mut linked = 0usize;
    let outcome = (|| -> Result<(Vec<UpdateRow>, Vec<NeedRow>), String> {
        let mut cache: std::collections::HashMap<(String, u64), Result<nexus::ModFiles, String>> = Default::default();
        let mut req_cache: std::collections::HashMap<(String, u64), Result<Vec<nexus::Requirement>, String>> = Default::default();
        let mut rows = Vec::new();
        let mut needs = Vec::new();
        for g in &games {
            let Some(lib) = library_of(g) else { continue };
            let mut mods = lib.mods();
            // a mod added by hand from a file with a Nexus name is tied to its page first, so it is checked like the others
            // and counts as installed when another mod asks for it
            let mut domain: Option<Option<String>> = None;
            let mut found = false;
            for m in mods.iter().filter(|m| m.nexus.is_none() && m.thunderstore.is_none() && m.github.is_none()) {
                if link_by_file_name(state, &client, g, &lib, m, &mut domain, &mut cache)?.is_some() {
                    linked += 1;
                    found = true;
                }
            }
            if found {
                mods = lib.mods();
            }
            for m in mods.iter().cloned() {
                let Some(nx) = m.nexus.clone().filter(|n| n.valid()) else { continue };
                let never = m.update_rule == "never";
                match req_cache.entry((nx.game.clone(), nx.mod_id)).or_insert_with(|| client.requirements(&nx.game, nx.mod_id)) {
                    Ok(reqs) => {
                        let missing = missing_requirements(reqs, &mods);
                        if !missing.is_empty() {
                            needs.push(NeedRow { appid: g.appid.clone(), lib_id: m.id.clone(), name: m.name.clone(), missing });
                        }
                    }
                    Err(e) if is_fatal_nexus_error(e) => return Err(e.clone()),
                    Err(_) => {}
                }
                let files = cache.entry((nx.game.clone(), nx.mod_id)).or_insert_with(|| client.mod_files(&nx.game, nx.mod_id));
                match files {
                    Ok(mf) => {
                        if let Some(newer) = mf.newer_than(nx.file_id).filter(|_| !never) {
                            rows.push(UpdateRow {
                                appid: g.appid.clone(),
                                game: g.name.clone(),
                                lib_id: m.id.clone(),
                                name: m.name.clone(),
                                installed_version: m.version.clone(),
                                new_version: newer.version.clone(),
                                new_file_id: newer.file_id,
                                new_file_name: newer.file_name.clone(),
                                changes: readme::plain(&newer.changelog).trim().chars().take(700).collect(),
                                page_url: nx.files_page(newer.file_id),
                                nexus: nx,
                            });
                        }
                    }
                    Err(e) if is_fatal_nexus_error(e) => return Err(e.clone()),
                    Err(_) => {}
                }
            }
        }
        Ok((rows, needs))
    })();
    let mut st = write_state(state);
    st.nexus.checking = false;
    st.notifier.bump();
    match outcome {
        Ok((rows, needs)) => {
            let n = rows.len();
            st.nexus.updates = rows;
            st.nexus.needs = needs;
            st.nexus.checked_ms = now_ms();
            if linked == 1 {
                st.log("INFO", "Linked 1 mod to its Nexus Mods page by its file name, so its updates and requirements can be checked.");
            } else if linked > 1 {
                st.log("INFO", format!("Linked {} mods to their Nexus Mods pages by their file names, so their updates and requirements can be checked.", linked));
            }
            if n > 0 {
                st.log(
                    "INFO",
                    if n == 1 { "1 mod update available on Nexus Mods.".to_string() } else { format!("{} mod updates available on Nexus Mods.", n) },
                );
            }
            Ok(n)
        }
        Err(e) => {
            st.nexus.check_error = Some(e.clone());
            Err(e)
        }
    }
}

/// Install a newer file for a library mod: directly for Premium accounts, through the website
/// (and the confirmation Gantry already asks for) for free ones.
fn nexus_update(state: &SharedState, appid: &str, lib_id: &str) {
    let row = read_state(state).nexus.updates.iter().find(|u| u.appid == appid && u.lib_id == lib_id).cloned();
    let Some(row) = row else {
        return log(state, "WARN", "That update is no longer listed. Check for updates again.");
    };
    let Some(game) = read_state(state).games.iter().find(|g| g.appid == appid).cloned() else {
        return log(state, "WARN", "The game of that mod is not in the library any more.");
    };
    let client = match nexus_client(state) {
        Ok(c) => c,
        Err(e) => return log(state, "ERR", e),
    };
    let known = read_state(state).nexus.user.clone();
    let premium = match known {
        Some(u) => u.premium,
        None => match client.validate() {
            Ok(u) => {
                let p = u.premium;
                write_state(state).nexus.user = Some(u);
                p
            }
            Err(e) => return log(state, "ERR", e),
        },
    };
    let target = UpdateTarget { appid: row.appid.clone(), lib_id: row.lib_id.clone(), name: row.name.clone() };
    if premium {
        let link = nexus::NxmLink { game: row.nexus.game.clone(), mod_id: row.nexus.mod_id, file_id: row.new_file_id, key: None, expires: None };
        return nexus_fetch_and_install(state, &game, &link, Some(row.lib_id));
    }
    let wanted = nexus::NexusRef { file_id: row.new_file_id, ..row.nexus.clone() };
    {
        let mut st = write_state(state);
        st.nexus.expect = Some((target, wanted));
        st.notifier.bump();
    }
    if !cfg!(test) {
        xdg_open(&row.page_url);
    }
    log(
        state,
        "INFO",
        format!("Opened the Nexus page of '{}'. Press its Mod Manager Download button; ModForge will ask once and install it as the update.", row.name),
    );
}

/// Look for mod updates shortly after start and then every few hours (only with an API key).
/// The first few updates as short lines for the desktop message: "Cool Pack v1.0 → v2.0: fixed the crash".
fn update_summary(nexus: &[UpdateRow], thunder: &[ThunderUpdate]) -> String {
    let mut lines: Vec<String> = nexus
        .iter()
        .map(|u| {
            let first = u.changes.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or("");
            let first: String = first.trim_start_matches(['-', '*', '•', ' ']).chars().take(70).collect();
            format!("{} v{} → v{}{}", u.name, u.installed_version, u.new_version, if first.is_empty() { String::new() } else { format!(": {first}") })
        })
        .chain(thunder.iter().map(|u| format!("{} v{} → v{}", u.name, u.installed_version, u.new_version)))
        .collect();
    let more = lines.len().saturating_sub(3);
    lines.truncate(3);
    if more > 0 {
        lines.push(format!("+{more}"));
    }
    lines.join("\n")
}

/// Every waiting update in one line for the notice: "Cool Pack v1.0 → v2.0: fixed the crash; Lib v1 → v2", the game
/// named when there are several, the first eight and how many more.
fn update_digest(st: &AppState) -> String {
    let first_line = |c: &str| -> String {
        let l = c.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or("");
        l.trim_start_matches(['-', '*', '•', ' ']).chars().take(70).collect()
    };
    let mut rows: Vec<(String, String, String, String, String)> = Vec::new();
    rows.extend(st.nexus.updates.iter().map(|u| (u.game.clone(), u.name.clone(), u.installed_version.clone(), u.new_version.clone(), first_line(&u.changes))));
    rows.extend(st.thunder.updates.iter().map(|u| (u.game.clone(), u.name.clone(), u.installed_version.clone(), u.new_version.clone(), String::new())));
    rows.extend(st.github_updates.iter().map(|u| (u.game.clone(), u.name.clone(), u.installed_version.clone(), u.new_version.clone(), String::new())));
    rows.extend(st.modio_updates.iter().map(|u| (u.game.clone(), u.name.clone(), u.installed_version.clone(), u.new_version.clone(), String::new())));
    let games: HashSet<&str> = rows.iter().map(|r| r.0.as_str()).collect();
    let mut parts: Vec<String> = rows
        .iter()
        .take(8)
        .map(|(game, name, old, new, what)| {
            let mut p = [name.as_str(), " v", old, " → v", new].concat();
            if games.len() > 1 && !game.is_empty() {
                p.push_str(&format!(" ({game})"));
            }
            if !what.is_empty() {
                p.push_str(": ");
                p.push_str(what);
            }
            p
        })
        .collect();
    if rows.len() > 8 {
        parts.push(format!("+{}", rows.len() - 8));
    }
    parts.join("; ")
}

fn spawn_update_checker(state: Weak<RwLock<AppState>>) {
    let mut told = 0usize;
    spawn_background(state, "mod_updates", "Looking for mod updates", Duration::from_secs(25), Duration::from_secs(6 * 3600), Box::new(move |st, _| {
        let mut err = None;
        if read_state(st).nexus.key.is_some() {
            err = check_nexus_updates(st).err();
        }
        if let Err(e) = check_thunder_updates(st) {
            err.get_or_insert(e);
        }
        auto_update(st);
        // one quiet desktop message when there is more to update than the last time we said
        let (n, lang, summary, digest) = {
            let s = read_state(st);
            (s.nexus.updates.len() + s.thunder.updates.len() + s.github_updates.len() + s.modio_updates.len(), s.config.language.clone(), update_summary(&s.nexus.updates, &s.thunder.updates), update_digest(&s))
        };
        if n > told {
            add_note(st, "updates", format!("Mod updates: {}", digest), "");
            let tail = i18n::tr(&lang, "Open ModForge and press Update all.");
            platform::notify(&i18n::tr(&lang, "Mod updates available"), &if summary.is_empty() { tail } else { format!("{summary}\n{tail}") });
        }
        told = n;
        err.map_or(Ok(true), Err)
    }));
}

// ───────────────────────────── Thunderstore ─────────────────────────────

/// Did the Nexus part of a combined check fail (so "all up to date" would be a lie)?
fn has_key_error(state: &SharedState) -> bool {
    let st = read_state(state);
    st.nexus.check_error.is_some()
}

fn thunder_client(state: &SharedState) -> thunderstore::Client {
    let st = read_state(state);
    thunderstore::Client::new(&st.thunder.base, st.thunder.allow_http)
}

/// "Cool_Mod" → "Cool Mod"
fn pretty_package_name(name: &str) -> String {
    name.replace('_', " ")
}

/// Install a Thunderstore package into the game's Mod Library together with whatever it needs.
/// Packages already in the library are not fetched again, except the one asked for, which is
/// replaced in place (so this is also how a package is updated).
fn thunder_install(state: &SharedState, game: &GameEntry, input: &str) -> Result<String, String> {
    let root = thunderstore::parse_ref(input)?;
    let lib = library_of(game).ok_or_else(|| format!("{} is not installed.", game.name))?;
    let installed: std::collections::HashMap<String, String> = lib
        .mods()
        .into_iter()
        .filter_map(|m| m.thunderstore.map(|t| (t.full_name(), m.id)))
        .collect();
    let skip: std::collections::HashSet<String> = installed.keys().cloned().collect();
    let client = thunder_client(state);
    let plan = client.plan(&root, &skip)?;
    let drops = read_state(state).cache_root().join("drops");
    let mut done = 0;
    for r in &plan {
        log(state, "INFO", format!("Fetching Thunderstore package {} {}...", r.pkg.full_name(), r.pkg.version));
        let dir = drops.join(fsutil::unique_token());
        let dest = dir.join(format!("{}-{}.zip", r.pkg.full_name(), r.pkg.version));
        let fetched = fs::create_dir_all(&dir).map_err(|e| e.to_string()).and_then(|_| client.download(&r.download_url, &dest));
        if let Err(e) = fetched {
            let _ = fs::remove_dir_all(&dir);
            return Err(e);
        }
        let opts = InstallOpts {
            game: Some(game.clone()),
            to_library: true,
            version: r.pkg.version.clone(),
            name: Some(pretty_package_name(&r.pkg.name)),
            thunder: Some(r.pkg.clone()),
            depends: r.dependencies.iter().filter_map(|d| d.rsplit_once('-').map(|(n, _)| n.to_string())).collect(),
            replaces: installed.get(&r.pkg.full_name()).cloned(),
            ..Default::default()
        };
        install_routed(dest, true, Arc::clone(state), opts);
        done += 1;
    }
    refresh_mods_shared(state);
    Ok(if done == 1 { "Thunderstore: 1 package installed.".to_string() } else { format!("Thunderstore: {} packages installed.", done) })
}

/// Install every known update that needs no clicks on a website: all Thunderstore ones, and the
/// Nexus ones for a Premium account. Free Nexus accounts are told which ones are left.
fn update_all(state: &SharedState) {
    if read_state(state).is_running.load(Ordering::SeqCst) {
        return log(state, "WARN", "Close the game first.");
    }
    let (thunder, nexus_rows, premium) = {
        let st = read_state(state);
        (st.thunder.updates.clone(), st.nexus.updates.clone(), st.nexus.user.as_ref().map(|u| u.premium).unwrap_or(false))
    };
    let github_rows = read_state(state).github_updates.clone();
    let modio_rows = read_state(state).modio_updates.clone();
    if thunder.is_empty() && nexus_rows.is_empty() && github_rows.is_empty() && modio_rows.is_empty() {
        return log(state, "OK", "All mods are up to date.");
    }
    let game_of = |appid: &str| read_state(state).games.iter().find(|g| g.appid == appid).cloned();
    let mut done = 0;
    for u in &thunder {
        let Some(g) = game_of(&u.appid) else { continue };
        match thunder_install(state, &g, &u.pkg.full_name()) {
            Ok(m) => {
                done += 1;
                log(state, "OK", m);
            }
            Err(e) => log(state, "ERR", e),
        }
    }
    for u in github_rows.iter().filter(|u| !u.url.is_empty()) {
        let Some(g) = game_of(&u.appid) else { continue };
        github_update_apply(state, &g, u);
        done += 1;
    }
    for u in &modio_rows {
        let Some(g) = game_of(&u.appid) else { continue };
        modio_update_apply(state, &g, u);
        done += 1;
    }
    let mut waiting = 0;
    for u in &nexus_rows {
        if premium {
            let Some(g) = game_of(&u.appid) else { continue };
            let link = nexus::NxmLink { game: u.nexus.game.clone(), mod_id: u.nexus.mod_id, file_id: u.new_file_id, key: None, expires: None };
            nexus_fetch_and_install(state, &g, &link, Some(u.lib_id.clone()));
            done += 1;
        } else {
            waiting += 1;
        }
    }
    if waiting > 0 {
        log(state, "INFO", format!("{} Nexus update(s) need a click on the website (free account): use the update button of each mod.", waiting));
    }
    log(state, "OK", format!("{} mod(s) updated.", done));
    refresh_mods_shared(state);
}

/// Ask Thunderstore about every library mod that came from there. Replaces the list of known updates.
/// Look at the repositories mods were installed from and list the ones with a newer release.
fn check_github_updates(state: &SharedState) -> usize {
    let (games, api, allow) = {
        let st = read_state(state);
        (st.games.iter().filter(|g| g.is_installed).cloned().collect::<Vec<_>>(), st.github_api.clone(), st.github_allow.clone())
    };
    let mut latest: std::collections::HashMap<String, Option<github::Release>> = Default::default();
    let mut rows: Vec<GithubUpdate> = Vec::new();
    for g in &games {
        let Some(lib) = library_of(g) else { continue };
        for m in lib.mods() {
            let Some(r) = m.github.clone().filter(|_| m.update_rule != "never") else { continue };
            let rel = latest.entry(r.repo_name().to_lowercase()).or_insert_with(|| github::latest(&r.owner, &r.repo, &api, allow.as_deref()).ok());
            let Some(rel) = rel else { continue };
            if !thunderstore::version_newer(rel.tag.trim_start_matches('v'), r.tag.trim_start_matches('v')) {
                continue;
            }
            let asset = github::matching_asset(&r.asset, &rel.assets);
            rows.push(GithubUpdate {
                appid: g.appid.clone(),
                game: g.name.clone(),
                lib_id: m.id.clone(),
                name: m.name.clone(),
                installed_version: r.tag.trim_start_matches('v').to_string(),
                new_version: rel.tag.trim_start_matches('v').to_string(),
                kind: "github",
                repo: r.repo_name(),
                url: asset.as_ref().map(|a| a.url.clone()).unwrap_or_default(),
                new_ref: asset.map(|a| github::GithubRef { owner: r.owner.clone(), repo: r.repo.clone(), tag: rel.tag.clone(), asset: a.name }),
            });
        }
    }
    let n = rows.len();
    let mut st = write_state(state);
    st.github_updates = rows;
    st.notifier.bump();
    if n > 0 {
        st.log("INFO", if n == 1 { "1 GitHub update is available.".to_string() } else { format!("{} GitHub updates are available.", n) });
    }
    n
}

/// Install the newer release of one mod from GitHub.
fn github_update_apply(state: &SharedState, g: &GameEntry, row: &GithubUpdate) {
    if row.url.is_empty() {
        return log(state, "WARN", format!("The new release of {} has several files and ModForge cannot tell which one replaces yours: pick it in the GitHub tab.", row.name));
    }
    github_install_for(state, g, &row.repo, &row.url, row.new_ref.clone());
    refresh_mods_shared(state);
}

fn check_thunder_updates(state: &SharedState) -> Result<usize, String> {
    let games: Vec<GameEntry> = read_state(state).games.iter().filter(|g| g.is_installed).cloned().collect();
    let mut rows: Vec<ThunderUpdate> = Vec::new();
    let mut latest: std::collections::HashMap<String, Result<String, String>> = Default::default();
    let client = thunder_client(state);
    for g in &games {
        let Some(lib) = library_of(g) else { continue };
        for m in lib.mods() {
            let Some(t) = m.thunderstore.clone().filter(|_| m.update_rule != "never") else { continue };
            let ver = latest.entry(t.full_name()).or_insert_with(|| {
                client.resolve(&thunderstore::PkgRef { namespace: t.namespace.clone(), name: t.name.clone(), version: None }).map(|r| r.pkg.version)
            });
            match ver {
                Ok(v) if thunderstore::version_newer(v, &t.version) => rows.push(ThunderUpdate {
                    appid: g.appid.clone(),
                    game: g.name.clone(),
                    lib_id: m.id.clone(),
                    name: m.name.clone(),
                    installed_version: t.version.clone(),
                    new_version: v.clone(),
                    pkg: t,
                }),
                Ok(_) => {}
                Err(e) if is_fatal_nexus_error(e) => return Err(e.clone()),
                Err(_) => {}
            }
        }
    }
    let n = rows.len();
    let mut st = write_state(state);
    st.thunder.updates = rows;
    st.notifier.bump();
    if n > 0 {
        st.log("INFO", if n == 1 { "1 Thunderstore update is available.".to_string() } else { format!("{} Thunderstore updates are available.", n) });
    }
    drop(st);
    let github_n = check_github_updates(state);
    let modio_n = check_modio_updates(state);
    Ok(n + github_n + modio_n)
}

/// Hand an `nxm://` link to the Gantry that is already running (it owns the window and the key).
fn forward_nxm(port: u16, url: &str) -> bool {
    use std::io::Write;
    use std::net::{SocketAddr, TcpStream};
    let addr: SocketAddr = ([127, 0, 0, 1], port).into();
    let send = |req: String| -> Option<(u16, String)> {
        let mut s = TcpStream::connect_timeout(&addr, Duration::from_millis(800)).ok()?;
        s.set_read_timeout(Some(Duration::from_secs(5))).ok()?;
        s.write_all(req.as_bytes()).ok()?;
        let mut raw = Vec::new();
        let _ = s.read_to_end(&mut raw);
        http_reply(&raw)
    };
    let host = format!("127.0.0.1:{}", port);
    let Some((_, page)) = send(format!("GET / HTTP/1.1\r\nHost: {}\r\nConnection: close\r\n\r\n", host)) else { return false };
    let Some(token) = page.split("RIG_TOKEN = \"").nth(1).and_then(|r| r.split('"').next()).map(str::to_string) else { return false };
    let body = json!({ "url": url }).to_string();
    send(format!(
        "POST /api/nxm HTTP/1.1\r\nHost: {}\r\nX-Rig-Token: {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        host,
        token,
        body.len(),
        body
    ))
    .is_some_and(|(code, _)| code == 200)
}

// ───────────────────────────── Undo ─────────────────────────────

/// Ids of the user mods enabled right now (for the selected game).
fn snapshot_enabled(state: &SharedState) -> Vec<String> {
    read_state(state).mods.iter().filter(|m| !m.is_system && m.enabled).map(|m| m.id.clone()).collect()
}

fn push_undo(state: &SharedState, appid: &str, label: String, enabled_before: Vec<String>, trashed: Vec<TrashedItem>) {
    let evicted = {
        let mut st = write_state(state);
        st.undo.push(UndoEntry { appid: appid.to_string(), label, enabled_before, trashed });
        let extra = st.undo.len().saturating_sub(UNDO_DEPTH);
        let old: Vec<UndoEntry> = st.undo.drain(..extra).collect();
        st.notifier.bump();
        old
    };
    // changes that fell off the stack can no longer be undone, so their trash is released
    for e in evicted {
        for it in e.trashed {
            if let Some(bundle) = it.stored.parent() {
                let _ = fs::remove_dir_all(bundle);
            }
        }
    }
}

/// Revert the newest change made to the selected game.
fn undo_last(state: &SharedState) -> Result<String, String> {
    let g = selected_game(state).ok_or("No game selected.")?;
    let entry = {
        let mut st = write_state(state);
        let idx = st.undo.iter().rposition(|u| u.appid == g.appid).ok_or("Nothing to undo for this game.")?;
        st.undo.remove(idx)
    };
    let (restored, conflicts) = ModManager::restore_trashed(&entry.trashed);
    apply_mod_set(state, &entry.appid, &entry.enabled_before)?;
    refresh_mods_shared(state);
    write_state(state).notifier.bump();
    if conflicts.is_empty() {
        Ok(format!("Undid: {}.", entry.label))
    } else {
        Ok(format!(
            "Undid: {}, but {} file(s) could not be put back because something is already there ({} restored). They stay in the trash folder.",
            entry.label,
            conflicts.len(),
            restored
        ))
    }
}

fn bisect_file(st: &AppState, appid: &str) -> PathBuf {
    let safe: String = appid.chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '_' }).collect();
    st.cache_root().join("bisect").join(format!("{}.json", safe))
}

/// Write the open session to disk so a restart cannot leave the user's mods switched off.
fn persist_bisect(state: &SharedState) {
    let st = read_state(state);
    if let Some(b) = &st.bisect {
        if let Ok(json) = serde_json::to_string_pretty(b) {
            let _ = atomic_write(&bisect_file(&st, &b.appid), json);
        }
    }
}

/// Resume a session that was open when rig last exited.
fn resume_bisect(state: &SharedState) {
    let dir = read_state(state).cache_root().join("bisect");
    let Ok(rd) = fs::read_dir(&dir) else { return };
    for e in rd.flatten() {
        let Some(b) = fs::read_to_string(e.path()).ok().and_then(|t| serde_json::from_str::<Bisect>(&t).ok()) else {
            continue;
        };
        let known = read_state(state).games.iter().any(|g| g.appid == b.appid);
        if !known {
            continue;
        }
        let name = read_state(state).games.iter().find(|g| g.appid == b.appid).map(|g| g.name.clone()).unwrap_or_default();
        write_state(state).bisect = Some(b);
        log(state, "WARN", format!("Resumed an unfinished Crash Detective session for {}. Some of its mods may be switched off.", name));
        return;
    }
}

fn game_by_appid(state: &SharedState, appid: &str) -> Option<GameEntry> {
    read_state(state).games.iter().find(|g| g.appid == appid).cloned()
}

/// Make exactly `enabled` the set of user mods switched on for `appid`'s game.
fn apply_mod_set(state: &SharedState, appid: &str, enabled: &[String]) -> Result<usize, String> {
    let g = game_by_appid(state, appid).ok_or("The game of this session is no longer in your library.")?;
    if read_state(state).bisect.as_ref().map(|b| b.library).unwrap_or(false) {
        let lib = library_of(&g).ok_or("The mod library of this game is not available.")?;
        let changed = lib.set_enabled_set(enabled)?;
        if let Err(e) = library_apply(state, &lib, false) {
            log(state, "ERR", e);
        }
        refresh_mods_shared(state);
        return Ok(changed);
    }
    let changed = ModManager::apply_profile(&g, enabled);
    if read_state(state).games.get(read_state(state).selected_idx).map(|s| s.appid == appid).unwrap_or(false) {
        refresh_mods_shared(state);
    }
    Ok(changed)
}

fn describe_round(state: &SharedState) -> String {
    let st = read_state(state);
    let Some(b) = &st.bisect else { return String::new() };
    let names: Vec<String> = b.testing.iter().map(|i| st.mod_display_name(i)).collect();
    match b.phase {
        Phase::Baseline => format!("Round {}: all your mods are OFF. Launch the game and see if it still crashes.", b.round),
        Phase::Narrow => format!("Round {}: only {} mod(s) are ON ({}). Launch and test.", b.round, names.len(), names.join(", ")),
        Phase::Verify => format!("Round {}: everything except the suspect is ON. Launch and test.", b.round),
        Phase::Done => format!("Found it: {}.", b.found.iter().map(|i| st.mod_display_name(i)).collect::<Vec<_>>().join(", ")),
        Phase::GaveUp => b.note.clone(),
    }
}

fn bisect_start(state: &SharedState) -> Result<String, String> {
    let g = selected_game(state).ok_or("No game selected.")?;
    if procs::is_game_running(&g) {
        return Err(format!("{} is running. Close it first.", g.name));
    }
    if let Some(b) = &read_state(state).bisect {
        return Err(if b.appid == g.appid {
            "A Crash Detective session is already open for this game.".to_string()
        } else {
            "Finish or cancel the Crash Detective session of another game first.".to_string()
        });
    }
    let mods = ModManager::list_mods(&g);
    let mut originals: Vec<String> = mods.iter().filter(|m| !m.is_system && m.enabled).map(|m| m.id.clone()).collect();
    // no UE4SS/PAK mods on: the suspects are the mods of the game's library
    let mut library_names: std::collections::BTreeMap<String, String> = Default::default();
    if originals.is_empty() {
        if let Some(lib) = library_of(&g) {
            for m in lib.mods().into_iter().filter(|m| m.enabled) {
                originals.push(m.id.clone());
                library_names.insert(m.id, m.name);
            }
        }
    }
    let likely: Vec<String> = read_state(state)
        .crash
        .as_ref()
        .filter(|c| c.appid == g.appid)
        .map(|c| c.suspects.iter().map(|s| s.id.clone()).collect())
        .unwrap_or_default();
    let mut b = Bisect::new(&g.appid, originals, likely, now_ms()).ok_or("None of your user mods are enabled, so there is nothing to test.")?;
    b.library = !library_names.is_empty();
    b.names = library_names;
    let testing = b.testing.clone();
    write_state(state).bisect = Some(b);
    persist_bisect(state);
    apply_mod_set(state, &g.appid, &testing)?;
    Ok(describe_round(state))
}

fn bisect_answer(state: &SharedState, crashed: bool) -> Result<String, String> {
    let (appid, finished) = {
        let st = read_state(state);
        let b = st.bisect.as_ref().ok_or("No Crash Detective session is open.")?;
        (b.appid.clone(), b.is_finished())
    };
    if finished {
        return Err("This session is finished. Choose what to keep.".into());
    }
    if let Some(g) = game_by_appid(state, &appid) {
        if procs::is_game_running(&g) {
            return Err(format!("{} is still running. Close it, then answer.", g.name));
        }
    }
    let testing = {
        let mut st = write_state(state);
        let b = st.bisect.as_mut().ok_or("No Crash Detective session is open.")?;
        b.answer(crashed, now_ms());
        b.testing.clone()
    };
    persist_bisect(state);
    let finished = read_state(state).bisect.as_ref().map(|b| b.is_finished()).unwrap_or(true);
    if !finished {
        apply_mod_set(state, &appid, &testing)?;
    }
    write_state(state).notifier.bump();
    Ok(describe_round(state))
}

/// End the session: keep everything as before (`restore_all`) or leave the culprit off.
fn bisect_finish(state: &SharedState, restore_all: bool) -> Result<String, String> {
    let (appid, enabled, found) = {
        let st = read_state(state);
        let b = st.bisect.as_ref().ok_or("No Crash Detective session is open.")?;
        (b.appid.clone(), b.final_enabled(restore_all), b.found.clone())
    };
    apply_mod_set(state, &appid, &enabled)?;
    let (file, names) = {
        let mut st = write_state(state);
        let names: Vec<String> = found.iter().map(|i| st.mod_display_name(i)).collect();
        // what it found is remembered, so a later crash with that mod on again points at it
        if !names.is_empty() {
            let known = st.config.culprits.entry(appid.clone()).or_default();
            for n in &names {
                if !known.contains(n) {
                    known.push(n.clone());
                }
            }
            known.truncate(20);
            st.save_config();
        }
        let file = bisect_file(&st, &appid);
        st.bisect = None;
        (file, names)
    };
    let _ = fs::remove_file(file);
    write_state(state).notifier.bump();
    Ok(if restore_all || names.is_empty() {
        "Crash Detective closed. Your mods are back as they were.".to_string()
    } else {
        format!("Crash Detective closed. {} stays disabled; everything else is back on.", names.join(", "))
    })
}

fn analyze_last_run(state: &SharedState) {
    let Some(g) = selected_game(state) else { return };
    let mods = ModManager::list_mods(&g);
    let mut report = crash::analyze_run(&g, &mods, 0, now_ms());
    if report.crashed {
        report.streak = read_state(state).config.crash_streak.get(&g.appid).copied().unwrap_or(0);
        report.tries = crash_tries(state, &g, &report);
    }
    let headline = report.headline.clone();
    write_state(state).crash = Some(report);
    log(state, "INFO", headline);
}

/// The places a game's support entry says its saves are in (Save Guard and per-profile saves use them too).
fn save_places(state: &SharedState, g: &GameEntry) -> Vec<String> {
    recipes::find(&read_state(state).recipes, &g.appid, g.recipe_steam_id()).map(|r| r.saves.clone()).unwrap_or_default()
}

/// Save Guard: snapshot the saves before a launch (silent when nothing changed or there are none).
fn snapshot_before_launch(state: &SharedState, g: &GameEntry) {
    let cache_root = read_state(state).cache_root();
    if let Ok(Some(s)) = saves::snapshot(g, &cache_root, "auto", &save_places(state, g)) {
        log(state, "INFO", format!("Saves backed up before launch ({} file(s)).", s.files));
    }
}

/// Saves as the session left them, so a bad session can be taken back.
fn snapshot_after_session(state: &SharedState, g: &GameEntry) {
    let cache_root = read_state(state).cache_root();
    if let Ok(Some(s)) = saves::snapshot(g, &cache_root, "session", &save_places(state, g)) {
        log(state, "INFO", format!("Saves backed up after the session ({} file(s)).", s.files));
    }
}

/// A profile was loaded: its launch options, environment, compatibility tool and trainer come back too. A tool or
/// trainer that is no longer on this computer is left as it is.
fn apply_profile_launch(state: &SharedState, g: &GameEntry, l: &deploy::ProfileLaunch) {
    let mut st = write_state(state);
    if let Some(game) = st.games.iter_mut().find(|x| x.appid == g.appid) {
        game.launch_args = l.launch_args.clone();
        game.env_vars = l.env_vars.clone();
    }
    st.config.launch_options.insert(g.appid.clone(), LaunchOpts { launch_args: l.launch_args.clone(), env_vars: l.env_vars.clone() });
    let mut missing = Vec::new();
    if g.launches_directly() && !l.tool.is_empty() {
        if Path::new(&l.tool).is_file() {
            st.config.proton_choice.insert(g.appid.clone(), l.tool.clone());
        } else {
            missing.push("compatibility tool");
        }
    }
    if !l.trainer.is_empty() {
        if Path::new(&l.trainer).is_file() {
            st.config.trainers.insert(g.appid.clone(), l.trainer.clone());
        } else {
            missing.push("trainer");
        }
    }
    st.save_config();
    st.save_custom_games();
    st.notifier.bump();
    drop(st);
    if missing.is_empty() {
        log(state, "OK", "The profile's launch options came back too.");
    } else {
        log(state, "WARN", "The profile's launch options came back; its compatibility tool or trainer is no longer on this computer, so the current one stays.");
    }
}

/// Save Guard before something that can change or hide the saves: a mod update, a rollback, a profile
/// switch, another compatibility tool. Silent when nothing changed or there are no saves.
fn guard_saves(state: &SharedState, g: &GameEntry, label: &str) {
    let cache_root = read_state(state).cache_root();
    match saves::snapshot(g, &cache_root, label, &save_places(state, g)) {
        Ok(Some(s)) => log(state, "INFO", format!("Saves backed up first, just in case ({} file(s)).", s.files)),
        Ok(None) => {}
        Err(e) if saves::nothing_to_guard(&e) => {}
        Err(e) => log(state, "WARN", format!("Could not back up the saves first: {}", e)),
    }
}

/// The save files as they are now, each with the session it was written in (so the list can say which
/// profile and mods made it). A file written outside any recorded session has no `made`.
fn saves_made_with(g: &GameEntry, files: Vec<saves::SaveFile>, all: &[stats::Session]) -> Value {
    const LATE_WRITE_MS: u64 = 120_000;
    let mine: Vec<&stats::Session> = all.iter().filter(|s| s.appid == g.appid).collect();
    let mut used: Vec<usize> = Vec::new();
    let rows: Vec<Value> = files
        .into_iter()
        .map(|f| {
            let at = mine.iter().rposition(|s| s.start_ms <= f.modified_ms && f.modified_ms <= s.end_ms + LATE_WRITE_MS);
            let made = at.map(|i| used.iter().position(|u| *u == i).unwrap_or_else(|| {
                used.push(i);
                used.len() - 1
            }));
            let mut v = serde_json::to_value(&f).unwrap_or(Value::Null);
            if let (Some(m), Some(o)) = (made, v.as_object_mut()) {
                o.insert("made".into(), json!(m));
            }
            v
        })
        .collect();
    let sessions: Vec<Value> = used.iter().map(|i| {
        let s = mine[*i];
        json!({ "start_ms": s.start_ms, "end_ms": s.end_ms, "mods": s.mods, "profile": s.profile, "names": s.mod_names, "crashed": s.crashed })
    }).collect();
    json!({ "files": rows, "sessions": sessions })
}

/// App id handed to Proton. Proton applies per-game fixes keyed on it, so an unrelated game must
/// not borrow another game's id: use its own Steam id, Half Sword's for Half Sword, else 0.
fn compat_app_id(g: &GameEntry) -> String {
    if !g.steam_appid.is_empty() {
        g.steam_appid.clone()
    } else if g.name.to_lowercase().contains("half sword") {
        "2397300".into()
    } else {
        "0".into()
    }
}

/// Before a game starts: make sure its mods are really in place (a game update or a launcher can
/// undo them), and say what is still missing.
fn prelaunch_check(state: &SharedState, g: &GameEntry) {
    let Some(lib) = library_of(g) else { return };
    if lib.mods().is_empty() {
        return;
    }
    let drift = lib.drift();
    if lib.is_dirty() || !drift.is_empty() {
        if !drift.is_empty() {
            log(state, "INFO", format!("{} deployed file(s) changed since the last deploy (a game update?). Putting your mods back before launch...", drift.len()));
            add_note(state, "gameupdate", format!("{} deployed file(s) changed since the last deploy (a game update?). Putting your mods back before launch...", drift.len()), &g.appid);
        }
        match library_apply(state, &lib, false) {
            Ok(msg) => log(state, "OK", msg),
            Err(e) => log(state, "ERR", e),
        }
    }
    let needs: Vec<NeedRow> = read_state(state).nexus.needs.iter().filter(|n| n.appid == g.appid).cloned().collect();
    for n in needs {
        let names = n.missing.iter().map(|r| r.name.clone()).collect::<Vec<_>>().join(", ");
        log(state, "WARN", format!("{} needs {} (not in your library yet).", n.name, names));
    }
}

fn play_game(state: &SharedState) {
    play_game_inner(state, false);
}

/// Start the selected game with its mods taken out; they come back when the game closes.
fn play_without_mods(state: &SharedState) {
    let Some(g) = selected_game(state).filter(|g| g.is_installed) else {
        return log(state, "WARN", "Select an installed game first.");
    };
    if read_state(state).is_running.load(Ordering::SeqCst) {
        return log(state, "WARN", "A game is already running.");
    }
    if let Err(e) = begin_vanilla(state, &g) {
        return log(state, "ERR", e);
    }
    play_game_inner(state, true);
    let st = Arc::clone(state);
    thread::spawn(move || watch_vanilla_session(&st, &g));
}

/// Remember that the mods must come back, then take them out. (In this order: if Gantry stops in
/// between, the next start still knows to put them back.)
fn begin_vanilla(state: &SharedState, g: &GameEntry) -> Result<(), String> {
    {
        let mut st = write_state(state);
        if !st.config.vanilla_sessions.contains(&g.appid) {
            st.config.vanilla_sessions.push(g.appid.clone());
        }
        st.save_config();
        st.notifier.bump();
    }
    if let Some(lib) = library_of(g) {
        switch_saves(state, g, &lib, VANILLA_SAVES, true);
        if !lib.mods().is_empty() {
            match lib.purge() {
                Ok(r) => log(state, "INFO", format!("Playing {} without mods ({} file(s) taken out for now).", g.name, r.removed)),
                Err(e) => {
                    let mut st = write_state(state);
                    st.config.vanilla_sessions.retain(|a| a != &g.appid);
                    st.save_config();
                    return Err(e);
                }
            }
        }
    }
    Ok(())
}

const VANILLA_SAVES: &str = "(without mods)";

/// When a library keeps separate saves per profile: put the current saves away and bring back those of
/// `to` (a profile that has none yet starts from what is there). Does nothing while the game runs.
fn switch_saves(state: &SharedState, g: &GameEntry, lib: &deploy::Library, to: &str, remember_return: bool) {
    if !lib.separate_saves() {
        return;
    }
    if read_state(state).is_running.load(Ordering::SeqCst) {
        return log(state, "WARN", "The saves were not switched because the game is running.");
    }
    let roots = profsaves::roots(g, &save_places(state, g));
    if roots.is_empty() {
        return;
    }
    let from = Some(lib.saves_of()).filter(|s| !s.is_empty()).unwrap_or_else(|| "(before profiles)".to_string());
    if from == to {
        return;
    }
    guard_saves(state, g, "before-profile");
    let store = lib.root().join("profile_saves");
    if let Err(e) = profsaves::put_away(&store, &from, &roots) {
        return log(state, "ERR", e);
    }
    let backup = store.join("_replaced").join(now_ms().to_string());
    match profsaves::bring_back(&store, &backup, to, &roots) {
        Ok(0) => log(state, "INFO", format!("'{}' has no saves of its own yet: it starts from the ones you have now.", to)),
        Ok(n) => log(state, "OK", format!("The saves of '{}' are back ({} folder(s)). What was there is kept in {}.", to, n, backup.display())),
        Err(e) => return log(state, "ERR", e),
    }
    if let Err(e) = lib.set_saves(to, remember_return.then_some(from.as_str())) {
        log(state, "ERR", e);
    }
}

/// The game is done (or never started): put the mods back.
fn end_vanilla(state: &SharedState, appid: &str) {
    {
        let mut st = write_state(state);
        if !st.config.vanilla_sessions.iter().any(|a| a == appid) {
            return;
        }
        st.config.vanilla_sessions.retain(|a| a != appid);
        st.save_config();
        st.notifier.bump();
    }
    let Some(g) = game_by_appid(state, appid) else { return };
    let Some(lib) = library_of(&g) else { return };
    if lib.mods().is_empty() {
        return;
    }
    let back = lib.saves_return();
    if !back.is_empty() && lib.saves_of() == VANILLA_SAVES {
        switch_saves(state, &g, &lib, &back, false);
    }
    match library_apply(state, &lib, false) {
        Ok(msg) => log(state, "OK", format!("{} is closed; your mods are back. {}", g.name, msg)),
        Err(e) => log(state, "ERR", e),
    }
    refresh_mods_shared(state);
}

/// Wait for the game to start and then to end, and restore the mods.
fn watch_vanilla_session(state: &SharedState, g: &GameEntry) {
    let started_by = Instant::now();
    let mut seen = false;
    loop {
        thread::sleep(if cfg!(test) { Duration::from_millis(50) } else { Duration::from_secs(2) });
        if !read_state(state).config.vanilla_sessions.contains(&g.appid) {
            return; // ended some other way
        }
        let running = procs::is_game_running(g);
        if running {
            seen = true;
        } else if seen || started_by.elapsed() > Duration::from_secs(if cfg!(test) { 4 } else { 240 }) {
            break;
        }
    }
    end_vanilla(state, &g.appid);
}

/// Start the selected game with only core loaders and framework mods enabled; content mods are restored when the game exits.
fn play_in_safe_mode(state: &SharedState) {
    let Some(g) = selected_game(state).filter(|g| g.is_installed) else {
        return log(state, "WARN", "Select an installed game first.");
    };
    if read_state(state).is_running.load(Ordering::SeqCst) {
        return log(state, "WARN", "A game is already running.");
    }
    if let Some(lib) = library_of(&g) {
        match lib.stage_safe_mode() {
            Ok(count) => {
                {
                    let mut st = write_state(state);
                    if !st.config.safe_mode_sessions.contains(&g.appid) {
                        st.config.safe_mode_sessions.push(g.appid.clone());
                    }
                    st.save_config();
                    st.notifier.bump();
                }
                log(state, "INFO", format!("Safe Mode: {} content mod(s) temporarily disabled; core loaders remain active.", count));
                let _ = library_apply(state, &lib, false);
                refresh_mods_shared(state);
            }
            Err(e) => return log(state, "ERR", e),
        }
    }
    play_game_inner(state, false);
    let st = Arc::clone(state);
    thread::spawn(move || watch_safe_mode_session(&st, &g));
}

fn watch_safe_mode_session(state: &SharedState, g: &GameEntry) {
    let started_by = Instant::now();
    let mut seen = false;
    loop {
        thread::sleep(if cfg!(test) { Duration::from_millis(50) } else { Duration::from_secs(2) });
        if !read_state(state).config.safe_mode_sessions.contains(&g.appid) {
            return;
        }
        let running = procs::is_game_running(g);
        if running {
            seen = true;
        } else if seen || started_by.elapsed() > Duration::from_secs(if cfg!(test) { 4 } else { 240 }) {
            break;
        }
    }
    end_safe_mode(state, &g.appid);
}

fn end_safe_mode(state: &SharedState, appid: &str) {
    {
        let mut st = write_state(state);
        if !st.config.safe_mode_sessions.iter().any(|a| a == appid) {
            return;
        }
        st.config.safe_mode_sessions.retain(|a| a != appid);
        st.save_config();
        st.notifier.bump();
    }
    let Some(g) = game_by_appid(state, appid) else { return };
    if let Some(lib) = library_of(&g) {
        let _ = lib.restore_safe_mode();
        let _ = library_apply(state, &lib, false);
        log(state, "OK", format!("{} is closed; all mods restored from Safe Mode.", g.name));
        refresh_mods_shared(state);
    }
}

/// After a restart: a safe mode session that was left open ends when its game is not running.
fn resume_safe_mode_sessions(state: &SharedState) {
    let ids = read_state(state).config.safe_mode_sessions.clone();
    for id in ids {
        let Some(g) = game_by_appid(state, &id) else {
            let mut st = write_state(state);
            st.config.safe_mode_sessions.retain(|a| a != &id);
            st.save_config();
            continue;
        };
        if procs::is_game_running(&g) {
            let st = Arc::clone(state);
            thread::spawn(move || watch_safe_mode_session(&st, &g));
        } else {
            log(state, "INFO", format!("{} was left in Safe Mode; restoring all mods.", g.name));
            end_safe_mode(state, &id);
        }
    }
}

/// After a restart: a vanilla session that was left open ends when its game is not running.
fn resume_vanilla_sessions(state: &SharedState) {
    let ids = read_state(state).config.vanilla_sessions.clone();
    for id in ids {
        let Some(g) = game_by_appid(state, &id) else {
            let mut st = write_state(state);
            st.config.vanilla_sessions.retain(|a| a != &id);
            st.save_config();
            continue;
        };
        if procs::is_game_running(&g) {
            let st = Arc::clone(state);
            thread::spawn(move || watch_vanilla_session(&st, &g));
        } else {
            log(state, "INFO", format!("{} was left without its mods; putting them back.", g.name));
            end_vanilla(state, &id);
        }
    }
}

/// Switch to a saved profile of the selected game's library, then start it.
fn play_with_profile(state: &SharedState, name: &str) {
    let Some(g) = selected_game(state).filter(|g| g.is_installed) else {
        return log(state, "WARN", "Select an installed game first.");
    };
    if read_state(state).is_running.load(Ordering::SeqCst) {
        return log(state, "WARN", "A game is already running.");
    }
    let Some(lib) = library_of(&g) else { return };
    match lib.load_profile(name) {
        Ok(n) => log(state, "OK", format!("Library profile '{}' loaded ({} mods changed).", name, n)),
        Err(e) => return log(state, "ERR", e),
    }
    if let Err(e) = library_apply(state, &lib, false) {
        return log(state, "ERR", e);
    }
    refresh_mods_shared(state);
    play_game_inner(state, false);
}

fn play_game_inner(state: &SharedState, skip_prelaunch: bool) {
    let (game_opt, proton_bin, gamemode, mangohud, auto_tr, trainer_opt, running_flag, running_pid) = {
        let st = read_state(state);
        let g = st.games.get(st.selected_idx).cloned();
        let pb = g.as_ref().and_then(|g| st.proton_for(g));
        let tr = g.as_ref().and_then(|gm| st.config.trainers.get(&gm.appid).cloned());
        (
            g,
            pb,
            st.gamemode,
            st.mangohud,
            st.auto_trainer,
            tr,
            Arc::clone(&st.is_running),
            Arc::clone(&st.running_pid),
        )
    };
    let Some(g) = game_opt else { return };
    if !g.is_installed {
        log(state, "WARN", format!("{} is not installed; install it from its launcher first.", g.name));
        return;
    }
    if steam_update_of(&g).phase == steamlib::Phase::Updating {
        // starting now would run half-updated files, and putting mods in would fight Steam's own writes
        log(state, "WARN", format!("Steam is updating {}. Wait until it is done, then press Play.", g.name));
        return;
    }
    let gdir = PathBuf::from(&g.game_dir);
    let ue_bin = RigEngine::find_ue_bin_dir(&gdir);
    if !skip_prelaunch && !running_flag.load(Ordering::SeqCst) {
        prelaunch_check(state, &g);
    }

    if ue_bin.as_ref().map(|u| u.join("dwmapi.dll").is_file()).unwrap_or(false) {
        for a in RepairEngine::repair(&g) {
            log(state, "UE5", a);
        }
    }

    let home = home_dir();
    let compat = if g.compatdata.is_empty() {
        home.join(".local/share/Steam/steamapps/compatdata/protonrig_custom").to_string_lossy().to_string()
    } else {
        g.compatdata.clone()
    };
    if !platform::IS_WINDOWS {
        let _ = fs::create_dir_all(&compat);
    }
    let effective_pfx = if platform::IS_WINDOWS {
        String::new()
    } else if g.pfx.is_empty() {
        format!("{}/pfx", compat)
    } else {
        g.pfx.clone()
    };

    if auto_tr {
        if let Some(tp) = trainer_opt {
            let tp_path = PathBuf::from(&tp);
            if !tp_path.is_file() {
                log(state, "WARN", format!("The trainer file is missing: {}", tp_path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default()));
            } else {
                let (st_tr, g_tr) = (Arc::clone(state), g.clone());
                thread::spawn(move || {
                    // the trainer has to join the game's Wine session, so it waits until the game is really up
                    let can_see = procs::game_needle(&g_tr).is_some();
                    let waited = (0..180).any(|_| {
                        thread::sleep(Duration::from_secs(1));
                        !can_see || procs::is_game_running(&g_tr)
                    });
                    if !waited {
                        log(&st_tr, "WARN", format!("{} did not start, so the trainer was not started. Use Run Trainer once the game is up.", g_tr.name));
                        return;
                    }
                    thread::sleep(Duration::from_secs(if can_see { 6 } else { 20 }));
                    start_trainer(&st_tr, &g_tr, &tp_path);
                });
            }
        }
    }

    if !g.launches_directly() {
        if running_flag.load(Ordering::SeqCst) {
            log(state, "WARN", format!("{} is already running.", g.name));
            return;
        }
        snapshot_before_launch(state, &g);
        sync_steam_before_launch(state, &g);
        if xdg_open(format!("steam://run/{}", g.appid)) {
            record_launch(state, &g.appid);
            log(state, "OK", format!("Launching {} via Steam...", g.name));
        } else {
            log(state, "ERR", "Could not hand the launch to Steam (xdg-open missing?).");
        }
        return;
    }

    // One game process at a time: a double-click must not start a second instance.
    if running_flag.compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst).is_err() {
        log(state, "WARN", "A game is already running.");
        return;
    }

    snapshot_before_launch(state, &g);

    let exe = if !g.shipping_exe.is_empty() && PathBuf::from(&g.shipping_exe).is_file() {
        PathBuf::from(&g.shipping_exe)
    } else if let Some(ref ub) = ue_bin {
        RigEngine::find_shipping_exe(ub).unwrap_or_else(|| PathBuf::from(&g.exe_path))
    } else {
        PathBuf::from(&g.exe_path)
    };
    if !exe.is_file() {
        running_flag.store(false, Ordering::SeqCst);
        log(state, "ERR", format!("Executable not found: {}", exe.display()));
        return;
    }

    let use_gamemode = gamemode && !platform::IS_WINDOWS && fsutil::find_in_path("gamemoderun").is_some();
    let mut cmd = match (&proton_bin, use_gamemode) {
        _ if platform::IS_WINDOWS => Command::new(&exe),
        (Some(p), true) => {
            let mut c = Command::new("gamemoderun");
            c.arg(p).arg("run").arg(&exe);
            c
        }
        (Some(p), false) => {
            let mut c = Command::new(p);
            c.arg("run").arg(&exe);
            c
        }
        (None, true) => {
            let mut c = Command::new("gamemoderun");
            c.arg("wine").arg(&exe);
            c
        }
        (None, false) => {
            let mut c = Command::new("wine");
            c.arg(&exe);
            c
        }
    };
    cmd.args(g.launch_args.split_whitespace());

    let dll_overrides = merge_overrides(Some(ModManager::get_active_dll_overrides(&g)), &library_overrides(&g))
        .unwrap_or_default();
    #[cfg(unix)]
    cmd.process_group(0); // own group so Stop can take down proton + wine children together
    cmd.current_dir(ue_bin.unwrap_or(gdir));
    if !platform::IS_WINDOWS {
        cmd.env("WINEDLLOVERRIDES", &dll_overrides)
            .env("STEAM_COMPAT_CLIENT_INSTALL_PATH", home.join(".local/share/Steam"))
            .env("STEAM_COMPAT_DATA_PATH", &compat)
            .env("STEAM_COMPAT_APP_ID", compat_app_id(&g))
            .env("WINEPREFIX", &effective_pfx);
    }
    if mangohud && !platform::IS_WINDOWS {
        cmd.env("MANGOHUD", "1");
    }
    for line in g.env_vars.lines() {
        if let Some((k, v)) = line.split_once('=') {
            let k = k.trim();
            if !k.is_empty() && !k.contains(char::is_whitespace) {
                cmd.env(k, v.trim().trim_matches('"'));
            }
        }
    }

    match cmd.spawn() {
        Ok(mut child) => {
            let pid = child.id();
            running_pid.store(pid, Ordering::SeqCst);
            record_launch(state, &g.appid);
            if platform::IS_WINDOWS {
                log(state, "OK", format!("Launched {} (PID: {})", g.name, pid));
            } else {
                log(state, "OK", format!("Launched {} (PID: {}) [WINEDLLOVERRIDES={}]", g.name, pid, dll_overrides));
            }
            let status = child.wait();
            running_pid.store(0, Ordering::SeqCst);
            running_flag.store(false, Ordering::SeqCst);
            match status {
                Ok(s) if s.success() => log(state, "INFO", format!("{} exited cleanly.", g.name)),
                Ok(s) => log(state, "WARN", format!("{} exited with {}.", g.name, s)),
                Err(e) => log(state, "WARN", format!("Lost track of {}: {}", g.name, e)),
            }
        }
        Err(e) => {
            running_flag.store(false, Ordering::SeqCst);
            log(state, "ERR", format!("Failed to launch {}: {}", g.name, e));
        }
    }
}

// ───────────────────────────── server bootstrap ─────────────────────────────

fn log_file_path() -> PathBuf {
    platform::cache_root().join("rig.log")
}

/// Tell once about reports written since the last start (Gantry failed and was started again).
fn announce_crash_reports(state: &SharedState, dir: &Path) {
    let seen = read_state(state).config.crash_seen_ms;
    let (total, fresh) = (crashreport::list(dir).len(), crashreport::unseen(dir, seen));
    write_state(state).crash_reports = total;
    if fresh > 0 {
        {
            let mut st = write_state(state);
            st.config.crash_seen_ms = now_ms();
            st.save_config();
        }
        log(state, "WARN", "ModForge stopped unexpectedly. A report was saved on this computer; nothing was sent.");
        add_note(state, "selfcrash", "ModForge stopped unexpectedly last time. A report was saved on this computer and nothing was sent. Open Help to read it.", "");
    }
}

/// Where the reports of Gantry's own failures are kept.
fn crash_dir() -> PathBuf {
    platform::cache_root().join("crash")
}

/// Launched from a menu there is no terminal: record the problem and tell the user.
fn fatal(msg: &str) -> ! {
    eprintln!("rig: {}", msg);
    let line = format!("[{}] {}\n", fsutil::local_hms(), msg);
    let path = log_file_path();
    if let Some(dir) = path.parent() {
        let _ = fs::create_dir_all(dir);
    }
    if let Ok(mut f) = fs::OpenOptions::new().create(true).append(true).open(&path) {
        let _ = std::io::Write::write_all(&mut f, line.as_bytes());
    }
    let lang = configured_language();
    platform::fatal_notice(&i18n::tr(&lang, "ModForge failed to start"), msg);
    std::process::exit(1);
}

/// The interface language from the config file, for messages shown before the app state exists.
fn configured_language() -> String {
    fs::read(RigPaths::new().config_file)
        .ok()
        .and_then(|b| serde_json::from_slice::<RigConfigData>(&b).ok())
        .map(|c| c.language)
        .unwrap_or_default()
}

/// Identity of this binary: changes whenever it is rebuilt or replaced.
fn build_id() -> String {
    std::env::current_exe()
        .and_then(fs::metadata)
        .map(|m| {
            let (_, mtime, size) = fsutil::file_signature(&m);
            format!("{}-{}-{}", env!("CARGO_PKG_VERSION"), mtime, size)
        })
        .unwrap_or_else(|_| env!("CARGO_PKG_VERSION").to_string())
}

/// If something answers on our port like a rig instance, return its build id.
fn probe_existing_instance(port: u16) -> Option<String> {
    use std::io::Write;
    use std::net::{SocketAddr, TcpStream};
    let addr: SocketAddr = ([127, 0, 0, 1], port).into();
    let mut s = TcpStream::connect_timeout(&addr, Duration::from_millis(800)).ok()?;
    s.set_read_timeout(Some(Duration::from_millis(1500))).ok()?;
    s.set_write_timeout(Some(Duration::from_millis(800))).ok()?;
    let req = format!("GET / HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nConnection: close\r\n\r\n", port);
    s.write_all(req.as_bytes()).ok()?;
    let mut raw = Vec::new();
    let _ = s.read_to_end(&mut raw);
    let text = String::from_utf8_lossy(&raw);
    let head = text.split("\r\n\r\n").next().unwrap_or("");
    if !text.contains("Gantry") && !head.to_lowercase().contains("x-rig-build") {
        return None;
    }
    Some(
        head.lines()
            .find_map(|l| l.to_lowercase().strip_prefix("x-rig-build:").map(|v| v.trim().to_string()))
            .unwrap_or_default(),
    )
}

/// Pids of other running rig processes (kernel comm name is "rig"), excluding ourselves.
#[cfg(not(target_os = "linux"))]
fn other_rig_pids() -> Vec<i32> {
    Vec::new()
}

#[cfg(target_os = "linux")]
fn other_rig_pids() -> Vec<i32> {
    let me = std::process::id() as i32;
    fs::read_dir("/proc")
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| e.file_name().to_string_lossy().parse::<i32>().ok())
        .filter(|&pid| pid != me)
        .filter(|pid| {
            fs::read_to_string(format!("/proc/{}/comm", pid))
                .map(|c| c.trim() == "rig")
                .unwrap_or(false)
        })
        .collect()
}

fn terminate_other_instances() {
    let pids = other_rig_pids();
    for &pid in &pids {
        fsutil::terminate_pid(pid, false);
    }
    for _ in 0..40 {
        if pids.iter().all(|pid| !Path::new(&format!("/proc/{}", pid)).exists()) {
            return;
        }
        thread::sleep(Duration::from_millis(100));
    }
    for &pid in &pids {
        fsutil::terminate_pid(pid, true);
    }
    thread::sleep(Duration::from_millis(200));
}

/// Bind the port. If a healthy instance of the *same build* already owns it, just open a
/// window onto it and exit; if it is older, hung or foreign-but-rig, replace it.
fn bind_or_reuse(port: u16, build: &str, profile_dir: &Path, nxm: Option<&str>) -> Server {
    let addr = format!("127.0.0.1:{}", port);
    if let Ok(srv) = Server::http(&addr) {
        return srv;
    }
    match probe_existing_instance(port) {
        Some(b) if b == build => {
            match nxm {
                // a Nexus link: the running window shows the confirmation, no second window
                Some(url) if forward_nxm(port, url) => {
                    let lang = configured_language();
                    platform::notify(&i18n::tr(&lang, "Mod download waiting"), &i18n::tr(&lang, "Open ModForge to confirm the Nexus download."));
                }
                _ => launch_native_app_window(&format!("http://127.0.0.1:{}", port), profile_dir),
            }
            std::process::exit(0);
        }
        _ => terminate_other_instances(),
    }
    let mut last = String::new();
    for _ in 0..30 {
        match Server::http(&addr) {
            Ok(srv) => return srv,
            Err(e) => last = e.to_string(),
        }
        thread::sleep(Duration::from_millis(100));
    }
    fatal(&format!("Could not bind {} ({}). Another program is using port {}.", addr, last, port))
}

pub fn serve(server: Server, state: SharedState, notifier: Arc<EventNotifier>, ctx: Arc<ServerCtx>) {
    for req in server.incoming_requests() {
        let (st, n, c) = (Arc::clone(&state), Arc::clone(&notifier), Arc::clone(&ctx));
        thread::spawn(move || handle_request(req, st, n, c));
    }
}

const USAGE: &str = "ModForge - Universal Game & Mod Studio\n\nUSAGE:\n  modforge              start ModForge (or connect to running instance)\n  modforge --play GAME [--vanilla | --profile NAME]   start a game (id or name) with its mods, without a window\n  modforge --doctor     print the state of every installed game (diagnostics & mod health)\n  modforge --deck       full-screen controller mode (Steam Deck / couch)\n  modforge nxm://...    handle Nexus Mods 1-click download\n  modforge --install    add menu entry, icon and ~/.local/bin/modforge for this binary\n  modforge --register-nxm / --unregister-nxm   take over / give back nxm:// links\n  modforge --uninstall  remove what --install created\n  modforge --version    print the version\n  modforge --help       this text\n";

// ───────────────────────────── rig --play ─────────────────────────────

/// One HTTP exchange with the Gantry on this machine: `(status, body)`.
fn local_http(port: u16, request: &str) -> Option<(u16, String)> {
    use std::io::Write;
    use std::net::{SocketAddr, TcpStream};
    let addr: SocketAddr = ([127, 0, 0, 1], port).into();
    let mut s = TcpStream::connect_timeout(&addr, Duration::from_millis(800)).ok()?;
    s.set_read_timeout(Some(Duration::from_secs(8))).ok()?;
    s.write_all(request.as_bytes()).ok()?;
    let mut raw = Vec::new();
    let _ = s.read_to_end(&mut raw);
    http_reply(&raw)
}

/// The status and body of a raw HTTP/1.1 reply. A big reply comes in chunks (with their sizes between them), which are
/// put back together.
fn http_reply(raw: &[u8]) -> Option<(u16, String)> {
    let split = raw.windows(4).position(|w| w == b"\r\n\r\n")?;
    let head = String::from_utf8_lossy(&raw[..split]).into_owned();
    let status = head.strip_prefix("HTTP/1.1 ")?.get(..3)?.parse().ok()?;
    let mut rest = &raw[split + 4..];
    if !head.lines().any(|l| l.to_lowercase().replace(' ', "") == "transfer-encoding:chunked") {
        return Some((status, String::from_utf8_lossy(rest).into_owned()));
    }
    let mut body = Vec::new();
    while let Some(eol) = rest.windows(2).position(|w| w == b"\r\n") {
        let size = usize::from_str_radix(String::from_utf8_lossy(&rest[..eol]).split(';').next()?.trim(), 16).ok()?;
        if size == 0 {
            break;
        }
        let chunk = rest.get(eol + 2..eol + 2 + size)?;
        body.extend_from_slice(chunk);
        rest = rest.get(eol + 2 + size + 2..).unwrap_or(&[]);
    }
    Some((status, String::from_utf8_lossy(&body).into_owned()))
}

/// The game a command-line name means: an exact id, an exact name, or the one game whose name contains it.
fn pick_game(games: &[(String, String)], q: &str) -> Result<usize, String> {
    let q = q.trim();
    if let Some(i) = games.iter().position(|(id, _)| id == q) {
        return Ok(i);
    }
    let lower = q.to_lowercase();
    if let Some(i) = games.iter().position(|(_, n)| n.to_lowercase() == lower) {
        return Ok(i);
    }
    let hits: Vec<usize> = games.iter().enumerate().filter(|(_, (_, n))| !lower.is_empty() && n.to_lowercase().contains(&lower)).map(|(i, _)| i).collect();
    match hits.as_slice() {
        [one] => Ok(*one),
        [] => Err(format!("No game called '{q}' in ModForge.")),
        many => Err(format!("'{q}' fits several games: {}. Use the id or the full name.", many.iter().take(6).map(|&i| format!("{} ({})", games[i].1, games[i].0)).collect::<Vec<_>>().join(", "))),
    }
}

/// Ask the running Gantry on `port` to start a game. `mode` is "mods", "vanilla" or "profile:<name>".
fn play_via_running(port: u16, query: &str, mode: &str) -> Result<String, String> {
    let host = format!("127.0.0.1:{port}");
    let (_, page) = local_http(port, &format!("GET / HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n")).ok_or("ModForge does not answer.")?;
    let token = page.split("RIG_TOKEN = \"").nth(1).and_then(|r| r.split('"').next()).ok_or("That is not ModForge.")?.to_string();
    let ask = |method: &str, path: &str, body: Option<Value>| -> Result<Value, String> {
        let body = body.map(|b| b.to_string()).unwrap_or_default();
        let req = format!("{method} {path} HTTP/1.1\r\nHost: {host}\r\nX-Rig-Token: {token}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
        let (code, text) = local_http(port, &req).ok_or("ModForge stopped answering.")?;
        if code != 200 {
            return Err(format!("ModForge refused the request ({code})."));
        }
        serde_json::from_str(&text).map_err(|_| "ModForge sent an unreadable answer.".to_string())
    };
    let state = ask("GET", "/api/state", None)?;
    let games: Vec<(String, String)> = state["games"].as_array().into_iter().flatten().map(|g| (g["appid"].as_str().unwrap_or("").to_string(), g["name"].as_str().unwrap_or("").to_string())).collect();
    let i = pick_game(&games, query)?;
    if state["games"][i]["is_installed"] == false {
        return Err(format!("{} is not installed.", games[i].1));
    }
    ask("POST", "/api/select", Some(json!({ "index": i })))?;
    match mode {
        "vanilla" => ask("POST", "/api/play_vanilla", Some(json!({})))?,
        m if m.starts_with("profile:") => ask("POST", "/api/play_profile", Some(json!({ "name": &m["profile:".len()..] })))?,
        _ => ask("POST", "/api/play", Some(json!({})))?,
    };
    Ok(games[i].1.clone())
}

/// `rig --play GAME [--vanilla | --profile NAME]`: make sure a Gantry is running (without a window when
/// none was), start the game through it, and return. A Gantry started this way ends itself after the game.
/// What `rig --doctor` prints: the state of every installed game, as data.
fn doctor_json(state: &SharedState) -> Value {
    let games: Vec<GameEntry> = read_state(state).games.iter().filter(|g| g.is_installed).cloned().collect();
    let rows: Vec<Value> = games
        .iter()
        .map(|g| {
            let (issues, health) = game_issues(state, g, false);
            let mods = library_of(g).map(|l| l.mods()).unwrap_or_default();
            json!({
                "name": g.name, "engine": g.engine, "source": if g.source.is_empty() { "Steam or custom" } else { g.source.as_str() },
                "loader": health["loader"], "mods": mods.len(), "mods_on": mods.iter().filter(|m| m.enabled).count(),
                "trainer": trainer_line(state, g),
                "issues": issues.iter().map(|i| json!({ "level": i["level"], "code": i["code"], "args": i["args"], "title": i["kb"]["title"]["en"], "evidence": i["kb"]["evidence"] })).collect::<Vec<_>>(),
            })
        })
        .collect();
    json!({ "version": env!("CARGO_PKG_VERSION"), "os": platform::OS, "games": rows })
}

/// Which trainer a game has and the Proton it would be started with (the one the game runs with), for `rig --doctor`.
fn trainer_line(state: &SharedState, g: &GameEntry) -> Value {
    let Some(tp) = read_state(state).config.trainers.get(&g.appid).cloned() else { return Value::Null };
    let file = Path::new(&tp).file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
    let with = game_proton(state, g).map(|script| all_tools(state).into_iter().find(|t| t.script == script).map(|t| t.display).unwrap_or_else(|| script.parent().and_then(|p| p.file_name()).map(|n| n.to_string_lossy().to_string()).unwrap_or_default()));
    json!({ "file": file, "exists": Path::new(&tp).is_file(), "runs_with": with })
}

/// The text of `modforge --doctor`.
fn doctor_text(d: &Value) -> String {
    let mut out = format!("ModForge {} on {}\n\n", d["version"].as_str().unwrap_or(""), d["os"].as_str().unwrap_or(""));
    for g in d["games"].as_array().into_iter().flatten() {
        let issues = g["issues"].as_array().cloned().unwrap_or_default();
        let worst = issues.iter().filter_map(|i| i["level"].as_str()).map(level_rank).max().unwrap_or(0);
        let mark = match worst {
            3 => "[bad] ",
            2 => "[warn]",
            _ => "[ok]  ",
        };
        let loader = g["loader"].as_str().filter(|l| !l.is_empty() && *l != "none").map(|l| format!(", {l}")).unwrap_or_default();
        out.push_str(&format!("{mark} {} ({}{}) {} of {} mods on\n", g["name"].as_str().unwrap_or(""), g["engine"].as_str().unwrap_or(""), loader, g["mods_on"], g["mods"]));
        if let Some(t) = g["trainer"].as_object() {
            out.push_str(&format!("         trainer {}{} runs with {}\n", t["file"].as_str().unwrap_or(""), if t["exists"] == false { " (file is missing)" } else { "" }, t["runs_with"].as_str().filter(|w| !w.is_empty()).unwrap_or("plain Wine")));
        }
        for i in issues.iter().filter(|i| i["level"] != "info") {
            let args: Vec<&str> = i["args"].as_array().into_iter().flatten().filter_map(Value::as_str).filter(|a| !a.is_empty()).collect();
            // a known problem from the logs has a title and the log line that showed it; the others a short code
            let what = i["title"].as_str().map(str::to_string).unwrap_or_else(|| match i["code"].as_str().unwrap_or("") {
                "launch_option" => "launch_option: Steam's launch options lack what the mods need (open ModForge, then Doctor, Close Steam & apply)".to_string(),
                "needs_framework" => format!("needs_framework: the mods need {} and the game does not have it", args.join(", ")),
                "duplicate_mod" => format!("duplicate_mod: installed more than once: {}", args.join("; ")),
                code => format!("{}{}", code, if args.is_empty() { String::new() } else { format!(": {}", args.join(", ")) }),
            });
            out.push_str(&format!("         {} {}\n", i["level"].as_str().unwrap_or(""), what));
            if let Some(line) = i["evidence"].as_str().filter(|l| !l.is_empty()) {
                out.push_str(&format!("                {}\n", line.chars().take(110).collect::<String>()));
            }
        }
    }
    out.push_str("\nMore: open ModForge to manage and inspect all games.\n");
    out
}

/// `modforge --doctor`: ask a running ModForge (or one started without a window) about every installed game and print it.
fn cli_doctor() -> i32 {
    let port = server_port();
    let mut child = None;
    if probe_existing_instance(port).is_none() {
        child = std::env::current_exe().ok().and_then(|exe| Command::new(exe).env("RIG_NO_WINDOW", "1").stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).spawn().ok());
        if child.is_none() || !(0..100).any(|_| {
            thread::sleep(Duration::from_millis(200));
            probe_existing_instance(port).is_some()
        }) {
            eprintln!("modforge: could not start ModForge engine.");
            if let Some(c) = child.as_mut() {
                let _ = c.kill();
            }
            return 1;
        }
    }
    let host = format!("127.0.0.1:{port}");
    let answer = (|| -> Result<Value, String> {
        let (_, page) = local_http(port, &format!("GET / HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n")).ok_or("ModForge engine does not answer.")?;
        let token = page.split("RIG_TOKEN = \"").nth(1).and_then(|r| r.split('"').next()).ok_or("That is not ModForge.")?.to_string();
        let (code, text) = local_http(port, &format!("GET /api/doctor_all HTTP/1.1\r\nHost: {host}\r\nX-Rig-Token: {token}\r\nConnection: close\r\n\r\n")).ok_or("ModForge engine stopped answering.")?;
        if code != 200 {
            return Err(format!("ModForge engine refused the request ({code})."));
        }
        serde_json::from_str(&text).map_err(|_| "ModForge engine sent an unreadable answer.".to_string())
    })();
    if let Some(c) = child.as_mut() {
        let _ = c.kill();
        let _ = c.wait();
    }
    match answer {
        Ok(d) => {
            print!("{}", doctor_text(&d));
            0
        }
        Err(e) => {
            eprintln!("modforge: {e}");
            1
        }
    }
}

fn cli_play(args: &[String]) -> i32 {
    let Some(query) = args.first().filter(|a| !a.starts_with("--")) else {
        eprintln!("rig: --play needs a game (its id or name).");
        return 2;
    };
    let mode = match args.get(1).map(String::as_str) {
        None => "mods".to_string(),
        Some("--vanilla") => "vanilla".to_string(),
        Some("--profile") => match args.get(2) {
            Some(n) => format!("profile:{n}"),
            None => {
                eprintln!("rig: --profile needs a name.");
                return 2;
            }
        },
        Some(other) => {
            eprintln!("rig: unknown option '{other}' after --play.");
            return 2;
        }
    };
    let port = server_port();
    if probe_existing_instance(port).is_none() {
        let spawned = std::env::current_exe().ok().map(|exe| {
            Command::new(exe)
                .env("RIG_NO_WINDOW", "1")
                .env("RIG_EXIT_AFTER_PLAY", "1")
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
        });
        if !matches!(spawned, Some(Ok(_))) {
            eprintln!("modforge: could not start ModForge engine.");
            return 1;
        }
        let up = (0..100).any(|_| {
            thread::sleep(Duration::from_millis(200));
            probe_existing_instance(port).is_some()
        });
        if !up {
            eprintln!("modforge: ModForge engine did not start.");
            return 1;
        }
    }
    match play_via_running(port, query, &mode) {
        Ok(name) => {
            println!("Starting {name}...");
            0
        }
        Err(e) => {
            eprintln!("modforge: {e}");
            1
        }
    }
}

/// A ModForge instance started by `modforge --play` has no window to close: it ends itself a little after the game does
/// (or when no game ever started).
fn spawn_exit_after_play(state: Weak<RwLock<AppState>>) {
    thread::spawn(move || {
        let started = Instant::now();
        let mut seen = false;
        let mut idle_since: Option<Instant> = None;
        loop {
            thread::sleep(Duration::from_secs(5));
            let Some(st) = state.upgrade() else { return };
            let (running, sessions) = {
                let s = read_state(&st);
                (s.is_running.load(Ordering::SeqCst), !s.config.vanilla_sessions.is_empty())
            };
            seen |= running;
            if running || sessions {
                idle_since = None;
                continue;
            }
            let idle = *idle_since.get_or_insert_with(Instant::now);
            if (seen && idle.elapsed() > Duration::from_secs(15)) || (!seen && started.elapsed() > Duration::from_secs(20 * 60)) {
                std::process::exit(0);
            }
        }
    });
}

/// Command-line switches. Returns an exit code when the process should stop right away.
fn handle_cli(args: Vec<String>) -> Option<i32> {
    // Finder adds a "-psn_..." argument when it starts an app
    let args: Vec<String> = args.into_iter().filter(|a| !a.starts_with("-psn_")).collect();
    if !args.is_empty() {
        platform::attach_parent_console();
    }
    let exe = std::env::current_exe().unwrap_or_default();
    match args.first().map(String::as_str) {
        None => None,
        Some(a) if a.to_ascii_lowercase().starts_with("nxm://") => None, // a Nexus link: run normally
        Some("--register-nxm") | Some("--unregister-nxm") => {
            let (home, cfg) = (home_dir(), platform::config_dir());
            if args[0] == "--register-nxm" {
                match install::register_nxm(&home, &cfg, &exe) {
                    Ok(m) => {
                        println!("{}", m);
                        Some(0)
                    }
                    Err(e) => {
                        eprintln!("modforge: {}", e);
                        Some(1)
                    }
                }
            } else {
                println!("{}", install::unregister_nxm(&home, &cfg));
                Some(0)
            }
        }
        Some("--play") => Some(cli_play(&args[1..])),
        Some("--doctor") => Some(cli_doctor()),
        // couch mode: full screen, big controls, the gamepad drives everything (the app then starts as usual)
        Some("--deck") => {
            std::env::set_var("RIG_DECK", "1");
            None
        }
        Some("--version") | Some("-V") => {
            println!("ModForge {}", env!("CARGO_PKG_VERSION"));
            Some(0)
        }
        Some("--help") | Some("-h") => {
            print!("{}", USAGE);
            Some(0)
        }
        Some("--install") => Some(match install::install(&home_dir(), &exe) {
            Ok(steps) => {
                steps.iter().for_each(|s| println!("{}", s));
                install::refresh_desktop_caches(&home_dir());
                println!("done. Search for \"ModForge\" in your application menu.");
                0
            }
            Err(e) => {
                eprintln!("modforge: install failed: {}", e);
                1
            }
        }),
        Some("--uninstall") => {
            let steps = install::uninstall(&home_dir(), &exe);
            if steps.is_empty() {
                println!("nothing to remove.");
            }
            steps.iter().for_each(|s| println!("{}", s));
            install::refresh_desktop_caches(&home_dir());
            Some(0)
        }
        Some(other) => {
            eprintln!("modforge: unknown option '{}'\n\n{}", other, USAGE);
            Some(2)
        }
    }
}

fn main() {
    std::panic::set_hook(Box::new(|info| {
        let line = format!("[{}] PANIC: {}\n", fsutil::local_hms(), info);
        eprint!("{}", line);
        if let Ok(mut f) = fs::OpenOptions::new().create(true).append(true).open(log_file_path()) {
            let _ = std::io::Write::write_all(&mut f, line.as_bytes());
        }
        // a report to read on this computer; nothing is sent
        let log = fs::read_to_string(log_file_path()).unwrap_or_default();
        let ms = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0);
        let home = home_dir().to_string_lossy().to_string();
        let _ = crashreport::write(&crash_dir(), ms, env!("CARGO_PKG_VERSION"), platform::OS, &info.to_string(), crashreport::tail(&log, 6000), &home);
    }));

    if let Some(code) = handle_cli(std::env::args().skip(1).collect()) {
        std::process::exit(code);
    }

    platform::hide_console_for_children();
    // Claim the port first (cheap) so a second launch never pays for a library scan.
    let paths = RigPaths::new();
    let build = build_id();
    let nxm_arg = std::env::args().skip(1).find(|a| a.to_ascii_lowercase().starts_with("nxm://"));
    let server = bind_or_reuse(server_port(), &build, &paths.webview_profile_dir, nxm_arg.as_deref());

    let notifier = Arc::new(EventNotifier::new());
    let app_state = AppState::new(Arc::clone(&notifier));
    let profile_dir = app_state.paths.webview_profile_dir.clone();
    let pid_file = app_state.cache_root().join("rig.pid");
    let _ = atomic_write(&pid_file, std::process::id().to_string());
    let state: SharedState = Arc::new(RwLock::new(app_state));

    let trash = read_state(&state).cache_root().join("trash");
    ModManager::purge_trash(&trash, TRASH_MAX_AGE);
    report_repaired(&state);
    spawn_self_check(Arc::downgrade(&state));
    resume_bisect(&state);
    load_nexus_key(&state);
    load_vt_key(&state);
    load_modio_key(&state);
    load_steam_lib(&state);
    load_steam_played(&state);
    spawn_steam_refresher(Arc::downgrade(&state));
    spawn_inotify_watcher(Arc::downgrade(&state));
    spawn_process_monitor(Arc::downgrade(&state));
    spawn_library_watcher(Arc::downgrade(&state));
    spawn_update_checker(Arc::downgrade(&state));
    {
        let path = read_state(&state).paths.config_file.with_file_name("recipes.json");
        let all = recipes::load(&path);
        recipes::publish(&all);
        write_state(&state).recipes = all;
    }
    {
        let st = read_state(&state);
        downloads::watch_game(Arc::clone(&st.is_running));
        downloads::set_limits(downloads::Limits { kb_per_sec: st.config.download_limit_kb, wait_for_game: st.config.downloads_wait });
        background::set_while_playing(st.config.checks_while_playing);
    }
    resume_vanilla_sessions(&state);
    resume_safe_mode_sessions(&state);
    spawn_health_monitor(Arc::downgrade(&state));
    spawn_cover_lookup(Arc::downgrade(&state));
    selfupdate::clean_up_after_update();
    announce_crash_reports(&state, &crash_dir());
    spawn_self_update_check(Arc::downgrade(&state));
    spawn_want_watcher(Arc::downgrade(&state));
    spawn_auto_backup(Arc::downgrade(&state));
    spawn_sync_push(Arc::downgrade(&state));
    if std::env::var_os("RIG_EXIT_AFTER_PLAY").is_some() {
        spawn_exit_after_play(Arc::downgrade(&state));
    }

    let ctx = Arc::new(ServerCtx::new(server_port(), fsutil::random_token(), build));
    let url = format!("http://127.0.0.1:{}", server_port());
    println!("⚡ ModForge running at {}", url);
    if let Some(u) = &nxm_arg {
        match nxm_request(&state, u) {
            Ok(m) => log(&state, "INFO", m),
            Err(e) => log(&state, "WARN", e),
        }
    }
    launch_native_app_window(&url, &profile_dir);
    serve(server, state, notifier, ctx);
    let _ = fs::remove_file(&pid_file);
}

// ───────────────────────────── tests ─────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fsutil::testutil::TempDir;
    use crate::repair::fixtures::make_game;
    use std::io::Write;
    use std::net::{SocketAddr, TcpStream};

    struct Harness {
        _dir: TempDir,
        state: SharedState,
        addr: SocketAddr,
        token: String,
        game: GameEntry,
    }

    fn harness(tag: &str) -> Harness {
        let (dir, game) = make_game(tag);
        let cache = dir.join("cache");
        let paths = RigPaths {
            custom_games_file: dir.join("cfg/custom_games.json"),
            config_file: dir.join("cfg/rig_config.json"),
            cache_dir: cache.join("injectors"),
            engine_cache_file: cache.join("engine_cache.json"),
            webview_profile_dir: cache.join("webview"),
            hydra_dir: None,
            launchers: Default::default(),
        };
        fs::create_dir_all(&paths.cache_dir).unwrap();
        let notifier = Arc::new(EventNotifier::new());
        let mut st = AppState::with(paths, RigConfigData::default(), vec![game.clone()], vec![], Arc::clone(&notifier));
        // Never touch the developer's real Steam: no library scan, and a localconfig path that
        // does not exist (tests that exercise launch options point this at a fixture file).
        st.steam_env = SteamEnv { config_vdf: None, localconfig: Some(dir.join("no-steam/localconfig.vdf")), running: Some(false), libraries: Some(vec![]) };
        let state: SharedState = Arc::new(RwLock::new(st));
        let server = Server::http("127.0.0.1:0").unwrap();
        let addr = server.server_addr().to_ip().unwrap();
        let token = fsutil::random_token();
        let ctx = Arc::new(ServerCtx::new(addr.port(), token.clone(), "test".into()));
        let (s2, n2) = (Arc::clone(&state), notifier);
        thread::spawn(move || serve(server, s2, n2, ctx));
        Harness { _dir: dir, state, addr, token, game }
    }

    /// Minimal blocking HTTP/1.1 client: (status, body).
    fn http(h: &Harness, method: &str, path: &str, extra: &[(&str, &str)], body: &[u8]) -> (u16, String) {
        let mut s = TcpStream::connect(h.addr).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(40))).unwrap();
        let mut head = format!("{} {} HTTP/1.1\r\nConnection: close\r\nContent-Length: {}\r\n", method, path, body.len());
        if !extra.iter().any(|(k, _)| k.eq_ignore_ascii_case("host")) {
            head.push_str(&format!("Host: 127.0.0.1:{}\r\n", h.addr.port()));
        }
        for (k, v) in extra {
            head.push_str(&format!("{}: {}\r\n", k, v));
        }
        head.push_str("\r\n");
        s.write_all(head.as_bytes()).unwrap();
        s.write_all(body).unwrap();
        let mut raw = Vec::new();
        let _ = s.read_to_end(&mut raw);
        let split = raw.windows(4).position(|w| w == b"\r\n\r\n").map(|p| p + 4).unwrap_or(raw.len());
        let head = String::from_utf8_lossy(&raw[..split]).to_string();
        let status = head.split_whitespace().nth(1).and_then(|c| c.parse().ok()).unwrap_or(0);
        let body_bytes = if head.to_lowercase().contains("transfer-encoding: chunked") { dechunk(&raw[split..]) } else { raw[split..].to_vec() };
        (status, String::from_utf8_lossy(&body_bytes).to_string())
    }

    /// Body of an HTTP/1.1 chunked response.
    fn dechunk(raw: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        let mut i = 0;
        while let Some(eol) = raw[i..].windows(2).position(|w| w == b"\r\n") {
            let n = usize::from_str_radix(String::from_utf8_lossy(&raw[i..i + eol]).trim(), 16).unwrap_or(0);
            i += eol + 2;
            if n == 0 || i + n > raw.len() {
                break;
            }
            out.extend_from_slice(&raw[i..i + n]);
            i += n + 2;
        }
        out
    }

    /// The archive program the tests use to make zips: bsdtar, which Windows ships under the name tar.
    fn tar_tool() -> &'static str {
        if fsutil::find_in_path("bsdtar").is_some() {
            "bsdtar"
        } else {
            "tar"
        }
    }

    fn api(h: &Harness, path: &str, body: Value) -> Value {
        let (code, text) = http(h, "POST", path, &[("X-Rig-Token", &h.token)], body.to_string().as_bytes());
        assert_eq!(code, 200, "{path}: {text}");
        serde_json::from_str(&text).unwrap_or_else(|_| panic!("bad json from {path}: {text}"))
    }

    fn wait_until(what: &str, mut f: impl FnMut() -> bool) {
        let end = Instant::now() + Duration::from_secs(20);
        while Instant::now() < end {
            if f() {
                return;
            }
            thread::sleep(Duration::from_millis(50));
        }
        panic!("timed out waiting for {what}");
    }

    fn mod_enabled(v: &Value, id: &str) -> Option<bool> {
        v["mods"].as_array()?.iter().find(|m| m["id"] == id).map(|m| m["enabled"].as_bool().unwrap())
    }

    #[test]
    fn index_is_served_with_token_and_hardening_headers() {
        let h = harness("index");
        let (code, body) = http(&h, "GET", "/", &[], b"");
        assert_eq!(code, 200);
        assert!(body.contains(&h.token), "token injected");
        assert!(!body.contains("__RIG_TOKEN__"));
        assert!(body.contains("ModForge"));
        assert_eq!(http(&h, "GET", "/nope", &[], b"").0, 404);
    }

    #[test]
    fn the_checks_gantry_does_on_its_own_are_listed_and_can_be_started_at_once() {
        let h = harness("bglist");
        background::register("t-api", "Looking up game covers", 60_000, 60_000, now_ms());
        let list = |h: &Harness| {
            let (code, body) = http(h, "GET", "/api/downloads", &[("X-Rig-Token", &h.token)], b"");
            assert_eq!(code, 200);
            serde_json::from_str::<Value>(&body).unwrap()["background"]["tasks"].as_array().unwrap().iter().find(|t| t["name"] == "t-api").cloned().unwrap()
        };
        assert!(list(&h)["next_ms"].as_u64().unwrap() > now_ms(), "not due yet");
        api(&h, "/api/background_run", json!({ "name": "t-api" }));
        assert_eq!(list(&h)["next_ms"], 0, "Run now makes it due at once");
        assert_eq!(background::due("t-api", now_ms(), true), Some(true), "and it does not wait for the game");
        api(&h, "/api/background_run", json!({ "name": "t-api" }));
        assert!(h.state.read().unwrap().logs.iter().any(|l| l.message.contains("already running")));
        background::finished("t-api", now_ms(), Ok(true));
        assert_eq!(list(&h)["running"], false);
    }

    #[test]
    fn api_rejects_missing_token_foreign_host_and_foreign_origin() {
        let h = harness("auth");
        assert_eq!(http(&h, "GET", "/api/state", &[], b"").0, 403, "no token");
        assert_eq!(http(&h, "GET", "/api/state", &[("X-Rig-Token", "wrong")], b"").0, 403);
        assert_eq!(http(&h, "POST", "/api/wipe_mods", &[], b"{}").0, 403);
        assert_eq!(
            http(&h, "GET", "/api/state", &[("X-Rig-Token", &h.token), ("Host", "evil.example:80")], b"").0,
            403,
            "DNS rebinding"
        );
        assert_eq!(
            http(&h, "GET", "/api/state", &[("X-Rig-Token", &h.token), ("Origin", "https://evil.example")], b"").0,
            403,
            "cross-site"
        );
        assert_eq!(http(&h, "GET", "/", &[("Host", "evil.example")], b"").0, 403);
        // token via query (used by sendBeacon) and header both work
        let q = format!("/api/state?t={}", h.token);
        assert_eq!(http(&h, "GET", &q, &[], b"").0, 200);
        assert_eq!(http(&h, "GET", "/api/state", &[("X-Rig-Token", &h.token)], b"").0, 200);
        // nothing was wiped by the rejected call
        assert!(PathBuf::from(&h.game.bin_dir).join("ue4ss").is_dir());
        let (c, _) = http(&h, "POST", "/api/definitely_not_real", &[("X-Rig-Token", &h.token)], b"{}");
        assert_eq!(c, 404, "unknown endpoints are not silently accepted");
    }

    #[test]
    fn toggle_by_id_survives_list_reordering() {
        let h = harness("toggleid");
        let st = api(&h, "/api/state", json!({}));
        assert_eq!(mod_enabled(&st, "PAK:Skin_P"), Some(true));
        let st = api(&h, "/api/toggle_mod", json!({ "id": "PAK:Skin_P", "enable": false }));
        assert_eq!(mod_enabled(&st, "PAK:Skin_P"), Some(false));
        assert_eq!(mod_enabled(&st, "LUA:CoolMod"), Some(true), "neighbours untouched");
        let st = api(&h, "/api/toggle_mod", json!({ "id": "LUA:CoolMod", "enable": false }));
        assert_eq!(mod_enabled(&st, "LUA:CoolMod"), Some(false));
        // unknown ids are a no-op, not a wrong-target toggle
        let st = api(&h, "/api/toggle_mod", json!({ "id": "PAK:Ghost", "enable": false }));
        assert_eq!(mod_enabled(&st, "PAK:Skin_P"), Some(false));
        assert_eq!(mod_enabled(&st, "LUA:CoolMod"), Some(false));
    }

    #[test]
    fn profiles_bulk_delete_and_config_editing() {
        let h = harness("profiles");
        api(&h, "/api/profile_save", json!({ "name": "all-on" }));
        api(&h, "/api/bulk_toggle_mods", json!({ "enable": false }));
        let st = api(&h, "/api/state", json!({}));
        assert_eq!(mod_enabled(&st, "PAK:Skin_P"), Some(false));
        assert_eq!(mod_enabled(&st, "LUA:Keybinds"), Some(true), "system mod stays on");
        assert_eq!(st["profiles"], json!(["all-on"]));
        let st = api(&h, "/api/profile_load", json!({ "name": "all-on" }));
        assert_eq!(mod_enabled(&st, "PAK:Skin_P"), Some(true));
        assert_eq!(mod_enabled(&st, "LUA:CoolMod"), Some(true));
        // profile persisted to disk atomically
        let cfg = fs::read_to_string(&h.state.read().unwrap().paths.config_file).unwrap();
        assert!(cfg.contains("all-on"));

        let got = api(&h, "/api/mod_config_get", json!({ "id": "LUA:CoolMod" }));
        assert_eq!(got["content"], "print('hi')");
        api(&h, "/api/mod_config_save", json!({ "id": "LUA:CoolMod", "content": "print('edited')" }));
        let got = api(&h, "/api/mod_config_get", json!({ "id": "LUA:CoolMod" }));
        assert_eq!(got["content"], "print('edited')");

        let st = api(&h, "/api/delete_mod", json!({ "id": "LUA:Keybinds" }));
        assert!(mod_enabled(&st, "LUA:Keybinds").is_some(), "system mods are protected");
        let st = api(&h, "/api/delete_mod", json!({ "id": "PAK:Skin_P" }));
        assert!(mod_enabled(&st, "PAK:Skin_P").is_none());
        let st = api(&h, "/api/profile_delete", json!({ "name": "all-on" }));
        assert_eq!(st["profiles"], json!([]));
    }

    #[test]
    fn launch_options_and_flags_persist() {
        let h = harness("launch");
        let st = api(&h, "/api/launch_options", json!({ "launch_args": "-dx12", "env_vars": "A=1" }));
        assert_eq!(st["games"][0]["launch_args"], "-dx12");
        let cfg = fs::read_to_string(&h.state.read().unwrap().paths.config_file).unwrap();
        assert!(cfg.contains("-dx12") && cfg.contains("A=1"));
        let st = api(&h, "/api/toggle_flag", json!({ "flag": "mangohud" }));
        assert_eq!(st["mangohud"], true);
        let st = api(&h, "/api/set_steam_appid", json!({ "steam_appid": "12x" }));
        assert_eq!(st["games"][0]["steam_appid"], "", "non-numeric ids rejected");
        let st = api(&h, "/api/set_steam_appid", json!({ "steam_appid": "12345" }));
        assert_eq!(st["games"][0]["steam_appid"], "12345");
    }

    #[test]
    fn tuner_endpoints_validate_and_roundtrip() {
        let h = harness("tuner");
        let get = |h: &Harness| {
            let (c, b) = http(h, "GET", "/api/ue4ss_tuner_get", &[("X-Rig-Token", &h.token)], b"");
            assert_eq!(c, 200);
            serde_json::from_str::<Value>(&b).unwrap()
        };
        let mut dto = get(&h);
        assert_eq!(dto["minor_version"], "3");
        dto["gui_console"] = json!(true);
        api(&h, "/api/ue4ss_tuner_save", dto.clone());
        assert_eq!(get(&h)["gui_console"], true);
        dto["minor_version"] = json!("9\nX=1");
        let st = api(&h, "/api/ue4ss_tuner_save", dto);
        assert!(st["last_toast"].as_str().unwrap().contains("Invalid"));
        assert_eq!(get(&h)["minor_version"], "3");
    }

    #[test]
    fn doctor_and_self_heal_over_http() {
        let h = harness("doctor");
        let (_, b) = http(&h, "GET", "/api/doctor", &[("X-Rig-Token", &h.token)], b"");
        let rep: Value = serde_json::from_str(&b).unwrap();
        assert_eq!(rep["has_ue"], true);
        assert_eq!(rep["detected_version"], "5.3");
        assert!(!rep["issues"].as_array().unwrap().is_empty());
        api(&h, "/api/self_heal", json!({}));
        wait_until("self-heal", || {
            let (_, b) = http(&h, "GET", "/api/doctor", &[("X-Rig-Token", &h.token)], b"");
            serde_json::from_str::<Value>(&b).unwrap()["issues"].as_array().unwrap().iter().all(|i| {
                // templates are not shipped in the fixture; everything else must be healed
                i.as_str().unwrap().contains("VTableLayout")
            })
        });
    }

    #[test]
    fn subscribe_wakes_immediately_on_change() {
        let h = harness("subscribe");
        let v = api(&h, "/api/state", json!({}))["version"].as_u64().unwrap();
        let (addr, tok) = (h.addr, h.token.clone());
        let waiter = thread::spawn(move || {
            let t0 = Instant::now();
            let mut s = TcpStream::connect(addr).unwrap();
            let req = format!(
                "GET /api/subscribe?v={} HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nX-Rig-Token: {}\r\nConnection: close\r\n\r\n",
                v, addr.port(), tok
            );
            s.write_all(req.as_bytes()).unwrap();
            let mut out = String::new();
            let _ = s.read_to_string(&mut out);
            (t0.elapsed(), out)
        });
        thread::sleep(Duration::from_millis(300));
        api(&h, "/api/toggle_flag", json!({ "flag": "gamemode" }));
        let (elapsed, out) = waiter.join().unwrap();
        assert!(elapsed < Duration::from_secs(5), "woke in {:?}", elapsed);
        assert!(out.contains("\"gamemode\":false"));
    }

    #[test]
    fn concurrent_mixed_traffic_never_deadlocks_or_corrupts() {
        let h = Arc::new(harness("stress"));
        let done = Arc::new(AtomicU64::new(0));
        let mut ts = Vec::new();
        for t in 0..8 {
            let (h, done) = (Arc::clone(&h), Arc::clone(&done));
            ts.push(thread::spawn(move || {
                for i in 0..12 {
                    match (t + i) % 4 {
                        0 => {
                            api(&h, "/api/toggle_mod", json!({ "id": "PAK:Skin_P", "enable": i % 2 == 0 }));
                        }
                        1 => {
                            api(&h, "/api/select", json!({ "index": 0 }));
                        }
                        2 => {
                            api(&h, "/api/toggle_mod", json!({ "id": "LUA:CoolMod", "enable": i % 2 == 1 }));
                        }
                        _ => {
                            let _ = http(&h, "GET", "/api/state", &[("X-Rig-Token", &h.token)], b"");
                        }
                    }
                    done.fetch_add(1, Ordering::SeqCst);
                }
            }));
        }
        let t0 = Instant::now();
        for t in ts {
            t.join().unwrap();
        }
        assert_eq!(done.load(Ordering::SeqCst), 96);
        assert!(t0.elapsed() < Duration::from_secs(60));
        // The mods.txt that all threads hammered is still well-formed.
        let txt = fs::read_to_string(PathBuf::from(&h.game.bin_dir).join("ue4ss/Mods/mods.txt")).unwrap();
        assert_eq!(txt.matches("CoolMod").count(), 1);
        assert!(txt.lines().all(|l| l.contains(" : ") || l.trim().is_empty()), "{txt}");
        // Final on-disk state agrees with the in-memory model.
        // (the file watcher may still be applying the last events, so wait until a fresh read agrees)
        wait_until("memory agrees with disk", || {
            let before = api(&h, "/api/state", json!({}))["mods"].clone();
            refresh_mods_shared(&h.state);
            before == api(&h, "/api/state", json!({}))["mods"]
        });
    }

    #[test]
    fn dropped_pak_is_streamed_installed_and_staging_cleaned() {
        let h = harness("droppak");
        let payload = vec![0xABu8; 3 * 1024 * 1024];
        let (code, _) = http(&h, "POST", "/api/drop_upload?name=Cool%20Hat_P.pak", &[("X-Rig-Token", &h.token)], &payload);
        assert_eq!(code, 200);
        let dest = PathBuf::from(&h.game.game_dir).join("Demo/Content/Paks/~mods/Cool Hat_P.pak");
        wait_until("pak installed", || dest.is_file());
        assert_eq!(fs::metadata(&dest).unwrap().len(), payload.len() as u64);
        let drops = h.state.read().unwrap().cache_root().join("drops");
        wait_until("drop buffer removed", || fs::read_dir(&drops).map(|mut r| r.next().is_none()).unwrap_or(true));
        wait_until("mod listed", || mod_enabled(&api(&h, "/api/state", json!({})), "PAK:Cool Hat_P") == Some(true));

        // path traversal in the upload name is neutralised
        let (c, _) = http(&h, "POST", "/api/drop_upload?name=..%2F..%2Fevil.dll", &[("X-Rig-Token", &h.token)], b"MZ");
        assert_eq!(c, 200);
        wait_until("dll placed in Win64", || PathBuf::from(&h.game.bin_dir).join("evil.dll").is_file());
        assert!(!h._dir.path().join("evil.dll").exists());
        // empty upload is rejected
        let (c, _) = http(&h, "POST", "/api/drop_upload?name=x.pak", &[("X-Rig-Token", &h.token)], b"");
        assert_eq!(c, 400);
        // GET is not accepted for uploads
        assert_eq!(http(&h, "GET", "/api/drop_upload?name=x.pak", &[("X-Rig-Token", &h.token)], b"").0, 405);
    }

    #[cfg(unix)]
    #[test]
    fn run_trainer_starts_it_with_the_proton_steam_uses_for_the_game_and_leaves_a_log() {
        use std::os::unix::fs::PermissionsExt;
        let h = harness("trainerrun");
        add_plain_game(&h);
        let appid = "4242".to_string();
        {
            let mut st = h.state.write().unwrap();
            let i = st.selected_idx;
            st.games[i].appid = appid.clone();
        }
        // two fake Protons: Steam is set to use the first one; Gantry's own pick is the second
        let ct = h._dir.join("ct");
        let fake = |dir: &str, says: &str| {
            let p = ct.join(dir).join("proton");
            fs::create_dir_all(p.parent().unwrap()).unwrap();
            fs::write(&p, format!("#!/bin/sh\necho \"{says} $@ app=$STEAM_COMPAT_APP_ID pfx=$WINEPREFIX\"\nsleep 1\n")).unwrap();
            fs::set_permissions(&p, fs::Permissions::from_mode(0o755)).unwrap();
            p
        };
        let steams = fake("Steamish", "steam-tool");
        let others = fake("Other", "other-tool");
        fs::write(ct.join("Steamish/compatibilitytool.vdf"), "\"compatibilitytools\"\n{\n \"compat_tools\"\n {\n  \"steamish\"\n  {\n   \"display_name\" \"Steamish\"\n  }\n }\n}\n").unwrap();
        // a private copy of Steam's config; nothing real is read or started
        let cfg = h._dir.join("config.vdf");
        fs::write(&cfg, "\"InstallConfigStore\"\n{\n\t\"Software\"\n\t{\n\t\t\"Valve\"\n\t\t{\n\t\t\t\"Steam\"\n\t\t\t{\n\t\t\t\t\"CompatToolMapping\"\n\t\t\t\t{\n\t\t\t\t\t\"0\"\n\t\t\t\t\t{\n\t\t\t\t\t\t\"name\"\t\t\"steamish\"\n\t\t\t\t\t}\n\t\t\t\t}\n\t\t\t}\n\t\t}\n\t}\n}\n").unwrap();
        let trainer = h._dir.join("t/Tr.exe");
        fs::create_dir_all(trainer.parent().unwrap()).unwrap();
        fs::write(&trainer, b"MZ").unwrap();
        {
            let mut st = h.state.write().unwrap();
            st.tools_dirs = Some(vec![ct.clone()]);
            st.steam_env = SteamEnv { config_vdf: Some(cfg), localconfig: None, running: Some(false), libraries: Some(vec![]) };
            st.config.proton_choice.insert(appid.clone(), others.to_string_lossy().to_string());
            st.config.trainers.insert(appid, trainer.to_string_lossy().to_string());
        }
        let log = h._dir.join("cache/trainer.log");
        api(&h, "/api/trainer", json!({}));
        wait_until("trainer log", || fs::read_to_string(&log).map(|t| t.contains("steam-tool run") && t.contains("Tr.exe")).unwrap_or(false));
        let said = fs::read_to_string(&log).unwrap();
        assert!(said.contains("-- proton run --") && said.contains("app=") && !said.contains("other-tool"), "Steam's own choice wins over Gantry's: {said}");
        let _ = steams;
    }

    #[test]
    fn several_trainers_are_kept_per_game_and_one_is_picked() {
        let h = harness("trainershelf");
        add_plain_game(&h);
        let (appid, gi) = {
            let st = h.state.read().unwrap();
            (st.games[st.selected_idx].appid.clone(), st.selected_idx)
        };
        let kept = trainers_dir(&h.state.read().unwrap().paths.cache_dir);
        fs::create_dir_all(&kept).unwrap();
        let old = kept.join(format!("{appid}_Game v1.0-v1.2 Trainer.exe"));
        fs::write(&old, b"MZ").unwrap();
        let mine = h._dir.join("Downloads/Game v2.0 Trainer.exe");
        fs::create_dir_all(mine.parent().unwrap()).unwrap();
        fs::write(&mine, b"MZ").unwrap();
        attach_trainer(&h.state, &appid, &old, "Game v1.0-v1.2 Trainer.exe");
        attach_trainer(&h.state, &appid, &mine, "Game v2.0 Trainer.exe");
        let shelf = || api(&h, "/api/state", json!({}))["trainer_shelf"].as_array().cloned().unwrap();
        let s = shelf();
        assert_eq!(s.len(), 2, "{s:?}");
        let cur: Vec<_> = s.iter().filter(|t| t["current"] == true).map(|t| t["name"].as_str().unwrap().to_string()).collect();
        assert_eq!(cur, vec!["Game v2.0 Trainer.exe"], "the newest one is attached");
        let older = s.iter().find(|t| t["current"] == false).unwrap();
        assert_eq!(older["name"], "Game v1.0-v1.2 Trainer.exe", "Gantry's copy is shown without the id in front");
        assert_eq!(older["made_for"], "1.0–1.2");
        // switch back to the older one
        api(&h, "/api/trainer_use", json!({ "id": older["id"] }));
        assert_eq!(h.state.read().unwrap().config.trainers.get(&appid).map(String::as_str), Some(old.to_string_lossy().as_ref()));
        assert_eq!(h.state.read().unwrap().games[gi].trainer_name, "Game v1.0-v1.2 Trainer.exe");
        // letting the one in Downloads go only forgets it; letting Gantry's copy go deletes the copy and detaches it
        let mine_id = shelf().iter().find(|t| t["current"] == false).unwrap()["id"].clone();
        api(&h, "/api/trainer_forget", json!({ "id": mine_id }));
        assert!(mine.is_file(), "a file Gantry does not own stays");
        api(&h, "/api/trainer_forget", json!({ "id": older["id"] }));
        assert!(!old.exists() && shelf().is_empty());
        assert!(!h.state.read().unwrap().config.trainers.contains_key(&appid) && h.state.read().unwrap().games[gi].trainer_name.is_empty());
        api(&h, "/api/trainer_use", json!({ "id": "../../etc/passwd" }));
        assert!(!h.state.read().unwrap().config.trainers.contains_key(&appid), "only kept trainers can be picked");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_started_trainer_is_seen_running_in_the_game_prefix_or_its_closing_is_explained() {
        use std::os::unix::fs::PermissionsExt;
        let h = harness("trainerwatch");
        add_plain_game(&h);
        let tag = std::process::id();
        let pfx = h._dir.join("compat/pfx");
        let exe = h._dir.join("game/Watch.exe");
        fs::create_dir_all(exe.parent().unwrap()).unwrap();
        // the game's executable says it is version 1.5
        let mut bin = b"MZ".to_vec();
        bin.extend_from_slice(&[0xBD, 0x04, 0xEF, 0xFE, 0, 0, 1, 0, 0, 0, 1, 0, 0, 0, 0, 0]);
        bin.extend_from_slice(&((1u32 << 16) | 5).to_le_bytes());
        bin.extend_from_slice(&0u32.to_le_bytes());
        fs::write(&exe, bin).unwrap();
        let ct = h._dir.join("ct");
        let proton = ct.join("Fake/proton");
        fs::create_dir_all(proton.parent().unwrap()).unwrap();
        let appid = "4343".to_string();
        let start_with = |script: &str, trainer_name: &str| {
            fs::write(&proton, format!("#!/bin/sh\n{script}\n")).unwrap();
            fs::set_permissions(&proton, fs::Permissions::from_mode(0o755)).unwrap();
            let trainer = h._dir.join("t").join(trainer_name);
            fs::create_dir_all(trainer.parent().unwrap()).unwrap();
            fs::write(&trainer, b"MZ").unwrap();
            {
                let mut st = h.state.write().unwrap();
                let i = st.selected_idx;
                st.games[i].appid = appid.clone();
                st.games[i].pfx = pfx.to_string_lossy().to_string();
                st.games[i].shipping_exe = exe.to_string_lossy().to_string();
                st.tools_dirs = Some(vec![ct.clone()]);
                st.steam_env = SteamEnv { config_vdf: Some(h._dir.join("no-config.vdf")), localconfig: None, running: Some(false), libraries: Some(vec![]) };
                st.config.proton_choice.insert(appid.clone(), proton.to_string_lossy().to_string());
                st.config.trainers.insert(appid.clone(), trainer.to_string_lossy().to_string());
                st.trainer_run = None;
            }
            api(&h, "/api/trainer", json!({}));
            wait_until("trainer checked", || {
                let r = &api(&h, "/api/state", json!({}))["trainer_run"];
                r["file"] == trainer_name && r["state"] != "starting"
            });
            api(&h, "/api/state", json!({}))["trainer_run"].clone()
        };
        let logs = |h: &Harness| h.state.read().unwrap().logs.iter().map(|l| l.message.clone()).collect::<Vec<_>>().join("\n");
        // Proton starts the trainer as a process of its own, inside the prefix it was given
        let name = format!("Watch{tag} v1.0-v1.2 Trainer.exe");
        let run = start_with("sh -c 'sleep 4; :' \"$2\" &\nwait", &name);
        assert_eq!(run["state"], "attached", "{run}");
        assert_eq!(run["alive"], true);
        let said = logs(&h);
        assert!(said.contains("Wine session"), "{said}");
        assert!(said.contains("made for") && said.contains("1.0–1.2") && said.contains("1.5"), "the version is checked against the name: {said}");
        // a trainer that closes at once is reported with what it printed
        let name = format!("Quit{tag}.exe");
        let run = start_with("echo 'err:module:import_dll Library MSVCP140.dll not found'; sleep 1.2; exit 3", &name);
        assert_eq!(run["state"], "closed", "{run}");
        let said = logs(&h);
        assert!(said.contains("The trainer closed") && said.contains("MSVCP140") && said.contains("vcrun2022"), "{said}");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_proxy_dll_mod_without_the_steam_launch_option_is_reported_even_when_there_is_no_loader() {
        if fsutil::find_in_path("bsdtar").is_none() {
            eprintln!("skipping: bsdtar missing");
            return;
        }
        let h = harness("proxyhealth");
        add_plain_game(&h);
        {
            let mut st = h.state.write().unwrap();
            let i = st.selected_idx;
            st.games[i].appid = "42".into();
        }
        let lc = h._dir.join("steam/userdata/1/config/localconfig.vdf");
        fs::create_dir_all(lc.parent().unwrap()).unwrap();
        fs::write(&lc, LOCALCONFIG).unwrap();
        h.state.write().unwrap().steam_env = SteamEnv { config_vdf: None, localconfig: Some(lc), running: Some(false), libraries: Some(vec![]) };
        let issues = || -> Vec<String> {
            let (_, body) = http(&h, "GET", "/api/health", &[("X-Rig-Token", &h.token)], b"");
            serde_json::from_str::<Value>(&body).unwrap()["issues"].as_array().unwrap().iter().map(|i| i["code"].as_str().unwrap_or("").to_string()).collect()
        };
        assert!(!issues().contains(&"launch_option".to_string()), "nothing deployed yet: nothing to report");
        let zip = zip_of(&h, "cet", &[("Cool Pack-77-1-3-1700000000/Data/new.esp", "NEW"), ("Cool Pack-77-1-3-1700000000/version.dll", "DLL")]);
        let (code, _) = http(&h, "POST", "/api/drop_upload?name=Cool%20Pack-77-1-3-1700000000.zip", &[("X-Rig-Token", &h.token)], &zip);
        assert_eq!(code, 200);
        wait_until("mod deployed", || library_of_state(&h)["deployed"] == 2);
        assert!(issues().contains(&"launch_option".to_string()), "version.dll needs Wine's override and Steam does not have it: {:?}", issues());
        api(&h, "/api/steam_launch_apply", json!({}));
        wait_until("options applied", || !issues().contains(&"launch_option".to_string()));
    }

    #[cfg(unix)]
    #[test]
    fn an_archive_that_cannot_fit_is_refused_before_anything_is_unpacked() {
        let d = fsutil::testutil::TempDir::new("room");
        let small = d.join("small.zip");
        fs::write(&small, b"PK").unwrap();
        assert!(ensure_room(&small, &d.join("")).is_ok());
        // a file of 64 TB (sparse: it takes no room) cannot be unpacked anywhere
        let huge = d.join("huge.zip");
        if fs::File::create(&huge).and_then(|f| f.set_len(1 << 46)).is_err() {
            return; // a file system without sparse files cannot show this
        }
        let e = ensure_room(&huge, &d.join("")).unwrap_err();
        assert!(e.starts_with("Not enough free space for huge.zip") && !e.contains("TB") && e.contains("GB"), "{e}");
        assert!(fsutil::free_bytes(&d.join("")).unwrap_or(0) > 0);
        assert_eq!(fsutil::size_text(5 << 20), "5 MB");
        assert_eq!(fsutil::size_text(3 << 30), "3.0 GB");
    }

    #[test]
    fn several_mods_that_need_a_guess_each_get_their_question_in_turn() {
        if fsutil::find_in_path("bsdtar").is_none() {
            eprintln!("skipping: bsdtar missing");
            return;
        }
        let h = harness("previewqueue");
        let dir = add_plain_game(&h);
        fs::create_dir_all(dir.join("VEHICLES")).unwrap();
        for n in ["VEH_1_HI.BNDL", "VEH_1_LO.BNDL", "VEH_2_HI.BNDL", "VEH_2_LO.BNDL"] {
            fs::write(dir.join("VEHICLES").join(n), "ORIGINAL").unwrap();
        }
        let state = || api(&h, "/api/state", json!({}));
        let mods = || state()["library"]["mods"].as_array().map(|m| m.len()).unwrap_or(0);
        let zip = |tag: &str, a: &str, b: &str| {
            let p = h._dir.join(&format!("{tag}-mod.zip"));
            fs::write(&p, zip_of(&h, tag, &[(&format!("Replace/{a}"), "NEW"), (&format!("Replace/{b}"), "NEW")])).unwrap();
            p
        };
        install_to_library_from_path(zip("a", "VEH_1_HI.BNDL", "VEH_1_LO.BNDL"), false, Arc::clone(&h.state));
        wait_until("the first question", || !state()["pending_install"].is_null());
        install_to_library_from_path(zip("b", "VEH_2_HI.BNDL", "VEH_2_LO.BNDL"), false, Arc::clone(&h.state));
        wait_until("the second waits behind it", || state()["pending_install"]["more"] == 1);
        assert_eq!(mods(), 0, "nothing is placed on a guess without an answer");
        api(&h, "/api/install_confirm", json!({ "accept": true }));
        wait_until("the first was added and the second asks", || mods() == 1 && !state()["pending_install"].is_null() && state()["pending_install"]["more"] == 0);
        api(&h, "/api/install_confirm", json!({ "accept": false }));
        wait_until("answered", || state()["pending_install"].is_null());
        assert_eq!(mods(), 1, "the one that was refused was not added");
        assert_eq!(fs::read_to_string(dir.join("VEHICLES/VEH_2_HI.BNDL")).unwrap(), "ORIGINAL");
    }

    #[test]
    fn an_answer_about_where_a_mod_goes_is_remembered_for_the_next_one_like_it() {
        if fsutil::find_in_path("bsdtar").is_none() {
            eprintln!("skipping: bsdtar missing");
            return;
        }
        let h = harness("placelearn");
        let dir = add_plain_game(&h);
        fs::create_dir_all(dir.join("VEHICLES")).unwrap();
        for n in ["VEH_1_HI.BNDL", "VEH_2_HI.BNDL", "VEH_3_HI.BNDL"] {
            fs::write(dir.join("VEHICLES").join(n), "ORIGINAL").unwrap();
        }
        let state = || api(&h, "/api/state", json!({}));
        let mods = || state()["library"]["mods"].as_array().map(|m| m.len()).unwrap_or(0);
        let zip = |tag: &str, a: &str| {
            let p = h._dir.join(&format!("{tag}-mod.zip"));
            fs::write(&p, zip_of(&h, tag, &[(&format!("Replace/{a}"), "NEW")])).unwrap();
            p
        };
        install_to_library_from_path(zip("a", "VEH_1_HI.BNDL"), false, Arc::clone(&h.state));
        wait_until("the question", || !state()["pending_install"].is_null());
        api(&h, "/api/install_confirm", json!({ "accept": true }));
        wait_until("the first is in", || mods() == 1);
        assert_eq!(state()["place_rules"], 1);
        // the next one shaped the same goes in without a question
        install_to_library_from_path(zip("b", "VEH_2_HI.BNDL"), false, Arc::clone(&h.state));
        wait_until("the second is in by itself", || mods() == 2);
        assert!(state()["pending_install"].is_null());
        // a folder chosen by hand is remembered as that folder, and forgetting asks again
        api(&h, "/api/place_rules_clear", json!({}));
        assert_eq!(state()["place_rules"], 0);
        install_to_library_from_path(zip("c", "VEH_3_HI.BNDL"), false, Arc::clone(&h.state));
        wait_until("asked again", || !state()["pending_install"].is_null());
        api(&h, "/api/install_confirm", json!({ "accept": true, "target": "Extra" }));
        wait_until("the third is in", || mods() == 3);
        let rules = h.state.read().unwrap().config.place_rules.values().flat_map(|r| r.values().cloned()).collect::<Vec<_>>();
        assert_eq!(rules, vec!["dir:Extra".to_string()]);
    }

    #[test]
    fn mods_added_together_go_in_one_after_another_with_one_summary() {
        if fsutil::find_in_path("bsdtar").is_none() {
            eprintln!("skipping: bsdtar missing");
            return;
        }
        let h = harness("batch");
        add_plain_game(&h);
        let state = || api(&h, "/api/state", json!({}));
        // hold the worker until all four are queued, or a quick machine finishes the first ones and starts a new batch
        h.state.write().unwrap().install_worker = true;
        for i in 0..3 {
            let zip = zip_of(&h, &format!("m{i}"), &[(&format!("Data/mod{i}.esp"), "X")]);
            assert_eq!(http(&h, "POST", &format!("/api/drop_upload?name=Mod%20{i}.zip"), &[("X-Rig-Token", &h.token)], &zip).0, 200);
        }
        assert_eq!(http(&h, "POST", "/api/drop_upload?name=broken.zip", &[("X-Rig-Token", &h.token)], b"this is not a zip").0, 200);
        let worker = Arc::clone(&h.state);
        thread::spawn(move || install_worker(&worker));
        wait_until("the batch is over", || state()["batch"]["finished"] == true);
        let b = state()["batch"].clone();
        assert_eq!((b["total"].as_u64(), b["done"].as_u64(), b["added"].as_u64()), (Some(4), Some(4), Some(3)), "{b}");
        assert_eq!(b["failed"][0][0], "broken.zip", "{b}");
        assert_eq!(state()["library"]["mods"].as_array().map(|m| m.len()), Some(3));
        let said: String = h.state.read().unwrap().logs.iter().map(|l| l.message.clone()).collect::<Vec<_>>().join("\n");
        assert!(said.contains("Mods added: 3 of 4."), "{said}");
        api(&h, "/api/batch_dismiss", json!({}));
        assert!(state()["batch"].is_null());
    }

    #[test]
    fn a_mod_that_needs_a_core_mod_the_game_lacks_is_reported_with_where_to_get_it() {
        if fsutil::find_in_path("bsdtar").is_none() {
            eprintln!("skipping: bsdtar missing");
            return;
        }
        let h = harness("frameworks");
        let dir = add_plain_game(&h);
        fs::create_dir_all(dir.join("bin/x64")).unwrap();
        fs::write(dir.join("bin/x64/Cyberpunk2077.exe"), b"MZ").unwrap();
        let issues = || -> Vec<Value> {
            let (_, body) = http(&h, "GET", "/api/health", &[("X-Rig-Token", &h.token)], b"");
            serde_json::from_str::<Value>(&body).unwrap()["issues"].as_array().unwrap().clone()
        };
        assert!(issues().iter().all(|i| i["code"] != "needs_framework"));
        let zip = zip_of(&h, "xl", &[("archive/pc/mod/Cat.archive", "A"), ("archive/pc/mod/Cat.archive.xl", "X")]);
        let (code, _) = http(&h, "POST", "/api/drop_upload?name=Cat-1-1-0-1700000000.zip", &[("X-Rig-Token", &h.token)], &zip);
        assert_eq!(code, 200);
        wait_until("mod added", || api(&h, "/api/state", json!({}))["library"]["mods"].as_array().map(|m| m.len()) == Some(1));
        let found = issues().into_iter().find(|i| i["code"] == "needs_framework").expect("ArchiveXL is asked for and missing");
        assert_eq!(found["args"][0], "ArchiveXL");
        assert!(found["links"][0]["url"].as_str().unwrap().starts_with("https://www.nexusmods.com/cyberpunk2077/"));
        // once it is in the game the line goes away
        fs::create_dir_all(dir.join("red4ext/plugins/ArchiveXL")).unwrap();
        fs::write(dir.join("red4ext/plugins/ArchiveXL/ArchiveXL.dll"), b"x").unwrap();
        assert!(issues().iter().all(|i| i["code"] != "needs_framework"));
    }

    #[test]
    fn one_click_makes_cyberpunk_moddable_from_the_official_releases() {
        if fsutil::find_in_path("bsdtar").is_none() {
            eprintln!("skipping: bsdtar missing");
            return;
        }
        let h = harness("makemoddable");
        let dir = add_plain_game(&h);
        fs::create_dir_all(dir.join("bin/x64")).unwrap();
        fs::write(dir.join("bin/x64/Cyberpunk2077.exe"), b"MZ").unwrap();
        // CET is already there (put in by hand): it is left alone
        fs::create_dir_all(dir.join("bin/x64/plugins")).unwrap();
        fs::write(dir.join("bin/x64/plugins/cyber_engine_tweaks.asi"), b"mine").unwrap();
        // the releases as they are really laid out, each next to the debug files that must not be taken
        let z = |name: &str, files: &[(&str, &str)]| zip_of(&h, name, files);
        let (base, hits) = github::fake::start_many(vec![
            ("wopss/RED4ext", "v1.30.0", vec![("red4ext-symbols-1.30.0.zip", z("r4s", &[("RED4ext.pdb", "p")])), ("red4ext-1.30.0.zip", z("r4", &[("bin/x64/winmm.dll", "w"), ("red4ext/RED4ext.dll", "r")]))]),
            ("jac3km4/redscript", "v0.5.32", vec![("redscript-v0.5.32-macos.zip", z("rsm", &[("x", "x")])), ("redscript-v0.5.32-windows.zip", z("rs", &[("engine/tools/scc.exe", "s"), ("engine/config/base/scripts.ini", "i")]))]),
            ("psiberx/cp2077-archive-xl", "v1.27.5", vec![("ArchiveXL-1.27.5-pdb.zip", z("axp", &[("a.pdb", "p")])), ("ArchiveXL-1.27.5.zip", z("ax", &[("red4ext/plugins/ArchiveXL/ArchiveXL.dll", "a"), ("r6/scripts/ArchiveXL/ArchiveXL.reds", "s")]))]),
            ("psiberx/cp2077-tweak-xl", "v1.11.5", vec![("TweakXL-1.11.5.zip", z("tx", &[("red4ext/plugins/TweakXL/TweakXL.dll", "t"), ("r6/scripts/TweakXL/TweakXL.reds", "s")]))]),
            ("psiberx/cp2077-codeware", "v1.20.6", vec![("Codeware-1.20.6.zip", z("cw", &[("red4ext/plugins/Codeware/Codeware.dll", "c"), ("r6/scripts/Codeware/Codeware.reds", "s")]))]),
        ]);
        {
            let mut st = h.state.write().unwrap();
            st.github_api = base.clone();
            st.github_allow = Some(format!("{base}/dl/"));
        }
        let plan = || -> Value {
            let (_, body) = http(&h, "GET", "/api/setup_plan", &[("X-Rig-Token", &h.token)], b"");
            serde_json::from_str(&body).unwrap()
        };
        // the dry run says what will happen, and nothing has been fetched for it
        let before = plan();
        let todo: Vec<String> = before["steps"].as_array().unwrap().iter().filter(|s| s["status"] == "todo").map(|s| s["id"].as_str().unwrap().to_string()).collect();
        assert_eq!(todo, vec!["core:RED4ext", "core:Redscript", "core:ArchiveXL", "core:TweakXL", "core:Codeware", "backup"], "{before}");
        assert_eq!(before["steps"][2]["status"], "done", "CET counts as there");
        assert_eq!(before["changes"], 5);
        assert!(hits.lock().unwrap().is_empty(), "a dry run never goes online");
        // the welcome list says the same in one word
        let (_, wl) = http(&h, "GET", "/api/welcome", &[("X-Rig-Token", &h.token)], b"");
        let row = serde_json::from_str::<Value>(&wl).unwrap()["games"].as_array().unwrap().iter().find(|r| r["appid"] == "plain1").cloned().unwrap();
        assert_eq!((row["verdict"].as_str(), row["detail"].as_str()), (Some("needs_core"), Some("5")));

        api(&h, "/api/full_setup", json!({ "appid": "plain1" }));
        wait_until("setup finished", || plan()["run"]["finished"] == true);
        for (f, want) in [("red4ext/RED4ext.dll", "r"), ("bin/x64/winmm.dll", "w"), ("engine/tools/scc.exe", "s"), ("red4ext/plugins/ArchiveXL/ArchiveXL.dll", "a"), ("red4ext/plugins/TweakXL/TweakXL.dll", "t"), ("red4ext/plugins/Codeware/Codeware.dll", "c"), ("r6/scripts/Codeware/Codeware.reds", "s"), ("bin/x64/plugins/cyber_engine_tweaks.asi", "mine")] {
            assert_eq!(fs::read_to_string(dir.join(f)).unwrap_or_default(), want, "{f}");
        }
        assert!(!dir.join("RED4ext.pdb").exists() && !dir.join("x").exists(), "debug and macOS files stay out");
        let run = plan()["run"].clone();
        let status: Vec<(String, String)> = run["steps"].as_array().unwrap().iter().map(|s| (s["id"].as_str().unwrap().to_string(), s["status"].as_str().unwrap().to_string())).collect();
        assert!(status.iter().filter(|(id, _)| id.starts_with("core:")).all(|(_, s)| s == "done"), "{status:?}");
        assert_eq!(run["steps"][0]["detail"], "1.30.0", "the version that went in");
        // they are library mods under their own names, so they can be turned off, updated and removed like any other
        let g = h.state.read().unwrap().games.last().unwrap().clone();
        let names: Vec<String> = library_of(&g).unwrap().mods().into_iter().map(|m| m.name).collect();
        assert_eq!(names, vec!["RED4ext", "Redscript", "ArchiveXL", "TweakXL", "Codeware"]);
        assert!(last_log(&h).contains("Setup complete"), "{}", last_log(&h));
        // the list is green now, and a second run fetches nothing
        assert_eq!(plan()["changes"], 0);
        let n = hits.lock().unwrap().len();
        api(&h, "/api/full_setup", json!({ "appid": "plain1" }));
        wait_until("second run", || h.state.read().unwrap().logs.iter().filter(|l| l.message.contains("Setup complete")).count() == 2);
        assert_eq!(hits.lock().unwrap().len(), n);
        assert_eq!(library_of(&g).unwrap().mods().len(), 5, "nothing twice");
    }

    #[test]
    fn play_is_checked_first_and_fix_and_play_puts_in_what_is_missing() {
        if fsutil::find_in_path("bsdtar").is_none() {
            eprintln!("skipping: bsdtar missing");
            return;
        }
        let h = harness("preflight");
        let dir = add_plain_game(&h);
        fs::create_dir_all(dir.join("bin/x64")).unwrap();
        fs::write(dir.join("bin/x64/Cyberpunk2077.exe"), b"MZ").unwrap();
        let check = || -> Vec<Value> {
            let (_, body) = http(&h, "GET", "/api/preflight", &[("X-Rig-Token", &h.token)], b"");
            let v: Value = serde_json::from_str(&body).unwrap();
            assert!(v["ms"].as_u64().unwrap() < 200, "the check is quick: {v}");
            v["problems"].as_array().unwrap().clone()
        };
        assert!(check().is_empty(), "a game without mods is fine");
        // a mod that needs ArchiveXL, which the game lacks
        let zip = zip_of(&h, "xl", &[("archive/pc/mod/Cat.archive", "A"), ("archive/pc/mod/Cat.archive.xl", "X")]);
        assert_eq!(http(&h, "POST", "/api/drop_upload?name=Cat-1-1-0-1700000000.zip", &[("X-Rig-Token", &h.token)], &zip).0, 200);
        wait_until("mod added", || api(&h, "/api/state", json!({}))["library"]["mods"].as_array().map(|m| m.len()) == Some(1));
        let found = check();
        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!((found[0]["code"].as_str(), found[0]["fix"].as_str(), found[0]["args"][0].as_str()), (Some("needs_framework"), Some("setup"), Some("ArchiveXL")));
        let ax = zip_of(&h, "ax", &[("red4ext/plugins/ArchiveXL/ArchiveXL.dll", "a")]);
        let r4 = zip_of(&h, "r4", &[("red4ext/RED4ext.dll", "r"), ("bin/x64/winmm.dll", "w")]);
        let (base, hits) = github::fake::start_many(vec![("psiberx/cp2077-archive-xl", "v1.27.5", vec![("ArchiveXL-1.27.5.zip", ax)]), ("wopss/RED4ext", "v1.30.0", vec![("red4ext-1.30.0.zip", r4)])]);
        {
            let mut st = h.state.write().unwrap();
            st.github_api = base.clone();
            st.github_allow = Some(format!("{base}/dl/"));
        }
        api(&h, "/api/play_fixed", json!({ "fixes": ["setup"] }));
        wait_until("ArchiveXL in, then played", || dir.join("red4ext/plugins/ArchiveXL/ArchiveXL.dll").is_file() && h.state.read().unwrap().logs.iter().any(|l| l.message.contains("Launching Plain Game")));
        assert!(dir.join("red4ext/RED4ext.dll").is_file(), "ArchiveXL needs RED4ext, so that came too");
        assert!(hits.lock().unwrap().iter().all(|u| !u.contains("tweak") && !u.contains("codeware")), "only what the mods need is fetched: {:?}", hits.lock().unwrap());
        assert!(check().is_empty(), "nothing left to fix");
        // a game with anti-cheat and mods in it: the fix is to play without them
        fs::write(dir.join("EasyAntiCheat_EOS.sys"), b"x").unwrap();
        fs::create_dir_all(dir.join("EasyAntiCheat")).unwrap();
        if deploy::anti_cheat(&dir).is_some() {
            let p = check();
            assert_eq!((p[0]["code"].as_str(), p[0]["fix"].as_str()), (Some("anti_cheat"), Some("vanilla")), "{p:?}");
        }
    }

    #[test]
    fn a_core_mod_that_cannot_be_had_does_not_stop_the_others() {
        if fsutil::find_in_path("bsdtar").is_none() {
            eprintln!("skipping: bsdtar missing");
            return;
        }
        let h = harness("makemoddable2");
        let dir = add_plain_game(&h);
        fs::create_dir_all(dir.join("bin/x64")).unwrap();
        fs::write(dir.join("bin/x64/Cyberpunk2077.exe"), b"MZ").unwrap();
        // only RED4ext is published (and Codeware's release has nothing usable); GitHub knows none of the others
        let r4 = zip_of(&h, "r4", &[("red4ext/RED4ext.dll", "r")]);
        let (base, _) = github::fake::start_many(vec![("wopss/RED4ext", "v1.30.0", vec![("red4ext-1.30.0.zip", r4)]), ("psiberx/cp2077-codeware", "v1.20.6", vec![("Codeware-1.20.6-pdb.zip", vec![1])])]);
        {
            let mut st = h.state.write().unwrap();
            st.github_api = base.clone();
            st.github_allow = Some(format!("{base}/dl/"));
        }
        api(&h, "/api/full_setup", json!({ "appid": "plain1" }));
        wait_until("finished", || h.state.read().unwrap().setup_run.as_ref().map(|r| r.finished).unwrap_or(false));
        assert!(dir.join("red4ext/RED4ext.dll").is_file());
        let run = h.state.read().unwrap().setup_run.clone().unwrap();
        let of = |id: &str| run.steps.iter().find(|s| s.id == id).unwrap().clone();
        assert_eq!(of("core:RED4ext").status, "done");
        assert_eq!(of("core:TweakXL").status, "failed");
        assert!(of("core:Codeware").detail.contains("no file ModForge recognises"), "{:?}", of("core:Codeware"));
        assert!(last_log(&h).contains("open item"), "{}", last_log(&h));
    }

    #[test]
    fn a_mod_shows_the_core_mods_it_needs_and_one_click_gets_one_with_what_it_needs_itself() {
        if fsutil::find_in_path("bsdtar").is_none() {
            eprintln!("skipping: bsdtar missing");
            return;
        }
        let h = harness("coreneeds");
        let dir = add_plain_game(&h);
        fs::create_dir_all(dir.join("bin/x64")).unwrap();
        fs::write(dir.join("bin/x64/Cyberpunk2077.exe"), b"MZ").unwrap();
        let z = |name: &str, files: &[(&str, &str)]| zip_of(&h, name, files);
        let (base, _) = github::fake::start_many(vec![
            ("wopss/RED4ext", "v1.30.0", vec![("red4ext-1.30.0.zip", z("r4", &[("red4ext/RED4ext.dll", "r")]))]),
            ("psiberx/cp2077-archive-xl", "v1.27.5", vec![("ArchiveXL-1.27.5.zip", z("ax", &[("red4ext/plugins/ArchiveXL/ArchiveXL.dll", "a")]))]),
            ("psiberx/cp2077-tweak-xl", "v1.11.5", vec![("TweakXL-1.11.5.zip", z("tx", &[("red4ext/plugins/TweakXL/TweakXL.dll", "t")]))]),
        ]);
        {
            let mut st = h.state.write().unwrap();
            st.github_api = base.clone();
            st.github_allow = Some(format!("{base}/dl/"));
        }
        // a clothing mod that needs ArchiveXL and TweakXL, and a plain one that needs nothing
        let g = selected_game(&h.state).unwrap();
        let lib = library_of(&g).unwrap();
        for (n, files) in [("Neon Jacket", vec!["archive/pc/mod/jacket.archive", "archive/pc/mod/jacket.archive.xl", "r6/tweaks/jacket.yaml"]), ("Plain Archive", vec!["archive/pc/mod/plain.archive"])] {
            let s = h._dir.join(&format!("stage-{n}"));
            for f in files {
                fs::create_dir_all(s.join(f).parent().unwrap()).unwrap();
                fs::write(s.join(f), "x").unwrap();
            }
            lib.add_with(&s, n, &format!("{n}.zip"), None, "1").unwrap();
        }
        refresh_mods_shared(&h.state);
        let needs = || api(&h, "/api/state", json!({}))["library"]["core_needs"].clone();
        let jacket = lib.mods().iter().find(|m| m.name == "Neon Jacket").unwrap().id.clone();
        let names = |v: &Value| v.as_array().map(|a| a.iter().map(|x| x["name"].as_str().unwrap().to_string()).collect::<Vec<_>>()).unwrap_or_default();
        let n = needs();
        assert_eq!(names(&n[&jacket]), vec!["ArchiveXL", "TweakXL"], "{n}");
        assert!(n[&jacket][0]["url"].as_str().unwrap().starts_with("https://www.nexusmods.com/"));
        assert_eq!(n.as_object().unwrap().len(), 1, "the plain mod needs nothing: {n}");
        // one click on ArchiveXL brings RED4ext along (ArchiveXL needs it), and leaves TweakXL for the user to decide
        api(&h, "/api/core_install", json!({ "name": "ArchiveXL" }));
        wait_until("ArchiveXL and RED4ext are in", || dir.join("red4ext/plugins/ArchiveXL/ArchiveXL.dll").is_file() && dir.join("red4ext/RED4ext.dll").is_file());
        wait_until("the list follows", || names(&needs()[&jacket]) == vec!["TweakXL"]);
        // ArchiveXL and RED4ext are core mods, the jacket is not
        wait_until("both core mods are marked", || api(&h, "/api/state", json!({}))["library"]["core_ids"].as_array().map(|a| a.len()) == Some(2));
        assert!(!dir.join("red4ext/plugins/TweakXL").exists());
        api(&h, "/api/core_install", json!({ "name": "Not A Core" }));
        assert!(last_log(&h).contains("does not know that core mod"));
    }

    #[test]
    fn conflicts_inside_archives_and_scripts_are_shown_with_who_wins() {
        let h = harness("deepclash");
        let dir = add_plain_game(&h);
        fs::create_dir_all(dir.join("bin/x64")).unwrap();
        fs::write(dir.join("bin/x64/Cyberpunk2077.exe"), b"MZ").unwrap();
        fs::create_dir_all(dir.join("archive/pc/mod")).unwrap();
        let g = selected_game(&h.state).unwrap();
        let lib = library_of(&g).unwrap();
        let add = |n: &str, files: Vec<(&str, Vec<u8>)>| {
            let s = h._dir.join(&format!("stage-{n}"));
            for (f, b) in files {
                fs::create_dir_all(s.join(f).parent().unwrap()).unwrap();
                fs::write(s.join(f), b).unwrap();
            }
            lib.add_with(&s, n, &format!("{n}.zip"), None, "1").unwrap();
        };
        add("Judy Stays", vec![("archive/pc/mod/#Stay_Judy.archive", clashes::testutil::archive(&[7, 8])), ("r6/scripts/j.reds", b"class StayHelper {}".to_vec()), ("shared.txt", b"J".to_vec())]);
        add("Panam Stays", vec![("archive/pc/mod/#Stay_Panam.archive", clashes::testutil::archive(&[8, 9])), ("r6/scripts/p.reds", b"class StayHelper {}".to_vec()), ("shared.txt", b"P".to_vec())]);
        refresh_mods_shared(&h.state);
        let (issues, _) = game_issues(&h.state, &g, false);
        let codes: Vec<&str> = issues.iter().filter_map(|i| i["code"].as_str()).collect();
        assert!(codes.contains(&"archive_clash") && codes.contains(&"script_clash"), "{codes:?}");
        let (_, body) = http(&h, "GET", "/api/conflict_view", &[("X-Rig-Token", &h.token)], b"");
        let v: Value = serde_json::from_str(&body).unwrap();
        assert_eq!((v["archives"][0]["winner_mod"].as_str(), v["archives"][0]["resources"].as_u64()), (Some("Judy Stays"), Some(1)), "{}", v["archives"]);
        assert_eq!(v["scripts"][0]["what"], "StayHelper");
        // the shared plain file: the later mod wins it, and each row says so
        let mods = api(&h, "/api/state", json!({}))["library"]["mods"].clone();
        let row = |n: &str| mods.as_array().unwrap().iter().find(|m| m["name"] == n).unwrap().clone();
        assert_eq!(row("Panam Stays")["wins"][0], json!({ "mod": "Judy Stays", "files": 1 }));
        assert_eq!(row("Judy Stays")["loses"][0], json!({ "mod": "Panam Stays", "files": 1 }));
        // a game without archives or scripts is not read at all
        assert!(clash_issues(&h._dir.join("nowhere"), &lib).is_empty());
    }

    #[test]
    fn a_mod_page_lists_the_core_mods_it_needs_and_the_mods_that_need_it() {
        let h = harness("needgraph");
        let dir = add_plain_game(&h);
        fs::create_dir_all(dir.join("bin/x64")).unwrap();
        fs::write(dir.join("bin/x64/Cyberpunk2077.exe"), b"MZ").unwrap();
        let g = selected_game(&h.state).unwrap();
        let lib = library_of(&g).unwrap();
        let add = |n: &str, files: &[&str]| {
            let s = h._dir.join(&format!("stage-{n}"));
            for f in files {
                fs::create_dir_all(s.join(f).parent().unwrap()).unwrap();
                fs::write(s.join(f), b"x").unwrap();
            }
            lib.add_with(&s, n, &format!("{n}.zip"), None, "1").unwrap();
        };
        add("RED4ext", &["red4ext/RED4ext.dll", "bin/x64/winmm.dll"]);
        add("Cool Plugin", &["red4ext/plugins/Cool/cool.dll", "archive/pc/mod/cool.archive.xl"]);
        add("Plain", &["archive/pc/mod/plain.archive"]);
        lib.deploy(false).unwrap();
        refresh_mods_shared(&h.state);
        let id = |n: &str| lib.mods().iter().find(|m| m.name == n).unwrap().id.clone();
        let page = |n: &str| -> Value {
            let (code, body) = http(&h, "GET", &format!("/api/mod_page?id={}", id(n)), &[("X-Rig-Token", &h.token)], b"");
            assert_eq!(code, 200, "{body}");
            serde_json::from_str(&body).unwrap()
        };
        let cool = page("Cool Plugin");
        let reqs: Vec<(String, bool, bool)> = cool["requirements"].as_array().unwrap().iter().map(|r| (r["name"].as_str().unwrap().to_string(), r["installed"].as_bool().unwrap(), r["core"].as_bool().unwrap())).collect();
        assert_eq!(reqs, vec![("RED4ext".to_string(), true, true), ("ArchiveXL".to_string(), false, true)], "{cool}");
        assert!(cool["needed_by"].as_array().unwrap().is_empty());
        let red = page("RED4ext");
        let by: Vec<(&str, &str)> = red["needed_by"].as_array().unwrap().iter().map(|x| (x["name"].as_str().unwrap(), x["via"].as_str().unwrap())).collect();
        assert_eq!(by, vec![("Cool Plugin", "RED4ext")], "the plain mod does not stand on it");
        assert!(page("Plain")["requirements"].as_array().unwrap().is_empty());
    }

    #[test]
    fn a_game_update_is_noticed_before_play_when_mods_run_code_in_it() {
        let h = harness("gameupd");
        let dir = add_plain_game(&h);
        let g = selected_game(&h.state).unwrap();
        let lib = library_of(&g).unwrap();
        let stage = h._dir.join("stage-plugin");
        fs::create_dir_all(&stage).unwrap();
        fs::write(stage.join("Hook.dll"), b"x").unwrap();
        lib.add_with(&stage, "Hook Plugin", "hook.zip", None, "1").unwrap();
        refresh_mods_shared(&h.state);
        let codes = |h: &Harness| -> Vec<String> { preflight(&h.state, &g).iter().chain(game_issues(&h.state, &g, false).0.iter()).filter_map(|i| i["code"].as_str().map(str::to_string)).collect() };
        // the first build seen is taken as it is
        assert!(!codes(&h).contains(&"game_updated".to_string()));
        assert!(h.state.read().unwrap().config.game_builds.contains_key(&g.appid));
        // the game changes: both checks say so, with the mod that runs code in it
        fs::write(dir.join("plain.exe"), b"MZ patched").unwrap();
        let pre = preflight(&h.state, &g);
        let i = pre.iter().find(|i| i["code"] == "game_updated").expect("noticed before play");
        assert_eq!(i["args"][1], "Hook Plugin");
        assert_eq!(codes(&h).iter().filter(|c| *c == "game_updated").count(), 2);
        // "they work" accepts the new build
        api(&h, "/api/game_update_ok", json!({}));
        assert!(!codes(&h).contains(&"game_updated".to_string()));
        // a change while no mod runs code in the game is taken quietly
        api(&h, "/api/lib_toggle", json!({ "id": lib.mods()[0].id, "enabled": false }));
        wait_until("the mod is off", || !lib.mods()[0].enabled);
        fs::write(dir.join("plain.exe"), b"MZ patched again").unwrap();
        assert!(!codes(&h).contains(&"game_updated".to_string()));
        api(&h, "/api/lib_toggle", json!({ "id": lib.mods()[0].id, "enabled": true }));
        wait_until("the mod is on", || lib.mods()[0].enabled);
        assert!(!codes(&h).contains(&"game_updated".to_string()), "that build was accepted");
    }

    #[test]
    fn the_same_mod_installed_twice_is_pointed_out() {
        let h = harness("dupes");
        add_plain_game(&h);
        let g = h.state.read().unwrap().games.last().unwrap().clone();
        let lib = library_of(&g).unwrap();
        for (n, v) in [("Cool Pack", "1.0"), ("Cool Pack", "2.0"), ("Cool Pack Extras", "1.0")] {
            let s = h._dir.join(&format!("dupe-{n}-{v}"));
            fs::create_dir_all(s.join("Data")).unwrap();
            fs::write(s.join(format!("Data/{n}-{v}.esp")), v).unwrap();
            lib.add_with(&s, n, &format!("{n}-{v}.zip"), Some(nexus::NexusRef { game: "coolgame".into(), mod_id: 5, file_id: if v == "1.0" { 10 } else { 12 } }), v).unwrap();
        }
        refresh_mods_shared(&h.state);
        let (issues, _) = game_issues(&h.state, &g, false);
        let d = issues.iter().find(|i| i["code"] == "duplicate_mod").expect("two copies of Nexus mod 5");
        let said = d["args"][0].as_str().unwrap();
        assert!(said.contains("Cool Pack v1.0") && said.contains("Cool Pack v2.0") && !said.contains("Extras"), "another file of the same page is not a copy: {d}");
        assert_eq!(issues.iter().filter(|i| i["code"] == "duplicate_mod").count(), 1);
        // turning one off clears it
        let id = lib.mods()[0].id.clone();
        lib.set_enabled(&id, false).unwrap();
        let (again, _) = game_issues(&h.state, &g, false);
        assert!(again.iter().all(|i| i["code"] != "duplicate_mod"));
    }

    #[test]
    fn finished_mod_files_in_the_downloads_folder_are_offered_once_and_added_on_request() {
        if fsutil::find_in_path("bsdtar").is_none() {
            eprintln!("skipping: bsdtar missing");
            return;
        }
        let h = harness("inbox");
        add_plain_game(&h);
        let dl = h._dir.join("Downloads");
        fs::create_dir_all(&dl).unwrap();
        h.state.write().unwrap().downloads_dir = Some(dl.clone());
        let ago = |p: &Path, secs: u64| {
            let f = fs::File::options().write(true).open(p).unwrap();
            f.set_modified(std::time::SystemTime::now() - Duration::from_secs(secs)).unwrap();
        };
        let put = |name: &str, bytes: &[u8], secs: u64| {
            fs::write(dl.join(name), bytes).unwrap();
            ago(&dl.join(name), secs);
        };
        let zip = zip_of(&h, "dl", &[("Data/from_downloads.esp", "D")]);
        put("Fast Cars 790 2.14 2026-10-21T20-49Z abc.zip", &zip, 120);
        put("Still Coming 5 1.0 2026-10-21T20-49Z abc.zip", &zip, 2);
        put("Ancient-1-1-0-100.zip", &zip, 30 * 86_400);
        put("notes.txt", &[b'x'; 2000], 120);
        put("Unconfirmed 123.crdownload", &[b'x'; 2000], 120);
        put("tiny.zip", b"PK", 120);
        let get = || -> Value {
            let (code, body) = http(&h, "GET", "/api/inbox", &[("X-Rig-Token", &h.token)], b"");
            assert_eq!(code, 200, "{body}");
            serde_json::from_str(&body).unwrap()
        };
        let first = get();
        let names: Vec<&str> = first["items"].as_array().unwrap().iter().map(|i| i["name"].as_str().unwrap()).collect();
        assert_eq!(names, vec!["Fast Cars 790 2.14 2026-10-21T20-49Z abc.zip"], "only a finished, recent mod archive: {first}");
        assert_eq!((first["items"][0]["clean"].as_str(), first["items"][0]["version"].as_str()), (Some("Fast Cars"), Some("2.14")));
        // a name that is not in the list is ignored, whatever it points at
        api(&h, "/api/inbox_add", json!({ "names": ["../../etc/passwd", "notes.txt"] }));
        thread::sleep(Duration::from_millis(300));
        assert_eq!(api(&h, "/api/state", json!({}))["library"]["mods"].as_array().unwrap().len(), 0);
        api(&h, "/api/inbox_add", json!({ "names": ["Fast Cars 790 2.14 2026-10-21T20-49Z abc.zip"] }));
        wait_until("added", || api(&h, "/api/state", json!({}))["library"]["mods"].as_array().map(|m| m.len()) == Some(1));
        assert_eq!(api(&h, "/api/state", json!({}))["library"]["mods"][0]["name"], "Fast Cars");
        assert!(dl.join("Fast Cars 790 2.14 2026-10-21T20-49Z abc.zip").exists(), "the download itself stays where it was");
        wait_until("offered no more", || get()["items"].as_array().unwrap().is_empty());
        // hiding is remembered too
        put("Another 6 1.0 2026-10-21T20-49Z abc.zip", &zip, 120);
        assert_eq!(get()["items"].as_array().unwrap().len(), 1);
        api(&h, "/api/inbox_hide", json!({ "names": ["Another 6 1.0 2026-10-21T20-49Z abc.zip"] }));
        wait_until("hidden", || get()["items"].as_array().unwrap().is_empty());
        assert_eq!(api(&h, "/api/state", json!({}))["library"]["mods"].as_array().unwrap().len(), 1, "hiding adds nothing");
    }

    #[test]
    fn dropped_archive_installs_lua_pak_dll_and_trainer() {
        if fsutil::find_in_path("bsdtar").is_none() && fsutil::find_in_path("7z").is_none() {
            eprintln!("skipping archive test: no bsdtar/7z");
            return;
        }
        let h = harness("droparc");
        let src = h._dir.join("pkg");
        fs::create_dir_all(src.join("SuperMod/Scripts")).unwrap();
        fs::write(src.join("SuperMod/Scripts/main.lua"), "-- lua").unwrap();
        fs::create_dir_all(src.join("Content/Paks/LogicMods")).unwrap();
        fs::write(src.join("Content/Paks/LogicMods/Extra_UI.pak"), b"p").unwrap();
        fs::write(src.join("Content/Paks/LogicMods/Extra_UI.utoc"), b"p").unwrap();
        fs::write(src.join("dxgi.dll"), b"d").unwrap();
        fs::write(src.join("MyTrainer.exe"), b"MZ").unwrap();
        fs::write(src.join("crashreporter.exe"), b"MZ").unwrap();
        let zip = h._dir.join("pkg.zip");
        let ok = if fsutil::find_in_path("bsdtar").is_some() {
            Command::new(tar_tool()).args(["-a", "-cf"]).arg(&zip).arg("-C").arg(&src).arg(".").status().unwrap().success()
        } else {
            Command::new("7z").arg("a").arg(&zip).arg(format!("{}/*", src.display())).stdout(Stdio::null()).status().unwrap().success()
        };
        assert!(ok);
        let bytes = fs::read(&zip).unwrap();
        let (code, _) = http(&h, "POST", "/api/drop_upload?name=pkg.zip", &[("X-Rig-Token", &h.token)], &bytes);
        assert_eq!(code, 200);

        let bin = PathBuf::from(&h.game.bin_dir);
        let gdir = PathBuf::from(&h.game.game_dir);
        wait_until("lua mod", || bin.join("ue4ss/Mods/SuperMod/Scripts/main.lua").is_file());
        wait_until("dll", || bin.join("dxgi.dll").is_file());
        wait_until("logic pak + companion", || {
            gdir.join("Demo/Content/Paks/LogicMods/Extra_UI.pak").is_file()
                && gdir.join("Demo/Content/Paks/LogicMods/Extra_UI.utoc").is_file()
        });
        wait_until("trainer attached", || {
            api(&h, "/api/state", json!({}))["games"][0]["trainer_name"] == "MyTrainer.exe"
        });
        // repair ran afterwards and registered the new mod as enabled
        wait_until("mods.txt updated", || {
            fs::read_to_string(bin.join("ue4ss/Mods/mods.txt")).map(|t| t.contains("SuperMod : 1")).unwrap_or(false)
        });
        // staging dir inside the game folder is gone, trainer stored persistently
        wait_until("stage removed", || {
            !fs::read_dir(&gdir).unwrap().flatten().any(|e| e.file_name().to_string_lossy().starts_with(".rig_stage"))
        });
        assert!(!fs::read_dir(&gdir).unwrap().flatten().any(|e| e.file_name().to_string_lossy().contains("rigtmp")));
        let tr = h.state.read().unwrap().cache_root().join("trainers/42_MyTrainer.exe");
        assert!(tr.is_file());
        assert!(!h.state.read().unwrap().config.trainers.get("42").map(|p| p.contains("crashreporter")).unwrap_or(false));
    }

    #[test]
    fn dropped_full_ue4ss_runtime_is_installed_into_win64() {
        if fsutil::find_in_path("bsdtar").is_none() {
            eprintln!("skipping: bsdtar missing");
            return;
        }
        let h = harness("dropue4ss");
        let bin = PathBuf::from(&h.game.bin_dir);
        fs::remove_dir_all(bin.join("ue4ss")).unwrap();
        let src = h._dir.join("rt");
        fs::create_dir_all(src.join("ue4ss/Mods/Keybinds/Scripts")).unwrap();
        fs::write(src.join("dwmapi.dll"), b"proxy").unwrap();
        fs::write(src.join("ue4ss/UE4SS.dll"), b"core").unwrap();
        fs::write(src.join("ue4ss/UE4SS-settings.ini"), "[General]\nbUseUObjectArrayCache = true\n").unwrap();
        fs::write(src.join("ue4ss/Mods/Keybinds/Scripts/main.lua"), "--").unwrap();
        let zip = h._dir.join("rt.zip");
        assert!(Command::new(tar_tool()).args(["-a", "-cf"]).arg(&zip).arg("-C").arg(&src).arg(".").status().unwrap().success());
        let (code, _) = http(&h, "POST", "/api/drop_upload?name=UE4SS_v3.zip", &[("X-Rig-Token", &h.token)], &fs::read(&zip).unwrap());
        assert_eq!(code, 200);
        wait_until("runtime installed", || bin.join("ue4ss/UE4SS.dll").is_file() && bin.join("dwmapi.dll").is_file());
        wait_until("repair patched ini", || {
            fs::read_to_string(bin.join("ue4ss/UE4SS-settings.ini"))
                .map(|t| t.contains("bUseUObjectArrayCache = false") && t.contains("MinorVersion = 3"))
                .unwrap_or(false)
        });
        wait_until("keybinds mod listed", || mod_enabled(&api(&h, "/api/state", json!({})), "LUA:Keybinds").is_some());
    }

    /// A second, non-Unreal game next to the harness's Unreal fixture, selected.
    fn add_plain_game(h: &Harness) -> PathBuf {
        let dir = h._dir.join("plain/common/Plain Game");
        fs::create_dir_all(dir.join("Data")).unwrap();
        fs::write(dir.join("Data/base.esp"), "ORIGINAL").unwrap();
        fs::write(dir.join("plain.exe"), b"MZ").unwrap();
        let g = GameEntry {
            appid: "plain1".into(),
            name: "Plain Game".into(),
            game_dir: dir.to_string_lossy().to_string(),
            exe_path: dir.join("plain.exe").to_string_lossy().to_string(),
            shipping_exe: String::new(),
            bin_dir: String::new(),
            engine: "Standard / Non-UE".into(),
            mod_count: 0,
            ..h.game.clone()
        };
        let idx = {
            let mut st = h.state.write().unwrap();
            st.games.push(g);
            st.games.len() - 1
        };
        api(h, "/api/select", json!({ "index": idx }));
        dir
    }

    fn zip_of(h: &Harness, name: &str, files: &[(&str, &str)]) -> Vec<u8> {
        let src = h._dir.join(&format!("zipsrc-{name}"));
        for (p, c) in files {
            fs::create_dir_all(src.join(p).parent().unwrap()).unwrap();
            fs::write(src.join(p), c).unwrap();
        }
        let zip = h._dir.join(&format!("{name}.zip"));
        assert!(Command::new(tar_tool()).args(["-a", "-cf"]).arg(&zip).arg("-C").arg(&src).arg(".").status().unwrap().success());
        fs::read(&zip).unwrap()
    }

    fn library_of_state(h: &Harness) -> Value {
        api(h, "/api/state", json!({}))["library"].clone()
    }

    #[test]
    fn a_non_unreal_game_gets_a_mod_library_that_deploys_toggles_and_purges() {
        if fsutil::find_in_path("bsdtar").is_none() {
            eprintln!("skipping: bsdtar missing");
            return;
        }
        let h = harness("libflow");
        let dir = add_plain_game(&h);
        let zip = zip_of(&h, "pack", &[("Cool Pack-77-1-3-1700000000/Data/new.esp", "NEW"), ("Cool Pack-77-1-3-1700000000/winhttp.dll", "DLL")]);
        let (code, _) = http(&h, "POST", "/api/drop_upload?name=Cool%20Pack-77-1-3-1700000000.zip", &[("X-Rig-Token", &h.token)], &zip);
        assert_eq!(code, 200);
        wait_until("mod deployed", || library_of_state(&h)["deployed"] == 2);
        let lib = library_of_state(&h);
        assert_eq!(lib["mods"][0]["name"], "Cool Pack");
        assert_eq!(lib["mods"][0]["version"], "1.3");
        assert_eq!(lib["overrides"], json!(["winhttp"]));
        assert_eq!(fs::read_to_string(dir.join("Data/new.esp")).unwrap(), "NEW");
        assert!(dir.join("winhttp.dll").is_file());

        // the Steam launch options now carry the DLL override the mod needs
        let g = h.state.read().unwrap().games.last().unwrap().clone();
        if cfg!(target_os = "linux") {
            assert!(steam_wanted_with(&g, false, false).contains("WINEDLLOVERRIDES=winhttp=n,b"));
        } else {
            assert_eq!(steam_wanted_with(&g, false, false), "%command%", "Wine's overrides are a Linux thing");
        }

        let id = lib["mods"][0]["id"].as_str().unwrap().to_string();
        api(&h, "/api/lib_toggle", json!({ "id": id, "enabled": false }));
        wait_until("mod switched off", || library_of_state(&h)["deployed"] == 0);
        assert!(!dir.join("Data/new.esp").exists());
        assert_eq!(steam_wanted_with(&g, false, false), "%command%", "no override once the mod is off");

        api(&h, "/api/lib_toggle", json!({ "id": id, "enabled": true }));
        wait_until("mod switched on", || library_of_state(&h)["deployed"] == 2);
        api(&h, "/api/lib_purge", json!({}));
        wait_until("purged", || library_of_state(&h)["deployed"] == 0);
        assert!(!dir.join("winhttp.dll").exists());
        assert_eq!(fs::read_to_string(dir.join("Data/base.esp")).unwrap(), "ORIGINAL");
        assert_eq!(library_of_state(&h)["dirty"], true, "purged but still enabled: needs a deploy");
        api(&h, "/api/lib_deploy", json!({}));
        wait_until("deployed again", || library_of_state(&h)["deployed"] == 2);

        api(&h, "/api/lib_remove", json!({ "id": id }));
        wait_until("mod removed", || library_of_state(&h)["mods"].as_array().map(|m| m.is_empty()).unwrap_or(false));
        assert!(!dir.join("Data/new.esp").exists());
    }

    #[test]
    fn thunderstore_packages_install_with_their_dependencies_and_update_in_place() {
        if fsutil::find_in_path("bsdtar").is_none() {
            eprintln!("skipping: bsdtar missing");
            return;
        }
        let h = harness("thunder");
        let dir = add_plain_game(&h);
        let f = thunderstore::fake::start();
        {
            let mut st = h.state.write().unwrap();
            st.thunder.base = f.base.clone();
            st.thunder.allow_http = true;
        }
        {
            let mut p = f.packages.lock().unwrap();
            p.insert("bbepis-BepInExPack".into(), vec![("5.4.2100".into(), vec![])]);
            p.insert("Own-Cool_Mod".into(), vec![("1.0.0".into(), vec!["bbepis-BepInExPack-5.4.2100".into()])]);
        }
        {
            let mut a = f.archives.lock().unwrap();
            a.insert("bbepis-BepInExPack".into(), zip_of(&h, "bep", &[("BepInExPack/winhttp.dll", "W"), ("BepInExPack/BepInEx/core/BepInEx.dll", "B")]));
            a.insert("Own-Cool_Mod".into(), zip_of(&h, "cool1", &[("manifest.json", "{}"), ("icon.png", "P"), ("CoolMod.dll", "v1")]));
        }
        let st = || library_of_state(&h);
        api(&h, "/api/thunder_install", json!({ "input": "https://thunderstore.io/c/riskofrain2/p/Own/Cool_Mod/" }));
        wait_until("both installed", || st()["mods"].as_array().map(|m| m.len()) == Some(2) && dir.join("BepInEx/plugins/CoolMod.dll").is_file());
        let mods = st()["mods"].clone();
        assert_eq!((mods[0]["name"].as_str(), mods[1]["name"].as_str()), (Some("BepInExPack"), Some("Cool Mod")), "the dependency is loaded first");
        assert_eq!((mods[1]["thunderstore"]["namespace"].as_str(), mods[1]["version"].as_str()), (Some("Own"), Some("1.0.0")));
        assert!(dir.join("winhttp.dll").is_file() && dir.join("BepInEx/core/BepInEx.dll").is_file());
        assert!(!dir.join("manifest.json").exists() && !dir.join("BepInEx/plugins/manifest.json").exists());
        assert_eq!(st()["overrides"], json!(["winhttp"]));

        // nothing newer yet
        api(&h, "/api/nexus_check", json!({}));
        wait_until("checked", || h.state.read().unwrap().logs.iter().any(|l| l.message.contains("All mods are up to date.")));

        // the author publishes 1.1.0
        f.packages.lock().unwrap().get_mut("Own-Cool_Mod").unwrap().push(("1.1.0".into(), vec!["bbepis-BepInExPack-5.4.2100".into()]));
        f.archives.lock().unwrap().insert("Own-Cool_Mod".into(), zip_of(&h, "cool2", &[("manifest.json", "{}"), ("CoolMod.dll", "v2")]));
        api(&h, "/api/nexus_check", json!({}));
        wait_until("update found", || api(&h, "/api/state", json!({}))["thunder_updates"].as_array().map(|u| u.len()) == Some(1));
        let u = api(&h, "/api/state", json!({}))["thunder_updates"][0].clone();
        assert_eq!((u["installed_version"].as_str(), u["new_version"].as_str(), u["name"].as_str()), (Some("1.0.0"), Some("1.1.0"), Some("Cool Mod")));
        // "Update all" does what the single button would
        api(&h, "/api/update_all", json!({}));
        wait_until("updated in place", || fs::read_to_string(dir.join("BepInEx/plugins/CoolMod.dll")).map(|t| t == "v2").unwrap_or(false));
        wait_until("no update left", || api(&h, "/api/state", json!({}))["thunder_updates"].as_array().map(|u| u.is_empty()).unwrap_or(false));
        wait_until("the new version is listed", || st()["mods"][1]["version"].as_str() == Some("1.1.0"));
        let mods = st()["mods"].clone();
        assert_eq!(mods.as_array().unwrap().len(), 2, "replaced, not added; BepInEx was not fetched again");
        assert_eq!((mods[1]["name"].as_str(), mods[1]["version"].as_str()), (Some("Cool Mod"), Some("1.1.0")));
        assert_eq!(f.requests.lock().unwrap().iter().filter(|r| r.contains("download/bbepis")).count(), 1);

        // bad input is explained, not fatal
        api(&h, "/api/thunder_install", json!({ "input": "not a package" }));
        wait_until("refused", || last_log(&h).contains("does not look like a Thunderstore package"));
        api(&h, "/api/thunder_install", json!({ "input": "Nobody-Nothing" }));
        wait_until("unknown", || last_log(&h).contains("does not know that package"));
    }

    #[test]
    fn library_profiles_and_undo_work_over_http() {
        let h = harness("libprof");
        let dir = add_plain_game(&h);
        let g = h.state.read().unwrap().games.last().unwrap().clone();
        let lib = library_of(&g).unwrap();
        for (n, f) in [("One", "one.esp"), ("Two", "two.esp")] {
            let stage = h._dir.join(&format!("st-{n}"));
            fs::create_dir_all(stage.join("Data")).unwrap();
            fs::write(stage.join("Data").join(f), n).unwrap();
            lib.add_from_dir(&stage, n, &format!("{n}.zip")).unwrap();
        }
        lib.deploy(false).unwrap();
        refresh_mods_shared(&h.state);
        let st = || library_of_state(&h);
        api(&h, "/api/lib_profile_save", json!({ "name": "Both" }));
        wait_until("profile saved", || st()["profiles"] == json!(["Both"]));

        let id_one = st()["mods"][0]["id"].as_str().unwrap().to_string();
        api(&h, "/api/lib_toggle", json!({ "id": id_one, "enabled": false }));
        wait_until("one is off", || !dir.join("Data/one.esp").exists() && st()["undo"] == "Disabled One");

        api(&h, "/api/lib_undo", json!({}));
        wait_until("undone and redeployed", || dir.join("Data/one.esp").exists() && st()["undo"].is_null());

        api(&h, "/api/lib_toggle", json!({ "id": id_one, "enabled": false }));
        wait_until("off again", || !dir.join("Data/one.esp").exists());
        api(&h, "/api/lib_profile_load", json!({ "name": "Both" }));
        wait_until("profile applied", || dir.join("Data/one.esp").exists() && st()["undo"] == "Loaded profile 'Both'");

        api(&h, "/api/lib_profile_load", json!({ "name": "Missing" }));
        wait_until("refusal logged", || last_log(&h).contains("No such profile."));
        api(&h, "/api/lib_profile_delete", json!({ "name": "Both" }));
        wait_until("profile gone", || st()["profiles"] == json!([]));
        api(&h, "/api/lib_profile_save", json!({ "name": "   " }));
        wait_until("empty name refused", || last_log(&h).contains("Give the profile a name."));
    }

    #[test]
    fn a_profile_brings_back_how_the_game_starts_and_its_paths_stay_on_this_computer() {
        let h = harness("proflaunch");
        let dir = add_plain_game(&h);
        let g = selected_game(&h.state).unwrap();
        let lib = library_of(&g).unwrap();
        let stage = h._dir.join("pl-one");
        fs::create_dir_all(stage.join("Data")).unwrap();
        fs::write(stage.join("Data/one.esp"), "1").unwrap();
        lib.add_with(&stage, "One", "one.zip", None, "1").unwrap();
        let trainer = dir.join("Cool Trainer.exe");
        fs::write(&trainer, b"MZ").unwrap();
        h.state.write().unwrap().config.trainers.insert(g.appid.clone(), trainer.to_string_lossy().to_string());
        api(&h, "/api/launch_options", json!({ "launch_args": "-skipintro", "env_vars": "DXVK_HUD=fps" }));
        api(&h, "/api/lib_profile_save", json!({ "name": "Night" }));
        wait_until("saved with its launch", || lib.profile_launch("Night").map(|l| l.launch_args == "-skipintro" && l.trainer.ends_with("Cool Trainer.exe")).unwrap_or(false));

        // something else later, then the profile again
        api(&h, "/api/launch_options", json!({ "launch_args": "", "env_vars": "" }));
        h.state.write().unwrap().config.trainers.remove(&g.appid);
        api(&h, "/api/lib_profile_load", json!({ "name": "Night" }));
        wait_until("back", || selected_game(&h.state).unwrap().launch_args == "-skipintro");
        let st = h.state.read().unwrap();
        assert_eq!(st.config.launch_options.get(&g.appid).map(|o| o.env_vars.as_str()), Some("DXVK_HUD=fps"));
        assert_eq!(st.config.trainers.get(&g.appid), Some(&trainer.to_string_lossy().to_string()));
        drop(st);
        // a trainer that is gone is not put back, and that is said
        fs::remove_file(&trainer).unwrap();
        h.state.write().unwrap().config.trainers.remove(&g.appid);
        api(&h, "/api/lib_profile_load", json!({ "name": "Night" }));
        wait_until("the warning", || h.state.read().unwrap().logs.iter().any(|l| l.message.contains("no longer on this computer")));
        assert!(!h.state.read().unwrap().config.trainers.contains_key(&g.appid));
        // what goes to a sync folder has the launch options but not the paths
        let bundle = sync_bundle(&h.state).to_string();
        assert!(bundle.contains("-skipintro") && !bundle.contains("Cool Trainer.exe"), "{bundle}");
    }

    #[test]
    fn unreal_games_can_take_pak_mods_into_the_library_while_ue4ss_mods_keep_their_routing() {
        if fsutil::find_in_path("bsdtar").is_none() {
            eprintln!("skipping: bsdtar missing");
            return;
        }
        let h = harness("uefirst");
        let gdir = PathBuf::from(&h.game.game_dir);
        let paks = ModManager::find_or_create_paks_dir(&gdir);
        refresh_mods_shared(&h.state);
        assert_eq!(library_of_state(&h)["first"], false, "off unless chosen");
        assert_eq!(library_of_state(&h)["unreal"], true);

        // by default a pak goes the usual way: no library involved
        let zip = zip_of(&h, "pak1", &[("Plain_P.pak", "P")]);
        http(&h, "POST", "/api/drop_upload?name=Plain%20Mod.zip", &[("X-Rig-Token", &h.token)], &zip);
        wait_until("usual routing", || paks.join("~mods/Plain_P.pak").is_file());
        assert!(library_of_state(&h)["mods"].as_array().map(|m| m.is_empty()).unwrap_or(true));

        api(&h, "/api/lib_first", json!({ "enabled": true }));
        wait_until("choice stored", || library_of_state(&h)["first"] == true);
        assert!(fs::read_to_string(h._dir.join("cfg/rig_config.json")).unwrap().contains(&h.game.appid));

        let zip = zip_of(&h, "pak2", &[("Lib_P.pak", "L"), ("Lib_P.ucas", "U")]);
        http(&h, "POST", "/api/drop_upload?name=Lib%20Mod-5-1-0-1700000000.zip", &[("X-Rig-Token", &h.token)], &zip);
        wait_until("went to the library", || library_of_state(&h)["mods"].as_array().map(|m| m.len()) == Some(1));
        let rel = paks.strip_prefix(&gdir).unwrap().to_string_lossy().replace('\\', "/");
        assert_eq!(library_of_state(&h)["mods"][0]["target"], format!("{rel}/~mods"));
        wait_until("deployed", || paks.join("~mods/Lib_P.pak").is_file() && paks.join("~mods/Lib_P.ucas").is_file());

        // a Lua mod needs UE4SS: back to the usual routing, the library stays as it is
        let zip = zip_of(&h, "lua", &[("CoolLua/Scripts/main.lua", "print('hi')")]);
        http(&h, "POST", "/api/drop_upload?name=CoolLua.zip", &[("X-Rig-Token", &h.token)], &zip);
        let bin = PathBuf::from(&h.game.bin_dir);
        wait_until("lua mod installed the usual way", || bin.join("ue4ss/Mods/CoolLua/Scripts/main.lua").is_file() || bin.join("Mods/CoolLua/Scripts/main.lua").is_file());
        assert_eq!(library_of_state(&h)["mods"].as_array().unwrap().len(), 1);
        assert!(h.state.read().unwrap().logs.iter().any(|l| l.message.contains("needs UE4SS")));
    }

    #[test]
    fn the_library_is_not_used_by_unreal_games_unless_asked_and_waits_while_the_game_runs() {
        if fsutil::find_in_path("bsdtar").is_none() {
            eprintln!("skipping: bsdtar missing");
            return;
        }
        let h = harness("libue");
        let zip = zip_of(&h, "plainmod", &[("Data/ue_file.txt", "X"), ("readme.txt", "R")]);
        let (code, _) = http(&h, "POST", "/api/drop_upload?name=Plain%20Mod.zip&lib=1", &[("X-Rig-Token", &h.token)], &zip);
        assert_eq!(code, 200);
        wait_until("added to the library", || library_of_state(&h)["mods"].as_array().map(|m| m.len()) == Some(1));
        let gdir = PathBuf::from(&h.game.game_dir);
        wait_until("deployed", || gdir.join("Data/ue_file.txt").is_file());

        // while the game runs nothing is touched; the change is remembered
        h.state.read().unwrap().is_running.store(true, Ordering::SeqCst);
        let id = library_of_state(&h)["mods"][0]["id"].as_str().unwrap().to_string();
        api(&h, "/api/lib_toggle", json!({ "id": id, "enabled": false }));
        wait_until("change is pending", || library_of_state(&h)["dirty"] == true);
        assert!(gdir.join("Data/ue_file.txt").is_file(), "files stay while the game runs");
        api(&h, "/api/lib_purge", json!({}));
        wait_until("refusal logged", || h.state.read().unwrap().logs.iter().any(|l| l.message.contains("Close the game first")));
        assert!(gdir.join("Data/ue_file.txt").is_file());
        h.state.read().unwrap().is_running.store(false, Ordering::SeqCst);
        api(&h, "/api/lib_deploy", json!({}));
        wait_until("applied after the game closed", || !gdir.join("Data/ue_file.txt").exists());
    }

    #[test]
    fn corrupt_archive_reports_error_and_leaves_game_untouched() {
        let h = harness("corrupt");
        let before: Vec<_> = fs::read_dir(&h.game.game_dir).unwrap().flatten().map(|e| e.file_name()).collect();
        let (code, _) = http(&h, "POST", "/api/drop_upload?name=broken.zip", &[("X-Rig-Token", &h.token)], b"this is not a zip");
        assert_eq!(code, 200);
        wait_until("error logged", || {
            h.state.read().unwrap().logs.iter().any(|l| l.level == "ERR" && l.message.contains("broken.zip"))
        });
        wait_until("stage removed", || {
            fs::read_dir(&h.game.game_dir).unwrap().flatten().all(|e| !e.file_name().to_string_lossy().starts_with(".rig_stage"))
        });
        let after: Vec<_> = fs::read_dir(&h.game.game_dir).unwrap().flatten().map(|e| e.file_name()).collect();
        assert_eq!(before.len(), after.len());
    }

    #[test]
    fn wipe_removes_everything_it_installed() {
        let h = harness("wipe");
        api(&h, "/api/self_heal", json!({}));
        // the repair keeps writing after dwmapi.dll is there: wait for all of it
        wait_until("healed", || PathBuf::from(&h.game.bin_dir).join("dwmapi.dll").is_file() && RepairEngine::diagnose(&h.game).issues.iter().all(|i| i.contains("VTableLayout")));
        let st = api(&h, "/api/wipe_mods", json!({}));
        assert!(st["mods"].as_array().unwrap().is_empty(), "{}", st["mods"]);
        let bin = PathBuf::from(&h.game.bin_dir);
        assert!(!bin.join("ue4ss").exists() && !bin.join("dwmapi.dll").exists());
    }

    #[test]
    fn wipe_removes_proxy_dlls_and_library_mods_on_generic_games() {
        let h = harness("wipe-generic");
        let gdir = PathBuf::from(&h.game.game_dir);
        let dll = gdir.join("winhttp.dll");
        fs::write(&dll, b"proxy dll").unwrap();
        let bep = gdir.join("BepInEx/plugins");
        fs::create_dir_all(&bep).unwrap();
        fs::write(bep.join("cool.dll"), b"plugin").unwrap();

        refresh_mods_shared(&h.state);
        let st_before = api(&h, "/api/state", json!({}));
        assert!(st_before["mods"].as_array().unwrap().len() >= 1);

        let st_after = api(&h, "/api/wipe_mods", json!({}));
        assert!(st_after["mods"].as_array().unwrap().is_empty());
        assert!(!dll.exists());
        assert!(!gdir.join("BepInEx").exists());
    }

    #[test]
    fn wipe_and_prune_clears_phantom_needs_and_drift_warnings() {
        let h = harness("wipe-phantom");
        let g = selected_game(&h.state).unwrap();
        // inject a need for a nonexistent mod
        {
            let mut st = h.state.write().unwrap();
            st.nexus.needs.push(NeedRow {
                appid: g.appid.clone(),
                lib_id: "ghost_mod_123".into(),
                name: "Ghost Mod".into(),
                missing: vec![MissingReq { name: "RequiredFramework".into(), page_url: String::new(), notes: String::new() }],
            });
        }
        // Doctor should NOT report needs for ghost mod
        let issues = game_issues(&h.state, &g, false).0;
        assert!(!issues.iter().any(|i| i["code"] == "needs"), "doctor ignores needs for mods not in library");

        // Now prune_updates directly
        prune_updates(&h.state);
        let st = api(&h, "/api/state", json!({}));
        let needs = st["nexus"]["needs"].as_array().unwrap();
        assert!(needs.is_empty(), "prune_updates removed ghost needs");

        // Inject needs and wipe
        {
            let mut st = h.state.write().unwrap();
            st.nexus.needs.push(NeedRow {
                appid: g.appid.clone(),
                lib_id: "another_ghost".into(),
                name: "Another Ghost".into(),
                missing: vec![MissingReq { name: "Another".into(), page_url: String::new(), notes: String::new() }],
            });
        }
        api(&h, "/api/wipe_mods", json!({}));
        let st_after = api(&h, "/api/state", json!({}));
        assert!(st_after["nexus"]["needs"].as_array().unwrap().is_empty(), "wipe_mods clears needs for the wiped game");
    }

    #[test]
    fn file_watcher_detects_external_changes_and_stops_with_state() {
        let h = harness("watch");
        spawn_inotify_watcher(Arc::downgrade(&h.state));
        thread::sleep(Duration::from_millis(900)); // let it register watches
        let pak = PathBuf::from(&h.game.game_dir).join("Demo/Content/Paks/~mods/Skin_P.pak");
        fs::rename(&pak, pak.with_extension("pak.disabled")).unwrap();
        wait_until("watcher notices rename", || mod_enabled(&api(&h, "/api/state", json!({})), "PAK:Skin_P") == Some(false));
        // log churn must not cause refreshes
        let v0 = api(&h, "/api/state", json!({}))["version"].as_u64().unwrap();
        let log = PathBuf::from(&h.game.bin_dir).join("ue4ss/UE4SS.log");
        for i in 0..20 {
            fs::write(&log, format!("line {}", i)).unwrap();
            thread::sleep(Duration::from_millis(20));
        }
        thread::sleep(Duration::from_millis(1200));
        let v1 = api(&h, "/api/state", json!({}))["version"].as_u64().unwrap();
        assert_eq!(v0, v1, "UE4SS.log writes are ignored");
    }

    #[test]
    fn rescan_keeps_selection_on_the_same_game() {
        let (dir, game) = make_game("rescan");
        let lib = dir.join("steamapps");
        // appmanifest for the fixture game plus a second game sorting before it
        fs::write(
            lib.join("appmanifest_42.acf"),
            "\"AppState\"\n{\n\t\"appid\"\t\t\"42\"\n\t\"name\"\t\t\"Zeta Demo\"\n\t\"installdir\"\t\t\"Unreal Engine Demo\"\n}\n",
        )
        .unwrap();
        fs::create_dir_all(lib.join("common/Alpha")).unwrap();
        fs::write(
            lib.join("appmanifest_7.acf"),
            "\"AppState\"\n{\n\t\"appid\"\t\t\"7\"\n\t\"name\"\t\t\"Alpha\"\n\t\"installdir\"\t\t\"Alpha\"\n}\n",
        )
        .unwrap();
        let cache = dir.join("cache");
        let paths = RigPaths {
            custom_games_file: dir.join("cfg/custom.json"),
            config_file: dir.join("cfg/cfg.json"),
            cache_dir: cache.join("injectors"),
            engine_cache_file: cache.join("engine.json"),
            webview_profile_dir: cache.join("wv"),
            hydra_dir: None,
            launchers: Default::default(),
        };
        let cfg = RigConfigData::default();
        let games = RigEngine::scan_libraries(std::slice::from_ref(&lib), &paths, &cfg);
        assert_eq!(games.iter().map(|g| g.name.as_str()).collect::<Vec<_>>(), vec!["Alpha", "Zeta Demo"]);
        let zeta = games.iter().find(|g| g.appid == "42").unwrap();
        assert!(zeta.engine.starts_with("Unreal Engine 5.3"), "{}", zeta.engine);
        assert_eq!(zeta.mod_count, ModManager::list_mods(&game).len());

        let mut st = AppState::with(paths, cfg, games, vec![], Arc::new(EventNotifier::new()));
        assert!(st.select_by_appid("42"));
        assert_eq!(st.selected_idx, 1);
        // a new game sorts in front: selection must follow the appid
        st.games.insert(0, GameEntry { appid: "1".into(), name: "AAA".into(), ..st.games[0].clone() });
        assert!(st.select_by_appid("42"));
        assert_eq!(st.selected_idx, 2);
        assert!(!st.select_by_appid("gone"));
        assert!(st.selected_idx < st.games.len());
    }

    const LOCALCONFIG: &str = "\"UserLocalConfigStore\"\n{\n\t\"Software\"\n\t{\n\t\t\"Valve\"\n\t\t{\n\t\t\t\"Steam\"\n\t\t\t{\n\t\t\t\t\"apps\"\n\t\t\t\t{\n\t\t\t\t\t\"7\"\n\t\t\t\t\t{\n\t\t\t\t\t\t\"LastPlayed\"\t\t\"1\"\n\t\t\t\t\t}\n\t\t\t\t}\n\t\t\t}\n\t\t}\n\t}\n}\n";

    fn doctor_steam(h: &Harness) -> Value {
        let (_, b) = http(h, "GET", "/api/doctor", &[("X-Rig-Token", &h.token)], b"");
        serde_json::from_str::<Value>(&b).unwrap()["steam"].clone()
    }

    fn last_log(h: &Harness) -> String {
        h.state.read().unwrap().logs.back().map(|l| format!("{}: {}", l.level, l.message)).unwrap_or_default()
    }

    #[cfg(target_os = "linux")] // Wine's launch options exist on Linux only
    #[test]
    fn steam_launch_options_flow_over_http() {
        let h = harness("steam");
        let lc = h._dir.join("steam/userdata/1/config/localconfig.vdf");
        fs::create_dir_all(lc.parent().unwrap()).unwrap();
        fs::write(&lc, LOCALCONFIG).unwrap();
        h.state.write().unwrap().steam_env = SteamEnv { config_vdf: None, localconfig: Some(lc.clone()), running: Some(false), libraries: Some(vec![]) };

        let s = doctor_steam(&h);
        assert_eq!(s["status"], "missing");
        assert_eq!(s["wanted"], "WINEDLLOVERRIDES=dwmapi=n,b gamemoderun %command%".replace(" gamemoderun", if fsutil::find_in_path("gamemoderun").is_some() { " gamemoderun" } else { "" }));

        // flags feed into the wanted string
        api(&h, "/api/toggle_flag", json!({ "flag": "mangohud" }));
        assert!(doctor_steam(&h)["wanted"].as_str().unwrap().contains("MANGOHUD=1"));
        api(&h, "/api/launch_options", json!({ "launch_args": "-dx12", "env_vars": "PROTON_ENABLE_NVAPI=1" }));
        let wanted = doctor_steam(&h)["wanted"].as_str().unwrap().to_string();
        assert!(wanted.starts_with("PROTON_ENABLE_NVAPI=1 WINEDLLOVERRIDES=dwmapi=n,b MANGOHUD=1"), "{wanted}");
        assert!(wanted.ends_with("%command% -dx12"), "{wanted}");

        api(&h, "/api/steam_launch_apply", json!({}));
        wait_until("options applied", || doctor_steam(&h)["status"] == "ok");
        assert!(fs::read_to_string(&lc).unwrap().contains("\"LaunchOptions\""));
        assert_eq!(steam::get_launch_options(&fs::read_to_string(&lc).unwrap(), "42").unwrap().unwrap(), wanted);
        assert_eq!(fs::read_dir(h.state.read().unwrap().cache_root().join("steam-backups")).unwrap().count(), 1);
        assert_eq!(h.state.read().unwrap().config.steam_applied.get("42"), Some(&wanted));

        // a setting changes -> outdated, and rig (which wrote the old value) may update it unasked
        api(&h, "/api/toggle_flag", json!({ "flag": "mangohud" }));
        assert_eq!(doctor_steam(&h)["status"], "stale");
        sync_steam_before_launch(&h.state, &selected_game(&h.state).unwrap());
        assert_eq!(doctor_steam(&h)["status"], "ok", "{}", last_log(&h));

        // the user's own options are never replaced without force
        let mine = fs::read_to_string(&lc).unwrap().replace(&steam::get_launch_options(&fs::read_to_string(&lc).unwrap(), "42").unwrap().unwrap(), "-my \\\"own\\\" %command%");
        fs::write(&lc, &mine).unwrap();
        assert_eq!(doctor_steam(&h)["status"], "custom");
        sync_steam_before_launch(&h.state, &selected_game(&h.state).unwrap());
        api(&h, "/api/steam_launch_apply", json!({}));
        wait_until("refusal logged", || last_log(&h).contains("Replace"));
        assert_eq!(fs::read_to_string(&lc).unwrap(), mine, "untouched");
        api(&h, "/api/steam_launch_apply", json!({ "force": true }));
        wait_until("forced", || doctor_steam(&h)["status"] == "ok");
    }

    #[cfg(target_os = "linux")] // Wine's launch options exist on Linux only
    #[test]
    fn pressing_play_never_edits_steams_config_just_for_optional_wrappers() {
        let h = harness("playsilent");
        let lc = h._dir.join("steam/userdata/1/config/localconfig.vdf");
        fs::create_dir_all(lc.parent().unwrap()).unwrap();
        fs::write(&lc, LOCALCONFIG).unwrap();
        h.state.write().unwrap().steam_env = SteamEnv { config_vdf: None, localconfig: Some(lc.clone()), running: Some(false), libraries: Some(vec![]) };

        // a plain (non-Unreal) game: only gamemode/MangoHud would be added -> Play leaves Steam alone
        {
            let mut st = h.state.write().unwrap();
            st.games[0].engine = "Unity".into();
            st.mangohud = true;
        }
        sync_steam_before_launch(&h.state, &selected_game(&h.state).unwrap());
        assert_eq!(fs::read_to_string(&lc).unwrap(), LOCALCONFIG, "untouched");
        // ...but an explicit request (Doctor / Full Setup) still applies them
        api(&h, "/api/steam_launch_apply", json!({}));
        wait_until("explicit apply", || doctor_steam(&h)["status"] == "ok");
        assert!(fs::read_to_string(&lc).unwrap().contains("MANGOHUD=1"));

        // a game with a real requirement (user env vars) is synced on Play
        fs::write(&lc, LOCALCONFIG).unwrap();
        h.state.write().unwrap().games[0].env_vars = "PROTON_ENABLE_NVAPI=1".into();
        sync_steam_before_launch(&h.state, &selected_game(&h.state).unwrap());
        assert!(fs::read_to_string(&lc).unwrap().contains("PROTON_ENABLE_NVAPI=1"));
    }

    #[cfg(target_os = "linux")] // Wine's launch options exist on Linux only
    #[test]
    fn steam_options_are_not_touched_while_steam_runs() {
        let h = harness("steamrun");
        let lc = h._dir.join("userdata/1/config/localconfig.vdf");
        fs::create_dir_all(lc.parent().unwrap()).unwrap();
        fs::write(&lc, LOCALCONFIG).unwrap();
        h.state.write().unwrap().steam_env = SteamEnv { config_vdf: None, localconfig: Some(lc.clone()), running: Some(true), libraries: Some(vec![]) };
        assert_eq!(doctor_steam(&h)["steam_running"], true);

        api(&h, "/api/steam_launch_apply", json!({}));
        wait_until("refused", || last_log(&h).contains("Steam is running"));
        api(&h, "/api/steam_launch_apply", json!({ "close_steam": true }));
        wait_until("cannot close", || last_log(&h).contains("close it first"));
        sync_steam_before_launch(&h.state, &selected_game(&h.state).unwrap());
        assert!(last_log(&h).contains("Steam is open"));
        assert_eq!(fs::read_to_string(&lc).unwrap(), LOCALCONFIG);
    }

    #[test]
    fn custom_and_missing_steam_setups_report_cleanly() {
        let h = harness("steamna");
        h.state.write().unwrap().steam_env = SteamEnv { config_vdf: None, localconfig: Some(h._dir.join("nope.vdf")), running: Some(false), libraries: Some(vec![]) };
        assert_eq!(doctor_steam(&h)["status"], "unavailable");
        api(&h, "/api/steam_launch_apply", json!({}));
        wait_until("reason logged", || last_log(&h).contains("Cannot read"));
        h.state.write().unwrap().games[0].is_custom = true;
        assert_eq!(doctor_steam(&h)["status"], "n/a");
    }

    #[cfg(target_os = "linux")] // starts a stand-in game process with bash
    #[test]
    fn monitor_tracks_a_game_started_outside_rig() {
        let h = harness("monitor");
        let tag = format!("RigMon{}-Win64-Shipping.exe", std::process::id());
        h.state.write().unwrap().games[0].shipping_exe = format!("/x/{}", tag);
        spawn_process_monitor(Arc::downgrade(&h.state));
        thread::sleep(Duration::from_millis(500));
        assert!(!h.state.read().unwrap().is_running.load(Ordering::SeqCst));

        let mut child = Command::new("bash")
            .args(["-c", &format!("exec -a 'Z:\\g\\{}' sleep 60", tag)])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        wait_until("detected running", || api(&h, "/api/state", json!({}))["is_running"] == true);
        assert!(h.state.read().unwrap().logs.iter().any(|l| l.message.contains("is running")));
        child.kill().unwrap();
        child.wait().unwrap();
        wait_until("detected exit", || api(&h, "/api/state", json!({}))["is_running"] == false);
        assert!(h.state.read().unwrap().logs.iter().any(|l| l.message.contains("no longer running")));

        // a rig-launched child keeps the flag up even before its command line is visible
        h.state.read().unwrap().running_pid.store(4_000_000, Ordering::SeqCst);
        wait_until("owned child", || api(&h, "/api/state", json!({}))["is_running"] == true);
        h.state.read().unwrap().running_pid.store(0, Ordering::SeqCst);
        wait_until("released", || api(&h, "/api/state", json!({}))["is_running"] == false);
    }

    /// Fabricate the UE4SS release zip the downloader would have cached.
    fn fake_ue4ss_zip(h: &Harness) {
        let src = h._dir.join("ue4ss_pkg");
        fs::create_dir_all(src.join("ue4ss/VTableLayoutTemplates")).unwrap();
        fs::create_dir_all(src.join("ue4ss/MemberVarLayoutTemplates")).unwrap();
        fs::create_dir_all(src.join("ue4ss/Mods/Keybinds/Scripts")).unwrap();
        fs::write(src.join("dwmapi.dll"), b"proxy").unwrap();
        fs::write(src.join("ue4ss/UE4SS.dll"), b"core").unwrap();
        fs::write(src.join("ue4ss/UE4SS-settings.ini"), "[General]\nbUseUObjectArrayCache = true\n").unwrap();
        fs::write(src.join("ue4ss/VTableLayoutTemplates/VTableLayout_5_03_Template.ini"), "[v]\n").unwrap();
        fs::write(src.join("ue4ss/MemberVarLayoutTemplates/MemberVariableLayout_5_03_Template.ini"), "[m]\n").unwrap();
        fs::write(src.join("ue4ss/Mods/Keybinds/Scripts/main.lua"), "--").unwrap();
        let zip = h.state.read().unwrap().paths.cache_dir.join(ue4ss::MANUAL_OVERRIDE);
        assert!(Command::new(tar_tool()).args(["-a", "-cf"]).arg(&zip).arg("-C").arg(&src).arg(".").status().unwrap().success());
    }

    #[test]
    fn full_setup_goes_from_a_bare_game_to_green_in_one_call() {
        if fsutil::find_in_path("bsdtar").is_none() {
            eprintln!("skipping: bsdtar missing");
            return;
        }
        let h = harness("fullsetup");
        let bin = PathBuf::from(&h.game.bin_dir);
        fs::remove_dir_all(bin.join("ue4ss")).unwrap(); // a game with no UE4SS at all
        fake_ue4ss_zip(&h);
        let lc = h._dir.join("steam/userdata/1/config/localconfig.vdf");
        fs::create_dir_all(lc.parent().unwrap()).unwrap();
        fs::write(&lc, LOCALCONFIG).unwrap();
        h.state.write().unwrap().steam_env = SteamEnv { config_vdf: None, localconfig: Some(lc.clone()), running: Some(false), libraries: Some(vec![]) };
        #[cfg(not(windows))] // the saves of this test live in a Wine prefix
        {
            fs::write(PathBuf::from(&h.game.pfx).join("drive_c/users/steamuser/AppData/Local/Demo/Saved/SaveGames/s.sav"), b"x").unwrap();
        }

        assert!(!RepairEngine::diagnose(&h.game).issues.is_empty());
        api(&h, "/api/full_setup", json!({ "close_steam": false }));
        wait_until("setup finished", || last_log(&h).contains("Setup complete"));

        assert!(bin.join("ue4ss/UE4SS.dll").is_file() && bin.join("dwmapi.dll").is_file());
        assert!(bin.join("ue4ss/VTableLayout.ini").is_file(), "templates deployed for 5.3");
        let ini = fs::read_to_string(bin.join("ue4ss/UE4SS-settings.ini")).unwrap();
        assert!(ini.contains("bUseUObjectArrayCache = false") && ini.contains("MinorVersion = 3"), "{ini}");
        let rep = RepairEngine::diagnose(&h.game);
        assert!(rep.issues.is_empty(), "{:?}", rep.issues);
        assert_eq!(doctor_steam(&h)["status"], "ok", "Steam launch options synced");
        #[cfg(not(windows))]
        {
            let snaps = saves::list(&h.game, &h.state.read().unwrap().cache_root());
            assert_eq!(snaps.len(), 1, "full setup backs saves up once");
            assert_eq!(snaps[0].roots.len(), 1, "{:?}", snaps[0].roots);
            assert!(h.state.read().unwrap().cache_root().join("saves/42").join(&snaps[0].id).join("data").join(&snaps[0].roots[0].key).join("s.sav").is_file());
        }
        assert_eq!(mod_enabled(&api(&h, "/api/state", json!({})), "LUA:Keybinds"), Some(true));
        let logs: Vec<String> = h.state.read().unwrap().logs.iter().map(|l| l.message.clone()).collect();
        for step in ["1/5 Installed UE4SS", "2/5", "3/5", "4/5", "5/5 Setup complete"] {
            assert!(logs.iter().any(|m| m.contains(step)), "missing step {step}: {logs:?}");
        }

        // running it again changes nothing and still ends green
        let before = fs::read_to_string(bin.join("ue4ss/Mods/mods.txt")).unwrap();
        api(&h, "/api/full_setup", json!({}));
        wait_until("second run", || h.state.read().unwrap().logs.iter().filter(|l| l.message.contains("Setup complete")).count() == 2);
        assert!(h.state.read().unwrap().logs.iter().any(|l| l.message.contains("1/5 UE4SS is already installed")));
        assert_eq!(fs::read_to_string(bin.join("ue4ss/Mods/mods.txt")).unwrap(), before);
    }

    #[test]
    fn full_setup_downloads_the_latest_release_it_finds_and_installs_it() {
        if fsutil::find_in_path("bsdtar").is_none() {
            eprintln!("skipping: bsdtar missing");
            return;
        }
        let h = harness("fullsetup_dl");
        let bin = PathBuf::from(&h.game.bin_dir);
        fs::remove_dir_all(bin.join("ue4ss")).unwrap();
        // build a release archive the way the real zDEV one is laid out
        let src = h._dir.join("rel");
        fs::create_dir_all(src.join("ue4ss/VTableLayoutTemplates")).unwrap();
        fs::create_dir_all(src.join("ue4ss/MemberVarLayoutTemplates")).unwrap();
        fs::create_dir_all(src.join("ue4ss/Docs")).unwrap();
        fs::write(src.join("dwmapi.dll"), b"p").unwrap();
        fs::write(src.join("ue4ss/UE4SS.dll"), b"core").unwrap();
        fs::write(src.join("ue4ss/UE4SS.pdb"), vec![0u8; 5000]).unwrap();
        fs::write(src.join("ue4ss/Docs/readme.md"), b"x").unwrap();
        fs::write(src.join("ue4ss/UE4SS-settings.ini"), "[General]\nbUseUObjectArrayCache = true\n").unwrap();
        fs::write(src.join("ue4ss/VTableLayoutTemplates/VTableLayout_5_03_Template.ini"), "[v]\n").unwrap();
        fs::write(src.join("ue4ss/MemberVarLayoutTemplates/MemberVariableLayout_5_03_Template.ini"), "[m]\n").unwrap();
        let zip = h._dir.join("release.zip");
        assert!(Command::new(tar_tool()).args(["-a", "-cf"]).arg(&zip).arg("-C").arg(&src).arg(".").status().unwrap().success());
        let fake = ue4ss::fake_release("zDEV-UE4SS_v9.9.9-1-gabc.zip", fs::read(&zip).unwrap());
        h.state.write().unwrap().ue4ss_source = fake.src.clone();

        api(&h, "/api/full_setup", json!({}));
        wait_until("setup complete", || last_log(&h).contains("Setup complete"));
        assert!(bin.join("ue4ss/UE4SS.dll").is_file() && bin.join("dwmapi.dll").is_file());
        assert!(!bin.join("ue4ss/UE4SS.pdb").exists() && !bin.join("ue4ss/Docs").exists(), "bulky extras are not installed");
        assert!(bin.join("ue4ss/VTableLayout.ini").is_file(), "template for the detected UE version deployed");
        assert!(RepairEngine::diagnose(&h.game).issues.is_empty());
        let cache = h.state.read().unwrap().paths.cache_dir.clone();
        assert!(cache.join("zDEV-UE4SS_v9.9.9-1-gabc.zip").is_file(), "kept for offline reuse");
        assert!(h.state.read().unwrap().logs.iter().any(|l| l.message.contains("Downloading zDEV-UE4SS_v9.9.9")));
    }

    /// Opt-in check against the real release archive:
    /// `RIG_REAL_UE4SS_ZIP=/path/to/zDEV-UE4SS_v….zip cargo test real_ue4ss -- --nocapture`
    #[test]
    fn real_ue4ss_release_installs_and_leaves_the_doctor_green() {
        let Some(zip) = std::env::var_os("RIG_REAL_UE4SS_ZIP").map(PathBuf::from).filter(|p| p.is_file()) else {
            eprintln!("skipping: RIG_REAL_UE4SS_ZIP not set");
            return;
        };
        let h = harness("realue4ss");
        let bin = PathBuf::from(&h.game.bin_dir);
        fs::remove_dir_all(bin.join("ue4ss")).unwrap();
        fs::copy(&zip, h.state.read().unwrap().paths.cache_dir.join(ue4ss::MANUAL_OVERRIDE)).unwrap();
        api(&h, "/api/full_setup", json!({}));
        wait_until("setup finished", || last_log(&h).contains("5/5"));
        let rep = RepairEngine::diagnose(&h.game);
        eprintln!("doctor issues: {:?}\nlast log: {}", rep.issues, last_log(&h));
        assert!(rep.issues.is_empty(), "{:?}", rep.issues);
        assert!(bin.join("ue4ss/Mods/Keybinds/Scripts/main.lua").is_file());
        let mods = api(&h, "/api/state", json!({}))["mods"].as_array().unwrap().iter().map(|m| m["id"].as_str().unwrap().to_string()).collect::<Vec<_>>();
        eprintln!("mods after install: {:?}", mods);
        let ini = fs::read_to_string(bin.join("ue4ss/UE4SS-settings.ini")).unwrap();
        for want in ["bUseUObjectArrayCache = false", "MinorVersion = 3", "GraphicsAPI = dx11"] {
            assert!(ini.contains(want), "{want}\n{ini}");
        }
    }

    #[cfg(target_os = "linux")] // starts a stand-in game process with bash
    #[test]
    fn full_setup_refuses_while_the_game_runs_and_handles_missing_archives() {
        let h = harness("fullsetup2");
        let tag = format!("RigSetup{}-Win64-Shipping.exe", std::process::id());
        h.state.write().unwrap().games[0].shipping_exe = format!("/x/{}", tag);
        let mut child = Command::new("bash")
            .args(["-c", &format!("exec -a 'Z:\\g\\{}' sleep 60", tag)])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        wait_until("fake game visible in /proc", || procs::is_game_running(&selected_game(&h.state).unwrap()));
        api(&h, "/api/full_setup", json!({}));
        wait_until("refused", || last_log(&h).contains("is running"));
        child.kill().unwrap();
        child.wait().unwrap();

        // not running any more, but no cached archive and no network access in tests: a clear error, nothing half-installed
        let bin = PathBuf::from(&h.game.bin_dir);
        fs::remove_dir_all(bin.join("ue4ss")).unwrap();
        h.state.write().unwrap().games[0].shipping_exe = h.game.shipping_exe.clone();
        let zip = h.state.read().unwrap().paths.cache_dir.join(ue4ss::MANUAL_OVERRIDE);
        fs::write(&zip, b"corrupt download").unwrap();
        api(&h, "/api/full_setup", json!({}));
        wait_until("extraction error", || last_log(&h).contains("Full setup stopped"));
        assert!(!zip.exists(), "corrupt archive discarded so the next try re-downloads");
        assert!(!bin.join("ue4ss").exists());
    }

    #[test]
    fn non_unreal_games_still_get_their_steam_options() {
        let h = harness("fullsetup3");
        let plain = h._dir.join("plain_game");
        fs::create_dir_all(&plain).unwrap();
        {
            let mut st = h.state.write().unwrap();
            st.games[0].game_dir = plain.to_string_lossy().to_string();
            st.games[0].engine = "Standard / Non-UE".into();
            st.games[0].env_vars = "FOO=1".into();
        }
        let lc = h._dir.join("steam/userdata/1/config/localconfig.vdf");
        fs::create_dir_all(lc.parent().unwrap()).unwrap();
        fs::write(&lc, LOCALCONFIG).unwrap();
        h.state.write().unwrap().steam_env = SteamEnv { config_vdf: None, localconfig: Some(lc.clone()), running: Some(false), libraries: Some(vec![]) };
        api(&h, "/api/full_setup", json!({}));
        wait_until("done", || last_log(&h).contains("Setup complete"));
        let gm = if fsutil::find_in_path("gamemoderun").is_some() { "gamemoderun " } else { "" };
        assert_eq!(
            steam::get_launch_options(&fs::read_to_string(&lc).unwrap(), "42").unwrap(),
            Some(format!("FOO=1 {}%command%", gm))
        );
        assert!(h.state.read().unwrap().logs.iter().any(|l| l.message.contains("Not an Unreal Engine game")));
    }

    #[test]
    fn full_setup_says_plainly_when_there_is_nothing_to_do() {
        let h = harness("fullsetup_nothing");
        let plain = h._dir.join("plain_direct");
        fs::create_dir_all(&plain).unwrap();
        {
            let mut st = h.state.write().unwrap();
            st.games[0].game_dir = plain.to_string_lossy().to_string();
            st.games[0].engine = "Standard / Non-UE".into();
            st.games[0].is_custom = true;
        }
        api(&h, "/api/full_setup", json!({}));
        wait_until("explained", || last_log(&h).contains("Nothing to set up"));
        let logs: Vec<String> = h.state.read().unwrap().logs.iter().map(|l| l.message.clone()).collect();
        assert!(!logs.iter().any(|m| m.contains("Setup complete")), "must not claim success for work it did not do: {logs:?}");
        assert!(last_log(&h).contains("not an Unreal Engine game"));
    }

    #[test]
    fn hydra_games_use_their_own_proton_and_launch_directly() {
        let h = harness("hydraproton");
        let proton = h._dir.join("p/GE-X");
        fs::create_dir_all(&proton).unwrap();
        fs::write(proton.join("proton"), b"#!/bin/sh").unwrap();
        {
            let mut st = h.state.write().unwrap();
            st.protons = vec![("Other".into(), PathBuf::from("/opt/other/proton"))];
            st.games[0].source = "hydra".into();
            st.games[0].proton_path = proton.to_string_lossy().to_string();
            let g = st.games[0].clone();
            assert_eq!(st.proton_for(&g), Some(proton.join("proton")));
            st.games[0].proton_path = "/gone".into();
            let g = st.games[0].clone();
            assert_eq!(st.proton_for(&g), Some(PathBuf::from("/opt/other/proton")), "falls back to rig's selection");
        }
        // Steam sync does not apply to a game rig launches itself
        assert_eq!(doctor_steam(&h)["status"], "n/a");
        let st = api(&h, "/api/state", json!({}));
        assert_eq!(st["games"][0]["source"], "hydra");
    }

    #[test]
    fn compat_app_id_never_borrows_another_games_id() {
        let mut g = harness_game_for_ids();
        g.name = "F1 2020".into();
        assert_eq!(compat_app_id(&g), "0");
        g.steam_appid = "1080110".into();
        assert_eq!(compat_app_id(&g), "1080110");
        g.steam_appid.clear();
        g.name = "Half Sword".into();
        assert_eq!(compat_app_id(&g), "2397300");
    }

    fn harness_game_for_ids() -> GameEntry {
        make_game("ids").1
    }

    #[test]
    fn steam_installs_and_uninstalls_show_up_without_a_manual_rescan() {
        let h = harness("steamwatch");
        let lib = h._dir.join("steamlib/steamapps");
        fs::create_dir_all(lib.join("common")).unwrap();
        h.state.write().unwrap().steam_env.libraries = Some(vec![lib.clone()]);
        rescan(&h.state);
        assert!(!h.state.read().unwrap().games.iter().any(|g| g.name == "Fresh Install"));
        spawn_library_watcher(Arc::downgrade(&h.state));
        thread::sleep(Duration::from_millis(500));

        // the user installs a game in Steam
        fs::create_dir_all(lib.join("common/Fresh")).unwrap();
        fs::write(
            lib.join("appmanifest_900.acf"),
            "\"AppState\"\n{\n\t\"appid\"\t\t\"900\"\n\t\"name\"\t\t\"Fresh Install\"\n\t\"installdir\"\t\t\"Fresh\"\n}\n",
        )
        .unwrap();
        wait_until("installed Steam game appears", || {
            h.state.read().unwrap().games.iter().any(|g| g.name == "Fresh Install" && g.is_installed)
        });

        // ...and removes it again
        fs::remove_file(lib.join("appmanifest_900.acf")).unwrap();
        wait_until("uninstalled Steam game disappears", || {
            !h.state.read().unwrap().games.iter().any(|g| g.name == "Fresh Install")
        });
        let st = h.state.read().unwrap();
        assert!(st.games.is_empty() && st.selected_idx == 0);
    }

    #[test]
    fn any_game_can_be_removed_from_the_list_and_restored_without_touching_its_files() {
        let h = harness("hidegame");
        let lib = h._dir.join("steamlib/steamapps");
        let acf = |id: &str, name: &str| format!("\"AppState\"\n{{\n\t\"appid\"\t\t\"{id}\"\n\t\"name\"\t\t\"{name}\"\n\t\"installdir\"\t\t\"{name}\"\n}}\n");
        for (id, name) in [("901", "Alpha"), ("902", "Beta")] {
            fs::create_dir_all(lib.join("common").join(name)).unwrap();
            fs::write(lib.join(format!("appmanifest_{id}.acf")), acf(id, name)).unwrap();
        }
        h.state.write().unwrap().steam_env.libraries = Some(vec![lib.clone()]);
        rescan(&h.state);
        let names = |h: &Harness| api(h, "/api/state", json!({}))["games"].as_array().unwrap().iter().map(|g| g["name"].as_str().unwrap().to_string()).collect::<Vec<_>>();
        assert!(names(&h).contains(&"Alpha".to_string()));

        api(&h, "/api/remove_game", json!({ "appid": "901" }));
        assert_eq!(names(&h), vec!["Beta".to_string()]);
        assert!(lib.join("common/Alpha").is_dir(), "files stay where they are");
        rescan(&h.state);
        assert_eq!(names(&h), vec!["Beta".to_string()], "a rescan keeps it hidden");
        assert_eq!(api(&h, "/api/state", json!({}))["hidden_games"][0]["name"], "Alpha");
        // it survives a restart: the config file has it
        let cfg: RigConfigData = serde_json::from_str(&fs::read_to_string(&h.state.read().unwrap().paths.config_file).unwrap()).unwrap();
        assert!(cfg.hidden_games.contains_key("901"));

        api(&h, "/api/restore_game", json!({ "appid": "901" }));
        wait_until("restored", || names(&h).contains(&"Alpha".to_string()));
        assert!(api(&h, "/api/state", json!({}))["hidden_games"].as_array().unwrap().is_empty());
    }

    #[test]
    fn playing_a_game_that_is_not_installed_is_refused() {
        let h = harness("notinstalled");
        {
            let mut st = h.state.write().unwrap();
            st.games[0].is_installed = false;
        }
        api(&h, "/api/play", json!({}));
        wait_until("refusal is logged", || {
            h.state.read().unwrap().logs.iter().any(|l| l.message.contains("is not installed"))
        });
        assert!(!h.state.read().unwrap().is_running.load(Ordering::SeqCst));
    }

    #[test]
    fn hydra_library_changes_show_up_without_a_manual_rescan() {
        let h = harness("hydrawatch");
        let hd = h._dir.join("hydra");
        fs::create_dir_all(hd.join("hydra-db")).unwrap();
        let exe1 = h._dir.join("hg/One/one.exe");
        let exe2 = h._dir.join("hg/Two/two.exe");
        for e in [&exe1, &exe2] {
            fs::create_dir_all(e.parent().unwrap()).unwrap();
            fs::write(e, b"MZ").unwrap();
        }
        let rec = |t: &str, id: &str, exe: &Path| format!(r#"{{"title":"{t}","objectId":"{id}","shop":"steam","isDeleted":false,"executablePath":"{}"}}"#, exe.to_string_lossy().replace('\\', "\\\\"));
        let k1 = b"!games!steam:1".to_vec();
        let v1 = rec("Game One", "1", &exe1).into_bytes();
        fs::write(hd.join("hydra-db/000001.log"), leveldb::builder::wal(&[(1, vec![(&k1[..], Some(&v1[..]))])])).unwrap();
        {
            let mut st = h.state.write().unwrap();
            st.paths.hydra_dir = Some(hd.clone());
        }
        // pretend the first scan already happened
        rescan(&h.state);
        assert!(h.state.read().unwrap().games.iter().any(|g| g.name == "Game One"));
        spawn_library_watcher(Arc::downgrade(&h.state));
        thread::sleep(Duration::from_millis(500));

        // the user installs a second game in Hydra (new log file), and "Game One" gets updated playtime-style
        let (k2, v2) = (b"!games!steam:2".to_vec(), rec("Game Two", "2", &exe2).into_bytes());
        fs::write(hd.join("hydra-db/000002.log"), leveldb::builder::wal(&[(5, vec![(&k2[..], Some(&v2[..]))])])).unwrap();
        wait_until("new Hydra game appears", || h.state.read().unwrap().games.iter().any(|g| g.name == "Game Two"));

        // playtime-only updates must not trigger rescans
        let n = h.state.read().unwrap().logs.iter().filter(|l| l.message.contains("game library changed")).count();
        let v1b = rec("Game One", "1", &exe1).replace("\"isDeleted\"", "\"playTimeInMilliseconds\":999,\"isDeleted\"").into_bytes();
        fs::write(hd.join("hydra-db/000003.log"), leveldb::builder::wal(&[(9, vec![(&k1[..], Some(&v1b[..]))])])).unwrap();
        thread::sleep(Duration::from_millis(900));
        assert_eq!(h.state.read().unwrap().logs.iter().filter(|l| l.message.contains("game library changed")).count(), n);

        // removing a game in Hydra removes it here, and the selection stays on a valid game
        let del = rec("Game Two", "2", &exe2).replace("false", "true").into_bytes();
        fs::write(hd.join("hydra-db/000004.log"), leveldb::builder::wal(&[(20, vec![(&k2[..], Some(&del[..]))])])).unwrap();
        wait_until("removed Hydra game disappears", || !h.state.read().unwrap().games.iter().any(|g| g.name == "Game Two"));
        let st = h.state.read().unwrap();
        assert!(st.selected_idx < st.games.len());
    }

    /// Start a stand-in "game" process and return only once its command line is *stably* visible
    /// (it is briefly empty or still `bash -c …` while the process is inside `exec`).
    #[cfg(target_os = "linux")]
    fn spawn_fake_game(tag: &str) -> std::process::Child {
        let child = Command::new("bash")
            .args(["-c", &format!("exec -a 'Z:\\\\g\\\\{}' sleep 120", tag)])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let (needle, pid) = (tag.to_lowercase(), child.id());
        let mut steady = 0;
        for _ in 0..500 {
            let seen = procs::pids_with_cmdline(&needle).contains(&pid)
                && fs::read(format!("/proc/{}/cmdline", pid)).map(|c| c.starts_with(b"Z:")).unwrap_or(false);
            steady = if seen { steady + 1 } else { 0 };
            if steady >= 5 {
                break;
            }
            thread::sleep(Duration::from_millis(20));
        }
        child
    }

    #[cfg(target_os = "linux")] // starts a stand-in game process with bash
    #[test]
    fn a_crashed_run_is_diagnosed_and_a_clean_one_is_not() {
        let h = harness("crashrun");
        let tag = format!("RigCrash{}-Win64-Shipping.exe", std::process::id());
        h.state.write().unwrap().games[0].shipping_exe = format!("/x/{}", tag);
        spawn_process_monitor(Arc::downgrade(&h.state));
        let bin = PathBuf::from(&h.game.bin_dir);

        // run 1: the game leaves a dump and a Lua error naming CoolMod, then dies
        let mut child = spawn_fake_game(&tag);
        wait_until("run seen", || api(&h, "/api/state", json!({}))["is_running"] == true);
        fs::write(bin.join("ue4ss/UE4SS.log"), "[1] Error: ...\\Mods\\CoolMod\\Scripts\\main.lua:7: attempt to call a nil value\n").unwrap();
        fs::write(bin.join("crash.dmp"), b"x").unwrap();
        child.kill().unwrap();
        child.wait().unwrap();
        wait_until("crash diagnosed", || !api(&h, "/api/state", json!({}))["crash"].is_null());
        let st = api(&h, "/api/state", json!({}));
        assert_eq!(st["crash"]["crashed"], true);
        assert_eq!(st["crash"]["suspects"][0]["id"], "LUA:CoolMod", "{}", st["crash"]);
        assert_eq!(st["crash"]["streak"], 1);
        let kinds: Vec<&str> = st["crash"]["tries"].as_array().unwrap().iter().filter_map(|t| t["kind"].as_str()).collect();
        assert!(!kinds.is_empty() && kinds.len() <= 3, "{}", st["crash"]["tries"]);
        assert_eq!(st["last_run"]["crashed"], true);
        assert!(last_log(&h).contains("crashed") || h.state.read().unwrap().logs.iter().any(|l| l.message.contains("crashed.")));

        // dismissing clears the banner
        api(&h, "/api/crash_dismiss", json!({}));
        assert!(api(&h, "/api/state", json!({}))["crash"].is_null());

        // run 2: nothing bad happens
        fs::remove_file(bin.join("crash.dmp")).unwrap();
        fs::remove_file(bin.join("ue4ss/UE4SS.log")).unwrap();
        let t_before = api(&h, "/api/state", json!({}))["last_run"]["ended_ms"].as_u64().unwrap();
        let mut child = spawn_fake_game(&tag);
        wait_until("run 2 seen", || api(&h, "/api/state", json!({}))["is_running"] == true);
        child.kill().unwrap();
        child.wait().unwrap();
        wait_until("run 2 recorded", || api(&h, "/api/state", json!({}))["last_run"]["ended_ms"].as_u64().unwrap() > t_before);
        let st = api(&h, "/api/state", json!({}));
        assert_eq!(st["last_run"]["crashed"], false);
        assert!(st["crash"].is_null(), "a clean run raises no alarm");
        assert_eq!(h.state.read().unwrap().config.crash_streak.get(&h.game.appid), Some(&0), "a clean run ends the streak");
    }

    #[test]
    fn a_crash_after_an_update_offers_the_way_back_and_a_clean_run_does_not() {
        let h = harness("crashupdate");
        let g = selected_game(&h.state).unwrap();
        let lib = library_of(&g).unwrap();
        let nx = |file_id| nexus::NexusRef { game: "g".into(), mod_id: 5, file_id };
        let stage = |name: &str, body: &str| {
            let d = h._dir.join(&format!("stage-{name}"));
            fs::create_dir_all(d.join("Data")).unwrap();
            fs::write(d.join("Data/cool.esp"), body).unwrap();
            d
        };
        let v1 = lib.add_with(&stage("v1", "ONE"), "Cool", "c1.zip", Some(nx(1)), "1.0").unwrap();
        lib.replace_with(&v1.id, &stage("v2", "TWO"), "Cool", "c2.zip", Some(nx(2)), "2.0").unwrap();
        let bin = PathBuf::from(&g.bin_dir);
        let started = now_ms().saturating_sub(125_000);

        // a clean run: nothing is said
        finish_run(&h.state, Run { game: g.clone(), started_ms: started, perf: Default::default(), seen: Vec::new(), next_scan_ms: 0 });
        assert!(api(&h, "/api/state", json!({}))["crash"].is_null());
        assert!(!read_state(&h.state).notes.json().to_string().contains("rollback"));

        // a crash: the update is named, with the folder to go back to
        fs::write(bin.join("crash.dmp"), b"x").unwrap();
        finish_run(&h.state, Run { game: g.clone(), started_ms: started, perf: Default::default(), seen: Vec::new(), next_scan_ms: 0 });
        let c = api(&h, "/api/state", json!({}))["crash"].clone();
        assert_eq!(c["crashed"], true);
        assert_eq!((c["recent_updates"][0]["name"].as_str(), c["recent_updates"][0]["version"].as_str(), c["recent_updates"][0]["old_version"].as_str()), (Some("Cool"), Some("2.0"), Some("1.0")), "{c}");
        assert!(read_state(&h.state).notes.json().to_string().contains("crashed after Cool was updated to v2.0"));
        // the way back is the ordinary rollback
        let (id, dir) = (c["recent_updates"][0]["id"].as_str().unwrap().to_string(), c["recent_updates"][0]["dir"].as_str().unwrap().to_string());
        api(&h, "/api/lib_rollback", json!({ "id": id, "dir": dir }));
        wait_until("rolled back", || lib.mods()[0].version == "1.0");
        fs::remove_file(bin.join("crash.dmp")).unwrap();
    }

    #[test]
    fn a_known_problem_written_to_a_log_during_play_is_told_once_and_old_ones_are_not() {
        let h = harness("livelog");
        let g = selected_game(&h.state).unwrap();
        let log_file = PathBuf::from(&g.game_dir).join("BepInEx/LogOutput.log");
        fs::create_dir_all(log_file.parent().unwrap()).unwrap();
        fs::write(&log_file, "[Error  :   BepInEx] Could not load [Old 1.0] because it has missing dependencies: Lib\n").unwrap();
        let seen = kb_findings(&h.state, &g).into_iter().map(|f| (f.id, f.evidence)).collect();
        let mut run = Run { game: g.clone(), started_ms: now_ms(), perf: Default::default(), seen, next_scan_ms: 0 };
        let known = || read_state(&h.state).notes.json()["items"].as_array().unwrap().iter().filter(|n| n["kind"] == "known").map(|n| n["text"].as_str().unwrap().to_string()).collect::<Vec<_>>();
        live_log_watch(&h.state, &mut run);
        assert!(known().is_empty(), "what the log said before the game started is not news");
        fs::write(&log_file, "[Error  :   BepInEx] Could not load [Old 1.0] because it has missing dependencies: Lib\nNo space left on device\n").unwrap();
        live_log_watch(&h.state, &mut run);
        live_log_watch(&h.state, &mut run);
        assert_eq!(known(), vec![format!("While {} runs, its log shows a known problem: The disk is full", g.name)]);
        // the user's language is used for the problem's name
        write_state(&h.state).config.language = "ro".into();
        fs::write(&log_file, "System.DllNotFoundException: native.dll\n").unwrap();
        live_log_watch(&h.state, &mut run);
        assert_eq!(known().len(), 2);
        assert!(!known()[0].contains("A mod needs a library file"), "{:?}", known());
    }

    #[test]
    fn the_log_viewer_reads_only_logs_it_found_itself() {
        let h = harness("logviewer");
        let bin = PathBuf::from(&h.game.bin_dir);
        let get = |q: &str| {
            let (c, b) = http(&h, "GET", &format!("/api/game_logs{q}"), &[("X-Rig-Token", &h.token)], b"");
            assert_eq!(c, 200);
            serde_json::from_str::<Value>(&b).unwrap()
        };
        assert_eq!(http(&h, "GET", "/api/game_logs", &[], b"").0, 403);
        assert!(get("")["sources"].as_array().unwrap().is_empty() || get("")["src"].is_string());
        fs::write(bin.join("ue4ss/UE4SS.log"), "[1] starting\n[2] Error: Mods/CoolMod/main.lua:1: boom\n[3] done\n").unwrap();
        let v = get("");
        assert!(v["sources"].as_array().unwrap().iter().any(|s| s["id"] == "ue4ss"), "{v}");
        assert!(v["sources"][0].get("path").is_none(), "paths are not shown to the page");
        let all = get("?src=ue4ss");
        assert_eq!((all["total"].as_u64(), all["lines"][1]["level"].as_str()), (Some(3), Some("bad")));
        assert_eq!(get("?src=ue4ss&q=BOOM")["total"], 1);
        // a name that is not one of ours, or a path, falls back to a log of the game and never reads that file
        let odd = get("?src=../../../../etc/passwd");
        assert!(odd["lines"].as_array().unwrap().iter().all(|l| !l["text"].as_str().unwrap().contains("root:")), "{odd}");
        fs::remove_file(bin.join("ue4ss/UE4SS.log")).unwrap();
    }

    #[test]
    fn manual_analysis_reads_whatever_is_on_disk() {
        let h = harness("crashmanual");
        let bin = PathBuf::from(&h.game.bin_dir);
        fs::write(bin.join("ue4ss/UE4SS.log"), "[1] Error: Mods/CoolMod/Scripts/main.lua:1: attempt to index a nil value\n").unwrap();
        api(&h, "/api/crash_analyze", json!({}));
        wait_until("report", || !api(&h, "/api/state", json!({}))["crash"].is_null());
        let c = api(&h, "/api/state", json!({}))["crash"].clone();
        assert_eq!(c["suspects"][0]["name"], "CoolMod");
        assert_eq!(c["crashed"], false, "errors alone are not a crash");
    }

    fn names(v: &Value) -> Vec<String> {
        v.as_array().unwrap().iter().map(|x| x.as_str().unwrap().to_string()).collect()
    }

    fn enabled_user_mods(h: &Harness) -> Vec<String> {
        let st = api(h, "/api/state", json!({}));
        let mut v: Vec<String> = st["mods"].as_array().unwrap().iter()
            .filter(|m| m["is_system"] == false && m["enabled"] == true)
            .map(|m| m["name"].as_str().unwrap().to_string()).collect();
        v.sort();
        v
    }

    #[test]
    fn find_the_culprit_session_end_to_end() {
        let h = harness("bisect");
        // add a few more user mods so the session has something to chew on
        let mods_dir = PathBuf::from(&h.game.bin_dir).join("ue4ss/Mods");
        for n in ["Alpha", "Bravo", "Charlie"] {
            fs::create_dir_all(mods_dir.join(n).join("Scripts")).unwrap();
            fs::write(mods_dir.join(n).join("Scripts/main.lua"), "--").unwrap();
            fs::write(mods_dir.join(n).join("enabled.txt"), "").unwrap();
        }
        refresh_mods_shared(&h.state);
        wait_until("mods listed", || enabled_user_mods(&h).len() == 5);
        let originals = enabled_user_mods(&h);
        assert_eq!(originals, vec!["Alpha", "Bravo", "Charlie", "CoolMod", "Skin_P"]);

        api(&h, "/api/bisect_start", json!({}));
        let mut st = api(&h, "/api/state", json!({}));
        assert_eq!(st["bisect"]["phase"], "baseline");
        assert!(enabled_user_mods(&h).is_empty(), "round 1: every user mod is off");
        assert!(st["mods"].as_array().unwrap().iter().any(|m| m["is_system"] == true && m["enabled"] == true), "system mods untouched");

        // a second session cannot be opened over the first
        api(&h, "/api/bisect_start", json!({}));
        assert!(last_log(&h).contains("already open"));

        // oracle: the game crashes whenever Charlie is enabled
        let culprit = "Charlie";
        for _ in 0..12 {
            st = api(&h, "/api/state", json!({}));
            if st["bisect"]["finished"] == true {
                break;
            }
            let on = enabled_user_mods(&h);
            assert_eq!(on, { let mut t = names(&st["bisect"]["testing"]); t.sort(); t }, "disk matches the round");
            api(&h, "/api/bisect_answer", json!({ "crashed": on.iter().any(|m| m == culprit) }));
        }
        st = api(&h, "/api/state", json!({}));
        assert_eq!(st["bisect"]["phase"], "done", "{}", st["bisect"]);
        assert_eq!(names(&st["bisect"]["found"]), vec!["Charlie"]);

        api(&h, "/api/bisect_finish", json!({ "restore_all": false }));
        st = api(&h, "/api/state", json!({}));
        assert!(st["bisect"].is_null());
        assert_eq!(enabled_user_mods(&h), vec!["Alpha", "Bravo", "CoolMod", "Skin_P"], "culprit stays off, the rest is back");
        assert!(!h.state.read().unwrap().cache_root().join("bisect").join("42.json").exists(), "session file removed");
        assert_eq!(h.state.read().unwrap().config.culprits.get("42").cloned(), Some(vec!["Charlie".to_string()]), "what it found is remembered");
        // with it on again, the next crash points at it first
        api(&h, "/api/toggle_mod", json!({ "id": "LUA:Charlie", "enable": true }));
        wait_until("Charlie is on again", || enabled_user_mods(&h).contains(&"Charlie".to_string()));
        let g = selected_game(&h.state).unwrap();
        let report = crash::analyze_run(&g, &ModManager::list_mods(&g), 0, now_ms());
        let tries = crash_tries(&h.state, &g, &report);
        let again = tries.iter().find(|t| t["kind"] == "culprit_again").unwrap_or_else(|| panic!("{tries:?}"));
        assert_eq!((again["id"].as_str(), again["library"].as_bool()), (Some("LUA:Charlie"), Some(false)));
    }

    #[test]
    fn crash_detective_finds_the_culprit_among_library_mods_of_any_game() {
        let h = harness("bisectlib");
        let dir = add_plain_game(&h);
        let g = selected_game(&h.state).unwrap();
        let lib = library_of(&g).unwrap();
        for n in ["Alpha", "Bravo", "Charlie", "Delta", "Echo"] {
            let stage = h._dir.join(&format!("stage-{n}"));
            fs::create_dir_all(stage.join("Data")).unwrap();
            fs::write(stage.join("Data").join(format!("{n}.esp")), n).unwrap();
            lib.add_from_dir(&stage, n, "x.zip").unwrap();
        }
        lib.deploy(false).unwrap();
        let on_disk = || {
            let mut v: Vec<String> = ["Alpha", "Bravo", "Charlie", "Delta", "Echo"].iter().filter(|n| dir.join("Data").join(format!("{n}.esp")).exists()).map(|n| n.to_string()).collect();
            v.sort();
            v
        };
        assert_eq!(on_disk().len(), 5);
        api(&h, "/api/bisect_start", json!({}));
        wait_until("round one deployed: nothing on", || on_disk().is_empty() && api(&h, "/api/state", json!({}))["bisect"]["phase"] == "baseline");
        for _ in 0..12 {
            let st = api(&h, "/api/state", json!({}));
            if st["bisect"]["finished"] == true {
                break;
            }
            let testing: Vec<String> = st["bisect"]["testing"].as_array().unwrap().iter().map(|v| v.as_str().unwrap().to_string()).collect();
            let mut want = testing.clone();
            want.sort();
            wait_until("the round is on disk", || on_disk() == want);
            api(&h, "/api/bisect_answer", json!({ "crashed": testing.iter().any(|m| m == "Charlie") }));
        }
        let st = api(&h, "/api/state", json!({}));
        assert_eq!(st["bisect"]["found"], json!(["Charlie"]), "{}", st["bisect"]);
        api(&h, "/api/bisect_finish", json!({ "restore_all": false }));
        wait_until("everything but the culprit is back", || on_disk() == vec!["Alpha", "Bravo", "Delta", "Echo"]);
        let mods = lib.mods();
        assert_eq!(mods.iter().filter(|m| m.enabled).count(), 4);
        assert!(api(&h, "/api/state", json!({}))["bisect"].is_null());
    }

    #[test]
    fn cancelling_restores_everything_and_a_mod_free_crash_is_not_blamed() {
        let h = harness("bisect2");
        let before = enabled_user_mods(&h);
        api(&h, "/api/bisect_start", json!({}));
        assert!(enabled_user_mods(&h).is_empty());
        api(&h, "/api/bisect_answer", json!({ "crashed": true })); // still crashes with no mods
        let st = api(&h, "/api/state", json!({}));
        assert_eq!(st["bisect"]["phase"], "gave_up");
        assert!(st["bisect"]["note"].as_str().unwrap().contains("not caused by your mods"));
        api(&h, "/api/bisect_finish", json!({ "restore_all": true }));
        assert_eq!(enabled_user_mods(&h), before);

        // answering with no session is a polite no-op
        api(&h, "/api/bisect_answer", json!({ "crashed": true }));
        assert!(last_log(&h).contains("No Crash Detective session"));
    }

    #[test]
    fn an_unfinished_session_survives_a_restart() {
        let h = harness("bisect3");
        let before = enabled_user_mods(&h);
        api(&h, "/api/bisect_start", json!({}));
        assert!(enabled_user_mods(&h).is_empty());
        // "restart": the in-memory session is gone, the file is not
        h.state.write().unwrap().bisect = None;
        assert!(api(&h, "/api/state", json!({}))["bisect"].is_null());
        resume_bisect(&h.state);
        let st = api(&h, "/api/state", json!({}));
        assert_eq!(st["bisect"]["phase"], "baseline");
        assert!(last_log(&h).contains("Resumed"));
        api(&h, "/api/bisect_finish", json!({ "restore_all": true }));
        assert_eq!(enabled_user_mods(&h), before, "nothing is left switched off");
    }

    #[cfg(target_os = "linux")] // starts a stand-in game process with bash
    #[test]
    fn answers_are_refused_while_the_game_is_running() {
        let h = harness("bisect4");
        let tag = format!("RigBis{}-Win64-Shipping.exe", std::process::id());
        h.state.write().unwrap().games[0].shipping_exe = format!("/x/{}", tag);
        api(&h, "/api/bisect_start", json!({}));
        let mut child = spawn_fake_game(&tag);
        wait_until("fake game visible in /proc", || procs::is_game_running(&selected_game(&h.state).unwrap()));
        api(&h, "/api/bisect_answer", json!({ "crashed": false }));
        assert!(last_log(&h).contains("still running"), "{}", last_log(&h));
        assert_eq!(api(&h, "/api/state", json!({}))["bisect"]["round"], 1, "round unchanged");
        child.kill().unwrap();
        child.wait().unwrap();
        api(&h, "/api/bisect_finish", json!({ "restore_all": true }));
        // and a new session cannot start while the game runs
        let mut child = spawn_fake_game(&tag);
        wait_until("fake game visible in /proc", || procs::is_game_running(&selected_game(&h.state).unwrap()));
        api(&h, "/api/bisect_start", json!({}));
        assert!(last_log(&h).contains("Close it first"));
        child.kill().unwrap();
        child.wait().unwrap();
    }

    fn undo_label(h: &Harness) -> Option<String> {
        api(h, "/api/state", json!({}))["undo"]["label"].as_str().map(str::to_string)
    }

    #[test]
    fn undo_reverts_toggles_bulk_changes_profiles_and_deletions() {
        let h = harness("undo");
        let all = enabled_user_mods(&h);
        assert_eq!(all, vec!["CoolMod", "Skin_P"]);
        assert_eq!(undo_label(&h), None, "nothing to undo at first");

        api(&h, "/api/toggle_mod", json!({ "id": "LUA:CoolMod", "enable": false }));
        assert_eq!(undo_label(&h).as_deref(), Some("Disabled CoolMod"));
        api(&h, "/api/bulk_toggle_mods", json!({ "enable": false }));
        assert_eq!(undo_label(&h).as_deref(), Some("Disabled all mods"));
        assert!(enabled_user_mods(&h).is_empty());
        api(&h, "/api/undo", json!({}));
        assert_eq!(enabled_user_mods(&h), vec!["Skin_P"], "back to just after the first toggle");
        assert_eq!(undo_label(&h).as_deref(), Some("Disabled CoolMod"));
        api(&h, "/api/undo", json!({}));
        assert_eq!(enabled_user_mods(&h), all);
        api(&h, "/api/undo", json!({}));
        assert!(last_log(&h).contains("Nothing to undo"));

        // deleting a PAK (with its .utoc) is undoable and restores its enabled state
        let paks = PathBuf::from(&h.game.game_dir).join("Demo/Content/Paks/~mods");
        api(&h, "/api/delete_mod", json!({ "id": "PAK:Skin_P" }));
        assert!(!paks.join("Skin_P.pak").exists() && !paks.join("Skin_P.utoc").exists());
        assert_eq!(undo_label(&h).as_deref(), Some("Deleted Skin_P"));
        api(&h, "/api/undo", json!({}));
        assert!(paks.join("Skin_P.pak").is_file() && paks.join("Skin_P.utoc").is_file());
        assert_eq!(enabled_user_mods(&h), all);

        // ...and so is deleting a Lua mod, including its mods.txt entry
        api(&h, "/api/delete_mod", json!({ "id": "LUA:CoolMod" }));
        assert!(!PathBuf::from(&h.game.bin_dir).join("ue4ss/Mods/CoolMod").exists());
        api(&h, "/api/undo", json!({}));
        assert!(PathBuf::from(&h.game.bin_dir).join("ue4ss/Mods/CoolMod/Scripts/main.lua").is_file());
        assert_eq!(enabled_user_mods(&h), all);

        // loading a profile can be reverted too
        api(&h, "/api/profile_save", json!({ "name": "both" }));
        api(&h, "/api/bulk_toggle_mods", json!({ "enable": false }));
        api(&h, "/api/profile_load", json!({ "name": "both" }));
        assert_eq!(enabled_user_mods(&h), all);
        assert_eq!(undo_label(&h).as_deref(), Some("Loaded profile 'both'"));
        api(&h, "/api/undo", json!({}));
        assert!(enabled_user_mods(&h).is_empty());

        // the trash is empty once everything has been restored
        let trash = h.state.read().unwrap().cache_root().join("trash");
        assert_eq!(fs::read_dir(&trash).map(|r| r.count()).unwrap_or(0), 0);
    }

    #[test]
    fn the_undo_stack_is_bounded_and_evicted_trash_is_released() {
        let h = harness("undo2");
        let bundle = h._dir.join("trash/b1");
        fs::create_dir_all(&bundle).unwrap();
        fs::write(bundle.join("gone.pak"), b"x").unwrap();
        push_undo(
            &h.state,
            "42",
            "first".into(),
            vec![],
            vec![TrashedItem { original: h._dir.join("orig.pak"), stored: bundle.join("gone.pak") }],
        );
        for i in 0..UNDO_DEPTH {
            push_undo(&h.state, "42", format!("change {i}"), vec![], vec![]);
        }
        assert_eq!(h.state.read().unwrap().undo.len(), UNDO_DEPTH);
        assert_eq!(undo_label(&h).as_deref(), Some("change 19"));
        assert!(!bundle.exists(), "the oldest entry fell off the stack, so its trash was deleted");
    }

    #[test]
    fn undo_only_touches_the_selected_games_changes() {
        let h = harness("undo3");
        push_undo(&h.state, "other-game", "Other change".into(), vec![], vec![]);
        assert_eq!(undo_label(&h), None, "another game's change is not offered");
        api(&h, "/api/undo", json!({}));
        assert!(last_log(&h).contains("Nothing to undo"));
        assert_eq!(h.state.read().unwrap().undo.len(), 1, "and is not consumed");
    }

    fn with_fake_nexus(tag: &str, premium_only: bool) -> (Harness, nexus::fake::FakeNexus) {
        let h = harness(tag);
        let f = nexus::fake::start("Cool Hat_P.pak", vec![0xAB; 4096], premium_only);
        {
            let mut st = h.state.write().unwrap();
            st.nexus.base = f.base.clone();
            st.nexus.allow_http = true;
        }
        (h, f)
    }

    fn connect_nexus(h: &Harness) {
        api(h, "/api/nexus_key_set", json!({ "key": nexus::fake::KEY }));
        wait_until("key accepted", || api(h, "/api/state", json!({}))["nexus"]["configured"] == true);
    }

    const MOD_FILES: &str = r#"{"files":[
        {"file_id":10,"name":"Main","file_name":"Cool Pack-5-1-0-100.zip","version":"1.0","category_name":"MAIN","uploaded_timestamp":100},
        {"file_id":12,"name":"Main","file_name":"Cool Pack-5-2-0-300.zip","version":"2.0","category_name":"MAIN","uploaded_timestamp":300},
        {"file_id":30,"name":"Unrelated","file_name":"Other.zip","version":"9.9","category_name":"MAIN","uploaded_timestamp":900}],
      "file_updates":[{"old_file_id":10,"new_file_id":12,"uploaded_timestamp":300}]}"#;

    /// A plain (non-Unreal) game with one library mod that came from Nexus (file 10), a fake Nexus
    /// that serves version 2.0 as a zip, and the key connected.
    fn nexus_update_setup(tag: &str, premium: bool) -> (Harness, nexus::fake::FakeNexus, PathBuf, String) {
        let h = harness(tag);
        let zip = zip_of(&h, "v2", &[("Data/new.esp", "V2"), ("Data/extra.esp", "E")]);
        let f = nexus::fake::start("Cool Pack-5-2-0-300.zip", zip, true);
        *f.files_json.lock().unwrap() = MOD_FILES.to_string();
        f.premium.store(premium, Ordering::SeqCst);
        {
            let mut st = h.state.write().unwrap();
            st.nexus.base = f.base.clone();
            st.nexus.allow_http = true;
        }
        let dir = add_plain_game(&h);
        let g = h.state.read().unwrap().games.last().unwrap().clone();
        let lib = library_of(&g).unwrap();
        let stage = h._dir.join("v1-stage");
        fs::create_dir_all(stage.join("Data")).unwrap();
        fs::write(stage.join("Data/new.esp"), "V1").unwrap();
        let r = nexus::NexusRef { game: "coolgame".into(), mod_id: 5, file_id: 10 };
        let m = lib.add_with(&stage, "Cool Pack", "Cool Pack-5-1-0-100.zip", Some(r), "1.0").unwrap();
        // a second mod without a Nexus origin must never be looked up
        let s2 = h._dir.join("manual-stage");
        fs::create_dir_all(s2.join("Data")).unwrap();
        fs::write(s2.join("Data/manual.esp"), "M").unwrap();
        lib.add_from_dir(&s2, "Manual", "manual.zip").unwrap();
        lib.deploy(false).unwrap();
        refresh_mods_shared(&h.state);
        connect_nexus(&h);
        (h, f, dir, m.id)
    }

    fn nexus_state(h: &Harness) -> Value {
        api(h, "/api/state", json!({}))["nexus"].clone()
    }


    fn page_of(h: &Harness, id: &str) -> Value {
        let (code, text) = http(h, "GET", &format!("/api/mod_page?id={id}"), &[("X-Rig-Token", &h.token)], b"");
        assert_eq!(code, 200, "{text}");
        serde_json::from_str(&text).unwrap()
    }

    #[test]
    fn the_mod_page_shows_the_authors_nexus_page_and_survives_the_site_being_down() {
        if fsutil::find_in_path("bsdtar").is_none() {
            eprintln!("skipping: bsdtar missing");
            return;
        }
        let _gate = modinfo::forget_all();
        let (h, f, _dir, id) = nexus_update_setup("modpage", true);
        *f.mod_json.lock().unwrap() = r#"{"name":"Cool Pack","summary":"A <b>cool</b> pack","description":"[b]Install[/b] it.<br />[url=https://example.com/docs]docs[/url] [img]https://evil.example/t.png[/img]<script>alert(1)</script>","version":"2.0","author":"Someone","endorsement_count":12,"mod_downloads":3400,"created_timestamp":100,"updated_timestamp":300,"available":true,"contains_adult_content":false,"picture_url":"https://staticdelivery.nexusmods.com/mods/1/p.jpg"}"#.into();
        *f.files_json.lock().unwrap() = r#"{"files":[{"file_id":12,"name":"Main","file_name":"x.zip","version":"2.0","category_name":"MAIN","uploaded_timestamp":300,"changelog_html":"<li>fixed the crash</li>"},{"file_id":10,"name":"Main","file_name":"y.zip","version":"1.0","category_name":"OLD_VERSION","uploaded_timestamp":100,"changelog_html":"ancient"}],"file_updates":[]}"#.into();
        *f.requirements_json.lock().unwrap() = r#"[{"modId":"5","modName":"Itself","gameId":"1","url":"","notes":"","externalRequirement":false},{"modId":"77","modName":"Base Lib","gameId":"1","url":"","notes":"Install first","externalRequirement":false}]"#.into();
        let v = page_of(&h, &id);
        assert_eq!(v["error"], Value::Null, "{v}");
        assert_eq!((v["source"].as_str(), v["name"].as_str(), v["author"].as_str(), v["version"].as_str()), (Some("nexus"), Some("Cool Pack"), Some("Someone"), Some("2.0")));
        assert_eq!((v["endorsements"].as_u64(), v["downloads"].as_u64()), (Some(12), Some(3400)));
        let d = v["description_html"].as_str().unwrap();
        assert!(d.contains("<strong>Install</strong>") && d.contains("data-url=\"https://example.com/docs\""), "{d}");
        assert!(!d.contains("<script") && !d.contains("<img"), "hostile parts are inert (a picture from an unknown site is only a link): {d}");
        assert_eq!(v["changes"].as_array().unwrap().len(), 1, "old versions are left out");
        assert!(v["changes"][0]["html"].as_str().unwrap().contains("<li>fixed the crash</li>"));
        let reqs = v["requirements"].as_array().unwrap();
        assert_eq!((reqs[0]["name"].as_str(), reqs[0]["installed"].as_bool()), (Some("Itself"), Some(true)), "the library has mod 5");
        assert_eq!((reqs[1]["name"].as_str(), reqs[1]["installed"].as_bool()), (Some("Base Lib"), Some(false)));
        assert_eq!(v["installed"]["name"], "Cool Pack");
        assert!(v["installed"]["file_count"].as_u64().unwrap() >= 1);
        // asking again costs no request
        let before = f.seen.lock().unwrap().requests.len();
        page_of(&h, &id);
        assert_eq!(f.seen.lock().unwrap().requests.len(), before, "kept for a while");
        // the site goes away: the kept copy is shown, marked old
        modinfo::forget_pages();
        let v2 = page_of(&h, &id);
        assert_eq!(v2["name"], "Cool Pack");
        // a hand-added mod has a page too (no online source)
        let manual = library_of(&selected_game(&h.state).unwrap()).unwrap().mods().into_iter().find(|m| m.name == "Manual").unwrap();
        let local = page_of(&h, &manual.id);
        assert_eq!((local["source"].as_str(), local["name"].as_str(), local["online_error"].as_str()), (Some("local"), Some("Manual"), Some("")));
        assert_eq!(page_of(&h, "nope")["error"], "That mod is not in the library any more.");
    }

    #[test]
    fn a_nexus_mod_page_without_a_key_falls_back_to_what_gantry_knows() {
        let _gate = modinfo::forget_all();
        let h = harness("modpage_nokey");
        let _dir = add_plain_game(&h);
        let g = selected_game(&h.state).unwrap();
        let lib = library_of(&g).unwrap();
        let stage = h._dir.join("mp-stage");
        fs::create_dir_all(stage.join("Data")).unwrap();
        fs::write(stage.join("Data/a.esp"), "A").unwrap();
        let m = lib.add_with(&stage, "From Nexus", "From Nexus-9-1-0-1.zip", Some(nexus::NexusRef { game: "coolgame".into(), mod_id: 9, file_id: 1 }), "1.0").unwrap();
        refresh_mods_shared(&h.state);
        let v = page_of(&h, &m.id);
        assert_eq!((v["source"].as_str(), v["name"].as_str()), (Some("local"), Some("From Nexus")));
        assert!(v["online_error"].as_str().unwrap().contains("API key"), "{v}");
    }

    #[test]
    fn the_mod_page_of_a_thunderstore_mod_has_its_readme_and_dependencies() {
        let _gate = modinfo::forget_all();
        let h = harness("modpage_ts");
        let _dir = add_plain_game(&h);
        let f = thunderstore::fake::start();
        {
            let mut st = h.state.write().unwrap();
            st.thunder.base = f.base.clone();
            st.thunder.allow_http = true;
        }
        f.packages.lock().unwrap().insert("Own-Cool_Mod".into(), vec![("1.0.0".into(), vec!["bbepis-BepInExPack-5.4.2100".into()])]);
        f.pages.lock().unwrap().insert("/api/experimental/package/Own/Cool_Mod/1.0.0/readme/".into(), r##"{"markdown":"# Cool\n\nDrop it **in**. <script>x</script>"}"##.into());
        let g = selected_game(&h.state).unwrap();
        let lib = library_of(&g).unwrap();
        let stage = h._dir.join("ts-stage");
        fs::create_dir_all(&stage).unwrap();
        fs::write(stage.join("CoolMod.dll"), "d").unwrap();
        let m = lib.add_with(&stage, "Cool Mod", "Own-Cool_Mod.zip", None, "1.0.0").unwrap();
        lib.set_thunderstore(&m.id, thunderstore::ThunderRef { namespace: "Own".into(), name: "Cool_Mod".into(), version: "1.0.0".into() }).unwrap();
        refresh_mods_shared(&h.state);
        let v = page_of(&h, &m.id);
        assert_eq!((v["source"].as_str(), v["author"].as_str(), v["version"].as_str()), (Some("thunderstore"), Some("Own"), Some("1.0.0")), "{v}");
        let d = v["description_html"].as_str().unwrap();
        assert!(d.contains("<h3>Cool</h3>") && d.contains("<strong>in</strong>") && !d.contains("<script"), "{d}");
        assert_eq!(v["requirements"][0]["name"], "BepInExPack 5.4.2100");
        assert_eq!(v["requirements"][0]["installed"], false);
    }


    fn social(h: &Harness, id: &str, act: &str) -> Value {
        let (code, text) = http(h, "GET", &format!("/api/nexus_social?id={id}&act={act}"), &[("X-Rig-Token", &h.token)], b"");
        assert_eq!(code, 200, "{text}");
        serde_json::from_str(&text).unwrap()
    }

    #[test]
    fn a_nexus_mod_can_be_endorsed_and_tracked_from_its_page() {
        if fsutil::find_in_path("bsdtar").is_none() {
            eprintln!("skipping: bsdtar missing");
            return;
        }
        let _gate = modinfo::forget_all();
        forget_nexus_social();
        let (h, f, _dir, id) = nexus_update_setup("social", true);
        let v = page_of(&h, &id);
        assert_eq!(v["nexus_user"], json!({ "endorsed": "Undecided", "tracked": false }), "{v}");
        let r = social(&h, &id, "endorse");
        assert_eq!(r["nexus_user"]["endorsed"], "Endorsed", "{r}");
        assert!(f.writes.lock().unwrap().iter().any(|w| w.starts_with("POST /v1/games/coolgame/mods/5/endorse.json Version=1.0")), "the installed version is sent");
        assert_eq!(social(&h, &id, "abstain")["nexus_user"]["endorsed"], "Abstained");
        assert_eq!(social(&h, &id, "track")["nexus_user"]["tracked"], true);
        assert_eq!(f.tracked.lock().unwrap().clone(), vec!["coolgame:5".to_string()]);
        assert_eq!(social(&h, &id, "untrack")["nexus_user"]["tracked"], false);
        assert!(social(&h, &id, "nonsense")["error"].is_string());
        // a mod that did not come from Nexus has no such buttons
        let manual = library_of(&selected_game(&h.state).unwrap()).unwrap().mods().into_iter().find(|m| m.name == "Manual").unwrap();
        assert_eq!(page_of(&h, &manual.id)["nexus_user"], Value::Null);
        assert!(social(&h, &manual.id, "endorse")["error"].as_str().unwrap().contains("did not come from Nexus"));
    }

    #[test]
    fn tracked_mods_show_up_in_the_mod_browser() {
        if fsutil::find_in_path("bsdtar").is_none() {
            eprintln!("skipping: bsdtar missing");
            return;
        }
        let _gate = modinfo::forget_all();
        let (h, f, _dir, _id) = nexus_update_setup("tracked", true);
        let appid = selected_game(&h.state).unwrap().appid;
        h.state.write().unwrap().config.nexus_domain.insert(appid, "coolgame".into());
        f.tracked.lock().unwrap().extend(["coolgame:5".to_string(), "other:9".to_string(), "coolgame:6".to_string()]);
        *f.mod_json.lock().unwrap() = r#"{"name":"Tracked One","summary":"Nice","version":"3","author":"a","endorsement_count":4,"available":true,"contains_adult_content":false}"#.into();
        let (code, text) = http(&h, "GET", "/api/nexus_browse?list=tracked", &[("X-Rig-Token", &h.token)], b"");
        assert_eq!(code, 200);
        let v: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(v["error"], Value::Null, "{v}");
        let ids: Vec<u64> = v["mods"].as_array().unwrap().iter().map(|m| m["mod_id"].as_u64().unwrap()).collect();
        assert_eq!(ids, vec![5, 6], "only this game's tracked mods");
        assert_eq!(v["mods"][0]["name"], "Tracked One");
    }

    #[test]
    fn a_collection_lists_what_is_missing_and_premium_installs_it() {
        if fsutil::find_in_path("bsdtar").is_none() {
            eprintln!("skipping: bsdtar missing");
            return;
        }
        let _gate = modinfo::forget_all();
        let (h, f, _dir, _id) = nexus_update_setup("collection", true);
        let appid = selected_game(&h.state).unwrap().appid;
        h.state.write().unwrap().config.nexus_domain.insert(appid, "coolgame".into());
        *f.collection_json.lock().unwrap() = r#"{"revisionNumber":2,"modCount":3,"collection":{"name":"Cool Set","slug":"abc123","game":{"domainName":"coolgame","name":"Cool Game"}},
            "modFiles":[{"optional":false,"fileId":10,"file":{"fileId":10,"name":"Cool Pack","version":"1.0","sizeInBytes":100,"mod":{"modId":5,"name":"Cool Pack","game":{"domainName":"coolgame"}}}},
                        {"optional":false,"fileId":71,"file":{"fileId":71,"name":"New Thing","version":"1.0","sizeInBytes":200,"mod":{"modId":7,"name":"New Thing","game":{"domainName":"coolgame"}}}},
                        {"optional":true,"fileId":81,"file":{"fileId":81,"name":"Extra","version":"1.0","sizeInBytes":300,"mod":{"modId":8,"name":"Extra","game":{"domainName":"coolgame"}}}}]}"#.into();
        let url = "https://www.nexusmods.com/games/coolgame/collections/abc123";
        let (code, text) = http(&h, "GET", &format!("/api/collection?url={}", fsutil::url_encode(url)), &[("X-Rig-Token", &h.token)], b"");
        assert_eq!(code, 200);
        let v: Value = serde_json::from_str(&text).unwrap();
        assert_eq!((v["name"].as_str(), v["revision"].as_u64(), v["other_game"].as_bool(), v["premium"].as_bool()), (Some("Cool Set"), Some(2), Some(false), Some(true)), "{v}");
        assert!(v["free_bytes"].as_u64().is_some(), "the free space of the game's drive comes along: {v}");
        let inst: Vec<(u64, bool)> = v["mods"].as_array().unwrap().iter().map(|m| (m["mod_id"].as_u64().unwrap(), m["installed"].as_bool().unwrap())).collect();
        assert_eq!(inst, vec![(5, true), (7, false), (8, false)], "mod 5 is in the library already");
        assert!(v["mods"][1]["page_url"].as_str().unwrap().ends_with("/coolgame/mods/7?tab=files&file_id=71&nmm=1"));
        assert!(http(&h, "GET", "/api/collection?url=nonsense", &[("X-Rig-Token", &h.token)], b"").1.contains("does not look like"));
        // premium: the missing, non-optional mod is installed
        api(&h, "/api/collection_install", json!({ "url": url, "optional": false }));
        wait_until("the collection's missing mod arrives", || library_of(&selected_game(&h.state).unwrap()).unwrap().mods().iter().any(|m| m.nexus.as_ref().map(|n| (n.mod_id, n.file_id)) == Some((7, 71))));
        let mods = library_of(&selected_game(&h.state).unwrap()).unwrap().mods();
        assert!(!mods.iter().any(|m| m.nexus.as_ref().map(|n| n.mod_id) == Some(8)), "optional mods wait unless asked for");
        wait_until("the run is reported", || h.state.read().unwrap().logs.iter().any(|l| l.message.contains("Collection “Cool Set” done")));
        // a collection of another game is refused
        let appid = selected_game(&h.state).unwrap().appid;
        h.state.write().unwrap().config.nexus_domain.insert(appid, "othergame".into());
        let (_, text) = http(&h, "GET", &format!("/api/collection?url={}", fsutil::url_encode(url)), &[("X-Rig-Token", &h.token)], b"");
        assert_eq!(serde_json::from_str::<Value>(&text).unwrap()["other_game"], true);
    }

    #[test]
    fn the_update_message_names_the_mods_and_what_changed() {
        let row = |name: &str, old: &str, new: &str, changes: &str| UpdateRow { appid: "1".into(), game: "g".into(), lib_id: "x".into(), name: name.into(), installed_version: old.into(), new_version: new.into(), new_file_id: 1, new_file_name: "f.zip".into(), changes: changes.into(), nexus: nexus::NexusRef { game: "g".into(), mod_id: 1, file_id: 1 }, page_url: String::new() };
        let t = |name: &str| ThunderUpdate { appid: "1".into(), game: "g".into(), lib_id: "t".into(), name: name.into(), installed_version: "1.0".into(), new_version: "1.1".into(), pkg: thunderstore::ThunderRef { namespace: "o".into(), name: "n".into(), version: "1.1".into() } };
        assert_eq!(update_summary(&[], &[]), "");
        let s = update_summary(&[row("Cool Pack", "1.0", "2.0", "\n- fixed the crash\n- more")], &[t("Lib")]);
        assert_eq!(s, "Cool Pack v1.0 → v2.0: fixed the crash\nLib v1.0 → v1.1");
        let many = update_summary(&[row("A", "1", "2", ""), row("B", "1", "2", ""), row("C", "1", "2", "")], &[t("D"), t("E")]);
        assert_eq!(many.lines().count(), 4);
        assert!(many.ends_with("+2"), "{many}");
        assert!(update_summary(&[row("X", "1", "2", &"very long ".repeat(30))], &[]).chars().count() < 120);
        // the notice lists every source in one line, naming the game when there are several
        let h = harness("digest");
        {
            let mut st = h.state.write().unwrap();
            st.nexus.updates = vec![row("Cool Pack", "1.0", "2.0", "- fixed the crash")];
            st.thunder.updates = vec![t("Lib")];
            assert_eq!(update_digest(&st), "Cool Pack v1.0 → v2.0: fixed the crash; Lib v1.0 → v1.1");
            st.thunder.updates[0].game = "Other".into();
            assert_eq!(update_digest(&st), "Cool Pack v1.0 → v2.0 (g): fixed the crash; Lib v1.0 → v1.1 (Other)");
            st.nexus.updates = (0..10).map(|i| row(&format!("M{i}"), "1", "2", "")).collect();
            assert!(update_digest(&st).ends_with("; +3"), "{}", update_digest(&st));
        }
    }


    #[test]
    fn a_game_is_found_by_id_or_name_for_the_command_line() {
        let g = |id: &str, n: &str| (id.to_string(), n.to_string());
        let games = vec![g("3164500", "Schedule I"), g("42", "Need for Speed Most Wanted"), g("43", "Need for Speed Heat"), g("hydra_x", "Half Sword")];
        assert_eq!(pick_game(&games, "3164500"), Ok(0));
        assert_eq!(pick_game(&games, "half sword"), Ok(3));
        assert_eq!(pick_game(&games, "schedule"), Ok(0), "a part of the name is enough when it is unique");
        assert!(pick_game(&games, "need for speed").unwrap_err().contains("fits several games"));
        assert_eq!(pick_game(&games, "most wanted"), Ok(1));
        assert!(pick_game(&games, "nothing").unwrap_err().contains("No game called"));
        assert!(pick_game(&games, "").is_err());
    }

    #[test]
    fn the_command_line_can_start_a_game_through_the_running_gantry() {
        let h = harness("cliplay");
        let _dir = add_plain_game(&h);
        let port = h.addr.port();
        let name = selected_game(&h.state).unwrap().name;
        // an unknown game is an error and starts nothing
        assert!(play_via_running(port, "no such game", "mods").is_err());
        // "play without mods" is safe to run for real in a test: the mods come out and the session ends by itself
        let started = play_via_running(port, &name, "vanilla").unwrap();
        assert_eq!(started, name);
        wait_until("the session was started", || !h.state.read().unwrap().config.vanilla_sessions.is_empty() || h.state.read().unwrap().logs.iter().any(|l| l.message.contains("without mods") || l.message.contains("vanilla")));
        // not Gantry at all
        assert!(play_via_running(1, &name, "mods").is_err());
    }

    #[test]
    fn a_game_shortcut_is_written_where_asked_and_never_into_the_real_home() {
        let h = harness("gshort");
        let _dir = add_plain_game(&h);
        let home = h._dir.join("fakehome");
        fs::create_dir_all(home.join("Desktop")).unwrap();
        h.state.write().unwrap().shortcut_home = Some(home.clone());
        let appid = selected_game(&h.state).unwrap().appid;
        api(&h, "/api/game_shortcut", json!({ "appid": appid, "desktop": true }));
        if cfg!(target_os = "linux") {
            let entries: Vec<_> = fs::read_dir(home.join(".local/share/applications")).unwrap().flatten().collect();
            assert_eq!(entries.len(), 1);
            let t = fs::read_to_string(entries[0].path()).unwrap();
            assert!(t.contains(&format!("--play {appid}")), "{t}");
            assert!(fs::read_dir(home.join("Desktop")).unwrap().count() == 1);
        }
    }


    #[test]
    fn one_search_covers_the_library_nexus_and_thunderstore() {
        if fsutil::find_in_path("bsdtar").is_none() {
            eprintln!("skipping: bsdtar missing");
            return;
        }
        let _gate = modinfo::forget_all();
        let (h, f, _dir, _id) = nexus_update_setup("findall", true);
        let appid = selected_game(&h.state).unwrap().appid;
        h.state.write().unwrap().config.nexus_domain.insert(appid.clone(), "coolgame".into());
        *f.search_json.lock().unwrap() = r#"[{"modId":"7","name":"Cool Cars","summary":"cars","version":"1","author":"n","endorsements":5,"adultContent":false},{"modId":"8","name":"Cool Skins","summary":"","version":"2","author":"n2","endorsements":3,"adultContent":false}]"#.into();
        let t = thunderstore::fake::start();
        {
            let mut st = h.state.write().unwrap();
            st.thunder.base = t.base.clone();
            st.thunder.allow_http = true;
            st.config.thunder_community.insert(appid, "coolcommunity".into());
        }
        t.pages.lock().unwrap().insert("/c/coolcommunity/api/v1/package/".into(), r#"[{"name":"Cool_Mod","owner":"Own","is_deprecated":false,"has_nsfw_content":false,"rating_score":1,"date_updated":"2026-01-01T00:00:00Z","categories":[],"versions":[{"version_number":"1.0.0","description":"Makes things cool","icon":"","downloads":50}]}]"#.into());
        let get = |q: &str| -> Value {
            let (code, text) = http(&h, "GET", &format!("/api/find_all?q={}", fsutil::url_encode(q)), &[("X-Rig-Token", &h.token)], b"");
            assert_eq!(code, 200);
            serde_json::from_str(&text).unwrap()
        };
        let v = get("cool");
        let order: Vec<String> = v["results"].as_array().unwrap().iter().map(|r| format!("{}:{}", r["source"].as_str().unwrap(), r["name"].as_str().unwrap())).collect();
        assert_eq!(order, vec!["library:Cool Pack", "nexus:Cool Cars", "thunderstore:Cool Mod", "nexus:Cool Skins"], "{v}");
        assert_eq!(v["results"][0]["installed"], true);
        assert!(v["notes"].as_array().unwrap().is_empty(), "{v}");
        // a short query searches nothing
        assert!(get("c")["results"].as_array().unwrap().is_empty());
        // the Nexus tab searches by name too
        let (_, text) = http(&h, "GET", "/api/nexus_browse?list=trending&q=cool", &[("X-Rig-Token", &h.token)], b"");
        let nb: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(nb["mods"][0]["name"], "Cool Cars", "{nb}");
        // the same mod on two sites is listed once, naming the other site
        *f.search_json.lock().unwrap() = r#"[{"modId":"9","name":"Cool-Mod","summary":"","version":"1","author":"Own","endorsements":1,"adultContent":false}]"#.into();
        let v = get("cool mod");
        let hits: Vec<&Value> = v["results"].as_array().unwrap().iter().filter(|r| r["name"].as_str().unwrap().to_lowercase().replace(['-', ' '], "") == "coolmod").collect();
        assert_eq!(hits.len(), 1, "{v}");
        assert_eq!(hits[0]["also"].as_array().map(|a| a.len()), Some(1), "{v}");
        // a site that cannot answer is named, the others still answer
        h.state.write().unwrap().nexus.key = None;
        let v2 = get("cool");
        assert!(v2["notes"][0].as_str().unwrap().starts_with("Nexus Mods:"), "{v2}");
        assert!(v2["results"].as_array().unwrap().iter().any(|r| r["source"] == "thunderstore"));
    }


    #[test]
    fn the_store_lists_both_sites_in_one_order_and_hides_adult_mods_until_asked() {
        if fsutil::find_in_path("bsdtar").is_none() {
            eprintln!("skipping: bsdtar missing");
            return;
        }
        let _gate = modinfo::forget_all();
        let (h, f, _dir, _id) = nexus_update_setup("store", true);
        let appid = selected_game(&h.state).unwrap().appid;
        h.state.write().unwrap().config.nexus_domain.insert(appid.clone(), "coolgame".into());
        *f.search_json.lock().unwrap() = r#"[{"modId":"7","name":"Cool Cars","summary":"cars","version":"1","author":"n","endorsements":5,"downloads":900,"updatedAt":"2025-03-02T03:15:43Z","fileSize":2048,"modCategory":{"name":"Vehicles"},"thumbnailLargeUrl":"https://images.nexusmods.com/x.png","adultContent":false},{"modId":"8","name":"Spicy Skins","summary":"","version":"2","author":"n2","endorsements":300,"downloads":5000,"adultContent":true},{"modId":"9","name":"Cool Skins","summary":"","version":"2","author":"n2","endorsements":3,"downloads":40,"adultContent":false}]"#.into();
        let t = thunderstore::fake::start();
        {
            let mut st = h.state.write().unwrap();
            st.thunder.base = t.base.clone();
            st.thunder.allow_http = true;
            st.config.thunder_community.insert(appid, "coolcommunity".into());
        }
        t.pages.lock().unwrap().insert("/c/coolcommunity/api/v1/package/".into(), r#"[{"name":"Cool_Mod","owner":"Own","is_deprecated":false,"has_nsfw_content":false,"rating_score":1,"date_updated":"2026-01-01T00:00:00Z","categories":["Mods"],"versions":[{"version_number":"1.0.0","description":"Makes things cool","icon":"","downloads":2000}]},{"name":"Naughty","owner":"Own","is_deprecated":false,"has_nsfw_content":true,"rating_score":50,"date_updated":"2026-01-02T00:00:00Z","categories":["Mods"],"versions":[{"version_number":"1.0.0","description":"adult","icon":"","downloads":99999}]}]"#.into());
        let get = |q: &str| -> Value {
            let (code, text) = http(&h, "GET", &format!("/api/store?{q}"), &[("X-Rig-Token", &h.token)], b"");
            assert_eq!(code, 200);
            serde_json::from_str(&text).unwrap()
        };
        let names = |v: &Value| v["cards"].as_array().unwrap().iter().map(|c| format!("{}:{}", c["source"].as_str().unwrap(), c["name"].as_str().unwrap())).collect::<Vec<_>>();
        // the fake answers every Nexus list the same way; the adult mod is dropped even though the server sent it
        let v = get("sort=popular");
        assert_eq!(names(&v), vec!["thunderstore:Cool Mod", "nexus:Cool Cars", "nexus:Cool Skins"], "most downloaded first, both sites together: {v}");
        let c = &v["cards"][1];
        assert_eq!((c["downloads"].as_u64(), c["endorsements"].as_u64(), c["size_kb"].as_u64(), c["category"].as_str()), (Some(900), Some(5), Some(2048), Some("Vehicles")));
        assert!(c["updated_ms"].as_u64().unwrap() > 1_700_000_000_000, "{c}");
        assert_eq!(c["page_url"], "https://www.nexusmods.com/coolgame/mods/7");
        assert_eq!(get("sort=popular&source=nexus")["cards"].as_array().unwrap().len(), 2);
        assert_eq!(names(&get("sort=popular&source=thunder")), vec!["thunderstore:Cool Mod"]);
        assert_eq!(names(&get("sort=endorsed&source=thunder&category=nothing")).len(), 0, "a category filters");
        // the switch for adult mods
        api(&h, "/api/settings", json!({ "show_adult": true }));
        let v = get("sort=popular");
        assert_eq!(names(&v)[0], "thunderstore:Naughty", "{v}");
        assert!(names(&v).contains(&"nexus:Spicy Skins".to_string()), "{v}");
        api(&h, "/api/settings", json!({ "show_adult": false }));
        // a library mod shows as installed; paging moves on
        let v = get("per=1&source=thunder");
        assert_eq!(v["more"], false);
        assert_eq!(v["next"]["t"], 1);
        // a site that cannot answer is named, the other still answers
        h.state.write().unwrap().nexus.key = None;
        let v = get("sort=popular");
        assert_eq!(v["notes"][0]["source"], "nexus");
        assert_eq!(names(&v), vec!["thunderstore:Cool Mod"]);
    }

    #[test]
    fn the_home_shelf_shows_what_is_trending_for_the_games_played_last() {
        if fsutil::find_in_path("bsdtar").is_none() {
            eprintln!("skipping: bsdtar missing");
            return;
        }
        let _gate = modinfo::forget_all();
        let (h, f, _dir, _id) = nexus_update_setup("trendshelf", true);
        let appid = selected_game(&h.state).unwrap().appid;
        h.state.write().unwrap().config.nexus_domain.insert(appid.clone(), "coolgame".into());
        *f.search_json.lock().unwrap() = r#"[{"modId":"7","name":"Cool Cars","summary":"cars","version":"1","author":"n","endorsements":5,"downloads":900,"updatedAt":"2025-03-02T03:15:43Z","adultContent":false},{"modId":"9","name":"Cool Skins","summary":"","version":"2","author":"n2","endorsements":3,"downloads":40,"adultContent":false}]"#.into();
        let get = |q: &str| -> Value {
            let (code, text) = http(&h, "GET", &format!("/api/home_trending{q}"), &[("X-Rig-Token", &h.token)], b"");
            assert_eq!(code, 200);
            serde_json::from_str(&text).unwrap()
        };
        // nothing played yet, nothing to show
        assert!(get("")["games"].as_array().unwrap().is_empty());
        h.state.write().unwrap().config.last_launched.insert(appid.clone(), now_ms());
        let v = get("?refresh=1");
        assert_eq!(v["games"].as_array().unwrap().len(), 1, "{v}");
        let names: Vec<&str> = v["games"][0]["cards"].as_array().unwrap().iter().map(|c| c["name"].as_str().unwrap()).collect();
        assert_eq!(names, vec!["Cool Cars", "Cool Skins"]);
        assert_eq!(v["games"][0]["appid"], json!(appid));
        // it is remembered for a while: a change on the sites does not show until it is asked for again
        *f.search_json.lock().unwrap() = "[]".into();
        assert_eq!(get("")["games"][0]["cards"].as_array().unwrap().len(), 2);
        assert!(get("?refresh=1")["games"].as_array().unwrap().is_empty());
        // the key is gone: an empty shelf, not an error
        h.state.write().unwrap().nexus.key = None;
        assert!(get("?refresh=1")["games"].as_array().unwrap().is_empty());
    }

    #[test]
    fn a_shared_setup_is_shown_before_anything_is_fetched_and_a_share_code_travels_as_text() {
        if fsutil::find_in_path("bsdtar").is_none() {
            eprintln!("skipping: bsdtar missing");
            return;
        }
        let h = harness("preview");
        let from = h.game.clone();
        let src = library_of(&from).unwrap();
        let stage = |n: &str, f: &str| {
            let d = h._dir.join(&format!("pv-{n}"));
            fs::create_dir_all(d.join("Data")).unwrap();
            fs::write(d.join("Data").join(f), n).unwrap();
            d
        };
        src.add_with(&stage("own", "own.esp"), "My Own Mod", "own.zip", None, "1.0").unwrap();
        src.add_with(&stage("nx", "nx.esp"), "Nexus Mod", "nx.zip", Some(nexus::NexusRef { game: "coolgame".into(), mod_id: 42, file_id: 7 }), "2.0").unwrap();
        let (c, b) = http(&h, "GET", "/api/share_code", &[("X-Rig-Token", &h.token)], b"");
        assert_eq!(c, 200);
        let share: Value = serde_json::from_str(&b).unwrap();
        assert_eq!((share["mods"].as_u64(), share["left_out"].as_u64()), (Some(1), Some(1)), "{share}");
        let pack = h._dir.join("my.gantrypack");
        export_pack_file(&h.state, &from, "Cool setup", &pack, false).unwrap();

        add_plain_game(&h);
        let preview = || api(&h, "/api/state", json!({}))["import_preview"].clone();
        let packs = || fs::read_dir(read_state(&h.state).cache_root().join("pack")).map(|d| d.count()).unwrap_or(0);
        let drop = || assert_eq!(http(&h, "POST", "/api/drop_upload?name=my.gantrypack", &[("X-Rig-Token", &h.token)], &fs::read(&pack).unwrap()).0, 200);
        // dropped on the window: shown, nothing added yet
        drop();
        wait_until("the preview", || !preview().is_null());
        let p = preview();
        assert_eq!((p["name"].as_str(), p["same_game"].as_bool(), p["counts"].clone()), (Some("Cool setup"), Some(false), json!({ "included": 1, "nexus": 1 })), "{p}");
        let lib = library_of(&selected_game(&h.state).unwrap()).unwrap();
        assert!(lib.mods().is_empty());
        // no: nothing changes and the unpacked files go away
        api(&h, "/api/import_confirm", json!({ "accept": false }));
        wait_until("closed", || preview().is_null());
        wait_until("the unpacked modpack is gone", || packs() == 0);
        assert!(lib.mods().is_empty());
        // yes: the mod that came with its files is in, the Nexus one waits in the list
        drop();
        wait_until("the preview again", || !preview().is_null());
        api(&h, "/api/import_confirm", json!({ "accept": true }));
        wait_until("imported", || lib.mods().len() == 1);
        assert_eq!(lib.mods()[0].name, "My Own Mod");
        wait_until("the rest is listed", || api(&h, "/api/state", json!({}))["library"]["pending_pack"]["mods"][0]["name"] == "Nexus Mod");
        assert_eq!(packs(), 0);

        // the share code says the same, without the hand-made mod
        api(&h, "/api/profile_code", json!({ "code": share["code"] }));
        wait_until("the code's preview", || !preview().is_null());
        assert_eq!(preview()["counts"], json!({ "nexus": 1 }));
        api(&h, "/api/import_confirm", json!({ "accept": false }));
        wait_until("closed", || preview().is_null());
        api(&h, "/api/profile_code", json!({ "code": "GANTRY1:broken" }));
        wait_until("the refusal", || last_log(&h).contains("damaged"));
        assert!(preview().is_null());
    }

    #[test]
    fn a_modpack_carries_hand_added_mods_and_links_the_rest_and_nexus_mods_fall_into_place_when_they_arrive() {
        if fsutil::find_in_path("bsdtar").is_none() {
            eprintln!("skipping: bsdtar missing");
            return;
        }
        let h = harness("modpack");
        let from = h.game.clone();
        let src = library_of(&from).unwrap();
        let stage = |n: &str, f: &str, c: &str| {
            let d = h._dir.join(&format!("mp-{n}"));
            fs::create_dir_all(d.join("Data")).unwrap();
            fs::write(d.join("Data").join(f), c).unwrap();
            d
        };
        src.add_with(&stage("own", "own.esp", "OWN"), "My Own Mod", "own.zip", None, "1.0").unwrap();
        let nx = nexus::NexusRef { game: "coolgame".into(), mod_id: 42, file_id: 7 };
        src.add_with(&stage("nx", "nx.esp", "NX"), "Nexus Mod", "nx.zip", Some(nx.clone()), "2.0").unwrap();
        let own_id = src.mods().iter().find(|m| m.name == "My Own Mod").unwrap().id.clone();
        src.set_group(&[own_id], "Mine").unwrap();
        src.set_enabled(&src.mods()[1].id, false).unwrap();

        let pack = h._dir.join("my.gantrypack");
        let msg = export_pack_file(&h.state, &from, "Cool setup", &pack, false).unwrap();
        assert!(msg.contains("1 with their files"), "{msg}");
        // inside: the list, the files of the hand-added mod only
        let listing = String::from_utf8_lossy(&Command::new(tar_tool()).arg("-tf").arg(&pack).output().unwrap().stdout).to_string();
        assert!(listing.contains("pack.json") && listing.contains("own.esp") && !listing.contains("nx.esp"), "{listing}");

        // another game takes it in
        let to = add_plain_game(&h);
        let to_game = game_by_appid(&h.state, "plain1").unwrap();
        let (spec, files) = read_pack_file(&h.state, &pack).unwrap();
        assert_eq!((spec.name.as_str(), spec.mods.len()), ("Cool setup", 2));
        let lib = library_of(&to_game).unwrap();
        import_pack(&h.state, &to_game, spec, files).unwrap();
        let mods = lib.mods();
        assert_eq!(mods.iter().map(|m| m.name.as_str()).collect::<Vec<_>>(), vec!["My Own Mod"], "the hand-added one came with its files, the Nexus one is still to get");
        assert_eq!((mods[0].group.as_str(), mods[0].version.as_str()), ("Mine", "1.0"));
        assert!(to.join("Data").exists());
        let pending = pending_pack_json(&h.state, &to_game);
        assert_eq!(pending["name"], "Cool setup");
        assert_eq!(pending["mods"][0]["nexus"]["url"], "https://www.nexusmods.com/coolgame/mods/42");
        // the Nexus mod arrives later: it takes its place (after the first) and its state (off), and the list clears
        lib.add_with(&stage("nx2", "nx.esp", "NX"), "Nexus Mod", "nx.zip", Some(nx), "2.0").unwrap();
        library_apply(&h.state, &lib, false).unwrap();
        let mods = lib.mods();
        assert_eq!(mods.iter().map(|m| (m.name.as_str(), m.enabled)).collect::<Vec<_>>(), vec![("My Own Mod", true), ("Nexus Mod", false)]);
        assert!(pending_pack_json(&h.state, &to_game).is_null());
        // not a modpack at all
        fs::write(h._dir.join("junk.gantrypack"), "PK\x03\x04nonsense").unwrap();
        assert!(read_pack_file(&h.state, &h._dir.join("junk.gantrypack")).is_err());
    }

    #[test]
    fn export_modpack_full_bundle_includes_all_files_for_offline_sharing() {
        let h = harness("fullpack");
        let from = h.game.clone();
        let src = library_of(&from).unwrap();
        let stage = |n: &str, f: &str| {
            let d = h._dir.join(&format!("fullp-{n}"));
            fs::create_dir_all(d.join("Data")).unwrap();
            fs::write(d.join("Data").join(f), n).unwrap();
            d
        };
        src.add_with(&stage("hand", "hand.esp"), "Hand Mod", "hand.zip", None, "1.0").unwrap();
        let nx = nexus::NexusRef { game: "coolgame".into(), mod_id: 101, file_id: 5 };
        src.add_with(&stage("nx", "nx.esp"), "Nexus Mod", "nx.zip", Some(nx), "2.0").unwrap();

        let pack = h._dir.join("standalone.zip");
        let msg = export_pack_file(&h.state, &from, "Full Pack", &pack, true).unwrap();
        assert!(msg.contains("2 with their files"), "{msg}");

        let listing = String::from_utf8_lossy(&Command::new(tar_tool()).arg("-tf").arg(&pack).output().unwrap().stdout).to_string();
        assert!(listing.contains("pack.json") && listing.contains("hand.esp") && listing.contains("nx.esp"), "{listing}");
        assert!(is_setup_file("standalone.zip", &pack));

        let plain_zip = h._dir.join("regular_mod.zip");
        let plain_stage = h._dir.join("plain-stage");
        fs::create_dir_all(&plain_stage).unwrap();
        fs::write(plain_stage.join("readme.txt"), "hello").unwrap();
        Command::new(tar_tool()).args(["--format", "zip", "-cf"]).arg(&plain_zip).arg("-C").arg(&plain_stage).arg(".").output().unwrap();
        assert!(!is_setup_file("regular_mod.zip", &plain_zip));
    }

    #[test]
    fn a_thunderstore_profile_code_becomes_a_setup_with_the_exact_versions_and_the_mods_are_fetched() {
        if fsutil::find_in_path("bsdtar").is_none() {
            eprintln!("skipping: bsdtar missing");
            return;
        }
        let h = harness("profcode");
        let dir = add_plain_game(&h);
        let g = game_by_appid(&h.state, "plain1").unwrap();
        let f = thunderstore::fake::start();
        {
            let mut st = h.state.write().unwrap();
            st.thunder.base = f.base.clone();
            st.thunder.allow_http = true;
        }
        f.packages.lock().unwrap().insert("Own-Cool_Mod".into(), vec![("1.0.0".into(), vec![]), ("1.2.3".into(), vec![])]);
        f.archives.lock().unwrap().insert("Own-Cool_Mod".into(), zip_of(&h, "cool-pc", &[("manifest.json", "{}"), ("CoolMod.dll", "v123")]));
        // the profile file as Thunderstore keeps it: #r2modman + base64 of a zip with export.r2x
        let r2x = "profileName: Friends run\nmods:\n  - name: Own-Cool_Mod\n    version:\n      major: 1\n      minor: 2\n      patch: 3\n    enabled: false\n  - name: Gone-Missing\n    version:\n      major: 1\n      minor: 0\n      patch: 0\n    enabled: true\n";
        let zip = zip_of(&h, "profile-pc", &[("export.r2x", r2x), ("config/x.cfg", "a=1")]);
        let code = "0f3e1c2a-1111-4222-8333-444455556666";
        f.pages.lock().unwrap().insert(format!("/api/experimental/legacyprofile/get/{code}/"), profilecode::wrap_profile(&zip));

        let spec = profile_from_code(&h.state, &format!("https://thunderstore.io/c/x/p/y/{code}/"), &g.name).unwrap();
        assert_eq!((spec.name.as_str(), spec.mods.len()), ("Friends run", 2));
        assert_eq!(spec.mods[0].thunderstore.as_ref().map(|t| t.version.as_str()), Some("1.2.3"));
        let msg = import_spec(&h.state, &g, spec).unwrap();
        assert!(msg.contains("1 fetched"), "{msg}");
        let lib = library_of(&g).unwrap();
        let mods = lib.mods();
        assert_eq!(mods.len(), 1, "the missing package is reported, not invented");
        assert_eq!((mods[0].version.as_str(), mods[0].enabled), ("1.2.3", false), "the exact version, and the profile's on/off state");
        assert!(dir.exists());
        // mistakes are explained
        assert!(profile_from_code(&h.state, "hello", &g.name).unwrap_err().contains("profile code"));
        assert!(profile_from_code(&h.state, "0f3e1c2a-1111-4222-8333-000000000000", &g.name).unwrap_err().contains("does not know"));
        f.pages.lock().unwrap().insert("/api/experimental/legacyprofile/get/0f3e1c2a-1111-4222-8333-111111111111/".into(), "<html>no</html>".into());
        assert!(profile_from_code(&h.state, "0f3e1c2a-1111-4222-8333-111111111111", &g.name).unwrap_err().contains("not a profile"));
    }

    #[test]
    fn a_starter_pack_comes_from_the_games_list_or_from_what_people_liked() {
        if fsutil::find_in_path("bsdtar").is_none() {
            eprintln!("skipping: bsdtar missing");
            return;
        }
        let _gate = modinfo::forget_all();
        let (h, f, _dir, _id) = nexus_update_setup("starter", true);
        let appid = selected_game(&h.state).unwrap().appid;
        {
            let mut st = h.state.write().unwrap();
            st.config.nexus_domain.insert(appid.clone(), "coolgame".into());
            st.recipes = vec![recipes::Recipe { appid: appid.clone(), starter: vec![recipes::StarterPick { source: "nexus".into(), id: "5".into(), why: "Fixes the menus.".into() }], ..Default::default() }];
        }
        *f.mod_json.lock().unwrap() = r#"{"name":"Menu Fix","summary":"Fixes <b>menus</b>","version":"1.0","author":"a","endorsement_count":40,"mod_downloads":900,"updated_timestamp":1700000000,"available":true,"picture_url":"https://staticdelivery.nexusmods.com/x.png"}"#.into();
        *f.search_json.lock().unwrap() = r#"[{"modId":"7","name":"Liked","summary":"","version":"1","author":"n","endorsements":300,"downloads":2000,"adultContent":false},{"modId":"8","name":"Only Downloaded","summary":"","version":"1","author":"n","endorsements":2,"downloads":9000,"adultContent":false},{"modId":"9","name":"Tiny","summary":"","version":"1","author":"n","endorsements":5,"downloads":50,"adultContent":false}]"#.into();
        let get = || -> Value {
            let (code, text) = http(&h, "GET", "/api/store?sort=starter", &[("X-Rig-Token", &h.token)], b"");
            assert_eq!(code, 200);
            serde_json::from_str(&text).unwrap()
        };
        let v = get();
        assert_eq!(v["curated"], true);
        assert_eq!((v["cards"][0]["name"].as_str(), v["cards"][0]["why"].as_str(), v["cards"][0]["downloads"].as_u64()), (Some("Menu Fix"), Some("Fixes the menus."), Some(900)), "{v}");
        // without a list: the downloaded and liked ones; a mod nobody endorsed is left out, and so is a tiny one
        h.state.write().unwrap().recipes.clear();
        let v = get();
        assert_eq!(v["curated"], false);
        let names: Vec<&str> = v["cards"].as_array().unwrap().iter().filter_map(|c| c["name"].as_str()).collect();
        assert_eq!(names, vec!["Liked"], "{v}");
    }

    #[test]
    fn a_game_with_anti_cheat_gets_a_reason_instead_of_a_store() {
        let h = harness("storeac");
        let dir = add_plain_game(&h);
        fs::create_dir_all(dir.join("EasyAntiCheat")).unwrap();
        let g = game_by_appid(&h.state, "plain1").unwrap();
        for sort in ["popular", "starter"] {
            let v = store_page(&h.state, &g, &StoreAsk { source: "both".into(), sort: sort.into(), query: String::new(), category: String::new(), off_nexus: 0, off_thunder: 0, off_modio: 0, per: 10, hide_libs: true });
            assert!(v["cards"].as_array().unwrap().is_empty(), "{v}");
            assert_eq!(v["notes"][0]["source"], "game");
            assert!(v["notes"][0]["error"].as_str().unwrap().contains("Easy Anti-Cheat"), "{v}");
        }
    }

    #[test]
    fn a_mod_that_is_not_in_the_library_has_a_page_before_it_is_installed() {
        if fsutil::find_in_path("bsdtar").is_none() {
            eprintln!("skipping: bsdtar missing");
            return;
        }
        let _gate = modinfo::forget_all();
        let (h, f, _dir, _id) = nexus_update_setup("storepage", true);
        let appid = selected_game(&h.state).unwrap().appid;
        h.state.write().unwrap().config.nexus_domain.insert(appid, "coolgame".into());
        *f.mod_json.lock().unwrap() = r#"{"name":"Fresh Pack","summary":"New here","description":"[b]Hello[/b]","version":"3.0","author":"Someone","endorsement_count":12,"mod_downloads":3400,"created_timestamp":100,"updated_timestamp":300,"available":true,"contains_adult_content":false}"#.into();
        *f.requirements_json.lock().unwrap() = "[]".into();
        let get = |q: &str| -> Value {
            let (code, text) = http(&h, "GET", &format!("/api/store_page?{q}"), &[("X-Rig-Token", &h.token)], b"");
            assert_eq!(code, 200);
            serde_json::from_str(&text).unwrap()
        };
        let v = get("source=nexus&id=99");
        assert_eq!((v["name"].as_str(), v["author"].as_str(), v["store"]["already"].as_bool()), (Some("Fresh Pack"), Some("Someone"), Some(false)), "{v}");
        assert!(v["description_html"].as_str().unwrap().contains("<strong>Hello</strong>"));
        assert!(get("source=nope&id=1")["error"].is_string());
        assert!(get("source=nexus&id=abc")["error"].is_string());
    }

    #[test]
    fn known_problems_in_the_logs_show_up_as_issues_with_a_cause_and_a_fix() {
        let h = harness("kbissue");
        let dir = add_plain_game(&h);
        let g = selected_game(&h.state).unwrap();
        let kinds = |h: &Harness, g: &GameEntry| -> Vec<(String, String)> {
            game_issues(&h.state, g, false).0.iter().filter(|i| i["code"] == "known").map(|i| (i["kb"]["id"].as_str().unwrap().to_string(), i["level"].as_str().unwrap().to_string())).collect()
        };
        assert!(kinds(&h, &g).is_empty(), "nothing in the logs, nothing to report");
        fs::create_dir_all(dir.join("BepInEx")).unwrap();
        fs::write(dir.join("BepInEx/LogOutput.log"), "[Info   :   BepInEx] Loading [Cool 1.0]\n[Error  :   BepInEx] Error loading [Cool 1.0] : BadImageFormatException: format is invalid\n").unwrap();
        assert_eq!(kinds(&h, &g), vec![("bad_image".to_string(), "bad".to_string()), ("bepinex_plugin_failed".to_string(), "warn".to_string())]);
        let i = game_issues(&h.state, &g, false).0.into_iter().find(|i| i["code"] == "known").unwrap();
        assert!(i["kb"]["title"]["en"].as_str().unwrap().len() > 10 && i["kb"]["fix"]["ro"].as_str().unwrap().len() > 10 && i["kb"]["evidence"].as_str().unwrap().contains("BadImageFormat"));
        // the user's own list wins over the built-in one
        let own = h.state.read().unwrap().paths.config_file.with_file_name("errors.json");
        fs::create_dir_all(own.parent().unwrap()).unwrap();
        fs::write(&own, r#"[{"id":"bad_image","contains":["BadImageFormatException"],"logs":["bepinex"],"level":"info","title":"My text","cause":"c","fix":"f"}]"#).unwrap();
        let again = game_issues(&h.state, &g, false).0.into_iter().find(|i| i["kb"]["id"] == "bad_image").unwrap();
        assert_eq!((again["kb"]["title"].as_str(), again["level"].as_str()), (Some("My text"), Some("info")));
    }


    #[test]
    fn a_games_support_entry_can_be_made_saved_and_used() {
        let h = harness("pack");
        let dir = add_plain_game(&h);
        let g = selected_game(&h.state).unwrap();
        fs::create_dir_all(dir.join("VEHICLES")).unwrap();
        let (code, text) = http(&h, "GET", "/api/pack", &[("X-Rig-Token", &h.token)], b"");
        assert_eq!(code, 200);
        let d: Value = serde_json::from_str(&text).unwrap();
        assert_eq!((d["existing"].as_bool(), d["entry"]["appid"].as_str()), (Some(false), Some(g.appid.as_str())), "{d}");
        assert!(d["folders_seen"].as_array().unwrap().iter().any(|f| f == "VEHICLES"), "{d}");
        let mut entry = d["entry"].clone();
        entry["notes"] = json!("Close the launcher first.");
        entry["folders"] = json!([{ "ext": ["bndl"], "folder": "VEHICLES" }, { "ext": ["x"], "folder": "../escape" }]);
        api(&h, "/api/pack_save", json!({ "entry": entry }));
        let saved = fs::read_to_string(h.state.read().unwrap().paths.config_file.with_file_name("recipes.json")).unwrap();
        assert!(saved.contains("VEHICLES") && !saved.contains("escape"), "{saved}");
        let (_, text) = http(&h, "GET", "/api/pack", &[("X-Rig-Token", &h.token)], b"");
        let again: Value = serde_json::from_str(&text).unwrap();
        assert_eq!((again["existing"].as_bool(), again["entry"]["notes"].as_str()), (Some(true), Some("Close the launcher first.")));
        // the note shows with the game's health, and the rule places a plain mod
        let issues = game_issues(&h.state, &g, false).0;
        assert!(issues.iter().any(|i| i["code"] == "pack_note" && i["args"][0] == "Close the launcher first."), "{issues:?}");
        let lib = library_of(&g).unwrap();
        let m = lib.add_from_dir(&{ let s = h._dir.join("pack-stage"); fs::create_dir_all(&s).unwrap(); fs::write(s.join("CAR.BNDL"), "C").unwrap(); s }, "Car", "c.zip").unwrap();
        assert_eq!((m.target.as_str(), m.placed_by.as_str()), ("VEHICLES", "pack"));
        // an entry that is not valid changes nothing
        api(&h, "/api/pack_save", json!({ "entry": { "appid": "../x" } }));
        assert!(h.state.read().unwrap().logs.iter().any(|l| l.level == "ERR" && l.message.contains("not a valid game entry")));
        recipes::publish(&[]);
    }


    #[test]
    fn a_download_must_match_the_checksum_nexus_published_and_programs_inside_a_mod_are_noted() {
        if fsutil::find_in_path("bsdtar").is_none() {
            eprintln!("skipping: bsdtar missing");
            return;
        }
        let _gate = modinfo::forget_all();
        let (h, f, _dir, id) = nexus_update_setup("checksum", true);
        let g = selected_game(&h.state).unwrap();
        let lib = library_of(&g).unwrap();
        let version = || lib.mods().into_iter().find(|m| m.id == id || m.name == "Cool Pack").unwrap().version;
        let link = nexus::NxmLink { game: "coolgame".into(), mod_id: 5, file_id: 12, key: None, expires: None };
        // a download that is not the published file is thrown away and nothing changes
        f.bad_md5.store(true, Ordering::SeqCst);
        nexus_fetch_and_install(&h.state, &g, &link, Some(id.clone()));
        assert_eq!(version(), "1.0");
        assert!(h.state.read().unwrap().logs.iter().any(|l| l.level == "ERR" && l.message.contains("does not match the checksum")));
        // the right one installs and is marked as checked
        f.bad_md5.store(false, Ordering::SeqCst);
        nexus_fetch_and_install(&h.state, &g, &link, Some(id.clone()));
        wait_until("updated", || version() == "2.0");
        let m = lib.mods().into_iter().find(|m| m.name == "Cool Pack").unwrap();
        assert_eq!(m.verified, "md5");
        let page = page_of(&h, &m.id);
        assert_eq!(page["trust"]["verified"], "md5");
        // program files inside a mod are listed
        let stage = h._dir.join("prog-stage");
        fs::create_dir_all(stage.join("bin")).unwrap();
        for (p, c) in [("tool.EXE", "x"), ("bin/run.bat", "y"), ("Data/ok.esp", "z"), ("lib.dll", "d")] {
            fs::create_dir_all(stage.join(p).parent().unwrap()).unwrap();
            fs::write(stage.join(p), c).unwrap();
        }
        let p = lib.add_from_dir(&stage, "Programs", "p.zip").unwrap();
        assert_eq!(p.programs.len(), 2, "{:?}", p.programs);
        assert!(p.programs.iter().any(|x| x.ends_with("tool.EXE")) && p.programs.iter().any(|x| x.ends_with("run.bat")));
        assert!(!deploy::is_program("lib.dll") && !deploy::is_program("readme.txt") && deploy::is_program("a/b.Ps1"));
    }


    #[cfg(not(windows))] // the saves of this test live in a Wine prefix; on Windows they are in the real AppData
    #[test]
    fn profiles_can_keep_their_own_saves_and_so_can_a_session_without_mods() {
        let h = harness("psaves");
        let g = h.game.clone();
        let live = PathBuf::from(&g.pfx).join("drive_c/users/steamuser/AppData/Local/Demo/Saved/SaveGames");
        fs::create_dir_all(&live).unwrap();
        let slot = || fs::read_to_string(live.join("slot.sav")).unwrap_or_default();
        fs::write(live.join("slot.sav"), "modded 1").unwrap();
        let lib = library_of(&g).unwrap();
        let stage = h._dir.join("ps-stage");
        fs::create_dir_all(stage.join("Data")).unwrap();
        fs::write(stage.join("Data/a.esp"), "A").unwrap();
        let m = lib.add_from_dir(&stage, "Mod", "m.zip").unwrap();
        api(&h, "/api/lib_profile_save", json!({ "name": "Modded" }));
        wait_until("saved", || lib.mods().len() == 1);
        lib.set_enabled(&m.id, false).unwrap();
        api(&h, "/api/lib_profile_save", json!({ "name": "Plain" }));
        thread::sleep(Duration::from_millis(300));

        // switched off: profiles share the saves, as before
        api(&h, "/api/lib_profile_load", json!({ "name": "Plain" }));
        wait_until("plain loaded", || !lib.mods()[0].enabled && !lib.is_dirty());
        assert_eq!(slot(), "modded 1");
        // switched on: the saves in the folders belong to Modded
        api(&h, "/api/lib_separate_saves", json!({ "enabled": true, "current": "Modded" }));
        wait_until("on", || lib.separate_saves() && lib.saves_of() == "Modded");
        wait_until("the state shows it", || {
            let st = api(&h, "/api/state", json!({}));
            (st["library"]["separate_saves"].as_bool(), st["library"]["save_folders"].as_u64()) == (Some(true), Some(1))
        });
        api(&h, "/api/lib_profile_load", json!({ "name": "Plain" }));
        wait_until("saves moved to plain", || lib.saves_of() == "Plain");
        assert_eq!(slot(), "modded 1", "Plain has no saves yet: it starts from the ones there are");
        let cache_root = read_state(&h.state).cache_root();
        assert!(saves::list(&g, &cache_root).iter().any(|s| s.label == "before-profile"), "the saves were backed up before the switch");
        fs::write(live.join("slot.sav"), "plain 1").unwrap();
        api(&h, "/api/lib_profile_load", json!({ "name": "Modded" }));
        wait_until("modded saves back", || lib.saves_of() == "Modded");
        assert_eq!(slot(), "modded 1");
        let replaced = lib.root().join("profile_saves/_replaced");
        let kept: Vec<_> = walkdir::WalkDir::new(&replaced).into_iter().flatten().filter(|e| e.file_name() == "slot.sav").collect();
        assert_eq!(fs::read_to_string(kept[0].path()).unwrap(), "plain 1", "what was replaced is kept");

        // a session without mods has saves of its own too, and the modded ones return afterwards
        begin_vanilla(&h.state, &g).unwrap();
        assert_eq!(lib.saves_of(), VANILLA_SAVES);
        assert_eq!(lib.saves_return(), "Modded");
        assert_eq!(slot(), "modded 1", "the first session without mods starts from what is there");
        fs::write(live.join("slot.sav"), "vanilla 1").unwrap();
        end_vanilla(&h.state, &g.appid);
        assert_eq!(lib.saves_of(), "Modded");
        assert_eq!(slot(), "modded 1");
        // and the next session without mods finds its own saves again
        begin_vanilla(&h.state, &g).unwrap();
        assert_eq!(slot(), "vanilla 1");
        end_vanilla(&h.state, &g.appid);
        assert_eq!(slot(), "modded 1");

        // while the game runs nothing is switched
        h.state.read().unwrap().is_running.store(true, Ordering::SeqCst);
        switch_saves(&h.state, &g, &lib, "Plain", false);
        h.state.read().unwrap().is_running.store(false, Ordering::SeqCst);
        assert_eq!((lib.saves_of().as_str(), slot().as_str()), ("Modded", "modded 1"));
        // off again: nothing moves
        api(&h, "/api/lib_separate_saves", json!({ "enabled": false }));
        wait_until("off", || !lib.separate_saves());
        api(&h, "/api/lib_profile_load", json!({ "name": "Plain" }));
        thread::sleep(Duration::from_millis(500));
        assert_eq!(slot(), "modded 1");
    }


    #[test]
    fn plugins_of_a_bethesda_style_game_are_listed_sorted_and_saved() {
        let h = harness("plugins");
        let dir = add_plain_game(&h);
        let g = selected_game(&h.state).unwrap();
        let plain = |h: &Harness| -> Value {
            let (code, text) = http(h, "GET", "/api/plugins", &[("X-Rig-Token", &h.token)], b"");
            assert_eq!(code, 200);
            serde_json::from_str(&text).unwrap()
        };
        assert_eq!(plain(&h)["supported"], false, "an ordinary game has no plugin list");
        fs::write(dir.join("SkyrimSE.exe"), b"MZ").unwrap();
        fs::remove_file(dir.join("Data/base.esp")).unwrap();
        for (n, flags, masters) in [("Skyrim.esm", 1u32, vec![]), ("Big.esm", 1, vec!["Skyrim.esm"]), ("A.esp", 0, vec!["B.esp"]), ("B.esp", 0, vec!["Big.esm", "Gone.esm"])] {
            fs::write(dir.join("Data").join(n), plugins::tests_support::plugin_bytes(flags, &masters, "me", "d")).unwrap();
        }
        let v = plain(&h);
        assert_eq!(v["supported"], true, "{v}");
        let names: Vec<&str> = v["order"].as_array().unwrap().iter().map(|r| r["name"].as_str().unwrap()).collect();
        assert_eq!(names, vec!["Skyrim.esm", "A.esp", "B.esp", "Big.esm"], "the game's own master first, the rest as found");
        assert_eq!(v["problems"], 2, "A loads before B, B before Big.esm and has a missing master: two plugins with a problem: {v}");
        let b = v["order"].as_array().unwrap().iter().find(|r| r["name"] == "B.esp").unwrap();
        assert_eq!(b["missing"], json!(["Gone.esm"]));
        // sorting suggests an order without writing anything
        let (_, text) = http(&h, "GET", "/api/plugins?sort=1", &[("X-Rig-Token", &h.token)], b"");
        let s: Value = serde_json::from_str(&text).unwrap();
        let sorted: Vec<&str> = s["order"].as_array().unwrap().iter().map(|r| r["name"].as_str().unwrap()).collect();
        assert_eq!(sorted, vec!["Skyrim.esm", "Big.esm", "B.esp", "A.esp"]);
        assert_eq!(s["problems"], 1, "only the missing master is left");
        let path = PathBuf::from(v["path"].as_str().unwrap());
        assert!(!path.exists(), "asking never writes");
        // saving writes the file (without the game's own masters) and keeps the first one
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, "*Old.esp\r\n").unwrap();
        api(&h, "/api/plugins_save", json!({ "order": [{ "name": "Skyrim.esm", "on": true }, { "name": "Big.esm", "on": true }, { "name": "B.esp", "on": false }, { "name": "A.esp", "on": true }, { "name": "../evil.esp", "on": true }, { "name": "Nothing.esp", "on": true }] }));
        wait_until("written", || fs::read_to_string(&path).map(|t| t.contains("Big.esm")).unwrap_or(false));
        let text = fs::read_to_string(&path).unwrap();
        assert!(text.contains("*Big.esm\r\nB.esp\r\n*A.esp\r\n") && !text.contains("Skyrim.esm") && !text.contains("evil") && !text.contains("Nothing"), "{text:?}");
        assert_eq!(fs::read_to_string(path.with_extension("txt.gantry-orig")).unwrap(), "*Old.esp\r\n");
        let again = plain(&h);
        assert_eq!(again["order"][1]["name"], "Big.esm");
        assert_eq!(again["order"][2]["on"], false);
        // not while the game runs
        h.state.read().unwrap().is_running.store(true, Ordering::SeqCst);
        api(&h, "/api/plugins_save", json!({ "order": [{ "name": "A.esp", "on": false }] }));
        h.state.read().unwrap().is_running.store(false, Ordering::SeqCst);
        assert!(fs::read_to_string(&path).unwrap().contains("*A.esp"));
        assert!(h.state.read().unwrap().logs.iter().any(|l| l.message.contains("Close the game first: it reads")));
        let _ = g;
    }

    #[test]
    fn a_mod_added_by_hand_is_tied_to_its_nexus_file_by_its_name_when_updates_are_checked() {
        if fsutil::find_in_path("bsdtar").is_none() {
            eprintln!("skipping: bsdtar missing");
            return;
        }
        let (h, f, _dir, _id) = nexus_update_setup("nxlink", false);
        let g = h.state.read().unwrap().games.last().unwrap().clone();
        h.state.write().unwrap().recipes = vec![recipes::Recipe { appid: g.appid.clone(), nexus: "coolgame".into(), ..Default::default() }];
        let lib = library_of(&g).unwrap();
        let stage = h._dir.join("hand-stage");
        fs::create_dir_all(stage.join("Data")).unwrap();
        fs::write(stage.join("Data/other.esp"), "O").unwrap();
        // what a browser saves from the current Nexus site: the name is the whole file name
        let source = "Other Mod 8 2.0 1970-01-01T00-05Z AbC123.zip";
        let hand = lib.add_from_dir(&stage, "Other Mod 8 2.0 1970-01-01T00-05Z AbC123", source).unwrap();
        assert!(hand.nexus.is_none());
        api(&h, "/api/nexus_check", json!({}));
        wait_until("check finished", || nexus_state(&h)["checked_ms"].as_u64().unwrap_or(0) > 0);
        let m = lib.mods().into_iter().find(|m| m.id == hand.id).unwrap();
        assert_eq!(m.nexus, Some(nexus::NexusRef { game: "coolgame".into(), mod_id: 8, file_id: 12 }));
        assert_eq!((m.name.as_str(), m.version.as_str()), ("Other Mod", "2.0"), "the clean name and the version of the file");
        assert!(h.state.read().unwrap().logs.iter().any(|l| l.message.contains("Linked 1 mod to its Nexus Mods page")));
        assert_eq!(nexus_state(&h)["updates"].as_array().unwrap().len(), 1, "only the older Cool Pack has an update; the linked one is current");
        // a mod that is not named like a Nexus download stays alone
        let manual = lib.mods().into_iter().find(|m| m.name == "Manual").unwrap();
        assert!(manual.nexus.is_none());
        let _ = f;
    }

    #[test]
    fn mod_io_mods_are_listed_and_installed_with_what_they_need() {
        if fsutil::find_in_path("bsdtar").is_none() {
            eprintln!("skipping: bsdtar missing");
            return;
        }
        let h = harness("modio");
        let dir = add_plain_game(&h);
        let zip = zip_of(&h, "mio", &[("Data/from_modio.esp", "M")]);
        let f = modio::fake::start(zip);
        {
            let mut st = h.state.write().unwrap();
            st.modio.base = f.base.clone();
            st.modio.allow_http = true;
            let i = st.selected_idx;
            st.games[i].name = "Cool Racer".into();
        }
        let get = |path: &str| -> Value {
            let (code, body) = http(&h, "GET", path, &[("X-Rig-Token", &h.token)], b"");
            assert_eq!(code, 200, "{body}");
            serde_json::from_str(&body).unwrap()
        };
        // no key yet: the list says so, and nothing was asked of mod.io
        let none = get("/api/store?source=modio&sort=popular");
        assert_eq!(none["notes"][0]["need_key"], true, "{none}");
        // a wrong key is refused, the right one is kept in a private file
        api(&h, "/api/modio_key_set", json!({ "key": "ffffffffffffffffffffffffffffffff" }));
        wait_until("wrong key refused", || h.state.read().unwrap().logs.iter().any(|l| l.message.contains("rejected")));
        assert!(!api(&h, "/api/state", json!({}))["modio"]["configured"].as_bool().unwrap());
        api(&h, "/api/modio_key_set", json!({ "key": modio::fake::KEY }));
        wait_until("key accepted", || api(&h, "/api/state", json!({}))["modio"]["configured"] == true);
        let keyfile = h._dir.join("cfg/modio_api_key");
        assert_eq!(fs::read_to_string(&keyfile).unwrap(), modio::fake::KEY);
        assert!(!api(&h, "/api/state", json!({})).to_string().contains(modio::fake::KEY), "the key is never in what the page gets");
        // the list: three mods (the adult one is left out), the most downloaded first is the fake's order
        let page = get("/api/store?source=modio&sort=popular");
        let ids: Vec<&str> = page["cards"].as_array().unwrap().iter().map(|c| c["id"].as_str().unwrap()).collect();
        assert_eq!(ids.len(), 3, "{page}");
        assert!(page["cards"][0]["source"] == "modio" && page["cards"][0]["page_url"].as_str().unwrap().starts_with("https://mod.io/"));
        // mod 3 needs 2, which needs 1: all three arrive, the needed ones first
        api(&h, "/api/modio_install", json!({ "id": "3" }));
        let lib = library_of(&h.state.read().unwrap().games.last().unwrap().clone()).unwrap();
        // a mod is added first and gets its mod.io record right after, so wait for both
        wait_until("three mods installed with their mod.io record", || {
            let mods = lib.mods();
            mods.len() == 3 && mods.iter().all(|m| m.modio.is_some())
        });
        let names: Vec<String> = lib.mods().iter().map(|m| m.name.clone()).collect();
        assert_eq!(names, vec!["Mod 1", "Mod 2", "Mod 3"], "what a mod needs is added before it");
        assert!(lib.mods().iter().all(|m| m.modio.as_ref().map(|r| r.game_id) == Some(7)));
        assert!(dir.join("Data/from_modio.esp").exists() || lib.mods().iter().all(|m| m.files.iter().any(|x| x.ends_with("from_modio.esp"))));
        // installing again changes nothing, and the list now shows them as installed
        api(&h, "/api/modio_install", json!({ "id": 3 }));
        wait_until("told it is there", || h.state.read().unwrap().logs.iter().any(|l| l.message.contains("already in the library")));
        assert_eq!(lib.mods().len(), 3);
        let after = get("/api/store?source=modio&sort=popular");
        assert!(after["cards"].as_array().unwrap().iter().all(|c| c["installed"] == true));
        // the key went only into the API calls, never into the download
        assert!(f.requests.lock().unwrap().iter().filter(|u| u.contains("/download/")).all(|u| !u.contains("KEY")));
        api(&h, "/api/modio_key_clear", json!({}));
        assert!(!keyfile.exists());
    }

    #[test]
    fn mod_io_mods_are_updated_have_pages_and_games_can_be_chosen() {
        if fsutil::find_in_path("bsdtar").is_none() {
            eprintln!("skipping: bsdtar missing");
            return;
        }
        let h = harness("modio2");
        add_plain_game(&h);
        let zip = zip_of(&h, "mio2", &[("Data/from_modio.esp", "M")]);
        let f = modio::fake::start(zip);
        {
            let mut st = h.state.write().unwrap();
            st.modio.base = f.base.clone();
            st.modio.allow_http = true;
            st.modio.key = Some(modio::fake::KEY.to_string());
            let i = st.selected_idx;
            st.games[i].name = "Cool Racers".into(); // not the name mod.io knows
        }
        let get = |path: &str| -> Value {
            let (code, body) = http(&h, "GET", path, &[("X-Rig-Token", &h.token)], b"");
            assert_eq!(code, 200, "{body}");
            serde_json::from_str(&body).unwrap()
        };
        // the name does not match: the person is offered the games that look like it, and the open one is chosen
        let none = get("/api/store?source=modio&sort=popular");
        let choose = none["notes"][0]["choose"].clone();
        assert_eq!(choose.as_array().map(|c| c.len()), Some(1), "only a game others may use is offered: {none}");
        assert_eq!(choose[0]["name"], "Cool Racer");
        api(&h, "/api/modio_game_set", json!({ "id": 8 }));
        wait_until("a closed game is refused", || h.state.read().unwrap().logs.iter().any(|l| l.message.contains("only allow their mod.io mods")));
        api(&h, "/api/modio_game_set", json!({ "id": 7 }));
        wait_until("chosen", || get("/api/store?source=modio&sort=popular")["cards"].as_array().map(|c| c.len()) == Some(3));
        // a mod page inside the app: text, what changed, what it needs
        let page = get("/api/store_page?source=modio&id=2");
        assert_eq!((page["name"].as_str(), page["source"].as_str(), page["store"]["already"].as_bool()), (Some("Mod 2"), Some("modio"), Some(false)), "{page}");
        assert!(page["description_html"].as_str().unwrap().contains("Long text about 2"));
        assert!(page["changes"][0]["html"].as_str().unwrap().contains("Fixed things"));
        assert_eq!((page["requirements"][0]["name"].as_str(), page["requirements"][0]["installed"].as_bool()), (Some("Mod 1"), Some(false)));
        // install, then the page of the library mod knows where it came from
        api(&h, "/api/modio_install", json!({ "id": 1 }));
        let g = selected_game(&h.state).unwrap();
        let lib = library_of(&g).unwrap();
        wait_until("installed", || lib.mods().len() == 1);
        let id = lib.mods()[0].id.clone();
        let own = get(&format!("/api/mod_page?id={id}"));
        assert_eq!((own["source"].as_str(), own["installed"]["name"].as_str()), (Some("modio"), Some("Mod 1")), "{own}");
        // no newer file yet; then the maker uploads one
        assert_eq!(check_modio_updates(&h.state), 0);
        f.bump.store(5, Ordering::SeqCst);
        assert_eq!(check_modio_updates(&h.state), 1);
        let u = api(&h, "/api/state", json!({}))["thunder_updates"][0].clone();
        assert_eq!((u["name"].as_str(), u["kind"].as_str(), u["new_version"].as_str()), (Some("Mod 1"), Some("modio"), Some("1.15")), "{u}");
        assert_eq!(get(&format!("/api/mod_page?id={id}"))["update"]["version"], "1.15");
        api(&h, "/api/thunder_update", json!({ "appid": g.appid, "id": id }));
        wait_until("replaced by the new file", || lib.mods().first().and_then(|m| m.modio.as_ref()).map(|r| r.file_id) == Some(15));
        assert_eq!(lib.mods().len(), 1, "an update replaces, it does not add");
        assert_eq!(lib.mods()[0].version, "1.15");
        wait_until("the list clears", || {
            prune_modio_updates(&h.state);
            h.state.read().unwrap().modio_updates.is_empty()
        });
        // a mod set to keep its version is not offered anything
        let current = lib.mods()[0].id.clone();
        lib.set_update_rule(&current, "never").unwrap();
        f.bump.store(9, Ordering::SeqCst);
        assert_eq!(check_modio_updates(&h.state), 0);
    }

    #[test]
    fn nexus_update_checks_find_only_real_replacements_and_leave_everything_else_alone() {
        if fsutil::find_in_path("bsdtar").is_none() {
            eprintln!("skipping: bsdtar missing");
            return;
        }
        let (h, f, dir, id) = nexus_update_setup("nxcheck", false);
        api(&h, "/api/nexus_check", json!({}));
        wait_until("check finished", || nexus_state(&h)["checked_ms"].as_u64().unwrap_or(0) > 0);
        let n = nexus_state(&h);
        assert_eq!(n["updates"].as_array().unwrap().len(), 1, "{n}");
        let u = &n["updates"][0];
        assert_eq!((u["name"].as_str(), u["installed_version"].as_str(), u["new_version"].as_str(), u["new_file_id"].as_u64()), (Some("Cool Pack"), Some("1.0"), Some("2.0"), Some(12)));
        assert_eq!(u["lib_id"], id.as_str());
        assert!(u["page_url"].as_str().unwrap().ends_with("/coolgame/mods/5?tab=files&file_id=12&nmm=1"));
        assert_eq!(f.seen.lock().unwrap().requests.iter().filter(|r| r.ends_with("/files.json")).count(), 1, "only the Nexus mod is looked up, once");
        assert_eq!(fs::read_to_string(dir.join("Data/new.esp")).unwrap(), "V1", "checking never changes any file");

        // nothing newer: the list empties
        *f.files_json.lock().unwrap() = r#"{"files":[],"file_updates":[]}"#.to_string();
        api(&h, "/api/nexus_check", json!({}));
        wait_until("empty again", || nexus_state(&h)["updates"].as_array().unwrap().is_empty() && !nexus_state(&h)["checking"].as_bool().unwrap());
        // a rejected key stops the check and keeps what was known
        *f.files_json.lock().unwrap() = MOD_FILES.to_string();
        api(&h, "/api/nexus_check", json!({}));
        wait_until("found again", || nexus_state(&h)["updates"].as_array().unwrap().len() == 1);
        h.state.write().unwrap().nexus.key = Some("ZZZZZZZZZZZZZZZZZZZZZZZZZZZZ".into());
        api(&h, "/api/nexus_check", json!({}));
        wait_until("rejection reported", || nexus_state(&h)["check_error"].as_str().map(|e| e.contains("rejected")).unwrap_or(false));
        assert_eq!(nexus_state(&h)["updates"].as_array().unwrap().len(), 1);
        api(&h, "/api/nexus_key_clear", json!({}));
        assert!(nexus_state(&h)["updates"].as_array().unwrap().is_empty(), "removing the key forgets the results");
    }


    #[test]
    fn mods_set_to_never_are_left_alone_auto_ones_update_themselves_and_updates_roll_back() {
        if fsutil::find_in_path("bsdtar").is_none() {
            eprintln!("skipping: bsdtar missing");
            return;
        }
        let _gate = modinfo::forget_all();
        let (h, _f, dir, id) = nexus_update_setup("rules", true);
        let updates = || nexus_state(&h)["updates"].as_array().unwrap().len();
        let check = || {
            api(&h, "/api/nexus_check", json!({}));
            wait_until("check finished", || !nexus_state(&h)["checking"].as_bool().unwrap() && nexus_state(&h)["checked_ms"].as_u64().unwrap_or(0) > 0);
        };
        // never: the update is not even listed
        api(&h, "/api/lib_update_rule", json!({ "id": id, "rule": "never" }));
        check();
        assert_eq!(updates(), 0);
        assert_eq!(page_of(&h, &id)["rule"], "never");
        assert_eq!(auto_update(&h.state), 0);
        // ask (the default): listed, but nothing is installed behind your back
        api(&h, "/api/lib_update_rule", json!({ "id": id, "rule": "" }));
        check();
        assert_eq!(updates(), 1);
        assert_eq!(auto_update(&h.state), 0);
        assert_eq!(fs::read_to_string(dir.join("Data/new.esp")).unwrap(), "V1");
        // auto: installed when asked to look (Premium), and the mod keeps its rule
        api(&h, "/api/lib_update_rule", json!({ "id": id, "rule": "auto" }));
        assert_eq!(auto_update(&h.state), 1);
        wait_until("updated", || fs::read_to_string(dir.join("Data/new.esp")).map(|t| t == "V2").unwrap_or(false));
        let lib = library_of(&selected_game(&h.state).unwrap()).unwrap();
        let cool = || lib.mods().into_iter().find(|m| m.name == "Cool Pack").unwrap();
        assert_eq!((cool().version.as_str(), cool().update_rule.as_str()), ("2.0", "auto"));
        // while a game runs nothing is touched
        h.state.read().unwrap().is_running.store(true, Ordering::SeqCst);
        assert_eq!(auto_update(&h.state), 0);
        h.state.read().unwrap().is_running.store(false, Ordering::SeqCst);

        // back to 1.0 from the mod page
        let page = page_of(&h, &cool().id);
        let prev = page["previous"].as_array().unwrap();
        assert_eq!((prev.len(), prev[0]["version"].as_str()), (1, Some("1.0")), "{page}");
        api(&h, "/api/lib_rollback", json!({ "id": cool().id, "dir": prev[0]["dir"] }));
        wait_until("rolled back", || cool().version == "1.0" && fs::read_to_string(dir.join("Data/new.esp")).map(|t| t == "V1").unwrap_or(false));
        assert_eq!(cool().update_rule, "auto");
    }

    #[test]
    fn a_premium_account_updates_a_mod_in_place_with_one_click() {
        if fsutil::find_in_path("bsdtar").is_none() {
            eprintln!("skipping: bsdtar missing");
            return;
        }
        let (h, _f, dir, id) = nexus_update_setup("nxprem", true);
        api(&h, "/api/nexus_check", json!({}));
        wait_until("update listed", || nexus_state(&h)["updates"].as_array().unwrap().len() == 1);
        let appid = nexus_state(&h)["updates"][0]["appid"].as_str().unwrap().to_string();
        api(&h, "/api/nexus_update", json!({ "appid": appid, "id": id }));
        wait_until("updated", || fs::read_to_string(dir.join("Data/new.esp")).map(|t| t == "V2").unwrap_or(false));
        wait_until("the library records the update", || api(&h, "/api/state", json!({}))["library"]["mods"][0]["version"] == "2.0");
        let lib = api(&h, "/api/state", json!({}))["library"].clone();
        let mods = lib["mods"].as_array().unwrap();
        assert_eq!(mods.len(), 2, "replaced, not added");
        assert_eq!((mods[0]["name"].as_str(), mods[0]["version"].as_str()), (Some("Cool Pack"), Some("2.0")), "same slot");
        assert_eq!(mods[0]["nexus"]["file_id"], 12);
        assert!(dir.join("Data/extra.esp").exists() && dir.join("Data/manual.esp").exists());
        wait_until("no update left", || nexus_state(&h)["updates"].as_array().unwrap().is_empty());
        assert!(last_log(&h).contains("Updated 'Cool Pack' to v2.0") || h.state.read().unwrap().logs.iter().any(|l| l.message.contains("Updated 'Cool Pack' to v2.0")));
    }

    #[test]
    fn a_free_account_updates_through_the_website_link_and_only_for_the_file_it_asked_for() {
        if fsutil::find_in_path("bsdtar").is_none() {
            eprintln!("skipping: bsdtar missing");
            return;
        }
        let (h, _f, dir, id) = nexus_update_setup("nxfree", false);
        api(&h, "/api/nexus_check", json!({}));
        wait_until("update listed", || nexus_state(&h)["updates"].as_array().unwrap().len() == 1);
        let appid = nexus_state(&h)["updates"][0]["appid"].as_str().unwrap().to_string();
        api(&h, "/api/nexus_update", json!({ "appid": appid, "id": id }));
        wait_until("waiting for the website", || nexus_state(&h)["waiting_for"]["name"] == "Cool Pack");
        assert_eq!(fs::read_to_string(dir.join("Data/new.esp")).unwrap(), "V1", "nothing happens before the link arrives");

        // some other file arrives first: an ordinary install, the wait continues
        api(&h, "/api/nxm", json!({ "url": "nxm://coolgame/mods/9/files/99?key=K1&expires=1999999999" }));
        let n = nexus_state(&h);
        assert_eq!((n["pending"]["update"].as_bool(), n["pending"]["mod_id"].as_u64()), (Some(false), Some(9)));
        assert_eq!(n["waiting_for"]["name"], "Cool Pack");
        api(&h, "/api/nxm_confirm", json!({ "accept": false }));

        api(&h, "/api/nxm", json!({ "url": "nxm://coolgame/mods/5/files/12?key=K2&expires=1999999999" }));
        let n = nexus_state(&h);
        assert_eq!((n["pending"]["update"].as_bool(), n["pending"]["target"].as_str()), (Some(true), Some("Cool Pack")));
        assert!(n["waiting_for"].is_null());
        api(&h, "/api/nxm_confirm", json!({ "accept": true }));
        wait_until("updated", || fs::read_to_string(dir.join("Data/new.esp")).map(|t| t == "V2").unwrap_or(false));
        wait_until("library records the new version", || api(&h, "/api/state", json!({}))["library"]["mods"][0]["version"] == "2.0");
        let mods = api(&h, "/api/state", json!({}))["library"]["mods"].clone();
        assert_eq!(mods.as_array().unwrap().len(), 2);
    }

    #[test]
    fn a_free_account_that_asked_for_a_mod_gets_it_installed_without_a_second_question() {
        if fsutil::find_in_path("bsdtar").is_none() {
            eprintln!("skipping: bsdtar missing");
            return;
        }
        let (h, _f, _dir, _id) = nexus_update_setup("nxget", false);
        let g = h.state.read().unwrap().games.last().unwrap().clone();
        h.state.write().unwrap().config.nexus_domain.insert(g.appid.clone(), "coolgame".into());
        let lib = library_of(&g).unwrap();
        let before = lib.mods().len();
        api(&h, "/api/nexus_get", json!({ "mod_id": 6, "name": "Needed Mod" }));
        wait_until("the page was opened and the link is awaited", || nexus_state(&h)["waiting_for"]["name"] == "Needed Mod");
        // a link nobody asked for still waits for the person's OK
        api(&h, "/api/nxm", json!({ "url": "nxm://coolgame/mods/9/files/99?key=K1&expires=1999999999" }));
        assert_eq!(nexus_state(&h)["pending"]["mod_id"], 9);
        api(&h, "/api/nxm_confirm", json!({ "accept": false }));
        assert_eq!(nexus_state(&h)["waiting_for"]["name"], "Needed Mod");
        // the link of the page Gantry opened installs by itself, as a new mod
        api(&h, "/api/nxm", json!({ "url": "nxm://coolgame/mods/6/files/12?key=K2&expires=1999999999" }));
        wait_until("installed", || lib.mods().len() == before + 1);
        assert!(nexus_state(&h)["pending"].is_null() && nexus_state(&h)["waiting_for"].is_null());
        assert_eq!(lib.mods().len(), before + 1, "added next to the others, nothing replaced");
    }

    #[test]
    fn what_a_mod_needs_is_fetched_one_page_at_a_time_on_a_free_account() {
        if fsutil::find_in_path("bsdtar").is_none() {
            eprintln!("skipping: bsdtar missing");
            return;
        }
        let (h, _f, _dir, id) = nexus_update_setup("nxneeds", false);
        let g = h.state.read().unwrap().games.last().unwrap().clone();
        let lib = library_of(&g).unwrap();
        let before = lib.mods().len();
        h.state.write().unwrap().nexus.needs = vec![NeedRow {
            appid: g.appid.clone(),
            lib_id: id.clone(),
            name: "Cool Pack".into(),
            missing: vec![
                MissingReq { name: "Frame One".into(), page_url: "https://www.nexusmods.com/coolgame/mods/6".into(), notes: String::new() },
                MissingReq { name: "Frame Two".into(), page_url: "https://www.nexusmods.com/coolgame/mods/7".into(), notes: String::new() },
            ],
        }];
        api(&h, "/api/nexus_install_needs", json!({ "appid": g.appid, "lib_id": id }));
        wait_until("the first page is awaited", || nexus_state(&h)["waiting_for"]["name"] == "Frame One");
        assert_eq!(nexus_state(&h)["queued"], 1, "the second one waits its turn");
        api(&h, "/api/nxm", json!({ "url": "nxm://coolgame/mods/6/files/12?key=K1&expires=1999999999" }));
        wait_until("the next page is awaited once the first arrived", || nexus_state(&h)["waiting_for"]["name"] == "Frame Two");
        assert_eq!(lib.mods().len(), before + 1);
        assert_eq!(nexus_state(&h)["queued"], 0);
        api(&h, "/api/nxm", json!({ "url": "nxm://coolgame/mods/7/files/12?key=K2&expires=1999999999" }));
        wait_until("both installed", || lib.mods().len() == before + 2);
        assert!(nexus_state(&h)["waiting_for"].is_null());
        assert!(nexus_state(&h)["queue"].is_null(), "the count ends with the queue");
    }

    #[test]
    fn the_free_queue_counts_skips_stops_and_takes_manual_downloads_too() {
        if fsutil::find_in_path("bsdtar").is_none() {
            eprintln!("skipping: bsdtar missing");
            return;
        }
        let (h, _f, _dir, id) = nexus_update_setup("nxqueue", false);
        let g = h.state.read().unwrap().games.last().unwrap().clone();
        let lib = library_of(&g).unwrap();
        let before = lib.mods().len();
        let downloads = h._dir.join("Downloads");
        fs::create_dir_all(&downloads).unwrap();
        h.state.write().unwrap().downloads_dir = Some(downloads.clone());
        let req = |n: &str, m: u64| MissingReq { name: n.into(), page_url: format!("https://www.nexusmods.com/coolgame/mods/{m}"), notes: String::new() };
        h.state.write().unwrap().nexus.needs = vec![NeedRow { appid: g.appid.clone(), lib_id: id.clone(), name: "Cool Pack".into(), missing: vec![req("Frame One", 6), req("Frame Two", 7), req("Frame Three", 8)] }];
        api(&h, "/api/nexus_install_needs", json!({ "appid": g.appid, "lib_id": id }));
        wait_until("first page", || nexus_state(&h)["waiting_for"]["name"] == "Frame One");
        let q = nexus_state(&h)["queue"].clone();
        assert_eq!((q["done"].as_u64(), q["total"].as_u64()), (Some(0), Some(3)), "{q}");
        assert_eq!(q["next"], json!(["Frame Two", "Frame Three"]));
        // the website's "Manual download" saved the file instead of handing the link over: it is picked up from Downloads
        let file = downloads.join("Frame One 6 1.0 2026-08-01T03-15Z q7jg7fflx.zip");
        fs::write(&file, zip_of(&h, "f1", &[("Data/frame1.esp", "F")])).unwrap();
        // a file that is still being written is left alone
        assert_eq!(match_downloads_to_wants(&h.state), 0);
        fs::File::options().write(true).open(&file).unwrap().set_modified(std::time::SystemTime::now() - Duration::from_secs(20)).unwrap();
        // and an old download of something else is never taken
        let other = downloads.join("Other Thing 99 1.0 2026-08-01T03-15Z zzzzzzzzz.zip");
        fs::write(&other, zip_of(&h, "o", &[("Data/o.esp", "O")])).unwrap();
        fs::File::options().write(true).open(&other).unwrap().set_modified(std::time::SystemTime::now() - Duration::from_secs(20)).unwrap();
        assert_eq!(match_downloads_to_wants(&h.state), 1);
        wait_until("installed from Downloads", || lib.mods().len() == before + 1);
        assert!(file.is_file(), "the file in Downloads is left where it is");
        wait_until("the next page is awaited", || nexus_state(&h)["waiting_for"]["name"] == "Frame Two");
        assert_eq!(nexus_state(&h)["queue"]["done"], 1);
        assert_eq!(match_downloads_to_wants(&h.state), 0, "the same file is not installed twice");
        // skip one, then stop the rest
        api(&h, "/api/nexus_queue", json!({ "action": "skip" }));
        wait_until("skipped to the third", || nexus_state(&h)["waiting_for"]["name"] == "Frame Three");
        assert_eq!(nexus_state(&h)["queue"]["done"], 2);
        api(&h, "/api/nexus_queue", json!({ "action": "stop" }));
        wait_until("stopped", || nexus_state(&h)["waiting_for"].is_null() && nexus_state(&h)["queue"].is_null());
        assert_eq!(lib.mods().len(), before + 1);
        assert!(other.is_file());
    }

    #[test]
    fn mods_installed_from_nexus_remember_where_they_came_from() {
        if fsutil::find_in_path("bsdtar").is_none() {
            eprintln!("skipping: bsdtar missing");
            return;
        }
        let h = harness("nxorigin");
        let zip = zip_of(&h, "orig", &[("Data/a.esp", "A"), ("readme.txt", "R")]);
        let f = nexus::fake::start("Fancy Mod-77-1-4-1700000000.zip", zip, false);
        {
            let mut st = h.state.write().unwrap();
            st.nexus.base = f.base.clone();
            st.nexus.allow_http = true;
        }
        add_plain_game(&h);
        connect_nexus(&h);
        api(&h, "/api/nxm", json!({ "url": "nxm://coolgame/mods/77/files/8" }));
        api(&h, "/api/nxm_confirm", json!({ "accept": true }));
        wait_until("installed", || api(&h, "/api/state", json!({}))["library"]["mods"].as_array().map(|m| m.len()) == Some(1));
        let m = api(&h, "/api/state", json!({}))["library"]["mods"][0].clone();
        assert_eq!((m["name"].as_str(), m["version"].as_str()), (Some("Fancy Mod"), Some("1.0")), "version comes from Nexus, not the file name");
        assert_eq!((m["nexus"]["game"].as_str(), m["nexus"]["mod_id"].as_u64(), m["nexus"]["file_id"].as_u64()), (Some("coolgame"), Some(77), Some(8)));
    }

    #[test]
    fn a_unity_game_gets_the_mod_loader_that_fits_it_through_the_library() {
        if fsutil::find_in_path("bsdtar").is_none() {
            eprintln!("skipping: bsdtar missing");
            return;
        }
        let h = harness("loader");
        let zip = zip_of(&h, "bep", &[("winhttp.dll", "W"), ("doorstop_config.ini", "I"), ("changelog.txt", "log"), ("BepInEx/core/BepInEx.dll", "B")]);
        let f = loaders::fake::start(zip);
        h.state.write().unwrap().loader_sources = f.src.clone();
        let dir = add_plain_game(&h);
        // an IL2CPP game: native code in GameAssembly.dll, 64-bit player
        let mut pe = vec![0u8; 128];
        pe[0] = b'M';
        pe[1] = b'Z';
        pe[60] = 64;
        pe[64..68].copy_from_slice(b"PE\0\0");
        pe[68..70].copy_from_slice(&0x8664u16.to_le_bytes());
        fs::write(dir.join("UnityPlayer.dll"), pe).unwrap();
        fs::write(dir.join("GameAssembly.dll"), "x").unwrap();
        fs::create_dir_all(dir.join("Plain_Data")).unwrap();
        fs::write(dir.join("Plain_Data/globalgamemanagers"), "x").unwrap();
        let unity = || api(&h, "/api/state", json!({}))["unity"].clone();
        assert_eq!((unity()["il2cpp"].as_bool(), unity()["x64"].as_bool(), unity()["loader"].as_str()), (Some(true), Some(true), Some("")));

        api(&h, "/api/loader_install", json!({ "kind": "bepinex6" }));
        wait_until("the loader is deployed", || dir.join("BepInEx/core/BepInEx.dll").is_file() && dir.join("winhttp.dll").is_file());
        wait_until("the state shows the loader as a library mod", || api(&h, "/api/state", json!({}))["library"]["mods"][0]["name"] == "BepInEx 6");
        assert_eq!(unity()["loader"], "bepinex");
        let m = api(&h, "/api/state", json!({}))["library"]["mods"][0].clone();
        assert_eq!((m["name"].as_str(), m["version"].as_str()), (Some("BepInEx 6"), Some("6.0.0-be.789")), "the newest build on the page");
        assert!(f.hits.lock().unwrap().iter().any(|u| u.contains("/789/")));
        // the library knows it, so Back to vanilla removes it again
        api(&h, "/api/lib_purge", json!({}));
        wait_until("removed again", || !dir.join("BepInEx").exists() && !dir.join("winhttp.dll").exists());
        assert_eq!(unity()["loader"], "");
    }

    #[test]
    fn everything_back_to_vanilla_restores_every_game_and_keeps_the_mods() {
        let h = harness("purgeall");
        let dir = add_plain_game(&h);
        let g = selected_game(&h.state).unwrap();
        let lib = library_of(&g).unwrap();
        let stage = h._dir.join("stage-purgeall");
        fs::create_dir_all(stage.join("Data")).unwrap();
        fs::write(stage.join("Data/base.esp"), "MODDED").unwrap();
        lib.add_from_dir(&stage, "Overhaul", "o.zip").unwrap();
        lib.deploy(false).unwrap();
        assert_eq!(fs::read_to_string(dir.join("Data/base.esp")).unwrap(), "MODDED");
        api(&h, "/api/purge_all", json!({}));
        wait_until("restored", || fs::read_to_string(dir.join("Data/base.esp")).map(|t| t == "ORIGINAL").unwrap_or(false));
        assert_eq!(lib.mods().len(), 1, "the mod stays in its library");
        // and the history can bring the earlier choice back
        lib.set_enabled("overhaul", false).unwrap();
        api(&h, "/api/lib_history", json!({ "index": 0 }));
        wait_until("history restored the mod's enabled state", || lib.mods()[0].enabled);
    }

    #[test]
    fn playing_without_mods_takes_them_out_and_brings_them_back_even_after_a_restart() {
        let h = harness("vanillaplay");
        let dir = add_plain_game(&h);
        let g = selected_game(&h.state).unwrap();
        let lib = library_of(&g).unwrap();
        let stage = h._dir.join("stage-vanilla");
        fs::create_dir_all(stage.join("Data")).unwrap();
        fs::write(stage.join("Data/base.esp"), "MODDED").unwrap();
        lib.add_from_dir(&stage, "Overhaul", "o.zip").unwrap();
        lib.deploy(false).unwrap();
        // the file is briefly missing while a backup is put back (Windows replaces it in two steps)
        let base = || fs::read_to_string(dir.join("Data/base.esp")).unwrap_or_default();
        assert_eq!(base(), "MODDED");

        api(&h, "/api/play_vanilla", json!({}));
        wait_until("mods are out for the session", || base() == "ORIGINAL");
        assert_eq!(api(&h, "/api/state", json!({}))["vanilla"], json!([g.appid.clone()]));
        // the game never shows up, so the session ends by itself and the mods return
        wait_until("mods are back", || base() == "MODDED" && api(&h, "/api/state", json!({}))["vanilla"] == json!([]));
        wait_until("the library is in order again", || lib.drift().is_empty() && !lib.is_dirty());

        // Gantry was closed in the middle of a session: the next start puts everything back
        begin_vanilla(&h.state, &g).unwrap();
        assert_eq!(base(), "ORIGINAL");
        resume_vanilla_sessions(&h.state);
        wait_until("resumed", || base() == "MODDED");
        assert!(h.state.read().unwrap().config.vanilla_sessions.is_empty());

        // a game without a library has nothing to take out and still ends cleanly
        end_vanilla(&h.state, "nobody");
    }

    #[test]
    fn playing_in_safe_mode_tracks_session_and_restores_on_restart() {
        let h = harness("safemodetest");
        let _dir = add_plain_game(&h);
        let g = selected_game(&h.state).unwrap();
        let lib = library_of(&g).unwrap();
        let stage_loader = h._dir.join("stage-loader");
        fs::create_dir_all(&stage_loader).unwrap();
        fs::write(stage_loader.join("winhttp.dll"), "DLL").unwrap();
        lib.add_from_dir(&stage_loader, "UE4SS Loader", "loader.zip").unwrap();

        let stage_content = h._dir.join("stage-content");
        fs::create_dir_all(stage_content.join("Data")).unwrap();
        fs::write(stage_content.join("Data/content.esp"), "CONTENT").unwrap();
        lib.add_from_dir(&stage_content, "Big Swords", "content.zip").unwrap();
        lib.deploy(false).unwrap();

        api(&h, "/api/play_safe_mode", json!({}));
        wait_until("safe mode is active in state", || {
            let st = api(&h, "/api/state", json!({}));
            st["safe_mode"] == json!([g.appid.clone()])
        });

        wait_until("safe mode ended and content restored", || {
            let st = api(&h, "/api/state", json!({}));
            st["safe_mode"] == json!([]) && lib.mods().iter().all(|m| m.enabled)
        });

        // Crash resilience test: session left in config resumes cleanly
        {
            let mut st = h.state.write().unwrap();
            st.config.safe_mode_sessions.push(g.appid.clone());
        }
        resume_safe_mode_sessions(&h.state);
        wait_until("resumed safe mode session", || {
            h.state.read().unwrap().config.safe_mode_sessions.is_empty()
        });
    }

    #[test]
    fn a_saved_profile_can_be_played_in_one_step() {
        let h = harness("playprofile");
        let dir = add_plain_game(&h);
        let g = selected_game(&h.state).unwrap();
        let lib = library_of(&g).unwrap();
        for (tag, name) in [("a", "Alpha"), ("b", "Beta")] {
            let stage = h._dir.join(&format!("stage-pp-{tag}"));
            fs::create_dir_all(stage.join("Data")).unwrap();
            fs::write(stage.join("Data").join(format!("{tag}.esp")), tag).unwrap();
            lib.add_from_dir(&stage, name, "x.zip").unwrap();
        }
        lib.set_enabled("beta", false).unwrap();
        lib.save_profile("Only Alpha").unwrap();
        lib.set_enabled("beta", true).unwrap();
        lib.set_enabled("alpha", false).unwrap();
        lib.deploy(false).unwrap();
        assert!(!dir.join("Data/a.esp").exists() && dir.join("Data/b.esp").exists());
        api(&h, "/api/play_profile", json!({ "name": "Only Alpha" }));
        wait_until("the profile is deployed", || dir.join("Data/a.esp").exists() && !dir.join("Data/b.esp").exists());
    }

    #[test]
    fn the_game_doctor_lists_what_is_wrong_worst_first_and_the_report_hides_secrets() {
        let h = harness("doctor2");
        let dir = add_plain_game(&h);
        let g = selected_game(&h.state).unwrap();
        let lib = library_of(&g).unwrap();
        let (none, _) = game_issues(&h.state, &g, false);
        assert!(none.is_empty(), "a game without mods has nothing to report: {none:?}");
        let stage = h._dir.join("stage-doctor");
        fs::create_dir_all(stage.join("Data")).unwrap();
        fs::write(stage.join("Data/base.esp"), "MODDED").unwrap();
        let m = lib.add_from_dir(&stage, "Overhaul", "o.zip").unwrap();
        let codes = |h: &Harness| game_issues(&h.state, &selected_game(&h.state).unwrap(), false).0.iter().map(|i| i["code"].as_str().unwrap().to_string()).collect::<Vec<_>>();
        assert_eq!(codes(&h), vec!["dirty"], "added but not deployed");
        lib.deploy(false).unwrap();
        assert!(codes(&h).is_empty());
        // a game update replaces a deployed file, and a required mod is missing
        fs::remove_file(dir.join("Data/base.esp")).unwrap();
        fs::write(dir.join("Data/base.esp"), "UPDATED").unwrap();
        h.state.write().unwrap().nexus.needs.push(NeedRow { appid: g.appid.clone(), lib_id: m.id.clone(), name: "Overhaul".into(), missing: vec![MissingReq { name: "Base Lib".into(), page_url: String::new(), notes: String::new() }] });
        assert_eq!(codes(&h), vec!["drift", "needs"]);
        // a game that bans for modified files, with mods deployed, comes first
        lib.deploy(false).unwrap();
        fs::create_dir_all(dir.join("EasyAntiCheat")).unwrap();
        fs::write(dir.join("EasyAntiCheat/x.dll"), "x").unwrap();
        let issues = game_issues(&h.state, &g, false).0;
        assert_eq!((issues[0]["code"].as_str(), issues[0]["level"].as_str(), issues[0]["fix"].as_str()), (Some("anti_cheat"), Some("bad"), Some("vanilla")));

        h.state.write().unwrap().nexus.key = Some("SECRETKEYSECRETKEYSECRETKEY1234567890".into());
        let report = problem_report(&h.state, &g);
        assert!(report.contains("Plain Game") && report.contains("Overhaul") && report.contains("anti_cheat"), "{report}");
        assert!(!report.contains("SECRETKEY"), "keys never leave the app");
        assert!(!report.contains(&home_dir().to_string_lossy().to_string()) || home_dir().to_string_lossy() == "/", "the home folder is shortened");
        let (code, body) = http(&h, "GET", "/api/health", &[("X-Rig-Token", &h.token)], b"");
        let v: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(code, 200);
        assert_eq!(v["issues"][0]["code"], "anti_cheat");
    }

    #[test]
    fn the_compatibility_window_sets_proton_per_game_toggles_tweaks_and_fetches_ge_proton() {
        if fsutil::find_in_path("bsdtar").is_none() || fsutil::find_in_path("tar").is_none() || fsutil::find_in_path("sha512sum").is_none() {
            eprintln!("skipping: tools missing");
            return;
        }
        let h = harness("compat");
        add_plain_game(&h);
        {
            // Steam games have numeric ids
            let mut st = h.state.write().unwrap();
            let i = st.selected_idx;
            st.games[i].appid = "4242".into();
        }
        // two Protons on disk: one Steam knows by a name, one plain folder; a private copy of Steam's config
        let ct = h._dir.join("ct");
        let exp = ct.join("Exp");
        fs::create_dir_all(&exp).unwrap();
        fs::write(exp.join("proton"), "x").unwrap();
        fs::write(exp.join("compatibilitytool.vdf"), "\"compatibilitytools\"\n{\n \"compat_tools\"\n {\n  \"proton_experimental\"\n  {\n   \"display_name\" \"Proton Experimental\"\n  }\n }\n}\n").unwrap();
        let plain = ct.join("Plain-Proton");
        fs::create_dir_all(&plain).unwrap();
        fs::write(plain.join("proton"), "x").unwrap();
        let cfg = h._dir.join("config.vdf");
        fs::write(&cfg, "\"InstallConfigStore\"\n{\n\t\"Software\"\n\t{\n\t\t\"Valve\"\n\t\t{\n\t\t\t\"Steam\"\n\t\t\t{\n\t\t\t\t\"CompatToolMapping\"\n\t\t\t\t{\n\t\t\t\t\t\"0\"\n\t\t\t\t\t{\n\t\t\t\t\t\t\"name\"\t\t\"proton-cachyos-slr\"\n\t\t\t\t\t}\n\t\t\t\t}\n\t\t\t}\n\t\t}\n\t}\n}\n").unwrap();
        {
            let mut st = h.state.write().unwrap();
            st.tools_dirs = Some(vec![ct.clone()]);
            st.steam_env = SteamEnv { config_vdf: Some(cfg.clone()), localconfig: None, running: Some(true), libraries: Some(vec![]) };
        }
        let get = |path: &str| -> Value {
            let (code, body) = http(&h, "GET", path, &[("X-Rig-Token", &h.token)], b"");
            assert_eq!(code, 200);
            serde_json::from_str(&body).unwrap()
        };
        let info = get("/api/compat");
        assert_eq!((info["kind"].as_str(), info["current"].as_str(), info["tools"].as_array().unwrap().len()), (Some("steam"), Some("proton-cachyos-slr"), 2), "{info}");
        // Steam is open: the change is refused and the file untouched
        let before = fs::read_to_string(&cfg).unwrap();
        api(&h, "/api/compat_proton", json!({ "tool": "proton_experimental" }));
        wait_until("refused", || last_log(&h).contains("Steam is running"));
        assert_eq!(fs::read_to_string(&cfg).unwrap(), before);
        h.state.write().unwrap().steam_env.running = Some(false);
        api(&h, "/api/compat_proton", json!({ "tool": "proton_experimental" }));
        wait_until("written", || get("/api/compat")["current"] == "proton_experimental");
        assert!(fs::read_dir(h.state.read().unwrap().cache_root().join("steam-backups")).unwrap().count() >= 1, "a backup was made");
        api(&h, "/api/compat_proton", json!({ "tool": "nothing-like-this" }));
        wait_until("unknown tool refused", || last_log(&h).contains("no longer installed"));

        // a game Gantry starts itself: its own choice, by script
        {
            let mut st = h.state.write().unwrap();
            let i = st.selected_idx;
            st.games[i].is_custom = true;
        }
        let script = plain.join("proton").to_string_lossy().to_string();
        api(&h, "/api/compat_proton", json!({ "tool": script }));
        wait_until("choice stored", || h.state.read().unwrap().config.proton_choice.len() == 1);
        let g = selected_game(&h.state).unwrap();
        assert_eq!(h.state.read().unwrap().proton_for(&g), Some(plain.join("proton")));
        assert_eq!(get("/api/compat")["kind"], "direct");

        // tweaks
        api(&h, "/api/compat_preset", json!({ "id": "no_ntsync", "on": true }));
        assert_eq!(selected_game(&h.state).unwrap().env_vars, "PROTON_NO_NTSYNC=1");
        let p = get("/api/compat")["presets"].clone();
        assert!(p.as_array().unwrap().iter().any(|x| x["id"] == "no_ntsync" && x["on"] == true));
        api(&h, "/api/compat_preset", json!({ "id": "no_ntsync", "on": false }));
        assert_eq!(selected_game(&h.state).unwrap().env_vars, "");
        api(&h, "/api/compat_preset", json!({ "id": "not-a-preset", "on": true }));
        assert_eq!(selected_game(&h.state).unwrap().env_vars, "");

        // GE-Proton: downloaded, checked and listed
        let src = h._dir.join("gesrc");
        fs::create_dir_all(src.join("GE-Proton99-9")).unwrap();
        fs::write(src.join("GE-Proton99-9/proton"), "x").unwrap();
        let tar = h._dir.join("ge.tar.gz");
        assert!(Command::new("tar").arg("-czf").arg(&tar).arg("-C").arg(&src).arg("GE-Proton99-9").status().unwrap().success());
        let hash = String::from_utf8_lossy(&Command::new("sha512sum").arg(&tar).output().unwrap().stdout).split_whitespace().next().unwrap().to_string();
        let fake = compat::fake::start("GE-Proton99-9", fs::read(&tar).unwrap(), format!("{hash}  x\n"));
        h.state.write().unwrap().ge_source = fake.src.clone();
        api(&h, "/api/ge_download", json!({}));
        wait_until("GE-Proton installed", || ct.join("GE-Proton99-9/proton").is_file());
        assert_eq!(get("/api/compat")["tools"].as_array().unwrap().len(), 3);
    }

    #[test]
    fn covers_are_found_by_name_once_and_remembered() {
        let h = harness("covers");
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let base = format!("http://{}/search", server.server_addr().to_ip().unwrap());
        let hits = Arc::new(Mutex::new(Vec::<String>::new()));
        let h2 = Arc::clone(&hits);
        thread::spawn(move || {
            for req in server.incoming_requests() {
                h2.lock().unwrap().push(req.url().to_string());
                let body = if req.url().contains("Most%20Wanted") || req.url().contains("Most+Wanted") {
                    r#"{"total":2,"items":[{"type":"app","id":1,"name":"Most Wanted Mod Pack"},{"type":"app","id":17410,"name":"Need for Speed\u2122 Most Wanted"}]}"#
                } else {
                    r#"{"total":0,"items":[]}"#
                };
                let _ = req.respond(tiny_http::Response::from_string(body));
            }
        });
        {
            let mut st = h.state.write().unwrap();
            st.covers_base = base;
            st.covers_allow_http = true;
            let mut g = st.games[0].clone();
            g.appid = "hydra_nfs".into();
            g.name = "Need for Speed\u{2122} Most Wanted".into();
            g.source = "hydra".into();
            g.steam_appid.clear();
            g.cover_id.clear();
            st.games.push(g.clone());
            g.appid = "hydra_unknown".into();
            g.name = "Some Obscure Game".into();
            st.games.push(g);
        }
        assert_eq!(lookup_missing_covers(&h.state), 1);
        let st = api(&h, "/api/state", json!({}));
        let cover = |id: &str| st["games"].as_array().unwrap().iter().find(|g| g["appid"] == id).unwrap()["cover_id"].clone();
        assert_eq!((cover("hydra_nfs"), cover("hydra_unknown")), (json!("17410"), json!("")));
        let asked = hits.lock().unwrap().len();
        // answers (including "nothing found") are remembered: no second round of questions
        assert_eq!(lookup_missing_covers(&h.state), 0);
        assert_eq!(hits.lock().unwrap().len(), asked);
        assert_eq!(h.state.read().unwrap().config.cover_appid.get("hydra_unknown").map(String::as_str), Some("0"));
    }

    #[test]
    fn pins_notes_tags_and_time_played_are_kept_per_game() {
        let h = harness("meta");
        let id = api(&h, "/api/state", json!({}))["games"][0]["appid"].as_str().unwrap().to_string();
        api(&h, "/api/game_meta", json!({ "appid": id, "favorite": true, "notes": "needs -dx11", "tags": " Co-op, racing ,co-op,, " }));
        let m = api(&h, "/api/state", json!({}))["meta"][&id].clone();
        assert_eq!((m["fav"].as_bool(), m["notes"].as_str(), m["tags"].clone()), (Some(true), Some("needs -dx11"), json!(["Co-op", "racing"])), "{m}");
        api(&h, "/api/game_meta", json!({ "appid": id, "favorite": false }));
        assert_eq!(api(&h, "/api/state", json!({}))["meta"][&id]["fav"], false);
        assert_eq!(api(&h, "/api/state", json!({}))["meta"][&id]["notes"], "needs -dx11", "other fields stay when only one is sent");
        api(&h, "/api/game_meta", json!({ "appid": id, "notes": "  ", "tags": "" }));
        assert!(api(&h, "/api/state", json!({}))["meta"].get(&id).is_none(), "empty notes and tags are removed");
        // time played is added when a run ends
        let g = selected_game(&h.state).unwrap();
        finish_run(&h.state, Run { game: g.clone(), started_ms: now_ms().saturating_sub(125_000), perf: Default::default(), seen: Vec::new(), next_scan_ms: 0 });
        let secs = api(&h, "/api/state", json!({}))["meta"][&g.appid]["playtime_s"].as_u64().unwrap();
        assert!((120..=130).contains(&secs), "{secs}");
        // unknown games are ignored
        api(&h, "/api/game_meta", json!({ "appid": "nobody", "favorite": true }));
        assert!(!h.state.read().unwrap().config.favorites.contains(&"nobody".to_string()));
    }

    #[test]
    fn a_backup_of_gantrys_data_restores_settings_and_mod_lists_and_cleanup_stays_in_its_own_folders() {
        let h = harness("backup");
        let _dir = add_plain_game(&h);
        let g = selected_game(&h.state).unwrap();
        let lib = library_of(&g).unwrap();
        let stage = h._dir.join("stage-backup");
        fs::create_dir_all(stage.join("Data")).unwrap();
        fs::write(stage.join("Data/x.esp"), "X").unwrap();
        lib.add_from_dir(&stage, "Keeper", "k.zip").unwrap();
        api(&h, "/api/game_meta", json!({ "appid": g.appid, "favorite": true, "notes": "remember me", "tags": "one" }));
        h.state.write().unwrap().nexus.key = Some("SECRETKEYSECRETKEYSECRETKEY1234567890".into());
        let bundle = backup_bundle(&h.state);
        assert!(!bundle.to_string().contains("SECRETKEY"), "keys are never part of a backup");
        assert_eq!(bundle["libraries"][0]["library"]["mods"][0]["id"], "keeper");

        // things change, then the backup is put back
        api(&h, "/api/game_meta", json!({ "appid": g.appid, "favorite": false, "notes": "", "tags": "" }));
        lib.set_enabled("keeper", false).unwrap();
        let msg = restore_bundle(&h.state, &bundle).unwrap();
        assert!(msg.contains("1 mod(s) back"), "{msg}");
        let m = api(&h, "/api/state", json!({}))["meta"][&g.appid].clone();
        assert_eq!((m["fav"].as_bool(), m["notes"].as_str()), (Some(true), Some("remember me")));
        assert!(lib.mods()[0].enabled);
        assert!(fs::read_dir(h.state.read().unwrap().cache_root().join("backups")).unwrap().count() >= 1, "what was there before is kept");
        assert!(restore_bundle(&h.state, &json!({ "hello": 1 })).is_err());

        // cleanup: only Gantry's own caches, only from the allow-list
        let root = h.state.read().unwrap().cache_root();
        fs::create_dir_all(root.join("loaders")).unwrap();
        fs::write(root.join("loaders/old.zip"), vec![0u8; 2_000_000]).unwrap();
        fs::create_dir_all(root.join("saves")).unwrap();
        fs::write(root.join("saves/important"), "S").unwrap();
        let r = disk_report(&h.state);
        assert!(r["caches"].as_array().unwrap().iter().any(|c| c["id"] == "loaders" && c["bytes"].as_u64().unwrap() >= 2_000_000));
        assert!(disk_clean(&h.state, "loaders").contains("Cleared"));
        assert!(!root.join("loaders").exists());
        assert_eq!(disk_clean(&h.state, "../saves"), "Nothing to clean there.");
        assert_eq!(disk_clean(&h.state, "saves"), "Nothing to clean there.");
        assert!(root.join("saves/important").is_file(), "anything outside the list is left alone");
    }

    #[test]
    fn nexus_lists_are_browsed_and_premium_accounts_install_mods_and_their_requirements_in_one_go() {
        if fsutil::find_in_path("bsdtar").is_none() {
            eprintln!("skipping: bsdtar missing");
            return;
        }
        let (h, f, _dir, _id) = nexus_update_setup("nxbrowse", true);
        *f.games_json.lock().unwrap() = r#"[{"domain_name":"coolgame","name":"Plain Game"},{"domain_name":"other","name":"Other"}]"#.into();
        *f.browse_json.lock().unwrap() = r#"[{"mod_id":77,"name":"Base Lib","summary":"<b>A</b> base","version":"1","author":"a","endorsement_count":5,"picture_url":"","available":true},{"mod_id":78,"name":"Hidden","summary":"x","contains_adult_content":true,"available":true}]"#.into();
        *f.files_json.lock().unwrap() = r#"{"files":[{"file_id":8,"name":"Main","file_name":"Cool Pack-5-2-0-300.zip","version":"1.0","category_name":"MAIN","uploaded_timestamp":5},{"file_id":9,"name":"Extra","file_name":"x.zip","version":"1.0","category_name":"OPTIONAL","uploaded_timestamp":9}],"file_updates":[]}"#.into();
        let get = |q: &str| -> Value {
            let (code, body) = http(&h, "GET", q, &[("X-Rig-Token", &h.token)], b"");
            assert_eq!(code, 200);
            serde_json::from_str(&body).unwrap()
        };
        let r = get("/api/nexus_browse?list=trending");
        assert_eq!((r["domain"].as_str(), r["mods"].as_array().unwrap().len(), r["premium"].as_bool()), (Some("coolgame"), 1, Some(true)), "adult content is left out: {r}");
        assert_eq!(r["mods"][0]["summary"], "A base", "markup is removed");
        assert!(get("/api/nexus_browse?list=nonsense")["error"].is_string());
        let mods = || api(&h, "/api/state", json!({}))["library"]["mods"].as_array().unwrap().len();
        let before = mods();

        // "get this mod": the MAIN file, not the newer optional one
        api(&h, "/api/nexus_get", json!({ "mod_id": 77 }));
        wait_until("installed from the list", || mods() == before + 1);
        assert!(f.seen.lock().unwrap().requests.iter().any(|u| u.contains("/files/8/")), "{:?}", f.seen.lock().unwrap().requests);

        // a mod that needs another mod gets it installed too
        let g = selected_game(&h.state).unwrap();
        let lib_id = library_of(&g).unwrap().mods()[0].id.clone();
        h.state.write().unwrap().nexus.needs.push(NeedRow {
            appid: g.appid.clone(),
            lib_id: lib_id.clone(),
            name: "Cool Pack".into(),
            missing: vec![MissingReq { name: "Other Lib".into(), page_url: "https://www.nexusmods.com/coolgame/mods/78".into(), notes: String::new() }],
        });
        api(&h, "/api/nexus_install_needs", json!({ "appid": g.appid, "lib_id": lib_id }));
        wait_until("the required mod is installed", || mods() == before + 2);

        // a hand-picked game name wins, and nonsense is refused
        api(&h, "/api/nexus_domain", json!({ "name": "../evil" }));
        assert!(h.state.read().unwrap().config.nexus_domain.is_empty());
        api(&h, "/api/nexus_domain", json!({ "name": "other" }));
        assert_eq!(get("/api/nexus_browse?list=trending")["domain"], "other");
    }

    #[test]
    fn a_github_release_file_installs_like_any_other_mod() {
        if fsutil::find_in_path("bsdtar").is_none() {
            eprintln!("skipping: bsdtar missing");
            return;
        }
        let h = harness("github");
        let dir = add_plain_game(&h);
        let zip = zip_of(&h, "gh", &[("Data/ghmod.esp", "FROM_GITHUB")]);
        let (base, hits) = github::fake::start("owner", "repo", vec![("CoolMod-1.0.zip", zip), ("source.zip", vec![1]), ("notes.txt", vec![2])]);
        {
            let mut st = h.state.write().unwrap();
            st.github_api = base.clone();
            st.github_allow = Some(format!("{base}/dl/"));
        }
        let get = |q: &str| -> Value {
            let (code, body) = http(&h, "GET", &format!("/api/github?repo={q}"), &[("X-Rig-Token", &h.token)], b"");
            assert_eq!(code, 200);
            serde_json::from_str(&body).unwrap()
        };
        let r = get("https://github.com/owner/repo");
        assert_eq!((r["repo"].as_str(), r["tag"].as_str(), r["files"].as_array().unwrap().len()), (Some("owner/repo"), Some("v1"), 1), "{r}");
        assert!(get("not%20a%20repo")["error"].as_str().unwrap().contains("does not look like"));
        let url = r["files"][0]["url"].as_str().unwrap().to_string();
        api(&h, "/api/github_install", json!({ "repo": "owner/repo", "url": url }));
        wait_until("installed from GitHub", || dir.join("Data/ghmod.esp").is_file());
        assert_eq!(fs::read_to_string(dir.join("Data/ghmod.esp")).unwrap(), "FROM_GITHUB");
        // a file from somewhere else is refused
        let before = hits.lock().unwrap().len();
        api(&h, "/api/github_install", json!({ "repo": "owner/repo", "url": "http://127.0.0.1:1/dl/evil.zip" }));
        // the first install may still log a line after the refusal, so any line counts, not only the last
        wait_until("refused", || h.state.read().unwrap().logs.iter().any(|l| l.message.contains("Refusing a download")));
        assert_eq!(hits.lock().unwrap().len(), before);
    }

    #[test]
    fn settings_are_saved_and_shown_back() {
        let h = harness("settings");
        let st = api(&h, "/api/state", json!({}));
        assert_eq!((st["settings"]["tour_done"].as_bool(), st["settings"]["keep_background"].as_bool(), st["settings"]["install_preview"].as_bool()), (Some(false), Some(false), Some(true)));
        api(&h, "/api/settings", json!({ "tour_done": true, "keep_background": true, "install_preview": false }));
        let st = api(&h, "/api/state", json!({}));
        assert_eq!((st["settings"]["tour_done"].as_bool(), st["settings"]["keep_background"].as_bool(), st["settings"]["install_preview"].as_bool()), (Some(true), Some(true), Some(false)));
        assert_eq!(st["settings"]["ui_scale"], 100);
        api(&h, "/api/settings", json!({ "ui_scale": 500, "reduce_motion": true, "high_contrast": true }));
        let st = api(&h, "/api/state", json!({}));
        assert_eq!((st["settings"]["ui_scale"].as_u64(), st["settings"]["reduce_motion"].as_bool(), st["settings"]["high_contrast"].as_bool()), (Some(200), Some(true), Some(true)), "the size is kept within reason");
        assert!(h.state.read().unwrap().config.no_install_preview);
        let saved: RigConfigData = serde_json::from_str(&fs::read_to_string(&h.state.read().unwrap().paths.config_file).unwrap()).unwrap();
        assert!(saved.tour_done && saved.keep_background, "the choices survive a restart");
        // motion: three levels; themes and accents only take known values; the custom theme is kept within range
        let get = |k: &str| api(&h, "/api/state", json!({}))["settings"][k].clone();
        assert_eq!(get("motion"), "off");
        api(&h, "/api/settings", json!({ "motion": "calm" }));
        assert_eq!((get("motion"), get("reduce_motion")), (json!("calm"), json!(false)));
        api(&h, "/api/settings", json!({ "motion": "full" }));
        assert_eq!(get("motion"), "full");
        assert_eq!((get("sounds"), get("haptics")), (json!(false), json!(false)), "sounds and rumble are off until asked for");
        api(&h, "/api/settings", json!({ "sounds": true, "haptics": true }));
        assert_eq!((get("sounds"), get("haptics")), (json!(true), json!(true)));
        assert_eq!((get("download_limit_kb"), get("downloads_wait")), (json!(0), json!(false)), "downloads are unrestricted until asked");
        api(&h, "/api/settings", json!({ "download_limit_kb": 2048, "downloads_wait": true }));
        assert_eq!((get("download_limit_kb"), get("downloads_wait")), (json!(2048), json!(true)));
        api(&h, "/api/settings", json!({ "download_limit_kb": 0, "downloads_wait": false }));
        assert_eq!(get("checks_while_playing"), json!(false), "background checks wait for the game until told otherwise");
        api(&h, "/api/settings", json!({ "checks_while_playing": true }));
        assert_eq!(get("checks_while_playing"), json!(true));
        api(&h, "/api/settings", json!({ "checks_while_playing": false }));
        api(&h, "/api/settings", json!({ "theme": "midnight", "accent": "pink", "show_adult": true, "accent_from_game": true }));
        assert_eq!((get("theme"), get("accent"), get("show_adult")), (json!("midnight"), json!("pink"), json!(true)));
        api(&h, "/api/settings", json!({ "theme": "<script>", "accent": "#ff0000" }));
        assert_eq!((get("theme"), get("accent")), (json!("midnight"), json!("pink")), "unknown values are refused");
        api(&h, "/api/settings", json!({ "theme": "custom", "custom_theme": { "hue": 999, "radius": 1, "light": true, "evil": "x" } }));
        assert_eq!(get("custom_theme"), json!({ "hue": 360.0, "radius": 4.0, "light": true }));
        api(&h, "/api/settings", json!({ "layout": { "hero": "bar", "tab": "activity", "stats": false, "evil": "x" } }));
        assert_eq!(get("layout"), json!({ "hero": "bar", "tab": "activity", "stats": false, "ring": true, "badges": true }));
        api(&h, "/api/settings", json!({ "layout": { "hero": "<b>", "tab": "../x" } }));
        assert_eq!(get("layout")["hero"], "large", "unknown words fall back to the default");
        assert_eq!(get("layout")["tab"], "mods");
    }

    /// A tiny deterministic generator, so a failure can be reproduced.
    fn noise(seed: u64, len: usize) -> String {
        let mut x = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        let alphabet: Vec<char> = "\"{}[]<>/\\=#%&'.:,;- \t\n\r0123456789abcXYZ_~\u{e9}\u{2122}\u{1F600}\u{0}".chars().collect();
        (0..len)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                alphabet[(x % alphabet.len() as u64) as usize]
            })
            .collect()
    }

    #[test]
    fn parsers_survive_garbage_and_damaged_input_without_panicking() {
        let vdf = "\"UserLocalConfigStore\"\n{\n\t\"Software\"\n\t{\n\t\t\"valve\"\n\t\t{\n\t\t\t\"Steam\"\n\t\t\t{\n\t\t\t\t\"apps\"\n\t\t\t\t{\n\t\t\t\t\t\"7\"\n\t\t\t\t\t{\n\t\t\t\t\t\t\"LaunchOptions\"\t\t\"-x\"\n\t\t\t\t\t}\n\t\t\t\t}\n\t\t\t}\n\t\t}\n\t}\n}\n";
        for seed in 0..400u64 {
            let junk = noise(seed, (seed as usize * 7) % 300);
            let _ = readme::folder_hints(&junk);
            let _ = readme::plain(&junk);
            let _ = thunderstore::parse_ref(&junk);
            let _ = nexus::parse_nxm(&junk);
            let _ = github::parse_repo(&junk);
            let _ = covers::best_match(&junk, &[(1, junk.clone())]);
            let _ = covers::simplified(&junk);
            let _ = compat::expected_sum(&junk);
            let _ = compat::toggle_preset(&junk, "A=1", true);
            let _ = steam::get_launch_options(&junk, "7");
            let _ = steam::set_launch_options(&junk, "7", "x");
            let _ = steam::get_compat_tool(&junk, "7");
            let _ = steam::set_compat_tool(&junk, "7", "proton_9");
            let _ = steamlib::parse_accounts(&junk);
            let _ = steamlib::last_played(&junk);
            // a valid file with a few characters damaged
            let mut b = vdf.as_bytes().to_vec();
            for k in 0..3 {
                let at = (noise(seed + k, 4).bytes().fold(0usize, |a, c| a.wrapping_mul(31).wrapping_add(c as usize))) % b.len();
                b[at] = junk.as_bytes().get(k as usize).copied().unwrap_or(b'}');
            }
            let damaged = String::from_utf8_lossy(&b).to_string();
            let _ = steam::get_launch_options(&damaged, "7");
            let _ = steam::set_launch_options(&damaged, "7", "y");
        }
    }

    /// Speed budget for what the window asks for most: the whole state, with a library of 500 games. It runs a few
    /// times a second while the window is open, so it has to stay far below a frame budget, and twice the games must
    /// not cost much more than twice the time.
    #[test]
    fn the_state_of_a_library_of_hundreds_of_games_is_built_quickly() {
        let h = harness("bigstate");
        fn timed(h: &Harness, games: usize) -> Duration {
            {
                let mut st = h.state.write().unwrap();
                let one = st.games[0].clone();
                st.games = (0..games).map(|i| GameEntry { name: format!("Game number {i}"), appid: format!("9{i:05}"), ..one.clone() }).collect();
            }
            let t = Instant::now();
            let mut size = 0;
            for _ in 0..5 {
                size = read_state(&h.state).to_json().to_string().len();
            }
            assert!(size > games * 50, "{size}");
            t.elapsed() / 5
        }
        let (small, big) = (timed(&h, 250), timed(&h, 500));
        // the automated checks run on shared machines: the numbers are generous on purpose
        let limit = if cfg!(any(windows, target_os = "macos")) { 2000 } else { 600 };
        assert!(big < Duration::from_millis(limit), "the state of 500 games took {big:?}");
        if small > Duration::from_millis(40) {
            assert!(big < small * 5, "twice the games took {big:?} against {small:?}");
        }
    }

    #[test]
    fn a_big_library_deploys_and_reports_quickly() {
        let h = harness("bigdeploy");
        let _dir = add_plain_game(&h);
        let g = selected_game(&h.state).unwrap();
        let lib = library_of(&g).unwrap();
        for m in 0..30 {
            let stage = h._dir.join(&format!("stage-big-{m}"));
            for f in 0..100 {
                let p = stage.join(format!("Data/mod{m}/sub{}/file{f}.bin", f % 7));
                fs::create_dir_all(p.parent().unwrap()).unwrap();
                fs::write(&p, format!("{m}-{f}")).unwrap();
            }
            lib.add_from_dir(&stage, &format!("Big {m}"), "big.zip").unwrap();
        }
        let t = Instant::now();
        let rep = lib.deploy(false).unwrap();
        let deploy = t.elapsed();
        assert_eq!(rep.linked + rep.copied, 3000);
        let t = Instant::now();
        let status = lib.status_json();
        let issues = game_issues(&h.state, &g, false).0;
        let report = problem_report(&h.state, &g);
        let after = t.elapsed();
        assert_eq!(status["deployed"], 3000);
        assert!(issues.is_empty(), "{issues:?}");
        assert!(report.len() > 100);
        assert!(deploy < Duration::from_secs(20), "deploying 3000 files took {deploy:?}");
        // the Windows and macOS runners of the automated checks have slow file systems
        let limit = if cfg!(any(windows, target_os = "macos")) { 8 } else { 2 };
        assert!(after < Duration::from_secs(limit), "status, doctor and report took {after:?}");
        let t = Instant::now();
        lib.purge().unwrap();
        assert!(t.elapsed() < Duration::from_secs(20));
    }

    #[test]
    fn the_setup_travels_through_a_sync_folder_without_the_parts_that_belong_to_one_computer() {
        let h = harness("syncfolder");
        let folder = h._dir.join("Sync");
        fs::create_dir_all(&folder).unwrap();
        let get = |path: &str| -> Value {
            let (code, text) = http(&h, "GET", path, &[("X-Rig-Token", &h.token)], b"");
            assert_eq!(code, 200);
            serde_json::from_str(&text).unwrap()
        };
        // nothing chosen: nothing happens, and it says so
        api(&h, "/api/sync_push", json!({}));
        assert!(last_log(&h).contains("Choose a sync folder first"), "{}", last_log(&h));
        // bad folders are refused
        for bad in ["relative/path", "/nonexistent/folder/here", "/", "", "  "] {
            api(&h, "/api/sync_folder_set", json!({ "path": bad }));
            assert!(h.state.read().unwrap().config.sync_folder.is_empty(), "{bad:?}");
        }
        let own = h.state.read().unwrap().paths.config_file.parent().unwrap().to_path_buf();
        fs::create_dir_all(&own).unwrap();
        api(&h, "/api/sync_folder_set", json!({ "path": own.to_string_lossy() }));
        assert!(h.state.read().unwrap().config.sync_folder.is_empty(), "not one of Gantry's own folders");
        api(&h, "/api/sync_folder_set", json!({ "path": folder.to_string_lossy() }));
        assert!(!h.state.read().unwrap().config.sync_folder.is_empty());
        assert_eq!(api(&h, "/api/state", json!({}))["settings"]["sync_folder"].as_str().map(|f| f.ends_with("Sync")), Some(true));

        // what travels: taste and lists. What stays: this computer's paths, trainers and play time.
        {
            let mut st = h.state.write().unwrap();
            st.config.theme = "midnight".into();
            st.config.game_notes.insert("42".into(), "my note".into());
            st.config.extra_libraries = vec!["/mnt/only-here".into()];
            st.config.playtime_s.insert("42".into(), 999);
            st.config.trainers.insert("42".into(), "/home/me/trainer.exe".into());
        }
        api(&h, "/api/sync_push", json!({}));
        let info = get("/api/sync_info");
        assert_eq!(info["count"], 1, "{info}");
        let dir = sync_dir(&h.state.read().unwrap().config.sync_folder.clone());
        let name = autobackup::list(&dir)[0].name.clone();
        let file = fs::read_to_string(dir.join(&name)).unwrap();
        assert!(file.contains("midnight") && file.contains("my note"));
        assert!(!file.contains("only-here") && !file.contains("trainer.exe") && !file.contains("999"), "no paths or play time in the shared copy");
        // the same setup again is not copied again
        api(&h, "/api/sync_push", json!({}));
        assert_eq!(get("/api/sync_info")["count"], 1);
        assert!(last_log(&h).contains("already has your current setup"), "{}", last_log(&h));

        // another computer changed the look: bringing it in changes the look, and keeps what is this computer's
        let other = file.replace("midnight", "forest");
        fs::write(dir.join(autobackup::file_name(now_ms() + 5000)), other).unwrap();
        api(&h, "/api/sync_pull", json!({}));
        wait_until("the setup coming in", || h.state.read().unwrap().config.theme == "forest");
        let st = h.state.read().unwrap();
        assert_eq!(st.config.extra_libraries, vec!["/mnt/only-here".to_string()]);
        assert_eq!(st.config.playtime_s.get("42"), Some(&999));
        assert_eq!(st.config.trainers.get("42").map(String::as_str), Some("/home/me/trainer.exe"));
        assert_eq!(st.config.game_notes.get("42").map(String::as_str), Some("my note"));
        drop(st);
        assert!(fs::read_dir(h.state.read().unwrap().cache_root().join("backups")).unwrap().flatten().any(|e| e.file_name().to_string_lossy().starts_with("rig_config-")), "the old settings were kept first");
        // switching it off
        api(&h, "/api/sync_folder_set", json!({ "path": "" }));
        assert!(h.state.read().unwrap().config.sync_folder.is_empty());
    }

    #[test]
    fn bringing_a_setup_here_downloads_the_mods_that_are_not_on_this_computer() {
        if fsutil::find_in_path("bsdtar").is_none() {
            eprintln!("skipping: bsdtar missing");
            return;
        }
        let h = harness("bringhere");
        add_plain_game(&h);
        {
            // a game added by hand, so the rescan after the pull keeps it
            let mut st = h.state.write().unwrap();
            st.games.last_mut().unwrap().is_custom = true;
            st.save_custom_games();
        }
        let folder = h._dir.join("Sync");
        fs::create_dir_all(&folder).unwrap();
        api(&h, "/api/sync_folder_set", json!({ "path": folder.to_string_lossy() }));
        let r4 = zip_of(&h, "r4", &[("red4ext/RED4ext.dll", "r")]);
        let (base, hits) = github::fake::start_many(vec![("wopss/RED4ext", "v1.30.0", vec![("red4ext-1.30.0.zip", r4)])]);
        {
            let mut st = h.state.write().unwrap();
            st.github_api = base.clone();
            st.github_allow = Some(format!("{base}/dl/"));
        }
        // the other computer had RED4ext from an older release (gone now) and a mod from Nexus Mods
        let lib = json!({ "mods": [
            { "id": "a", "name": "RED4ext", "version": "1.29.0", "enabled": true, "files": ["red4ext/RED4ext.dll"], "github": { "owner": "wopss", "repo": "RED4ext", "tag": "v1.29.0", "asset": "red4ext-1.29.0.zip" } },
            { "id": "b", "name": "Cool Cars", "version": "2", "enabled": true, "files": ["x.archive"], "nexus": { "game": "cyberpunk2077", "mod_id": 77, "file_id": 1 } },
        ] });
        let bundle = json!({ "gantry_backup": 1, "created_ms": now_ms(), "config": {}, "custom_games": [], "libraries": [{ "appid": "plain1", "library": lib }] });
        let dir = sync_dir(&h.state.read().unwrap().config.sync_folder.clone());
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join(autobackup::file_name(now_ms())), bundle.to_string()).unwrap();

        api(&h, "/api/sync_pull", json!({}));
        wait_until("the setup is here", || last_log(&h).contains("Your setup is here"));
        let g = selected_game(&h.state).unwrap();
        let mods = library_of(&g).unwrap().mods();
        let r = mods.iter().find(|m| m.name == "RED4ext").expect("fetched from its releases");
        assert_eq!(r.github.as_ref().map(|x| x.tag.as_str()), Some("v1.30.0"), "the old release is gone, so the newest one's file");
        assert!(hits.lock().unwrap().iter().any(|u| u.contains("/releases/tags/v1.29.0")), "the same release was tried first");
        let pending = api(&h, "/api/state", json!({}))["library"]["pending_pack"].clone();
        assert_eq!(pending["mods"][0]["name"], "Cool Cars", "the Nexus one waits to be got: {pending}");
    }

    #[test]
    fn screenshots_are_listed_and_served_by_their_place_in_the_list() {
        let h = harness("screenshots");
        let dir = add_plain_game(&h);
        let _ = dir;
        h.state.write().unwrap().games.last_mut().unwrap().steam_appid = "620".into();
        let root = h._dir.join("FakeSteam");
        let shots = root.join("userdata/9/760/remote/620/screenshots");
        fs::create_dir_all(shots.join("thumbnails")).unwrap();
        let now = now_ms();
        for (name, ago_ms) in [("new.jpg", 1000u64), ("old.jpg", 5_000_000)] {
            let p = shots.join(name);
            fs::write(&p, format!("FULL {name}")).unwrap();
            fs::File::options().write(true).open(&p).unwrap().set_modified(std::time::UNIX_EPOCH + Duration::from_millis(now - ago_ms)).unwrap();
        }
        fs::write(shots.join("thumbnails/new.jpg"), "SMALL new.jpg").unwrap();
        h.state.write().unwrap().steam_lib.roots = Some(vec![root]);
        // one session with mods around the newer picture
        let g = selected_game(&h.state).unwrap();
        let sessions = h.state.read().unwrap().paths.config_file.with_file_name("sessions.jsonl");
        stats::record(&sessions, &stats::Session { appid: g.appid.clone(), name: g.name.clone(), start_ms: now - 600_000, end_ms: now, secs: 600, crashed: false, mods: 2, perf: None, ..Default::default() });
        let (code, body) = http(&h, "GET", "/api/screenshots", &[("X-Rig-Token", &h.token)], b"");
        assert_eq!(code, 200);
        let v: Value = serde_json::from_str(&body).unwrap();
        assert_eq!((v["items"][0]["name"].as_str(), v["items"][0]["kind"].as_str(), v["items"][1]["kind"].as_str(), v["modded"].as_u64()), (Some("new.jpg"), Some("modded"), Some("unknown"), Some(1)), "{v}");
        let get = |q: &str| http(&h, "GET", &format!("/api/screenshot?{q}"), &[("X-Rig-Token", &h.token)], b"");
        assert_eq!(get("n=0&thumb=1"), (200, "SMALL new.jpg".to_string()));
        assert_eq!(get("n=0"), (200, "FULL new.jpg".to_string()));
        assert_eq!(get("n=1&thumb=1"), (200, "FULL old.jpg".to_string()), "no small copy: the picture itself");
        for bad in ["n=2", "n=-1", "n=abc", "", "n=0/../../etc/passwd"] {
            assert_eq!(get(bad).0, 404, "{bad}");
        }
        // the token can also come in the address, which is what an <img> tag can do
        let (c, _) = http(&h, "GET", &format!("/api/screenshot?n=0&t={}", h.token), &[], b"");
        assert_eq!(c, 200);
        assert_eq!(http(&h, "GET", "/api/screenshot?n=0", &[], b"").0, 403);
    }

    #[test]
    fn a_mod_from_github_is_checked_for_a_newer_release_and_replaced_by_it() {
        if fsutil::find_in_path("bsdtar").is_none() || fsutil::find_in_path("curl").is_none() {
            return;
        }
        let h = harness("ghupdate");
        let _dir = add_plain_game(&h);
        let g = selected_game(&h.state).unwrap();
        let lib = library_of(&g).unwrap();
        let stage = h._dir.join("gh-stage");
        fs::create_dir_all(stage.join("Data")).unwrap();
        fs::write(stage.join("Data/cool.esp"), "V1").unwrap();
        let m = lib.add_with(&stage, "Cool Mod", "Cool-Mod-1.0.0.zip", None, "1.0.0").unwrap();
        lib.set_github(&m.id, github::GithubRef { owner: "owner".into(), repo: "Cool-Mod".into(), tag: "v1.0.0".into(), asset: "Cool-Mod-1.0.0.zip".into() }).unwrap();
        refresh_mods_shared(&h.state);
        // the release is the same: nothing to do
        let (base, _) = github::fake::start_tagged("owner", "Cool-Mod", "v1.0.0", vec![("Cool-Mod-1.0.0.zip", zip_of(&h, "gh1", &[("Data/cool.esp", "V1")]))]);
        {
            let mut st = h.state.write().unwrap();
            st.github_api = base.clone();
            st.github_allow = Some(base);
        }
        assert_eq!(check_github_updates(&h.state), 0);
        // a newer one, with the same file under a new name
        let (base, _) = github::fake::start_tagged("owner", "Cool-Mod", "v1.1.0", vec![("Cool-Mod-1.1.0.zip", zip_of(&h, "gh2", &[("Data/cool.esp", "V2")])), ("source.zip", b"x".to_vec())]);
        {
            let mut st = h.state.write().unwrap();
            st.github_api = base.clone();
            st.github_allow = Some(base);
        }
        assert_eq!(check_github_updates(&h.state), 1);
        let row = h.state.read().unwrap().github_updates[0].clone();
        assert_eq!((row.name.as_str(), row.installed_version.as_str(), row.new_version.as_str(), row.kind), ("Cool Mod", "1.0.0", "1.1.0", "github"));
        assert!(row.url.ends_with("/dl/Cool-Mod-1.1.0.zip"), "{}", row.url);
        // the interface sees it among the updates (its existing button applies it)
        let st = api(&h, "/api/state", json!({}));
        assert!(st["thunder_updates"].as_array().unwrap().iter().any(|u| u["lib_id"] == json!(m.id) && u["kind"] == "github" && u["new_version"] == "1.1.0"), "{}", st["thunder_updates"]);
        // applying it replaces the mod instead of adding a second one, and remembers the new release
        api(&h, "/api/thunder_update", json!({ "appid": g.appid, "id": m.id }));
        wait_until("the update", || lib.mods().iter().any(|x| x.github.as_ref().map(|r| r.tag.as_str()) == Some("v1.1.0")));
        let mods = lib.mods();
        assert_eq!(mods.len(), 1, "replaced, not added: {mods:?}");
        assert_eq!((mods[0].github.as_ref().map(|r| r.asset.as_str()), mods[0].version.as_str()), (Some("Cool-Mod-1.1.0.zip"), "1.1.0"));
        // and the finished update is gone from the list
        wait_until("the list to clear", || {
            prune_github_updates(&h.state);
            h.state.read().unwrap().github_updates.is_empty()
        });
        // a mod set to "keep this version" is not offered updates
        lib.set_update_rule(&mods[0].id, "never").unwrap();
        let (base, _) = github::fake::start_tagged("owner", "Cool-Mod", "v9.0.0", vec![("Cool-Mod-9.0.0.zip", b"x".to_vec())]);
        h.state.write().unwrap().github_api = base;
        assert_eq!(check_github_updates(&h.state), 0);
    }

    #[test]
    fn the_quick_folder_buttons_open_only_folders_that_exist() {
        let h = harness("quickfolders");
        let g = selected_game(&h.state).unwrap();
        let cache = h._dir.join("cache");
        fs::create_dir_all(&cache).unwrap();
        assert_eq!(folder_target(&g, "", &cache, &[]), Some(PathBuf::from(&g.game_dir)));
        assert_eq!(folder_target(&g, "game", &cache, &[]), Some(PathBuf::from(&g.game_dir)));
        assert_eq!(folder_target(&g, "prefix", &cache, &[]), Some(PathBuf::from(&g.pfx)));
        assert_eq!(folder_target(&g, "logs", &cache, &[]), Some(cache.clone()));
        assert_eq!(folder_target(&g, "../../etc", &cache, &[]), None);
        assert_eq!(folder_target(&g, "logs", &h._dir.join("nowhere"), &[]), None);
        let mut no_prefix = g.clone();
        no_prefix.pfx = String::new();
        assert_eq!(folder_target(&no_prefix, "prefix", &cache, &[]), None);
        // the saves folder is the first place the game keeps saves
        #[cfg(not(windows))]
        {
            let live = PathBuf::from(&g.pfx).join("drive_c/users/steamuser/AppData/Local/Demo/Saved/SaveGames");
            fs::create_dir_all(&live).unwrap();
            assert_eq!(folder_target(&g, "saves", &cache, &[]), Some(live));
        }
        api(&h, "/api/open_folder", json!({ "which": "saves" }));
        api(&h, "/api/open_folder", json!({}));
    }

    #[test]
    fn open_mod_folder_opens_staged_directory() {
        let h = harness("openmodfolder");
        let g = selected_game(&h.state).unwrap();
        let lib = library_of(&g).unwrap();
        let d = h._dir.join("test-staged");
        fs::create_dir_all(d.join("Data")).unwrap();
        fs::write(d.join("Data/test.esp"), "test").unwrap();
        let m = lib.add_from_dir(&d, "Staged Test Mod", "staged.zip").unwrap();

        assert!(lib.mod_files_dir(&m.id).exists());

        let res = api(&h, "/api/open_mod_folder", json!({ "id": m.id }));
        assert!(res["games"].is_array());

        let res_bad = api(&h, "/api/open_mod_folder", json!({ "id": "unknown-id" }));
        assert!(res_bad["games"].is_array());
    }

    #[test]
    fn clean_prefix_caches_endpoint_cleans_caches() {
        let h = harness("pfxcleanend");
        let g = selected_game(&h.state).unwrap();
        let cache_file = PathBuf::from(&g.compatdata).join("dxvk.cache");
        fs::create_dir_all(&g.compatdata).unwrap();
        fs::write(&cache_file, b"cache").unwrap();
        assert!(cache_file.exists());

        let res = api(&h, "/api/clean_prefix_caches", json!({}));
        assert!(res["games"].is_array());
        assert!(!cache_file.exists());

        let res2 = api(&h, "/api/clean_prefix_caches", json!({}));
        assert!(res2["games"].is_array());
    }

    #[test]
    fn mods_can_be_set_aside_in_the_store_and_found_again() {
        let h = harness("savedmods");
        let appid = selected_game(&h.state).unwrap().appid;
        let card = json!({ "source": "nexus", "id": "77", "name": "Cool Cars", "summary": "cars", "author": "n", "version": "1.2", "downloads": 900, "endorsements": 5, "picture": "https://images.nexusmods.com/x.png", "page_url": "https://www.nexusmods.com/coolgame/mods/77" });
        api(&h, "/api/saved_toggle", card.clone());
        let st = api(&h, "/api/state", json!({}));
        assert_eq!(st["saved_mods"], json!(["nexus:77"]));
        let (code, body) = http(&h, "GET", "/api/store?sort=saved", &[("X-Rig-Token", &h.token)], b"");
        assert_eq!(code, 200);
        let page: Value = serde_json::from_str(&body).unwrap();
        assert_eq!((page["cards"][0]["name"].as_str(), page["cards"][0]["downloads"].as_u64(), page["cards"][0]["installed"].as_bool()), (Some("Cool Cars"), Some(900), Some(false)), "{page}");
        // it survives a restart of the settings file, and a second press takes it out again
        assert!(fs::read_to_string(&h.state.read().unwrap().paths.config_file).unwrap().contains("Cool Cars"));
        // hostile cards are refused or cleaned
        for bad in [json!({ "source": "evil", "id": "1", "name": "x" }), json!({ "source": "nexus", "id": "../1", "name": "x" }), json!({ "source": "nexus", "id": "5", "name": "" }), json!({ "source": "thunderstore", "id": "a b", "name": "x" })] {
            api(&h, "/api/saved_toggle", bad);
        }
        api(&h, "/api/saved_toggle", json!({ "source": "thunderstore", "id": "Own-Mod", "name": "Own Mod", "picture": "javascript:alert(1)", "page_url": "https://evil.example/x" }));
        let list = h.state.read().unwrap().config.saved_mods.get(&appid).cloned().unwrap();
        assert_eq!(list.len(), 2);
        assert_eq!((list[0]["picture"].as_str(), list[0]["page_url"].as_str()), (Some(""), Some("")), "only web pictures and the sites' own pages");
        api(&h, "/api/saved_toggle", card);
        api(&h, "/api/saved_toggle", json!({ "source": "thunderstore", "id": "Own-Mod", "name": "Own Mod" }));
        assert!(!h.state.read().unwrap().config.saved_mods.contains_key(&appid), "an empty list is not kept");
        assert_eq!(api(&h, "/api/state", json!({}))["saved_mods"], json!([]));
    }

    #[test]
    fn the_doctor_lists_every_installed_game_with_what_is_wrong() {
        let h = harness("doctor");
        let dir = add_plain_game(&h);
        let (code, body) = http(&h, "GET", "/api/doctor_all", &[("X-Rig-Token", &h.token)], b"");
        assert_eq!(code, 200);
        let d: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(d["version"], env!("CARGO_PKG_VERSION"));
        let names: Vec<&str> = d["games"].as_array().unwrap().iter().filter_map(|g| g["name"].as_str()).collect();
        assert!(names.contains(&"Plain Game"), "{names:?}");
        assert_eq!(http(&h, "GET", "/api/doctor_all", &[], b"").0, 403, "token required like every API call");
        // a game with a deployed mod that something then changed shows the drift
        let g = selected_game(&h.state).unwrap();
        let lib = library_of(&g).unwrap();
        let stage = h._dir.join("doctor-stage");
        fs::create_dir_all(stage.join("Data")).unwrap();
        fs::write(stage.join("Data/base.esp"), "MOD").unwrap();
        lib.add_from_dir(&stage, "Overhaul", "o.zip").unwrap();
        lib.deploy(false).unwrap();
        fs::remove_file(dir.join("Data/base.esp")).unwrap();
        fs::write(dir.join("Data/base.esp"), "UPDATED BY STEAM").unwrap();
        let (_, body) = http(&h, "GET", "/api/doctor_all", &[("X-Rig-Token", &h.token)], b"");
        let d: Value = serde_json::from_str(&body).unwrap();
        let row = d["games"].as_array().unwrap().iter().find(|g| g["name"] == "Plain Game").unwrap().clone();
        assert_eq!((row["mods"].as_u64(), row["mods_on"].as_u64()), (Some(1), Some(1)));
        let text = doctor_text(&d);
        assert!(text.starts_with(&format!("ModForge {} on ", env!("CARGO_PKG_VERSION"))), "{text}");
        assert!(text.contains("Plain Game") && text.contains("1 of 1 mods on") && text.contains("drift"), "{text}");
        assert!(text.contains("[warn]"), "{text}");
        assert_eq!(handle_cli(vec!["--help".into()]), Some(0));
    }

    #[test]
    fn the_window_size_is_remembered_and_nonsense_is_ignored() {
        assert_eq!(window_size_or_default(0, 0), (1280, 825));
        assert_eq!(window_size_or_default(1600, 900), (1600, 900));
        for (w, h) in [(10, 10), (99_999, 900), (1600, 0), (639, 900)] {
            assert_eq!(window_size_or_default(w, h), (1280, 825), "{w}x{h}");
        }
        let h = harness("winsize");
        api(&h, "/api/settings", json!({ "window_size": [1500, 950] }));
        assert_eq!((h.state.read().unwrap().config.window_w, h.state.read().unwrap().config.window_h), (1500, 950));
        api(&h, "/api/settings", json!({ "window_size": [5, 5] }));
        api(&h, "/api/settings", json!({ "window_size": "big" }));
        api(&h, "/api/settings", json!({ "window_size": [1500] }));
        assert_eq!((h.state.read().unwrap().config.window_w, h.state.read().unwrap().config.window_h), (1500, 950), "unchanged");
    }

    #[test]
    fn the_help_buttons_only_reach_gantrys_own_pages() {
        assert_eq!(docs_url("faq").as_deref(), Some("https://github.com/pigeoonn/gantry/blob/main/docs/FAQ.md"));
        assert!(docs_url("guide").is_some() && docs_url("reference").is_some() && docs_url("tutorial").is_some() && docs_url("install").is_some());
        for bad in ["", "../x", "GUIDE.md", "https://example.com", "faq/../../x"] {
            assert!(docs_url(bad).is_none(), "{bad}");
        }
        let h = harness("docslink");
        api(&h, "/api/open_link", json!({ "which": "docs", "page": "guide" }));
        api(&h, "/api/open_link", json!({ "which": "docs", "page": "nope" }));
    }

    #[test]
    fn the_issue_form_opens_with_the_version_or_the_game_filled_in() {
        let bug = issue_url(false, "Gantry 5.1.0 on Arch Linux", "ignored");
        assert_eq!(bug, "https://github.com/pigeoonn/gantry/issues/new?template=bug.yml&version=Gantry%205.1.0%20on%20Arch%20Linux");
        let game = issue_url(true, "ignored", "Mafia III & more (Steam or custom)");
        assert!(game.starts_with("https://github.com/pigeoonn/gantry/issues/new?template=game.yml&game=Mafia%20III%20%26%20more"), "{game}");
        assert!(!game.contains(' ') && !game.contains("ignored"));
        // the button only ever reaches GitHub
        let h = harness("issueform");
        api(&h, "/api/open_link", json!({ "which": "report", "kind": "bug" }));
        api(&h, "/api/open_link", json!({ "which": "report", "kind": "game" }));
    }

    #[test]
    fn a_newer_gantry_release_is_noticed_and_an_older_or_equal_one_is_not() {
        let h = harness("selfupdate");
        for (tag, newer) in [("v99.0.0", true), ("v5.0.0", false), ("v0.1.0", false)] {
            let (base, _) = github::fake::start_tagged("pigeoonn", "gantry", tag, vec![]);
            h.state.write().unwrap().github_api = base;
            assert_eq!(newer_gantry(&h.state).is_some(), newer, "{tag}");
        }
        let (base, _) = github::fake::start_tagged("pigeoonn", "gantry", "v99.0.0", vec![]);
        h.state.write().unwrap().github_api = base;
        api(&h, "/api/self_update_check", json!({}));
        wait_until("noticed", || api(&h, "/api/state", json!({}))["new_version"] == "v99.0.0");
        assert_eq!(api(&h, "/api/state", json!({}))["app_version"], env!("CARGO_PKG_VERSION"));
        // a beta is a pre-release, which GitHub's "latest" answer leaves out: the list has it
        let (base, _) = github::fake::start_prerelease_only("pigeoonn", "gantry", "v99.1.0");
        h.state.write().unwrap().github_api = base;
        assert_eq!(newer_gantry(&h.state).map(|(t, _)| t).as_deref(), Some("v99.1.0"));
        // nothing answers (a private repository, no connection): quietly no news
        h.state.write().unwrap().github_api = "http://127.0.0.1:1".into();
        assert!(newer_gantry(&h.state).is_none());
    }

    #[test]
    fn gantry_replaces_itself_only_with_a_file_whose_checksum_matches() {
        if cfg!(windows) {
            return; // the Windows build comes in a zip; the pieces are tested in selfupdate.rs
        }
        let h = harness("inplace");
        let target = h._dir.join("Gantry.AppImage");
        let name = format!("Gantry-99.0.0-{}.AppImage", std::env::consts::ARCH);
        let sum_of = |bytes: &[u8]| hashing::sha256_hex(&mut &bytes[..]).unwrap();
        let serve = |sum: Option<String>| {
            let mut files = vec![(name.as_str(), b"NEW PROGRAM".to_vec())];
            if let Some(s) = sum {
                files.push((Box::leak(format!("{name}.sha256").into_boxed_str()) as &str, format!("{s}  {name}\n").into_bytes()));
            }
            let (base, hits) = github::fake::start_many(vec![("pigeoonn/gantry", "v99.0.0", files)]);
            let mut st = h.state.write().unwrap();
            st.github_allow = Some(format!("{base}/dl/"));
            st.github_api = base;
            hits
        };
        fs::write(&target, "OLD PROGRAM").unwrap();
        let ready = || api(&h, "/api/state", json!({}))["update_ready"].clone();

        // an install that cannot replace itself (a package, a tarball) says so and changes nothing
        assert_eq!(api(&h, "/api/state", json!({}))["can_self_update"], false);
        h.state.write().unwrap().new_version = Some("v99.0.0".into());
        api(&h, "/api/self_update", json!({}));
        wait_until("told", || last_log(&h).contains("cannot update itself"));

        h.state.write().unwrap().self_target = Some(target.clone());
        assert_eq!(api(&h, "/api/state", json!({}))["can_self_update"], true);
        // a wrong checksum, a missing checksum file and a download from somewhere else: the program stays as it was
        serve(Some("0".repeat(64)));
        api(&h, "/api/self_update", json!({}));
        wait_until("refused", || last_log(&h).contains("does not match its checksum"));
        serve(None);
        api(&h, "/api/self_update", json!({}));
        wait_until("no checksum", || last_log(&h).contains("no file for this kind of install"));
        serve(Some(sum_of(b"NEW PROGRAM")));
        h.state.write().unwrap().github_allow = None;
        api(&h, "/api/self_update", json!({}));
        wait_until("not from its releases", || last_log(&h).contains("not from ModForge's own releases"));
        assert_eq!(fs::read_to_string(&target).unwrap(), "OLD PROGRAM");
        assert_eq!(ready(), Value::Null);

        // the right file: swapped in, runnable, the old copy gone, a restart asked for
        let base = h.state.read().unwrap().github_api.clone();
        h.state.write().unwrap().github_allow = Some(format!("{base}/dl/"));
        api(&h, "/api/self_update", json!({}));
        wait_until("in place", || ready() == "v99.0.0");
        assert_eq!(fs::read_to_string(&target).unwrap(), "NEW PROGRAM");
        assert!(!h._dir.join("Gantry.AppImage.old").exists());
        assert!(last_log(&h).contains("Close ModForge and open it again"), "{}", last_log(&h));
        // a tag with odd characters is not looked up at all
        h.state.write().unwrap().new_version = Some("v1/../../x".into());
        api(&h, "/api/self_update", json!({}));
        wait_until("no newer", || last_log(&h).contains("no newer ModForge"));
    }

    #[test]
    fn a_failure_of_gantry_is_announced_once_and_the_reports_stay_readable() {
        let h = harness("selfcrash");
        let dir = h._dir.join("crash");
        let told = || h.state.read().unwrap().notes.json()["items"].as_array().unwrap().iter().filter(|n| n["kind"] == "selfcrash").count();
        announce_crash_reports(&h.state, &dir);
        assert_eq!(api(&h, "/api/state", json!({}))["crash_reports"], 0);
        assert_eq!(told(), 0);
        crashreport::write(&dir, 5, "5.4.0", "Linux", "panicked at src/x.rs:1", "log line", "/home/ann").unwrap();
        announce_crash_reports(&h.state, &dir);
        announce_crash_reports(&h.state, &dir);
        assert_eq!(told(), 1, "told once, not at every start");
        assert_eq!(api(&h, "/api/state", json!({}))["crash_reports"], 1);
        assert!(h.state.read().unwrap().config.crash_seen_ms > 0);
        // a newer failure is told about again
        std::thread::sleep(Duration::from_millis(5));
        crashreport::write(&dir, now_ms() + 1000, "5.4.0", "Linux", "panicked again", "", "/home/ann").unwrap();
        let before = h.state.read().unwrap().config.crash_seen_ms;
        announce_crash_reports(&h.state, &dir);
        assert_eq!(api(&h, "/api/state", json!({}))["crash_reports"], 2);
        assert!(h.state.read().unwrap().config.crash_seen_ms >= before);
        assert!(last_log(&h).contains("stopped unexpectedly"), "{}", last_log(&h));
        assert_eq!(told(), 1, "the same words within a minute are one note");
        api(&h, "/api/open_crash_reports", json!({}));
    }

    #[test]
    fn gantry_is_added_to_steams_library_only_while_steam_is_closed_and_only_to_a_file_it_understands() {
        let h = harness("steamshortcut");
        let cfg = h._dir.join("steam-acct/config");
        fs::create_dir_all(&cfg).unwrap();
        let file = cfg.join("shortcuts.vdf");
        {
            let mut st = h.state.write().unwrap();
            st.steam_env.localconfig = Some(cfg.join("localconfig.vdf"));
            st.steam_env.running = Some(true);
        }
        // Steam is open: nothing is written
        api(&h, "/api/steam_shortcut", json!({}));
        wait_until("told", || last_log(&h).contains("Steam is running"));
        assert!(!file.exists());
        // a file in a form nobody knows is left alone
        h.state.write().unwrap().steam_env.running = Some(false);
        fs::write(&file, b"not a shortcuts file").unwrap();
        api(&h, "/api/steam_shortcut", json!({}));
        wait_until("refused", || last_log(&h).contains("a form ModForge does not know"));
        assert_eq!(fs::read(&file).unwrap(), b"not a shortcuts file");
        // an empty folder gets the entry, with the program and controller mode
        fs::remove_file(&file).unwrap();
        api(&h, "/api/steam_shortcut", json!({}));
        wait_until("added", || last_log(&h).contains("is in Steam's library now"));
        let bytes = fs::read(&file).unwrap();
        let exe = std::env::current_exe().unwrap().to_string_lossy().to_string();
        assert_eq!(steamshortcut::has_gantry(&bytes, &exe), Some(true));
        assert!(String::from_utf8_lossy(&bytes).contains("--deck"));
        // again: it is there already, and the file is not written a second time
        api(&h, "/api/steam_shortcut", json!({}));
        wait_until("already", || last_log(&h).contains("already in Steam's library"));
        assert_eq!(fs::read(&file).unwrap(), bytes);
    }

    #[test]
    fn launching_puts_mods_back_after_a_game_update_and_warns_about_missing_requirements() {
        let h = harness("prelaunch");
        let dir = add_plain_game(&h);
        let g = selected_game(&h.state).unwrap();
        let lib = library_of(&g).unwrap();
        let stage = h._dir.join("stage-prelaunch");
        fs::create_dir_all(stage.join("Data")).unwrap();
        fs::write(stage.join("Data/base.esp"), "MODDED").unwrap();
        let m = lib.add_from_dir(&stage, "Overhaul", "o.zip").unwrap();
        lib.deploy(false).unwrap();
        assert!(lib.drift().is_empty());
        // the game updates itself: a new file where our link was
        fs::remove_file(dir.join("Data/base.esp")).unwrap();
        fs::write(dir.join("Data/base.esp"), "UPDATED").unwrap();
        h.state.write().unwrap().nexus.needs.push(NeedRow {
            appid: g.appid.clone(),
            lib_id: m.id.clone(),
            name: "Overhaul".into(),
            missing: vec![MissingReq { name: "Base Lib".into(), page_url: String::new(), notes: String::new() }],
        });
        prelaunch_check(&h.state, &g);
        assert_eq!(fs::read_to_string(dir.join("Data/base.esp")).unwrap(), "MODDED", "the mod is back");
        assert!(lib.drift().is_empty());
        let logs: Vec<String> = h.state.read().unwrap().logs.iter().map(|l| l.message.clone()).collect();
        assert!(logs.iter().any(|l| l.contains("changed since the last deploy")), "{logs:?}");
        assert!(logs.iter().any(|l| l.contains("Overhaul needs Base Lib")), "{logs:?}");
        // nothing to do when everything is in place: no re-deploy, no noise
        let before = logs.len();
        prelaunch_check(&h.state, &g);
        let after: Vec<String> = h.state.read().unwrap().logs.iter().map(|l| l.message.clone()).collect();
        assert!(!after[before..].iter().any(|l| l.contains("Deployed")), "{after:?}");
    }

    #[test]
    fn a_game_steam_is_updating_is_flagged_and_not_started() {
        let h = harness("steamupd");
        let dir = add_plain_game(&h);
        let lib = dir.parent().unwrap().parent().unwrap().to_path_buf();
        h.state.write().unwrap().games.last_mut().unwrap().steam_appid = "4242".into();
        let g = selected_game(&h.state).unwrap();
        TEST_STEAM_RUNNING.with(|c| c.set(Some(true)));
        let codes = |h: &Harness, g: &GameEntry| game_issues(&h.state, g, false).0.iter().map(|i| i["code"].as_str().unwrap().to_string()).collect::<Vec<_>>();
        // no manifest, nothing said
        assert!(!codes(&h, &g).iter().any(|c| c.starts_with("steam_update")));
        fs::write(lib.join("appmanifest_4242.acf"), "\"AppState\"\n{\n\t\"StateFlags\"\t\t\"4\"\n}\n").unwrap();
        assert!(!codes(&h, &g).iter().any(|c| c.starts_with("steam_update")), "installed and quiet");
        fs::write(lib.join("appmanifest_4242.acf"), "\"AppState\"\n{\n\t\"StateFlags\"\t\t\"6\"\n}\n").unwrap();
        assert!(codes(&h, &g).contains(&"steam_update_waiting".to_string()));
        // an update in progress: shown, and Play refuses instead of starting half-updated files
        fs::write(lib.join("appmanifest_4242.acf"), "\"AppState\"\n{\n\t\"StateFlags\"\t\t\"1048580\"\n\t\"BytesToDownload\"\t\t\"200\"\n\t\"BytesDownloaded\"\t\t\"50\"\n}\n").unwrap();
        let issues = game_issues(&h.state, &g, false).0;
        let i = issues.iter().find(|i| i["code"] == "steam_updating").expect("flagged");
        assert_eq!((i["level"].as_str(), i["args"][0].as_str()), (Some("warn"), Some("25")));
        play_game(&h.state);
        assert!(!h.state.read().unwrap().is_running.load(Ordering::SeqCst));
        let logs: Vec<String> = h.state.read().unwrap().logs.iter().map(|l| l.message.clone()).collect();
        assert!(logs.iter().any(|m| m.contains("Steam is updating Plain Game")), "{logs:?}");
        // Steam is not running: a record left behind by a crash does not hold the game back
        TEST_STEAM_RUNNING.with(|c| c.set(Some(false)));
        assert!(!codes(&h, &g).iter().any(|c| c.starts_with("steam_update")), "stale record while Steam is closed");
        TEST_STEAM_RUNNING.with(|c| c.set(Some(true)));
        // a game that does not come from Steam is never held back
        h.state.write().unwrap().games.last_mut().unwrap().steam_appid = String::new();
        assert!(!codes(&h, &selected_game(&h.state).unwrap()).iter().any(|c| c.starts_with("steam_update")));
    }

    #[test]
    fn workshop_items_of_a_steam_game_are_listed_with_names_and_never_changed() {
        if fsutil::find_in_path("curl").is_none() {
            return;
        }
        let h = harness("workshop");
        let dir = add_plain_game(&h);
        let lib = dir.parent().unwrap().parent().unwrap().to_path_buf();
        h.state.write().unwrap().games.last_mut().unwrap().steam_appid = "294100".into();
        let get = |path: &str| {
            let (code, body) = http(&h, "GET", path, &[("X-Rig-Token", &h.token)], b"");
            assert_eq!(code, 200);
            serde_json::from_str::<Value>(&body).unwrap()
        };
        assert_eq!(get("/api/workshop")["items"].as_array().unwrap().len(), 0, "nothing installed, nothing listed");
        fs::create_dir_all(lib.join("workshop/content/294100/555")).unwrap();
        fs::write(lib.join("workshop/content/294100/555/About.xml"), "x").unwrap();
        let acf = "\"AppWorkshop\"\n{\n\t\"WorkshopItemsInstalled\"\n\t{\n\t\t\"555\"\n\t\t{\n\t\t\t\"size\"\t\t\"1\"\n\t\t\t\"timeupdated\"\t\t\"9\"\n\t\t}\n\t}\n}\n";
        fs::write(lib.join("workshop/appworkshop_294100.acf"), acf).unwrap();
        // a fake Steam that knows the item
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let base = format!("http://{}", server.server_addr().to_ip().unwrap());
        std::thread::spawn(move || {
            for req in server.incoming_requests() {
                let _ = req.respond(tiny_http::Response::from_string(r#"{"response":{"publishedfiledetails":[{"publishedfileid":"555","result":1,"title":"Colony Tweaks","subscriptions":77}]}}"#));
            }
        });
        {
            let mut st = h.state.write().unwrap();
            st.steam_lib.base = base;
            st.steam_lib.allow_http = true;
        }
        let v = get("/api/workshop");
        assert_eq!((v["items"][0]["id"].as_str(), v["items"][0]["title"].as_str(), v["items"][0]["subscribers"].as_u64()), (Some("555"), Some("Colony Tweaks"), Some(77)), "{v}");
        assert_eq!(library_status(&selected_game(&h.state).unwrap(), false)["workshop"], 1);
        // opening an item's page or folder only ever takes digits, and changes nothing
        api(&h, "/api/workshop_folder", json!({ "id": "../../etc" }));
        api(&h, "/api/workshop_folder", json!({ "id": "555" }));
        api(&h, "/api/open_link", json!({ "which": "workshop", "id": "555" }));
        assert!(lib.join("workshop/content/294100/555/About.xml").is_file());
        assert_eq!(fs::read_to_string(lib.join("workshop/appworkshop_294100.acf")).unwrap(), acf);
        // a game found in a Steam library is its own id (the usual case): it has the same Workshop
        {
            let mut st = h.state.write().unwrap();
            let g = st.games.last_mut().unwrap();
            g.steam_appid = String::new();
            g.appid = "294100".into();
        }
        assert_eq!(get("/api/workshop")["items"].as_array().unwrap().len(), 1);
        assert_eq!(selected_game(&h.state).unwrap().steam_id(), "294100");
        // a game of another launcher, or a custom one, is not a Steam install
        h.state.write().unwrap().games.last_mut().unwrap().source = "heroic".into();
        assert!(get("/api/workshop")["items"].as_array().unwrap().is_empty());
        {
            let mut st = h.state.write().unwrap();
            let g = st.games.last_mut().unwrap();
            g.source = String::new();
            g.appid = "plain1".into();
        }
        assert!(get("/api/workshop")["items"].as_array().unwrap().is_empty());
    }

    #[test]
    fn the_welcome_list_says_what_each_game_needs() {
        let h = harness("welcome");
        let dir = add_plain_game(&h);
        let rows = || {
            let (code, body) = http(&h, "GET", "/api/welcome", &[("X-Rig-Token", &h.token)], b"");
            assert_eq!(code, 200);
            serde_json::from_str::<Value>(&body).unwrap()["games"].as_array().cloned().unwrap_or_default()
        };
        let mine = || rows().into_iter().find(|r| r["appid"] == "plain_game").or_else(|| rows().into_iter().last()).unwrap();
        assert_eq!(mine()["verdict"], "ready", "a game without an engine of interest takes mods as they are");
        // a Unity game with native code but no loader gets the IL2CPP loader suggested
        let mut pe = vec![0u8; 128];
        pe[0] = b'M';
        pe[1] = b'Z';
        pe[60] = 64;
        pe[64..68].copy_from_slice(b"PE\0\0");
        pe[68..70].copy_from_slice(&0x8664u16.to_le_bytes());
        fs::write(dir.join("UnityPlayer.dll"), pe).unwrap();
        fs::write(dir.join("GameAssembly.dll"), "x").unwrap();
        fs::create_dir_all(dir.join("Plain_Data")).unwrap();
        fs::write(dir.join("Plain_Data/globalgamemanagers"), "x").unwrap();
        let r = mine();
        assert_eq!(r["verdict"], "needs_loader");
        assert_eq!(r["kind"], "bepinex6");
        // and with a loader in place it is ready
        fs::write(dir.join("winhttp.dll"), "W").unwrap();
        fs::create_dir_all(dir.join("BepInEx/core")).unwrap();
        fs::write(dir.join("BepInEx/core/BepInEx.dll"), "B").unwrap();
        assert_eq!(mine()["verdict"], "ready");
    }

    #[test]
    fn a_mod_loader_can_be_swapped_for_another_one() {
        if fsutil::find_in_path("bsdtar").is_none() {
            eprintln!("skipping: bsdtar missing");
            return;
        }
        let h = harness("switch");
        let dir = add_plain_game(&h);
        let mut pe = vec![0u8; 128];
        pe[0] = b'M';
        pe[1] = b'Z';
        pe[60] = 64;
        pe[64..68].copy_from_slice(b"PE\0\0");
        pe[68..70].copy_from_slice(&0x8664u16.to_le_bytes());
        fs::write(dir.join("UnityPlayer.dll"), pe).unwrap();
        fs::write(dir.join("GameAssembly.dll"), "x").unwrap();
        fs::create_dir_all(dir.join("Plain_Data")).unwrap();
        fs::write(dir.join("Plain_Data/globalgamemanagers"), "x").unwrap();
        let melon = loaders::fake::start(zip_of(&h, "melon", &[("version.dll", "V"), ("MelonLoader/net6/MelonLoader.dll", "M")]));
        h.state.write().unwrap().loader_sources = melon.src.clone();
        api(&h, "/api/loader_install", json!({ "kind": "melonloader" }));
        wait_until("MelonLoader is in", || dir.join("version.dll").is_file() && dir.join("MelonLoader/net6/MelonLoader.dll").is_file());
        // the files are in a moment before the install is done with the library; a switch before that is refused
        let key = library_of(&selected_game(&h.state).unwrap()).unwrap().root().to_string_lossy().to_string();
        wait_until("the install is over", || !LOADER_BUSY.lock().unwrap().contains(&key));
        // the first run generates files of its own and the user's mod folder appears
        fs::create_dir_all(dir.join("MelonLoader/Dependencies")).unwrap();
        fs::write(dir.join("MelonLoader/Dependencies/generated.dll"), "G").unwrap();
        fs::create_dir_all(dir.join("Mods")).unwrap();
        fs::write(dir.join("Mods/UserMod.dll"), "U").unwrap();
        let state = || api(&h, "/api/state", json!({}));
        assert_eq!(state()["unity"]["loader"], "melonloader");

        let bep = loaders::fake::start(zip_of(&h, "bep", &[("winhttp.dll", "W"), ("doorstop_config.ini", "I"), ("BepInEx/core/BepInEx.dll", "B")]));
        h.state.write().unwrap().loader_sources = bep.src.clone();
        api(&h, "/api/loader_switch", json!({ "kind": "bepinex6" }));
        wait_until("BepInEx is in", || dir.join("BepInEx/core/BepInEx.dll").is_file() && dir.join("winhttp.dll").is_file());
        assert!(!dir.join("version.dll").exists() && !dir.join("MelonLoader").exists(), "the old loader and its generated folder are gone");
        assert_eq!(fs::read_to_string(dir.join("Mods/UserMod.dll")).unwrap(), "U", "the user's own files are never touched");
        // two quick clicks on the same switch must not put the loader into the library twice
        api(&h, "/api/loader_switch", json!({ "kind": "bepinex6" }));
        api(&h, "/api/loader_switch", json!({ "kind": "bepinex6" }));
        api(&h, "/api/loader_install", json!({ "kind": "bepinex6" }));
        thread::sleep(Duration::from_millis(700));
        // the state follows the files a moment later
        wait_until("the state shows only BepInEx", || {
            let s = state();
            s["library"]["mods"].as_array().map(|m| m.iter().map(|x| x["name"].as_str().unwrap_or("").to_string()).collect::<Vec<_>>()) == Some(vec!["BepInEx 6".to_string()]) && s["unity"]["loader"] == "bepinex"
        });
    }


    const FOMOD_XML: &str = r#"<config>
      <moduleName>Style Pack</moduleName>
      <requiredInstallFiles><file source="core\core.esp" destination="Data\core.esp"/></requiredInstallFiles>
      <installSteps order="Explicit"><installStep name="Look"><optionalFileGroups><group name="Style" type="SelectExactlyOne"><plugins order="Explicit">
        <plugin name="Dark"><description>Dark</description><image path="fomod\dark.png"/><files><file source="styles\dark.esp" destination="Data\style.esp"/></files><typeDescriptor><type name="Recommended"/></typeDescriptor></plugin>
        <plugin name="Light"><description>Light</description><files><file source="styles\light.esp" destination="Data\style.esp"/></files><typeDescriptor><type name="Optional"/></typeDescriptor></plugin>
      </plugins></group></optionalFileGroups></installStep></installSteps>
    </config>"#;

    #[test]
    fn a_mod_with_an_installer_script_asks_its_questions_and_installs_the_answers() {
        if fsutil::find_in_path("bsdtar").is_none() {
            eprintln!("skipping: bsdtar missing");
            return;
        }
        let h = harness("fomod");
        let dir = add_plain_game(&h);
        let st = || api(&h, "/api/state", json!({}));
        let pending = || st()["pending_fomod"].clone();
        let mods = || st()["library"]["mods"].as_array().cloned().unwrap_or_default();
        let archive = |tag: &str| {
            let p = h._dir.join(&format!("{tag}-fomod.zip"));
            fs::write(&p, zip_of(&h, tag, &[("Style Pack 1.0/fomod/ModuleConfig.xml", FOMOD_XML), ("Style Pack 1.0/fomod/dark.png", "PNGDATA"), ("Style Pack 1.0/core/core.esp", "CORE"), ("Style Pack 1.0/styles/dark.esp", "DARK"), ("Style Pack 1.0/styles/light.esp", "LIGHT")])).unwrap();
            p
        };

        install_to_library_from_path(archive("a"), false, Arc::clone(&h.state));
        let p = pending();
        assert_eq!(p["name"], "Style Pack", "{p}");
        assert!(mods().is_empty(), "nothing is added before the answers");
        assert!(!dir.join("Data/core.esp").exists());

        // what the user is shown, with the script's own default (Dark) chosen
        let (code, text) = http(&h, "GET", "/api/fomod_view?sel=%7B%7D", &[("X-Rig-Token", &h.token)], b"");
        assert_eq!(code, 200);
        let v: Value = serde_json::from_str(&text).unwrap();
        assert_eq!((v["complete"].as_bool(), v["files"].as_u64()), (Some(true), Some(2)), "{v}");
        assert_eq!(v["steps"][0]["groups"][0]["plugins"][0]["selected"], true);
        assert_eq!(v["steps"][0]["groups"][0]["plugins"][0]["name"], "Dark");
        // the answers change the view and the number of files
        let (_, text) = http(&h, "GET", &format!("/api/fomod_view?sel={}", fsutil::url_encode(r#"{"0:0":[1]}"#)), &[("X-Rig-Token", &h.token)], b"");
        let v2: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(v2["steps"][0]["groups"][0]["plugins"][1]["selected"], true);
        // a picture of the installer is served from the archive, and only from it
        let (code, _) = http(&h, "GET", &format!("/api/fomod_image?p={}", fsutil::url_encode("fomod\\dark.png")), &[("X-Rig-Token", &h.token)], b"");
        assert_eq!(code, 200);
        for bad in ["../../etc/passwd", "fomod/ModuleConfig.xml", "core/core.esp"] {
            assert_eq!(http(&h, "GET", &format!("/api/fomod_image?p={}", fsutil::url_encode(bad)), &[("X-Rig-Token", &h.token)], b"").0, 404, "{bad}");
        }

        api(&h, "/api/fomod_finish", json!({ "selection": { "0:0": [1] } }));
        wait_until("the mod is in", || mods().len() == 1 && fs::read_to_string(dir.join("Data/style.esp")).map(|t| t == "LIGHT").unwrap_or(false));
        assert_eq!(fs::read_to_string(dir.join("Data/core.esp")).unwrap(), "CORE");
        assert!(pending().is_null());
        assert_eq!(mods()[0]["name"], "Style Pack");
        let lib = library_of(&selected_game(&h.state).unwrap()).unwrap();
        let kept = lib.mods()[0].fomod.clone().unwrap();
        assert_eq!(kept["selection"]["0:0"], json!([1]), "the answers are kept with the mod");

        // an update of that mod reuses the answers: no questions
        let id = lib.mods()[0].id.clone();
        let g = selected_game(&h.state).unwrap();
        let opts = InstallOpts { game: Some(g), replaces: Some(id), ..Default::default() };
        install_routed(archive("b"), false, Arc::clone(&h.state), opts);
        wait_until("updated without asking", || lib.mods().len() == 1 && lib.mods()[0].fomod.is_some() && pending().is_null());
        assert_eq!(fs::read_to_string(dir.join("Data/style.esp")).unwrap(), "LIGHT");

        // cancelling adds nothing
        let before = mods().len();
        install_to_library_from_path(archive("c"), false, Arc::clone(&h.state));
        assert!(!pending().is_null());
        api(&h, "/api/fomod_cancel", json!({}));
        wait_until("cancelled", || pending().is_null());
        assert_eq!(mods().len(), before);
    }


    #[test]
    fn an_installer_without_questions_installs_at_once_and_data_relative_paths_land_in_data() {
        if fsutil::find_in_path("bsdtar").is_none() {
            eprintln!("skipping: bsdtar missing");
            return;
        }
        let h = harness("fomod_data");
        let dir = add_plain_game(&h);
        let xml = r#"<config><moduleName>Plain Installer</moduleName><requiredInstallFiles><file source="brandnew.esp" destination="brandnew.esp"/><folder source="tex" destination="textures"/></requiredInstallFiles></config>"#;
        let p = h._dir.join("plain-fomod.zip");
        fs::write(&p, zip_of(&h, "plainf", &[("fomod/ModuleConfig.xml", xml), ("brandnew.esp", "E"), ("tex/new.dds", "T"), ("tex/more/x.dds", "X")])).unwrap();
        install_to_library_from_path(p, false, Arc::clone(&h.state));
        wait_until("installed without questions", || dir.join("Data/textures/new.dds").is_file() && dir.join("Data/brandnew.esp").is_file());
        assert!(dir.join("Data/textures/more/x.dds").is_file());
        assert!(h.state.read().unwrap().pending_fomod.is_empty());
        let m = library_of(&selected_game(&h.state).unwrap()).unwrap().mods();
        assert_eq!((m[0].name.as_str(), m[0].target.as_str()), ("Plain Installer", "Data"));
    }

    #[test]
    fn a_mod_whose_place_had_to_be_guessed_waits_for_the_users_ok() {
        if fsutil::find_in_path("bsdtar").is_none() {
            eprintln!("skipping: bsdtar missing");
            return;
        }
        let h = harness("preview");
        let dir = add_plain_game(&h);
        fs::create_dir_all(dir.join("VEHICLES")).unwrap();
        for n in ["VEH_1_HI.BNDL", "VEH_1_LO.BNDL"] {
            fs::write(dir.join("VEHICLES").join(n), "ORIGINAL").unwrap();
        }
        let pending = || api(&h, "/api/state", json!({}))["pending_install"].clone();
        let mods = || api(&h, "/api/state", json!({}))["library"]["mods"].as_array().map(|m| m.len()).unwrap_or(0);
        let file = |tag: &str, files: &[(&str, &str)]| {
            let p = h._dir.join(&format!("{tag}-mod.zip"));
            fs::write(&p, zip_of(&h, tag, files)).unwrap();
            p
        };
        let zip = |tag: &str| file(tag, &[("Replace/VEH_1_HI.BNDL", "NEW_HI"), ("Replace/VEH_1_LO.BNDL", "NEW_LO"), ("Readme.txt", "hi")]);

        install_to_library_from_path(zip("a"), false, Arc::clone(&h.state));
        let p = pending();
        assert_eq!((p["placed_by"].as_str(), p["files"].as_u64(), p["folders"][0][0].as_str()), (Some("names"), Some(2), Some("VEHICLES")), "{p}");
        assert_eq!(mods(), 0, "nothing is added before the answer");
        assert_eq!(fs::read_to_string(dir.join("VEHICLES/VEH_1_HI.BNDL")).unwrap(), "ORIGINAL");
        api(&h, "/api/install_confirm", json!({ "accept": true }));
        wait_until("added where guessed", || mods() == 1 && fs::read_to_string(dir.join("VEHICLES/VEH_1_HI.BNDL")).map(|t| t == "NEW_HI").unwrap_or(false));
        assert!(pending().is_null());

        // saying no leaves everything as it was
        api(&h, "/api/lib_remove", json!({ "id": api(&h, "/api/state", json!({}))["library"]["mods"][0]["id"] }));
        wait_until("removed", || mods() == 0);
        // the answer above is remembered for mods like it; forgotten, the same mod is asked about again
        api(&h, "/api/place_rules_clear", json!({}));
        install_to_library_from_path(zip("b"), false, Arc::clone(&h.state));
        assert!(!pending().is_null());
        api(&h, "/api/install_confirm", json!({ "accept": false }));
        wait_until("answered", || pending().is_null());
        assert_eq!((mods(), fs::read_to_string(dir.join("VEHICLES/VEH_1_HI.BNDL")).unwrap()), (0, "ORIGINAL".to_string()));

        // choosing another folder wins over the guess
        install_to_library_from_path(zip("c"), false, Arc::clone(&h.state));
        api(&h, "/api/install_confirm", json!({ "accept": true, "target": "Elsewhere" }));
        wait_until("added into the chosen folder", || dir.join("Elsewhere/Replace/VEH_1_HI.BNDL").is_file());
        assert_eq!(fs::read_to_string(dir.join("VEHICLES/VEH_1_HI.BNDL")).unwrap(), "ORIGINAL");

        // a mod that fits the game on its own is not questioned, and the preview can be switched off
        install_to_library_from_path(file("d", &[("VEHICLES/VEH_9_HI.BNDL", "X")]), false, Arc::clone(&h.state));
        wait_until("fitting mod added at once", || mods() == 2);
        assert!(pending().is_null());
        h.state.write().unwrap().config.no_install_preview = true;
        install_to_library_from_path(zip("e"), false, Arc::clone(&h.state));
        wait_until("guessed mod added at once", || mods() == 3);
        assert!(pending().is_null());
    }

    #[test]
    fn the_thunderstore_browser_finds_the_games_community_searches_and_installs() {
        if fsutil::find_in_path("bsdtar").is_none() {
            eprintln!("skipping: bsdtar missing");
            return;
        }
        let h = harness("browse");
        let dir = add_plain_game(&h);
        let f = thunderstore::fake::start();
        {
            let mut st = h.state.write().unwrap();
            st.thunder.base = f.base.clone();
            st.thunder.allow_http = true;
        }
        f.pages.lock().unwrap().insert(
            "/api/experimental/community/".into(),
            r#"{"pagination":{"next_link":null},"results":[{"identifier":"other-game","name":"Other Game"},{"identifier":"plain-game","name":"Plain Game"}]}"#.into(),
        );
        f.pages.lock().unwrap().insert(
            "/c/plain-game/api/v1/package/".into(),
            r#"[{"name":"Cool_Mod","owner":"Own","is_deprecated":false,"has_nsfw_content":false,"rating_score":3,"versions":[{"version_number":"1.0.0","description":"Makes things cool","icon":"https://ccdn.thunderstore.io/x.png","downloads":42}]},
                {"name":"Other","owner":"Zed","is_deprecated":false,"has_nsfw_content":false,"versions":[{"version_number":"2.0.0","description":"Something else","downloads":7}]}]"#.into(),
        );
        f.packages.lock().unwrap().insert("Own-Cool_Mod".into(), vec![("1.0.0".into(), vec![])]);
        f.archives.lock().unwrap().insert("Own-Cool_Mod".into(), zip_of(&h, "cool", &[("manifest.json", "{}"), ("CoolMod.dll", "v1")]));
        let search = |q: &str| -> Value {
            let (code, body) = http(&h, "GET", &format!("/api/thunder_search?q={q}"), &[("X-Rig-Token", &h.token)], b"");
            assert_eq!(code, 200);
            serde_json::from_str(&body).unwrap()
        };
        let r = search("cool");
        assert_eq!((r["community"].as_str(), r["total"].as_u64(), r["results"].as_array().unwrap().len()), (Some("plain-game"), Some(2), 1), "{r}");
        assert_eq!((r["results"][0]["full_name"].as_str(), r["results"][0]["installed"].as_bool()), (Some("Own-Cool_Mod"), Some(false)));
        // an empty search lists the most downloaded ones
        assert_eq!(search("")["results"][0]["full_name"], "Own-Cool_Mod");
        // the list is fetched once and then kept
        let fetches = |f: &thunderstore::fake::FakeThunder| f.requests.lock().unwrap().iter().filter(|u| u.contains("/api/v1/package/")).count();
        assert_eq!(fetches(&f), 1);

        api(&h, "/api/thunder_install", json!({ "input": "Own-Cool_Mod" }));
        wait_until("installed from the browser", || dir.join("BepInEx/plugins/CoolMod.dll").is_file() || dir.join("CoolMod.dll").is_file());
        assert_eq!(search("cool")["results"][0]["installed"], true);

        // a game whose name matches no community asks for one, and remembers the answer
        {
            let mut st = h.state.write().unwrap();
            let i = st.selected_idx;
            st.games[i].name = "Unmatched Title".into();
        }
        let r = search("x");
        assert_eq!(r["need_community"], true, "{r}");
        // what is known about the game beats guessing from its name
        let appid = selected_game(&h.state).unwrap().appid;
        h.state.write().unwrap().recipes = vec![recipes::Recipe { appid: appid.clone(), loader: "bepinex5".into(), thunderstore: "plain-game".into(), ..Default::default() }];
        assert_eq!(search("cool")["community"], "plain-game");
        assert_eq!(api(&h, "/api/state", json!({}))["unity"], Value::Null, "not a Unity game");
        h.state.write().unwrap().recipes.clear();
        api(&h, "/api/thunder_community", json!({ "name": "plain-game" }));
        assert_eq!(search("cool")["community"], "plain-game");
        api(&h, "/api/thunder_community", json!({ "name": "../evil" }));
        assert_eq!(h.state.read().unwrap().config.thunder_community.values().next().map(String::as_str), Some("plain-game"), "bad names are refused");
    }

    #[test]
    fn a_shared_setup_recreates_the_mod_list_on_another_machine() {
        if fsutil::find_in_path("bsdtar").is_none() {
            eprintln!("skipping: bsdtar missing");
            return;
        }
        let h = harness("import");
        let dir = add_plain_game(&h);
        let f = thunderstore::fake::start();
        {
            let mut st = h.state.write().unwrap();
            st.thunder.base = f.base.clone();
            st.thunder.allow_http = true;
        }
        {
            let mut p = f.packages.lock().unwrap();
            p.insert("bbepis-BepInExPack".into(), vec![("5.4.2100".into(), vec![])]);
            p.insert("Own-Cool_Mod".into(), vec![("1.0.0".into(), vec!["bbepis-BepInExPack-5.4.2100".into()])]);
        }
        {
            let mut a = f.archives.lock().unwrap();
            a.insert("bbepis-BepInExPack".into(), zip_of(&h, "bep", &[("BepInExPack/winhttp.dll", "W"), ("BepInExPack/BepInEx/core/BepInEx.dll", "B")]));
            a.insert("Own-Cool_Mod".into(), zip_of(&h, "cool", &[("manifest.json", "{}"), ("CoolMod.dll", "v1")]));
        }
        let g = selected_game(&h.state).unwrap();
        let spec: deploy::ProfileSpec = serde_json::from_value(json!({
            "gantry_profile": 1, "game": "Plain Game",
            "mods": [
                { "name": "BepInExPack", "version": "5.4.2100", "enabled": true, "thunderstore": { "namespace": "bbepis", "name": "BepInExPack", "version": "5.4.2100" } },
                { "name": "Cool Mod", "version": "1.0.0", "enabled": false, "thunderstore": { "namespace": "Own", "name": "Cool_Mod", "version": "1.0.0" } },
                { "name": "HD Textures", "version": "2.0", "enabled": true, "source": "hd.7z", "nexus": { "game": "coolgame", "mod_id": 7, "file_id": 70 } }
            ]
        }))
        .unwrap();
        let msg = import_spec(&h.state, &g, spec.clone()).unwrap();
        assert!(msg.contains("2 fetched, 1 still to get yourself"), "{msg}");
        let lib = library_of(&g).unwrap();
        let mods = lib.mods();
        assert_eq!(mods.iter().map(|m| (m.name.as_str(), m.enabled)).collect::<Vec<_>>(), vec![("BepInExPack", true), ("Cool Mod", false)], "order and on/off follow the setup");
        assert!(dir.join("winhttp.dll").is_file() && !dir.join("BepInEx/plugins/CoolMod.dll").exists(), "the switched-off mod is not deployed");
        assert!(h.state.read().unwrap().logs.iter().any(|l| l.message.contains("HD Textures") && l.message.contains("nexusmods.com/coolgame/mods/7")));
        // exporting what we have and importing again changes nothing
        let again = lib.export_spec("Plain Game");
        assert_eq!(again.mods.len(), 2);
        assert!(lib.apply_spec(&again).unwrap().is_empty());
        // nonsense is refused
        assert!(lib.apply_spec(&deploy::ProfileSpec { gantry_profile: 9, game: String::new(), mods: vec![], name: String::new(), note: String::new() }).is_err());
    }

    #[test]
    fn mods_that_need_other_mods_say_so_until_those_are_installed() {
        if fsutil::find_in_path("bsdtar").is_none() {
            eprintln!("skipping: bsdtar missing");
            return;
        }
        let (h, f, _dir, id) = nexus_update_setup("nxreq", false);
        *f.requirements_json.lock().unwrap() = r#"[{"modId":"77","modName":"Base Lib","gameId":"1","url":"","notes":"Install first","externalRequirement":false},{"modId":"0","modName":"Some Runtime","gameId":"0","url":"https://example.com/rt","notes":"","externalRequirement":true}]"#.into();
        api(&h, "/api/nexus_check", json!({}));
        wait_until("check finished", || nexus_state(&h)["checked_ms"].as_u64().unwrap_or(0) > 0);
        let n = nexus_state(&h);
        assert_eq!(n["needs"].as_array().unwrap().len(), 1, "{n}");
        let need = &n["needs"][0];
        assert_eq!((need["lib_id"].as_str(), need["missing"].as_array().unwrap().len()), (Some(id.as_str()), 1), "outside-Nexus requirements cannot be checked, so they are not listed");
        assert_eq!(need["missing"][0]["name"], "Base Lib");
        assert_eq!(need["missing"][0]["page_url"], "https://www.nexusmods.com/coolgame/mods/77");

        // installing the mod that was missing clears the warning
        api(&h, "/api/nxm", json!({ "url": "nxm://coolgame/mods/77/files/8?key=K9&expires=1999999999" }));
        api(&h, "/api/nxm_confirm", json!({ "accept": true }));
        wait_until("installed", || api(&h, "/api/state", json!({}))["library"]["mods"].as_array().map(|m| m.len()) == Some(3));
        wait_until("the need is gone", || nexus_state(&h)["needs"].as_array().unwrap().is_empty());
    }

    #[test]
    fn the_nexus_page_is_kept_as_a_note_and_its_install_instructions_are_followed() {
        if fsutil::find_in_path("bsdtar").is_none() {
            eprintln!("skipping: bsdtar missing");
            return;
        }
        let h = harness("nxpage");
        let zip = zip_of(&h, "pg", &[("tool.esp", "T")]);
        let f = nexus::fake::start("Tool-88-1-0-1700000000.zip", zip, false);
        *f.mod_json.lock().unwrap() = r#"{"name":"Tool","summary":"Does things","description":"<p>Installation:<br>Place the plugin into the <b>Data</b> folder.</p>","version":"1.0"}"#.into();
        *f.files_json.lock().unwrap() = r#"{"files":[{"file_id":9,"name":"Tool","file_name":"Tool-88-1-0-1700000000.zip","version":"1.0","category_name":"MAIN","uploaded_timestamp":5,"description":"Main file","changelog_html":"<ul><li>Fixed a crash</li></ul>"}],"file_updates":[]}"#.into();
        {
            let mut st = h.state.write().unwrap();
            st.nexus.base = f.base.clone();
            st.nexus.allow_http = true;
        }
        let dir = add_plain_game(&h);
        fs::create_dir_all(dir.join("Data")).unwrap();
        connect_nexus(&h);
        api(&h, "/api/nxm", json!({ "url": "nxm://coolgame/mods/88/files/9" }));
        api(&h, "/api/nxm_confirm", json!({ "accept": true }));
        wait_until("installed", || api(&h, "/api/state", json!({}))["library"]["mods"].as_array().map(|m| m.len()) == Some(1));
        let m = api(&h, "/api/state", json!({}))["library"]["mods"][0].clone();
        assert_eq!((m["target"].as_str(), m["notes"].as_u64()), (Some("Data"), Some(1)));
        wait_until("deployed into the folder the page named", || dir.join("Data/tool.esp").is_file());
        let mf = nexus::Nexus::new(&f.base, nexus::fake::KEY, true).unwrap().mod_files("coolgame", 88).unwrap();
        assert_eq!((mf.files[0].description.as_str(), readme::plain(&mf.files[0].changelog).trim()), ("Main file", "Fixed a crash"));
    }

    #[cfg(unix)]
    fn with_fake_steam(tag: &str) -> (Harness, steamlib::fake::FakeSteam) {
        let h = harness(tag);
        let f = steamlib::fake::start();
        let root = h._dir.join("steamroot");
        fs::create_dir_all(root.join("config")).unwrap();
        fs::write(
            root.join("config/loginusers.vdf"),
            "\"users\"\n{\n\t\"76561198000000001\"\n\t{\n\t\t\"AccountName\"\t\t\"account_one\"\n\t\t\"PersonaName\"\t\t\"Alpha\"\n\t\t\"timestamp\"\t\t\"200\"\n\t}\n\t\"76561198000000002\"\n\t{\n\t\t\"AccountName\"\t\t\"account_two\"\n\t\t\"PersonaName\"\t\t\"Beta\"\n\t\t\"timestamp\"\t\t\"100\"\n\t}\n}\n",
        )
        .unwrap();
        {
            let mut st = h.state.write().unwrap();
            st.steam_lib.base = f.base.clone();
            st.steam_lib.allow_http = true;
            st.steam_lib.roots = Some(vec![root]);
        }
        (h, f)
    }

    #[cfg(unix)]
    fn steam_lib(h: &Harness) -> Value {
        api(h, "/api/state", json!({}))["steam_lib"].clone()
    }

    #[cfg(unix)]
    #[test]
    fn the_steam_library_is_read_with_a_key_kept_private_and_restored_after_a_restart() {
        use std::os::unix::fs::PermissionsExt;
        let (h, f) = with_fake_steam("steamlib");
        let n = steam_lib(&h);
        assert_eq!((n["configured"].as_bool(), n["active"].as_str(), n["accounts"].as_array().map(|a| a.len())), (Some(false), Some("76561198000000001"), Some(2)), "newest login is the default account");
        assert_eq!(n["accounts"][0]["persona"], "Alpha");

        api(&h, "/api/steam_key_set", json!({ "key": "not a key" }));
        wait_until("shape rejected", || last_log(&h).contains("does not look like a Steam Web API key"));
        api(&h, "/api/steam_key_set", json!({ "key": "ffffffffffffffffffffffffffffffff" }));
        wait_until("steam rejected", || last_log(&h).contains("rejected the Web API key"));
        let keyfile = h._dir.join("cfg/steam_web_api_key");
        assert!(!keyfile.exists() && steam_lib(&h)["configured"] == false, "a key that does not work is never stored");

        api(&h, "/api/steam_key_set", json!({ "key": steamlib::fake::KEY }));
        wait_until("connected", || steam_lib(&h)["configured"] == true);
        let n = steam_lib(&h);
        assert_eq!((n["owned_count"].as_u64(), n["owned_for"].as_str()), (Some(3), Some("76561198000000001")));
        assert!(last_log(&h).contains("Steam library connected: 3 games found for Alpha."), "{}", last_log(&h));
        assert_eq!(fs::read_to_string(&keyfile).unwrap(), steamlib::fake::KEY);
        assert_eq!(fs::metadata(&keyfile).unwrap().permissions().mode() & 0o777, 0o600);
        assert!(!api(&h, "/api/state", json!({})).to_string().contains(steamlib::fake::KEY), "the key never reaches the page");
        let (code, body) = http(&h, "GET", "/api/steam_owned", &[("X-Rig-Token", &h.token)], b"");
        assert_eq!(code, 200);
        let list: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(list.as_array().unwrap().iter().map(|o| o["name"].as_str().unwrap()).collect::<Vec<_>>(), vec!["Counter-Strike 2", "Portal 2", "Subnautica 2"]);
        assert_eq!(list[2]["minutes"], 600);

        // a restart needs no network: key and list come back from disk
        {
            let mut st = h.state.write().unwrap();
            st.steam_lib.key = None;
            st.steam_lib.owned.clear();
            st.steam_lib.base = "http://127.0.0.1:1".into();
        }
        load_steam_lib(&h.state);
        let n = steam_lib(&h);
        assert_eq!((n["configured"].as_bool(), n["owned_count"].as_u64()), (Some(true), Some(3)));
        h.state.write().unwrap().steam_lib.base = f.base.clone();

        // a refresh picks up a new purchase
        *f.games_json.lock().unwrap() = r#"{"response":{"games":[{"appid":1,"name":"Alpha Game","playtime_forever":5},{"appid":2,"name":"Beta Game"}]}}"#.into();
        api(&h, "/api/steam_refresh", json!({}));
        wait_until("refreshed", || steam_lib(&h)["owned_count"] == 2);
        assert!(last_log(&h).contains("Steam library updated: 2 games."));

        // the other account is another library: the fake only knows the first one
        api(&h, "/api/steam_account", json!({ "steamid": "76561198000000002" }));
        wait_until("other account answered", || steam_lib(&h)["error"].as_str().map(|e| e.contains("rejected")).unwrap_or(false));
        assert_eq!(steam_lib(&h)["active"], "76561198000000002");
        assert_eq!(steam_lib(&h)["owned_count"], 2, "a failed refresh keeps the last good list");
        api(&h, "/api/steam_account", json!({ "steamid": "" }));
        wait_until("back to the newest account", || steam_lib(&h)["error"].is_null() && steam_lib(&h)["active"] == "76561198000000001");

        // harmless inputs
        api(&h, "/api/steam_install", json!({ "appid": "620; rm -rf /" }));
        api(&h, "/api/steam_install", json!({ "appid": "620" }));
        api(&h, "/api/open_link", json!({ "which": "something else" }));

        api(&h, "/api/steam_key_clear", json!({}));
        let n = steam_lib(&h);
        assert_eq!((n["configured"].as_bool(), n["owned_count"].as_u64()), (Some(false), Some(0)));
        assert!(!keyfile.exists() && !h._dir.join("cache/steam_owned.json").exists());
    }

    #[test]
    fn the_interface_language_is_chosen_stored_and_inlined_into_the_page() {
        let h = harness("lang");
        let state = || api(&h, "/api/state", json!({}));
        assert_eq!(state()["language"], "en", "English unless the user chooses otherwise");
        let (_, page) = http(&h, "GET", "/", &[], b"");
        assert!(page.contains("let LANG = 'en';") && page.contains("let DICT = {};"), "English needs no dictionary");

        assert_eq!(http(&h, "GET", "/api/i18n?lang=ro", &[("X-Rig-Token", &h.token)], b"").0, 200);
        let (code, body) = http(&h, "GET", "/api/i18n?lang=ru", &[("X-Rig-Token", &h.token)], b"");
        assert_eq!(code, 200);
        assert_eq!(serde_json::from_str::<Value>(&body).unwrap()["Remove"], "Удалить");
        assert_eq!(http(&h, "GET", "/api/i18n?lang=xx", &[("X-Rig-Token", &h.token)], b"").0, 404);
        assert_eq!(http(&h, "GET", "/api/i18n", &[("X-Rig-Token", &h.token)], b"").0, 404);
        assert_eq!(http(&h, "GET", "/api/i18n?lang=ro", &[], b"").0, 403, "the dictionary is behind the token like everything else");

        api(&h, "/api/set_language", json!({ "lang": "xx" }));
        assert_eq!(state()["language"], "en", "unknown languages are ignored");
        api(&h, "/api/set_language", json!({ "lang": "it" }));
        assert_eq!(state()["language"], "it");
        let saved = fs::read_to_string(h._dir.join("cfg/cfg.json")).or_else(|_| fs::read_to_string(h._dir.join("cfg/rig_config.json"))).unwrap();
        assert!(saved.contains("\"language\": \"it\""), "{saved}");
        let (_, page) = http(&h, "GET", "/", &[], b"");
        assert!(page.contains("let LANG = 'it';") && page.contains("Libreria mod"), "the page arrives already translated");
        assert!(!page.contains("__RIG_DICT__") && !page.contains("__RIG_LANG__"));
        assert!(!page.contains("\"</"), "no closing tag may appear inside the inlined dictionary");
    }

    #[cfg(unix)]
    #[test]
    fn the_nexus_key_is_checked_stored_privately_and_never_sent_to_the_page() {
        use std::os::unix::fs::PermissionsExt;
        let (h, _f) = with_fake_nexus("nxkey", false);
        // not even shaped like a key
        api(&h, "/api/nexus_key_set", json!({ "key": "pwned\"; header = \"x" }));
        wait_until("shape rejected", || last_log(&h).contains("does not look like"));
        // shaped like a key, but Nexus says no
        api(&h, "/api/nexus_key_set", json!({ "key": "ZZZZZZZZZZZZZZZZZZZZZZZZZZZZ" }));
        wait_until("nexus rejected", || last_log(&h).contains("rejected the API key"));
        assert_eq!(api(&h, "/api/state", json!({}))["nexus"]["configured"], false);
        let keyfile = h._dir.join("cfg/nexus_api_key");
        assert!(!keyfile.exists(), "a bad key is never stored");

        connect_nexus(&h);
        let st = api(&h, "/api/state", json!({}));
        assert_eq!(st["nexus"]["user"], "tester");
        assert_eq!(st["nexus"]["key_hint"], "…tail");
        assert!(!st.to_string().contains(nexus::fake::KEY), "the full key never reaches the UI");
        assert_eq!(fs::read_to_string(&keyfile).unwrap(), nexus::fake::KEY);
        assert_eq!(fs::metadata(&keyfile).unwrap().permissions().mode() & 0o777, 0o600);
        assert!(last_log(&h).contains("connected as tester (free account)"));

        // survives a restart without any network access
        h.state.write().unwrap().nexus.key = None;
        load_nexus_key(&h.state);
        assert_eq!(api(&h, "/api/state", json!({}))["nexus"]["configured"], true);

        api(&h, "/api/nexus_key_clear", json!({}));
        assert!(!keyfile.exists());
        assert_eq!(api(&h, "/api/state", json!({}))["nexus"]["configured"], false);
    }

    #[test]
    fn a_website_download_waits_for_confirmation_then_installs_into_the_selected_game() {
        let (h, f) = with_fake_nexus("nxflow", false);
        let link = "nxm://halfsword/mods/12/files/34?key=OneTimeKey_1&expires=1900000000&user_id=5";
        let downloads = |f: &nexus::fake::FakeNexus| f.seen.lock().unwrap().requests.iter().filter(|r| r.contains("download_link")).count();
        let pak = PathBuf::from(&h.game.game_dir).join("Demo/Content/Paks/~mods/Cool Hat_P.pak");

        // without a key the link is refused and nothing is queued
        api(&h, "/api/nxm", json!({ "url": link }));
        assert!(last_log(&h).contains("no API key"));
        assert!(api(&h, "/api/state", json!({}))["nexus"]["pending"].is_null());

        connect_nexus(&h);
        api(&h, "/api/nxm", json!({ "url": "http://evil.example/mods/1/files/2" }));
        assert!(last_log(&h).contains("Not an nxm://"));
        api(&h, "/api/nxm", json!({ "url": link }));
        let st = api(&h, "/api/state", json!({}));
        assert_eq!(st["nexus"]["pending"]["mod_id"], 12);
        assert_eq!(st["nexus"]["pending"]["free_key"], true);
        assert_eq!(st["nexus"]["pending"]["target"], "Demo", "the user sees where it will go");
        thread::sleep(Duration::from_millis(300));
        assert_eq!(downloads(&f), 0, "nothing is fetched before the user agrees");
        assert!(!pak.exists());

        // declining throws the request away
        api(&h, "/api/nxm_confirm", json!({ "accept": false }));
        wait_until("declined", || api(&h, "/api/state", json!({}))["nexus"]["pending"].is_null());
        assert_eq!(downloads(&f), 0);
        assert!(!pak.exists());

        // accepting downloads and installs through the normal pipeline
        api(&h, "/api/nxm", json!({ "url": link }));
        api(&h, "/api/nxm_confirm", json!({ "accept": true }));
        wait_until("installed", || pak.is_file());
        assert_eq!(fs::metadata(&pak).unwrap().len(), 4096);
        wait_until("listed", || mod_enabled(&api(&h, "/api/state", json!({})), "PAK:Cool Hat_P") == Some(true));
        assert!(api(&h, "/api/state", json!({}))["nexus"]["pending"].is_null());
        assert_eq!(downloads(&f), 1);
        let dl = f.seen.lock().unwrap().requests.iter().find(|r| r.contains("download_link")).cloned().unwrap();
        assert!(dl.contains("key=OneTimeKey_1&expires=1900000000"), "{dl}");
        let drops = h.state.read().unwrap().cache_root().join("drops");
        wait_until("temp download removed", || fs::read_dir(&drops).map(|mut r| r.next().is_none()).unwrap_or(true));

        // confirming when nothing is pending is harmless
        api(&h, "/api/nxm_confirm", json!({ "accept": true }));
        wait_until("nothing waiting", || last_log(&h).contains("No Nexus download is waiting"));
    }

    #[test]
    fn free_accounts_get_a_clear_message_when_the_link_carries_no_key() {
        let (h, _f) = with_fake_nexus("nxfree", true);
        connect_nexus(&h);
        api(&h, "/api/nxm", json!({ "url": "nxm://halfsword/mods/1/files/2" }));
        api(&h, "/api/nxm_confirm", json!({ "accept": true }));
        wait_until("explained", || last_log(&h).contains("Mod Manager Download"));
        assert!(!PathBuf::from(&h.game.game_dir).join("Demo/Content/Paks/~mods/Cool Hat_P.pak").exists());
    }

    #[test]
    fn a_second_gantry_hands_its_nexus_link_to_the_running_one() {
        let (h, _f) = with_fake_nexus("nxfwd", false);
        connect_nexus(&h);
        assert!(forward_nxm(h.addr.port(), "nxm://halfsword/mods/7/files/8?key=K&expires=1900000000"));
        let st = api(&h, "/api/state", json!({}));
        assert_eq!(st["nexus"]["pending"]["file_id"], 8);
        assert!(!forward_nxm(1, "nxm://halfsword/mods/7/files/8"), "no running instance: reports failure");
    }

    #[test]
    fn a_reply_in_chunks_is_put_back_together() {
        let raw = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n9\r\nRIG_TOKEN\r\n6\r\n = \"ab\r\n3;x=1\r\ncd\"\r\n0\r\n\r\n";
        assert_eq!(http_reply(raw), Some((200, "RIG_TOKEN = \"abcd\"".to_string())));
        assert_eq!(http_reply(b"HTTP/1.1 404 Not Found\r\nContent-Length: 2\r\n\r\nno"), Some((404, "no".to_string())));
        assert_eq!(http_reply(b"garbage"), None);
    }

    #[test]
    fn conflicts_between_enabled_pak_mods_are_reported_over_http() {
        let h = harness("conflicts");
        let paks = PathBuf::from(&h.game.game_dir).join("Demo/Content/Paks/~mods");
        fs::write(paks.join("Skin_P.pak"), pak::builder::v11("../../../Demo/", &["Content/Hero.uasset", "Content/Hero.uexp"], false)).unwrap();
        fs::write(paks.join("Hat.pak"), pak::builder::v11("../../../Demo/", &["Content/Hero.uasset", "Content/Hat.uasset"], false)).unwrap();
        fs::write(paks.join("Broken.pak"), vec![1u8; 200]).unwrap();
        let get = |h: &Harness| {
            let (c, b) = http(h, "GET", "/api/conflicts", &[("X-Rig-Token", &h.token)], b"");
            assert_eq!(c, 200);
            serde_json::from_str::<Value>(&b).unwrap()
        };
        let r = get(&h);
        assert_eq!(r["scanned"], 2);
        assert_eq!(r["conflicts"].as_array().unwrap().len(), 1, "{r}");
        assert_eq!(r["conflicts"][0]["shared"], 1);
        assert_eq!(r["conflicts"][0]["likely_winner"], "Skin_P");
        assert!(r["unreadable"][0].as_str().unwrap().starts_with("Broken"));
        // disabled mods are not part of the comparison
        refresh_mods_shared(&h.state);
        api(&h, "/api/toggle_mod", json!({ "id": "PAK:Hat", "enable": false }));
        assert!(get(&h)["conflicts"].as_array().unwrap().is_empty());
        assert_eq!(http(&h, "GET", "/api/conflicts", &[], b"").0, 403, "token required like every API call");
    }

    #[test]
    #[cfg(not(windows))] // the saves of this test live in a Wine prefix
    fn a_session_leaves_a_save_snapshot_only_when_the_saves_changed() {
        let h = harness("sessionsnap");
        let slot = PathBuf::from(&h.game.pfx).join("drive_c/users/steamuser/AppData/Local/Demo/Saved/SaveGames/slot1.sav");
        let g = selected_game(&h.state).unwrap();
        let count = || {
            let (_, b) = http(&h, "GET", "/api/saves_list", &[("X-Rig-Token", &h.token)], b"");
            serde_json::from_str::<Value>(&b).unwrap()["snapshots"].as_array().unwrap().clone()
        };
        snapshot_after_session(&h.state, &g);
        assert!(count().is_empty());
        fs::write(&slot, "LEVEL 9").unwrap();
        snapshot_after_session(&h.state, &g);
        snapshot_after_session(&h.state, &g);
        let all = count();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0]["label"], "session");
        assert!(last_log(&h).contains("Saves backed up after the session"));
    }

    #[test]
    fn the_slow_screen_compares_modded_and_plain_sessions_and_names_heavy_mods() {
        let h = harness("perfscreen");
        let g = selected_game(&h.state).unwrap();
        let path = read_state(&h.state).paths.config_file.with_file_name("sessions.jsonl");
        let sess = |mods: usize, end: u64, fps: u32, mb: u64| stats::Session {
            appid: g.appid.clone(), name: g.name.clone(), start_ms: end - 600_000, end_ms: end, secs: 600, crashed: false, mods,
            perf: Some(perf::Summary { peak_mb: mb, avg_cpu: 90, peak_cpu: 150, frames: (fps > 0).then(|| perf::FrameStats { frames: 100, avg_fps: fps, low1_fps: fps / 4, spikes: 7, ..Default::default() }) }),
            ..Default::default()
        };
        let get = || {
            let (c, b) = http(&h, "GET", "/api/perf", &[("X-Rig-Token", &h.token)], b"");
            assert_eq!(c, 200);
            serde_json::from_str::<Value>(&b).unwrap()
        };
        assert_eq!(http(&h, "GET", "/api/perf", &[], b"").0, 403);
        assert_eq!(get()["sessions"], 0);
        // a session without measurements and one of another game do not count
        stats::record(&path, &stats::Session { perf: None, ..sess(2, 5_000_000, 0, 0) });
        stats::record(&path, &stats::Session { appid: "other".into(), ..sess(2, 5_500_000, 30, 1) });
        stats::record(&path, &sess(2, 6_000_000, 50, 6000));
        stats::record(&path, &sess(0, 7_000_000, 90, 4000));
        stats::record(&path, &sess(3, 8_000_000, 40, 8000));
        let v = get();
        assert_eq!((v["sessions"].as_u64(), v["modded"]["sessions"].as_u64(), v["plain"]["sessions"].as_u64()), (Some(3), Some(2), Some(1)));
        assert_eq!((v["modded"]["avg_fps"].as_u64(), v["plain"]["avg_fps"].as_u64(), v["modded"]["peak_mb"].as_u64()), (Some(45), Some(90), Some(7000)));
        assert_eq!(v["last"]["mods"], 3);
        assert_eq!(v["last"]["tips"], json!(["spikes", "dips"]));
        assert!(v["heavy"].as_array().unwrap().is_empty(), "no mods yet");
        assert!(v["manual"].is_null());
    }

    #[test]
    fn virustotal_asks_nothing_until_switched_on_and_sends_only_fingerprints() {
        let h = harness("virustotal");
        let _dir = add_plain_game(&h);
        let zip = zip_of(&h, "tool", &[("Tool Pack-5-1-0-1700000000/Data/new.esp", "NEW"), ("Tool Pack-5-1-0-1700000000/helper.exe", "MZ-this-is-a-program")]);
        assert_eq!(http(&h, "POST", "/api/drop_upload?name=Tool%20Pack-5-1-0-1700000000.zip", &[("X-Rig-Token", &h.token)], &zip).0, 200);
        wait_until("mod added", || library_of_state(&h)["mods"].as_array().map(|m| !m.is_empty()).unwrap_or(false));
        let id = library_of_state(&h)["mods"][0]["id"].as_str().unwrap().to_string();
        let sha = hashing::sha256_hex(&mut &b"MZ-this-is-a-program"[..]).unwrap();
        let fake = virustotal::fake::start();
        fake.known.lock().unwrap().insert(sha.clone(), (3, 60));
        {
            let mut st = write_state(&h.state);
            st.vt.base = fake.base.clone();
            st.vt.allow_http = true;
        }
        let asked = || fake.seen.lock().unwrap().len();
        let page = || {
            let (c, b) = http(&h, "GET", &format!("/api/mod_page?id={id}"), &[("X-Rig-Token", &h.token)], b"");
            assert_eq!(c, 200);
            serde_json::from_str::<Value>(&b).unwrap()
        };

        // off by default: nothing is asked, and the page says so
        assert_eq!(page()["vt"]["on"], false);
        api(&h, "/api/vt_check", json!({ "id": id }));
        wait_until("the refusal", || last_log(&h).contains("switched off"));
        // on, but no key yet
        api(&h, "/api/settings", json!({ "virustotal": true }));
        api(&h, "/api/vt_check", json!({ "id": id }));
        wait_until("the missing key", || last_log(&h).contains("Add your VirusTotal key"));
        // a key that is not one is not saved
        api(&h, "/api/vt_key_set", json!({ "key": "not a key" }));
        assert!(!page()["vt"]["has_key"].as_bool().unwrap());
        assert_eq!(asked(), 0, "nothing has left the machine so far");

        api(&h, "/api/vt_key_set", json!({ "key": virustotal::fake::KEY }));
        let key_file = read_state(&h.state).paths.config_file.with_file_name("virustotal_api_key");
        assert_eq!(fs::read_to_string(&key_file).unwrap(), virustotal::fake::KEY);
        let (_, state_json) = http(&h, "POST", "/api/state", &[("X-Rig-Token", &h.token)], b"{}");
        assert!(!state_json.contains(virustotal::fake::KEY), "the key is never shown to the page");
        let (_, page_json) = http(&h, "GET", &format!("/api/mod_page?id={id}"), &[("X-Rig-Token", &h.token)], b"");
        assert!(!page_json.contains(virustotal::fake::KEY));

        api(&h, "/api/vt_check", json!({ "id": id }));
        wait_until("the answer", || page()["vt"]["report"]["files"][0]["verdict"]["malicious"] == 3);
        let seen = fake.seen.lock().unwrap().clone();
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0].0, format!("/api/v3/files/{sha}"), "only the fingerprint is in the request");
        assert_eq!(page()["vt"]["report"]["files"][0]["file"], "helper.exe");
        wait_until("the warning", || read_state(&h.state).notes.json().to_string().contains("virustotal"));

        // asking again within a few days uses what is known
        api(&h, "/api/vt_check", json!({ "id": id }));
        thread::sleep(Duration::from_millis(500));
        wait_until("the second check to finish", || read_state(&h.state).vt.busy.is_empty());
        assert_eq!(asked(), 1);
        // switched off again: no more questions, even for a file nobody asked about
        api(&h, "/api/settings", json!({ "virustotal": false }));
        fake.known.lock().unwrap().clear();
        api(&h, "/api/vt_check", json!({ "id": id }));
        wait_until("the refusal again", || last_log(&h).contains("switched off"));
        assert_eq!(asked(), 1);
        // removing the key forgets the answers
        api(&h, "/api/vt_key_clear", json!({}));
        assert!(!key_file.exists());
        assert!(page()["vt"]["report"].is_null());
    }

    #[test]
    fn the_conflict_viewer_lists_conflicts_and_lets_a_mod_win_over_http() {
        let h = harness("conflictview");
        let dir = add_plain_game(&h);
        for (name, body) in [("Alpha", "FROM A"), ("Beta", "FROM B")] {
            let zip = zip_of(&h, name, &[(&format!("{name}-1-1-1-1700000000/Data/shared.esp"), body), (&format!("{name}-1-1-1-1700000000/Data/own_{name}.esp"), "x")]);
            let (code, _) = http(&h, "POST", &format!("/api/drop_upload?name={name}-1-1-1-1700000000.zip"), &[("X-Rig-Token", &h.token)], &zip);
            assert_eq!(code, 200);
            wait_until("mod deployed", || library_of_state(&h)["mods"].as_array().map(|m| m.iter().any(|x| x["name"] == name)).unwrap_or(false));
        }
        wait_until("both deployed", || fs::read_to_string(dir.join("Data/shared.esp")).map(|t| t == "FROM B").unwrap_or(false));
        let view = |q: &str| {
            let (c, b) = http(&h, "GET", &format!("/api/conflict_view?q={q}"), &[("X-Rig-Token", &h.token)], b"");
            assert_eq!(c, 200);
            serde_json::from_str::<Value>(&b).unwrap()
        };
        assert_eq!(http(&h, "GET", "/api/conflict_view", &[], b"").0, 403);
        let v = view("");
        assert_eq!((v["total"].as_u64(), v["pairs"][0]["winner"].as_str(), v["pairs"][0]["loser"].as_str()), (Some(1), Some("Beta"), Some("Alpha")));
        assert_eq!(view("nomatch")["files"].as_array().unwrap().len(), 0);

        let (loser, winner) = (v["pairs"][0]["loser_id"].as_str().unwrap().to_string(), v["pairs"][0]["winner_id"].as_str().unwrap().to_string());
        api(&h, "/api/lib_prefer", json!({ "winner": loser, "over": winner }));
        wait_until("Alpha winning", || fs::read_to_string(dir.join("Data/shared.esp")).map(|t| t == "FROM A").unwrap_or(false));
        assert_eq!(view("")["pinned"], 1);
        api(&h, "/api/lib_clear_pins", json!({}));
        wait_until("the load order again", || fs::read_to_string(dir.join("Data/shared.esp")).map(|t| t == "FROM B").unwrap_or(false));
        assert_eq!(view("")["pinned"], 0);
    }

    #[test]
    fn automatic_backups_are_listed_restored_and_can_be_switched_off() {
        let h = harness("autobackup");
        let get = |path: &str| {
            let (c, b) = http(&h, "GET", path, &[("X-Rig-Token", &h.token)], b"");
            assert_eq!(c, 200);
            serde_json::from_str::<Value>(&b).unwrap()
        };
        assert_eq!(http(&h, "GET", "/api/auto_backups", &[], b"").0, 403);
        assert!(get("/api/auto_backups")["backups"].as_array().unwrap().is_empty());
        assert_eq!(api(&h, "/api/state", json!({}))["settings"]["auto_backup"], true);

        // the first backup is due at once, the next one a week later
        let name = run_auto_backup(&h.state).unwrap();
        assert!(autobackup::due(&auto_backup_dir(&read_state(&h.state)), now_ms() + autobackup::INTERVAL_MS));
        assert!(!autobackup::due(&auto_backup_dir(&read_state(&h.state)), now_ms()));
        let listed = get("/api/auto_backups");
        assert_eq!(listed["backups"][0]["name"], name.as_str());
        assert_eq!(listed["keep"], 8);

        // settings changed after the backup come back with it
        api(&h, "/api/settings", json!({ "theme": "midnight" }));
        api(&h, "/api/auto_backup_restore", json!({ "name": name }));
        wait_until("the settings coming back", || api(&h, "/api/state", json!({}))["settings"]["theme"] != "midnight");
        // a name that is not one of ours is refused
        api(&h, "/api/auto_backup_restore", json!({ "name": "../../config/protonrig/rig_config.json" }));
        wait_until("the refusal", || h.state.read().unwrap().logs.iter().any(|l| l.message.contains("not a backup of ModForge")));

        api(&h, "/api/settings", json!({ "auto_backup": false }));
        assert_eq!(api(&h, "/api/state", json!({}))["settings"]["auto_backup"], false);
    }

    #[test]
    #[cfg(not(windows))] // the saves of this test live in a Wine prefix
    fn save_guard_snapshots_before_launch_and_restores_over_http() {
        let h = harness("saveguard");
        let slot = PathBuf::from(&h.game.pfx).join("drive_c/users/steamuser/AppData/Local/Demo/Saved/SaveGames/slot1.sav");
        let list = |h: &Harness| {
            let (c, b) = http(h, "GET", "/api/saves_list", &[("X-Rig-Token", &h.token)], b"");
            assert_eq!(c, 200);
            serde_json::from_str::<Value>(&b).unwrap()["snapshots"].as_array().unwrap().clone()
        };
        assert!(list(&h).is_empty());
        assert_eq!(http(&h, "GET", "/api/saves_list", &[], b"").0, 403);

        // no save yet: Play must not complain or create anything
        let g = selected_game(&h.state).unwrap();
        snapshot_before_launch(&h.state, &g);
        assert!(list(&h).is_empty());

        fs::write(&slot, "PROGRESS").unwrap();
        snapshot_before_launch(&h.state, &g);
        assert_eq!(list(&h).len(), 1);
        assert_eq!(list(&h)[0]["label"], "auto");
        assert!(last_log(&h).contains("Saves backed up before launch"));
        snapshot_before_launch(&h.state, &g);
        assert_eq!(list(&h).len(), 1, "unchanged saves are not copied again");

        // a mod corrupts the save; the next launch snapshots the damage too, the good one survives
        thread::sleep(Duration::from_millis(1100));
        fs::write(&slot, "DAMAGED!").unwrap();
        snapshot_before_launch(&h.state, &g);
        assert_eq!(list(&h).len(), 2);
        let good = list(&h).into_iter().find(|s| s["bytes"] == 8 && s["files"] == 1).and_then(|_| list(&h).last().cloned()).unwrap();
        api(&h, "/api/saves_restore", json!({ "id": good["id"] }));
        assert_eq!(fs::read_to_string(&slot).unwrap(), "PROGRESS");
        assert!(last_log(&h).contains("Restored 1 save file"));
        // the damaged state had already been captured before launch, so no duplicate is made and
        // it can still be brought back
        assert_eq!(list(&h).len(), 2);
        let newest = list(&h)[0]["id"].clone();
        api(&h, "/api/saves_restore", json!({ "id": newest }));
        assert_eq!(fs::read_to_string(&slot).unwrap(), "DAMAGED!");
        api(&h, "/api/saves_restore", json!({ "id": good["id"] }));
        assert_eq!(fs::read_to_string(&slot).unwrap(), "PROGRESS");

        // manual backup reports "unchanged" instead of piling up duplicates
        api(&h, "/api/saves_backup", json!({}));
        let n = list(&h).len();
        api(&h, "/api/saves_backup", json!({}));
        assert!(last_log(&h).contains("have not changed"));
        assert_eq!(list(&h).len(), n);
        // unknown ids are refused politely
        api(&h, "/api/saves_restore", json!({ "id": "../../etc" }));
        assert!(last_log(&h).contains("does not exist"));
    }

    #[test]
    #[cfg(not(windows))] // the saves of this test live in a Wine prefix
    fn the_save_browser_says_what_made_each_save_and_puts_back_one_file() {
        let h = harness("savebrowser");
        let dir = PathBuf::from(&h.game.pfx).join("drive_c/users/steamuser/AppData/Local/Demo/Saved/SaveGames");
        let g = selected_game(&h.state).unwrap();
        let get = |q: &str| {
            let (c, b) = http(&h, "GET", &format!("/api/saves_files{q}"), &[("X-Rig-Token", &h.token)], b"");
            assert_eq!(c, 200);
            serde_json::from_str::<Value>(&b).unwrap()
        };
        assert_eq!(http(&h, "GET", "/api/saves_files", &[], b"").0, 403);
        assert_eq!(get("")["files"], json!([]));

        fs::write(dir.join("slot1.sav"), "ONE").unwrap();
        fs::write(dir.join("slot2.sav"), "TWO").unwrap();
        let now = now_ms();
        let sessions = read_state(&h.state).paths.config_file.with_file_name("sessions.jsonl");
        stats::record(&sessions, &stats::Session { appid: g.appid.clone(), name: g.name.clone(), start_ms: now - 600_000, end_ms: now - 1000, secs: 599, mods: 2, profile: "Modded".into(), mod_names: vec!["A".into(), "B".into()], ..Default::default() });
        stats::record(&sessions, &stats::Session { appid: "other".into(), start_ms: now - 600_000, end_ms: now, ..Default::default() });
        let live = get("");
        assert_eq!(live["files"].as_array().unwrap().len(), 2);
        let made = &live["sessions"][live["files"][0]["made"].as_u64().unwrap() as usize];
        assert_eq!((made["profile"].as_str(), made["mods"].as_u64(), made["names"].clone()), (Some("Modded"), Some(2), json!(["A", "B"])), "written during that session (or just after)");
        assert_eq!(live["sessions"].as_array().unwrap().len(), 1, "one session row shared by both files; the other game's is not used");

        snapshot_before_launch(&h.state, &g);
        let id = saves::list(&g, &read_state(&h.state).cache_root())[0].id.clone();
        let inside = get(&format!("?id={id}"));
        let key = inside["files"][0]["key"].as_str().unwrap().to_string();
        assert_eq!(inside["files"].as_array().unwrap().len(), 2);
        thread::sleep(Duration::from_millis(1100));
        fs::write(dir.join("slot2.sav"), "RUINED").unwrap();
        let diff = get(&format!("?a={id}&b=now"));
        assert_eq!((diff["diff"]["changed"][0]["path"].as_str(), diff["diff"]["same"].as_u64()), (Some("slot2.sav"), Some(1)));
        assert!(get("?a=nope&b=now")["error"].is_string());

        api(&h, "/api/saves_restore_file", json!({ "id": id, "key": key, "path": "slot2.sav" }));
        wait_until("one file back", || fs::read_to_string(dir.join("slot2.sav")).unwrap() == "TWO");
        assert!(last_log(&h).contains("slot2.sav"));
        api(&h, "/api/saves_restore_file", json!({ "id": id, "key": key, "path": "../../../../x" }));
        wait_until("the refusal", || last_log(&h).contains("not a file of this backup"));
    }

    #[test]
    fn damaged_settings_come_back_from_the_copy_or_the_weekly_backup_never_the_defaults() {
        let h = harness("cfgrecover");
        let paths = h.state.read().unwrap().paths.clone();
        {
            let mut st = h.state.write().unwrap();
            st.config.theme = "midnight".into();
            st.save_config();
            st.config.game_notes.insert("42".into(), "my note".into());
            st.save_config();
        }
        // half a file (a crash of the computer, a full disk)
        let text = fs::read_to_string(&paths.config_file).unwrap();
        fs::write(&paths.config_file, &text[..text.len() / 2]).unwrap();
        let c = load_config(&paths);
        assert_eq!(c.theme, "midnight", "the copy from before the last change");
        assert!(serde_json::from_str::<RigConfigData>(&fs::read_to_string(&paths.config_file).unwrap()).is_ok(), "and it is back in place");
        let dir = paths.config_file.parent().unwrap();
        assert!(fs::read_dir(dir).unwrap().flatten().any(|e| e.file_name().to_string_lossy().starts_with("rig_config.json.corrupt-")), "the damaged one is kept");

        // no good copy either: the newest weekly backup
        let backups = h.state.read().unwrap().cache_root().join("auto-backups");
        let mut bundle = backup_bundle(&h.state);
        bundle["config"]["theme"] = json!("forest");
        autobackup::store(&backups, now_ms(), &bundle.to_string()).unwrap();
        fs::write(&paths.config_file, "{ broken").unwrap();
        fs::write(fsutil::kept_copy(&paths.config_file), "also broken").unwrap();
        assert_eq!(load_config(&paths).theme, "forest");
        // a missing file is simply a first start
        fs::remove_file(&paths.config_file).unwrap();
        fs::remove_file(fsutil::kept_copy(&paths.config_file)).unwrap();
        assert_eq!(load_config(&paths).theme, RigConfigData::default().theme);
    }

    #[test]
    fn command_line_switches() {
        assert_eq!(handle_cli(vec![]), None);
        assert_eq!(handle_cli(vec!["--version".into()]), Some(0));
        assert_eq!(handle_cli(vec!["--help".into()]), Some(0));
        assert_eq!(handle_cli(vec!["--bogus".into()]), Some(2));
    }

    #[test]
    fn notifier_is_monotonic_and_times_out() {
        let n = Arc::new(EventNotifier::new());
        let t0 = Instant::now();
        assert_eq!(n.wait_for_change(1, Duration::from_millis(200)), 1);
        assert!(t0.elapsed() >= Duration::from_millis(190));
        let ts: Vec<_> = (0..16).map(|_| { let n = Arc::clone(&n); thread::spawn(move || { for _ in 0..100 { n.bump(); } }) }).collect();
        for t in ts { t.join().unwrap(); }
        assert_eq!(n.version.load(Ordering::SeqCst), 1601);
        assert_eq!(*n.lock.lock().unwrap(), 1601);
        assert_eq!(n.wait_for_change(5, Duration::from_secs(5)), 1601, "returns immediately when behind");
    }

    #[test]
    fn helper_functions() {
        assert_eq!(sanitize_mod_folder_name("../Weird Mod!"), "WeirdMod");
        assert_eq!(sanitize_mod_folder_name("!!!"), "CustomLuaMod");
        assert!(pak_goes_to_logic_mods(Path::new("HSTM_UI.pak")));
        assert!(pak_goes_to_logic_mods(Path::new("/x/LogicMods/foo.pak")));
        assert!(!pak_goes_to_logic_mods(Path::new("Quick_Build.pak")));
        assert!(!pak_goes_to_logic_mods(Path::new("Suit.pak")));
        assert_eq!(query_param("name=a%20b&t=xyz", "name").as_deref(), Some("a b"));
        assert_eq!(query_param("a=1", "t"), None);
        assert!(constant_time_eq("abc", "abc") && !constant_time_eq("abc", "abd") && !constant_time_eq("abc", "ab"));
    }

    #[test]
    fn logs_are_bounded_and_ordered() {
        let n = Arc::new(EventNotifier::new());
        let mut st = AppState::with(
            RigPaths {
                custom_games_file: PathBuf::new(),
                config_file: PathBuf::new(),
                cache_dir: PathBuf::from("/nonexistent/x"),
                engine_cache_file: PathBuf::new(),
                webview_profile_dir: PathBuf::new(),
            hydra_dir: None,
            launchers: Default::default(),
            },
            RigConfigData::default(),
            vec![],
            vec![],
            n,
        );
        for i in 0..(MAX_LOG_HISTORY + 50) {
            st.log("INFO", format!("m{}", i));
        }
        assert_eq!(st.logs.len(), MAX_LOG_HISTORY);
        assert_eq!(st.logs.front().unwrap().message, "m50");
        assert_eq!(st.last_toast, format!("m{}", MAX_LOG_HISTORY + 49));
        assert!(st.to_json()["games"].as_array().unwrap().is_empty());
    }

    #[test]
    fn store_pagination_isolation_and_state_hints() {
        let h = harness("store_state_hints");
        {
            let mut st = h.state.write().unwrap();
            st.modio.key = Some("1234567890abcdef".into());
            st.vt.key = Some("abcdef1234567890".into());
            st.config.virustotal = true;
            // Add an uninstalled game with stale health and update
            let mut uninstalled = h.game.clone();
            uninstalled.appid = "uninstalled_999".into();
            uninstalled.name = "Uninstalled Game".into();
            uninstalled.is_installed = false;
            st.games.push(uninstalled);
            st.health.insert("uninstalled_999".into(), "warn".into());
            st.nexus.updates.push(UpdateRow {
                appid: "uninstalled_999".into(),
                game: "Uninstalled Game".into(),
                lib_id: "mod1".into(),
                name: "Some Mod".into(),
                installed_version: "1.0".into(),
                new_version: "2.0".into(),
                new_file_id: 123,
                new_file_name: "mod.zip".into(),
                changes: String::new(),
                nexus: nexus::NexusRef { game: "uninstalled".into(), mod_id: 1, file_id: 1 },
                page_url: String::new(),
            });
        }
        let state = api(&h, "/api/state", json!({}));
        assert_eq!(state["modio"]["configured"], true);
        assert_eq!(state["modio"]["key_hint"], "…cdef");
        assert_eq!(state["settings"]["vt_key"], true);
        assert_eq!(state["settings"]["vt_key_hint"], "…7890");
        // Ensure uninstalled game's health and updates are never leaked to client
        assert!(state["health"].get("uninstalled_999").is_none());
        let nx_updates = state["nexus"]["updates"].as_array().unwrap();
        assert!(!nx_updates.iter().any(|u| u["appid"] == "uninstalled_999"));
    }

    #[test]
    fn profile_save_validation_and_notifier() {
        let h = harness("prof_val");
        // Empty profile name should be rejected with warning and never stored
        api(&h, "/api/profile_save", json!({ "name": "" }));
        assert!(last_log(&h).contains("empty"));
        assert_eq!(h.state.read().unwrap().config.profiles.get(&h.game.appid).and_then(|m| m.get("")), None);

        // Valid profile name should be accepted, bump notifier, and be stored in config
        let v1 = h.state.read().unwrap().notifier.version.load(Ordering::SeqCst);
        api(&h, "/api/profile_save", json!({ "name": "Speedrun" }));
        assert!(last_log(&h).contains("Speedrun"));
        assert!(h.state.read().unwrap().notifier.version.load(Ordering::SeqCst) > v1);
        assert!(h.state.read().unwrap().config.profiles.get(&h.game.appid).and_then(|m| m.get("Speedrun")).is_some());
    }

    #[test]
    fn key_file_clearing_purges_migration_candidates() {
        let h = harness("purge_cand");
        let target = h._dir.join("target_key");
        let hp = home_dir();
        let old1 = hp.join(".config/gantry/test_key_purge");
        let old2 = hp.join(".config/protonrig/test_key_purge");
        let _ = fs::create_dir_all(old1.parent().unwrap());
        let _ = fs::create_dir_all(old2.parent().unwrap());
        fs::write(&target, "secret_key").unwrap();
        let _ = fs::write(&old1, "old_secret_1");
        let _ = fs::write(&old2, "old_secret_2");

        clear_key_file_with_migration(&target, "test_key_purge");
        assert!(!target.exists());
        assert!(!old1.exists());
        assert!(!old2.exists());
    }

    #[test]
    fn modio_game_selection_persists_in_config() {
        let h = harness("modio_cfg");
        let g = selected_game(&h.state).unwrap();
        let f = modio::fake::start(vec![]);
        {
            let mut st = h.state.write().unwrap();
            st.modio.key = Some(modio::fake::KEY.into());
            st.modio.base = f.base.clone();
            st.modio.allow_http = true;
        }
        let res = modio_game_set(&h.state, &g, 7);
        assert!(res.is_ok(), "{res:?}");
        {
            let st = h.state.read().unwrap();
            assert_eq!(st.config.modio_game.get(&g.appid), Some(&7));
        }
        let resolved = modio_game_for(&h.state, &g);
        assert_eq!(resolved.unwrap(), 7);
    }

    #[test]
    fn batch_actions_and_tagging_update_library_order_and_rules() {
        let h = harness("batch_ops");
        add_plain_game(&h);
        let g = h.state.read().unwrap().games.last().unwrap().clone();
        let lib = library_of(&g).unwrap();

        for name in ["Mod Alpha", "Mod Beta", "Mod Gamma"] {
            let s = h._dir.join(&format!("batch-{name}"));
            fs::create_dir_all(s.join("Data")).unwrap();
            fs::write(s.join(format!("Data/{name}.esp")), "CONTENT").unwrap();
            lib.add_with(&s, name, &format!("{name}.zip"), None, "1.0").unwrap();
        }
        refresh_mods_shared(&h.state);

        let mods = lib.mods();
        assert_eq!(mods.len(), 3);
        let id_a = mods[0].id.clone();
        let id_b = mods[1].id.clone();
        let id_c = mods[2].id.clone();

        // 1. Tagging / Custom Grouping
        api(&h, "/api/lib_group", json!({ "ids": [id_a.clone(), id_b.clone()], "group": "Visuals" }));
        wait_until("mods grouped", || {
            let m = lib.mods();
            m.iter().find(|x| x.id == id_a).unwrap().group == "Visuals" &&
            m.iter().find(|x| x.id == id_b).unwrap().group == "Visuals"
        });

        // 2. Batch Update Rule
        api(&h, "/api/lib_update_rule_many", json!({ "ids": [id_a.clone(), id_c.clone()], "rule": "never" }));
        wait_until("update rules set", || {
            let m = lib.mods();
            m.iter().find(|x| x.id == id_a).unwrap().update_rule == "never" &&
            m.iter().find(|x| x.id == id_c).unwrap().update_rule == "never"
        });

        // 3. Batch Target Folder
        api(&h, "/api/lib_target_many", json!({ "ids": [id_b.clone(), id_c.clone()], "target": "submods" }));
        wait_until("targets set", || {
            let m = lib.mods();
            m.iter().find(|x| x.id == id_b).unwrap().target == "submods" &&
            m.iter().find(|x| x.id == id_c).unwrap().target == "submods"
        });

        // 4. Batch Move To Bottom
        api(&h, "/api/lib_move_many_to", json!({ "ids": [id_a.clone()], "top": false }));
        wait_until("alpha moved to bottom", || {
            let m = lib.mods();
            m.last().unwrap().id == id_a
        });

        // Batch Move To Top: Move [Gamma, Alpha] to top in order -> [Gamma, Alpha, Beta]
        api(&h, "/api/lib_move_many_to", json!({ "ids": [id_c.clone(), id_a.clone()], "top": true }));
        wait_until("gamma and alpha moved to top", || {
            let m = lib.mods();
            m[0].id == id_c && m[1].id == id_a
        });
    }

    #[test]
    fn steam_deck_sync_syncs_custom_game_shortcut_and_grid_artwork() {
        let h = harness("deck_sync");
        let cfg = h._dir.join("steam-acct/config");
        fs::create_dir_all(&cfg).unwrap();
        let file = cfg.join("shortcuts.vdf");
        let grid = cfg.join("grid");

        let g_dir = h._dir.join("MyGame");
        fs::create_dir_all(&g_dir).unwrap();
        fs::write(g_dir.join("game.exe"), b"MZ custom").unwrap();
        fs::write(g_dir.join("icon.png"), b"CUSTOM_ART").unwrap();

        let custom_game: engine::GameEntry = serde_json::from_value(json!({
            "appid": "custom-mygame",
            "name": "My Indie Game",
            "game_dir": g_dir.to_string_lossy(),
            "exe_path": "game.exe",
            "is_custom": true,
            "is_installed": true,
            "launch_args": "-novid",
        })).unwrap();

        {
            let mut st = h.state.write().unwrap();
            st.steam_env.localconfig = Some(cfg.join("localconfig.vdf"));
            st.steam_env.running = Some(false);
            st.games.push(custom_game);
        }

        api(&h, "/api/steam_deck_sync", json!({ "appid": "custom-mygame", "close_steam": true }));
        wait_until("game synced to steam deck", || last_log(&h).contains("is in Steam's library now for Steam Deck Game Mode"));

        assert!(file.exists());
        let bytes = fs::read(&file).unwrap();
        let list = steamshortcut::list_shortcuts(&bytes).unwrap();
        let item = list.iter().find(|s| s.name == "My Indie Game").expect("found in shortcuts.vdf");
        assert_eq!(item.launch_options, "-novid");
        assert!(item.tags.contains(&"ModForge".to_string()));

        let sid = item.appid;
        assert!(grid.join(format!("{sid}p.png")).exists());
        assert_eq!(fs::read(grid.join(format!("{sid}p.png"))).unwrap(), b"CUSTOM_ART");
    }

    #[test]
    fn startup_benchmark_and_conflict_auto_resolve_endpoints_operate_correctly() {
        let h = harness("bench_resolve");
        let g = selected_game(&h.state).unwrap();
        let lib = library_of(&g).unwrap();

        let s1 = h._dir.join("test-patch");
        fs::create_dir_all(s1.join("Data")).unwrap();
        fs::write(s1.join("Data/settings.ini"), "PATCH").unwrap();
        lib.add_with(&s1, "Compatibility Patch", "patch.zip", None, "1.0").unwrap();

        let s2 = h._dir.join("test-base");
        fs::create_dir_all(s2.join("Data")).unwrap();
        fs::write(s2.join("Data/settings.ini"), "BASE").unwrap();
        lib.add_with(&s2, "Base Overhaul", "base.zip", None, "1.0").unwrap();
        refresh_mods_shared(&h.state);

        let (code, text) = http(&h, "GET", "/api/startup_benchmark", &[("X-Rig-Token", &h.token)], b"");
        assert_eq!(code, 200);
        let bm: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(bm["mod_count"].as_u64(), Some(2));
        assert!(bm["overhead_ms"].as_u64().unwrap() > 0);

        api(&h, "/api/lib_auto_resolve", json!({ "strategy": "topological_patches_last" }));
        wait_until("conflicts auto resolved", || {
            let m = lib.mods();
            m.len() == 2 && m[0].name == "Base Overhaul" && m[1].name == "Compatibility Patch"
        });
    }
}
