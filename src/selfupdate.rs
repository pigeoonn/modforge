//! Update Gantry in place: the AppImage on Linux and the Windows build replace themselves after a yes.
//!
//! The file comes from Gantry's own GitHub release, its SHA-256 is compared with the `.sha256` file of the same
//! release (that catches a broken or cut-off download, not a stolen GitHub account), and the running copy is swapped
//! by renaming, so the old one is kept next to it until the next start. Other installs (a package manager, a
//! tarball, a build from source) are told to update the way they were installed.

use serde_json::Value;
use std::io;
use std::path::{Path, PathBuf};

/// The file that would be replaced: the AppImage that is running, or the Windows `.exe`. None for every other kind of
/// install (and under test, unless a test sets it).
pub fn target() -> Option<PathBuf> {
    if cfg!(test) {
        return None;
    }
    if cfg!(windows) {
        return std::env::current_exe().ok().filter(|p| p.extension().is_some_and(|e| e.eq_ignore_ascii_case("exe")));
    }
    if cfg!(target_os = "linux") {
        return std::env::var_os("APPIMAGE").map(PathBuf::from).filter(|p| p.is_file());
    }
    None
}

/// Which file of a release to take for this kind of install, and the file that holds its checksum:
/// `(name, download address, checksum address)`.
pub fn pick_asset(assets: &Value, windows: bool, arch: &str) -> Option<(String, String, String)> {
    let list = assets.as_array()?;
    let url_of = |name: &str| list.iter().find(|a| a["name"].as_str() == Some(name)).and_then(|a| a["browser_download_url"].as_str()).map(str::to_string);
    for a in list {
        let name = a["name"].as_str()?;
        let wanted = if windows {
            name.starts_with("gantry-") && name.ends_with("-windows-x86_64.zip") && arch == "x86_64"
        } else {
            name.ends_with(".AppImage") && (name.contains(arch) || !name.contains('_'))
        };
        if wanted {
            let (url, sum) = (url_of(name)?, url_of(&format!("{name}.sha256"))?);
            return Some((name.to_string(), url, sum));
        }
    }
    None
}

/// The checksum a `sha256sum` file states for `name` (`<64 hex>  <name>`), in lower case.
pub fn expected_sum(text: &str, name: &str) -> Option<String> {
    let line = text.lines().find(|l| l.trim_end().ends_with(name)).or_else(|| text.lines().next())?;
    let hex = line.split_whitespace().next()?.to_ascii_lowercase();
    (hex.len() == 64 && hex.bytes().all(|b| b.is_ascii_hexdigit())).then_some(hex)
}

/// Put `new` where `target` is. The running file is renamed to `<name>.old` first (allowed on Linux and on Windows
/// while it runs); if the copy fails the old one is put back.
pub fn swap_in(target: &Path, new: &Path) -> io::Result<()> {
    let mut old_name = target.file_name().unwrap_or_default().to_os_string();
    old_name.push(".old");
    let old = target.with_file_name(old_name);
    let _ = std::fs::remove_file(&old);
    // next to the target first, so the last step is a rename inside one folder
    let mut stage_name = target.file_name().unwrap_or_default().to_os_string();
    stage_name.push(".new");
    let stage = target.with_file_name(stage_name);
    std::fs::copy(new, &stage)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&stage, std::fs::Permissions::from_mode(0o755))?;
    }
    std::fs::rename(target, &old).inspect_err(|_| {
        let _ = std::fs::remove_file(&stage);
    })?;
    if let Err(e) = std::fs::rename(&stage, target) {
        let _ = std::fs::rename(&old, target);
        let _ = std::fs::remove_file(&stage);
        return Err(e);
    }
    let _ = std::fs::remove_file(&old);
    Ok(())
}

/// A file of that name anywhere under `dir` (the zip of the Windows build holds the program in a folder).
pub fn find_named(dir: &Path, name: &str) -> Option<PathBuf> {
    for e in std::fs::read_dir(dir).ok()?.flatten() {
        let p = e.path();
        if p.is_dir() {
            if let Some(f) = find_named(&p, name) {
                return Some(f);
            }
        } else if p.file_name().is_some_and(|n| n.to_string_lossy().eq_ignore_ascii_case(name)) {
            return Some(p);
        }
    }
    None
}

/// The leftover of an earlier update (Windows could not delete the running file then).
pub fn clean_up_after_update() {
    if let Ok(exe) = std::env::current_exe() {
        let mut old = exe.file_name().unwrap_or_default().to_os_string();
        old.push(".old");
        let _ = std::fs::remove_file(exe.with_file_name(old));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn assets(names: &[&str]) -> Value {
        Value::Array(names.iter().map(|n| json!({ "name": n, "browser_download_url": format!("https://github.com/pigeoonn/modforge/releases/download/v9/{n}") })).collect())
    }

    #[test]
    fn the_right_file_of_a_release_is_chosen_with_its_checksum() {
        let a = assets(&["gantry-v9-linux-x86_64.tar.gz", "gantry-v9-linux-x86_64.tar.gz.sha256", "Gantry-9-x86_64.AppImage", "Gantry-9-x86_64.AppImage.sha256", "gantry-v9-windows-x86_64.zip", "gantry-v9-windows-x86_64.zip.sha256"]);
        let (n, u, s) = pick_asset(&a, false, "x86_64").unwrap();
        assert_eq!(n, "Gantry-9-x86_64.AppImage");
        assert!(u.ends_with("/Gantry-9-x86_64.AppImage") && s.ends_with("/Gantry-9-x86_64.AppImage.sha256"));
        assert_eq!(pick_asset(&a, true, "x86_64").unwrap().0, "gantry-v9-windows-x86_64.zip");
        // another processor gets no AppImage of this one, a missing checksum file means no update
        assert!(pick_asset(&a, false, "aarch64").is_none());
        assert!(pick_asset(&assets(&["Gantry-9-x86_64.AppImage"]), false, "x86_64").is_none());
        assert!(pick_asset(&Value::Null, false, "x86_64").is_none());
    }

    #[test]
    fn a_checksum_file_is_read_strictly() {
        let h = "a".repeat(64);
        assert_eq!(expected_sum(&format!("{}  Gantry-9-x86_64.AppImage\n", h.to_uppercase()), "Gantry-9-x86_64.AppImage"), Some(h.clone()));
        assert_eq!(expected_sum(&format!("{h}  other\n{}  mine\n", "b".repeat(64)), "mine"), Some("b".repeat(64)));
        for bad in ["", "zz  mine", &format!("{}  mine", "g".repeat(64)), &format!("{}  mine", "a".repeat(63))] {
            assert_eq!(expected_sum(bad, "mine"), None, "{bad:?}");
        }
    }

    #[test]
    fn swapping_keeps_the_old_file_until_the_new_one_is_in() {
        let d = std::env::temp_dir().join(format!("rig-swap-{}", crate::fsutil::unique_token()));
        std::fs::create_dir_all(&d).unwrap();
        let (target, new) = (d.join("Gantry.AppImage"), d.join("download"));
        std::fs::write(&target, "old").unwrap();
        std::fs::write(&new, "new").unwrap();
        swap_in(&target, &new).unwrap();
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "new");
        assert!(!d.join("Gantry.AppImage.old").exists() && !d.join("Gantry.AppImage.new").exists());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(&target).unwrap().permissions().mode() & 0o111, 0o111, "it can be run");
        }
        // a download that is gone changes nothing
        assert!(swap_in(&target, &d.join("missing")).is_err());
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "new");
        let _ = std::fs::remove_dir_all(&d);
    }
}
