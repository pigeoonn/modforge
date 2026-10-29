//! Scheduled backups of Gantry's own settings and mod lists: one file per week, newest eight kept.
//! Only the file naming, schedule and retention live here; `main.rs` builds and restores the bundle.

use std::fs;
use std::path::Path;

pub const INTERVAL_MS: u64 = 7 * 24 * 3600 * 1000;
pub const KEEP: usize = 8;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub name: String,
    pub at_ms: u64,
    pub size: u64,
}

pub fn file_name(at_ms: u64) -> String {
    format!("modforge-{:013}.json", at_ms)
}

/// The timestamp in a backup file name; `None` for anything that is not one of ours.
pub fn parse_name(name: &str) -> Option<u64> {
    let digits = name.strip_prefix("modforge-").or_else(|| name.strip_prefix("gantry-"))?.strip_suffix(".json")?;
    if digits.is_empty() || digits.len() > 20 || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    digits.parse().ok()
}

/// Backups in `dir`, newest first.
pub fn list(dir: &Path) -> Vec<Entry> {
    let mut out: Vec<Entry> = fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().to_string();
            let at_ms = parse_name(&name)?;
            let meta = e.metadata().ok().filter(|m| m.is_file())?;
            Some(Entry { name, at_ms, size: meta.len() })
        })
        .collect();
    out.sort_by_key(|e| std::cmp::Reverse(e.at_ms));
    out
}

/// Whether a new backup is due: none yet, or the newest is a full interval old.
pub fn due(dir: &Path, now_ms: u64) -> bool {
    match list(dir).first() {
        None => true,
        Some(newest) => now_ms.saturating_sub(newest.at_ms) >= INTERVAL_MS,
    }
}

pub fn prune(dir: &Path, keep: usize) {
    for old in list(dir).into_iter().skip(keep) {
        let _ = fs::remove_file(dir.join(old.name));
    }
}

/// Write `json` as the backup for `at_ms`, then drop the ones beyond `KEEP`.
pub fn store(dir: &Path, at_ms: u64, json: &str) -> Result<String, String> {
    fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    let name = file_name(at_ms);
    crate::fsutil::atomic_write(&dir.join(&name), json).map_err(|e| e.to_string())?;
    prune(dir, KEEP);
    Ok(name)
}

/// Read one backup by file name. Names are checked so nothing outside `dir` can be reached.
pub fn read(dir: &Path, name: &str) -> Result<serde_json::Value, String> {
    parse_name(name).ok_or_else(|| "That is not a backup of ModForge.".to_string())?;
    let bytes = fs::read(dir.join(name)).map_err(|_| "That backup is gone.".to_string())?;
    serde_json::from_slice(&bytes).map_err(|_| "This file is not a ModForge backup.".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(tag: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("modforge-autobackup-{}-{}", tag, crate::fsutil::unique_token()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn names_round_trip_and_reject_strangers() {
        assert_eq!(parse_name(&file_name(1_700_000_000_000)), Some(1_700_000_000_000));
        for bad in ["gantry-.json", "gantry-12a.json", "../gantry-1.json", "gantry-1.json.bak", "rig_config-1.json", "gantry-1/../x.json", "gantry--1.json"] {
            assert_eq!(parse_name(bad), None, "{bad}");
        }
    }

    #[test]
    fn a_backup_is_due_after_a_week_and_not_before() {
        let d = tmp("due");
        assert!(due(&d, 5));
        store(&d, 1_000, "{}").unwrap();
        assert!(!due(&d, 1_000 + INTERVAL_MS - 1));
        assert!(due(&d, 1_000 + INTERVAL_MS));
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn only_the_newest_eight_are_kept() {
        let d = tmp("keep");
        for i in 1..=12u64 {
            store(&d, i * 1_000, "{}").unwrap();
        }
        fs::write(d.join("notes.txt"), "mine").unwrap();
        let names: Vec<u64> = list(&d).iter().map(|e| e.at_ms).collect();
        assert_eq!(names, (5..=12u64).rev().map(|i| i * 1_000).collect::<Vec<_>>());
        assert!(d.join("notes.txt").exists(), "files that are not backups stay");
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn reading_checks_the_name_and_the_content() {
        let d = tmp("read");
        store(&d, 7, "{\"gantry_backup\":1}").unwrap();
        assert_eq!(read(&d, &file_name(7)).unwrap()["gantry_backup"], 1);
        assert!(read(&d, "../../etc/passwd").is_err());
        assert!(read(&d, &file_name(8)).is_err());
        fs::write(d.join(file_name(9)), "nope").unwrap();
        assert!(read(&d, &file_name(9)).is_err());
        let _ = fs::remove_dir_all(&d);
    }
}
