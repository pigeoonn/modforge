//! One place to read every log a game and its loaders write: the loader's own log, Unity's `Player.log`,
//! UE4SS, the engine log and Proton's. Only files Gantry itself finds are ever read.

use crate::engine::GameEntry;
use serde::Serialize;
use std::fs;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

/// Lines kept per file: the end of a long log is what matters.
const TAIL_BYTES: u64 = 600_000;
/// Lines handed to the screen at most.
pub const MAX_LINES: usize = 1500;

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct LogFile {
    pub id: String,
    pub modified_ms: u64,
    pub size: u64,
    #[serde(skip)]
    pub path: PathBuf,
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct Line {
    pub text: String,
    /// "bad", "warn" or "".
    pub level: &'static str,
}

fn meta_of(id: &str, path: PathBuf) -> Option<LogFile> {
    let m = fs::metadata(&path).ok().filter(|m| m.is_file())?;
    let modified_ms = m.modified().ok()?.duration_since(UNIX_EPOCH).ok()?.as_millis() as u64;
    Some(LogFile { id: id.into(), modified_ms, size: m.len(), path })
}

fn find_reframework_log(dir: &Path) -> Option<PathBuf> {
    for candidate in [
        dir.join("re_framework_log.txt"),
        dir.join("reframework/re_framework_log.txt"),
        dir.join("reframework/re2_framework_log.txt"),
        dir.join("reframework/re3_framework_log.txt"),
        dir.join("reframework/re4_framework_log.txt"),
        dir.join("reframework/re7_framework_log.txt"),
        dir.join("reframework/re8_framework_log.txt"),
        dir.join("reframework/mhrise_framework_log.txt"),
        dir.join("reframework/sf6_framework_log.txt"),
        dir.join("reframework/dd2_framework_log.txt"),
    ] {
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    let rf = dir.join("reframework");
    if let Ok(entries) = fs::read_dir(rf) {
        for e in entries.flatten() {
            let p = e.path();
            if p.is_file() && p.file_name().map(|n| n.to_string_lossy().ends_with("_framework_log.txt")).unwrap_or(false) {
                return Some(p);
            }
        }
    }
    None
}

fn find_witcher_log(dir: &Path) -> Option<PathBuf> {
    for candidate in [dir.join("bin/x64_dx12/scriptlog.txt"), dir.join("bin/x64/scriptlog.txt")] {
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

/// The logs that exist for a game, newest first.
pub fn files(game: &GameEntry, proton_log: Option<PathBuf>) -> Vec<LogFile> {
    let dir = Path::new(&game.game_dir);
    let prefixes: Vec<PathBuf> = [game.pfx.clone(), format!("{}/pfx", game.compatdata)].into_iter().filter(|p| !p.is_empty() && p != "/pfx").map(PathBuf::from).collect();
    let mut out: Vec<LogFile> = Vec::new();
    let mut add = |id: &str, p: Option<PathBuf>| {
        if let Some(f) = p.and_then(|p| meta_of(id, p)) {
            out.push(f);
        }
    };
    add("bepinex", Some(dir.join("BepInEx/LogOutput.log")));
    add("melonloader", Some(dir.join("MelonLoader/Latest.log")));
    add("reframework", find_reframework_log(dir));
    add("witcher3", find_witcher_log(dir));
    add("red4ext", crate::kb::newest_log_in(&dir.join("red4ext/logs")));
    add("redscript", Some(dir.join("r6/logs/redscript_rCURRENT.log")));
    add("cet", Some(dir.join("bin/x64/plugins/cyber_engine_tweaks/cyber_engine_tweaks.log")));
    add("player", prefixes.iter().find_map(|p| crate::logdoctor::newest_player_log(p)));
    let (ue4ss, engine) = crate::crash::unreal_log_files(game);
    add("ue4ss", ue4ss);
    add("game", engine);
    add("proton", proton_log);
    out.sort_by_key(|f| std::cmp::Reverse(f.modified_ms));
    out
}

fn level_of(line: &str) -> &'static str {
    let l = line.to_lowercase();
    if l.contains("error") || l.contains("exception") || l.contains("fatal") || l.contains("crash") || l.contains("[err") || l.contains("failed") {
        "bad"
    } else if l.contains("warn") {
        "warn"
    } else {
        ""
    }
}

/// The end of a file, lines containing `query` (any case) only when one is given. Returns the lines (at most
/// `MAX_LINES`, the newest ones) and how many matched in all.
pub fn read(path: &Path, query: &str) -> std::io::Result<(Vec<Line>, usize)> {
    let mut f = fs::File::open(path)?;
    let len = f.metadata()?.len();
    let start = len.saturating_sub(TAIL_BYTES);
    f.seek(SeekFrom::Start(start))?;
    let mut bytes = Vec::new();
    f.take(TAIL_BYTES).read_to_end(&mut bytes)?;
    let text = String::from_utf8_lossy(&bytes);
    let q = query.trim().to_lowercase();
    let mut lines: Vec<&str> = text.lines().collect();
    if start > 0 && !lines.is_empty() {
        lines.remove(0); // a cut line
    }
    let matched: Vec<&str> = lines.into_iter().filter(|l| q.is_empty() || l.to_lowercase().contains(&q)).collect();
    let total = matched.len();
    let from = total.saturating_sub(MAX_LINES);
    Ok((matched[from..].iter().map(|l| Line { text: l.chars().take(2000).collect(), level: level_of(l) }).collect(), total))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fsutil::testutil::TempDir;

    #[test]
    fn the_end_of_a_big_log_is_read_and_filtered() {
        let d = TempDir::new("gamelogs");
        let p = d.join("a.log");
        let mut text = String::new();
        for i in 0..50_000 {
            text.push_str(&format!("line {i} ok\n"));
        }
        text.push_str("[ERR] something broke\nWarning: careful\nlast line\n");
        fs::write(&p, &text).unwrap();
        let (lines, total) = read(&p, "").unwrap();
        assert_eq!(lines.len(), MAX_LINES);
        assert_eq!(lines.last().unwrap().text, "last line");
        assert_eq!((lines[lines.len() - 3].level, lines[lines.len() - 2].level, lines[lines.len() - 1].level), ("bad", "warn", ""));
        assert!(total > MAX_LINES);
        let (hit, n) = read(&p, "BROKE").unwrap();
        assert_eq!((hit.len(), n, hit[0].text.as_str()), (1, 1, "[ERR] something broke"));
        assert_eq!(read(&p, "no such text").unwrap().0.len(), 0);
        assert!(read(&d.join("missing.log"), "").is_err());
        // a line is cut to a sane length, and bytes that are not text do not break anything
        fs::write(&p, [b"x".repeat(5000), b"\n\xff\xfe\n".to_vec()].concat()).unwrap();
        let (l2, _) = read(&p, "").unwrap();
        assert_eq!((l2.len(), l2[0].text.chars().count()), (2, 2000));
    }

    #[test]
    fn reframework_and_witcher3_logs_are_discovered() {
        let d = TempDir::new("re_witcher_logs");
        let rf_dir = d.join("reframework");
        fs::create_dir_all(&rf_dir).unwrap();
        fs::write(rf_dir.join("re4_framework_log.txt"), "REFramework initialized\n").unwrap();
        let w_dir = d.join("bin/x64_dx12");
        fs::create_dir_all(&w_dir).unwrap();
        fs::write(w_dir.join("scriptlog.txt"), "[Script] Mod loaded\n").unwrap();

        let mut ge = crate::repair::fixtures::make_game("re_witcher").1;
        ge.game_dir = d.path().to_string_lossy().to_string();

        let list = files(&ge, None);
        assert!(list.iter().any(|f| f.id == "reframework"));
        assert!(list.iter().any(|f| f.id == "witcher3"));
    }
}
