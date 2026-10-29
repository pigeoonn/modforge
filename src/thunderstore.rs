//! Thunderstore: the mod site of Risk of Rain 2, Valheim, Lethal Company and many other Unity
//! games. Packages are found by link or by `Owner-Name`, installed together with the packages
//! they depend on, and checked for newer versions. The public API needs no key.

use crate::nexus::curl_ok;
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashSet;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::LazyLock;

pub const DEFAULT_BASE: &str = "https://thunderstore.io";

/// One package as the mod browser lists it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Listing {
    /// `Owner-Name`, what installing takes.
    pub full_name: String,
    pub owner: String,
    pub name: String,
    pub description: String,
    pub version: String,
    pub downloads: u64,
    pub rating: i64,
    pub categories: Vec<String>,
    pub updated: String,
    pub icon: String,
    /// Marked adult by its author (kept in the list; the store hides these unless asked).
    #[serde(default)]
    pub nsfw: bool,
    /// The packages the newest version needs, as `Owner-Name` (no version).
    #[serde(default)]
    pub deps: Vec<String>,
}

/// Packages of a community out of Thunderstore's list: deprecated and adult ones are left out.
pub fn parse_listing(v: &Value) -> Vec<Listing> {
    let s = |x: &Value, k: &str| x.get(k).and_then(Value::as_str).unwrap_or("").to_string();
    v.as_array()
        .map(|a| {
            a.iter()
                .filter(|p| !p.get("is_deprecated").and_then(Value::as_bool).unwrap_or(false))
                .filter_map(|p| {
                    let latest = p.get("versions")?.as_array()?.first()?;
                    let owner = s(p, "owner");
                    let name = s(p, "name");
                    if owner.is_empty() || name.is_empty() {
                        return None;
                    }
                    Some(Listing {
                        full_name: format!("{owner}-{name}"),
                        owner,
                        name,
                        description: s(latest, "description"),
                        version: s(latest, "version_number"),
                        downloads: p.get("versions").and_then(Value::as_array).map(|vs| vs.iter().filter_map(|x| x.get("downloads").and_then(Value::as_u64)).sum()).unwrap_or(0),
                        rating: p.get("rating_score").and_then(Value::as_i64).unwrap_or(0),
                        categories: p.get("categories").and_then(Value::as_array).map(|c| c.iter().filter_map(|x| x.as_str().map(str::to_string)).collect()).unwrap_or_default(),
                        updated: s(p, "date_updated"),
                        icon: s(latest, "icon"),
                        nsfw: p.get("has_nsfw_content").and_then(Value::as_bool).unwrap_or(false),
                        deps: latest.get("dependencies").and_then(Value::as_array).map(|d| d.iter().filter_map(Value::as_str).map(|x| x.rsplit_once('-').map(|(n, _)| n).unwrap_or(x).to_string()).collect()).unwrap_or_default(),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Best matches for a search, most useful first: every word must appear in the name, owner or
/// description; name matches rank above description matches, then popularity.
pub fn search<'a>(all: &'a [Listing], query: &str, limit: usize) -> Vec<&'a Listing> {
    let words: Vec<String> = query.to_lowercase().split_whitespace().map(str::to_string).collect();
    let mut hits: Vec<(i64, &Listing)> = all
        .iter()
        .filter(|l| !l.nsfw)
        .filter_map(|l| {
            let name = l.name.replace('_', " ").to_lowercase();
            let owner = l.owner.to_lowercase();
            let desc = l.description.to_lowercase();
            let mut score = 0i64;
            for w in &words {
                score += if name == *w {
                    1000
                } else if name.starts_with(w.as_str()) {
                    300
                } else if name.contains(w.as_str()) {
                    200
                } else if owner.contains(w.as_str()) {
                    60
                } else if desc.contains(w.as_str()) {
                    20
                } else {
                    return None;
                };
            }
            Some((score * 1_000_000 + (l.downloads.min(900_000) as i64) + l.rating.clamp(0, 99_999) * 10, l))
        })
        .collect();
    hits.sort_by_key(|h| std::cmp::Reverse(h.0));
    hits.into_iter().take(limit).map(|h| h.1).collect()
}

fn norm(s: &str) -> String {
    s.chars().filter(|c| c.is_alphanumeric()).collect::<String>().to_lowercase()
}

/// The Thunderstore community of a game, found by comparing names ("Risk of Rain 2" → `riskofrain2`).
pub fn match_community(game_name: &str, communities: &[(String, String)]) -> Option<String> {
    let g = norm(&game_name.replace(['™', '®'], ""));
    if g.is_empty() {
        return None;
    }
    communities.iter().find(|(id, name)| norm(id) == g || norm(name) == g).map(|(id, _)| id.clone())
}

/// Where a library mod came from on Thunderstore.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ThunderRef {
    pub namespace: String,
    pub name: String,
    pub version: String,
}

impl ThunderRef {
    pub fn full_name(&self) -> String {
        format!("{}-{}", self.namespace, self.name)
    }
}

/// A package to look up: the version is optional ("latest").
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PkgRef {
    pub namespace: String,
    pub name: String,
    pub version: Option<String>,
}

static IDENT: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[A-Za-z0-9_]{1,128}$").unwrap());
static VERSION: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^\d{1,6}\.\d{1,6}\.\d{1,6}$").unwrap());

fn pkg(ns: &str, name: &str, version: Option<&str>) -> Result<PkgRef, String> {
    if !IDENT.is_match(ns) || !IDENT.is_match(name) || version.is_some_and(|v| !VERSION.is_match(v)) {
        return Err("That does not look like a Thunderstore package.".into());
    }
    Ok(PkgRef { namespace: ns.into(), name: name.into(), version: version.map(str::to_string) })
}

/// Accepts a package page (`.../c/<community>/p/<owner>/<name>/[<version>/]`), `.../package/<owner>/<name>/`,
/// the mod manager link `ror2mm://v1/install/thunderstore.io/<owner>/<name>/<version>/`, or `Owner-Name[-1.2.3]`.
pub fn parse_ref(input: &str) -> Result<PkgRef, String> {
    let t = input.trim().trim_end_matches('/');
    if t.is_empty() || t.len() > 400 || t.chars().any(char::is_whitespace) {
        return Err("That does not look like a Thunderstore package.".into());
    }
    let lower = t.to_ascii_lowercase();
    let path = if let Some(p) = lower.strip_prefix("ror2mm://v1/install/thunderstore.io/") {
        Some(&t[t.len() - p.len()..])
    } else {
        ["https://thunderstore.io/", "http://thunderstore.io/", "thunderstore.io/", "https://www.thunderstore.io/"]
            .iter()
            .find_map(|pre| lower.strip_prefix(pre).map(|_| &t[pre.len()..]))
    };
    if let Some(path) = path {
        let path = path.split(['?', '#']).next().unwrap_or("");
        let seg: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
        return match seg.as_slice() {
            ["c", _, "p", ns, name] | ["package", ns, name] | [ns, name] if !["c", "package", "api"].contains(ns) => pkg(ns, name, None),
            ["c", _, "p", ns, name, v] | ["package", ns, name, v] | [ns, name, v] if !["c", "package", "api"].contains(ns) => pkg(ns, name, Some(v)),
            _ => Err("That does not look like a Thunderstore package.".into()),
        };
    }
    // dependency string: Owner-Name or Owner-Name-1.2.3
    let parts: Vec<&str> = t.split('-').collect();
    match parts.as_slice() {
        [ns, name] => pkg(ns, name, None),
        [ns, name, v] => pkg(ns, name, Some(v)),
        _ => Err("That does not look like a Thunderstore package.".into()),
    }
}

/// `1.10.0` > `1.9.3`; unparsable parts count as 0.
pub fn version_newer(candidate: &str, installed: &str) -> bool {
    let parts = |v: &str| -> Vec<u64> { v.split('.').map(|p| p.parse().unwrap_or(0)).collect() };
    parts(candidate) > parts(installed)
}

/// One concrete version of a package.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Resolved {
    pub pkg: ThunderRef,
    pub download_url: String,
    /// `Owner-Name-1.2.3` strings.
    pub dependencies: Vec<String>,
}

pub struct Client {
    base: String,
    allow_http: bool,
}

impl Client {
    /// `allow_http` exists only so tests can talk to a local fake server.
    pub fn new(base: &str, allow_http: bool) -> Self {
        Self { base: base.trim_end_matches('/').to_string(), allow_http }
    }

    fn check(&self, url: &str) -> Result<(), String> {
        if url.starts_with("https://") || (self.allow_http && url.starts_with("http://")) {
            Ok(())
        } else {
            Err("Refusing a non-HTTPS download address.".into())
        }
    }

    fn get_json(&self, path: &str) -> Result<Value, String> {
        let url = format!("{}{}", self.base, path);
        self.check(&url)?;
        let (out, err) = curl_ok(
            Command::new("curl").args(["-sS", "-L", "--max-time", "30", "-A", "ModForge", "-w", "\n%{http_code}", &url]).stdin(Stdio::null()),
            None,
        )?;
        let (body, code) = out.rsplit_once('\n').unwrap_or(("", "000"));
        match code.trim().parse::<u16>().unwrap_or(0) {
            200..=299 => serde_json::from_str(body).map_err(|_| "Thunderstore sent an unreadable reply.".to_string()),
            0 => Err(format!("Could not reach Thunderstore ({}).", err.trim().lines().last().unwrap_or("network error"))),
            404 => Err("Thunderstore does not know that package.".into()),
            429 => Err("Thunderstore rate limit reached; try again in a while.".into()),
            c => Err(format!("Thunderstore error {c}.")),
        }
    }

    /// The file behind a profile code (`#r2modman` and base64 of a zip), as text.
    pub fn profile_file(&self, code: &str) -> Result<String, String> {
        if !code.chars().all(|c| c.is_ascii_hexdigit() || c == '-') || code.len() < 8 || code.len() > 64 {
            return Err("That does not look like a profile code.".into());
        }
        let url = format!("{}/api/experimental/legacyprofile/get/{}/", self.base, code);
        self.check(&url)?;
        let (out, err) = curl_ok(
            Command::new("curl").args(["-sS", "-L", "--max-time", "60", "-A", "ModForge", "-w", "\n%{http_code}", &url]).stdin(Stdio::null()),
            None,
        )?;
        let (body, status) = out.rsplit_once('\n').unwrap_or(("", "000"));
        match status.trim().parse::<u16>().unwrap_or(0) {
            200..=299 if body.len() <= 40_000_000 => Ok(body.to_string()),
            200..=299 => Err("That profile is too large.".into()),
            0 => Err(format!("Could not reach Thunderstore ({}).", err.trim().lines().last().unwrap_or("network error"))),
            404 => Err("Thunderstore does not know that profile code (it may have expired, or have a typo).".into()),
            429 => Err("Thunderstore rate limit reached; try again in a while.".into()),
            c => Err(format!("Thunderstore error {c}.")),
        }
    }

    /// A package's own page data (owner, rating, downloads, latest version with its dependencies).
    pub fn package(&self, namespace: &str, name: &str) -> Result<Value, String> {
        let ok = |s: &str| !s.is_empty() && s.len() <= 100 && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '.' || c == '-');
        if !ok(namespace) || !ok(name) {
            return Err("Unrecognised Thunderstore package.".into());
        }
        self.get_json(&format!("/api/experimental/package/{namespace}/{name}/"))
    }

    /// The README or the changelog of a package version as Markdown ("" when it has none).
    pub fn document(&self, namespace: &str, name: &str, version: &str, which: &str) -> String {
        let ok = |s: &str| !s.is_empty() && s.len() <= 100 && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '.' || c == '-');
        if !ok(namespace) || !ok(name) || !ok(version) || !["readme", "changelog"].contains(&which) {
            return String::new();
        }
        self.get_json(&format!("/api/experimental/package/{namespace}/{name}/{version}/{which}/"))
            .ok()
            .and_then(|v| v.get("markdown").and_then(Value::as_str).map(str::to_string))
            .unwrap_or_default()
    }

    /// Every (non-deprecated) package of a community.
    pub fn community_packages(&self, community: &str) -> Result<Vec<Listing>, String> {
        if community.is_empty() || !community.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') {
            return Err("Unrecognised Thunderstore community.".into());
        }
        let url = format!("{}/c/{}/api/v1/package/", self.base, community);
        self.check(&url)?;
        let (out, err) = curl_ok(
            Command::new("curl").args(["-sS", "-L", "--compressed", "--max-time", "120", "-A", "ModForge", "-w", "\n%{http_code}", &url]).stdin(Stdio::null()),
            None,
        )?;
        let (body, code) = out.rsplit_once('\n').unwrap_or(("", "000"));
        match code.trim().parse::<u16>().unwrap_or(0) {
            200..=299 => serde_json::from_str::<Value>(body).map(|v| parse_listing(&v)).map_err(|_| "Thunderstore sent an unreadable reply.".to_string()),
            0 => Err(format!("Could not reach Thunderstore ({}).", err.trim().lines().last().unwrap_or("network error"))),
            404 => Err("Thunderstore does not know that package.".into()),
            c => Err(format!("Thunderstore error {c}.")),
        }
    }

    /// `(identifier, name)` of every community.
    pub fn communities(&self) -> Result<Vec<(String, String)>, String> {
        let mut out = Vec::new();
        let mut next = Some(format!("{}/api/experimental/community/", self.base));
        for _ in 0..20 {
            let Some(url) = next.take() else { break };
            if !url.starts_with(&self.base) {
                break; // only ever follow links on the same site
            }
            let path = url[self.base.len()..].to_string();
            let v = self.get_json(&path)?;
            for r in v.get("results").and_then(Value::as_array).into_iter().flatten() {
                if let (Some(id), Some(name)) = (r.get("identifier").and_then(Value::as_str), r.get("name").and_then(Value::as_str)) {
                    out.push((id.to_string(), name.to_string()));
                }
            }
            next = v.get("pagination").and_then(|p| p.get("next_link")).and_then(Value::as_str).map(str::to_string);
        }
        Ok(out)
    }

    fn resolved(v: &Value, ns: &str, name: &str) -> Option<Resolved> {
        let s = |k: &str| v.get(k).and_then(Value::as_str).map(str::to_string);
        Some(Resolved {
            pkg: ThunderRef { namespace: ns.into(), name: name.into(), version: s("version_number")? },
            download_url: s("download_url")?,
            dependencies: v.get("dependencies").and_then(Value::as_array).map(|a| a.iter().filter_map(|d| d.as_str().map(str::to_string)).collect()).unwrap_or_default(),
        })
    }

    /// The requested version of a package, or its latest.
    pub fn resolve(&self, p: &PkgRef) -> Result<Resolved, String> {
        let (ns, name) = (&p.namespace, &p.name);
        let doc = match &p.version {
            Some(v) => self.get_json(&format!("/api/experimental/package/{ns}/{name}/{v}/"))?,
            None => self.get_json(&format!("/api/experimental/package/{ns}/{name}/"))?.get("latest").cloned().unwrap_or(Value::Null),
        };
        Self::resolved(&doc, ns, name).ok_or_else(|| "Thunderstore sent an unreadable reply.".to_string())
    }

    /// The package and everything it depends on (once each, newest first requested), dependencies
    /// before the packages that need them. `skip` names packages (`Owner-Name`) that are already there.
    pub fn plan(&self, root: &PkgRef, skip: &HashSet<String>) -> Result<Vec<Resolved>, String> {
        let mut out: Vec<Resolved> = Vec::new();
        let mut seen: HashSet<String> = HashSet::new();
        self.visit(root, skip, &mut seen, &mut out, 0, true)?;
        Ok(out)
    }

    fn visit(&self, p: &PkgRef, skip: &HashSet<String>, seen: &mut HashSet<String>, out: &mut Vec<Resolved>, depth: usize, is_root: bool) -> Result<(), String> {
        let key = format!("{}-{}", p.namespace, p.name);
        if !seen.insert(key.clone()) || (!is_root && skip.contains(&key)) {
            return Ok(());
        }
        if depth > 8 || out.len() >= 30 {
            return Err("This package has too many dependencies to install automatically.".into());
        }
        let r = self.resolve(p)?;
        for d in &r.dependencies {
            // a dependency string pins a version, which is the one the author tested
            self.visit(&parse_ref(d)?, skip, seen, out, depth + 1, false)?;
        }
        out.push(r);
        Ok(())
    }

    /// Download a package archive (a CDN address; nothing identifying is sent).
    pub fn download(&self, url: &str, dest: &Path) -> Result<(), String> {
        self.check(url)?;
        crate::downloads::fetch(url, dest, Some("ModForge"), "Thunderstore")
    }
}

#[cfg(test)]
pub mod fake {
    //! A stand-in for thunderstore.io, for tests.
    use serde_json::json;
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};
    use tiny_http::{Response, Server};

    /// `Owner-Name` -> `(version, dependencies)` list; the last entry is the latest version.
    pub type Packages = Arc<Mutex<HashMap<String, Vec<(String, Vec<String>)>>>>;

    pub struct FakeThunder {
        pub base: String,
        /// `Owner-Name` -> list of `(version, dependencies)`; the last entry is the latest.
        pub packages: Packages,
        /// Archive served for every download.
        pub archives: Arc<Mutex<HashMap<String, Vec<u8>>>>,
        pub requests: Arc<Mutex<Vec<String>>>,
        /// Path (with query) → body served as is, for list endpoints.
        pub pages: Arc<Mutex<HashMap<String, String>>>,
    }

    pub fn start() -> FakeThunder {
        let server = Server::http("127.0.0.1:0").unwrap();
        let base = format!("http://{}", server.server_addr().to_ip().unwrap());
        let packages: Packages = Arc::default();
        let archives: Arc<Mutex<HashMap<String, Vec<u8>>>> = Arc::default();
        let requests: Arc<Mutex<Vec<String>>> = Arc::default();
        let pages: Arc<Mutex<HashMap<String, String>>> = Arc::default();
        let (p2, a2, r2, b2, g2) = (packages.clone(), archives.clone(), requests.clone(), base.clone(), pages.clone());
        std::thread::spawn(move || {
            for req in server.incoming_requests() {
                let url = req.url().to_string();
                r2.lock().unwrap().push(url.clone());
                let parts: Vec<&str> = url.trim_matches('/').split('/').collect();
                let version_doc = |ns: &str, name: &str, v: &str, deps: &Vec<String>| {
                    json!({"namespace": ns, "name": name, "version_number": v, "full_name": format!("{ns}-{name}-{v}"),
                           "download_url": format!("{b2}/package/download/{ns}/{name}/{v}/"), "dependencies": deps})
                };
                let page = g2.lock().unwrap().get(&url).cloned();
                let resp = match parts.as_slice() {
                    _ if page.is_some() => Response::from_string(page.unwrap_or_default()),
                    ["api", "experimental", "package", ns, name] | ["api", "experimental", "package", ns, name, _] => {
                        let pk = p2.lock().unwrap();
                        match pk.get(&format!("{ns}-{name}")) {
                            None => Response::from_string(r#"{"detail":"Not found."}"#).with_status_code(404),
                            Some(list) => {
                                let v = parts.get(5).and_then(|v| list.iter().find(|(x, _)| x == v)).or_else(|| if parts.len() == 5 { list.last() } else { None });
                                match v {
                                    None => Response::from_string(r#"{"detail":"Not found."}"#).with_status_code(404),
                                    Some((ver, deps)) if parts.len() == 5 => Response::from_string(json!({"namespace": ns, "name": name, "latest": version_doc(ns, name, ver, deps)}).to_string()),
                                    Some((ver, deps)) => Response::from_string(version_doc(ns, name, ver, deps).to_string()),
                                }
                            }
                        }
                    }
                    ["package", "download", ns, name, v] => match a2.lock().unwrap().get(&format!("{ns}-{name}")) {
                        Some(bytes) => Response::from_data(bytes.clone()).with_status_code(200),
                        None => {
                            let _ = v;
                            Response::from_string("missing").with_status_code(404)
                        }
                    },
                    _ => Response::from_string("not found").with_status_code(404),
                };
                let _ = req.respond(resp);
            }
        });
        FakeThunder { base, packages, archives, requests, pages }
    }
}

#[cfg(test)]
mod tests {
    use super::fake::*;
    use super::*;

    fn sample_list() -> Value {
        serde_json::json!([
            {"name":"Cool_Mod","owner":"Own","is_deprecated":false,"has_nsfw_content":false,"rating_score":12,"date_updated":"2026-01-01T00:00:00Z","categories":["Mods"],
             "versions":[{"version_number":"1.2.0","description":"Makes everything cooler","icon":"https://ccdn.thunderstore.io/i/a.png","downloads":500,"dependencies":["BepInEx-BepInExPack-5.4.2100","Zed-Cooler-1.0.0"]},{"version_number":"1.0.0","description":"old","icon":"","downloads":300}]},
            {"name":"Cooler","owner":"Zed","is_deprecated":false,"has_nsfw_content":false,"rating_score":1,"categories":[],"versions":[{"version_number":"1.0.0","description":"Alternative to Cool Mod","icon":"","downloads":9000}]},
            {"name":"Old_Thing","owner":"Own","is_deprecated":true,"has_nsfw_content":false,"versions":[{"version_number":"1.0.0","description":"cool but dead","downloads":1}]},
            {"name":"Spicy","owner":"Own","is_deprecated":false,"has_nsfw_content":true,"versions":[{"version_number":"1.0.0","description":"cool","downloads":1}]},
            {"name":"Texture_Pack","owner":"CoolGuy","is_deprecated":false,"has_nsfw_content":false,"versions":[{"version_number":"2.0.0","description":"HD textures","downloads":50}]}
        ])
    }

    #[test]
    fn the_package_list_is_read_and_searched_sensibly() {
        let all = parse_listing(&sample_list());
        assert_eq!(all.len(), 4, "deprecated packages are left out; adult ones are kept but marked");
        assert_eq!(all.iter().filter(|l| l.nsfw).count(), 1);
        assert_eq!((all[0].full_name.as_str(), all[0].version.as_str(), all[0].downloads), ("Own-Cool_Mod", "1.2.0", 800), "latest version, all downloads counted");
        assert_eq!(all[0].deps, vec!["BepInEx-BepInExPack", "Zed-Cooler"], "what the newest version needs, without versions");
        let names = |q: &str| search(&all, q, 10).iter().map(|l| l.full_name.clone()).collect::<Vec<_>>();
        assert_eq!(names("cool"), vec!["Zed-Cooler", "Own-Cool_Mod", "CoolGuy-Texture_Pack"], "names that start with it first, the more downloaded one before the other; owner matches last");
        assert_eq!(names("cool mod"), vec!["Own-Cool_Mod", "Zed-Cooler"], "every word has to match somewhere; the exact name ranks first");
        assert_eq!(names("cool texture"), vec!["CoolGuy-Texture_Pack"], "a package missing one of the words is out");
        assert_eq!(names("textures"), vec!["CoolGuy-Texture_Pack"]);
        assert_eq!(names("hd"), vec!["CoolGuy-Texture_Pack"], "descriptions are searched too");
        assert!(names("zzz").is_empty());
        assert_eq!(search(&all, "cool", 1).len(), 1);
    }

    #[test]
    fn a_games_community_is_found_by_name() {
        let c = |a: &str, b: &str| (a.to_string(), b.to_string());
        let list = vec![c("riskofrain2", "Risk of Rain 2"), c("lethal-company", "Lethal Company"), c("repo", "R.E.P.O.")];
        assert_eq!(match_community("Risk of Rain 2", &list).as_deref(), Some("riskofrain2"));
        assert_eq!(match_community("Lethal Company™", &list).as_deref(), Some("lethal-company"));
        assert_eq!(match_community("R.E.P.O.", &list).as_deref(), Some("repo"));
        assert_eq!(match_community("Among Us", &list), None);
        assert_eq!(match_community("", &list), None);
    }

    #[test]
    fn links_and_names_are_understood_and_hostile_input_is_not() {
        let r = |ns: &str, n: &str, v: Option<&str>| PkgRef { namespace: ns.into(), name: n.into(), version: v.map(Into::into) };
        for (input, want) in [
            ("https://thunderstore.io/c/riskofrain2/p/bbepis/BepInExPack/", r("bbepis", "BepInExPack", None)),
            ("https://thunderstore.io/c/riskofrain2/p/Owner/Cool_Mod/1.2.3/", r("Owner", "Cool_Mod", Some("1.2.3"))),
            ("thunderstore.io/package/Owner/Cool_Mod", r("Owner", "Cool_Mod", None)),
            ("https://thunderstore.io/c/lethal-company/p/Owner/Mod/?tab=versions#x", r("Owner", "Mod", None)),
            ("ror2mm://v1/install/thunderstore.io/Owner/Cool_Mod/1.2.3/", r("Owner", "Cool_Mod", Some("1.2.3"))),
            ("Owner-Cool_Mod", r("Owner", "Cool_Mod", None)),
            ("  Owner-Cool_Mod-10.0.1 ", r("Owner", "Cool_Mod", Some("10.0.1"))),
        ] {
            assert_eq!(parse_ref(input).unwrap(), want, "{input}");
        }
        for bad in ["", "   ", "Owner", "a-b-c-d", "Owner-Mod-1.2", "Owner-Mod-v1.2.3", "https://thunderstore.io/", "https://thunderstore.io/api/v1/x", "https://evil.example/c/g/p/Owner/Mod/",
                    "Own er-Mod", "Owner-../Mod", "https://thunderstore.io/c/g/p/Own%20er/Mod/", &"x".repeat(500)] {
            assert!(parse_ref(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn versions_compare_numerically() {
        assert!(version_newer("1.10.0", "1.9.3") && version_newer("2.0.0", "1.99.99") && !version_newer("1.2.3", "1.2.3") && !version_newer("1.2.2", "1.2.3"));
    }

    #[test]
    fn a_package_is_planned_with_its_dependencies_first_and_each_one_once() {
        let f = start();
        {
            let mut p = f.packages.lock().unwrap();
            p.insert("Own-Top".into(), vec![("1.0.0".into(), vec![]), ("1.1.0".into(), vec!["Lib-Core-2.0.0".into(), "Lib-Extra-1.0.0".into(), "bbepis-BepInExPack-5.4.2100".into()])]);
            p.insert("Lib-Core".into(), vec![("2.0.0".into(), vec!["bbepis-BepInExPack-5.4.2100".into()])]);
            p.insert("Lib-Extra".into(), vec![("1.0.0".into(), vec!["Lib-Core-2.0.0".into()])]);
            p.insert("bbepis-BepInExPack".into(), vec![("5.4.2100".into(), vec![])]);
        }
        let c = Client::new(&f.base, true);
        let plan = c.plan(&parse_ref("Own-Top").unwrap(), &HashSet::new()).unwrap();
        assert_eq!(plan.iter().map(|r| format!("{}-{}", r.pkg.full_name(), r.pkg.version)).collect::<Vec<_>>(), vec!["bbepis-BepInExPack-5.4.2100", "Lib-Core-2.0.0", "Lib-Extra-1.0.0", "Own-Top-1.1.0"]);
        assert!(plan[3].download_url.ends_with("/package/download/Own/Top/1.1.0/"));
        // what is already installed is not fetched again, but the package asked for always is
        let skip: HashSet<String> = ["bbepis-BepInExPack".to_string(), "Own-Top".to_string()].into();
        let plan = c.plan(&parse_ref("Own-Top").unwrap(), &skip).unwrap();
        assert_eq!(plan.iter().map(|r| r.pkg.name.as_str()).collect::<Vec<_>>(), vec!["Core", "Extra", "Top"]);
        // a pinned version is honoured
        assert_eq!(c.resolve(&parse_ref("Own-Top-1.0.0").unwrap()).unwrap().pkg.version, "1.0.0");
        // unknown packages and dead servers are explained
        assert!(c.resolve(&parse_ref("Nobody-Nothing").unwrap()).unwrap_err().contains("does not know"));
        assert!(c.resolve(&parse_ref("Own-Top-9.9.9").unwrap()).is_err());
        assert!(Client::new("http://127.0.0.1:1", true).resolve(&parse_ref("Own-Top").unwrap()).unwrap_err().contains("Could not reach"));
        assert!(Client::new("http://x.example", false).resolve(&parse_ref("Own-Top").unwrap()).unwrap_err().contains("HTTPS"));
    }

    #[test]
    fn dependency_loops_and_runaway_chains_stop() {
        let f = start();
        {
            let mut p = f.packages.lock().unwrap();
            p.insert("A-One".into(), vec![("1.0.0".into(), vec!["A-Two-1.0.0".into()])]);
            p.insert("A-Two".into(), vec![("1.0.0".into(), vec!["A-One-1.0.0".into()])]);
            for i in 0..12 {
                p.insert(format!("C-P{i}"), vec![("1.0.0".into(), vec![format!("C-P{}-1.0.0", i + 1)])]);
            }
        }
        let c = Client::new(&f.base, true);
        assert_eq!(c.plan(&parse_ref("A-One").unwrap(), &HashSet::new()).unwrap().len(), 2, "a loop is followed once");
        assert!(c.plan(&parse_ref("C-P0").unwrap(), &HashSet::new()).unwrap_err().contains("too many dependencies"));
    }

    #[test]
    fn downloads_land_in_the_given_file_or_nowhere() {
        let f = start();
        f.archives.lock().unwrap().insert("Own-Top".into(), vec![9u8; 1000]);
        let d = crate::fsutil::testutil::TempDir::new("thunder-dl");
        let c = Client::new(&f.base, true);
        c.download(&format!("{}/package/download/Own/Top/1.0.0/", f.base), &d.join("a.zip")).unwrap();
        assert_eq!(std::fs::metadata(d.join("a.zip")).unwrap().len(), 1000);
        assert!(c.download(&format!("{}/package/download/No/Pe/1.0.0/", f.base), &d.join("b.zip")).is_err());
        assert!(!d.join("b.zip").exists());
        assert!(c.download("ftp://x/y", &d.join("c.zip")).is_err());
    }
}
