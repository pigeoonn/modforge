//! When ModForge itself fails: a short report on this computer that the person can read before sending it anywhere.
//! Nothing is sent by ModForge. The report holds the version, the system, where in the program it failed and the end
//! of the log, with the home folder written as `~`.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

/// How many reports are kept (the oldest go first).
const KEEP: usize = 10;

/// `text` with common authentication secrets (API keys, tokens, passwords) replaced with `[REDACTED]`.
pub fn redact_secrets(text: &str) -> String {
    let mut out = text.to_string();
    if let Ok(re_token) = regex::Regex::new(r#"(?i)(token|key|api_key|secret|password)=([a-zA-Z0-9_\-\.]{8,})"#) {
        out = re_token.replace_all(&out, "$1=[REDACTED]").to_string();
    }
    if let Ok(re_bearer) = regex::Regex::new(r#"(?i)(bearer\s+)([a-zA-Z0-9_\-\.]{8,})"#) {
        out = re_bearer.replace_all(&out, "$1[REDACTED]").to_string();
    }
    out
}

/// `text` with the person's home folder (in either slash style) written as `~`.
pub fn redact(text: &str, home: &str) -> String {
    let home = home.trim_end_matches(['/', '\\']);
    if home.len() < 3 {
        return text.to_string();
    }
    text.replace(home, "~").replace(&home.replace('\\', "/"), "~")
}

/// The last `max` bytes of `text`, starting at a line.
pub fn tail(text: &str, max: usize) -> &str {
    if text.len() <= max {
        return text;
    }
    let mut at = text.len() - max;
    while !text.is_char_boundary(at) {
        at += 1;
    }
    match text[at..].find('\n') {
        Some(n) => &text[at + n + 1..],
        None => &text[at..],
    }
}

/// Write one report into `dir` and keep only the newest few. Returns the file.
pub fn write(dir: &Path, now_ms: u64, version: &str, os: &str, what: &str, log_tail: &str, home: &str) -> io::Result<PathBuf> {
    fs::create_dir_all(dir)?;
    let body = format!(
        "ModForge {version} on {os}\n\nWhat failed:\n{what}\n\nThe end of the log:\n{log_tail}\n\nNothing in this file was sent anywhere. Read it before you attach it to a bug report.\n"
    );
    let path = dir.join(format!("crash-{now_ms}.txt"));
    let sanitized = redact(&redact_secrets(&body), home);
    fs::write(&path, sanitized)?;
    for old in list(dir).into_iter().skip(KEEP) {
        let _ = fs::remove_file(old);
    }
    Ok(path)
}

/// The reports in `dir`, newest first.
pub fn list(dir: &Path) -> Vec<PathBuf> {
    let mut found: Vec<(u64, PathBuf)> = fs::read_dir(dir)
        .map(|r| {
            r.flatten()
                .filter_map(|e| {
                    let name = e.file_name().to_string_lossy().to_string();
                    let ms = name.strip_prefix("crash-")?.strip_suffix(".txt")?.parse::<u64>().ok()?;
                    Some((ms, e.path()))
                })
                .collect()
        })
        .unwrap_or_default();
    found.sort_by_key(|f| std::cmp::Reverse(f.0));
    found.into_iter().map(|(_, p)| p).collect()
}

/// How many reports are newer than `seen_ms`.
pub fn unseen(dir: &Path, seen_ms: u64) -> usize {
    list(dir).iter().filter(|p| time_of(p) > seen_ms).count()
}

fn time_of(p: &Path) -> u64 {
    p.file_name().and_then(|n| n.to_str()).and_then(|n| n.strip_prefix("crash-")?.strip_suffix(".txt")?.parse().ok()).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_home_folder_never_ends_up_in_a_report() {
        assert_eq!(redact("at /home/ann/.cache/rig/x and /home/ann/y", "/home/ann"), "at ~/.cache/rig/x and ~/y");
        assert_eq!(redact(r"C:\Users\Ann\AppData and C:/Users/Ann/x", r"C:\Users\Ann"), r"~\AppData and ~/x");
        assert_eq!(redact("nothing here", "/home/ann/"), "nothing here");
        assert_eq!(redact("a/b", "/"), "a/b", "a bare root is not a home folder");
    }

    #[test]
    fn only_the_end_of_a_long_log_is_kept_from_a_whole_line() {
        let log = "one\ntwo\nthree\nfour\n";
        assert_eq!(tail(log, 100), log);
        assert_eq!(tail(log, 9), "four\n");
        assert_eq!(tail("ééééé", 3), "é", "never cuts a letter in half");
    }

    #[test]
    fn reports_are_written_listed_newest_first_and_pruned() {
        let d = std::env::temp_dir().join(format!("rig-crash-{}", crate::fsutil::unique_token()));
        for i in 1..=13u64 {
            write(&d, 1000 + i, "5.4.0", "Linux", "thread 'main' panicked at src/x.rs:1: boom in /home/ann/game", "line", "/home/ann").unwrap();
        }
        let l = list(&d);
        assert_eq!(l.len(), KEEP);
        assert!(l[0].ends_with("crash-1013.txt") && l[KEEP - 1].ends_with("crash-1004.txt"));
        let text = fs::read_to_string(&l[0]).unwrap();
        assert!(text.contains("ModForge 5.4.0 on Linux") && text.contains("boom in ~/game") && !text.contains("/home/ann"), "{text}");
        assert_eq!(unseen(&d, 0), KEEP);
        assert_eq!(unseen(&d, 1010), 3);
        assert_eq!(unseen(&d, 1013), 0);
        assert_eq!(unseen(&d.join("nothing"), 0), 0);
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn tokens_and_secrets_are_redacted() {
        let input = "GET /api/lan_info?token=12345678abcdef HTTP/1.1\nAuthorization: Bearer my_secret_bearer_token\n";
        let out = redact_secrets(input);
        assert!(!out.contains("12345678abcdef"));
        assert!(!out.contains("my_secret_bearer_token"));
        assert!(out.contains("token=[REDACTED]"));
        assert!(out.contains("Bearer [REDACTED]"));
    }
}
