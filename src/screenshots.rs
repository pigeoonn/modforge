//! A game's Steam screenshots, told apart by whether the game was played with mods or without them at that moment.
//! Read-only: Steam keeps its pictures in `userdata/<account>/760/remote/<game>/screenshots/` (with small copies in
//! `thumbnails/`); Gantry only lists them and, for the browser, hands out one picture at a time by its place in the list.

use crate::stats::Session;
use serde::Serialize;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

/// How many pictures one list holds (the newest).
const MAX: usize = 300;
/// A picture taken this long before a session started or after it ended still belongs to it (the file is written at the
/// moment of the key press, the session record at the moments the game started and closed).
const SLACK_MS: u64 = 90_000;
/// Pictures bigger than this are not handed out.
pub const MAX_BYTES: u64 = 30_000_000;

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Shot {
    pub name: String,
    pub at_ms: u64,
    /// "modded" (a session with mods was running), "plain" (one without), or "unknown".
    pub kind: &'static str,
    #[serde(skip)]
    pub path: PathBuf,
    #[serde(skip)]
    pub thumb: Option<PathBuf>,
}

fn is_picture(p: &Path) -> bool {
    p.extension().map(|e| matches!(e.to_string_lossy().to_lowercase().as_str(), "jpg" | "jpeg" | "png")).unwrap_or(false)
}

fn digits(s: &str) -> bool {
    !s.is_empty() && s.len() <= 20 && s.bytes().all(|b| b.is_ascii_digit())
}

/// The kind of a picture taken at `at_ms`, from the game's sessions.
pub fn kind_at(at_ms: u64, sessions: &[Session]) -> &'static str {
    // the session that is closest to it wins when two overlap their slack
    sessions
        .iter()
        .filter(|s| at_ms + SLACK_MS >= s.start_ms && at_ms <= s.end_ms.max(s.start_ms) + SLACK_MS)
        .min_by_key(|s| if at_ms >= s.start_ms && at_ms <= s.end_ms { 0 } else { at_ms.abs_diff(s.start_ms).min(at_ms.abs_diff(s.end_ms)) })
        .map(|s| if s.mods > 0 { "modded" } else { "plain" })
        .unwrap_or("unknown")
}

/// Every Steam screenshot of `appid` in these Steam installs, newest first.
pub fn list(steam_roots: &[PathBuf], appid: &str, sessions: &[Session]) -> Vec<Shot> {
    list_with_dirs(steam_roots, appid, sessions, &[])
}

/// Every Steam and local game screenshot from extra folders (e.g. game/Screenshots or Wine prefix Pictures), newest first.
pub fn list_with_dirs(steam_roots: &[PathBuf], appid: &str, sessions: &[Session], extra_dirs: &[PathBuf]) -> Vec<Shot> {
    let mut out: Vec<Shot> = Vec::new();
    if digits(appid) {
        for root in steam_roots {
            for acct in fs::read_dir(root.join("userdata")).into_iter().flatten().flatten() {
                let dir = acct.path().join("760/remote").join(appid).join("screenshots");
                for e in fs::read_dir(&dir).into_iter().flatten().flatten() {
                    let path = e.path();
                    let Ok(meta) = e.metadata() else { continue };
                    if !meta.is_file() || meta.len() > MAX_BYTES || !is_picture(&path) {
                        continue;
                    }
                    let Some(at_ms) = meta.modified().ok().and_then(|t| t.duration_since(UNIX_EPOCH).ok()).map(|d| d.as_millis() as u64) else { continue };
                    let name = e.file_name().to_string_lossy().to_string();
                    let thumb = dir.join("thumbnails").join(&name);
                    out.push(Shot { name, at_ms, kind: kind_at(at_ms, sessions), thumb: thumb.is_file().then_some(thumb), path });
                }
            }
        }
    }

    for dir in extra_dirs {
        for e in fs::read_dir(dir).into_iter().flatten().flatten() {
            let path = e.path();
            let Ok(meta) = e.metadata() else { continue };
            if !meta.is_file() || meta.len() > MAX_BYTES || !is_picture(&path) {
                continue;
            }
            let Some(at_ms) = meta.modified().ok().and_then(|t| t.duration_since(UNIX_EPOCH).ok()).map(|d| d.as_millis() as u64) else { continue };
            let name = e.file_name().to_string_lossy().to_string();
            if out.iter().any(|s| s.path == path || s.name == name) {
                continue;
            }
            let thumb = dir.join("thumbnails").join(&name);
            out.push(Shot { name, at_ms, kind: kind_at(at_ms, sessions), thumb: thumb.is_file().then_some(thumb), path });
        }
    }

    out.sort_by(|a, b| b.at_ms.cmp(&a.at_ms).then_with(|| a.name.cmp(&b.name)));
    out.truncate(MAX);
    out
}

/// The picture bytes and their type for one place in the list; `thumb` asks for the small copy when there is one.
pub fn bytes(shot: &Shot, thumb: bool) -> Option<(Vec<u8>, &'static str)> {
    let path = if thumb { shot.thumb.as_ref().unwrap_or(&shot.path) } else { &shot.path };
    let ctype = match path.extension()?.to_string_lossy().to_lowercase().as_str() {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        _ => return None,
    };
    if fs::metadata(path).ok()?.len() > MAX_BYTES {
        return None;
    }
    Some((fs::read(path).ok()?, ctype))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fsutil::testutil::TempDir;
    use std::time::{Duration, SystemTime};

    fn session(start: u64, end: u64, mods: usize) -> Session {
        Session { appid: "620".into(), name: "G".into(), start_ms: start, end_ms: end, secs: (end - start) / 1000, crashed: false, mods, perf: None, ..Default::default() }
    }

    fn picture(dir: &Path, name: &str, at_ms: u64, with_thumb: bool) {
        fs::create_dir_all(dir.join("thumbnails")).unwrap();
        let p = dir.join(name);
        fs::write(&p, b"\xff\xd8JPEG").unwrap();
        fs::File::options().write(true).open(&p).unwrap().set_modified(SystemTime::UNIX_EPOCH + Duration::from_millis(at_ms)).unwrap();
        if with_thumb {
            fs::write(dir.join("thumbnails").join(name), b"\xff\xd8thumb").unwrap();
        }
    }

    #[test]
    fn a_picture_is_told_apart_by_the_session_it_was_taken_in() {
        let s = vec![session(1_000_000, 2_000_000, 3), session(5_000_000, 6_000_000, 0)];
        assert_eq!(kind_at(1_500_000, &s), "modded");
        assert_eq!(kind_at(5_500_000, &s), "plain");
        assert_eq!(kind_at(2_050_000, &s), "modded", "a few seconds after the game closed still counts");
        assert_eq!(kind_at(3_500_000, &s), "unknown", "between sessions nothing is known");
        assert_eq!(kind_at(10, &[]), "unknown");
        // two sessions that nearly meet: the one the picture is inside wins
        let close = vec![session(0, 1_000_000, 2), session(1_050_000, 2_000_000, 0)];
        assert_eq!(kind_at(1_100_000, &close), "plain");
        assert_eq!(kind_at(990_000, &close), "modded");
    }

    #[test]
    fn steams_pictures_are_listed_newest_first_with_their_kind_and_only_for_that_game() {
        let d = TempDir::new("shots");
        let root = d.join("Steam");
        let mine = root.join("userdata/111/760/remote/620/screenshots");
        picture(&mine, "20260101_1.jpg", 1_500_000, true);
        picture(&mine, "20260102_1.jpg", 5_500_000, false);
        picture(&mine, "20260103_1.jpg", 9_000_000, true);
        fs::write(mine.join("notes.txt"), "x").unwrap();
        picture(&root.join("userdata/111/760/remote/730/screenshots"), "other_1.jpg", 1_500_000, false);
        // a second account of the same computer
        picture(&root.join("userdata/222/760/remote/620/screenshots"), "20260104_1.jpg", 1_600_000, false);
        let sessions = vec![session(1_000_000, 2_000_000, 3), session(5_000_000, 6_000_000, 0)];
        let l = list(&[d.join("none"), root.clone()], "620", &sessions);
        assert_eq!(l.iter().map(|s| (s.name.as_str(), s.kind)).collect::<Vec<_>>(), vec![("20260103_1.jpg", "unknown"), ("20260102_1.jpg", "plain"), ("20260104_1.jpg", "modded"), ("20260101_1.jpg", "modded")]);
        assert!(l[0].thumb.is_some() && l[1].thumb.is_none());
        assert!(list(std::slice::from_ref(&root), "../620", &sessions).is_empty() && list(&[root], "999", &sessions).is_empty());
    }

    #[test]
    fn the_bytes_come_from_the_list_only_and_the_small_copy_is_used_when_there_is_one() {
        let d = TempDir::new("shotbytes");
        let dir = d.join("Steam/userdata/1/760/remote/620/screenshots");
        picture(&dir, "a.jpg", 1000, true);
        picture(&dir, "b.jpg", 500, false);
        let l = list(&[d.join("Steam")], "620", &[]);
        assert_eq!(bytes(&l[0], true).unwrap(), (b"\xff\xd8thumb".to_vec(), "image/jpeg"));
        assert_eq!(bytes(&l[0], false).unwrap().0, b"\xff\xd8JPEG".to_vec());
        assert_eq!(bytes(&l[1], true).unwrap().0, b"\xff\xd8JPEG".to_vec(), "no small copy: the picture itself");
        // something that is not a picture is never handed out
        let bad = Shot { name: "x".into(), at_ms: 0, kind: "unknown", path: dir.join("../../../../../../../etc/passwd"), thumb: None };
        assert!(bytes(&bad, false).is_none());
    }

    #[test]
    fn extra_screenshot_folders_are_scanned_even_without_steam_id() {
        let d = TempDir::new("custom_shots");
        let extra = d.join("Screenshots");
        picture(&extra, "shot1.png", 2_000_000, false);
        picture(&extra, "shot2.jpg", 3_000_000, true);
        let sessions = vec![session(1_500_000, 2_500_000, 2)];
        let l = list_with_dirs(&[], "custom_game_999", &sessions, &[extra]);
        assert_eq!(l.len(), 2);
        assert_eq!(l[0].name, "shot2.jpg");
        assert_eq!(l[0].kind, "unknown");
        assert_eq!(l[1].name, "shot1.png");
        assert_eq!(l[1].kind, "modded");
    }
}
