//! `rig --install` / `rig --uninstall`: desktop-menu entry, icon and `~/.local/bin/rig` link,
//! so a rebuilt or relocated binary is one command away from being launchable by name.

use crate::fsutil::atomic_write;
use std::fs;
use std::path::{Path, PathBuf};

pub const ICON_PNG: &[u8] = include_bytes!("icon.png");
const ICON_SVG: &str = include_str!("icon.svg");

pub fn exec_quote(p: &Path) -> String {
    let s = p.to_string_lossy();
    if s.chars().any(|c| c.is_whitespace() || "\"'\\><~|&;$*?#()`".contains(c)) {
        format!("\"{}\"", s.replace('\\', "\\\\\\\\").replace('"', "\\\\\"").replace('$', "\\\\$").replace('`', "\\\\`"))
    } else {
        s.into_owned()
    }
}

pub fn desktop_entry(exe: &Path, icon: &Path) -> String {
    format!(
        "[Desktop Entry]\nType=Application\nName=Gantry\nGenericName=Mod & Trainer Hub\nComment=Mods, trainers and crash hunting for Proton games\n\
Exec={}\nIcon={}\nTerminal=false\nCategories=Game;Utility;\nKeywords=gantry;proton;mods;ue4ss;trainer;steam;hydra;\nStartupNotify=false\nStartupWMClass=Gantry\n",
        exec_quote(exe),
        icon.display()
    )
}

struct Targets {
    desktop: PathBuf,
    png: PathBuf,
    svg: PathBuf,
    link: PathBuf,
    /// Shortcut some users keep on their desktop, and what it was called before the rename.
    shortcut: PathBuf,
    old_desktop: PathBuf,
    old_png: PathBuf,
    old_svg: PathBuf,
    old_shortcut: PathBuf,
}

fn targets(home: &Path) -> Targets {
    let apps = home.join(".local/share/applications");
    let icons = home.join(".local/share/icons/hicolor");
    Targets {
        desktop: apps.join("gantry.desktop"),
        png: icons.join("256x256/apps/gantry.png"),
        svg: icons.join("scalable/apps/gantry.svg"),
        link: home.join(".local/bin/rig"),
        shortcut: home.join("Desktop/Gantry.desktop"),
        old_desktop: apps.join("protonrig.desktop"),
        old_png: icons.join("256x256/apps/protonrig.png"),
        old_svg: icons.join("scalable/apps/protonrig.svg"),
        old_shortcut: home.join("Desktop/ProtonRig.desktop"),
    }
}

fn file_has(p: &Path, needle: &str) -> bool {
    fs::read_to_string(p).map(|t| t.contains(needle)).unwrap_or(false)
}

const LINUX_ONLY: &str = "Desktop integration is only available on Linux for now.";

// ───────────────────────────── macOS ─────────────────────────────

/// An .icns file around a 256x256 PNG (the `ic08` entry holds a PNG as is).
pub fn png_to_icns(png: &[u8]) -> Vec<u8> {
    let mut out = b"icns".to_vec();
    out.extend((png.len() as u32 + 16).to_be_bytes());
    out.extend(b"ic08");
    out.extend((png.len() as u32 + 8).to_be_bytes());
    out.extend(png);
    out
}

fn mac_info_plist() -> String {
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n<plist version=\"1.0\"><dict>\n\
<key>CFBundleName</key><string>Gantry</string>\n<key>CFBundleDisplayName</key><string>Gantry</string>\n<key>CFBundleIdentifier</key><string>io.github.pigeoonn.gantry</string>\n\
<key>CFBundleExecutable</key><string>Gantry</string>\n<key>CFBundleIconFile</key><string>gantry</string>\n<key>CFBundlePackageType</key><string>APPL</string>\n\
<key>CFBundleShortVersionString</key><string>{}</string>\n<key>LSApplicationCategoryType</key><string>public.app-category.utilities</string>\n\
<key>NSHighResolutionCapable</key><true/>\n</dict></plist>\n",
        env!("CARGO_PKG_VERSION")
    )
}

fn mac_app(home: &Path) -> PathBuf {
    home.join("Applications/Gantry.app")
}

fn mac_install(home: &Path, exe: &Path) -> Result<Vec<String>, String> {
    let app = mac_app(home);
    let io = |what: &str, e: std::io::Error| format!("{}: {}", what, e);
    if !exe.is_file() {
        return Err(format!("cannot resolve {}: not found", exe.display()));
    }
    let main = app.join("Contents/MacOS/Gantry");
    if crate::fsutil::canonical(exe).ok() != crate::fsutil::canonical(&main).ok() {
        crate::fsutil::atomic_copy(exe, &main).map_err(|e| io("copying the program", e))?;
    }
    crate::fsutil::set_executable(&main).map_err(|e| io("program", e))?;
    atomic_write(&app.join("Contents/Info.plist"), mac_info_plist()).map_err(|e| io("Info.plist", e))?;
    atomic_write(&app.join("Contents/Resources/gantry.icns"), png_to_icns(ICON_PNG)).map_err(|e| io("icon", e))?;
    Ok(vec![format!("app       → {}", app.display())])
}

fn mac_uninstall(home: &Path) -> Vec<String> {
    let app = mac_app(home);
    // only a folder that is recognisably ours
    if file_has(&app.join("Contents/Info.plist"), "io.github.pigeoonn.gantry") && fs::remove_dir_all(&app).is_ok() {
        return vec![format!("removed   {}", app.display())];
    }
    Vec::new()
}

// ───────────────────────────── Windows ─────────────────────────────

/// Wrap a PNG in an ICO container (Windows reads PNG images inside .ico since Vista).
pub fn png_to_ico(png: &[u8]) -> Vec<u8> {
    let (w, h) = if png.len() > 24 { (u32::from_be_bytes([png[16], png[17], png[18], png[19]]), u32::from_be_bytes([png[20], png[21], png[22], png[23]])) } else { (256, 256) };
    let dim = |v: u32| if v >= 256 { 0u8 } else { v as u8 };
    let mut out = vec![0, 0, 1, 0, 1, 0, dim(w), dim(h), 0, 0, 1, 0, 32, 0];
    out.extend((png.len() as u32).to_le_bytes());
    out.extend(22u32.to_le_bytes());
    out.extend(png);
    out
}

/// Folder the program is copied to by `--install` on Windows, and the Start menu entry.
fn win_paths(home: &Path) -> (PathBuf, PathBuf) {
    (home.join("AppData/Local/Programs/Gantry"), home.join("AppData/Roaming/Microsoft/Windows/Start Menu/Programs/Gantry.lnk"))
}

/// PowerShell that creates a shortcut (the system has no other scriptable way to make one).
pub fn shortcut_script(lnk: &Path, target: &Path, icon: &Path, args: &str) -> String {
    let q = |p: &Path| p.to_string_lossy().replace('\'', "''");
    format!(
        "$s=(New-Object -ComObject WScript.Shell).CreateShortcut('{}');$s.TargetPath='{}';$s.Arguments='{}';$s.IconLocation='{}';$s.Description='Gantry: mods for your games';$s.Save()",
        q(lnk),
        q(target),
        args.replace('\'', "''"),
        q(icon)
    )
}

fn reg(args: &[&str]) -> bool {
    use std::process::{Command, Stdio};
    Command::new("reg").args(args).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).status().map(|s| s.success()).unwrap_or(false)
}

/// The handler `nxm://` links currently go to, as the command line registered for them.
fn windows_nxm_handler() -> Option<String> {
    use std::process::{Command, Stdio};
    let out = Command::new("reg").args(["query", r"HKCU\Software\Classes\nxm\shell\open\command", "/ve"]).stdin(Stdio::null()).stderr(Stdio::null()).output().ok()?;
    parse_reg_default(&String::from_utf8_lossy(&out.stdout))
}

/// The data of the `(Default)` value in `reg query` output (the label is translated on other
/// languages of Windows, so the line is found by its value type).
pub fn parse_reg_default(text: &str) -> Option<String> {
    text.lines().find_map(|l| {
        let (_, rest) = l.split_once("REG_SZ")?;
        Some(rest.trim().to_string()).filter(|d| !d.is_empty())
    })
}

fn windows_install(home: &Path, exe: &Path) -> Result<Vec<String>, String> {
    use std::process::{Command, Stdio};
    let (dir, lnk) = win_paths(home);
    let target = dir.join("rig.exe");
    if !exe.is_file() {
        return Err(format!("cannot resolve {}: not found", exe.display()));
    }
    let io = |what: &str, e: std::io::Error| format!("{}: {}", what, e);
    fs::create_dir_all(&dir).map_err(|e| io("program folder", e))?;
    if crate::fsutil::canonical(exe).ok() != crate::fsutil::canonical(&target).ok() {
        crate::fsutil::atomic_copy(exe, &target).map_err(|e| io("copying the program", e))?;
    }
    let ico = dir.join("gantry.ico");
    atomic_write(&ico, png_to_ico(ICON_PNG)).map_err(|e| io("icon", e))?;
    let mut steps = vec![format!("program   → {}", target.display())];
    if let Some(p) = lnk.parent() {
        fs::create_dir_all(p).map_err(|e| io("Start menu folder", e))?;
    }
    let ok = Command::new("powershell")
        .args(["-NoProfile", "-NonInteractive", "-Command", &shortcut_script(&lnk, &target, &ico, "")])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if !ok || !lnk.is_file() {
        return Err("could not create the Start menu shortcut".into());
    }
    steps.push(format!("menu      → {}", lnk.display()));
    Ok(steps)
}

fn windows_uninstall(home: &Path) -> Vec<String> {
    let (dir, lnk) = win_paths(home);
    let mut steps = Vec::new();
    if fs::remove_file(&lnk).is_ok() {
        steps.push(format!("removed   {}", lnk.display()));
    }
    // the running program cannot delete itself: everything else goes, and the folder if it can
    for f in ["gantry.ico", "rig.exe"] {
        if fs::remove_file(dir.join(f)).is_ok() {
            steps.push(format!("removed   {}", dir.join(f).display()));
        }
    }
    let _ = fs::remove_dir(&dir);
    steps
}

fn windows_register_nxm(config_dir: &Path, exe: &Path) -> Result<String, String> {
    let exe = crate::fsutil::canonical(exe).map_err(|e| format!("cannot resolve {}: {}", exe.display(), e))?;
    let exe = exe.to_string_lossy().trim_start_matches(r"\\?\").to_string();
    let previous = windows_nxm_handler().filter(|p| !p.contains("rig.exe") && !p.contains("gantry"));
    if let Some(prev) = &previous {
        atomic_write(&config_dir.join("nxm_previous_handler"), prev).map_err(|e| e.to_string())?;
    }
    let command = format!("\"{exe}\" \"%1\"");
    let ok = reg(&["add", r"HKCU\Software\Classes\nxm", "/ve", "/d", "URL:NXM Protocol", "/f"])
        && reg(&["add", r"HKCU\Software\Classes\nxm", "/v", "URL Protocol", "/d", "", "/f"])
        && reg(&["add", r"HKCU\Software\Classes\nxm\shell\open\command", "/ve", "/d", &command, "/f"]);
    if !ok {
        return Err("Windows did not accept the change.".into());
    }
    Ok(match previous {
        Some(_) => format!("Gantry now opens Nexus Mods links (it replaced {}; Unregister restores it).", "the previous program"),
        None => "Gantry now opens Nexus Mods links.".into(),
    })
}

fn windows_unregister_nxm(config_dir: &Path) -> String {
    let prev_file = config_dir.join("nxm_previous_handler");
    let prev = fs::read_to_string(&prev_file).unwrap_or_default().trim().to_string();
    if prev.is_empty() {
        reg(&["delete", r"HKCU\Software\Classes\nxm", "/f"]);
    } else {
        reg(&["add", r"HKCU\Software\Classes\nxm\shell\open\command", "/ve", "/d", &prev, "/f"]);
    }
    let _ = fs::remove_file(prev_file);
    if prev.is_empty() { "Gantry no longer handles Nexus Mods links.".into() } else { format!("Nexus Mods links go back to {}.", "the previous program") }
}

/// Remove what the app installed under its former name (ProtonRig). Only files that are
/// recognisably ours are touched.
fn remove_legacy(t: &Targets, steps: &mut Vec<String>) {
    if file_has(&t.old_desktop, "Name=ProtonRig") && fs::remove_file(&t.old_desktop).is_ok() {
        steps.push(format!("removed   old menu entry {}", t.old_desktop.display()));
    }
    for p in [&t.old_png, &t.old_svg] {
        if fs::remove_file(p).is_ok() {
            steps.push(format!("removed   old icon {}", p.display()));
        }
    }
}

/// Install for `home`, pointing the launcher at `exe`. Returns human-readable steps.
pub fn install(home: &Path, exe: &Path) -> Result<Vec<String>, String> {
    if cfg!(windows) {
        return windows_install(home, exe);
    }
    if cfg!(target_os = "macos") {
        return mac_install(home, exe);
    }
    if !cfg!(target_os = "linux") {
        return Err(LINUX_ONLY.into());
    }
    let exe = crate::fsutil::canonical(exe).map_err(|e| format!("cannot resolve {}: {}", exe.display(), e))?;
    let t = targets(home);
    let mut steps = Vec::new();
    let io = |what: &str, e: std::io::Error| format!("{}: {}", what, e);

    atomic_write(&t.png, ICON_PNG).map_err(|e| io("icon (png)", e))?;
    atomic_write(&t.svg, ICON_SVG).map_err(|e| io("icon (svg)", e))?;
    steps.push(format!("icon      → {}", t.png.display()));

    let entry = desktop_entry(&exe, &t.png);
    atomic_write(&t.desktop, &entry).map_err(|e| io("menu entry", e))?;
    steps.push(format!("menu      → {}", t.desktop.display()));

    // Someone who had a ProtonRig shortcut on the desktop gets a Gantry one in its place.
    if file_has(&t.old_shortcut, "Name=ProtonRig") {
        atomic_write(&t.shortcut, &entry).map_err(|e| io("desktop shortcut", e))?;
        let _ = crate::fsutil::set_executable(&t.shortcut);
        let _ = fs::remove_file(&t.old_shortcut);
        steps.push(format!("shortcut  → {} (replaces ProtonRig.desktop)", t.shortcut.display()));
    }
    remove_legacy(&t, &mut steps);

    // Only ever replace a symlink (or create one); never clobber a real file named `rig`.
    match fs::symlink_metadata(&t.link) {
        Ok(m) if !m.file_type().is_symlink() => {
            steps.push(format!("command   → left {} alone (not a symlink)", t.link.display()));
        }
        other => {
            if other.is_ok() {
                fs::remove_file(&t.link).map_err(|e| io("old link", e))?;
            }
            if let Some(p) = t.link.parent() {
                fs::create_dir_all(p).map_err(|e| io("bin dir", e))?;
            }
            crate::fsutil::symlink(&exe, &t.link).map_err(|e| io("link", e))?;
            steps.push(format!("command   → {} → {}", t.link.display(), exe.display()));
        }
    }
    Ok(steps)
}

pub fn uninstall(home: &Path, exe: &Path) -> Vec<String> {
    if cfg!(windows) {
        return windows_uninstall(home);
    }
    if cfg!(target_os = "macos") {
        return mac_uninstall(home);
    }
    let t = targets(home);
    let mut steps = Vec::new();
    for p in [&t.desktop, &t.png, &t.svg] {
        if fs::remove_file(p).is_ok() {
            steps.push(format!("removed   {}", p.display()));
        }
    }
    if file_has(&t.shortcut, "Name=Gantry") && fs::remove_file(&t.shortcut).is_ok() {
        steps.push(format!("removed   {}", t.shortcut.display()));
    }
    remove_legacy(&t, &mut steps);
    // remove the link only if it is ours
    if let (Ok(target), Ok(me)) = (fs::read_link(&t.link), crate::fsutil::canonical(exe)) {
        if crate::fsutil::canonical(&target).map(|c| c == me).unwrap_or(false) && fs::remove_file(&t.link).is_ok() {
            steps.push(format!("removed   {}", t.link.display()));
        }
    }
    steps
}

// ───────────────────────────── one launcher per game ─────────────────────────────

/// Game ids that can go on a command line and into a file name without any quoting.
pub fn id_is_plain(id: &str) -> bool {
    !id.is_empty() && id.len() <= 80 && id.chars().all(|c| c.is_ascii_alphanumeric() || "_.-".contains(c))
}

fn file_slug(name: &str) -> String {
    let s: String = name.chars().map(|c| if c.is_ascii_alphanumeric() { c.to_ascii_lowercase() } else { '-' }).collect();
    let s = s.split('-').filter(|p| !p.is_empty()).collect::<Vec<_>>().join("-");
    if s.is_empty() { "game".into() } else { s.chars().take(40).collect() }
}

/// A menu entry for one game that starts it with its mods through Gantry (`rig --play <id>`), and
/// optionally the same on the desktop. Returns the files written.
pub fn game_shortcut(home: &Path, exe: &Path, appid: &str, name: &str, on_desktop: bool) -> Result<Vec<PathBuf>, String> {
    if !id_is_plain(appid) {
        return Err("This game's id cannot be put into a shortcut.".into());
    }
    let clean_name: String = name.chars().filter(|c| !c.is_control()).collect::<String>().trim().to_string();
    let title = format!("{} (Gantry)", if clean_name.is_empty() { "Game" } else { &clean_name });
    if cfg!(windows) {
        let (dir, menu) = win_paths(home);
        let target = dir.join("rig.exe");
        let target = if target.is_file() { target } else { exe.to_path_buf() };
        let ico = dir.join("gantry.ico");
        let mut written = Vec::new();
        let mut places = vec![menu.with_file_name(format!("{title}.lnk"))];
        if on_desktop {
            places.push(home.join("Desktop").join(format!("{title}.lnk")));
        }
        for lnk in places {
            if let Some(p) = lnk.parent() {
                fs::create_dir_all(p).map_err(|e| e.to_string())?;
            }
            let ok = std::process::Command::new("powershell")
                .args(["-NoProfile", "-NonInteractive", "-Command", &shortcut_script(&lnk, &target, &ico, &format!("--play {appid}"))])
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
                .map(|s| s.success())
                .unwrap_or(false);
            if !ok || !lnk.is_file() {
                return Err("Could not create the shortcut.".into());
            }
            written.push(lnk);
        }
        return Ok(written);
    }
    if !cfg!(target_os = "linux") {
        return Err("Game shortcuts are only available on Linux and Windows for now.".into());
    }
    let exe = crate::fsutil::canonical(exe).map_err(|e| format!("cannot resolve {}: {}", exe.display(), e))?;
    let entry = format!(
        "[Desktop Entry]\nType=Application\nName={title}\nComment=Play {title} with its mods\nExec={} --play {appid}\nIcon=gantry\nTerminal=false\nCategories=Game;\nStartupNotify=false\nX-Gantry-Game={appid}\n",
        exec_quote(&exe)
    );
    let file = format!("gantry-play-{}.desktop", file_slug(&clean_name));
    let mut written = Vec::new();
    let menu = home.join(".local/share/applications").join(&file);
    atomic_write(&menu, &entry).map_err(|e| e.to_string())?;
    written.push(menu);
    if on_desktop && home.join("Desktop").is_dir() {
        let d = home.join("Desktop").join(&file);
        atomic_write(&d, &entry).map_err(|e| e.to_string())?;
        let _ = crate::fsutil::set_executable(&d);
        written.push(d);
    }
    Ok(written)
}

const NXM_DESKTOP: &str = "gantry-nxm.desktop";

pub fn nxm_desktop_entry(exe: &Path) -> String {
    format!(
        "[Desktop Entry]\nType=Application\nName=Gantry (Nexus Mods links)\nExec={} %u\nNoDisplay=true\nTerminal=false\nMimeType=x-scheme-handler/nxm;\nCategories=Game;\n",
        exec_quote(exe)
    )
}

/// Write the handler's `.desktop` file and remember which handler (if any) it replaces.
/// Returns the file written. The caller runs `xdg-mime` (kept apart so this is testable).
pub fn register_nxm_files(home: &Path, config_dir: &Path, exe: &Path, previous: Option<&str>) -> Result<PathBuf, String> {
    let exe = crate::fsutil::canonical(exe).map_err(|e| format!("cannot resolve {}: {}", exe.display(), e))?;
    let desktop = home.join(".local/share/applications").join(NXM_DESKTOP);
    atomic_write(&desktop, nxm_desktop_entry(&exe)).map_err(|e| e.to_string())?;
    if let Some(prev) = previous.map(str::trim).filter(|p| !p.is_empty() && *p != NXM_DESKTOP) {
        atomic_write(&config_dir.join("nxm_previous_handler"), prev).map_err(|e| e.to_string())?;
    }
    Ok(desktop)
}

fn xdg_mime(args: &[&str]) -> String {
    use std::process::{Command, Stdio};
    Command::new("xdg-mime")
        .args(args)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default()
}

/// Make Gantry the program that opens `nxm://` links (the "Mod Manager Download" button).
pub fn register_nxm(home: &Path, config_dir: &Path, exe: &Path) -> Result<String, String> {
    if cfg!(windows) {
        return windows_register_nxm(config_dir, exe);
    }
    if !cfg!(target_os = "linux") {
        return Err(LINUX_ONLY.into());
    }
    let previous = xdg_mime(&["query", "default", "x-scheme-handler/nxm"]);
    register_nxm_files(home, config_dir, exe, Some(&previous))?;
    refresh_desktop_caches(home);
    xdg_mime(&["default", NXM_DESKTOP, "x-scheme-handler/nxm"]);
    if xdg_mime(&["query", "default", "x-scheme-handler/nxm"]) != NXM_DESKTOP {
        return Err("xdg-mime did not accept the change (is xdg-utils installed?).".into());
    }
    Ok(if previous.is_empty() || previous == NXM_DESKTOP {
        "Gantry now opens Nexus Mods links.".into()
    } else {
        format!("Gantry now opens Nexus Mods links (it replaced {}; Unregister restores it).", previous)
    })
}

/// Give `nxm://` back to whatever handled it before, and remove Gantry's handler file.
pub fn unregister_nxm(home: &Path, config_dir: &Path) -> String {
    if cfg!(windows) {
        return windows_unregister_nxm(config_dir);
    }
    let prev_file = config_dir.join("nxm_previous_handler");
    let prev = fs::read_to_string(&prev_file).unwrap_or_default().trim().to_string();
    if !prev.is_empty() && home.join(".local/share/applications").join(&prev).exists()
        || !prev.is_empty() && Path::new("/usr/share/applications").join(&prev).exists()
    {
        xdg_mime(&["default", &prev, "x-scheme-handler/nxm"]);
    }
    let _ = fs::remove_file(home.join(".local/share/applications").join(NXM_DESKTOP));
    let _ = fs::remove_file(prev_file);
    refresh_desktop_caches(home);
    if prev.is_empty() {
        "Gantry no longer handles Nexus Mods links.".into()
    } else {
        format!("Nexus Mods links go back to {}.", prev)
    }
}

/// Ask the desktop to pick up the new entry (best effort; silent if the tools are absent).
pub fn refresh_desktop_caches(home: &Path) {
    use std::process::{Command, Stdio};
    let apps = home.join(".local/share/applications");
    for (bin, args) in [("update-desktop-database", vec![apps.to_string_lossy().to_string()]), ("kbuildsycoca6", vec![])] {
        let _ = Command::new(bin).args(args).stdout(Stdio::null()).stderr(Stdio::null()).status();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fsutil::testutil::TempDir;

    #[test]
    fn the_windows_icon_wraps_the_png_and_the_shortcut_script_quotes_paths() {
        let mut png = vec![0x89, b'P', b'N', b'G', 13, 10, 26, 10, 0, 0, 0, 13, b'I', b'H', b'D', b'R'];
        png.extend(256u32.to_be_bytes());
        png.extend(256u32.to_be_bytes());
        png.extend([8, 6, 0, 0, 0]);
        let ico = png_to_ico(&png);
        assert_eq!(&ico[..6], &[0, 0, 1, 0, 1, 0]);
        assert_eq!(u32::from_le_bytes(ico[14..18].try_into().unwrap()) as usize, png.len());
        assert_eq!(u32::from_le_bytes(ico[18..22].try_into().unwrap()), 22);
        assert_eq!(&ico[22..], &png[..]);
        assert_eq!(ico[6], 0, "256 is written as 0");
        let s = shortcut_script(Path::new(r"C:\Menu\Gantry.lnk"), Path::new(r"C:\Users\O'Neil\rig.exe"), Path::new(r"C:\i.ico"), "--play 12");
        assert!(s.contains("O''Neil") && s.contains("CreateShortcut('C:\\Menu\\Gantry.lnk')") && s.contains("$s.Arguments='--play 12'"), "{s}");
        assert_eq!(parse_reg_default("\r\nHKEY\\x\r\n    (Standard)    REG_SZ    \"C:\\a b\\rig.exe\" \"%1\"\r\n").as_deref(), Some("\"C:\\a b\\rig.exe\" \"%1\""));
        assert_eq!(parse_reg_default("nothing"), None);
    }

    #[test]
    fn the_mac_icon_and_app_description_are_well_formed() {
        let icns = png_to_icns(&[1, 2, 3, 4]);
        assert_eq!(&icns[..4], b"icns");
        assert_eq!(u32::from_be_bytes(icns[4..8].try_into().unwrap()) as usize, icns.len());
        assert_eq!(&icns[8..12], b"ic08");
        assert_eq!(u32::from_be_bytes(icns[12..16].try_into().unwrap()) as usize, icns.len() - 8);
        assert!(mac_info_plist().contains("<key>CFBundleExecutable</key><string>Gantry</string>"));
    }

    #[test]
    fn a_game_gets_a_launcher_that_plays_it_with_its_mods() {
        let d = TempDir::new("gshort");
        let home = d.join("home");
        let exe = d.join("My Tools/rig");
        fs::create_dir_all(exe.parent().unwrap()).unwrap();
        fs::write(&exe, b"bin").unwrap();
        fs::create_dir_all(home.join("Desktop")).unwrap();
        assert!(id_is_plain("3164500") && id_is_plain("hydra_steam_1080110") && !id_is_plain("") && !id_is_plain("a b") && !id_is_plain("x;rm") && !id_is_plain("../x"));
        if cfg!(target_os = "linux") {
            let files = game_shortcut(&home, &exe, "3164500", "Schedule I: the \"game\"\n", true).unwrap();
            assert_eq!(files.len(), 2);
            let t = fs::read_to_string(&files[0]).unwrap();
            assert!(t.contains("Name=Schedule I: the \"game\" (Gantry)\n") && t.contains("--play 3164500\n") && t.contains("Categories=Game;"), "{t}");
            assert!(t.contains(&format!("Exec=\"{}\" --play", crate::fsutil::canonical(&exe).unwrap().display())), "the path with a space is quoted: {t}");
            assert_eq!(files[0].file_name().unwrap().to_string_lossy(), "gantry-play-schedule-i-the-game.desktop");
            assert!(files[1].starts_with(home.join("Desktop")));
            // no desktop folder, no desktop copy
            let h2 = d.join("home2");
            assert_eq!(game_shortcut(&h2, &exe, "1", "X", true).unwrap().len(), 1);
            // an id that could smuggle something into the command line is refused
            assert!(game_shortcut(&home, &exe, "1; rm -rf ~", "X", false).is_err());
        }
    }

    #[test]
    fn desktop_entry_quotes_awkward_paths() {
        let e = desktop_entry(Path::new("/opt/Proton Rig/rig"), Path::new("/i/x.png"));
        assert!(e.contains("Exec=\"/opt/Proton Rig/rig\"\n"), "{e}");
        assert!(e.contains("Icon=/i/x.png\n") && e.contains("Terminal=false") && e.contains("Name=Gantry\n"));
        assert!(desktop_entry(Path::new("/usr/bin/rig"), Path::new("/i")).contains("Exec=/usr/bin/rig\n"));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn install_is_idempotent_and_uninstall_cleans_up() {
        let d = TempDir::new("inst");
        let home = d.join("home");
        let exe = d.join("target/rig");
        fs::create_dir_all(exe.parent().unwrap()).unwrap();
        fs::write(&exe, b"bin").unwrap();

        let steps = install(&home, &exe).unwrap();
        assert_eq!(steps.len(), 3);
        install(&home, &exe).unwrap();
        let t = targets(&home);
        assert!(fs::read_to_string(&t.desktop).unwrap().contains(&crate::fsutil::canonical(&exe).unwrap().to_string_lossy().to_string()));
        assert_eq!(fs::read(&t.png).unwrap(), ICON_PNG);
        assert_eq!(fs::read_link(&t.link).unwrap(), crate::fsutil::canonical(&exe).unwrap());

        // an old link to somewhere else is repointed
        let other = d.join("old/rig");
        fs::create_dir_all(other.parent().unwrap()).unwrap();
        fs::write(&other, b"x").unwrap();
        fs::remove_file(&t.link).unwrap();
        std::os::unix::fs::symlink(&other, &t.link).unwrap();
        install(&home, &exe).unwrap();
        assert_eq!(fs::read_link(&t.link).unwrap(), crate::fsutil::canonical(&exe).unwrap());

        // a real file named rig is never overwritten
        fs::remove_file(&t.link).unwrap();
        fs::write(&t.link, b"user script").unwrap();
        let steps = install(&home, &exe).unwrap();
        assert!(steps.iter().any(|s| s.contains("left")));
        assert_eq!(fs::read(&t.link).unwrap(), b"user script");

        fs::remove_file(&t.link).unwrap();
        install(&home, &exe).unwrap();
        let removed = uninstall(&home, &exe);
        assert_eq!(removed.len(), 4);
        assert!(!t.desktop.exists() && !t.png.exists() && fs::symlink_metadata(&t.link).is_err());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn the_old_protonrig_entries_are_replaced_by_gantry_ones() {
        use std::os::unix::fs::PermissionsExt;
        let d = TempDir::new("inst3");
        let home = d.join("home");
        let exe = d.join("bin/rig");
        fs::create_dir_all(exe.parent().unwrap()).unwrap();
        fs::write(&exe, b"x").unwrap();
        let t = targets(&home);
        for p in [&t.old_desktop, &t.old_png, &t.old_svg, &t.old_shortcut] {
            fs::create_dir_all(p.parent().unwrap()).unwrap();
            fs::write(p, "[Desktop Entry]\nName=ProtonRig\nExec=/old\n").unwrap();
        }
        // an unrelated launcher that merely lives next to ours must survive
        fs::write(home.join("Desktop/Other.desktop"), "[Desktop Entry]\nName=Other\n").unwrap();

        let steps = install(&home, &exe).unwrap();
        assert!(steps.iter().any(|s| s.contains("replaces ProtonRig.desktop")), "{steps:?}");
        assert!(!t.old_desktop.exists() && !t.old_png.exists() && !t.old_svg.exists() && !t.old_shortcut.exists());
        assert!(t.desktop.is_file() && t.shortcut.is_file());
        assert_eq!(fs::metadata(&t.shortcut).unwrap().permissions().mode() & 0o111, 0o111, "shortcut is executable");
        assert!(home.join("Desktop/Other.desktop").is_file());

        // a legacy file that is NOT ours is left alone
        fs::write(&t.old_desktop, "[Desktop Entry]\nName=Something else\n").unwrap();
        install(&home, &exe).unwrap();
        assert!(t.old_desktop.is_file());

        // no Desktop shortcut is created for people who never had one
        let d2 = TempDir::new("inst4");
        install(&d2.join("h"), &exe).unwrap();
        assert!(!targets(&d2.join("h")).shortcut.exists());

        let removed = uninstall(&home, &exe);
        assert!(removed.iter().any(|s| s.contains("Gantry.desktop")), "{removed:?}");
        assert!(!t.shortcut.exists() && !t.desktop.exists());
    }

    #[cfg(unix)] // the .desktop file is a Linux thing
    #[test]
    fn the_nxm_handler_entry_registers_the_scheme_and_remembers_the_previous_one() {
        let d = TempDir::new("nxm");
        let exe = d.join("bin/rig");
        fs::create_dir_all(exe.parent().unwrap()).unwrap();
        fs::write(&exe, b"x").unwrap();
        let e = nxm_desktop_entry(&exe);
        assert!(e.contains("MimeType=x-scheme-handler/nxm;") && e.contains("%u") && e.contains("NoDisplay=true"), "{e}");
        let cfg = d.join("cfg");
        let f = register_nxm_files(&d.join("home"), &cfg, &exe, Some("mo2.desktop\n")).unwrap();
        assert!(fs::read_to_string(&f).unwrap().contains(&crate::fsutil::canonical(&exe).unwrap().to_string_lossy().to_string()));
        assert_eq!(fs::read_to_string(cfg.join("nxm_previous_handler")).unwrap(), "mo2.desktop");
        // registering twice must not remember ourselves as the "previous" handler
        register_nxm_files(&d.join("home"), &cfg, &exe, Some("gantry-nxm.desktop")).unwrap();
        assert_eq!(fs::read_to_string(cfg.join("nxm_previous_handler")).unwrap(), "mo2.desktop");
        assert!(register_nxm_files(&d.join("home"), &cfg, &d.join("nope"), None).is_err());
    }

    #[test]
    fn install_fails_cleanly_for_a_missing_binary() {
        let d = TempDir::new("inst2");
        assert!(install(&d.join("h"), &d.join("nope")).is_err());
    }
}
