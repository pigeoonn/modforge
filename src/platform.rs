//! Everything that differs between Linux, Windows and macOS in one place: where Gantry keeps its
//! files, where Steam lives, how a folder or link is opened, how a desktop notification is shown
//! and which browser can host the app window.
//!
//! Linux keeps the folders earlier versions used, so nothing moves for existing users.

use crate::fsutil::{find_in_path, home_dir};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

pub const IS_WINDOWS: bool = cfg!(windows);
pub const IS_MAC: bool = cfg!(target_os = "macos");

/// "linux", "windows" or "macos": what the interface uses to hide what only exists on Linux.
pub const OS: &str = if cfg!(windows) {
    "windows"
} else if cfg!(target_os = "macos") {
    "macos"
} else {
    "linux"
};

/// True on a Steam Deck (or in its game mode): the maker's own signs, none of which a PC has.
/// `product` is the board name from the firmware ("Jupiter" is the first Deck, "Galileo" the OLED one),
/// `os_release` the text of /etc/os-release.
pub fn looks_like_deck(steam_deck_env: Option<&str>, product: &str, os_release: &str, gamescope: bool) -> bool {
    steam_deck_env == Some("1")
        || gamescope
        || matches!(product.trim(), "Jupiter" | "Galileo")
        || os_release.lines().any(|l| l.trim().replace('"', "").eq_ignore_ascii_case("ID=steamos"))
}

/// Whether this computer is a Steam Deck. Only Linux can be one; the answer is looked up once.
pub fn is_steam_deck() -> bool {
    static DECK: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *DECK.get_or_init(|| {
        if cfg!(test) || !cfg!(target_os = "linux") {
            return false;
        }
        let read = |p: &str| std::fs::read_to_string(p).unwrap_or_default();
        looks_like_deck(
            std::env::var("SteamDeck").ok().as_deref(),
            &read("/sys/devices/virtual/dmi/id/product_name"),
            &read("/etc/os-release"),
            std::env::var_os("GAMESCOPE_WAYLAND_DISPLAY").is_some(),
        )
    })
}

fn env_path(var: &str) -> Option<PathBuf> {
    std::env::var_os(var).filter(|v| !v.is_empty()).map(PathBuf::from)
}

/// Settings and the list of custom games.
pub fn config_dir() -> PathBuf {
    let home = home_dir();
    if IS_WINDOWS {
        env_path("APPDATA").unwrap_or_else(|| home.join("AppData/Roaming")).join("Gantry")
    } else if IS_MAC {
        home.join("Library/Application Support/Gantry")
    } else {
        home.join(".config/protonrig")
    }
}

/// Caches, the log and the browser profile of the app window.
pub fn cache_root() -> PathBuf {
    let home = home_dir();
    if IS_WINDOWS {
        env_path("LOCALAPPDATA").unwrap_or_else(|| home.join("AppData/Local")).join("Gantry/cache")
    } else if IS_MAC {
        home.join("Library/Caches/Gantry")
    } else {
        home.join(".cache/rig")
    }
}

/// Where a mod library goes when the game's own folder cannot hold it.
pub fn data_dir() -> PathBuf {
    let home = home_dir();
    if IS_WINDOWS {
        env_path("LOCALAPPDATA").unwrap_or_else(|| home.join("AppData/Local")).join("Gantry/data")
    } else if IS_MAC {
        home.join("Library/Application Support/Gantry/data")
    } else {
        home.join(".local/share/gantry")
    }
}

/// The folder Hydra, Heroic and similar launchers keep their settings in.
pub fn app_settings_dir() -> PathBuf {
    let home = home_dir();
    if IS_WINDOWS {
        env_path("APPDATA").unwrap_or_else(|| home.join("AppData/Roaming"))
    } else if IS_MAC {
        home.join("Library/Application Support")
    } else {
        home.join(".config")
    }
}

/// Possible Steam installations, most likely first.
pub fn steam_roots() -> Vec<PathBuf> {
    let home = home_dir();
    if IS_WINDOWS {
        let mut v: Vec<PathBuf> = windows_steam_path().into_iter().collect();
        for var in ["ProgramFiles(x86)", "ProgramFiles"] {
            if let Some(p) = env_path(var) {
                v.push(p.join("Steam"));
            }
        }
        v.push(PathBuf::from(r"C:\Program Files (x86)\Steam"));
        v.dedup();
        v
    } else if IS_MAC {
        vec![home.join("Library/Application Support/Steam")]
    } else {
        vec![
            home.join(".local/share/Steam"),
            home.join(".steam/steam"),
            home.join(".steam/root"),
            home.join(".var/app/com.valvesoftware.Steam/.local/share/Steam"),
        ]
    }
}

/// Value of a registry entry (`reg query`), Windows only.
pub fn registry_value(key: &str, name: &str) -> Option<String> {
    if !IS_WINDOWS {
        return None;
    }
    let out = Command::new("reg").args(["query", key, "/v", name]).stdin(Stdio::null()).stderr(Stdio::null()).output().ok()?;
    parse_reg_query(&String::from_utf8_lossy(&out.stdout), name)
}

/// The value part of `    Name    REG_SZ    data` lines.
pub fn parse_reg_query(text: &str, name: &str) -> Option<String> {
    text.lines().find_map(|l| {
        let l = l.trim();
        let rest = l.strip_prefix(name)?.trim_start();
        let (kind, data) = rest.split_once(char::is_whitespace)?;
        kind.starts_with("REG_").then(|| data.trim().to_string()).filter(|d| !d.is_empty())
    })
}

fn windows_steam_path() -> Option<PathBuf> {
    registry_value(r"HKCU\Software\Valve\Steam", "SteamPath").map(|p| PathBuf::from(p.replace('/', "\\")))
}

/// Text in the format of Steam's `registry.vdf` that holds the logged-in account's name, so one
/// parser serves every system.
pub fn steam_registry_text() -> String {
    if IS_WINDOWS {
        return registry_value(r"HKCU\Software\Valve\Steam", "AutoLoginUser").map(|u| format!("\"AutoLoginUser\"\t\t\"{u}\"\n")).unwrap_or_default();
    }
    let home = home_dir();
    [home.join(".steam/registry.vdf"), home.join(".local/share/Steam/registry.vdf")].iter().find_map(|p| std::fs::read_to_string(p).ok()).unwrap_or_default()
}

/// Open a web link, a `steam://` link or a folder with whatever the system uses for it.
pub fn open(target: impl AsRef<std::ffi::OsStr>) -> bool {
    let target = target.as_ref();
    let mut c = if IS_WINDOWS {
        // no shell involved, so nothing in a path or link is interpreted
        let is_dir = Path::new(target).is_dir();
        let mut c = Command::new(if is_dir { "explorer" } else { "rundll32" });
        if !is_dir {
            c.arg("url.dll,FileProtocolHandler");
        }
        c
    } else if IS_MAC {
        Command::new("open")
    } else {
        Command::new("xdg-open")
    };
    c.arg(target).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
    match c.spawn() {
        Ok(mut child) => {
            std::thread::spawn(move || {
                let _ = child.wait();
            });
            true
        }
        Err(_) => false,
    }
}

/// Say why Gantry cannot start, where the user will see it: a message box on Windows (there is
/// no console), a notification elsewhere.
pub fn fatal_notice(title: &str, body: &str) {
    #[cfg(windows)]
    {
        use windows_sys::Win32::UI::WindowsAndMessaging::{MessageBoxW, MB_ICONERROR, MB_OK};
        let wide = |s: &str| s.encode_utf16().chain(Some(0)).collect::<Vec<u16>>();
        unsafe {
            MessageBoxW(std::ptr::null_mut(), wide(body).as_ptr(), wide(title).as_ptr(), MB_OK | MB_ICONERROR);
        }
    }
    #[cfg(not(windows))]
    notify(title, body);
}

/// A program started from a terminal on Windows has no console of its own (the app is built
/// without one so double-clicking it shows no black window); borrow the terminal's so `--help`
/// and friends can print.
pub fn attach_parent_console() {
    #[cfg(windows)]
    unsafe {
        windows_sys::Win32::System::Console::AttachConsole(windows_sys::Win32::System::Console::ATTACH_PARENT_PROCESS);
    }
}

/// Without a console of its own, every program Gantry starts (curl, tar, reg...) would flash a black
/// window on Windows. Giving Gantry a hidden console once makes them all use that one instead.
pub fn hide_console_for_children() {
    #[cfg(windows)]
    unsafe {
        use windows_sys::Win32::System::Console::{AllocConsole, GetConsoleWindow};
        use windows_sys::Win32::UI::WindowsAndMessaging::{ShowWindow, SW_HIDE};
        if GetConsoleWindow().is_null() && AllocConsole() != 0 {
            let w = GetConsoleWindow();
            if !w.is_null() {
                ShowWindow(w, SW_HIDE);
            }
        }
    }
}

/// PowerShell code that shows a Windows toast (it borrows PowerShell's own application identity, the
/// only one a program without an installer can use).
pub fn toast_script(title: &str, body: &str) -> String {
    let q = |s: &str| s.replace('\'', "''").replace(['\r', '\n'], " ");
    format!(
        "$app='{{1AC14E77-02E7-4E5D-B744-2EB1AE5198B7}}\\WindowsPowerShell\\v1.0\\powershell.exe';\
[Windows.UI.Notifications.ToastNotificationManager, Windows.UI.Notifications, ContentType = WindowsRuntime] | Out-Null;\
$t=[Windows.UI.Notifications.ToastNotificationManager]::GetTemplateContent([Windows.UI.Notifications.ToastTemplateType]::ToastText02);\
$n=$t.GetElementsByTagName('text');\
$n.Item(0).AppendChild($t.CreateTextNode('{}')) | Out-Null;\
$n.Item(1).AppendChild($t.CreateTextNode('{}')) | Out-Null;\
[Windows.UI.Notifications.ToastNotificationManager]::CreateToastNotifier($app).Show([Windows.UI.Notifications.ToastNotification]::new($t))",
        q(title),
        q(body)
    )
}

/// A desktop notification, when the system has a simple way to show one.
pub fn notify(title: &str, body: &str) {
    let mut c = if IS_MAC {
        let esc = |s: &str| s.replace('\\', "\\\\").replace('"', "\\\"");
        let mut c = Command::new("osascript");
        c.arg("-e").arg(format!("display notification \"{}\" with title \"{}\"", esc(body), esc(title)));
        c
    } else if IS_WINDOWS {
        let mut c = Command::new("powershell");
        c.args(["-NoProfile", "-NonInteractive", "-WindowStyle", "Hidden", "-Command", &toast_script(title, body)]);
        c
    } else {
        let mut c = Command::new("notify-send");
        c.args(["-a", "ModForge", title, body]);
        c
    };
    let _ = c.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).status();
}

/// Chromium based browsers that can show the interface as an app window, best first.
pub fn browsers() -> Vec<PathBuf> {
    let mut out = Vec::new();
    if IS_WINDOWS {
        let roots: Vec<PathBuf> = ["ProgramFiles", "ProgramFiles(x86)", "LOCALAPPDATA"].iter().filter_map(|v| env_path(v)).collect();
        for rel in [
            r"Microsoft\Edge\Application\msedge.exe",
            r"BraveSoftware\Brave-Browser\Application\brave.exe",
            r"Google\Chrome\Application\chrome.exe",
            r"Chromium\Application\chrome.exe",
            r"Vivaldi\Application\vivaldi.exe",
        ] {
            out.extend(roots.iter().map(|r| r.join(rel)).filter(|p| p.is_file()));
        }
    } else if IS_MAC {
        for app in ["Brave Browser", "Google Chrome", "Microsoft Edge", "Chromium", "Vivaldi"] {
            let p = PathBuf::from(format!("/Applications/{app}.app/Contents/MacOS/{app}"));
            if p.is_file() {
                out.push(p);
            }
        }
    } else {
        for b in ["brave", "brave-browser", "chromium", "chromium-browser", "google-chrome-stable", "google-chrome", "thorium-browser", "vivaldi", "microsoft-edge-stable"] {
            if let Some(p) = find_in_path(b) {
                out.push(p);
            }
        }
    }
    out
}

/// Commands that can unpack a mod archive into `dest`, in the order they are tried.
pub fn extract_commands(archive: &Path, dest: &Path) -> Vec<Command> {
    let mut o_arg = std::ffi::OsString::from("-o");
    o_arg.push(dest);
    let mut list = Vec::new();
    let mut sevenzip = vec![PathBuf::from("7z")];
    if IS_WINDOWS {
        for var in ["ProgramFiles", "ProgramFiles(x86)"] {
            if let Some(p) = env_path(var) {
                sevenzip.push(p.join("7-Zip/7z.exe"));
            }
        }
    }
    for z in sevenzip {
        let mut c = Command::new(z);
        c.arg("x").arg("-y").arg(&o_arg).arg(archive);
        list.push(c);
    }
    // bsdtar ships with Arch and with Windows 10+ (as tar.exe); it handles zip/7z/rar/tar.*
    for t in if IS_WINDOWS { ["tar", "bsdtar"] } else { ["bsdtar", "tar"] } {
        let mut c = Command::new(t);
        c.arg("-xf").arg(archive).arg("-C").arg(dest);
        list.push(c);
    }
    if !IS_WINDOWS {
        let mut c = Command::new("unzip");
        c.args(["-q", "-o"]).arg(archive).arg("-d").arg(dest);
        list.push(c);
        let mut c = Command::new("unrar");
        c.args(["x", "-y"]).arg(archive).arg(format!("{}/", dest.display()));
        list.push(c);
    }
    list
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_output_is_parsed() {
        let out = "\r\nHKEY_CURRENT_USER\\Software\\Valve\\Steam\r\n    SteamPath    REG_SZ    c:/program files (x86)/steam\r\n\r\n";
        assert_eq!(parse_reg_query(out, "SteamPath").as_deref(), Some("c:/program files (x86)/steam"));
        assert_eq!(parse_reg_query(out, "AutoLoginUser"), None);
        assert_eq!(parse_reg_query("    SteamPathExtra    REG_SZ    x", "SteamPath"), None);
    }

    #[test]
    fn a_steam_deck_is_told_by_its_own_signs() {
        assert!(looks_like_deck(Some("1"), "", "", false));
        assert!(looks_like_deck(None, "Jupiter\n", "", false));
        assert!(looks_like_deck(None, "Galileo", "", false));
        assert!(looks_like_deck(None, "", "NAME=\"SteamOS\"\nID=steamos\n", false));
        assert!(looks_like_deck(None, "", "", true), "inside the Deck's game mode");
        assert!(!looks_like_deck(None, "MS-7C02", "ID=cachyos\nID_LIKE=arch\n", false));
        assert!(!looks_like_deck(Some("0"), "", "", false));
    }

    #[test]
    fn every_system_has_its_own_folders() {
        assert!(config_dir().is_absolute() || std::env::var_os("HOME").is_none());
        assert_ne!(config_dir(), cache_root());
        assert!(!steam_roots().is_empty());
        assert!(["linux", "windows", "macos"].contains(&OS));
    }

    #[test]
    fn the_toast_script_quotes_what_it_shows() {
        let s = toast_script("It's a \"title\"", "line1\nline2 O'Neil");
        assert!(s.contains("CreateTextNode('It''s a \"title\"')") && s.contains("CreateTextNode('line1 line2 O''Neil')"), "{s}");
        assert!(s.contains("1AC14E77-02E7-4E5D-B744-2EB1AE5198B7"));
        assert!(!s.contains('\n'), "one line for -Command");
    }

    #[test]
    fn archives_are_unpacked_with_a_fallback_chain() {
        let cmds = extract_commands(Path::new("a.zip"), Path::new("out"));
        assert!(cmds.len() >= 3);
        assert!(cmds.iter().any(|c| c.get_program() == if IS_WINDOWS { "tar" } else { "bsdtar" }));
    }
}
