//! The "find the culprit" state machine behind Crash Detective.
//!
//! Pure logic, no I/O: the caller applies `testing` (the mods that must be enabled for this
//! round), the user launches the game and reports whether it crashed, and `answer` narrows the
//! suspect pool. It copes with several culprits (confirming each by re-testing everything
//! else) and gives up with an explanation instead of looping when the crash looks like an
//! interaction between mods or is not caused by mods at all.

use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    /// Every user mod off: if the game still crashes, mods are not the cause.
    Baseline,
    /// Halving the suspect pool.
    Narrow,
    /// One suspect left: confirm it by enabling every other mod.
    Verify,
    Done,
    GaveUp,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Bisect {
    pub appid: String,
    /// User mods that were enabled when the session began (restored on finish).
    pub originals: Vec<String>,
    pub pool: Vec<String>,
    pub found: Vec<String>,
    /// Mods that must be enabled for the current round (every other user mod is off).
    pub testing: Vec<String>,
    pub phase: Phase,
    pub round: u32,
    pub max_rounds: u32,
    pub started_ms: u64,
    pub round_started_ms: u64,
    pub note: String,
    /// The mods under test live in the game's Mod Library (not UE4SS/PAK folders).
    #[serde(default)]
    pub library: bool,
    /// Names for the ids when they are library mods.
    #[serde(default)]
    pub names: std::collections::BTreeMap<String, String>,
}

fn ceil_log2(n: usize) -> u32 {
    usize::BITS - n.saturating_sub(1).leading_zeros()
}

impl Bisect {
    /// `pool_order` lists the suspects, likeliest first (that half is tested first). Returns
    /// `None` when there are no enabled user mods to test.
    pub fn new(appid: &str, originals: Vec<String>, pool_order: Vec<String>, now_ms: u64) -> Option<Self> {
        if originals.is_empty() {
            return None;
        }
        let mut pool: Vec<String> = pool_order.into_iter().filter(|m| originals.contains(m)).collect();
        for m in &originals {
            if !pool.contains(m) {
                pool.push(m.clone());
            }
        }
        let max_rounds = 2 * ceil_log2(originals.len() + 1) + 7;
        let single = originals.len() == 1; // "all off" and "confirm the suspect" are the same test
        let mut b = Bisect {
            appid: appid.to_string(),
            originals,
            pool,
            found: Vec::new(),
            testing: Vec::new(),
            phase: Phase::Baseline,
            round: 1,
            max_rounds,
            started_ms: now_ms,
            round_started_ms: now_ms,
            note: String::new(),
            library: false,
            names: Default::default(),
        };
        if single {
            b.plan_round();
        }
        Some(b)
    }

    pub fn is_finished(&self) -> bool {
        matches!(self.phase, Phase::Done | Phase::GaveUp)
    }

    /// Set `testing` (and the phase) for the round about to be played.
    fn plan_round(&mut self) {
        if self.pool.len() <= 1 {
            self.phase = Phase::Verify;
            self.testing = self
                .originals
                .iter()
                .filter(|m| !self.found.contains(m) && !self.pool.contains(m))
                .cloned()
                .collect();
        } else {
            self.phase = Phase::Narrow;
            let half = self.pool.len().div_ceil(2);
            self.testing = self.pool[..half].to_vec();
        }
    }

    /// Record the outcome of the current round and plan the next one.
    pub fn answer(&mut self, crashed: bool, now_ms: u64) {
        if self.is_finished() {
            return;
        }
        match self.phase {
            Phase::Baseline => {
                if crashed {
                    self.phase = Phase::GaveUp;
                    self.note = "The game still crashes with every user mod off, so the crash is not caused by your mods.".into();
                    return;
                }
            }
            Phase::Narrow => {
                if crashed {
                    self.pool = self.testing.clone();
                } else {
                    let tested = self.testing.clone();
                    self.pool.retain(|m| !tested.contains(m));
                }
            }
            Phase::Verify => {
                let suspect = self.pool.first().cloned();
                if let Some(s) = suspect {
                    self.found.push(s);
                }
                if !crashed {
                    self.phase = Phase::Done;
                    self.note = "Confirmed: the game runs once the culprit is off.".into();
                    return;
                }
                // still crashes without it: another culprit (or not mods at all)
                self.pool = self.originals.iter().filter(|m| !self.found.contains(m)).cloned().collect();
                if self.pool.is_empty() {
                    self.phase = Phase::GaveUp;
                    self.found.clear();
                    self.note = "The game still crashes with every user mod off, so the crash is not caused by your mods.".into();
                    return;
                }
            }
            Phase::Done | Phase::GaveUp => return,
        }
        self.round += 1;
        self.round_started_ms = now_ms;
        if self.round > self.max_rounds || self.found.len() >= 3 {
            self.phase = Phase::GaveUp;
            self.note = "No single mod explains the crash; it looks like an interaction between several mods, or something other than mods.".into();
            return;
        }
        self.plan_round();
    }

    /// Rough number of rounds still to play (for the progress line).
    pub fn rounds_left(&self) -> u32 {
        match self.phase {
            Phase::Done | Phase::GaveUp => 0,
            Phase::Verify => 1,
            Phase::Baseline => ceil_log2(self.pool.len()) + 2,
            Phase::Narrow => ceil_log2(self.pool.len()) + 1,
        }
    }

    /// Estimated progress percentage (0..100) through the diagnostic process.
    pub fn progress_pct(&self) -> u32 {
        if self.is_finished() {
            return 100;
        }
        let total_est = self.round + self.rounds_left();
        if total_est == 0 {
            return 0;
        }
        ((self.round.saturating_sub(1) * 100) / total_est).min(95)
    }

    /// The set of mods to leave enabled when the session ends.
    pub fn final_enabled(&self, restore_all: bool) -> Vec<String> {
        if restore_all || self.phase == Phase::GaveUp {
            self.originals.clone()
        } else {
            self.originals.iter().filter(|m| !self.found.contains(m)).cloned().collect()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ids(n: usize) -> Vec<String> {
        (0..n).map(|i| format!("LUA:Mod{:02}", i)).collect()
    }

    /// Play a whole session against an oracle: `crash(enabled)` says whether the game crashes.
    fn run(n: usize, order: Vec<String>, crash: impl Fn(&[String]) -> bool) -> (Bisect, u32) {
        let mut b = Bisect::new("1", ids(n), order, 0).unwrap();
        let mut rounds = 0;
        while !b.is_finished() {
            let c = crash(&b.testing);
            b.answer(c, 0);
            rounds += 1;
            assert!(rounds < 100, "session never ends: {:?}", b);
        }
        (b, rounds)
    }

    #[test]
    fn finds_a_single_culprit_in_logarithmic_rounds() {
        for n in 1..=40usize {
            for culprit in 0..n {
                let c = format!("LUA:Mod{:02}", culprit);
                let (b, rounds) = run(n, vec![], |en| en.contains(&c));
                assert_eq!(b.phase, Phase::Done, "n={n} culprit={culprit}: {:?}", b);
                assert_eq!(b.found, vec![c.clone()]);
                assert!(rounds <= ceil_log2(n) + 2, "n={n}: {rounds} rounds");
                assert_eq!(b.final_enabled(false).len(), n - 1);
                assert!(!b.final_enabled(false).contains(&c));
                assert_eq!(b.final_enabled(true), ids(n));
            }
        }
    }

    #[test]
    fn finds_two_and_three_independent_culprits() {
        let c = ["LUA:Mod03", "LUA:Mod11"];
        let (b, _) = run(16, vec![], |en| c.iter().any(|x| en.iter().any(|e| e == x)));
        assert_eq!(b.phase, Phase::Done);
        let mut f = b.found.clone();
        f.sort();
        assert_eq!(f, c.to_vec());
        assert!(!b.final_enabled(false).iter().any(|m| c.contains(&m.as_str())));

        let c = ["LUA:Mod00", "LUA:Mod07", "LUA:Mod08"];
        let (b, _) = run(10, vec![], |en| c.iter().any(|x| en.iter().any(|e| e == x)));
        // three culprits is where the session stops asking and says so (found.len() >= 3 guard)
        assert!(matches!(b.phase, Phase::Done | Phase::GaveUp), "{:?}", b);
        if b.phase == Phase::Done {
            assert_eq!(b.found.len(), 3);
        }
    }

    #[test]
    fn suspect_ordering_makes_a_good_guess_cheap() {
        // the culprit is listed first, so the first half contains it, but rounds stay logarithmic
        let c = "LUA:Mod30".to_string();
        let order = vec![c.clone()];
        let (b, rounds) = run(32, order, |en| en.contains(&c));
        assert_eq!(b.found, vec![c]);
        assert!(rounds <= ceil_log2(32) + 2);
    }

    #[test]
    fn crash_unrelated_to_mods_is_reported_not_blamed() {
        let (b, rounds) = run(8, vec![], |_| true);
        assert_eq!(rounds, 1, "caught by the all-mods-off baseline");
        assert_eq!(b.phase, Phase::GaveUp);
        assert!(b.note.contains("not caused by your mods"), "{}", b.note);
        assert!(b.found.is_empty());
        assert_eq!(b.final_enabled(false), ids(8), "gave up: everything is restored");
    }

    #[test]
    fn single_mod_is_tested_by_removing_it() {
        let (b, rounds) = run(1, vec![], |en| en.contains(&"LUA:Mod00".to_string()));
        assert_eq!((b.phase, rounds), (Phase::Done, 1));
        let (b, _) = run(1, vec![], |_| true);
        assert_eq!(b.phase, Phase::GaveUp);
    }

    #[test]
    fn an_interaction_between_two_mods_ends_in_a_clear_give_up() {
        // crashes only when BOTH are on: plain bisection cannot isolate that, and must not loop
        let (b, rounds) = run(12, vec![], |en| {
            en.contains(&"LUA:Mod02".to_string()) && en.contains(&"LUA:Mod09".to_string())
        });
        assert!(b.is_finished());
        assert!(rounds <= b.max_rounds + 1);
        if b.phase == Phase::GaveUp {
            assert!(!b.note.is_empty());
        }
        // whatever happened, restoring leaves the user's original set intact
        assert_eq!(b.final_enabled(true), ids(12));
    }

    #[test]
    fn rounds_always_enable_a_consistent_subset() {
        let mut b = Bisect::new("1", ids(9), vec![], 0).unwrap();
        assert_eq!((b.phase, b.testing.len()), (Phase::Baseline, 0), "first round: everything off");
        while !b.is_finished() {
            assert!(b.testing.iter().all(|m| b.originals.contains(m)));
            assert!(b.testing.iter().all(|m| !b.found.contains(m)));
            if b.phase == Phase::Narrow {
                assert!(!b.testing.is_empty() && b.testing.len() <= b.pool.len());
            }
            b.answer(b.testing.contains(&"LUA:Mod05".to_string()), 7);
        }
        assert_eq!(b.found, vec!["LUA:Mod05".to_string()]);
    }

    #[test]
    fn serialises_for_resuming_after_a_restart() {
        let mut b = Bisect::new("42", ids(6), ids(6).into_iter().rev().collect(), 5).unwrap();
        b.answer(false, 8); // baseline: works without mods
        b.answer(true, 9);
        let json = serde_json::to_string(&b).unwrap();
        let back: Bisect = serde_json::from_str(&json).unwrap();
        assert_eq!(b, back);
        assert!(json.contains("\"phase\":\"narrow\""), "{json}");
        assert!(Bisect::new("1", vec![], vec![], 0).is_none());
        // answers after the end are ignored
        let mut d = run(3, vec![], |en| en.contains(&"LUA:Mod01".to_string())).0;
        let snapshot = d.clone();
        d.answer(true, 1);
        assert_eq!(d, snapshot);
        assert_eq!(d.rounds_left(), 0);
        assert_eq!(d.progress_pct(), 100);
    }

    #[test]
    fn progress_pct_starts_low_and_ends_at_100() {
        let mut b = Bisect::new("1", ids(10), vec![], 0).unwrap();
        assert_eq!(b.progress_pct(), 0);
        b.answer(false, 1);
        assert!(b.progress_pct() > 0 && b.progress_pct() < 100);
        while !b.is_finished() {
            b.answer(false, 2);
        }
        assert_eq!(b.progress_pct(), 100);
    }
}
