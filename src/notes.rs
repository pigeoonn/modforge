//! The notification centre: the few things worth coming back to (a crash, updates, a failed download, a game update that
//! changed files, a modpack that is complete), kept in a short list the header bell opens.

use serde::Serialize;
use serde_json::{json, Value};

const KEEP: usize = 40;

#[derive(Clone, Debug, Serialize)]
pub struct Note {
    pub id: u64,
    pub ts: u64,
    /// "crash", "updates", "download", "gameupdate", "pack" or "waiting"
    pub kind: String,
    pub text: String,
    pub appid: String,
    pub read: bool,
}

#[derive(Default)]
pub struct Notes {
    list: Vec<Note>,
    next: u64,
}

impl Notes {
    /// Add a note. The same words again within a minute are not added twice.
    pub fn add(&mut self, now_ms: u64, kind: &str, text: &str, appid: &str) {
        if self.list.iter().rev().take(5).any(|n| n.kind == kind && n.text == text && n.appid == appid && now_ms.saturating_sub(n.ts) < 60_000) {
            return;
        }
        self.next += 1;
        self.list.push(Note { id: self.next, ts: now_ms, kind: kind.into(), text: text.chars().take(300).collect(), appid: appid.into(), read: false });
        if self.list.len() > KEEP {
            self.list.remove(0);
        }
    }

    pub fn mark_read(&mut self, id: Option<u64>) {
        for n in self.list.iter_mut().filter(|n| id.map(|i| i == n.id).unwrap_or(true)) {
            n.read = true;
        }
    }

    pub fn clear(&mut self) {
        self.list.clear();
    }

    /// Clear all notes belonging to a specific `appid`.
    pub fn clear_for_game(&mut self, appid: &str) {
        self.list.retain(|n| n.appid != appid);
    }

    /// Notes for a specific `appid`, newest first.
    pub fn for_game(&self, appid: &str) -> Vec<&Note> {
        self.list.iter().rev().filter(|n| n.appid == appid).collect()
    }

    /// Count of unread notes for a specific `appid`.
    pub fn unread_count_for_game(&self, appid: &str) -> usize {
        self.list.iter().filter(|n| n.appid == appid && !n.read).count()
    }

    /// Newest first, with the number not read yet.
    pub fn json(&self) -> Value {
        json!({ "unread": self.list.iter().filter(|n| !n.read).count(), "items": self.list.iter().rev().collect::<Vec<_>>() })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn notes_come_newest_first_are_not_repeated_and_can_be_read_and_cleared() {
        let mut n = Notes::default();
        n.add(1000, "crash", "Game crashed", "1");
        n.add(2000, "crash", "Game crashed", "1");
        assert_eq!(n.json()["items"].as_array().unwrap().len(), 1, "the same words within a minute count once");
        n.add(200_000, "crash", "Game crashed", "1");
        n.add(200_001, "updates", "2 updates", "");
        let j = n.json();
        assert_eq!((j["unread"].as_u64(), j["items"][0]["kind"].as_str()), (Some(3), Some("updates")));
        n.mark_read(Some(2));
        assert_eq!(n.json()["unread"], 2);
        n.mark_read(None);
        assert_eq!(n.json()["unread"], 0);
        for i in 0..60 {
            n.add(1_000_000 + i * 100_000, "x", &format!("n{i}"), "");
        }
        assert_eq!(n.json()["items"].as_array().unwrap().len(), KEEP, "only the newest are kept");
        n.clear();
        assert!(n.json()["items"].as_array().unwrap().is_empty());
    }

    #[test]
    fn per_game_notes_can_be_queried_counted_and_cleared() {
        let mut n = Notes::default();
        n.add(1000, "crash", "Cyberpunk crash", "1091500");
        n.add(2000, "updates", "Witcher updates", "292030");
        n.add(3000, "download", "Cyberpunk mod done", "1091500");

        assert_eq!(n.for_game("1091500").len(), 2);
        assert_eq!(n.unread_count_for_game("1091500"), 2);
        assert_eq!(n.unread_count_for_game("292030"), 1);

        n.clear_for_game("1091500");
        assert_eq!(n.for_game("1091500").len(), 0);
        assert_eq!(n.unread_count_for_game("1091500"), 0);
        assert_eq!(n.for_game("292030").len(), 1);
    }
}
