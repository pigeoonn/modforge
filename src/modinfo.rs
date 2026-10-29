//! What the mod page shows: the author's own page (Nexus or Thunderstore) turned into a few plain,
//! safe fields, kept for a while so opening a page twice costs nothing and Nexus' request limit is
//! never a problem. The raw JSON is read here; the site clients only fetch it.

use crate::nexus::{ModFiles, Requirement};
use crate::richtext;
use serde::Serialize;
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct Change {
    pub version: String,
    pub name: String,
    pub category: String,
    pub date_ms: u64,
    pub html: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct Req {
    pub name: String,
    pub page_url: String,
    pub notes: String,
    pub external: bool,
    /// Already in this game's library.
    pub installed: bool,
    /// One of the game's core mods, found from the mod's files (Gantry installs it from its official releases).
    pub core: bool,
    #[serde(skip)]
    pub nexus_id: u64,
    /// Lower-case `owner-name` of a Thunderstore package.
    #[serde(skip)]
    pub key: String,
}

/// One mod page, ready for the screen.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct Page {
    /// "nexus", "thunderstore" or "local"
    pub source: String,
    pub name: String,
    pub summary: String,
    pub author: String,
    pub version: String,
    pub picture: String,
    pub endorsements: u64,
    pub downloads: u64,
    pub rating: i64,
    pub created_ms: u64,
    pub updated_ms: u64,
    pub page_url: String,
    pub categories: Vec<String>,
    pub description_html: String,
    pub changes: Vec<Change>,
    pub requirements: Vec<Req>,
}

fn s(v: &Value, k: &str) -> String {
    v.get(k).and_then(Value::as_str).unwrap_or("").trim().to_string()
}

fn n(v: &Value, k: &str) -> u64 {
    v.get(k).and_then(Value::as_u64).unwrap_or(0)
}

/// Thunderstore sends ISO dates ("2024-05-01T12:30:00.123Z"); the screen wants milliseconds.
pub fn iso_to_ms(d: &str) -> u64 {
    let b = d.as_bytes();
    if b.len() < 19 || b[4] != b'-' || b[7] != b'-' {
        return 0;
    }
    let num = |a: usize, z: usize| d.get(a..z).and_then(|x| x.parse::<i64>().ok());
    let (Some(y), Some(mo), Some(da), Some(h), Some(mi), Some(se)) = (num(0, 4), num(5, 7), num(8, 10), num(11, 13), num(14, 16), num(17, 19)) else { return 0 };
    // days since 1970-01-01 (civil-from-days, Howard Hinnant)
    let (y, m) = if mo <= 2 { (y - 1, mo + 9) } else { (y, mo - 3) };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * m + 2) / 5 + da - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    ((days * 86_400 + h * 3600 + mi * 60 + se).max(0) as u64) * 1000
}

/// The Nexus mod JSON as a page. Adult pages and withdrawn mods are not shown.
pub fn nexus_page(v: &Value, game: &str, mod_id: u64) -> Result<Page, String> {
    if v.get("contains_adult_content").and_then(Value::as_bool).unwrap_or(false) {
        return Err("This mod's page is marked for adults, so ModForge does not show it.".into());
    }
    if v.get("available").and_then(Value::as_bool) == Some(false) {
        return Err("The author has taken this mod down.".into());
    }
    Ok(Page {
        source: "nexus".into(),
        name: s(v, "name"),
        summary: crate::readme::plain(&s(v, "summary")).trim().to_string(),
        author: if s(v, "author").is_empty() { s(v, "uploaded_by") } else { s(v, "author") },
        version: s(v, "version"),
        picture: richtext::image_url(&s(v, "picture_url")).unwrap_or_default(),
        endorsements: n(v, "endorsement_count"),
        downloads: n(v, "mod_downloads"),
        rating: 0,
        created_ms: n(v, "created_timestamp") * 1000,
        updated_ms: n(v, "updated_timestamp") * 1000,
        page_url: format!("https://www.nexusmods.com/{game}/mods/{mod_id}"),
        categories: Vec::new(),
        description_html: richtext::bbcode_html(&s(v, "description")),
        changes: Vec::new(),
        requirements: Vec::new(),
    })
}

/// The author's changelogs, newest first (old versions, archives and files without words are left out).
pub fn nexus_changes(files: &ModFiles) -> Vec<Change> {
    let mut rows: Vec<&crate::nexus::ModFile> = files.files.iter().filter(|f| !matches!(f.category.to_uppercase().as_str(), "OLD_VERSION" | "ARCHIVED" | "DELETED") && !(f.changelog.trim().is_empty() && f.description.trim().is_empty())).collect();
    rows.sort_by_key(|f| std::cmp::Reverse(f.uploaded));
    rows.into_iter()
        .take(15)
        .map(|f| Change {
            version: f.version.clone(),
            name: f.name.clone(),
            category: f.category.to_lowercase(),
            date_ms: f.uploaded * 1000,
            html: richtext::text_html(if f.changelog.trim().is_empty() { &f.description } else { &f.changelog }),
        })
        .collect()
}

pub fn nexus_requirements(reqs: &[Requirement]) -> Vec<Req> {
    reqs.iter().map(|r| Req { name: r.name.clone(), page_url: r.page(), notes: crate::readme::plain(&r.notes).trim().to_string(), external: r.external, installed: false, nexus_id: if r.external { 0 } else { r.mod_id }, key: String::new(), core: false }).collect()
}

impl Page {
    /// Say which requirements the game's library already has.
    pub fn mark_installed(&mut self, nexus_ids: &HashSet<u64>, thunder_keys: &HashSet<String>) {
        for r in &mut self.requirements {
            r.installed = (r.nexus_id > 0 && nexus_ids.contains(&r.nexus_id)) || (!r.key.is_empty() && thunder_keys.contains(&r.key));
        }
    }
}

/// A Thunderstore package (`/api/experimental/package/<owner>/<name>/`) with its README and changelog (Markdown).
/// The page of a mod.io mod: its own text, the changelog of the current file and the mods it needs.
pub fn modio_page(c: &crate::modio::Card, deps: &[crate::modio::Dep]) -> Page {
    let mut changes = Vec::new();
    if !c.changelog.trim().is_empty() {
        changes.push(Change { version: c.version.clone(), name: "Changelog".into(), category: "main".into(), date_ms: c.updated_ms, html: richtext::text_html(&c.changelog) });
    }
    let page_url = richtext::safe_url(&c.page_url).unwrap_or_default();
    Page {
        source: "modio".into(),
        name: c.name.clone(),
        summary: c.summary.clone(),
        author: c.author.clone(),
        version: c.version.clone(),
        picture: richtext::image_url(&c.picture).unwrap_or_default(),
        endorsements: 0,
        downloads: c.downloads,
        rating: 0,
        created_ms: c.created_ms,
        updated_ms: c.updated_ms,
        page_url,
        categories: c.tags.clone(),
        description_html: richtext::text_html(&c.description),
        changes,
        requirements: deps.iter().map(|d| Req { name: d.name.clone(), page_url: String::new(), notes: String::new(), external: false, installed: false, nexus_id: 0, key: format!("modio:{}", d.mod_id), core: false }).collect(),
    }
}

pub fn thunder_page(pkg: &Value, readme_md: &str, changelog_md: &str) -> Page {
    let latest = pkg.get("latest").cloned().unwrap_or(Value::Null);
    let (owner, name) = (s(pkg, "owner").to_string(), s(pkg, "name"));
    let owner = if owner.is_empty() { s(&latest, "namespace") } else { owner };
    let version = s(&latest, "version_number");
    let mut changes = Vec::new();
    if !changelog_md.trim().is_empty() {
        changes.push(Change { version: version.clone(), name: "Changelog".into(), category: "main".into(), date_ms: iso_to_ms(&s(&latest, "date_created")), html: richtext::markdown_html(changelog_md) });
    }
    let requirements = latest
        .get("dependencies")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(Value::as_str)
                .filter_map(|d| {
                    // "Owner-Name-1.2.3"
                    let (base, ver) = d.rsplit_once('-')?;
                    let (o, nm) = base.split_once('-')?;
                    Some(Req { name: [nm, ver].join(" "), page_url: format!("https://thunderstore.io/package/{o}/{nm}/"), notes: o.to_string(), external: false, installed: false, nexus_id: 0, key: base.to_lowercase(), core: false })
                })
                .collect()
        })
        .unwrap_or_default();
    let description = s(&latest, "description");
    Page {
        source: "thunderstore".into(),
        name: if name.is_empty() { s(&latest, "name") } else { name.replace('_', " ") },
        summary: description,
        author: owner.clone(),
        version,
        picture: richtext::image_url(&s(&latest, "icon")).unwrap_or_default(),
        endorsements: 0,
        downloads: pkg.get("total_downloads").and_then(Value::as_u64).unwrap_or_else(|| n(&latest, "downloads")),
        rating: pkg.get("rating_score").and_then(Value::as_i64).unwrap_or(0),
        created_ms: iso_to_ms(&s(pkg, "date_created")),
        updated_ms: iso_to_ms(&s(pkg, "date_updated")),
        page_url: richtext::safe_url(&s(pkg, "package_url")).unwrap_or_else(|| format!("https://thunderstore.io/package/{owner}/{}/", s(pkg, "name"))),
        categories: Vec::new(),
        description_html: richtext::markdown_html(readme_md),
        changes,
        requirements,
    }
}

// ───────────────────────────── cache ─────────────────────────────

const FRESH: Duration = Duration::from_secs(600);
static CACHE: LazyLock<Mutex<HashMap<String, (Instant, Page)>>> = LazyLock::new(|| Mutex::new(HashMap::new()));

/// A page fetched in the last ten minutes, or the result of `fetch`. When fetching fails, an older
/// copy is better than nothing; the second value says the copy is old.
pub fn cached(key: &str, fetch: impl FnOnce() -> Result<Page, String>) -> Result<(Page, bool), String> {
    if let Some((at, p)) = CACHE.lock().unwrap_or_else(|e| e.into_inner()).get(key) {
        if at.elapsed() < FRESH {
            return Ok((p.clone(), false));
        }
    }
    match fetch() {
        Ok(p) => {
            let mut c = CACHE.lock().unwrap_or_else(|e| e.into_inner());
            if c.len() > 200 {
                c.clear();
            }
            c.insert(key.to_string(), (Instant::now(), p.clone()));
            Ok((p, false))
        }
        Err(e) => match CACHE.lock().unwrap_or_else(|e| e.into_inner()).get(key) {
            Some((_, p)) => Ok((p.clone(), true)),
            None => Err(e),
        },
    }
}

/// Empty the cache without taking the gate (for a test that already holds it).
#[cfg(test)]
pub fn forget_pages() {
    CACHE.lock().unwrap_or_else(|e| e.into_inner()).clear();
}

#[cfg(test)]
static GATE: Mutex<()> = Mutex::new(());

/// Empty the cache. The returned guard keeps other tests that use the cache from running at the
/// same time (they would otherwise find each other's pages): hold it for the whole test.
#[cfg(test)]
pub fn forget_all() -> std::sync::MutexGuard<'static, ()> {
    let gate = GATE.lock().unwrap_or_else(|e| e.into_inner());
    CACHE.lock().unwrap_or_else(|e| e.into_inner()).clear();
    gate
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::nexus::ModFile;
    use serde_json::json;

    #[test]
    fn dates_from_thunderstore_become_milliseconds() {
        assert_eq!(iso_to_ms("1970-01-01T00:00:00Z"), 0);
        assert_eq!(iso_to_ms("2000-03-01T00:00:01.5Z"), 951_868_801_000);
        assert_eq!(iso_to_ms("2024-02-29T12:30:00.123456Z"), 1_709_209_800_000);
        assert_eq!(iso_to_ms("garbage"), 0);
        assert_eq!(iso_to_ms(""), 0);
    }

    #[test]
    fn a_nexus_page_is_read_and_adult_or_removed_ones_are_refused() {
        let v = json!({ "name": "Cool Cars", "summary": "<b>Fast</b> cars", "description": "[b]Install[/b] by copying.<br />[url=https://example.com]site[/url]", "version": "1.2", "author": "someone",
            "endorsement_count": 5, "mod_downloads": 900, "created_timestamp": 100, "updated_timestamp": 200, "picture_url": "https://staticdelivery.nexusmods.com/mods/1/p.jpg", "available": true, "contains_adult_content": false });
        let p = nexus_page(&v, "needforspeedmostwanted2012", 77).unwrap();
        assert_eq!((p.name.as_str(), p.version.as_str(), p.author.as_str(), p.endorsements, p.downloads), ("Cool Cars", "1.2", "someone", 5, 900));
        assert_eq!((p.created_ms, p.updated_ms), (100_000, 200_000));
        assert_eq!(p.page_url, "https://www.nexusmods.com/needforspeedmostwanted2012/mods/77");
        assert_eq!(p.summary, "Fast cars");
        assert!(p.description_html.contains("<strong>Install</strong>") && p.description_html.contains("data-url=\"https://example.com\""), "{}", p.description_html);
        assert!(p.picture.starts_with("https://staticdelivery.nexusmods.com/"));
        assert!(nexus_page(&json!({ "name": "x", "contains_adult_content": true }), "g", 1).is_err());
        assert!(nexus_page(&json!({ "name": "x", "available": false }), "g", 1).is_err());
        // a picture from anywhere else is dropped
        assert_eq!(nexus_page(&json!({ "name": "x", "picture_url": "https://evil.example/a.png" }), "g", 1).unwrap().picture, "");
    }

    fn file(id: u64, v: &str, cat: &str, up: u64, log: &str, desc: &str) -> ModFile {
        ModFile { file_id: id, name: format!("f{id}"), file_name: format!("f{id}.zip"), version: v.into(), category: cat.into(), uploaded: up, description: desc.into(), changelog: log.into() }
    }

    #[test]
    fn changelogs_come_newest_first_without_old_versions_or_empty_ones() {
        let files = ModFiles { files: vec![file(1, "1.0", "MAIN", 100, "first release", ""), file(2, "1.1", "UPDATE", 300, "<li>fixed the crash</li><li>new car</li>", ""), file(3, "0.9", "OLD_VERSION", 50, "ancient", ""), file(4, "1.1b", "OPTIONAL", 200, "", ""), file(5, "x", "MAIN", 400, "", "Only a description")], updates: vec![] };
        let c = nexus_changes(&files);
        assert_eq!(c.iter().map(|x| x.version.as_str()).collect::<Vec<_>>(), vec!["x", "1.1", "1.0"]);
        assert!(c[1].html.contains("<li>fixed the crash</li><li>new car</li>"), "{}", c[1].html);
        assert_eq!(c[1].date_ms, 300_000);
        assert!(c[0].html.contains("Only a description"));
    }

    #[test]
    fn requirements_know_what_is_already_installed() {
        let reqs = vec![
            Requirement { mod_id: 5, game: "g".into(), name: "Lib".into(), url: String::new(), notes: "needed <b>first</b>".into(), external: false },
            Requirement { mod_id: 0, game: "g".into(), name: "Runtime".into(), url: "https://example.com/rt".into(), notes: String::new(), external: true },
        ];
        let mut page = Page { requirements: nexus_requirements(&reqs), ..Default::default() };
        page.mark_installed(&HashSet::from([5]), &HashSet::new());
        let r = page.requirements.clone();
        assert!(r[0].installed && !r[1].installed);
        assert_eq!(r[0].notes, "needed first");
        assert_eq!(r[0].page_url, "https://www.nexusmods.com/g/mods/5");
        assert_eq!(r[1].page_url, "https://example.com/rt");
        page.mark_installed(&HashSet::new(), &HashSet::new());
        assert!(page.requirements.iter().all(|x| !x.installed));
    }

    #[test]
    fn a_thunderstore_package_gets_readme_changelog_and_dependencies() {
        let pkg = json!({ "owner": "bbepis", "name": "BepInExPack", "full_name": "bbepis-BepInExPack", "package_url": "https://thunderstore.io/c/riskofrain2/p/bbepis/BepInExPack/", "date_created": "2020-01-01T00:00:00Z", "date_updated": "2024-02-29T12:30:00Z",
            "rating_score": 42, "total_downloads": 12345,
            "latest": { "version_number": "5.4.2100", "description": "The loader", "icon": "https://gcdn.thunderstore.io/live/repository/icons/x.png", "date_created": "2024-02-29T12:30:00Z", "dependencies": ["Owner-Lib-1.0.0", "Other-Thing-2.1.0"] } });
        let mut p = thunder_page(&pkg, "# Hello\n\nUse **this**.", "- fixed things");
        p.mark_installed(&HashSet::new(), &HashSet::from(["owner-lib".to_string()]));
        assert_eq!((p.source.as_str(), p.name.as_str(), p.author.as_str(), p.version.as_str(), p.downloads, p.rating), ("thunderstore", "BepInExPack", "bbepis", "5.4.2100", 12345, 42));
        assert!(p.description_html.contains("<h3>Hello</h3>") && p.description_html.contains("<strong>this</strong>"));
        assert_eq!(p.changes.len(), 1);
        assert!(p.changes[0].html.contains("<li>fixed things</li>"));
        assert_eq!(p.requirements.len(), 2);
        assert!(p.requirements[0].installed && !p.requirements[1].installed);
        assert_eq!(p.requirements[1].page_url, "https://thunderstore.io/package/Other/Thing/");
        assert_eq!(p.updated_ms, 1_709_209_800_000);
        assert_eq!(p.page_url, "https://thunderstore.io/c/riskofrain2/p/bbepis/BepInExPack/");
        // no changelog, no dependencies, nothing known: still a page
        let bare = thunder_page(&json!({ "owner": "a", "name": "b", "latest": {} }), "", "");
        assert!(bare.changes.is_empty() && bare.requirements.is_empty() && bare.name == "b");
    }

    #[test]
    fn pages_are_kept_for_a_while_and_an_old_copy_beats_an_error() {
        let _gate = forget_all();
        let page = |n: &str| Page { name: n.into(), ..Default::default() };
        assert_eq!(cached("k1", || Ok(page("one"))).unwrap().0.name, "one");
        // fresh: the fetch is not even called
        assert_eq!(cached("k1", || panic!("must not fetch")).unwrap(), (page("one"), false));
        // an error with nothing kept is an error
        assert!(cached("k2", || Err("down".into())).is_err());
        // an old copy is returned with the "old" flag when the site is down
        CACHE.lock().unwrap().insert("k3".into(), (Instant::now() - Duration::from_secs(9999), page("old")));
        assert_eq!(cached("k3", || Err("down".into())).unwrap(), (page("old"), true));
        assert_eq!(cached("k3", || Ok(page("new"))).unwrap(), (page("new"), false));
    }
}
