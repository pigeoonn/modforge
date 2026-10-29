//! Core mods that other mods need before they do anything: Cyberpunk 2077's RED4ext, ArchiveXL, TweakXL, Redscript and
//! Cyber Engine Tweaks, and the script extenders of Skyrim and Fallout 4. They are found from the files, so no website
//! or key is needed: a mod that brings `.archive.xl` files needs ArchiveXL, and so on. Each of them is also published as
//! a GitHub release, so "Make this game moddable" can put them in without a website click.

use std::path::Path;

/// A core mod that mods ask for but the game does not have.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Missing {
    pub name: &'static str,
    /// Where to get it (a mod page or the search for one).
    pub page: &'static str,
    pub needed_by: usize,
}

struct Framework {
    name: &'static str,
    page: &'static str,
    /// A file whose presence means it is installed (paths from the game folder, any letter case).
    present: &'static [&'static str],
    /// Does an enabled mod file (lower-case, forward slashes) need it?
    needs: fn(&str) -> bool,
    /// The GitHub repository (`owner/repo`) whose releases carry it.
    repo: &'static str,
    /// Which file of a release is the one to install (the name, lower-case).
    asset: fn(&str) -> bool,
}

/// A core mod of a game and whether the game has it, for the setup checklist.
#[derive(Clone, Copy)]
pub struct Core {
    pub name: &'static str,
    pub page: &'static str,
    pub repo: &'static str,
    pub installed: bool,
    asset: fn(&str) -> bool,
}

impl Core {
    /// The file of a release to install, out of its file names.
    pub fn pick<'a>(&self, names: &'a [String]) -> Option<&'a String> {
        names.iter().find(|n| (self.asset)(&n.to_lowercase()))
    }
}

fn zip_of(prefix: &str, n: &str) -> bool {
    n.starts_with(prefix) && n.ends_with(".zip") && !n.contains("pdb") && !n.contains("symbols")
}

fn has(game: &Path, rel: &str) -> bool {
    let mut cur = game.to_path_buf();
    for part in rel.split('/') {
        let want = part.to_lowercase();
        let found = std::fs::read_dir(&cur).ok().and_then(|rd| rd.flatten().find(|e| e.file_name().to_string_lossy().to_lowercase() == want).map(|e| e.path()));
        match found {
            Some(p) => cur = p,
            None => return false,
        }
    }
    true
}

// in the order they go in: RED4ext first, the plugins that live in its folder after it
const CYBERPUNK: &[Framework] = &[
    Framework { name: "RED4ext", page: "https://www.nexusmods.com/cyberpunk2077/mods/2380", present: &["red4ext/RED4ext.dll"], needs: |f| f.starts_with("red4ext/plugins/"), repo: "wopss/RED4ext", asset: |n| zip_of("red4ext-", n) },
    Framework {
        name: "Redscript",
        page: "https://www.nexusmods.com/cyberpunk2077/mods/1511",
        present: &["engine/tools/scc.exe"],
        needs: |f| f.starts_with("r6/scripts/") && f.ends_with(".reds"),
        repo: "jac3km4/redscript",
        asset: |n| n.starts_with("redscript-") && n.contains("windows") && n.ends_with(".zip"),
    },
    Framework {
        name: "Cyber Engine Tweaks",
        page: "https://www.nexusmods.com/cyberpunk2077/mods/107",
        present: &["bin/x64/plugins/cyber_engine_tweaks.asi"],
        needs: |f| f.starts_with("bin/x64/plugins/cyber_engine_tweaks/mods/"),
        repo: "maximegmd/CyberEngineTweaks",
        asset: |n| zip_of("cet_", n),
    },
    Framework { name: "ArchiveXL", page: "https://www.nexusmods.com/cyberpunk2077/mods/4198", present: &["red4ext/plugins/ArchiveXL/ArchiveXL.dll"], needs: |f| f.ends_with(".archive.xl"), repo: "psiberx/cp2077-archive-xl", asset: |n| zip_of("archivexl-", n) },
    Framework {
        name: "TweakXL",
        page: "https://www.nexusmods.com/cyberpunk2077/search/?gsearch=TweakXL&gsearchtype=mods",
        present: &["red4ext/plugins/TweakXL/TweakXL.dll"],
        needs: |f| f.starts_with("r6/tweaks/") && (f.ends_with(".yaml") || f.ends_with(".yml") || f.ends_with(".xl")),
        repo: "psiberx/cp2077-tweak-xl",
        asset: |n| zip_of("tweakxl-", n),
    },
    // nothing in a mod's files says it needs Codeware (its scripts just call it), so it is only offered by the setup
    Framework {
        name: "Codeware",
        page: "https://www.nexusmods.com/cyberpunk2077/search/?gsearch=Codeware&gsearchtype=mods",
        present: &["red4ext/plugins/Codeware/Codeware.dll"],
        needs: |_| false,
        repo: "psiberx/cp2077-codeware",
        asset: |n| zip_of("codeware-", n),
    },
];

const SKYRIM_SE: &[Framework] = &[Framework {
    name: "SKSE64",
    page: "https://skse.silverlock.org/",
    present: &["skse64_loader.exe"],
    needs: |f| f.starts_with("data/skse/plugins/") && f.ends_with(".dll"),
    repo: "ianpatt/skse64",
    asset: |n| n.starts_with("skse64_") && (n.ends_with(".7z") || n.ends_with(".zip")),
}];

const FALLOUT_4: &[Framework] = &[Framework {
    name: "F4SE",
    page: "https://f4se.silverlock.org/",
    present: &["f4se_loader.exe"],
    needs: |f| f.starts_with("data/f4se/plugins/") && f.ends_with(".dll"),
    repo: "ianpatt/f4se",
    asset: |n| n.starts_with("f4se_") && (n.ends_with(".7z") || n.ends_with(".zip")),
}];

const STARFIELD: &[Framework] = &[Framework {
    name: "SFSE",
    page: "https://www.nexusmods.com/starfield/mods/106",
    present: &["sfse_loader.exe"],
    needs: |f| f.starts_with("data/sfse/plugins/") && f.ends_with(".dll"),
    repo: "ianpatt/sfse",
    asset: |n| n.starts_with("sfse_") && (n.ends_with(".7z") || n.ends_with(".zip")),
}];

const FALLOUT_NV: &[Framework] = &[Framework {
    name: "xNVSE",
    page: "https://github.com/xNVSE/NVSE/releases",
    present: &["nvse_loader.exe"],
    needs: |f| f.starts_with("data/nvse/plugins/") && f.ends_with(".dll"),
    repo: "xNVSE/NVSE",
    asset: |n| n.starts_with("nvse_") && (n.ends_with(".7z") || n.ends_with(".zip")),
}];

/// Which table fits the game in `game`, by files only that game has.
fn table_for(game: &Path) -> Option<&'static [Framework]> {
    if has(game, "bin/x64/Cyberpunk2077.exe") {
        Some(CYBERPUNK)
    } else if has(game, "SkyrimSE.exe") {
        Some(SKYRIM_SE)
    } else if has(game, "Fallout4.exe") {
        Some(FALLOUT_4)
    } else if has(game, "Starfield.exe") {
        Some(STARFIELD)
    } else if has(game, "FalloutNV.exe") {
        Some(FALLOUT_NV)
    } else {
        None
    }
}

/// The core mods that the enabled mods ask for and the game does not have, with how many files ask.
pub fn missing(game: &Path, enabled_files: &[String]) -> Vec<Missing> {
    let Some(table) = table_for(game) else { return Vec::new() };
    let lower: Vec<String> = enabled_files.iter().map(|f| f.replace('\\', "/").to_lowercase()).collect();
    table
        .iter()
        .filter_map(|fw| {
            let n = lower.iter().filter(|f| (fw.needs)(f)).count();
            (n > 0 && !fw.present.iter().any(|p| has(game, p))).then_some(Missing { name: fw.name, page: fw.page, needed_by: n })
        })
        .collect()
}

/// The core mods that `files` (paths from the game folder) ask for, whether the game has them or not.
pub fn wanted(game: &Path, files: &[String]) -> Vec<Core> {
    let Some(table) = table_for(game) else { return Vec::new() };
    let lower: Vec<String> = files.iter().map(|f| f.replace('\\', "/").to_lowercase()).collect();
    table
        .iter()
        .filter(|fw| lower.iter().any(|f| (fw.needs)(f)))
        .map(|fw| Core { name: fw.name, page: fw.page, repo: fw.repo, installed: fw.present.iter().any(|p| has(game, p)), asset: fw.asset })
        .collect()
}

/// The core mods that `files` are: the files a core mod is recognised by are among them.
pub fn provided(game: &Path, files: &[String]) -> Vec<&'static str> {
    let Some(table) = table_for(game) else { return Vec::new() };
    let lower: Vec<String> = files.iter().map(|f| f.replace('\\', "/").to_lowercase()).collect();
    table.iter().filter(|fw| fw.present.iter().any(|p| lower.contains(&p.to_lowercase()))).map(|fw| fw.name).collect()
}

/// Every core mod of the game, in the order they go in, and whether each is there already. Empty for a game Gantry has
/// no table for.
pub fn cores(game: &Path) -> Vec<Core> {
    let Some(table) = table_for(game) else { return Vec::new() };
    table.iter().map(|fw| Core { name: fw.name, page: fw.page, repo: fw.repo, installed: fw.present.iter().any(|p| has(game, p)), asset: fw.asset }).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fsutil::testutil::TempDir;
    use std::fs;

    fn touch(p: &Path) {
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, "x").unwrap();
    }

    #[test]
    fn cyberpunk_mods_are_matched_with_the_core_mods_they_need() {
        let d = TempDir::new("fw");
        let game = d.join("Cyberpunk 2077");
        touch(&game.join("bin/x64/Cyberpunk2077.exe"));
        touch(&game.join("engine/tools/scc.exe"));
        touch(&game.join("bin/x64/Plugins/Cyber_Engine_Tweaks.asi"));
        let files: Vec<String> = ["archive/pc/mod/Cat.archive", "archive/pc/mod/Cat.archive.xl", "r6/tweaks/Cat.yaml", "r6/scripts/Cat.reds", "red4ext/plugins/Thing/Thing.dll", "bin/x64/plugins/cyber_engine_tweaks/mods/Foo/init.lua"].iter().map(|s| s.to_string()).collect();
        let m = missing(&game, &files);
        assert_eq!(m.iter().map(|x| (x.name, x.needed_by)).collect::<Vec<_>>(), vec![("RED4ext", 1), ("ArchiveXL", 1), ("TweakXL", 1)], "Redscript and CET are there (any letter case), the rest is not");
        // once they are installed nothing is reported
        touch(&game.join("red4ext/RED4ext.dll"));
        touch(&game.join("red4ext/plugins/ArchiveXL/ArchiveXL.dll"));
        touch(&game.join("red4ext/plugins/TweakXL/TweakXL.dll"));
        assert!(missing(&game, &files).is_empty());
        // a mod that asks for nothing special is fine without any of them
        assert!(missing(&game, &["archive/pc/mod/Plain.archive".to_string()]).is_empty());
    }

    #[test]
    fn other_games_only_get_the_script_extender_they_use() {
        let d = TempDir::new("fw2");
        let sky = d.join("Skyrim");
        touch(&sky.join("SkyrimSE.exe"));
        let plugin = vec!["Data\\SKSE\\Plugins\\Foo.dll".to_string()];
        assert_eq!(missing(&sky, &plugin).iter().map(|m| m.name).collect::<Vec<_>>(), vec!["SKSE64"]);
        touch(&sky.join("skse64_loader.exe"));
        assert!(missing(&sky, &plugin).is_empty());
        let plain = d.join("Plain");
        touch(&plain.join("plain.exe"));
        assert!(missing(&plain, &plugin).is_empty(), "a game Gantry knows nothing about is never nagged");
        assert!(cores(&plain).is_empty());
        assert_eq!(cores(&sky).iter().map(|c| (c.name, c.installed)).collect::<Vec<_>>(), vec![("SKSE64", true)]);
    }

    #[test]
    fn the_setup_knows_every_core_mod_and_which_release_file_is_the_mod() {
        let d = TempDir::new("fw3");
        let game = d.join("Cyberpunk 2077");
        touch(&game.join("bin/x64/Cyberpunk2077.exe"));
        touch(&game.join("engine/tools/scc.exe"));
        let c = cores(&game);
        assert_eq!(c.iter().map(|c| c.name).collect::<Vec<_>>(), vec!["RED4ext", "Redscript", "Cyber Engine Tweaks", "ArchiveXL", "TweakXL", "Codeware"], "RED4ext goes in first");
        assert_eq!(c.iter().filter(|c| c.installed).map(|c| c.name).collect::<Vec<_>>(), vec!["Redscript"]);
        let names = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        // the file lists of the real releases (October 2026)
        let pick = |name: &str, files: &[&str]| c.iter().find(|x| x.name == name).unwrap().pick(&names(files)).cloned();
        assert_eq!(pick("RED4ext", &["red4ext-symbols-1.30.0.zip", "red4ext-1.30.0.zip"]).as_deref(), Some("red4ext-1.30.0.zip"));
        assert_eq!(pick("ArchiveXL", &["ArchiveXL-1.27.5-pdb.zip", "ArchiveXL-1.27.5.zip"]).as_deref(), Some("ArchiveXL-1.27.5.zip"));
        assert_eq!(pick("TweakXL", &["TweakXL-1.11.5-pdb.zip", "TweakXL-1.11.5.zip"]).as_deref(), Some("TweakXL-1.11.5.zip"));
        assert_eq!(pick("Codeware", &["Codeware-1.20.6-pdb.zip", "Codeware-1.20.6.zip"]).as_deref(), Some("Codeware-1.20.6.zip"));
        assert_eq!(pick("Redscript", &["redscript-cli-aarch64-darwin", "redscript-cli.exe", "redscript-v0.5.32-macos.zip", "redscript-v0.5.32-windows.zip"]).as_deref(), Some("redscript-v0.5.32-windows.zip"));
        assert_eq!(pick("Cyber Engine Tweaks", &["cet_1_38_0.zip"]).as_deref(), Some("cet_1_38_0.zip"));
        assert_eq!(pick("RED4ext", &["red4ext-symbols-1.30.0.zip"]), None, "never the debug files");
    }

    #[test]
    fn what_a_mod_asks_for_and_which_core_mod_it_is_come_from_its_files() {
        let d = TempDir::new("fwwant");
        touch(&d.join("bin/x64/Cyberpunk2077.exe"));
        touch(&d.join("red4ext/RED4ext.dll"));
        let files = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<_>>();
        let w = wanted(&d.join(""), &files(&["archive/pc/mod/a.archive.xl", "red4ext/plugins/X/x.dll"]));
        let got: Vec<(&str, bool)> = w.iter().map(|c| (c.name, c.installed)).collect();
        assert_eq!(got, vec![("RED4ext", true), ("ArchiveXL", false)]);
        assert_eq!(provided(&d.join(""), &files(&["RED4EXT/red4ext.dll", "red4ext/config.ini"])), vec!["RED4ext"]);
        assert!(provided(&d.join(""), &files(&["archive/pc/mod/a.archive"])).is_empty());
        let other = TempDir::new("fwwant2");
        assert!(wanted(&other.join(""), &files(&["red4ext/plugins/X/x.dll"])).is_empty(), "a game without a table asks for nothing");
    }
}
