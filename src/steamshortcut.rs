//! Gantry as a non-Steam game: an entry in Steam's `shortcuts.vdf`, so Gantry opens from Steam's library, Big Picture
//! and the Steam Deck's game mode.
//!
//! `shortcuts.vdf` is a small binary file (`\0shortcuts\0` then numbered entries, `\x08\x08` at the end). Steam rewrites
//! it when it exits, so it is only edited while Steam is closed. The edit adds one entry before the final end markers
//! and leaves every other byte alone; the file is read as a whole first and left untouched if it does not look right.

use std::path::{Path, PathBuf};

/// The name the entry has in Steam's library.
pub const NAME: &str = "Gantry";

/// Where `shortcuts.vdf` is for the account whose `localconfig.vdf` is `localconfig` (the same `config` folder).
pub fn path_beside(localconfig: &Path) -> Option<PathBuf> {
    Some(localconfig.parent()?.join("shortcuts.vdf"))
}

#[derive(Debug, PartialEq)]
enum Val {
    Str(String),
    Num(u32),
    Map(Vec<(String, Val)>),
}

fn cstr(b: &[u8], at: &mut usize) -> Option<String> {
    let end = b.get(*at..)?.iter().position(|&c| c == 0)?;
    let s = String::from_utf8_lossy(&b[*at..*at + end]).into_owned();
    *at += end + 1;
    Some(s)
}

/// One map of the binary format, up to and including its `\x08` end marker.
fn map(b: &[u8], at: &mut usize, depth: usize) -> Option<Vec<(String, Val)>> {
    if depth > 8 {
        return None;
    }
    let mut out = Vec::new();
    loop {
        let kind = *b.get(*at)?;
        *at += 1;
        if kind == 8 {
            return Some(out);
        }
        let key = cstr(b, at)?;
        let val = match kind {
            0 => Val::Map(map(b, at, depth + 1)?),
            1 => Val::Str(cstr(b, at)?),
            2 => {
                let n = u32::from_le_bytes(b.get(*at..*at + 4)?.try_into().ok()?);
                *at += 4;
                Val::Num(n)
            }
            _ => return None,
        };
        out.push((key, val));
    }
}

/// The whole file as `(entry number, name, program)` per entry, or None when it is not a shortcuts file.
fn entries(b: &[u8]) -> Option<Vec<(u64, String, String)>> {
    if b.is_empty() {
        return Some(Vec::new());
    }
    let mut at = 0;
    let top = map(b, &mut at, 0)?;
    // the whole file must be one map and nothing else
    if at != b.len() {
        return None;
    }
    let (_, Val::Map(list)) = top.into_iter().find(|(k, _)| k.eq_ignore_ascii_case("shortcuts"))? else { return None };
    let mut out = Vec::new();
    for (k, v) in list {
        let Val::Map(fields) = v else { return None };
        let get = |name: &str| fields.iter().find(|(f, _)| f.eq_ignore_ascii_case(name)).and_then(|(_, v)| if let Val::Str(s) = v { Some(s.clone()) } else { None }).unwrap_or_default();
        out.push((k.parse().ok()?, get("AppName"), get("Exe")));
    }
    Some(out)
}

/// Is Gantry (this program) already one of the shortcuts?
pub fn has_gantry(file: &[u8], exe: &str) -> Option<bool> {
    let list = entries(file)?;
    Some(list.iter().any(|(_, name, e)| name == NAME || e.trim_matches('"') == exe))
}

fn crc32(data: &[u8]) -> u32 {
    let mut crc = !0u32;
    for &byte in data {
        crc ^= byte as u32;
        for _ in 0..8 {
            crc = if crc & 1 != 0 { (crc >> 1) ^ 0xEDB8_8320 } else { crc >> 1 };
        }
    }
    !crc
}

/// The number Steam gives a non-Steam game (its program and name decide it).
pub fn shortcut_appid(exe: &str, name: &str) -> u32 {
    crc32(format!("\"{exe}\"{name}").as_bytes()) | 0x8000_0000
}

fn push_str(out: &mut Vec<u8>, key: &str, value: &str) {
    out.push(1);
    out.extend(key.as_bytes());
    out.push(0);
    out.extend(value.as_bytes());
    out.push(0);
}

fn push_num(out: &mut Vec<u8>, key: &str, value: u32) {
    out.push(2);
    out.extend(key.as_bytes());
    out.push(0);
    out.extend(value.to_le_bytes());
}

fn entry(index: u64, exe: &str, start_dir: &str, args: &str) -> Vec<u8> {
    let mut o = vec![0];
    o.extend(index.to_string().as_bytes());
    o.push(0);
    push_num(&mut o, "appid", shortcut_appid(exe, NAME));
    push_str(&mut o, "AppName", NAME);
    push_str(&mut o, "Exe", &format!("\"{exe}\""));
    push_str(&mut o, "StartDir", &format!("\"{start_dir}\""));
    push_str(&mut o, "icon", "");
    push_str(&mut o, "ShortcutPath", "");
    push_str(&mut o, "LaunchOptions", args);
    push_num(&mut o, "IsHidden", 0);
    push_num(&mut o, "AllowDesktopConfig", 1);
    push_num(&mut o, "AllowOverlay", 1);
    push_num(&mut o, "OpenVR", 0);
    push_num(&mut o, "Devkit", 0);
    push_str(&mut o, "DevkitGameID", "");
    push_num(&mut o, "DevkitOverrideAppID", 0);
    push_num(&mut o, "LastPlayTime", 0);
    push_str(&mut o, "FlatpakAppID", "");
    o.push(0);
    o.extend(b"tags");
    o.push(0);
    o.push(8);
    o.push(8);
    o
}

/// `file` with a Gantry entry added. An empty or missing file becomes a file with just that entry.
pub fn add_gantry(file: &[u8], exe: &str, start_dir: &str, args: &str) -> Result<Vec<u8>, String> {
    let list = entries(file).ok_or("Steam's shortcuts file has a form Gantry does not know, so it was left as it is.")?;
    if list.iter().any(|(_, name, e)| name == NAME || e.trim_matches('"') == exe) {
        return Err("Gantry is already in Steam's library.".into());
    }
    let next = list.iter().map(|(i, _, _)| i + 1).max().unwrap_or(0);
    let mut out = if file.is_empty() { b"\0shortcuts\0\x08\x08".to_vec() } else { file.to_vec() };
    // before the end marker of the list and the end marker of the file
    let at = out.len() - 2;
    out.splice(at..at, entry(next, exe, start_dir, args));
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_empty_file_gets_one_entry_that_reads_back() {
        let f = add_gantry(b"", "/opt/Gantry.AppImage", "/opt", "--deck").unwrap();
        assert!(f.starts_with(b"\0shortcuts\0\0") && f.ends_with(&[8, 8]));
        let e = entries(&f).unwrap();
        assert_eq!(e, vec![(0, "Gantry".to_string(), "\"/opt/Gantry.AppImage\"".to_string())]);
        assert_eq!(has_gantry(&f, "/opt/Gantry.AppImage"), Some(true));
        assert_eq!(has_gantry(&f, "/somewhere/else"), Some(true), "the name alone is enough");
    }

    #[test]
    fn other_entries_stay_byte_for_byte_and_the_number_continues() {
        let mut other = b"\0shortcuts\0".to_vec();
        for (i, name) in [(0u64, "Game A"), (4, "Game B")] {
            other.push(0);
            other.extend(i.to_string().as_bytes());
            other.push(0);
            push_num(&mut other, "appid", 0x8123_4567);
            push_str(&mut other, "AppName", name);
            push_str(&mut other, "Exe", "\"/games/x\"");
            other.extend(b"\0tags\0\x08\x08");
        }
        other.extend([8, 8]);
        assert_eq!(has_gantry(&other, "/opt/rig"), Some(false));
        let f = add_gantry(&other, "/opt/rig", "/opt", "").unwrap();
        assert!(f.starts_with(&other[..other.len() - 2]), "everything before the new entry is untouched");
        let names: Vec<(u64, String)> = entries(&f).unwrap().into_iter().map(|(i, n, _)| (i, n)).collect();
        assert_eq!(names, vec![(0, "Game A".into()), (4, "Game B".into()), (5, "Gantry".into())]);
        // adding twice is refused
        assert!(add_gantry(&f, "/opt/rig", "/opt", "").unwrap_err().contains("already"));
    }

    #[test]
    fn a_file_written_by_steam_itself_is_understood() {
        // the shape Steam writes for a game added by hand (with its extra "sortas" field), paths made up
        let steam: &[u8] = b"\x00shortcuts\x00\x000\x00\x02appid\x00/Qk\x82\x01AppName\x00Some Game.exe\x00\x01Exe\x00\"/games/Some Game/game.exe\"\x00\x01StartDir\x00/games/Some Game/\x00\x01icon\x00\x00\x01ShortcutPath\x00\x00\x01LaunchOptions\x00\x00\x02IsHidden\x00\x00\x00\x00\x00\x02AllowDesktopConfig\x00\x01\x00\x00\x00\x02AllowOverlay\x00\x01\x00\x00\x00\x02OpenVR\x00\x00\x00\x00\x00\x02Devkit\x00\x00\x00\x00\x00\x01DevkitGameID\x00\x00\x02DevkitOverrideAppID\x00\x00\x00\x00\x00\x02LastPlayTime\x00\xd0\xbd\xcfj\x01FlatpakAppID\x00\x00\x01sortas\x00\x00\x00tags\x00\x08\x08\x08\x08";
        assert_eq!(entries(steam).unwrap(), vec![(0, "Some Game.exe".to_string(), "\"/games/Some Game/game.exe\"".to_string())]);
        let f = add_gantry(steam, "/opt/rig", "/opt", "--deck").unwrap();
        assert!(f.starts_with(&steam[..steam.len() - 2]) && f.ends_with(&[8, 8]));
        assert_eq!(entries(&f).unwrap().len(), 2);
    }

    #[test]
    fn a_file_in_a_form_nobody_knows_is_not_touched() {
        for bad in [&b"hello"[..], b"\0shortcuts\0", b"\0shortcuts\0\x08", b"\0other\0\x08\x08", b"\0shortcuts\0\x08\x08trailing", b"\0shortcuts\0\x07x\0\x08\x08"] {
            assert!(add_gantry(bad, "/opt/rig", "/opt", "").is_err(), "{bad:?}");
            assert_eq!(has_gantry(bad, "/opt/rig"), None);
        }
    }

    #[test]
    fn the_number_steam_gives_a_shortcut_has_its_top_bit_set() {
        let a = shortcut_appid("/opt/rig", "Gantry");
        assert!(a & 0x8000_0000 != 0);
        assert_eq!(a, shortcut_appid("/opt/rig", "Gantry"));
        assert_ne!(a, shortcut_appid("/opt/rig2", "Gantry"));
        // the checksum itself is the usual one
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
    }

    #[test]
    fn the_file_sits_beside_the_launch_options_file() {
        assert_eq!(path_beside(Path::new("/s/userdata/1/config/localconfig.vdf")), Some(PathBuf::from("/s/userdata/1/config/shortcuts.vdf")));
    }
}
