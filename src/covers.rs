//! Finding artwork for games that do not come from Steam: the game's name is looked up in the
//! Steam store, and the matching app's pictures are shown. Only an exact or very close name
//! counts as a match, so a wrong cover is rarer than a missing one.

use serde_json::Value;
use std::process::{Command, Stdio};

pub const STORE_SEARCH: &str = "https://store.steampowered.com/api/storesearch/";

fn norm(s: &str) -> String {
    s.chars().filter(|c| c.is_alphanumeric()).collect::<String>().to_lowercase()
}

/// The name without trademark signs and edition words ("Grand Theft Auto IV: The Complete Edition").
pub fn simplified(name: &str) -> String {
    let mut n = name.replace(['™', '®', '©'], "");
    for sep in [
        ": The Complete Edition", " - Complete Edition", " Complete Edition",
        ": Definitive Edition", " - Definitive Edition", " Definitive Edition",
        ": Director's Cut", " - Director's Cut", " Director's Cut",
        ": Enhanced Edition", " - Enhanced Edition", " Enhanced Edition",
        ": Anniversary Edition", " - Anniversary Edition", " Anniversary Edition",
        ": Limited Edition", " Limited Edition",
        ": Deluxe Edition", " - Deluxe Edition", " Deluxe Edition",
        ": Digital Deluxe Edition", " Digital Deluxe Edition",
        ": Game of the Year Edition", " - Game of the Year Edition", " Game of the Year Edition",
        ": GOTY Edition", " - GOTY Edition", " GOTY Edition",
        ": Remastered", " - Remastered", " Remastered",
    ] {
        // ASCII-only lower-casing keeps every byte offset, so the cut always falls on a character boundary
        if let Some(i) = n.to_ascii_lowercase().find(&sep.to_ascii_lowercase()) {
            n.truncate(i);
        }
    }
    n.trim().trim_end_matches([':', '-']).trim().to_string()
}

/// `(app id, name)` pairs from a store search answer.
pub fn parse_results(v: &Value) -> Vec<(u64, String)> {
    v.get("items")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter(|i| i.get("type").and_then(Value::as_str).map(|t| t == "app").unwrap_or(true)).filter_map(|i| Some((i.get("id")?.as_u64()?, i.get("name")?.as_str()?.to_string()))).collect())
        .unwrap_or_default()
}

/// The result that is the same game as `name`, if there is one.
pub fn best_match(name: &str, results: &[(u64, String)]) -> Option<u64> {
    let want = [norm(name), norm(&simplified(name))];
    for w in want.iter().filter(|w| w.len() >= 3) {
        if let Some((id, _)) = results.iter().find(|(_, n)| norm(n) == *w || norm(&simplified(n)) == *w) {
            return Some(*id);
        }
    }
    None
}

/// Ask the store about `name` (`base` is the search address).
pub fn lookup(name: &str, base: &str, allow_http: bool) -> Result<Option<u64>, String> {
    if !(base.starts_with("https://") || (allow_http && base.starts_with("http://"))) {
        return Err("Refusing a non-HTTPS address.".into());
    }
    let mut last = None;
    for term in [name.to_string(), simplified(name)].iter().filter(|t| t.len() >= 3) {
        let out = Command::new("curl")
            .args(["-sSL", "--max-time", "20", "-A", "ModForge", "--get", "--data-urlencode"])
            .arg(format!("term={term}"))
            .args(["--data-urlencode", "l=english", "--data-urlencode", "cc=US"])
            .arg(base)
            .stdin(Stdio::null())
            .output()
            .map_err(|e| format!("Could not run curl ({e})."))?;
        let Ok(v) = serde_json::from_slice::<Value>(&out.stdout) else { return Err("The store did not answer.".into()) };
        last = best_match(name, &parse_results(&v));
        if last.is_some() {
            break;
        }
    }
    Ok(last)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn r(items: &[(u64, &str)]) -> Vec<(u64, String)> {
        items.iter().map(|(i, n)| (*i, n.to_string())).collect()
    }

    #[test]
    fn names_are_cleaned_of_signs_and_edition_words() {
        assert_eq!(simplified("Need for Speed™ Most Wanted"), "Need for Speed Most Wanted");
        assert_eq!(simplified("Grand Theft Auto IV: The Complete Edition"), "Grand Theft Auto IV");
        assert_eq!(simplified("Mafia III: Definitive Edition"), "Mafia III");
        assert_eq!(simplified("Death Stranding: Director's Cut"), "Death Stranding");
        assert_eq!(simplified("Baldur's Gate: Enhanced Edition"), "Baldur's Gate");
        assert_eq!(simplified("The Elder Scrolls V: Skyrim - Anniversary Edition"), "The Elder Scrolls V: Skyrim");
        // letters whose lower case is longer or shorter in bytes must not shift the cut into the middle of a character
        assert_eq!(simplified("İstanbul Savaşı Definitive Edition"), "İstanbul Savaşı");
        assert_eq!(simplified("ẞ Game: The Complete Edition"), "ẞ Game");
    }

    #[test]
    fn only_a_matching_name_counts() {
        let res = r(&[(1, "Need for Speed Most Wanted Mod Pack"), (17410, "Need for Speed™ Most Wanted"), (9, "Mafia")]);
        assert_eq!(best_match("Need for Speed™ Most Wanted", &res), Some(17410));
        assert_eq!(best_match("Mafia III: Definitive Edition", &r(&[(360430, "Mafia III: Definitive Edition")])), Some(360430));
        assert_eq!(best_match("Mafia III: Definitive Edition", &r(&[(50130, "Mafia II (Classic)"), (9, "Mafia")])), None, "a different game is not a match");
        assert_eq!(best_match("Aviassembly", &r(&[(1, "Aviassembly Playtest")])), None);
        assert_eq!(best_match("GTA", &r(&[(1, "GTA")])), Some(1));
        assert_eq!(best_match("", &res), None);
    }

    #[test]
    fn store_answers_are_read() {
        let v = serde_json::json!({"total":2,"items":[{"type":"app","id":17410,"name":"Need for Speed™ Most Wanted"},{"type":"sub","id":5,"name":"Bundle"},{"id":7,"name":"No type"}]});
        let res = parse_results(&v);
        assert!(res.contains(&(17410, "Need for Speed™ Most Wanted".to_string())));
        assert!(!res.iter().any(|r| r.1 == "Bundle"), "bundles are not games");
        assert!(parse_results(&serde_json::json!({})).is_empty());
    }
}
