//! Gantry as a non-Steam game: an entry in Steam's `shortcuts.vdf`, so Gantry opens from Steam's library, Big Picture
//! and the Steam Deck's game mode.
//!
//! `shortcuts.vdf` is a small binary file (`\0shortcuts\0` then numbered entries, `\x08\x08` at the end). Steam rewrites
//! it when it exits, so it is only edited while Steam is closed. The edit adds one entry before the final end markers
//! and leaves every other byte alone; the file is read as a whole first and left untouched if it does not look right.

use std::fs;
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

/// A parsed Steam non-Steam shortcut entry.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Shortcut {
    pub index: u64,
    pub appid: u32,
    pub name: String,
    pub exe: String,
    pub start_dir: String,
    pub launch_options: String,
    pub icon: String,
    pub tags: Vec<String>,
}

/// List all shortcuts in the file.
pub fn list_shortcuts(b: &[u8]) -> Option<Vec<Shortcut>> {
    if b.is_empty() {
        return Some(Vec::new());
    }
    let mut at = 0;
    let top = map(b, &mut at, 0)?;
    if at != b.len() {
        return None;
    }
    let (_, Val::Map(list)) = top.into_iter().find(|(k, _)| k.eq_ignore_ascii_case("shortcuts"))? else { return None };
    let mut out = Vec::new();
    for (k, v) in list {
        let Val::Map(fields) = v else { return None };
        let get_str = |name: &str| fields.iter().find(|(f, _)| f.eq_ignore_ascii_case(name)).and_then(|(_, val)| if let Val::Str(s) = val { Some(s.clone()) } else { None }).unwrap_or_default();
        let get_num = |name: &str| fields.iter().find(|(f, _)| f.eq_ignore_ascii_case(name)).and_then(|(_, val)| if let Val::Num(n) = val { Some(*n) } else { None }).unwrap_or(0);
        let tags = fields.iter().find(|(f, _)| f.eq_ignore_ascii_case("tags")).and_then(|(_, val)| {
            if let Val::Map(sub) = val {
                Some(sub.iter().filter_map(|(_, tv)| if let Val::Str(s) = tv { Some(s.clone()) } else { None }).collect())
            } else {
                None
            }
        }).unwrap_or_default();
        let name = get_str("AppName");
        let exe = get_str("Exe").trim_matches('"').to_string();
        let mut appid = get_num("appid");
        if appid == 0 {
            appid = shortcut_appid(&exe, &name);
        }
        out.push(Shortcut {
            index: k.parse().unwrap_or(0),
            appid,
            name,
            exe,
            start_dir: get_str("StartDir").trim_matches('"').to_string(),
            launch_options: get_str("LaunchOptions"),
            icon: get_str("icon"),
            tags,
        });
    }
    Some(out)
}

/// Is Gantry (this program) already one of the shortcuts?
pub fn has_gantry(file: &[u8], exe: &str) -> Option<bool> {
    let list = entries(file)?;
    Some(list.iter().any(|(_, name, e)| name == NAME || name == "Gantry" || name == "ModForge" || e.trim_matches('"') == exe))
}

/// Checks if a shortcut with the given name or executable exists.
pub fn has_shortcut(file: &[u8], name: &str, exe: &str) -> Option<bool> {
    let list = entries(file)?;
    let clean_exe = exe.trim_matches('"');
    Some(list.iter().any(|(_, n, e)| n == name || e.trim_matches('"') == clean_exe))
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

fn make_entry(index: u64, name: &str, exe: &str, start_dir: &str, args: &str, icon: &str, tags: &[String]) -> Vec<u8> {
    let mut o = vec![0];
    o.extend(index.to_string().as_bytes());
    o.push(0);
    push_num(&mut o, "appid", shortcut_appid(exe, name));
    push_str(&mut o, "AppName", name);
    push_str(&mut o, "Exe", &format!("\"{exe}\""));
    push_str(&mut o, "StartDir", &format!("\"{start_dir}\""));
    push_str(&mut o, "icon", icon);
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
    for (t_idx, tag) in tags.iter().enumerate() {
        push_str(&mut o, &t_idx.to_string(), tag);
    }
    o.push(8);
    o.push(8);
    o
}

fn entry(index: u64, exe: &str, start_dir: &str, args: &str) -> Vec<u8> {
    make_entry(index, NAME, exe, start_dir, args, "", &[])
}

fn serialize_file(list: &[(String, Val)]) -> Vec<u8> {
    let mut out = vec![0];
    out.extend(b"shortcuts\0");
    for (k, v) in list {
        if let Val::Map(fields) = v {
            out.push(0);
            out.extend(k.as_bytes());
            out.push(0);
            for (fk, fv) in fields {
                match fv {
                    Val::Str(s) => push_str(&mut out, fk, s),
                    Val::Num(n) => push_num(&mut out, fk, *n),
                    Val::Map(sub) => {
                        out.push(0);
                        out.extend(fk.as_bytes());
                        out.push(0);
                        for (sk, sv) in sub {
                            match sv {
                                Val::Str(s) => push_str(&mut out, sk, s),
                                Val::Num(n) => push_num(&mut out, sk, *n),
                                _ => {}
                            }
                        }
                        out.push(8);
                    }
                }
            }
            out.push(8);
        }
    }
    out.push(8);
    out.push(8);
    out
}

/// Remove a shortcut matching name or executable from shortcuts.vdf.
pub fn remove_shortcut(file: &[u8], name: &str, exe: &str) -> Result<Vec<u8>, String> {
    if file.is_empty() {
        return Ok(Vec::new());
    }
    let mut at = 0;
    let top = map(file, &mut at, 0).ok_or("Invalid shortcuts file format.")?;
    if at != file.len() {
        return Err("Unexpected trailing data in shortcuts file.".into());
    }
    let (_, Val::Map(list)) = top.into_iter().find(|(k, _)| k.eq_ignore_ascii_case("shortcuts")).ok_or("Missing shortcuts root.")? else {
        return Err("Malformed shortcuts root.".into());
    };
    let exe_clean = exe.trim_matches('"');
    let mut filtered = Vec::new();
    for (_, v) in list {
        if let Val::Map(ref fields) = v {
            let matches_name = fields.iter().any(|(f, val)| f.eq_ignore_ascii_case("AppName") && matches!(val, Val::Str(s) if s == name));
            let matches_exe = fields.iter().any(|(f, val)| f.eq_ignore_ascii_case("Exe") && matches!(val, Val::Str(s) if s.trim_matches('"') == exe_clean));
            if matches_name || matches_exe {
                continue;
            }
        }
        filtered.push(v);
    }
    let reindexed: Vec<(String, Val)> = filtered.into_iter().enumerate().map(|(i, val)| (i.to_string(), val)).collect();
    Ok(serialize_file(&reindexed))
}

/// Add or update a shortcut in shortcuts.vdf.
pub fn add_shortcut(
    file: &[u8],
    name: &str,
    exe: &str,
    start_dir: &str,
    args: &str,
    icon: &str,
    tags: &[String],
) -> Result<(Vec<u8>, u32), String> {
    let clean = if !file.is_empty() && has_shortcut(file, name, exe).unwrap_or(false) {
        remove_shortcut(file, name, exe)?
    } else if file.is_empty() {
        b"\0shortcuts\0\x08\x08".to_vec()
    } else {
        file.to_vec()
    };
    let list = entries(&clean).ok_or("Steam's shortcuts file has a form Gantry does not know, so it was left as it is.")?;
    let next = list.iter().map(|(i, _, _)| i + 1).max().unwrap_or(0);
    let appid = shortcut_appid(exe, name);
    let mut out = clean;
    let at = out.len() - 2;
    out.splice(at..at, make_entry(next, name, exe, start_dir, args, icon, tags));
    Ok((out, appid))
}

/// Sync grid artwork images to Steam's userdata/<id>/config/grid/ folder.
pub fn sync_grid_artwork(
    grid_dir: &Path,
    exe: &str,
    name: &str,
    icon: Option<&[u8]>,
    poster: Option<&[u8]>,
    banner: Option<&[u8]>,
    hero: Option<&[u8]>,
    logo: Option<&[u8]>,
) -> Result<u32, String> {
    if !grid_dir.exists() {
        fs::create_dir_all(grid_dir).map_err(|e| format!("Could not create grid directory: {e}"))?;
    }
    let id = shortcut_appid(exe, name);
    let big_id = ((id as u64) << 32) | 0x02000000;
    let write_art = |suffix: &str, bytes: &[u8]| -> Result<(), String> {
        let p1 = grid_dir.join(format!("{id}{suffix}"));
        let p2 = grid_dir.join(format!("{big_id}{suffix}"));
        let _ = fs::write(&p1, bytes);
        let _ = fs::write(&p2, bytes);
        Ok(())
    };
    if let Some(b) = icon { write_art("_icon.png", b)?; }
    if let Some(b) = poster { write_art("p.png", b)?; }
    if let Some(b) = banner { write_art(".png", b)?; }
    if let Some(b) = hero { write_art("_hero.png", b)?; }
    if let Some(b) = logo { write_art("_logo.png", b)?; }
    Ok(id)
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

    #[test]
    fn custom_game_shortcut_add_list_update_and_remove() {
        let empty = b"";
        let (f1, appid1) = add_shortcut(empty, "Hollow Knight", "/games/hk/hollow_knight.exe", "/games/hk", "--modded", "/games/hk/icon.png", &["Metroidvania".into(), "ModForge".into()]).unwrap();
        assert_eq!(appid1, shortcut_appid("/games/hk/hollow_knight.exe", "Hollow Knight"));
        assert!(has_shortcut(&f1, "Hollow Knight", "/games/hk/hollow_knight.exe").unwrap());

        // Add second game
        let (f2, appid2) = add_shortcut(&f1, "Silksong", "/games/ss/silksong.exe", "/games/ss", "", "", &[]).unwrap();
        assert_ne!(appid1, appid2);

        let list = list_shortcuts(&f2).unwrap();
        assert_eq!(list.len(), 2);
        assert_eq!(list[0].name, "Hollow Knight");
        assert_eq!(list[0].tags, vec!["Metroidvania".to_string(), "ModForge".to_string()]);
        assert_eq!(list[1].name, "Silksong");

        // Update first game (e.g. changed launch options)
        let (f3, appid3) = add_shortcut(&f2, "Hollow Knight", "/games/hk/hollow_knight.exe", "/games/hk", "--new-args", "/games/hk/icon.png", &["Updated".into()]).unwrap();
        assert_eq!(appid1, appid3);
        let list3 = list_shortcuts(&f3).unwrap();
        assert_eq!(list3.len(), 2);
        let hk = list3.iter().find(|s| s.name == "Hollow Knight").unwrap();
        assert_eq!(hk.launch_options, "--new-args");
        assert_eq!(hk.tags, vec!["Updated".to_string()]);

        // Remove Silksong
        let f4 = remove_shortcut(&f3, "Silksong", "/games/ss/silksong.exe").unwrap();
        let list4 = list_shortcuts(&f4).unwrap();
        assert_eq!(list4.len(), 1);
        assert_eq!(list4[0].name, "Hollow Knight");
    }

    #[test]
    fn sync_grid_artwork_writes_standard_and_big_appid_images() {
        let dir = std::env::temp_dir().join(format!("steamshortcut_grid_{}", crate::fsutil::unique_token()));
        let _ = fs::create_dir_all(&dir);
        let grid = dir.join("grid");
        let id = sync_grid_artwork(&grid, "/games/hk/game.exe", "HK", Some(b"ICON"), Some(b"POSTER"), Some(b"BANNER"), Some(b"HERO"), Some(b"LOGO")).unwrap();
        let big = ((id as u64) << 32) | 0x02000000;

        assert_eq!(fs::read(grid.join(format!("{id}p.png"))).unwrap(), b"POSTER");
        assert_eq!(fs::read(grid.join(format!("{big}p.png"))).unwrap(), b"POSTER");
        assert_eq!(fs::read(grid.join(format!("{id}.png"))).unwrap(), b"BANNER");
        assert_eq!(fs::read(grid.join(format!("{big}.png"))).unwrap(), b"BANNER");
        assert_eq!(fs::read(grid.join(format!("{id}_hero.png"))).unwrap(), b"HERO");
        assert_eq!(fs::read(grid.join(format!("{id}_logo.png"))).unwrap(), b"LOGO");
        assert_eq!(fs::read(grid.join(format!("{id}_icon.png"))).unwrap(), b"ICON");
        let _ = fs::remove_dir_all(&dir);
    }
}
