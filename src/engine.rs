use crate::fsutil::{self, home_dir};
use crate::repair::{ModManager, RigConfigData};
use memchr::memmem;
use memmap2::MmapOptions;
use rayon::prelude::*;
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};
use walkdir::WalkDir;

static SIMD_ASCII_RELEASE: LazyLock<memmem::Finder<'static>> =
LazyLock::new(|| memmem::Finder::new(b"+Release-"));

static SIMD_UTF16_RELEASE: LazyLock<memmem::Finder<'static>> = LazyLock::new(|| {
    memmem::Finder::new(b"+\x00R\x00e\x00l\x00e\x00a\x00s\x00e\x00-\x00")
});

static RE_VDF_PATH: LazyLock<Regex> =
LazyLock::new(|| Regex::new(r#""path"\s+"([^"]+)""#).unwrap());
static RE_ACF_ID: LazyLock<Regex> =
LazyLock::new(|| Regex::new(r#""appid"\s+"(\d+)""#).unwrap());
static RE_ACF_NAME: LazyLock<Regex> =
LazyLock::new(|| Regex::new(r#""name"\s+"([^"]+)""#).unwrap());
static RE_ACF_DIR: LazyLock<Regex> =
LazyLock::new(|| Regex::new(r#""installdir"\s+"([^"]+)""#).unwrap());

static ENGINE_CACHE_MEM: LazyLock<Mutex<HashMap<String, CachedBinarySig>>> =
LazyLock::new(|| Mutex::new(HashMap::new()));

/// Short-lived memo for the (comparatively expensive) Win64 directory search.
type UeBinMemo = HashMap<PathBuf, (Instant, Option<PathBuf>)>;
static UE_BIN_MEMO: LazyLock<Mutex<UeBinMemo>> = LazyLock::new(|| Mutex::new(HashMap::new()));
const UE_BIN_MEMO_TTL: Duration = Duration::from_secs(4);

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CachedBinarySig {
    pub ino: u64,
    pub mtime: i64,
    pub size: u64,
    /// Empty `major` records "scanned, no UE version string present" so huge
    /// non-UE executables are not re-scanned on every rescan.
    pub major: String,
    pub minor: String,
    /// Detection logic revision; entries from older logic (which could not read the PE
    /// version resource) are ignored so their "not found" answers get re-examined.
    #[serde(default)]
    pub rev: u32,
}

const CACHE_REV: u32 = 2;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct GameEntry {
    pub appid: String,
    pub name: String,
    pub game_dir: String,
    #[serde(default)]
    pub exe_path: String,
    #[serde(default)]
    pub shipping_exe: String,
    #[serde(default)]
    pub bin_dir: String,
    #[serde(default = "default_engine")]
    pub engine: String,
    #[serde(default)]
    pub compatdata: String,
    #[serde(default)]
    pub pfx: String,
    #[serde(default)]
    pub has_pfx: bool,
    #[serde(default)]
    pub is_custom: bool,
    #[serde(default = "default_true")]
    pub is_installed: bool,
    #[serde(default)]
    pub steam_appid: String,
    #[serde(default)]
    pub launch_args: String,
    #[serde(default)]
    pub env_vars: String,
    #[serde(default)]
    pub trainer_name: String,
    #[serde(default)]
    pub mod_count: usize,
    /// "hydra", "heroic" or "lutris" for games discovered in another launcher; empty for Steam and custom games.
    #[serde(default)]
    pub source: String,
    /// Per-game Proton directory chosen elsewhere (Hydra); empty = use rig's selection.
    #[serde(default)]
    pub proton_path: String,
    /// UE4SS is installed in this (Unreal) game.
    #[serde(default)]
    pub ue4ss: bool,
    /// Unity games: "none", "bepinex" or "melonloader"; empty for other engines.
    #[serde(default)]
    pub loader: String,
    /// Steam app whose artwork is shown for a game that is not from Steam (found by name).
    #[serde(default)]
    pub cover_id: String,
}

/// Is UE4SS present in the game's Win64 folder?
pub fn ue4ss_installed_in(ue_bin: &Path) -> bool {
    ModManager::get_ue4ss_root(ue_bin).join("UE4SS.dll").is_file() || ue_bin.join("UE4SS.dll").is_file()
}

impl GameEntry {
    /// The Steam id support entries are looked up by: the game's own, or the one its cover was found under by name.
    /// The Steam app id of a game that Steam itself has installed: a game found in a Steam library is its own id,
    /// one that came from another launcher carries Steam's number separately (and may not live in a Steam library).
    pub fn steam_id(&self) -> &str {
        if !self.steam_appid.is_empty() {
            &self.steam_appid
        } else if !self.is_custom && self.source.is_empty() && !self.appid.is_empty() && self.appid.bytes().all(|b| b.is_ascii_digit()) {
            &self.appid
        } else {
            ""
        }
    }

    pub fn recipe_steam_id(&self) -> &str {
        if self.steam_appid.is_empty() {
            &self.cover_id
        } else {
            &self.steam_appid
        }
    }

    /// Re-check things that change when the user installs or removes mod loaders or tools.
    pub fn refresh_flags(&mut self) {
        self.ue4ss = RigEngine::find_ue_bin_dir(Path::new(&self.game_dir)).map(|ub| ue4ss_installed_in(&ub)).unwrap_or(false);
        if self.is_installed && !self.game_dir.is_empty() {
            let p = Path::new(&self.game_dir);
            if self.engine == "Standard / Non-UE" {
                if crate::loaders::detect(p).is_some() {
                    self.engine = "Unity".into();
                } else if let Some(b) = crate::loaders::detect_bethesda(p) {
                    self.engine = format!("Bethesda ({})", b.game);
                } else if let Some(s) = crate::loaders::detect_source(p) {
                    self.engine = if s.is_goldsrc { "GoldSrc".into() } else { "Source Engine".into() };
                } else if crate::loaders::detect_godot(p).is_some() {
                    self.engine = "Godot".into();
                }
            }
            self.loader = if self.engine == "Unity" {
                crate::loaders::detect(p).map(|u| if u.loader.is_empty() { "none".to_string() } else { u.loader }).unwrap_or_default()
            } else if self.engine.starts_with("Bethesda") {
                crate::loaders::detect_bethesda(p).map(|b| if b.loader.is_empty() { "none".to_string() } else { b.loader }).unwrap_or_default()
            } else if self.engine == "Source Engine" || self.engine == "GoldSrc" {
                crate::loaders::detect_source(p).map(|s| if s.loader.is_empty() { "none".to_string() } else { s.loader }).unwrap_or_default()
            } else if self.engine == "Godot" {
                crate::loaders::detect_godot(p).map(|g| if g.loader.is_empty() { "none".to_string() } else { g.loader }).unwrap_or_default()
            } else {
                String::new()
            };
        } else {
            self.loader = String::new();
        }
    }

    /// Launched by rig itself (own Proton/Wine process) rather than handed to Steam.
    pub fn launches_directly(&self) -> bool {
        self.is_custom || !self.source.is_empty()
    }
}

fn default_engine() -> String {
    "Standard / Non-UE".to_string()
}
fn default_true() -> bool {
    true
}

#[derive(Clone)]
pub struct RigPaths {
    pub custom_games_file: PathBuf,
    pub config_file: PathBuf,
    pub cache_dir: PathBuf,
    pub engine_cache_file: PathBuf,
    pub webview_profile_dir: PathBuf,
    /// Hydra Launcher's data directory, when installed.
    pub hydra_dir: Option<PathBuf>,
    /// Heroic and Lutris, when installed.
    pub launchers: crate::launchers::LauncherPaths,
}

impl Default for RigPaths {
    fn default() -> Self {
        Self::new()
    }
}

impl RigPaths {
    pub fn new() -> Self {
        let hp = home_dir();
        let config_dir = crate::platform::config_dir();
        let rig_cache_root = crate::platform::cache_root();
        let cache_dir = rig_cache_root.join("injectors");
        let webview_profile_dir = rig_cache_root.join("webview_profile");

        let _ = fs::create_dir_all(&config_dir);
        let _ = fs::create_dir_all(&cache_dir);
        let _ = fs::create_dir_all(&webview_profile_dir);

        Self {
            custom_games_file: config_dir.join("custom_games.json"),
            config_file: config_dir.join("rig_config.json"),
            engine_cache_file: rig_cache_root.join("engine_cache.json"),
            cache_dir,
            webview_profile_dir,
            hydra_dir: crate::hydra::find_dir(&hp),
            launchers: crate::launchers::LauncherPaths::detect(&hp),
        }
    }

    pub fn steam_roots() -> Vec<PathBuf> {
        crate::platform::steam_roots()
    }
}

pub fn is_auxiliary_exe(name_lower: &str) -> bool {
    name_lower.contains("crashreport")
    || name_lower.contains("prereq")
    || name_lower.contains("cefsubprocess")
    || name_lower.contains("unins")
    || name_lower.contains("unitycrash")
    || name_lower.contains("benchmark")
}

pub struct RigEngine;

impl RigEngine {
    pub fn load_disk_cache(cache_file: &Path) {
        if let Ok(txt) = fs::read_to_string(cache_file) {
            if let Ok(map) = serde_json::from_str::<HashMap<String, CachedBinarySig>>(&txt) {
                let mut guard = ENGINE_CACHE_MEM.lock().unwrap_or_else(|e| e.into_inner());
                *guard = map;
            }
        }
    }

    /// Atomic write so the cache is never corrupted on sudden power loss.
    /// Entries for executables that no longer exist are pruned first.
    pub fn save_disk_cache(cache_file: &Path) {
        let mut guard = ENGINE_CACHE_MEM.lock().unwrap_or_else(|e| e.into_inner());
        guard.retain(|path, _| Path::new(path).is_file());
        if let Ok(txt) = serde_json::to_string(&*guard) {
            let _ = fsutil::atomic_write(cache_file, txt);
        }
    }

    pub fn find_steam_root() -> Option<PathBuf> {
        RigPaths::steam_roots()
        .into_iter()
        .find(|p| p.is_dir() && p.join("steamapps").is_dir())
    }

    /// Every `steamapps` folder: Steam's own, those listed in `libraryfolders.vdf`, and the
    /// user's `extra_libraries` (each either a Steam library folder or a `steamapps` folder).
    pub fn parse_libraries(steam_root: Option<&Path>, extra: &[String]) -> Vec<PathBuf> {
        let mut libs = HashSet::new();
        if let Some(root) = steam_root {
            let apps = root.join("steamapps");
            libs.insert(crate::fsutil::canonical(&apps).unwrap_or(apps.clone()));
            if let Ok(txt) = fs::read_to_string(apps.join("libraryfolders.vdf")) {
                for cap in RE_VDF_PATH.captures_iter(&txt) {
                    let d = PathBuf::from(&cap[1]).join("steamapps");
                    if d.is_dir() {
                        libs.insert(crate::fsutil::canonical(&d).unwrap_or(d));
                    }
                }
            }
        }
        for e in extra {
            let p = PathBuf::from(e);
            let d = if p.file_name().map(|n| n == "steamapps").unwrap_or(false) { p } else { p.join("steamapps") };
            if d.is_dir() {
                libs.insert(crate::fsutil::canonical(&d).unwrap_or(d));
            }
        }
        let mut out: Vec<PathBuf> = libs.into_iter().collect();
        out.sort();
        out
    }

    pub fn find_protons() -> Vec<(String, PathBuf)> {
        let mut protons = Vec::new();
        let mut seen = HashSet::new();
        let s_root = Self::find_steam_root();
        for lib in Self::parse_libraries(s_root.as_deref(), &[]) {
            if let Ok(entries) = fs::read_dir(lib.join("common")) {
                for e in entries.flatten() {
                    let name = e.file_name().to_string_lossy().to_string();
                    if name.to_lowercase().contains("proton") {
                        let ps = e.path().join("proton");
                        if ps.is_file()
                            && seen.insert(crate::fsutil::canonical(&ps).unwrap_or(ps.clone()))
                            {
                                protons.push((name, ps));
                            }
                    }
                }
            }
        }
        let home = home_dir();
        for cbase in [
            home.join(".local/share/Steam/compatibilitytools.d"),
            home.join(".steam/root/compatibilitytools.d"),
        ] {
            if let Ok(entries) = fs::read_dir(cbase) {
                for e in entries.flatten() {
                    let name = e.file_name().to_string_lossy().to_string();
                    let ps = e.path().join("proton");
                    if ps.is_file() && seen.insert(crate::fsutil::canonical(&ps).unwrap_or(ps.clone())) {
                        protons.push((format!("Custom: {}", name), ps));
                    }
                }
            }
        }
        protons.sort_by_key(|(n, _)| {
            let low = n.to_lowercase();
            if low.contains("experimental") {
                0
            } else if low.contains("ge-proton") {
                1
            } else if low.contains("cachyos") {
                2
            } else {
                3
            }
        });
        protons
    }

    pub fn resolve_proton_wine_bin(proton_script: Option<&Path>) -> PathBuf {
        if let Some(ps) = proton_script {
            if let Some(pdir) = ps.parent() {
                for rel in [
                    "files/bin/wine64",
                    "files/bin/wine",
                    "dist/bin/wine64",
                    "dist/bin/wine",
                ] {
                    let candidate = pdir.join(rel);
                    if candidate.is_file() {
                        return candidate;
                    }
                }
            }
        }
        PathBuf::from("wine")
    }

    /// Locate the game's `Binaries/Win64` directory (memoised for a few seconds
    /// because list/watch/repair paths all ask for it repeatedly).
    pub fn find_ue_bin_dir(game_dir: &Path) -> Option<PathBuf> {
        {
            let memo = UE_BIN_MEMO.lock().unwrap_or_else(|e| e.into_inner());
            if let Some((at, res)) = memo.get(game_dir) {
                let still_valid = at.elapsed() < UE_BIN_MEMO_TTL
                    && res.as_ref().map(|p| p.is_dir()).unwrap_or(true);
                if still_valid {
                    return res.clone();
                }
            }
        }
        let res = Self::find_ue_bin_dir_uncached(game_dir);
        let mut memo = UE_BIN_MEMO.lock().unwrap_or_else(|e| e.into_inner());
        if memo.len() > 256 {
            memo.clear();
        }
        memo.insert(game_dir.to_path_buf(), (Instant::now(), res.clone()));
        res
    }

    /// Forget memoised directory lookups (after installs/wipes that change the layout).
    pub fn invalidate_ue_bin_memo() {
        UE_BIN_MEMO.lock().unwrap_or_else(|e| e.into_inner()).clear();
    }

    fn find_ue_bin_dir_uncached(game_dir: &Path) -> Option<PathBuf> {
        if !game_dir.is_dir() {
            return None;
        }
        if game_dir
            .file_name()
            .map(|n| n.to_string_lossy().eq_ignore_ascii_case("win64"))
            .unwrap_or(false)
        {
            return Some(game_dir.to_path_buf());
        }
        let mut fallback = None;
        // Only directories matter, and Content/Saved trees (huge, never contain
        // Binaries) are pruned so the walk stays cheap on 100 GB installs.
        let walker = WalkDir::new(game_dir)
            .min_depth(1)
            .max_depth(5)
            .follow_links(true)
            .into_iter()
            .filter_entry(|e| {
                if !e.file_type().is_dir() {
                    return false;
                }
                let n = e.file_name().to_string_lossy();
                !(n.eq_ignore_ascii_case("content")
                    || n.eq_ignore_ascii_case("saved")
                    || n.starts_with(fsutil::TMP_MARK)
                    || n.starts_with(".rig_stage"))
            });
        for entry in walker.flatten() {
            if !entry.file_name().to_string_lossy().eq_ignore_ascii_case("win64") {
                continue;
            }
            // Judge "engine" / "binaries" on the path *below* the game dir so an
            // install folder like ".../Unreal Engine Demo/" is not rejected wholesale.
            let rel = entry.path().strip_prefix(game_dir).unwrap_or(entry.path());
            if rel
                .components()
                .any(|c| c.as_os_str().to_string_lossy().eq_ignore_ascii_case("engine"))
            {
                continue;
            }
            let in_binaries = entry
                .path()
                .parent()
                .and_then(|p| p.file_name())
                .map(|n| n.to_string_lossy().eq_ignore_ascii_case("binaries"))
                .unwrap_or(false);
            if !in_binaries {
                continue;
            }
            if Self::find_shipping_exe(entry.path()).is_some() {
                return Some(entry.path().to_path_buf());
            }
            if fallback.is_none() {
                fallback = Some(entry.path().to_path_buf());
            }
        }
        fallback
    }

    pub fn find_shipping_exe(ue_bin: &Path) -> Option<PathBuf> {
        let mut candidates: Vec<PathBuf> = fs::read_dir(ue_bin)
        .ok()?
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            let n = p
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .to_lowercase();
            n.ends_with(".exe") && !is_auxiliary_exe(&n)
        })
        .collect();

        candidates.sort_by_key(|p| {
            let n = p
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .to_lowercase();
            let is_shipping = n.ends_with("shipping.exe");
            let size = fs::metadata(p).map(|m| m.len()).unwrap_or(0);
            (!is_shipping, std::cmp::Reverse(size))
        });

        candidates.into_iter().next()
    }

    /// Pull `(major, minor)` out of a UE build-string such as `++UE5+Release-5.4`
    /// (ASCII) or its UTF-16LE form. Pure function over the mapped bytes.
    pub fn scan_ue_version(data: &[u8]) -> Option<(String, String)> {
        for pos in SIMD_ASCII_RELEASE.find_iter(data) {
            if pos >= 3 && (&data[pos - 3..pos] == b"UE5" || &data[pos - 3..pos] == b"UE4") {
                let start = pos + 9;
                if start + 3 <= data.len() {
                    let (maj, dot, min1) = (data[start], data[start + 1], data[start + 2]);
                    if (maj == b'4' || maj == b'5') && dot == b'.' && min1.is_ascii_digit() {
                        let mut min_str = (min1 as char).to_string();
                        if start + 4 <= data.len() && data[start + 3].is_ascii_digit() {
                            min_str.push(data[start + 3] as char);
                        }
                        return Some(((maj as char).to_string(), min_str));
                    }
                }
            }
        }
        for pos in SIMD_UTF16_RELEASE.find_iter(data) {
            if pos >= 6 && data[pos - 6] == b'U' && data[pos - 4] == b'E' {
                let start = pos + 18;
                if start + 5 <= data.len() {
                    let (maj, dot, min1) = (data[start], data[start + 2], data[start + 4]);
                    if (maj == b'4' || maj == b'5') && dot == b'.' && min1.is_ascii_digit() {
                        let mut min_str = (min1 as char).to_string();
                        if start + 7 <= data.len() && data[start + 6].is_ascii_digit() {
                            min_str.push(data[start + 6] as char);
                        }
                        return Some(((maj as char).to_string(), min_str));
                    }
                }
            }
        }
        None
    }

    /// The engine version a Windows executable declares in its PE version resource
    /// (`VS_FIXEDFILEINFO`). Unreal stamps the engine version there ("5.6.1.0"), which covers
    /// projects whose custom branch name hides it from the `UE5+Release-x.y` strings.
    pub fn scan_pe_version(data: &[u8]) -> Option<(String, String)> {
        static SIG: LazyLock<memmem::Finder<'static>> = LazyLock::new(|| memmem::Finder::new(&[0xBD, 0x04, 0xEF, 0xFE]));
        for pos in SIG.find_iter(data) {
            let Some(b) = data.get(pos..pos + 16) else { continue };
            let struc_version = u32::from_le_bytes(b[4..8].try_into().unwrap());
            let file_ms = u32::from_le_bytes(b[8..12].try_into().unwrap());
            let (major, minor) = (file_ms >> 16, file_ms & 0xffff);
            if struc_version == 0x0001_0000 && (major == 4 || major == 5) && minor <= 30 {
                return Some((major.to_string(), minor.to_string()));
            }
        }
        None
    }

    /// The Unreal Engine version of the game in `ue_bin`, or `None` when it cannot be told.
    /// Never guesses: a wrong version makes UE4SS read memory with the wrong layout.
    pub fn detect_ue_version_opt(ue_bin: &Path) -> Option<(String, String)> {
        let rd = fs::read_dir(ue_bin).ok()?;
        let mut exes: Vec<PathBuf> = rd
            .flatten()
            .map(|e| e.path())
            .filter(|p| {
                let n = p.file_name().unwrap_or_default().to_string_lossy().to_lowercase();
                n.ends_with(".exe") && !is_auxiliary_exe(&n)
            })
            .collect();

        exes.sort_by_key(|p| {
            !p.file_name()
                .map(|n| n.to_string_lossy().to_lowercase().contains("shipping"))
                .unwrap_or(false)
        });

        for exe in exes {
            let key = exe.to_string_lossy().to_string();
            let Ok(meta) = fs::metadata(&exe) else {
                continue;
            };
            let (ino, mtime, size) = crate::fsutil::file_signature(&meta);

            {
                let guard = ENGINE_CACHE_MEM.lock().unwrap_or_else(|e| e.into_inner());
                if let Some(c) = guard.get(&key) {
                    if c.rev == CACHE_REV && c.ino == ino && c.mtime == mtime && c.size == size {
                        if c.major.is_empty() {
                            continue; // examined before: no version information inside
                        }
                        return Some((c.major.clone(), c.minor.clone()));
                    }
                }
            }

            if size == 0 {
                continue;
            }
            let Ok(file) = File::open(&exe) else {
                continue;
            };
            // SAFETY: the mapping is read-only and dropped before this iteration ends;
            // a concurrent truncation of an installed game binary is the only hazard
            // and is the same risk every PE scanner accepts.
            let Ok(mmap) = (unsafe { MmapOptions::new().map(&file) }) else {
                continue;
            };
            #[cfg(unix)]
            let _ = mmap.advise(memmap2::Advice::Sequential);

            let found = Self::scan_ue_version(&mmap).or_else(|| Self::scan_pe_version(&mmap));
            let (major, minor) = found.clone().unwrap_or_default();
            ENGINE_CACHE_MEM
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(key, CachedBinarySig { ino, mtime, size, major, minor, rev: CACHE_REV });
            if found.is_some() {
                return found;
            }
        }
        None
    }

    /// "Unreal Engine 5.6", or just "Unreal Engine" when the version cannot be determined.
    pub fn engine_label(ue_bin: &Path) -> String {
        match Self::detect_ue_version_opt(ue_bin) {
            Some((maj, min)) => format!("Unreal Engine {}.{}", maj, min),
            None => "Unreal Engine".to_string(),
        }
    }

    pub fn resolve_executable(exe_path: &Path) -> Option<GameEntry> {
        if !exe_path.exists() {
            return None;
        }
        let parent = exe_path.parent()?;
        let mut game_dir = parent.to_path_buf();
        if parent.file_name()?.to_string_lossy().eq_ignore_ascii_case("win64") {
            if let Some(gp) = parent.parent().and_then(|p| p.parent()) {
                game_dir = gp.to_path_buf();
            }
        }
        Self::entry_for_root(exe_path, &game_dir)
    }

    /// Build a game entry for `exe_path` whose installation lives in `game_dir`.
    pub fn entry_for_root(exe_path: &Path, game_dir: &Path) -> Option<GameEntry> {
        if !exe_path.exists() {
            return None;
        }
        let parent = exe_path.parent()?;
        let game_dir = game_dir.to_path_buf();

        let mut engine = "Standard / Non-UE".to_string();
        let mut bin_dir = parent.to_string_lossy().to_string();
        let mut shipping_exe = exe_path.to_string_lossy().to_string();

        if let Some(ub) = Self::find_ue_bin_dir(&game_dir) {
            engine = Self::engine_label(&ub);
            bin_dir = ub.to_string_lossy().to_string();
            if let Some(sh) = Self::find_shipping_exe(&ub) {
                shipping_exe = sh.to_string_lossy().to_string();
            }
        } else if game_dir.join("UnityPlayer.dll").is_file() || crate::loaders::detect(&game_dir).is_some() {
            engine = "Unity".to_string();
        } else if let Some(b) = crate::loaders::detect_bethesda(&game_dir) {
            engine = format!("Bethesda ({})", b.game);
        } else if let Some(s) = crate::loaders::detect_source(&game_dir) {
            engine = if s.is_goldsrc { "GoldSrc".to_string() } else { "Source Engine".to_string() };
        } else if crate::loaders::detect_godot(&game_dir).is_some() {
            engine = "Godot".to_string();
        }

        let stem = exe_path.file_stem()?.to_string_lossy().to_string();
        let clean_name = stem
        .replace("-Win64-Shipping", "")
        .replace("UE5", "")
        .replace('_', " ")
        .trim()
        .to_string();

        let final_name = if clean_name.is_empty() { stem } else { clean_name };
        let auto_steam_id = if final_name.to_lowercase().contains("half") {
            "2397300".to_string()
        } else {
            String::new()
        };

        let compatdata = home_dir()
            .join(".local/share/Steam/steamapps/compatdata/protonrig_custom")
            .to_string_lossy()
            .to_string();
        let pfx = format!("{}/pfx", compatdata);

        let mut entry = GameEntry {
            appid: format!("custom_{}", fsutil::now_ms()),
            name: final_name,
            game_dir: game_dir.to_string_lossy().to_string(),
            exe_path: exe_path.to_string_lossy().to_string(),
            shipping_exe,
            bin_dir,
            engine,
            has_pfx: PathBuf::from(&pfx).is_dir(),
            compatdata,
            pfx,
            is_custom: true,
            is_installed: true,
            steam_appid: auto_steam_id,
            launch_args: String::new(),
            env_vars: String::new(),
            trainer_name: String::new(),
            mod_count: 0, source: String::new(), proton_path: String::new(), ue4ss: false, loader: String::new(), cover_id: String::new(),
        };
        entry.mod_count = ModManager::list_mods(&entry).len();
        entry.refresh_flags();
        Some(entry)
    }

    pub fn scan_all_games(paths: &RigPaths, cfg: &RigConfigData) -> Vec<GameEntry> {
        let s_root = Self::find_steam_root();
        let libs = Self::parse_libraries(s_root.as_deref(), &cfg.extra_libraries);
        Self::scan_libraries(&libs, paths, cfg)
    }

    /// Scan the given `steamapps` directories (plus the user's custom games).
    pub fn scan_libraries(libs: &[PathBuf], paths: &RigPaths, cfg: &RigConfigData) -> Vec<GameEntry> {
        Self::load_disk_cache(&paths.engine_cache_file);

        let mut acf_paths = Vec::new();

        for lib in libs {
            if let Ok(rd) = fs::read_dir(lib) {
                for entry in rd.flatten() {
                    let fname = entry.file_name().to_string_lossy().to_string();
                    if fname.starts_with("appmanifest_") && fname.ends_with(".acf") {
                        acf_paths.push((lib.clone(), entry.path()));
                    }
                }
            }
        }

        let raw_steam_games: Vec<GameEntry> = acf_paths
        .par_iter()
        .filter_map(|(lib, acf)| {
            let txt = fs::read_to_string(acf).ok()?;
            let appid = RE_ACF_ID.captures(&txt)?.get(1)?.as_str().to_string();
            let name = RE_ACF_NAME.captures(&txt)?.get(1)?.as_str().to_string();
            let low = name.to_lowercase();
            if low.contains("proton")
                || low.contains("steam linux runtime")
                || low.contains("steamworks common")
                {
                    return None;
                }

                let idir = RE_ACF_DIR
                .captures(&txt)
                .and_then(|c| c.get(1))
                .map(|m| m.as_str());
            let gdir = idir.map(|d| lib.join("common").join(d)).unwrap_or_default();
            let is_installed = gdir.is_dir();
            let compatdata = lib.join("compatdata").join(&appid);
            let pfx = compatdata.join("pfx");

            let mut engine = "Standard / Non-UE".to_string();
            let mut bin_dir = String::new();
            let mut shipping_exe = String::new();

            if is_installed {
                if let Some(ub) = Self::find_ue_bin_dir(&gdir) {
                    engine = Self::engine_label(&ub);
                    bin_dir = ub.to_string_lossy().to_string();
                    if let Some(sh) = Self::find_shipping_exe(&ub) {
                        shipping_exe = sh.to_string_lossy().to_string();
                    }
                } else if gdir.join("UnityPlayer.dll").is_file() || crate::loaders::detect(&gdir).is_some() {
                    engine = "Unity".to_string();
                } else if let Some(b) = crate::loaders::detect_bethesda(&gdir) {
                    engine = format!("Bethesda ({})", b.game);
                } else if let Some(s) = crate::loaders::detect_source(&gdir) {
                    engine = if s.is_goldsrc { "GoldSrc".to_string() } else { "Source Engine".to_string() };
                } else if crate::loaders::detect_godot(&gdir).is_some() {
                    engine = "Godot".to_string();
                }
            }

            let lopts = cfg.launch_options.get(&appid).cloned().unwrap_or_default();
            let trainer_name = cfg
            .trainers
            .get(&appid)
            .and_then(|p| {
                PathBuf::from(p)
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
            })
            .unwrap_or_default();

            let mut entry = GameEntry {
                appid,
                name,
                game_dir: gdir.to_string_lossy().to_string(),
                    exe_path: shipping_exe.clone(),
                    shipping_exe,
                    bin_dir,
                    engine,
                    compatdata: compatdata.to_string_lossy().to_string(),
                    has_pfx: pfx.is_dir(),
                    pfx: pfx.to_string_lossy().to_string(),
                    is_custom: false,
                    is_installed,
                    steam_appid: String::new(),
                    launch_args: lopts.launch_args,
                    env_vars: lopts.env_vars,
                    trainer_name,
                    mod_count: 0, source: String::new(), proton_path: String::new(), ue4ss: false, loader: String::new(), cover_id: String::new(),
            };
            if is_installed && (entry.engine.starts_with("Unreal") || entry.engine == "Unity" || entry.engine.starts_with("Bethesda") || entry.engine == "Godot" || entry.engine == "Source Engine" || entry.engine == "GoldSrc") {
                entry.mod_count = ModManager::list_mods(&entry).len();
                entry.refresh_flags();
            }
            Some(entry)
        })
        .collect();

        let mut games = Vec::new();
        let mut seen = HashSet::new();
        for g in raw_steam_games {
            if seen.insert(g.appid.clone()) {
                games.push(g);
            }
        }

        {
            if let Some(custom_list) = crate::fsutil::read_json_kept::<Vec<GameEntry>>(&paths.custom_games_file).value() {
                for mut cg in custom_list {
                    cg.is_custom = true;
                    let gpath = PathBuf::from(&cg.game_dir);
                    cg.is_installed = gpath.is_dir() || PathBuf::from(&cg.exe_path).is_file();
                    if cg.is_installed && gpath.is_dir() {
                        if let Some(ub) = Self::find_ue_bin_dir(&gpath) {
                            cg.engine = Self::engine_label(&ub);
                            cg.bin_dir = ub.to_string_lossy().to_string();
                            if let Some(sh) = Self::find_shipping_exe(&ub) {
                                cg.shipping_exe = sh.to_string_lossy().to_string();
                            }
                        } else if gpath.join("UnityPlayer.dll").is_file() || crate::loaders::detect(&gpath).is_some() {
                            cg.engine = "Unity".to_string();
                        } else if let Some(b) = crate::loaders::detect_bethesda(&gpath) {
                            cg.engine = format!("Bethesda ({})", b.game);
                        } else if let Some(s) = crate::loaders::detect_source(&gpath) {
                            cg.engine = if s.is_goldsrc { "GoldSrc".to_string() } else { "Source Engine".to_string() };
                        } else if crate::loaders::detect_godot(&gpath).is_some() {
                            cg.engine = "Godot".to_string();
                        }
                    }
                    if let Some(lopts) = cfg.launch_options.get(&cg.appid) {
                        if cg.launch_args.is_empty() {
                            cg.launch_args = lopts.launch_args.clone();
                        }
                        if cg.env_vars.is_empty() {
                            cg.env_vars = lopts.env_vars.clone();
                        }
                    }
                    cg.has_pfx = PathBuf::from(&cg.pfx).is_dir();
                    cg.trainer_name = cfg
                    .trainers
                    .get(&cg.appid)
                    .and_then(|p| {
                        PathBuf::from(p)
                        .file_name()
                        .map(|n| n.to_string_lossy().to_string())
                    })
                    .unwrap_or_default();
                    cg.mod_count = ModManager::list_mods(&cg).len();
                    cg.refresh_flags();
                    games.push(cg);
                }
            }
        }

        if let Some(hd) = &paths.hydra_dir {
            for mut hg in crate::hydra::library_games(hd) {
                let dup = games.iter().any(|g| {
                    (!g.exe_path.is_empty() && g.exe_path == hg.exe_path)
                        || (!g.game_dir.is_empty() && g.game_dir == hg.game_dir)
                });
                let steam_twin = !hg.is_installed
                    && !hg.steam_appid.is_empty()
                    && games.iter().any(|g| g.appid == hg.steam_appid || g.steam_appid == hg.steam_appid);
                if dup || steam_twin {
                    continue; // already listed as a Steam or custom game; that entry wins
                }
                if let Some(lopts) = cfg.launch_options.get(&hg.appid) {
                    hg.launch_args = lopts.launch_args.clone();
                    hg.env_vars = lopts.env_vars.clone();
                }
                hg.trainer_name = cfg
                    .trainers
                    .get(&hg.appid)
                    .and_then(|p| PathBuf::from(p).file_name().map(|n| n.to_string_lossy().to_string()))
                    .unwrap_or_default();
                hg.mod_count = ModManager::list_mods(&hg).len();
                hg.refresh_flags();
                games.push(hg);
            }
        }

        for mut lg in paths.launchers.games() {
            let dup = games.iter().any(|g| {
                (!g.exe_path.is_empty() && g.exe_path == lg.exe_path) || (!g.game_dir.is_empty() && g.game_dir == lg.game_dir)
            });
            if dup {
                continue;
            }
            if let Some(lopts) = cfg.launch_options.get(&lg.appid) {
                lg.launch_args = lopts.launch_args.clone();
                lg.env_vars = lopts.env_vars.clone();
            }
            lg.trainer_name = cfg
                .trainers
                .get(&lg.appid)
                .and_then(|p| PathBuf::from(p).file_name().map(|n| n.to_string_lossy().to_string()))
                .unwrap_or_default();
            lg.mod_count = ModManager::list_mods(&lg).len();
            lg.refresh_flags();
            games.push(lg);
        }

        Self::save_disk_cache(&paths.engine_cache_file);
        games.retain(|g| !cfg.hidden_games.contains_key(&g.appid));
        for g in &mut games {
            if let Some(c) = cfg.cover_appid.get(&g.appid).filter(|c| c.as_str() != "0") {
                g.cover_id = c.clone();
                if g.steam_appid.is_empty() {
                    g.steam_appid = c.clone();
                }
            }
        }
        games.sort_by_cached_key(|g| g.name.to_lowercase());
        games
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fsutil::testutil::TempDir;

    #[test]
    fn version_scanner_handles_ascii_utf16_and_garbage() {
        let mut ascii = vec![0u8; 64];
        ascii.extend_from_slice(b"++UE5+Release-5.4.2 ");
        assert_eq!(RigEngine::scan_ue_version(&ascii), Some(("5".into(), "4".into())));

        let mut ue4 = b"xx UE4+Release-4.27 yy".to_vec();
        ue4.extend_from_slice(&[0; 8]);
        assert_eq!(RigEngine::scan_ue_version(&ue4), Some(("4".into(), "27".into())));

        let mut wide = vec![0u8; 32];
        for ch in "UE5+Release-5.6".encode_utf16() {
            wide.extend_from_slice(&ch.to_le_bytes());
        }
        wide.extend_from_slice(&[0; 8]);
        assert_eq!(RigEngine::scan_ue_version(&wide), Some(("5".into(), "6".into())));

        // truncated right after the marker, wrong engine prefix, non-UE strings: no panic, no match
        assert_eq!(RigEngine::scan_ue_version(b"UE5+Release-"), None);
        assert_eq!(RigEngine::scan_ue_version(b"UE5+Release-5"), None);
        assert_eq!(RigEngine::scan_ue_version(b"XXX+Release-5.4"), None);
        assert_eq!(RigEngine::scan_ue_version(b"+Release-"), None);
        assert_eq!(RigEngine::scan_ue_version(b""), None);
    }

    fn fixed_file_info(ms: u32) -> Vec<u8> {
        let mut v = vec![0xBD, 0x04, 0xEF, 0xFE];
        v.extend_from_slice(&0x0001_0000u32.to_le_bytes()); // struct version
        v.extend_from_slice(&ms.to_le_bytes()); // file version (major<<16 | minor)
        v.extend_from_slice(&[0u8; 20]);
        v
    }

    #[test]
    fn pe_version_resource_gives_the_engine_version_when_no_release_string_exists() {
        let mut exe = vec![0x90u8; 5000];
        exe.extend(fixed_file_info((5 << 16) | 6));
        exe.extend_from_slice(&[0x11; 300]);
        assert_eq!(RigEngine::scan_pe_version(&exe), Some(("5".into(), "6".into())));
        // an app/project stamped with its own version number is not an engine version
        assert_eq!(RigEngine::scan_pe_version(&fixed_file_info(1 << 16)), None);
        assert_eq!(RigEngine::scan_pe_version(&fixed_file_info((5 << 16) | 99)), None);
        // a stray signature before the real resource is skipped; truncated data never panics
        let mut tricky = vec![0xBD, 0x04, 0xEF, 0xFE, 1, 2, 3];
        tricky.extend(fixed_file_info((4 << 16) | 27));
        assert_eq!(RigEngine::scan_pe_version(&tricky), Some(("4".into(), "27".into())));
        assert_eq!(RigEngine::scan_pe_version(&exe[..5002]), None);
        assert_eq!(RigEngine::scan_pe_version(b""), None);

        // end to end through the file-based detector: no UE+Release string, only the resource
        let d = TempDir::new("pever");
        let mut file = vec![0u8; 2048];
        file.extend(fixed_file_info((5 << 16) | 6));
        fs::write(d.join("Game-Win64-Shipping.exe"), file).unwrap();
        assert_eq!(RigEngine::detect_ue_version_opt(d.path()), Some(("5".into(), "6".into())));
        assert_eq!(RigEngine::engine_label(d.path()), "Unreal Engine 5.6");
        // a release string, when present, takes precedence over the resource
        let mut both = b"..++UE5+Release-5.3..".to_vec();
        both.extend(fixed_file_info((5 << 16) | 6));
        let d2 = TempDir::new("pever2");
        fs::write(d2.join("G-Win64-Shipping.exe"), both).unwrap();
        assert_eq!(RigEngine::detect_ue_version_opt(d2.path()), Some(("5".into(), "3".into())));
    }

    #[test]
    fn the_real_subnautica_2_executable_reports_5_6_when_it_is_installed() {
        let exe = std::path::Path::new("/mnt/games/SteamLibrary/steamapps/common/Subnautica2/Subnautica2/Binaries/Win64");
        if !exe.is_dir() {
            eprintln!("skipping: Subnautica 2 not installed here");
            return;
        }
        assert_eq!(RigEngine::detect_ue_version_opt(exe), Some(("5".into(), "6".into())));
    }

    #[test]
    fn detect_version_uses_and_validates_cache() {
        let d = TempDir::new("detect");
        let exe = d.join("Game-Win64-Shipping.exe");
        fs::write(&exe, b"....++UE5+Release-5.5....").unwrap();
        assert_eq!(RigEngine::detect_ue_version_opt(d.path()), Some(("5".into(), "5".into())));
        // cached entry is keyed on inode/mtime/size: changing the file busts it
        fs::write(&exe, b"....++UE5+Release-5.1.........").unwrap();
        assert_eq!(RigEngine::detect_ue_version_opt(d.path()), Some(("5".into(), "1".into())));
        // negative results are cached and fall back to the default
        let other = TempDir::new("detect2");
        fs::write(other.join("Plain-Win64-Shipping.exe"), vec![0u8; 10_000]).unwrap();
        assert_eq!(RigEngine::detect_ue_version_opt(other.path()), None, "no information: no guess");
        assert_eq!(RigEngine::engine_label(other.path()), "Unreal Engine");
        let key = other.join("Plain-Win64-Shipping.exe").to_string_lossy().to_string();
        assert_eq!(ENGINE_CACHE_MEM.lock().unwrap().get(&key).map(|c| c.major.clone()), Some(String::new()));
        // missing dir is safe
        assert_eq!(RigEngine::detect_ue_version_opt(&d.join("nope")), None);
    }

    #[test]
    fn bin_dir_search_ignores_engine_only_inside_the_game() {
        let d = TempDir::new("bindir");
        // install path itself contains "engine" — must still be found
        let game = d.join("Unreal Engine Showcase");
        fs::create_dir_all(game.join("Engine/Binaries/Win64")).unwrap();
        fs::write(game.join("Engine/Binaries/Win64/CrashReportClient.exe"), b"x").unwrap();
        fs::create_dir_all(game.join("Proj/Binaries/Win64")).unwrap();
        fs::write(game.join("Proj/Binaries/Win64/Proj-Win64-Shipping.exe"), b"x").unwrap();
        fs::create_dir_all(game.join("Proj/Content/Deep/Binaries/Win64")).unwrap();
        let found = RigEngine::find_ue_bin_dir(&game).unwrap();
        assert!(found.ends_with("Proj/Binaries/Win64"), "{}", found.display());
        // direct Win64 path short-circuits
        assert_eq!(RigEngine::find_ue_bin_dir(&found), Some(found.clone()));
        assert_eq!(RigEngine::find_ue_bin_dir(&d.join("missing")), None);
    }

    #[test]
    fn shipping_exe_prefers_shipping_then_size_and_skips_helpers() {
        let d = TempDir::new("ship");
        fs::write(d.join("Launcher.exe"), vec![0u8; 5000]).unwrap();
        fs::write(d.join("CrashReportClient.exe"), vec![0u8; 9000]).unwrap();
        fs::write(d.join("G-Win64-Shipping.exe"), vec![0u8; 100]).unwrap();
        assert!(RigEngine::find_shipping_exe(d.path()).unwrap().ends_with("G-Win64-Shipping.exe"));
        fs::remove_file(d.join("G-Win64-Shipping.exe")).unwrap();
        assert!(RigEngine::find_shipping_exe(d.path()).unwrap().ends_with("Launcher.exe"));
    }

    #[test]
    fn libraries_and_custom_games_are_scanned_in_parallel_safely() {
        let d = TempDir::new("libs");
        let lib = d.join("steamapps");
        for i in 0..40 {
            let dir = lib.join(format!("common/G{}", i));
            fs::create_dir_all(dir.join("Binaries/Win64")).unwrap();
            fs::write(
                lib.join(format!("appmanifest_{}.acf", 1000 + i)),
                format!("\"AppState\"\n{{\n\"appid\" \"{}\"\n\"name\" \"Game {:02}\"\n\"installdir\" \"G{}\"\n}}\n", 1000 + i, i, i),
            )
            .unwrap();
        }
        // tools that must be hidden
        fs::write(lib.join("appmanifest_1.acf"), "\"AppState\"\n{\n\"appid\" \"1\"\n\"name\" \"Proton 9.0\"\n\"installdir\" \"P\"\n}\n").unwrap();
        // a custom game from disk
        let paths = RigPaths {
            custom_games_file: d.join("custom.json"),
            config_file: d.join("cfg.json"),
            cache_dir: d.join("cache/injectors"),
            engine_cache_file: d.join("cache/engine.json"),
            webview_profile_dir: d.join("cache/wv"),
            hydra_dir: None,
            launchers: Default::default(),
        };
        let custom = GameEntry {
            appid: "custom_1".into(), name: "Aardvark".into(), game_dir: d.join("custom").to_string_lossy().into(),
            exe_path: String::new(), shipping_exe: String::new(), bin_dir: String::new(), engine: default_engine(),
            compatdata: String::new(), pfx: String::new(), has_pfx: false, is_custom: true, is_installed: true,
            steam_appid: String::new(), launch_args: String::new(), env_vars: String::new(),
            trainer_name: String::new(), mod_count: 0, source: String::new(), proton_path: String::new(), ue4ss: false, loader: String::new(), cover_id: String::new(),
        };
        fs::create_dir_all(&custom.game_dir).unwrap();
        fs::write(&paths.custom_games_file, serde_json::to_string(&vec![custom]).unwrap()).unwrap();
        let games = RigEngine::scan_libraries(&[lib], &paths, &RigConfigData::default());
        assert_eq!(games.len(), 41, "40 steam games + 1 custom, Proton hidden");
        assert_eq!(games[0].name, "Aardvark");
        assert!(games.windows(2).all(|w| w[0].name.to_lowercase() <= w[1].name.to_lowercase()));
        // engine cache written atomically and loadable
        assert!(paths.engine_cache_file.parent().unwrap().is_dir());
        RigEngine::save_disk_cache(&paths.engine_cache_file);
        let txt = fs::read_to_string(&paths.engine_cache_file).unwrap();
        serde_json::from_str::<HashMap<String, CachedBinarySig>>(&txt).unwrap();
    }

    #[test]
    fn extra_libraries_may_be_a_library_folder_or_a_steamapps_folder() {
        let d = TempDir::new("extralibs");
        fs::create_dir_all(d.join("a/steamapps")).unwrap();
        fs::create_dir_all(d.join("b/steamapps")).unwrap();
        fs::create_dir_all(d.join("c")).unwrap(); // no steamapps inside: ignored
        let extra = vec![
            d.join("a").to_string_lossy().to_string(),
            d.join("b/steamapps").to_string_lossy().to_string(),
            d.join("c").to_string_lossy().to_string(),
            d.join("missing").to_string_lossy().to_string(),
        ];
        let libs = RigEngine::parse_libraries(None, &extra);
        let names: Vec<_> = libs.iter().map(|p| p.parent().unwrap().file_name().unwrap().to_string_lossy().to_string()).collect();
        assert_eq!(names, vec!["a", "b"], "{libs:?}");
        assert!(RigEngine::parse_libraries(None, &[]).is_empty());
    }

    #[test]
    fn resolve_executable_generates_distinct_ids() {
        let d = TempDir::new("resolve");
        let exe = d.join("My_Game-Win64-Shipping.exe");
        fs::write(&exe, b"MZ").unwrap();
        let a = RigEngine::resolve_executable(&exe).unwrap();
        std::thread::sleep(Duration::from_millis(3));
        let b = RigEngine::resolve_executable(&exe).unwrap();
        assert_ne!(a.appid, b.appid);
        assert_eq!(a.name, "My Game");
        assert!(RigEngine::resolve_executable(&d.join("none.exe")).is_none());
    }

    #[test]
    fn refresh_flags_recognizes_godot_bethesda_source_and_loaders() {
        let d = TempDir::new("flags_test");

        // Godot game with GDWeave
        let godot_dir = d.join("GodotGame");
        fs::create_dir_all(godot_dir.join("GDWeave/mods")).unwrap();
        fs::write(godot_dir.join("game.exe"), b"MZ").unwrap();
        fs::write(godot_dir.join("game.pck"), b"GDPCK").unwrap();

        let mut g_entry = GameEntry {
            appid: "godot_1".into(),
            name: "Godot Game".into(),
            game_dir: godot_dir.to_string_lossy().into(),
            exe_path: String::new(),
            shipping_exe: String::new(),
            bin_dir: String::new(),
            engine: default_engine(),
            compatdata: String::new(),
            pfx: String::new(),
            has_pfx: false,
            is_custom: true,
            is_installed: true,
            steam_appid: String::new(),
            launch_args: String::new(),
            env_vars: String::new(),
            trainer_name: String::new(),
            mod_count: 0,
            source: String::new(),
            proton_path: String::new(),
            ue4ss: false,
            loader: String::new(),
            cover_id: String::new(),
        };
        g_entry.refresh_flags();
        assert_eq!(g_entry.engine, "Godot");
        assert_eq!(g_entry.loader, "gdweave");

        // Bethesda Skyrim game with SKSE64
        let skyrim_dir = d.join("SkyrimGame");
        fs::create_dir_all(skyrim_dir.join("Data")).unwrap();
        fs::write(skyrim_dir.join("SkyrimSE.exe"), b"MZ").unwrap();
        fs::write(skyrim_dir.join("skse64_loader.exe"), b"MZ").unwrap();
        fs::write(skyrim_dir.join("Data/Skyrim.esm"), b"TES4").unwrap();

        let mut b_entry = GameEntry {
            appid: "skyrim_1".into(),
            name: "Skyrim SE".into(),
            game_dir: skyrim_dir.to_string_lossy().into(),
            exe_path: String::new(),
            shipping_exe: String::new(),
            bin_dir: String::new(),
            engine: default_engine(),
            compatdata: String::new(),
            pfx: String::new(),
            has_pfx: false,
            is_custom: true,
            is_installed: true,
            steam_appid: String::new(),
            launch_args: String::new(),
            env_vars: String::new(),
            trainer_name: String::new(),
            mod_count: 0,
            source: String::new(),
            proton_path: String::new(),
            ue4ss: false,
            loader: String::new(),
            cover_id: String::new(),
        };
        b_entry.refresh_flags();
        assert_eq!(b_entry.engine, "Bethesda (Skyrim Special Edition)");
        assert_eq!(b_entry.loader, "skse64");

        // Source game with sourcemod
        let hl2_dir = d.join("HL2Game");
        fs::create_dir_all(hl2_dir.join("hl2/addons/sourcemod")).unwrap();
        fs::write(hl2_dir.join("hl2.exe"), b"MZ").unwrap();
        fs::write(hl2_dir.join("hl2/gameinfo.txt"), b"GameInfo").unwrap();

        let mut s_entry = GameEntry {
            appid: "hl2_1".into(),
            name: "Half-Life 2".into(),
            game_dir: hl2_dir.to_string_lossy().into(),
            exe_path: String::new(),
            shipping_exe: String::new(),
            bin_dir: String::new(),
            engine: default_engine(),
            compatdata: String::new(),
            pfx: String::new(),
            has_pfx: false,
            is_custom: true,
            is_installed: true,
            steam_appid: String::new(),
            launch_args: String::new(),
            env_vars: String::new(),
            trainer_name: String::new(),
            mod_count: 0,
            source: String::new(),
            proton_path: String::new(),
            ue4ss: false,
            loader: String::new(),
            cover_id: String::new(),
        };
        s_entry.refresh_flags();
        assert_eq!(s_entry.engine, "Source Engine");
        assert_eq!(s_entry.loader, "sourcemod");
    }
}
