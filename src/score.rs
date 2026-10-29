//! One number for "how healthy is this game": 100 when nothing is wrong, lower for every problem.
//! The inputs are the issues the game page already lists, so the number never disagrees with the list.

use serde_json::{json, Value};

/// Points taken off for one issue.
fn cost(level: &str, code: &str) -> u32 {
    match (level, code) {
        ("bad", "anti_cheat") => 40,
        ("bad", _) => 30,
        ("warn", "needs") | ("warn", "needs_loader") => 20,
        ("warn", _) => 12,
        ("info", "pack_note") => 0,
        ("info", _) => 3,
        _ => 0,
    }
}

/// 0 to 100 from the issue list and the number of mod updates waiting.
pub fn score(issues: &[Value], updates: usize) -> u32 {
    let mut s: i32 = 100;
    for (n, i) in issues.iter().enumerate() {
        let c = cost(i["level"].as_str().unwrap_or(""), i["code"].as_str().unwrap_or(""));
        // the first problem hurts fully, the rest a little less (one root cause often shows up as several lines)
        s -= if n == 0 { c as i32 } else { (c as i32 * 3 + 2) / 4 };
    }
    s -= (updates.min(4) * 3) as i32;
    s.clamp(0, 100) as u32
}

/// A short word for the number: `great`, `good`, `attention` or `broken`.
pub fn grade(score: u32) -> &'static str {
    match score {
        90.. => "great",
        70..=89 => "good",
        40..=69 => "attention",
        _ => "broken",
    }
}

/// The part of the health answer the game page needs: `{ score, grade, breakdown }`.
pub fn summary(issues: &[Value], updates: usize) -> Value {
    let s = score(issues, updates);
    let bad_count = issues.iter().filter(|i| i["level"].as_str() == Some("bad")).count();
    let warn_count = issues.iter().filter(|i| i["level"].as_str() == Some("warn")).count();
    let info_count = issues.iter().filter(|i| i["level"].as_str() == Some("info")).count();
    json!({
        "score": s,
        "grade": grade(s),
        "breakdown": {
            "issues_count": issues.len(),
            "bad_count": bad_count,
            "warn_count": warn_count,
            "info_count": info_count,
            "updates_pending": updates,
            "clean": s >= 90 && bad_count == 0
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn i(level: &str, code: &str) -> Value {
        json!({ "level": level, "code": code })
    }

    #[test]
    fn a_game_without_problems_is_perfect_and_problems_only_lower_it() {
        assert_eq!(score(&[], 0), 100);
        let one = score(&[i("warn", "drift")], 0);
        let two = score(&[i("warn", "drift"), i("warn", "dirty")], 0);
        assert!(one < 100 && two <= one);
    }

    #[test]
    fn the_worst_issue_counts_fully_and_the_number_never_leaves_the_range() {
        assert_eq!(score(&[i("bad", "anti_cheat")], 0), 60);
        let many: Vec<Value> = (0..40).map(|_| i("bad", "errors")).collect();
        assert_eq!(score(&many, 0), 0);
        assert_eq!(score(&[], 99), 88, "waiting updates cost at most 12 points");
    }

    #[test]
    fn a_note_from_a_support_entry_is_not_a_problem() {
        assert_eq!(score(&[i("info", "pack_note")], 0), 100);
    }

    #[test]
    fn grades_follow_the_number() {
        assert_eq!(grade(100), "great");
        assert_eq!(grade(75), "good");
        assert_eq!(grade(50), "attention");
        assert_eq!(grade(10), "broken");
    }

    #[test]
    fn summary_includes_detailed_breakdown() {
        let issues = vec![i("bad", "crash"), i("warn", "conflict"), i("info", "note")];
        let sum = summary(&issues, 2);
        assert_eq!(sum["grade"], "attention");
        let b = &sum["breakdown"];
        assert_eq!(b["issues_count"], 3);
        assert_eq!(b["bad_count"], 1);
        assert_eq!(b["warn_count"], 1);
        assert_eq!(b["info_count"], 1);
        assert_eq!(b["updates_pending"], 2);
        assert_eq!(b["clean"], false);

        let clean_sum = summary(&[], 0);
        assert_eq!(clean_sum["breakdown"]["clean"], true);
    }
}
