//! Plugins of Bethesda style games (Skyrim, Fallout 4, Starfield): the `.esp`, `.esm` and `.esl` files in
//! `Data` and the order the game loads them in, kept in `plugins.txt`. This reads each plugin's header
//! (what it needs loaded first), shows which are switched on, sorts them so every plugin comes after the
//! ones it needs, and writes `plugins.txt` back (the first version is kept next to it).

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Layout {
    /// The folder under AppData/Local that holds `plugins.txt`.
    pub appdata: &'static str,
    /// Master files the game always loads first, in this order (they are never listed in `plugins.txt`).
    pub base: &'static [&'static str],
}

const LAYOUTS: &[(&str, Layout)] = &[
    ("SkyrimSE.exe", Layout { appdata: "Skyrim Special Edition", base: &["Skyrim.esm", "Update.esm", "Dawnguard.esm", "HearthFires.esm", "Dragonborn.esm"] }),
    ("SkyrimVR.exe", Layout { appdata: "Skyrim VR", base: &["Skyrim.esm", "Update.esm", "Dawnguard.esm", "HearthFires.esm", "Dragonborn.esm", "SkyrimVR.esm"] }),
    ("TESV.exe", Layout { appdata: "Skyrim", base: &["Skyrim.esm", "Update.esm", "Dawnguard.esm", "HearthFires.esm", "Dragonborn.esm"] }),
    ("Fallout4.exe", Layout { appdata: "Fallout4", base: &["Fallout4.esm", "DLCRobot.esm", "DLCworkshop01.esm", "DLCCoast.esm", "DLCworkshop02.esm", "DLCworkshop03.esm", "DLCNukaWorld.esm"] }),
    ("Fallout4VR.exe", Layout { appdata: "Fallout4VR", base: &["Fallout4.esm", "DLCRobot.esm", "DLCworkshop01.esm", "DLCCoast.esm", "DLCworkshop02.esm", "DLCworkshop03.esm", "DLCNukaWorld.esm", "Fallout4_VR.esm"] }),
    ("Starfield.exe", Layout { appdata: "Starfield", base: &["Starfield.esm", "Constellation.esm", "OldMars.esm", "BlueprintShips-Starfield.esm"] }),
    ("FalloutNV.exe", Layout { appdata: "FalloutNV", base: &["FalloutNV.esm", "DeadMoney.esm", "HonestHearts.esm", "OldWorldBlues.esm", "LonesomeRoad.esm", "GunRunnersArsenal.esm", "CaravanPack.esm", "ClassicPack.esm", "MercenaryPack.esm", "TribalPack.esm"] }),
    ("Fallout3.exe", Layout { appdata: "Fallout3", base: &["Fallout3.esm", "Anchorage.esm", "ThePitt.esm", "BrokenSteel.esm", "PointLookout.esm", "Zeta.esm"] }),
    ("Oblivion.exe", Layout { appdata: "Oblivion", base: &["Oblivion.esm"] }),
    ("Morrowind.exe", Layout { appdata: "Morrowind", base: &["Morrowind.esm", "Tribunal.esm", "Bloodmoon.esm"] }),
];

/// Which game this is, from the program in its folder.
pub fn layout_for(game_dir: &Path) -> Option<Layout> {
    let names: HashSet<String> = fs::read_dir(game_dir).ok()?.flatten().map(|e| e.file_name().to_string_lossy().to_lowercase()).collect();
    LAYOUTS.iter().find(|(exe, _)| names.contains(&exe.to_lowercase())).map(|(_, l)| *l)
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Header {
    pub master_flag: bool,
    pub light_flag: bool,
    pub masters: Vec<String>,
    pub description: String,
    pub author: String,
}

fn text(b: &[u8]) -> String {
    let end = b.iter().position(|&c| c == 0).unwrap_or(b.len());
    String::from_utf8_lossy(&b[..end]).trim().to_string()
}

/// The header record (`TES4`) of a plugin: its flags and what it needs.
pub fn parse_header(b: &[u8]) -> Option<Header> {
    if b.len() < 24 || &b[..4] != b"TES4" {
        return None;
    }
    let size = u32::from_le_bytes(b[4..8].try_into().ok()?) as usize;
    let flags = u32::from_le_bytes(b[8..12].try_into().ok()?);
    // Oblivion's record header is 20 bytes, the later games' 24
    let start = if &b[20..24] == b"HEDR" { 20 } else { 24 };
    let end = (start + size).min(b.len());
    let mut h = Header { master_flag: flags & 1 != 0, light_flag: flags & 0x200 != 0, ..Default::default() };
    let mut i = start;
    while i + 6 <= end {
        let kind = &b[i..i + 4];
        let len = u16::from_le_bytes([b[i + 4], b[i + 5]]) as usize;
        let data_end = (i + 6 + len).min(end);
        let data = &b[i + 6..data_end];
        match kind {
            b"MAST" => h.masters.push(text(data)),
            b"CNAM" => h.author = text(data),
            b"SNAM" => h.description = text(data),
            _ => {}
        }
        i += 6 + len;
    }
    Some(h)
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct Plugin {
    pub name: String,
    /// "master", "plugin" or "light"
    pub kind: &'static str,
    pub masters: Vec<String>,
    pub description: String,
    pub author: String,
    /// One of the game's own master files: always loaded first, not listed in `plugins.txt`.
    pub base: bool,
}

fn is_plugin_file(name: &str) -> bool {
    let l = name.to_lowercase();
    l.ends_with(".esp") || l.ends_with(".esm") || l.ends_with(".esl")
}

/// Every plugin in the `Data` folder, with its header read.
pub fn scan(data_dir: &Path, layout: &Layout) -> Vec<Plugin> {
    let mut out: Vec<Plugin> = fs::read_dir(data_dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.path().is_file() && is_plugin_file(&e.file_name().to_string_lossy()))
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().to_string();
            let mut buf = vec![0u8; 64 * 1024];
            let n = std::io::Read::read(&mut fs::File::open(e.path()).ok()?, &mut buf).ok()?;
            let h = parse_header(&buf[..n])?;
            let l = name.to_lowercase();
            let kind = if l.ends_with(".esl") || h.light_flag { "light" } else if l.ends_with(".esm") || h.master_flag { "master" } else { "plugin" };
            let base = layout.base.iter().any(|b| b.eq_ignore_ascii_case(&name));
            Some(Plugin { name, kind, masters: h.masters, description: h.description, author: h.author, base })
        })
        .collect();
    out.sort_by_key(|p| p.name.to_lowercase());
    out
}

/// The load order saved in `plugins.txt`: `(name, enabled)`. A line starting with `*` is switched on.
pub fn parse_plugins_txt(text: &str) -> Vec<(String, bool)> {
    let mut seen = HashSet::new();
    text.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .filter_map(|l| {
            let (name, on) = match l.strip_prefix('*') {
                Some(n) => (n.trim(), true),
                None => (l, false),
            };
            (is_plugin_file(name) && seen.insert(name.to_lowercase())).then(|| (name.to_string(), on))
        })
        .collect()
}

/// The text of `plugins.txt` for an order. The game's own masters are left out (it loads them anyway).
pub fn write_plugins_txt(order: &[(String, bool)], layout: &Layout) -> String {
    let mut out = String::from("# This file is used by the game to keep track of your downloaded content.\n# Please do not modify this file.\n");
    for (name, on) in order {
        if layout.base.iter().any(|b| b.eq_ignore_ascii_case(name)) {
            continue;
        }
        if *on {
            out.push('*');
        }
        out.push_str(name);
        out.push_str("\r\n");
    }
    out.replace("\n#", "\r\n#").replace("\r\r\n", "\r\n")
}

/// The load order to show: the saved one, with new plugins appended and gone ones dropped; the game's
/// own masters first (always on).
pub fn merged_order(plugins: &[Plugin], saved: &[(String, bool)], layout: &Layout) -> Vec<(String, bool)> {
    let have: HashMap<String, &Plugin> = plugins.iter().map(|p| (p.name.to_lowercase(), p)).collect();
    let mut out: Vec<(String, bool)> = Vec::new();
    for b in layout.base {
        if let Some(p) = have.get(&b.to_lowercase()) {
            out.push((p.name.clone(), true));
        }
    }
    for (name, on) in saved {
        if let Some(p) = have.get(&name.to_lowercase()) {
            if !p.base {
                out.push((p.name.clone(), *on));
            }
        }
    }
    let listed: HashSet<String> = out.iter().map(|(n, _)| n.to_lowercase()).collect();
    for p in plugins.iter().filter(|p| !listed.contains(&p.name.to_lowercase())) {
        out.push((p.name.clone(), false));
    }
    out
}

/// Masters a plugin needs that are not there at all.
pub fn missing_masters(p: &Plugin, plugins: &[Plugin]) -> Vec<String> {
    p.masters.iter().filter(|m| !plugins.iter().any(|x| x.name.eq_ignore_ascii_case(m))).cloned().collect()
}

/// Masters first, light and ordinary plugins after, every plugin after the plugins it needs; plugins
/// keep their current order whenever the rules allow it. Switched-on state is kept.
pub fn sort(order: &[(String, bool)], plugins: &[Plugin], layout: &Layout) -> Vec<(String, bool)> {
    let info: HashMap<String, &Plugin> = plugins.iter().map(|p| (p.name.to_lowercase(), p)).collect();
    let on: HashMap<String, bool> = order.iter().map(|(n, o)| (n.to_lowercase(), *o)).collect();
    let rank = |n: &str| -> (u8, usize) {
        let l = n.to_lowercase();
        if let Some(i) = layout.base.iter().position(|b| b.to_lowercase() == l) {
            return (0, i);
        }
        let kind = info.get(&l).map(|p| p.kind).unwrap_or("plugin");
        (if kind == "master" { 1 } else { 2 }, 0)
    };
    // current position breaks ties, so a sort changes as little as possible
    let pos: HashMap<String, usize> = order.iter().enumerate().map(|(i, (n, _))| (n.to_lowercase(), i)).collect();
    let mut names: Vec<String> = order.iter().map(|(n, _)| n.clone()).collect();
    names.sort_by_key(|n| (rank(n), pos.get(&n.to_lowercase()).copied().unwrap_or(usize::MAX)));
    // pull each plugin's masters in front of it (depth first, with a guard against loops)
    let mut done: Vec<String> = Vec::new();
    let mut state: BTreeMap<String, u8> = BTreeMap::new();
    fn visit(n: &str, names: &[String], info: &HashMap<String, &Plugin>, done: &mut Vec<String>, state: &mut BTreeMap<String, u8>) {
        let l = n.to_lowercase();
        if state.get(&l).copied().unwrap_or(0) != 0 {
            return;
        }
        state.insert(l.clone(), 1);
        if let Some(p) = info.get(&l) {
            for m in &p.masters {
                if let Some(real) = names.iter().find(|x| x.eq_ignore_ascii_case(m)) {
                    visit(real, names, info, done, state);
                }
            }
        }
        state.insert(l, 2);
        done.push(n.to_string());
    }
    for n in &names {
        visit(n, &names, &info, &mut done, &mut state);
    }
    done.into_iter().map(|n| { let o = on.get(&n.to_lowercase()).copied().unwrap_or(false); (n, o) }).collect()
}

/// Where `plugins.txt` is for a game (it may not exist yet).
pub fn plugins_txt_path(local_appdata: &Path, layout: &Layout) -> PathBuf {
    local_appdata.join(layout.appdata).join("plugins.txt")
}

#[cfg(test)]
pub mod tests_support {
    /// A plugin file the way Skyrim writes one: a header record with HEDR, CNAM, SNAM and MAST/DATA pairs.
    pub fn plugin_bytes(flags: u32, masters: &[&str], author: &str, desc: &str) -> Vec<u8> {
            let mut sub: Vec<u8> = Vec::new();
            let mut add = |k: &[u8; 4], d: &[u8]| {
                sub.extend(k);
                sub.extend((d.len() as u16).to_le_bytes());
                sub.extend(d);
            };
            add(b"HEDR", &[0u8; 12]);
            add(b"CNAM", format!("{author}\0").as_bytes());
            add(b"SNAM", format!("{desc}\0").as_bytes());
            for m in masters {
                add(b"MAST", format!("{m}\0").as_bytes());
                add(b"DATA", &[0u8; 8]);
            }
            let mut out = b"TES4".to_vec();
            out.extend((sub.len() as u32).to_le_bytes());
            out.extend(flags.to_le_bytes());
            out.extend([0u8; 4]); // form id
            out.extend([0u8; 4]); // version control info
            out.extend(44u16.to_le_bytes());
            out.extend([0u8; 2]);
            out.extend(sub);
            out
        }

}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fsutil::testutil::TempDir;

    use super::tests_support::plugin_bytes;

    fn layout() -> Layout {
        LAYOUTS[0].1
    }

    fn data(d: &TempDir, files: &[(&str, u32, &[&str])]) -> PathBuf {
        let data = d.join("Data");
        fs::create_dir_all(&data).unwrap();
        for (n, flags, masters) in files {
            fs::write(data.join(n), plugin_bytes(*flags, masters, "someone", "does things")).unwrap();
        }
        data
    }

    #[test]
    fn a_plugin_header_gives_flags_masters_author_and_description() {
        let h = parse_header(&plugin_bytes(0x201, &["Skyrim.esm", "Update.esm"], "Ann", "A fine mod")).unwrap();
        assert!(h.master_flag && h.light_flag);
        assert_eq!((h.masters.as_slice(), h.author.as_str(), h.description.as_str()), (&["Skyrim.esm".to_string(), "Update.esm".to_string()][..], "Ann", "A fine mod"));
        // Oblivion's shorter record header
        let mut old = plugin_bytes(0, &["Oblivion.esm"], "x", "y");
        old.drain(20..24);
        assert_eq!(parse_header(&old).unwrap().masters, vec!["Oblivion.esm"]);
        for bad in [&b""[..], b"TES4", b"NOPE0000000000000000000000000", &[0u8; 40]] {
            assert!(parse_header(bad).is_none());
        }
        // a header cut off in the middle gives what it has, never a panic
        let full = plugin_bytes(0, &["A.esm", "B.esm"], "x", "y");
        for n in 0..full.len() {
            let _ = parse_header(&full[..n]);
        }
    }

    #[test]
    fn the_game_is_recognised_by_its_program() {
        let d = TempDir::new("pl_layout");
        assert!(layout_for(d.path()).is_none());
        fs::write(d.join("skyrimse.EXE"), b"x").unwrap();
        assert_eq!(layout_for(d.path()).unwrap().appdata, "Skyrim Special Edition");
        let e = TempDir::new("pl_layout2");
        fs::write(e.join("Fallout4.exe"), b"x").unwrap();
        assert_eq!(layout_for(e.path()).unwrap().base[0], "Fallout4.esm");
    }

    #[test]
    fn plugins_txt_is_read_and_written_the_way_the_game_does() {
        let t = "# comment\r\n*Cool.esp\r\nOff.esp\r\n\r\n*cool.ESP\r\nnotes.txt\r\n*Big.esm\r\n";
        assert_eq!(parse_plugins_txt(t), vec![("Cool.esp".to_string(), true), ("Off.esp".to_string(), false), ("Big.esm".to_string(), true)], "comments, repeats and other files are dropped");
        let out = write_plugins_txt(&[("Skyrim.esm".into(), true), ("Big.esm".into(), true), ("Off.esp".into(), false)], &layout());
        assert!(out.contains("*Big.esm\r\nOff.esp\r\n") && !out.contains("Skyrim.esm"), "{out:?}");
        assert!(out.starts_with("# This file is used by the game"));
        assert_eq!(parse_plugins_txt(&out), vec![("Big.esm".to_string(), true), ("Off.esp".to_string(), false)], "it reads back as written");
    }

    #[test]
    fn the_data_folder_is_scanned_and_the_saved_order_is_merged_with_it() {
        let d = TempDir::new("pl_scan");
        let dir = data(&d, &[("Skyrim.esm", 1, &[]), ("Update.esm", 1, &["Skyrim.esm"]), ("Big.esm", 1, &["Skyrim.esm"]), ("Cool.esp", 0, &["Big.esm", "Gone.esm"]), ("New.esp", 0, &[]), ("Tiny.esl", 0x200, &[])]);
        fs::write(dir.join("readme.txt"), "x").unwrap();
        fs::write(dir.join("broken.esp"), b"not a plugin").unwrap();
        let ps = scan(&dir, &layout());
        assert_eq!(ps.iter().map(|p| p.name.as_str()).collect::<Vec<_>>(), vec!["Big.esm", "Cool.esp", "New.esp", "Skyrim.esm", "Tiny.esl", "Update.esm"]);
        let kind = |n: &str| ps.iter().find(|p| p.name == n).unwrap().kind;
        assert_eq!((kind("Big.esm"), kind("Cool.esp"), kind("Tiny.esl")), ("master", "plugin", "light"));
        assert!(ps.iter().find(|p| p.name == "Skyrim.esm").unwrap().base && !ps.iter().find(|p| p.name == "Big.esm").unwrap().base);
        let cool = ps.iter().find(|p| p.name == "Cool.esp").unwrap();
        assert_eq!(missing_masters(cool, &ps), vec!["Gone.esm"]);
        // saved: Cool (on), Ghost (gone from disk), Big (on); New and Tiny are new on disk
        let saved = vec![("Cool.esp".to_string(), true), ("Ghost.esp".to_string(), true), ("Big.esm".to_string(), true)];
        let order = merged_order(&ps, &saved, &layout());
        assert_eq!(
            order.iter().map(|(n, o)| format!("{}{}", if *o { "*" } else { "" }, n)).collect::<Vec<_>>(),
            vec!["*Skyrim.esm", "*Update.esm", "*Cool.esp", "*Big.esm", "New.esp", "Tiny.esl"],
            "{order:?}"
        );
    }

    #[test]
    fn sorting_puts_masters_first_and_everything_after_what_it_needs() {
        let d = TempDir::new("pl_sort");
        let dir = data(&d, &[("Skyrim.esm", 1, &[]), ("Update.esm", 1, &["Skyrim.esm"]), ("Big.esm", 1, &["Skyrim.esm"]), ("A.esp", 0, &["B.esp"]), ("B.esp", 0, &["Big.esm"]), ("C.esp", 0, &[]), ("Loop1.esp", 0, &["Loop2.esp"]), ("Loop2.esp", 0, &["Loop1.esp"]), ("Tiny.esl", 0x200, &["Big.esm"])]);
        let ps = scan(&dir, &layout());
        // a bad order: a plugin before its master, an ESM at the end
        let order: Vec<(String, bool)> = ["Skyrim.esm", "Update.esm", "A.esp", "C.esp", "B.esp", "Loop1.esp", "Loop2.esp", "Tiny.esl", "Big.esm"].iter().map(|n| (n.to_string(), *n != "C.esp")).collect();
        let sorted = sort(&order, &ps, &layout());
        let names: Vec<&str> = sorted.iter().map(|(n, _)| n.as_str()).collect();
        let at = |n: &str| names.iter().position(|x| *x == n).unwrap();
        assert_eq!(names.len(), order.len(), "nothing lost or doubled: {names:?}");
        assert!(at("Skyrim.esm") < at("Update.esm") && at("Update.esm") < at("Big.esm"), "{names:?}");
        assert!(at("Big.esm") < at("B.esp") && at("B.esp") < at("A.esp") && at("Big.esm") < at("Tiny.esl"), "{names:?}");
        assert!(at("Big.esm") < at("C.esp") && at("Big.esm") < at("A.esp"), "masters before ordinary plugins: {names:?}");
        assert!(sorted.iter().find(|(n, _)| n == "C.esp").map(|(_, o)| !o).unwrap(), "switched-off stays off");
        // sorting a sorted list changes nothing
        // (a pair of plugins that need each other cannot be satisfied: they just must not get lost)
        let calm = |v: &[(String, bool)]| v.iter().filter(|(n, _)| !n.starts_with("Loop")).cloned().collect::<Vec<_>>();
        let again = sort(&sorted, &ps, &layout());
        assert_eq!(calm(&again), calm(&sorted));
        assert_eq!(again.len(), sorted.len());
    }
}
