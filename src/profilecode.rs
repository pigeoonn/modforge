//! Thunderstore profile codes, the way r2modman and Gale share a profile: a short code stands for a file kept by
//! Thunderstore. The file is `#r2modman` followed by base64 of a zip whose `export.r2x` lists the mods.
//!
//! Only the list of mods is used (name, version, on/off, order); the config files a profile may carry are ignored.

use crate::deploy::{ProfileSpec, SpecMod};
use crate::thunderstore::ThunderRef;

pub const PREFIX: &str = "#r2modman";

const B64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

pub fn base64_encode(data: &[u8]) -> String {
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for c in data.chunks(3) {
        let n = (u32::from(c[0]) << 16) | (u32::from(*c.get(1).unwrap_or(&0)) << 8) | u32::from(*c.get(2).unwrap_or(&0));
        out.push(B64[(n >> 18) as usize & 63] as char);
        out.push(B64[(n >> 12) as usize & 63] as char);
        out.push(if c.len() > 1 { B64[(n >> 6) as usize & 63] as char } else { '=' });
        out.push(if c.len() > 2 { B64[n as usize & 63] as char } else { '=' });
    }
    out
}

/// Base64 (whitespace ignored, padding optional); `None` for anything that is not base64.
pub fn base64_decode(text: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(text.len() / 4 * 3);
    let (mut acc, mut bits) = (0u32, 0);
    for ch in text.bytes().filter(|b| !b.is_ascii_whitespace() && *b != b'=') {
        let v = match ch {
            b'A'..=b'Z' => ch - b'A',
            b'a'..=b'z' => ch - b'a' + 26,
            b'0'..=b'9' => ch - b'0' + 52,
            b'+' | b'-' => 62,
            b'/' | b'_' => 63,
            _ => return None,
        };
        acc = (acc << 6) | u32::from(v);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
            acc &= (1 << bits) - 1;
        }
    }
    Some(out)
}

/// The zip inside a downloaded profile file (`None` when it is not one).
pub fn unwrap_profile(text: &str) -> Option<Vec<u8>> {
    let body = text.trim().strip_prefix(PREFIX)?;
    base64_decode(body).filter(|b| b.starts_with(b"PK"))
}

/// A profile file made of a zip.
pub fn wrap_profile(zip: &[u8]) -> String {
    format!("{PREFIX}\n{}", base64_encode(zip))
}

/// One mod of an `export.r2x`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct R2Mod {
    pub full_name: String,
    pub version: String,
    pub enabled: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct R2x {
    pub name: String,
    pub mods: Vec<R2Mod>,
}

fn unquote(v: &str) -> String {
    let v = v.trim();
    let v = v.strip_prefix('"').and_then(|x| x.strip_suffix('"')).or_else(|| v.strip_prefix('\'').and_then(|x| x.strip_suffix('\''))).unwrap_or(v);
    v.to_string()
}

/// Read the part of `export.r2x` that matters. The file is YAML written by r2modman:
/// `profileName`, then `mods:` with `name`, `version: {major, minor, patch}` and `enabled` per mod.
pub fn parse_r2x(text: &str) -> Result<R2x, String> {
    let mut out = R2x::default();
    let mut in_mods = false;
    let mut cur: Option<(String, [String; 3], bool)> = None;
    let flush = |cur: &mut Option<(String, [String; 3], bool)>, out: &mut R2x| {
        if let Some((name, v, enabled)) = cur.take() {
            if !name.is_empty() {
                out.mods.push(R2Mod { full_name: name, version: format!("{}.{}.{}", v[0], v[1], v[2]), enabled });
            }
        }
    };
    for raw in text.lines().take(20_000) {
        let line = raw.trim_end();
        let t = line.trim_start();
        if t.is_empty() || t.starts_with('#') {
            continue;
        }
        if !in_mods {
            if let Some(v) = t.strip_prefix("profileName:") {
                out.name = unquote(v);
            } else if t.starts_with("mods:") {
                in_mods = true;
            }
            continue;
        }
        let (item, body) = match t.strip_prefix("- ") {
            Some(b) => (true, b.trim_start()),
            None => (false, t),
        };
        if item {
            flush(&mut cur, &mut out);
            cur = Some((String::new(), ["0".into(), "0".into(), "0".into()], true));
        }
        let Some(c) = cur.as_mut() else { continue };
        if let Some((k, v)) = body.split_once(':') {
            let v = unquote(v);
            match k.trim() {
                "name" => c.0 = v,
                "major" => c.1[0] = v,
                "minor" => c.1[1] = v,
                "patch" => c.1[2] = v,
                "enabled" => c.2 = !matches!(v.as_str(), "false" | "no" | "off"),
                _ => {}
            }
        }
    }
    flush(&mut cur, &mut out);
    if out.name.is_empty() && out.mods.is_empty() {
        return Err("This profile has no mods list.".into());
    }
    Ok(out)
}

/// `Owner-Name` into its two parts (the owner never contains a dash; the name may).
fn split_full(full: &str) -> Option<(String, String)> {
    let (ns, name) = full.split_once('-')?;
    let ok = |s: &str| !s.is_empty() && s.len() <= 100 && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '.' || c == '-');
    (ok(ns) && ok(name) && !ns.contains('-')).then(|| (ns.to_string(), name.to_string()))
}

/// A modpack-style setup out of a profile: every mod a Thunderstore package at the version it was exported with.
pub fn to_spec(r: &R2x, game: &str) -> ProfileSpec {
    let mods = r
        .mods
        .iter()
        .filter_map(|m| {
            let (ns, name) = split_full(&m.full_name)?;
            Some(SpecMod {
                name: name.replace('_', " "),
                version: m.version.clone(),
                enabled: m.enabled,
                target: String::new(),
                source: "profile code".into(),
                nexus: None,
                thunderstore: Some(ThunderRef { namespace: ns, name, version: m.version.clone() }),
                ..Default::default()
            })
        })
        .collect();
    ProfileSpec { gantry_profile: 1, game: game.to_string(), mods, name: r.name.clone(), note: String::new() }
}

/// The other way: an `export.r2x` for the mods that have a Thunderstore origin. Returns the text and how many were left out.
pub fn make_r2x(name: &str, mods: &[(String, String, bool)], left_out: usize) -> (String, usize) {
    let mut s = format!("profileName: {}\nmods:\n", name.replace(['\n', '\r', ':'], " "));
    for (full, version, enabled) in mods {
        let mut p = version.split('.').map(|x| x.parse::<u32>().unwrap_or(0));
        let (a, b, c) = (p.next().unwrap_or(0), p.next().unwrap_or(0), p.next().unwrap_or(0));
        s.push_str(&format!("  - name: {full}\n    version:\n      major: {a}\n      minor: {b}\n      patch: {c}\n    enabled: {enabled}\n"));
    }
    (s, left_out)
}

/// A profile code as typed or pasted: the UUID itself, or a link that ends with it.
pub fn clean_code(input: &str) -> Option<String> {
    let t = input.trim().trim_end_matches('/');
    let last = t.rsplit('/').next().unwrap_or(t).trim();
    let ok = last.len() >= 8 && last.len() <= 64 && last.chars().all(|c| c.is_ascii_hexdigit() || c == '-');
    ok.then(|| last.to_lowercase())
}

/// Gantry's own share code: a setup as one line of text, made and read without going online.
pub const SHARE_PREFIX: &str = "MODFORGE1:";
pub const LEGACY_SHARE_PREFIX: &str = "GANTRY1:";
/// A code longer than this is not something anyone pasted on purpose.
const SHARE_MAX: usize = 200_000;
const SHARE_MODS_MAX: usize = 1000;

fn short(s: &str, n: usize) -> String {
    s.chars().filter(|c| !c.is_control()).take(n).collect()
}

/// Where a mod comes from, written short: `nx/<game>/<mod>/<file>`, `ts/<owner>/<name>/<version>`,
/// `gh/<owner>/<repo>/<tag>/<file>` or `mio/<game>/<mod>/<file>`.
fn source_of(m: &SpecMod) -> Option<String> {
    if let Some(t) = &m.thunderstore {
        return Some(format!("ts/{}/{}/{}", t.namespace, t.name, t.version));
    }
    if let Some(n) = &m.nexus {
        return Some(format!("nx/{}/{}/{}", n.game, n.mod_id, n.file_id));
    }
    if let Some(g) = &m.github {
        return Some(format!("gh/{}/{}/{}/{}", g.owner, g.repo, g.tag, g.asset));
    }
    m.modio.as_ref().map(|r| format!("mio/{}/{}/{}", r.game_id, r.mod_id, r.file_id))
}

fn plain(s: &str) -> bool {
    !s.is_empty() && s.len() <= 120 && !s.starts_with('.') && s.chars().all(|c| c.is_ascii_alphanumeric() || "-_.+".contains(c))
}

fn read_source(src: &str, m: &mut SpecMod) -> bool {
    let p: Vec<&str> = src.split('/').collect();
    let num = |s: &str| s.parse::<u64>().ok();
    match p.as_slice() {
        ["ts", ns, name, ver] if plain(ns) && plain(name) && plain(ver) => {
            m.thunderstore = Some(ThunderRef { namespace: ns.to_string(), name: name.to_string(), version: ver.to_string() });
        }
        ["nx", game, id, file] if plain(game) && num(id).is_some() => {
            m.nexus = Some(crate::nexus::NexusRef { game: game.to_string(), mod_id: num(id).unwrap_or(0), file_id: num(file).unwrap_or(0) });
        }
        ["gh", owner, repo, tag @ .., asset] if plain(owner) && plain(repo) && plain(asset) && !tag.is_empty() && tag.iter().all(|t| plain(t)) => {
            m.github = Some(crate::github::GithubRef { owner: owner.to_string(), repo: repo.to_string(), tag: tag.join("/"), asset: asset.to_string() });
        }
        ["mio", game, id, file] if num(game).is_some() && num(id).is_some() => {
            m.modio = Some(crate::modio::ModioRef { game_id: num(game).unwrap_or(0), mod_id: num(id).unwrap_or(0), file_id: num(file).unwrap_or(0) });
        }
        _ => return false,
    }
    true
}

/// A setup as a share code. Mods that were never online (made from files on this computer) cannot be in it;
/// how many were left out is returned too.
pub fn share_code(spec: &ProfileSpec) -> (String, usize) {
    let mut left_out = 0;
    let mods: Vec<serde_json::Value> = spec
        .mods
        .iter()
        .filter_map(|m| {
            let Some(src) = source_of(m) else {
                left_out += 1;
                return None;
            };
            let mut v = serde_json::json!({ "n": m.name, "s": src });
            if !m.version.is_empty() {
                v["v"] = m.version.clone().into();
            }
            if !m.enabled {
                v["off"] = true.into();
            }
            if !m.target.is_empty() {
                v["t"] = m.target.clone().into();
            }
            Some(v)
        })
        .collect();
    let mut body = serde_json::json!({ "g": spec.game, "m": mods });
    if !spec.name.is_empty() {
        body["n"] = spec.name.clone().into();
    }
    (format!("{SHARE_PREFIX}{}", base64_encode(body.to_string().as_bytes())), left_out)
}

/// Is this text a Gantry share code (and not a Thunderstore one)?
pub fn is_share_code(text: &str) -> bool {
    text.trim().starts_with(SHARE_PREFIX) || text.trim().starts_with(LEGACY_SHARE_PREFIX)
}

/// A share code back into a setup. Anything that is not a mod with a source Gantry knows is dropped.
pub fn from_share_code(text: &str, game: &str) -> Result<ProfileSpec, String> {
    let bad = || "That share code is damaged or incomplete.".to_string();
    let t = text.trim();
    if t.len() > SHARE_MAX {
        return Err(bad());
    }
    let raw = base64_decode((t.strip_prefix(SHARE_PREFIX).or_else(|| t.strip_prefix(LEGACY_SHARE_PREFIX))).ok_or_else(bad)?).ok_or_else(bad)?;
    let v: serde_json::Value = serde_json::from_slice(&raw).map_err(|_| bad())?;
    let mods: Vec<SpecMod> = v["m"]
        .as_array()
        .ok_or_else(bad)?
        .iter()
        .take(SHARE_MODS_MAX)
        .filter_map(|x| {
            let mut m = SpecMod {
                name: short(x["n"].as_str()?, 200),
                version: short(x["v"].as_str().unwrap_or(""), 60),
                enabled: !x["off"].as_bool().unwrap_or(false),
                target: short(x["t"].as_str().unwrap_or(""), 200),
                source: "share code".into(),
                ..Default::default()
            };
            // a folder inside the game folder, never one outside it
            if m.target.split(['/', '\\']).any(|p| p == ".." || p.contains(':')) || m.target.starts_with(['/', '\\']) {
                m.target.clear();
            }
            (!m.name.trim().is_empty() && read_source(x["s"].as_str()?, &mut m)).then_some(m)
        })
        .collect();
    if mods.is_empty() {
        return Err("This share code has no mods ModForge can fetch.".into());
    }
    Ok(ProfileSpec { gantry_profile: 1, game: game.to_string(), mods, name: short(v["n"].as_str().unwrap_or(""), 80), note: String::new() })
}

/// The game a share code was made for, as its maker's Gantry named it.
pub fn share_code_game(text: &str) -> Option<String> {
    let raw = base64_decode((text.trim().strip_prefix(SHARE_PREFIX).or_else(|| text.trim().strip_prefix(LEGACY_SHARE_PREFIX)))?)?;
    serde_json::from_slice::<serde_json::Value>(&raw).ok()?["g"].as_str().map(|g| short(g, 200))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_share_code_carries_every_online_source_and_nothing_that_could_escape() {
        let m = |name: &str| SpecMod { name: name.into(), version: "1.0".into(), enabled: true, ..Default::default() };
        let spec = ProfileSpec {
            gantry_profile: 1,
            game: "Cyberpunk 2077".into(),
            name: "Night run".into(),
            note: String::new(),
            mods: vec![
                SpecMod { github: Some(crate::github::GithubRef { owner: "wopss".into(), repo: "RED4ext".into(), tag: "v1.30.0".into(), asset: "red4ext-1.30.0.zip".into() }), ..m("RED4ext") },
                SpecMod { nexus: Some(crate::nexus::NexusRef { game: "cyberpunk2077".into(), mod_id: 77, file_id: 9 }), enabled: false, target: "archive/pc/mod".into(), ..m("Cool Cars") },
                SpecMod { thunderstore: Some(ThunderRef { namespace: "Own".into(), name: "Cool_Mod".into(), version: "1.2.3".into() }), ..m("Cool Mod") },
                SpecMod { modio: Some(crate::modio::ModioRef { game_id: 5, mod_id: 6, file_id: 7 }), ..m("Map") },
                m("My own edit"),
            ],
        };
        let (code, left_out) = share_code(&spec);
        assert!(is_share_code(&code) && !code.contains(' ') && left_out == 1, "{code}");
        assert_eq!(share_code_game(&code).as_deref(), Some("Cyberpunk 2077"));
        let back = from_share_code(&format!("  {code}\n"), "Cyberpunk 2077").unwrap();
        let mut want = spec.clone();
        want.mods.pop();
        for x in &mut want.mods {
            x.source = "share code".into();
        }
        assert_eq!(back, want);

        // damaged, foreign and hostile codes
        for bad in ["GANTRY1:", "GANTRY1:@@@", "GANTRY1:bm90IGpzb24=", "#r2modman", "hello"] {
            assert!(from_share_code(bad, "G").is_err(), "{bad}");
        }
        let hostile = format!("{SHARE_PREFIX}{}", base64_encode(serde_json::json!({ "g": "G", "m": [
            { "n": "Escape", "s": "gh/../x/v1/a.zip" },
            { "n": "Slash", "s": "nx/a b/1/2" },
            { "n": "Folder", "s": "nx/game/1/2", "t": "../../.ssh" },
            { "n": "", "s": "nx/game/2/2" },
            { "n": "Fine", "s": "ts/Own/Mod/1.0.0" },
        ] }).to_string().as_bytes()));
        let got = from_share_code(&hostile, "G").unwrap();
        assert_eq!(got.mods.iter().map(|m| (m.name.as_str(), m.target.as_str())).collect::<Vec<_>>(), vec![("Folder", ""), ("Fine", "")]);
    }

    #[test]
    fn base64_round_trips_and_refuses_what_is_not_base64() {
        for data in [&b""[..], b"a", b"ab", b"abc", b"PK\x03\x04hello world\x00\xff"] {
            assert_eq!(base64_decode(&base64_encode(data)).unwrap(), data);
        }
        assert_eq!(base64_encode(b"Man"), "TWFu");
        assert_eq!(base64_decode("TWE=\n").unwrap(), b"Ma");
        assert!(base64_decode("a*b").is_none());
    }

    #[test]
    fn the_export_file_is_read_and_written() {
        let text = "profileName: My run\nmods:\n  - name: bbepis-BepInExPack\n    version:\n      major: 5\n      minor: 4\n      patch: 2100\n    enabled: true\n  - name: Own-Cool_Mod\n    version:\n      major: 1\n      minor: 2\n      patch: 3\n    enabled: false\n";
        let r = parse_r2x(text).unwrap();
        assert_eq!(r.name, "My run");
        assert_eq!(r.mods, vec![R2Mod { full_name: "bbepis-BepInExPack".into(), version: "5.4.2100".into(), enabled: true }, R2Mod { full_name: "Own-Cool_Mod".into(), version: "1.2.3".into(), enabled: false }]);
        let spec = to_spec(&r, "ror2");
        assert_eq!(spec.mods[1].thunderstore.as_ref().unwrap().full_name(), "Own-Cool_Mod");
        assert_eq!((spec.mods[1].name.as_str(), spec.mods[1].enabled), ("Cool Mod", false));
        // what we write is read back
        let (out, _) = make_r2x("My run", &[("Own-Cool_Mod".into(), "1.2.3".into(), false)], 0);
        assert_eq!(parse_r2x(&out).unwrap().mods[0].version, "1.2.3");
        assert!(parse_r2x("nonsense").is_err());
        // quotes and the dash-in-name case
        let q = parse_r2x("profileName: \"Q\"\nmods:\n- name: 'Some-Mod-With-Dashes'\n  version: {}\n").unwrap();
        assert_eq!(split_full(&q.mods[0].full_name), Some(("Some".into(), "Mod-With-Dashes".into())));
    }

    #[test]
    fn a_profile_file_is_a_prefix_and_a_zip_and_a_code_is_cleaned() {
        let file = wrap_profile(b"PK\x03\x04zipbytes");
        assert!(file.starts_with("#r2modman\n"));
        assert_eq!(unwrap_profile(&file).unwrap(), b"PK\x03\x04zipbytes");
        assert!(unwrap_profile("<html>").is_none() && unwrap_profile("#r2modman\nTWFu").is_none(), "a zip is required");
        assert_eq!(clean_code(" 0F3E1C2A-1111-4222-8333-444455556666 ").unwrap(), "0f3e1c2a-1111-4222-8333-444455556666");
        assert_eq!(clean_code("https://thunderstore.io/c/riskofrain2/p/x/0f3e1c2a-1111-4222-8333-444455556666/").unwrap(), "0f3e1c2a-1111-4222-8333-444455556666");
        assert!(clean_code("hello world").is_none() && clean_code("../../etc").is_none());
    }
}
