//! Nexus Mods integration: parse `nxm://` links from the "Mod Manager Download" button and
//! fetch the file through the public API using the user's personal API key.
//!
//! Network access goes through `curl` (already a runtime dependency). The key is handed to
//! curl on stdin so it never appears in a process list, and the link/ids are validated before
//! they are placed into any URL.

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

pub const DEFAULT_BASE: &str = "https://api.nexusmods.com";
const APP_NAME: &str = "ModForge";

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct NxmLink {
    pub game: String,
    pub mod_id: u64,
    pub file_id: u64,
    /// One-time download key and expiry that Nexus puts in links for free accounts.
    pub key: Option<String>,
    pub expires: Option<String>,
}

/// Where a library mod came from on Nexus Mods, so updates can be found later.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct NexusRef {
    pub game: String,
    pub mod_id: u64,
    pub file_id: u64,
}

/// When a downloaded file was uploaded to Nexus Mods, as its name tells it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Uploaded {
    /// Old names end in the upload time in seconds.
    Second(u64),
    /// New names carry it to the minute, as `2026-10-20T10-52Z`.
    Minute(u64),
}

/// What the name of a file downloaded from Nexus Mods says about it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DownloadName {
    pub name: String,
    pub mod_id: u64,
    pub version: String,
    pub uploaded: Option<Uploaded>,
    /// The newer naming (`Name 4198 1.27.5 2026-10-20T10-52Z q7jg7fflx`) rather than `Name-4198-1-27-5-1760956320`.
    pub modern: bool,
}

/// Seconds since 1970 of a calendar time in UTC.
fn epoch_of(y: i64, mo: i64, d: i64, h: i64, mi: i64) -> u64 {
    let y2 = if mo <= 2 { y - 1 } else { y };
    let era = y2.div_euclid(400);
    let yoe = y2 - era * 400;
    let doy = (153 * (mo + if mo > 2 { -3 } else { 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    (days * 86_400 + h * 3600 + mi * 60).max(0) as u64
}

/// Reads both naming styles of Nexus downloads (`Cool Mod-123-1-2-1700000000.zip` and
/// `Cool Mod 123 1.2 2026-10-20T10-52Z q7jg7fflx.zip`); `None` when the name is not one of them.
pub fn parse_download_name(file_name: &str) -> Option<DownloadName> {
    let stem = std::path::Path::new(file_name).file_stem()?.to_string_lossy().to_string();
    let stem = stem.trim_end_matches(".tar").to_string();
    static NEW: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| regex::Regex::new(r"^(.+?) (\d{1,9}) (\S+) (\d{4})-(\d{2})-(\d{2})T(\d{2})-(\d{2})Z \S+$").expect("static regex"));
    static OLD: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| regex::Regex::new(r"^(.*?)-(\d+)((?:-v?\d+)+)$").expect("static regex"));
    if let Some(c) = NEW.captures(&stem) {
        let n = |i: usize| c[i].parse::<i64>().unwrap_or(0);
        let (mo, d, h, mi) = (n(5), n(6), n(7), n(8));
        let ok = (1..=12).contains(&mo) && (1..=31).contains(&d) && h < 24 && mi < 60;
        return Some(DownloadName {
            name: c[1].trim().to_string(),
            mod_id: c[2].parse().ok()?,
            version: c[3].to_string(),
            uploaded: ok.then(|| Uploaded::Minute(epoch_of(n(4), mo, d, h, mi))),
            modern: true,
        });
    }
    let c = OLD.captures(&stem)?;
    let segs: Vec<&str> = c[3].split('-').filter(|s| !s.is_empty()).collect();
    let version = if segs.len() >= 2 { segs[..segs.len() - 1].join(".") } else { String::new() };
    let uploaded = segs.last().filter(|t| t.len() >= 9).and_then(|t| t.parse().ok()).map(Uploaded::Second);
    let name = if c[1].contains(' ') { c[1].to_string() } else { c[1].replace('_', " ") };
    Some(DownloadName { name: name.trim().to_string(), mod_id: c[2].parse().ok()?, version, uploaded, modern: false })
}

/// Game domains end up inside request URLs, so anything hand-edited is checked again.
pub fn domain_ok(g: &str) -> bool {
    !g.is_empty() && g.len() <= 64 && g.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

impl NexusRef {
    pub fn valid(&self) -> bool {
        domain_ok(&self.game) && self.mod_id > 0
    }

    /// The mod's file list; `nmm=1` makes Nexus offer the "Mod Manager Download" for that file.
    pub fn files_page(&self, file_id: u64) -> String {
        format!("https://www.nexusmods.com/{}/mods/{}?tab=files&file_id={}&nmm=1", self.game, self.mod_id, file_id)
    }
}

impl From<&NxmLink> for NexusRef {
    fn from(l: &NxmLink) -> Self {
        NexusRef { game: l.game.clone(), mod_id: l.mod_id, file_id: l.file_id }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModFile {
    pub file_id: u64,
    pub name: String,
    pub file_name: String,
    pub version: String,
    pub category: String,
    pub uploaded: u64,
    /// The author's description of this file and what changed in it, as plain text.
    pub description: String,
    pub changelog: String,
}

/// Something a mod needs before it works: another Nexus mod, or a file from elsewhere.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Requirement {
    pub mod_id: u64,
    pub game: String,
    pub name: String,
    pub url: String,
    pub notes: String,
    pub external: bool,
}

impl Requirement {
    /// Where to get it: its Nexus page, or the address the author gave.
    pub fn page(&self) -> String {
        if !self.external && self.mod_id > 0 && domain_ok(&self.game) {
            format!("https://www.nexusmods.com/{}/mods/{}", self.game, self.mod_id)
        } else if self.url.starts_with("https://") {
            self.url.clone()
        } else {
            String::new()
        }
    }
}

/// A mod as the browse lists show it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Listing {
    pub mod_id: u64,
    pub name: String,
    pub summary: String,
    pub version: String,
    pub author: String,
    pub endorsements: u64,
    pub picture: String,
    pub downloads: u64,
    /// When the mod was last changed (milliseconds since 1970; 0 = unknown).
    pub updated_ms: u64,
    pub size_kb: u64,
    pub category: String,
}

/// What the mod store asks for.
#[derive(Clone, Debug, Default)]
pub struct StoreQuery {
    /// "popular", "lately", "endorsed", "new" or "updated"
    pub sort: String,
    pub query: String,
    pub category: String,
    pub adult: bool,
    pub count: usize,
    pub offset: usize,
}

fn norm(s: &str) -> String {
    s.chars().filter(|c| c.is_alphanumeric()).collect::<String>().to_lowercase()
}

/// What Nexus says about one file of a mod.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileInfo {
    pub name: String,
    pub version: String,
    /// The MD5 Nexus published (32 hex digits), when it did.
    pub md5: Option<String>,
}

/// A Nexus collection (a list of mods, each with its exact file) as far as Gantry can use it.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct Collection {
    pub name: String,
    pub slug: String,
    pub domain: String,
    pub game_name: String,
    pub revision: u32,
    pub mods: Vec<CollectionMod>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct CollectionMod {
    pub mod_id: u64,
    pub file_id: u64,
    pub name: String,
    pub version: String,
    pub size: u64,
    pub optional: bool,
    pub domain: String,
}

fn collection_slug_ok(s: &str) -> bool {
    !s.is_empty() && s.len() <= 40 && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// The collection a link points to: `(game domain, slug, revision)`. Accepts the website's addresses and
/// the `nxm://<game>/collections/<slug>/revisions/<n>` link of the "Add to Vortex" button.
pub fn parse_collection_link(link: &str) -> Option<(String, String, Option<u32>)> {
    let l = link.trim();
    let rest = l.strip_prefix("nxm://").or_else(|| l.strip_prefix("https://www.nexusmods.com/")).or_else(|| l.strip_prefix("https://next.nexusmods.com/")).or_else(|| l.strip_prefix("https://nexusmods.com/"))?;
    let path = rest.split(['?', '#']).next().unwrap_or("");
    let parts: Vec<&str> = path.split('/').filter(|p| !p.is_empty()).collect();
    // [games/]<domain>/collections/<slug>[/revisions/<n> | /mods ...]
    let parts = if parts.first() == Some(&"games") { &parts[1..] } else { &parts[..] };
    if parts.len() < 3 || parts[1] != "collections" || !domain_ok(parts[0]) || !collection_slug_ok(parts[2]) {
        return None;
    }
    let rev = (parts.get(3) == Some(&"revisions")).then(|| parts.get(4).and_then(|r| r.parse::<u32>().ok())).flatten();
    Some((parts[0].to_string(), parts[2].to_string(), rev))
}

fn parse_collection(v: &Value) -> Result<Collection, String> {
    let s = |x: &Value, k: &str| x.get(k).and_then(Value::as_str).unwrap_or("").to_string();
    let c = v.get("collection").ok_or("Nexus does not know that collection (or it is hidden).")?;
    let mods = v
        .get("modFiles")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|m| {
                    let f = m.get("file")?;
                    let md = f.get("mod")?;
                    Some(CollectionMod {
                        mod_id: md.get("modId").and_then(Value::as_u64)?,
                        file_id: m.get("fileId").and_then(Value::as_u64).or_else(|| f.get("fileId").and_then(Value::as_u64))?,
                        name: s(md, "name"),
                        version: s(f, "version"),
                        size: f.get("sizeInBytes").and_then(|x| x.as_u64().or_else(|| x.as_str().and_then(|t| t.parse().ok()))).unwrap_or(0),
                        optional: m.get("optional").and_then(Value::as_bool).unwrap_or(false),
                        domain: md.get("game").map(|g| s(g, "domainName")).filter(|d| !d.is_empty()).unwrap_or_else(|| c.get("game").map(|g| s(g, "domainName")).unwrap_or_default()),
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    Ok(Collection {
        name: s(c, "name"),
        slug: s(c, "slug"),
        domain: c.get("game").map(|g| s(g, "domainName")).unwrap_or_default(),
        game_name: c.get("game").map(|g| s(g, "name")).unwrap_or_default(),
        revision: v.get("revisionNumber").and_then(Value::as_u64).unwrap_or(0) as u32,
        mods,
    })
}

/// The Nexus game domain of a game, found by name ("Need for Speed Most Wanted" → the newest
/// "(2012)" one when several share the name).
pub fn match_domain(game_name: &str, games: &[(String, String)]) -> Option<String> {
    let want = norm(&game_name.replace(['™', '®'], ""));
    if want.len() < 3 {
        return None;
    }
    if let Some((d, _)) = games.iter().find(|(_, n)| norm(n) == want) {
        return Some(d.clone());
    }
    // "Name (2012)": same name plus a year; the newest year wins
    let mut with_year: Vec<(&String, String)> = games.iter().filter_map(|(d, n)| n.rsplit_once(" (").map(|(base, y)| (d, base.to_string(), y.trim_end_matches(')').to_string()))).filter(|(_, b, y)| norm(b) == want && y.len() == 4 && y.chars().all(|c| c.is_ascii_digit())).map(|(d, _, y)| (d, y)).collect();
    with_year.sort_by(|a, b| b.1.cmp(&a.1));
    with_year.first().map(|(d, _)| (*d).clone())
}

/// The mod's own page: what the author wrote about it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ModPage {
    pub name: String,
    pub summary: String,
    pub description: String,
    pub version: String,
}

/// Instructions from the Nexus page in one readable text (it is kept next to the mod like a readme).
pub fn page_notes(page: &ModPage, file: Option<&ModFile>) -> String {
    let mut out = String::new();
    let mut part = |title: &str, body: &str| {
        let body = crate::readme::plain(body);
        let body = body.trim();
        if !body.is_empty() {
            out.push_str(&format!("{title}\n{body}\n\n"));
        }
    };
    part(&page.name, &page.summary);
    if let Some(f) = file {
        part("File", &f.description);
        part("Changes", &f.changelog);
    }
    part("Description", &page.description);
    out.trim().to_string()
}

/// A mod's files plus the "this file replaced that one" chain its author maintains.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ModFiles {
    pub files: Vec<ModFile>,
    /// `(old file id, new file id, uploaded)`
    pub updates: Vec<(u64, u64, u64)>,
}

impl ModFiles {
    /// The file on the site that a downloaded file came from: the one uploaded at the time its name says, else the only one
    /// with its version.
    pub fn find_download(&self, d: &DownloadName) -> Option<&ModFile> {
        let by_time: Vec<&ModFile> = self
            .files
            .iter()
            .filter(|f| match d.uploaded {
                Some(Uploaded::Second(s)) => f.uploaded == s,
                Some(Uploaded::Minute(m)) => f.uploaded / 60 == m / 60,
                None => false,
            })
            .collect();
        match by_time.as_slice() {
            [one] => return Some(one),
            [] => {}
            many => return many.iter().copied().find(|f| f.version.eq_ignore_ascii_case(&d.version)),
        }
        let by_version: Vec<&ModFile> = self.files.iter().filter(|f| !d.version.is_empty() && f.version.eq_ignore_ascii_case(&d.version)).collect();
        match by_version.as_slice() {
            [one] => Some(one),
            _ => None,
        }
    }

    /// The newest file that replaced `file_id` (following the author's replacement chain), if any.
    /// Files that merely exist next to it (optional files, other mains) never count as updates,
    /// and a malformed chain that loops back is resolved by upload time, not by position.
    pub fn newer_than(&self, file_id: u64) -> Option<&ModFile> {
        let mut seen = vec![file_id];
        let mut cur = file_id;
        while seen.len() < 64 {
            let next = self.updates.iter().filter(|(old, ..)| *old == cur).max_by_key(|(_, _, up)| *up).map(|(_, new, _)| *new);
            match next {
                Some(n) if !seen.contains(&n) => {
                    seen.push(n);
                    cur = n;
                }
                _ => break,
            }
        }
        seen[1..].iter().filter_map(|id| self.files.iter().find(|f| f.file_id == *id)).max_by_key(|f| f.uploaded)
    }
}

fn url_safe(s: &str) -> bool {
    !s.is_empty() && s.len() <= 400 && s.chars().all(|c| c.is_ascii_alphanumeric() || "+/=_-.%".contains(c))
}

/// Personal API keys are long base64-like strings; anything else is rejected before use.
pub fn valid_api_key(k: &str) -> bool {
    (16..=400).contains(&k.len()) && k.chars().all(|c| c.is_ascii_alphanumeric() || "+/=_-".contains(c))
}

/// `nxm://<game>/mods/<id>/files/<id>?key=...&expires=...&user_id=...`
pub fn parse_nxm(url: &str) -> Result<NxmLink, String> {
    let url = url.trim();
    let rest = url
        .get(..6)
        .filter(|p| p.eq_ignore_ascii_case("nxm://"))
        .map(|_| &url[6..])
        .ok_or("Not an nxm:// link.")?;
    let (path, query) = rest.split_once('?').unwrap_or((rest, ""));
    let seg: Vec<&str> = path.trim_end_matches('/').split('/').collect();
    let [game, "mods", mod_id, "files", file_id] = seg[..] else {
        return Err("Unrecognised nxm:// link (expected game/mods/ID/files/ID).".into());
    };
    if !domain_ok(game) {
        return Err("Unrecognised game name in the link.".into());
    }
    let id = |s: &str| s.parse::<u64>().ok().filter(|_| s.bytes().all(|b| b.is_ascii_digit())).ok_or("Bad id in the link.");
    let (mut key, mut expires) = (None, None);
    for kv in query.split('&').filter(|s| !s.is_empty()) {
        let (k, v) = kv.split_once('=').unwrap_or((kv, ""));
        match k {
            "key" if url_safe(v) => key = Some(v.to_string()),
            "expires" if v.bytes().all(|b| b.is_ascii_digit()) && !v.is_empty() => expires = Some(v.to_string()),
            _ => {}
        }
    }
    Ok(NxmLink { game: game.to_string(), mod_id: id(mod_id)?, file_id: id(file_id)?, key, expires })
}

/// A file name that is safe to create inside a folder.
pub fn safe_file_name(name: &str, fallback: &str) -> String {
    let base = Path::new(name.replace('\\', "/").as_str())
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();
    let cleaned: String = base.chars().filter(|c| !c.is_control()).collect();
    let cleaned = cleaned.trim().trim_start_matches('.').to_string();
    if cleaned.is_empty() {
        fallback.to_string()
    } else {
        cleaned
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UserInfo {
    pub name: String,
    pub premium: bool,
}

pub struct Nexus {
    base: String,
    key: String,
    allow_http: bool,
}

pub(crate) fn curl_ok(cmd: &mut Command, stdin: Option<&str>) -> Result<(String, String), String> {
    let mut child = cmd
        .stdin(if stdin.is_some() { Stdio::piped() } else { Stdio::null() })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("Could not run curl ({}). Is it installed?", e))?;
    if let (Some(text), Some(mut si)) = (stdin, child.stdin.take()) {
        let _ = si.write_all(text.as_bytes());
    }
    let out = child.wait_with_output().map_err(|e| e.to_string())?;
    Ok((String::from_utf8_lossy(&out.stdout).into_owned(), String::from_utf8_lossy(&out.stderr).into_owned()))
}

impl Nexus {
    /// `allow_http` exists only so tests can talk to a local fake server.
    pub fn new(base: &str, key: &str, allow_http: bool) -> Result<Self, String> {
        if !valid_api_key(key) {
            return Err("That does not look like a Nexus API key.".into());
        }
        Ok(Self { base: base.trim_end_matches('/').to_string(), key: key.to_string(), allow_http })
    }

    fn check_url(&self, url: &str) -> Result<(), String> {
        if url.starts_with("https://") || (self.allow_http && url.starts_with("http://")) {
            Ok(())
        } else {
            Err("Refusing a non-HTTPS download address.".into())
        }
    }

    /// GET a JSON document from the API; errors carry Nexus's own message when it sent one.
    fn get_json(&self, path_query: &str) -> Result<Value, String> {
        self.send(path_query, None)
    }

    fn send(&self, path_query: &str, post: Option<&str>) -> Result<Value, String> {
        self.request(if post.is_some() { "POST" } else { "GET" }, path_query, post.map(|b| ("application/json", b)))
    }

    /// Any request to the API; `body` is `(content type, text)`.
    fn request(&self, method: &str, path_query: &str, body: Option<(&str, &str)>) -> Result<Value, String> {
        let url = format!("{}{}", self.base, path_query);
        self.check_url(&url)?;
        let config = format!(
            "header = \"apikey: {}\"\nheader = \"Application-Name: {}\"\nheader = \"Application-Version: {}\"\nuser-agent = \"{}/{} (Linux)\"\n",
            self.key,
            APP_NAME,
            env!("CARGO_PKG_VERSION"),
            APP_NAME,
            env!("CARGO_PKG_VERSION")
        );
        let config = match body {
            Some((ct, _)) => format!("{config}header = \"Content-Type: {ct}\"\n"),
            None => config,
        };
        let mut cmd = Command::new("curl");
        cmd.args(["-sS", "-L", "--max-time", "30", "-w", "\n%{http_code}", "-K", "-"]);
        if method != "GET" && method != "POST" {
            cmd.args(["-X", method]);
        }
        if let Some((_, b)) = body {
            cmd.args(["--data-binary", b]);
        }
        let (out, err) = curl_ok(cmd.arg(&url), Some(&config))?;
        let (body, code) = out.rsplit_once('\n').unwrap_or(("", "000"));
        let code: u16 = code.trim().parse().unwrap_or(0);
        let json: Option<Value> = serde_json::from_str(body).ok();
        if (200..300).contains(&code) {
            return json.ok_or_else(|| "Nexus sent an unreadable reply.".to_string());
        }
        let msg = json
            .as_ref()
            .and_then(|v| v.get("message").or_else(|| v.get("error")))
            .and_then(|m| m.as_str())
            .map(str::to_string);
        Err(match (code, msg) {
            (0, _) => format!("Could not reach Nexus Mods ({}).", err.trim().lines().last().unwrap_or("network error")),
            (401, _) => "Nexus rejected the API key. Check it, or request a new one.".into(),
            (403, m) => format!("Nexus refused the download{}. Free accounts must start downloads from the website's \"Mod Manager Download\" button.", m.map(|m| format!(" ({})", m)).unwrap_or_default()),
            (429, _) => "Nexus rate limit reached; try again in a while.".into(),
            (c, Some(m)) => format!("Nexus error {}: {}", c, m),
            (c, None) => format!("Nexus error {}.", c),
        })
    }

    pub fn validate(&self) -> Result<UserInfo, String> {
        let v = self.get_json("/v1/users/validate.json")?;
        Ok(UserInfo {
            name: v.get("name").and_then(Value::as_str).unwrap_or("unknown").to_string(),
            premium: v.get("is_premium").and_then(Value::as_bool).unwrap_or(false),
        })
    }

    fn file_path(link: &NxmLink) -> String {
        format!("/v1/games/{}/mods/{}/files/{}", link.game, link.mod_id, link.file_id)
    }

    /// Original file name of the mod file (best effort: falls back to `fallback`).
    pub fn file_name(&self, link: &NxmLink, fallback: &str) -> String {
        self.get_json(&format!("{}.json", Self::file_path(link)))
            .ok()
            .and_then(|v| v.get("file_name").and_then(Value::as_str).map(|n| safe_file_name(n, fallback)))
            .unwrap_or_else(|| fallback.to_string())
    }

    /// File name, version and the checksum Nexus published for one file (best effort).
    pub fn file_info(&self, link: &NxmLink) -> Option<FileInfo> {
        let v = self.get_json(&format!("{}.json", Self::file_path(link))).ok()?;
        let name = v.get("file_name").and_then(Value::as_str).map(|n| safe_file_name(n, ""))?;
        let version = v.get("version").and_then(Value::as_str).unwrap_or("").trim().to_string();
        let md5 = v.get("md5").and_then(Value::as_str).unwrap_or("").trim().to_lowercase();
        Some(FileInfo { name, version, md5: Some(md5).filter(|m| m.len() == 32 && m.chars().all(|c| c.is_ascii_hexdigit())) })
    }

    /// Mods (and files outside Nexus) this mod needs, as its author listed them.
    pub fn requirements(&self, game: &str, mod_id: u64) -> Result<Vec<Requirement>, String> {
        if !domain_ok(game) || mod_id == 0 {
            return Err("Unrecognised Nexus mod reference.".into());
        }
        let gql = |query: &str, vars: Value| -> Result<Value, String> {
            let v = self.send("/v2/graphql", Some(&json!({ "query": query, "variables": vars }).to_string()))?;
            if let Some(e) = v.get("errors").and_then(|e| e.get(0)).and_then(|e| e.get("message")).and_then(Value::as_str) {
                return Err(format!("Nexus error: {e}"));
            }
            Ok(v["data"].clone())
        };
        let id_of = |d: &Value| d["game"]["id"].as_u64().or_else(|| d["game"]["id"].as_str().and_then(|s| s.parse().ok()));
        let game_id = id_of(&gql("query($d:String!){ game(domainName:$d){ id } }", json!({ "d": game }))?).ok_or("Nexus does not know that game.")?;
        let data = gql(
            "query($m:ID!,$g:ID!){ mod(modId:$m,gameId:$g){ modRequirements{ nexusRequirements{ nodes{ modId modName gameId url notes externalRequirement } } } } }",
            json!({ "m": mod_id.to_string(), "g": game_id.to_string() }),
        )?;
        let nodes = data["mod"]["modRequirements"]["nexusRequirements"]["nodes"].as_array().cloned().unwrap_or_default();
        let mut domains: std::collections::HashMap<u64, String> = std::collections::HashMap::new();
        domains.insert(game_id, game.to_string());
        let mut out = Vec::new();
        for n in nodes {
            let text = |k: &str| n.get(k).and_then(Value::as_str).unwrap_or("").trim().to_string();
            let num = |k: &str| n.get(k).and_then(Value::as_u64).or_else(|| n.get(k).and_then(Value::as_str).and_then(|s| s.parse().ok())).unwrap_or(0);
            let external = n.get("externalRequirement").and_then(Value::as_bool).unwrap_or(false);
            let gid = num("gameId");
            let domain = if external || gid == 0 {
                String::new()
            } else if let Some(d) = domains.get(&gid) {
                d.clone()
            } else {
                let d = gql("query($i:ID!){ game(id:$i){ domainName } }", json!({ "i": gid.to_string() }))
                    .ok()
                    .and_then(|d| d["game"]["domainName"].as_str().map(str::to_string))
                    .unwrap_or_default();
                domains.insert(gid, d.clone());
                d
            };
            out.push(Requirement { mod_id: num("modId"), game: domain, name: text("modName"), url: text("url"), notes: text("notes"), external });
        }
        Ok(out)
    }

    /// `(domain, name)` of every game Nexus knows.
    pub fn games(&self) -> Result<Vec<(String, String)>, String> {
        let v = self.get_json("/v1/games.json")?;
        Ok(v.as_array()
            .map(|a| a.iter().filter_map(|g| Some((g.get("domain_name")?.as_str()?.to_string(), g.get("name")?.as_str()?.to_string()))).filter(|(d, _)| domain_ok(d)).collect())
            .unwrap_or_default())
    }

    /// Mods of a game whose name matches `query` (Nexus matches word starts), best match first. Adult
    /// mods are left out.
    pub fn search(&self, domain: &str, query: &str, count: usize) -> Result<Vec<Listing>, String> {
        let query: String = query.chars().filter(|c| !c.is_control() && !"\"\\*".contains(*c)).collect::<String>().trim().chars().take(80).collect();
        if !domain_ok(domain) || query.len() < 2 {
            return Err("Type at least two letters to search.".into());
        }
        let q = "query($name:String!,$domain:String!,$count:Int){ mods(filter:{ name:[{value:$name, op:WILDCARD}], gameDomainName:[{value:$domain}] }, sort:[{relevance:{direction:DESC}}], count:$count, offset:0){ nodes{ modId name summary version author endorsements downloads thumbnailUrl adultContent } } }";
        let v = self.send("/v2/graphql", Some(&json!({ "query": q, "variables": { "name": query, "domain": domain, "count": count.clamp(1, 40) } }).to_string()))?;
        if let Some(e) = v.get("errors").and_then(|e| e.get(0)).and_then(|e| e.get("message")).and_then(Value::as_str) {
            return Err(format!("Nexus error: {e}"));
        }
        let s = |m: &Value, k: &str| m.get(k).and_then(Value::as_str).unwrap_or("").to_string();
        Ok(v.pointer("/data/mods/nodes")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter(|m| !m.get("adultContent").and_then(Value::as_bool).unwrap_or(false))
                    .filter_map(|m| {
                        Some(Listing {
                            mod_id: m.get("modId").and_then(|x| x.as_u64().or_else(|| x.as_str().and_then(|t| t.parse().ok())))?,
                            name: s(m, "name"),
                            summary: crate::readme::plain(&s(m, "summary")).chars().take(220).collect(),
                            version: s(m, "version"),
                            author: s(m, "author"),
                            endorsements: m.get("endorsements").and_then(Value::as_u64).unwrap_or(0),
                            picture: s(m, "thumbnailUrl"),
                            downloads: m.get("downloads").and_then(Value::as_u64).unwrap_or(0),
                            updated_ms: crate::fsutil::iso_ms(&s(m, "updatedAt")),
                            size_kb: m.get("fileSize").and_then(Value::as_u64).unwrap_or(0),
                            category: m.pointer("/modCategory/name").and_then(Value::as_str).unwrap_or("").to_string(),
                        })
                    })
                    .collect()
            })
            .unwrap_or_default())
    }

    /// The mods of a game sorted the way a shop window would: most downloaded, most endorsed, newest, recently
    /// changed, or "lately" (most downloaded among mods changed in the last six months). Returns the page and the
    /// total number of matches (0 when Nexus did not say).
    pub fn store(&self, domain: &str, q: &StoreQuery) -> Result<(Vec<Listing>, u64), String> {
        if !domain_ok(domain) {
            return Err("Unrecognised Nexus game.".into());
        }
        let clean = |s: &str, n: usize| -> String { s.chars().filter(|c| !c.is_control() && !"\"\\*".contains(*c)).collect::<String>().trim().chars().take(n).collect() };
        let name = clean(&q.query, 80);
        let category = clean(&q.category, 60);
        let (field, since) = match q.sort.as_str() {
            "endorsed" => ("endorsements", false),
            "new" => ("createdAt", false),
            "updated" => ("updatedAt", false),
            "lately" => ("downloads", true),
            _ => ("downloads", false),
        };
        let mut decl = String::from("$domain:String!,$count:Int,$offset:Int");
        let mut filter = String::from("gameDomainName:[{value:$domain}]");
        let mut vars = json!({ "domain": domain, "count": q.count.clamp(1, 40), "offset": q.offset.min(10_000) });
        if name.len() >= 2 {
            decl.push_str(",$name:String!");
            filter.push_str(", name:[{value:$name, op:WILDCARD}]");
            vars["name"] = json!(name);
        }
        if since {
            decl.push_str(",$since:String!");
            filter.push_str(", updatedAt:[{value:$since, op:GTE}]");
            vars["since"] = json!((crate::fsutil::now_ms() / 1000).saturating_sub(180 * 86_400).to_string());
        }
        if !category.is_empty() {
            decl.push_str(",$cat:String!");
            filter.push_str(", categoryName:[{value:$cat}]");
            vars["cat"] = json!(category);
        }
        if !q.adult {
            filter.push_str(", adultContent:[{value:false}]");
        }
        let sort = if name.len() >= 2 && q.sort.is_empty() { "relevance".to_string() } else { field.to_string() };
        let query = format!("query({decl}){{ mods(filter:{{ {filter} }}, sort:[{{{sort}:{{direction:DESC}}}}], count:$count, offset:$offset){{ nodes{{ modId name summary version author endorsements downloads thumbnailUrl thumbnailLargeUrl adultContent fileSize updatedAt modCategory{{name}} }} totalCount }} }}");
        let v = self.send("/v2/graphql", Some(&json!({ "query": query, "variables": vars }).to_string()))?;
        if let Some(e) = v.get("errors").and_then(|e| e.get(0)).and_then(|e| e.get("message")).and_then(Value::as_str) {
            return Err(format!("Nexus error: {e}"));
        }
        let total = v.pointer("/data/mods/totalCount").and_then(Value::as_u64).unwrap_or(0);
        let s = |m: &Value, k: &str| m.get(k).and_then(Value::as_str).unwrap_or("").to_string();
        let list = v
            .pointer("/data/mods/nodes")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter(|m| q.adult || !m.get("adultContent").and_then(Value::as_bool).unwrap_or(false))
                    .filter_map(|m| {
                        let large = s(m, "thumbnailLargeUrl");
                        Some(Listing {
                            mod_id: m.get("modId").and_then(|x| x.as_u64().or_else(|| x.as_str().and_then(|t| t.parse().ok())))?,
                            name: s(m, "name"),
                            summary: crate::readme::plain(&s(m, "summary")).chars().take(220).collect(),
                            version: s(m, "version"),
                            author: s(m, "author"),
                            endorsements: m.get("endorsements").and_then(Value::as_u64).unwrap_or(0),
                            picture: if large.is_empty() { s(m, "thumbnailUrl") } else { large },
                            downloads: m.get("downloads").and_then(Value::as_u64).unwrap_or(0),
                            updated_ms: crate::fsutil::iso_ms(&s(m, "updatedAt")),
                            size_kb: m.get("fileSize").and_then(Value::as_u64).unwrap_or(0),
                            category: m.pointer("/modCategory/name").and_then(Value::as_str).unwrap_or("").to_string(),
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();
        Ok((list, total))
    }

    /// The category names Nexus uses for a game's mods.
    pub fn categories(&self, domain: &str) -> Vec<String> {
        if !domain_ok(domain) {
            return Vec::new();
        }
        self.get_json(&format!("/v1/games/{domain}.json"))
            .ok()
            .and_then(|v| {
                // Nexus lists the game itself as the first, empty category: it is no use as a filter
                let game = v.get("name").and_then(Value::as_str).unwrap_or("").to_string();
                v.get("categories").and_then(Value::as_array).map(|a| a.iter().filter_map(|c| c.get("name").and_then(Value::as_str).map(str::to_string)).filter(|n| norm(n) != norm(&game)).collect())
            })
            .unwrap_or_default()
    }

    /// One of Nexus's lists of mods for a game: "trending", "latest_added" or "latest_updated".
    pub fn browse(&self, game: &str, list: &str) -> Result<Vec<Listing>, String> {
        if !domain_ok(game) || !["trending", "latest_added", "latest_updated"].contains(&list) {
            return Err("Unrecognised Nexus list.".into());
        }
        let v = self.get_json(&format!("/v1/games/{game}/mods/{list}.json"))?;
        Ok(v.as_array()
            .map(|a| {
                a.iter()
                    .filter(|m| m.get("available").and_then(Value::as_bool).unwrap_or(true) && !m.get("contains_adult_content").and_then(Value::as_bool).unwrap_or(false))
                    .filter_map(|m| {
                        let s = |k: &str| m.get(k).and_then(Value::as_str).unwrap_or("").to_string();
                        Some(Listing {
                            mod_id: m.get("mod_id")?.as_u64()?,
                            name: s("name"),
                            summary: crate::readme::plain(&s("summary")).chars().take(220).collect(),
                            version: s("version"),
                            author: s("author"),
                            endorsements: m.get("endorsement_count").and_then(Value::as_u64).unwrap_or(0),
                            picture: s("picture_url"),
                            downloads: 0,
                            updated_ms: m.get("updated_timestamp").and_then(Value::as_u64).unwrap_or(0) * 1000,
                            size_kb: 0,
                            category: String::new(),
                        })
                    })
                    .collect()
            })
            .unwrap_or_default())
    }

    /// What you endorsed: `(domain, mod id) -> "Endorsed" | "Abstained" | "Undecided"`.
    pub fn endorsements(&self) -> Result<std::collections::HashMap<(String, u64), String>, String> {
        let v = self.get_json("/v1/user/endorsements.json")?;
        Ok(v.as_array()
            .map(|a| a.iter().filter_map(|e| Some(((e.get("domain_name")?.as_str()?.to_string(), e.get("mod_id")?.as_u64()?), e.get("status").and_then(Value::as_str).unwrap_or("Undecided").to_string()))).collect())
            .unwrap_or_default())
    }

    /// Endorse (or take back the endorsement of) a mod. Nexus only allows it for mods you downloaded.
    pub fn endorse(&self, game: &str, mod_id: u64, version: &str, on: bool) -> Result<(), String> {
        if !domain_ok(game) || mod_id == 0 {
            return Err("Unrecognised Nexus mod reference.".into());
        }
        let body = format!("Version={}", crate::fsutil::url_encode(version));
        let what = if on { "endorse" } else { "abstain" };
        self.request("POST", &format!("/v1/games/{game}/mods/{mod_id}/{what}.json"), Some(("application/x-www-form-urlencoded", &body))).map(|_| ()).map_err(|e| {
            if e.starts_with("Nexus refused the download") {
                "Nexus does not let you endorse this mod yet (you have to download it from the website first).".to_string()
            } else {
                e
            }
        })
    }

    /// Mods on your Nexus tracking list as `(domain, mod id)`.
    pub fn tracked(&self) -> Result<Vec<(String, u64)>, String> {
        let v = self.get_json("/v1/user/tracked_mods.json")?;
        Ok(v.as_array().map(|a| a.iter().filter_map(|e| Some((e.get("domain_name")?.as_str()?.to_string(), e.get("mod_id")?.as_u64()?))).collect()).unwrap_or_default())
    }

    pub fn track(&self, game: &str, mod_id: u64, on: bool) -> Result<(), String> {
        if !domain_ok(game) || mod_id == 0 {
            return Err("Unrecognised Nexus mod reference.".into());
        }
        let body = format!("mod_id={mod_id}");
        self.request(if on { "POST" } else { "DELETE" }, &format!("/v1/user/tracked_mods.json?domain_name={game}"), Some(("application/x-www-form-urlencoded", &body))).map(|_| ())
    }

    /// A Nexus collection: its mods and the exact file of each. `revision` None means the newest published one.
    pub fn collection(&self, domain: &str, slug: &str, revision: Option<u32>) -> Result<Collection, String> {
        if !domain_ok(domain) || !collection_slug_ok(slug) {
            return Err("That does not look like a Nexus collection link.".into());
        }
        let latest = "query($slug:String!,$domain:String){ collection(slug:$slug, domainName:$domain, viewAdultContent:false){ latestPublishedRevision{ revisionNumber } } }";
        let rev = match revision {
            Some(r) => r,
            None => {
                let v = self.send("/v2/graphql", Some(&json!({ "query": latest, "variables": { "slug": slug, "domain": domain } }).to_string()))?;
                v.pointer("/data/collection/latestPublishedRevision/revisionNumber").and_then(Value::as_u64).ok_or("Nexus does not know that collection.")? as u32
            }
        };
        let q = "query($slug:String!,$rev:Int,$domain:String){ collectionRevision(slug:$slug, revision:$rev, viewAdultContent:false, domainName:$domain){ revisionNumber modCount collection{ name slug game{ domainName name } } modFiles{ optional fileId file{ fileId name version sizeInBytes mod{ modId name game{ domainName } } } } } }";
        let v = self.send("/v2/graphql", Some(&json!({ "query": q, "variables": { "slug": slug, "rev": rev, "domain": domain } }).to_string()))?;
        if let Some(e) = v.get("errors").and_then(|e| e.get(0)).and_then(|e| e.get("message")).and_then(Value::as_str) {
            return Err(format!("Nexus error: {e}"));
        }
        parse_collection(v.pointer("/data/collectionRevision").unwrap_or(&Value::Null))
    }

    /// The mod's page exactly as Nexus sends it (read by `modinfo`).
    pub fn mod_raw(&self, game: &str, mod_id: u64) -> Result<Value, String> {
        if !domain_ok(game) || mod_id == 0 {
            return Err("Unrecognised Nexus mod reference.".into());
        }
        self.get_json(&format!("/v1/games/{game}/mods/{mod_id}.json"))
    }

    /// What the author wrote on the mod's page.
    pub fn mod_page(&self, game: &str, mod_id: u64) -> Result<ModPage, String> {
        if !domain_ok(game) || mod_id == 0 {
            return Err("Unrecognised Nexus mod reference.".into());
        }
        let v = self.get_json(&format!("/v1/games/{}/mods/{}.json", game, mod_id))?;
        let s = |k: &str| v.get(k).and_then(Value::as_str).unwrap_or("").trim().to_string();
        Ok(ModPage { name: s("name"), summary: s("summary"), description: s("description"), version: s("version") })
    }

    /// Every file of a mod and the author's replacement chain.
    pub fn mod_files(&self, game: &str, mod_id: u64) -> Result<ModFiles, String> {
        if !domain_ok(game) || mod_id == 0 {
            return Err("Unrecognised Nexus mod reference.".into());
        }
        let v = self.get_json(&format!("/v1/games/{}/mods/{}/files.json", game, mod_id))?;
        let s = |f: &Value, k: &str| f.get(k).and_then(Value::as_str).unwrap_or("").trim().to_string();
        let files = v
            .get("files")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(|f| {
                        Some(ModFile {
                            file_id: f.get("file_id").and_then(Value::as_u64)?,
                            name: s(f, "name"),
                            file_name: safe_file_name(&s(f, "file_name"), ""),
                            version: s(f, "version"),
                            category: s(f, "category_name"),
                            uploaded: f.get("uploaded_timestamp").and_then(Value::as_u64).unwrap_or(0),
                            description: s(f, "description"),
                            changelog: s(f, "changelog_html"),
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();
        let updates = v
            .get("file_updates")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(|u| {
                        Some((
                            u.get("old_file_id").and_then(Value::as_u64)?,
                            u.get("new_file_id").and_then(Value::as_u64)?,
                            u.get("uploaded_timestamp").and_then(Value::as_u64).unwrap_or(0),
                        ))
                    })
                    .collect()
            })
            .unwrap_or_default();
        Ok(ModFiles { files, updates })
    }

    /// First CDN address Nexus offers for the file.
    pub fn download_url(&self, link: &NxmLink) -> Result<String, String> {
        let mut q = String::new();
        if let (Some(k), Some(e)) = (&link.key, &link.expires) {
            q = format!("?key={}&expires={}", k, e);
        }
        let v = self.get_json(&format!("{}/download_link.json{}", Self::file_path(link), q))?;
        v.as_array()
            .and_then(|a| a.iter().find_map(|o| o.get("URI").and_then(Value::as_str)))
            .map(str::to_string)
            .ok_or_else(|| "Nexus returned no download address.".to_string())
    }

    /// Download `url` (a CDN address; no API key is sent there) to `dest`.
    pub fn download(&self, url: &str, dest: &Path) -> Result<(), String> {
        self.check_url(url)?;
        crate::downloads::fetch(url, dest, None, "Nexus Mods")
    }
}

#[cfg(test)]
pub mod fake {
    //! A tiny stand-in for the Nexus API and its CDN, for tests.
    use std::sync::{Arc, Mutex};
    use tiny_http::{Response, Server};

    pub const KEY: &str = "AbCdEf0123456789+/AbCdEf0123456789==--tail";

    #[derive(Default)]
    pub struct Seen {
        pub requests: Vec<String>,
        pub apikeys: Vec<Option<String>>,
    }

    pub struct FakeNexus {
        /// Whether `validate` reports a Premium account (and downloads need no website key).
        pub premium: Arc<std::sync::atomic::AtomicBool>,
        /// Body served for `/v1/games/<g>/mods/<id>/files.json`.
        pub files_json: Arc<Mutex<String>>,
        /// Body served for `/v1/games/<g>/mods/<id>.json` (the mod page).
        pub mod_json: Arc<Mutex<String>>,
        /// `nodes` of the GraphQL requirements answer.
        pub requirements_json: Arc<Mutex<String>>,
        /// Bodies for `/v1/games.json` and the browse lists.
        pub games_json: Arc<Mutex<String>>,
        pub browse_json: Arc<Mutex<String>>,
        /// "game:mod id" -> "Endorsed" | "Abstained"
        pub endorsed: Arc<Mutex<std::collections::HashMap<String, String>>>,
        /// "game:mod id" of the tracked mods
        pub tracked: Arc<Mutex<Vec<String>>>,
        /// `collectionRevision` node served for collection requests
        pub collection_json: Arc<Mutex<String>>,
        /// "METHOD url body" of every write the app made
        pub writes: Arc<Mutex<Vec<String>>>,
        /// Make the file info claim a checksum that the download does not have.
        pub bad_md5: Arc<std::sync::atomic::AtomicBool>,
        /// `nodes` served for a name search, and the search requests received
        pub search_json: Arc<Mutex<String>>,
        pub searches: Arc<Mutex<Vec<String>>>,
        pub base: String,
        pub seen: Arc<Mutex<Seen>>,
        pub payload: Vec<u8>,
    }

    /// Serves: validate, file info, download_link and the CDN file. `premium_only` makes
    /// download_link answer 403 unless the link carried key/expires.
    pub fn start(file_name: &str, payload: Vec<u8>, premium_only: bool) -> FakeNexus {
        let server = Server::http("127.0.0.1:0").unwrap();
        let base = format!("http://{}", server.server_addr().to_ip().unwrap());
        let seen = Arc::new(Mutex::new(Seen::default()));
        let files_json = Arc::new(Mutex::new(r#"{"files":[],"file_updates":[]}"#.to_string()));
        let requirements_json = Arc::new(Mutex::new("[]".to_string()));
        let games_json = Arc::new(Mutex::new("[]".to_string()));
        let browse_json = Arc::new(Mutex::new("[]".to_string()));
        let mod_json = Arc::new(Mutex::new(r#"{"name":"Cool Pack","summary":"","description":"","version":"1.0"}"#.to_string()));
        let premium = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let endorsed: Arc<Mutex<std::collections::HashMap<String, String>>> = Arc::default();
        let tracked: Arc<Mutex<Vec<String>>> = Arc::default();
        let collection_json = Arc::new(Mutex::new("null".to_string()));
        let writes: Arc<Mutex<Vec<String>>> = Arc::default();
        let bad_md5 = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (bm2, md5_good) = (bad_md5.clone(), crate::hashing::md5_hex(&mut &payload[..]).unwrap_or_default());
        let search_json = Arc::new(Mutex::new("[]".to_string()));
        let searches: Arc<Mutex<Vec<String>>> = Arc::default();
        let (sj2, sq2) = (search_json.clone(), searches.clone());
        let (en2, tr2, cj2, wr2) = (endorsed.clone(), tracked.clone(), collection_json.clone(), writes.clone());
        let (s2, b2, name, data, fj, pr, mj, rj, gj, bj) = (seen.clone(), base.clone(), file_name.to_string(), payload.clone(), files_json.clone(), premium.clone(), mod_json.clone(), requirements_json.clone(), games_json.clone(), browse_json.clone());
        std::thread::spawn(move || {
            for mut req in server.incoming_requests() {
                let url = req.url().to_string();
                let apikey = req.headers().iter().find(|h| h.field.equiv("apikey")).map(|h| h.value.as_str().to_string());
                {
                    let mut s = s2.lock().unwrap();
                    s.requests.push(url.clone());
                    s.apikeys.push(apikey.clone());
                }
                let mut sink = Vec::new();
                let _ = req.as_reader().read_to_end(&mut sink);
                let json = |code: u16, body: String| Response::from_string(body).with_status_code(code);
                if url.starts_with("/cdn/") {
                    let _ = req.respond(Response::from_data(data.clone()));
                    continue;
                }
                if apikey.as_deref() != Some(KEY) {
                    let _ = req.respond(json(401, r#"{"message":"Please provide a valid API Key"}"#.into()));
                    continue;
                }
                let method = req.method().to_string();
                let sent = String::from_utf8_lossy(&sink).to_string();
                if method != "GET" && !url.starts_with("/v2/graphql") {
                    wr2.lock().unwrap().push(format!("{method} {url} {sent}"));
                }
                let key_of = |u: &str| {
                    let game = u.split("/games/").nth(1).and_then(|r| r.split('/').next()).unwrap_or("");
                    let id = u.split("/mods/").nth(1).and_then(|r| r.split('/').next()).unwrap_or("");
                    format!("{game}:{id}")
                };
                let resp = if url.starts_with("/v1/user/endorsements.json") {
                    let list: Vec<String> = en2.lock().unwrap().iter().map(|(k, v)| {
                        let (g, i) = k.split_once(':').unwrap_or(("", "0"));
                        format!(r#"{{"mod_id":{i},"domain_name":"{g}","date":1,"version":"1","status":"{v}"}}"#)
                    }).collect();
                    json(200, format!("[{}]", list.join(",")))
                } else if url.contains("/endorse.json") || url.contains("/abstain.json") {
                    let status = if url.contains("/endorse.json") { "Endorsed" } else { "Abstained" };
                    en2.lock().unwrap().insert(key_of(&url), status.into());
                    json(200, format!(r#"{{"message":"ok","status":"{status}"}}"#))
                } else if url.starts_with("/v1/user/tracked_mods.json") {
                    let game = url.split("domain_name=").nth(1).unwrap_or("").to_string();
                    let id = sent.trim_start_matches("mod_id=").to_string();
                    match method.as_str() {
                        "POST" => tr2.lock().unwrap().push(format!("{game}:{id}")),
                        "DELETE" => tr2.lock().unwrap().retain(|k| *k != format!("{game}:{id}")),
                        _ => {}
                    }
                    let list: Vec<String> = tr2.lock().unwrap().iter().map(|k| {
                        let (g, i) = k.split_once(':').unwrap_or(("", "0"));
                        format!(r#"{{"mod_id":{i},"domain_name":"{g}"}}"#)
                    }).collect();
                    if method == "GET" { json(200, format!("[{}]", list.join(","))) } else { json(200, r#"{"message":"ok"}"#.into()) }
                } else if url.starts_with("/v2/graphql") {
                    let q = String::from_utf8_lossy(&sink).to_string();
                    if q.contains("mods(filter") {
                        sq2.lock().unwrap().push(q.clone());
                        json(200, format!(r#"{{"data":{{"mods":{{"nodes":{}}}}}}}"#, sj2.lock().unwrap()))
                    } else if q.contains("latestPublishedRevision") {
                        json(200, r#"{"data":{"collection":{"latestPublishedRevision":{"revisionNumber":3}}}}"#.into())
                    } else if q.contains("collectionRevision(") {
                        json(200, format!(r#"{{"data":{{"collectionRevision":{}}}}}"#, cj2.lock().unwrap()))
                    } else if q.contains("game(domainName") {
                        json(200, r#"{"data":{"game":{"id":1}}}"#.into())
                    } else if q.contains("game(id") {
                        json(200, r#"{"data":{"game":{"domainName":"coolgame"}}}"#.into())
                    } else {
                        json(200, format!(r#"{{"data":{{"mod":{{"modRequirements":{{"nexusRequirements":{{"nodes":{}}}}}}}}}}}"#, rj.lock().unwrap()))
                    }
                } else if url.starts_with("/v1/games.json") {
                    json(200, gj.lock().unwrap().clone())
                } else if url.contains("/mods/trending.json") || url.contains("/mods/latest_added.json") || url.contains("/mods/latest_updated.json") {
                    json(200, bj.lock().unwrap().clone())
                } else if url.starts_with("/v1/users/validate.json") {
                    let p = pr.load(std::sync::atomic::Ordering::SeqCst);
                    json(200, format!(r#"{{"name":"tester","is_premium":{p},"user_id":1}}"#))
                } else if url.contains("/download_link.json") {
                    if premium_only && !pr.load(std::sync::atomic::Ordering::SeqCst) && !url.contains("key=") {
                        json(403, r#"{"message":"You need to be a premium member to download via the API"}"#.into())
                    } else {
                        json(200, format!(r#"[{{"name":"Nexus CDN","short_name":"Nexus CDN","URI":"{}/cdn/{}"}}]"#, b2, name.replace(' ', "%20")))
                    }
                } else if url.contains("/mods/") && url.split('?').next().unwrap_or("").ends_with(".json") && !url.contains("/files") && !url.contains("download_link") {
                    json(200, mj.lock().unwrap().clone())
                } else if url.contains("/mods/") && url.split('?').next().unwrap_or("").ends_with("/files.json") {
                    json(200, fj.lock().unwrap().clone())
                } else if url.contains("/files/") && url.contains(".json") {
                    // version of that file as listed in files.json (1.0 when it is not listed)
                    let id: u64 = url.split("/files/").nth(1).and_then(|r| r.split(|c: char| !c.is_ascii_digit()).next()).and_then(|d| d.parse().ok()).unwrap_or(0);
                    let version = serde_json::from_str::<serde_json::Value>(&fj.lock().unwrap())
                        .ok()
                        .and_then(|v| v["files"].as_array().and_then(|a| a.iter().find(|f| f["file_id"] == id).map(|f| f["version"].as_str().unwrap_or("1.0").to_string())))
                        .unwrap_or_else(|| "1.0".to_string());
                    let md5 = if bm2.load(std::sync::atomic::Ordering::SeqCst) { "0".repeat(32) } else { md5_good.clone() };
                    json(200, format!(r#"{{"file_name":"{}","file_id":{},"version":"{}","md5":"{}"}}"#, name, id, version, md5))
                } else {
                    json(404, r#"{"message":"not found"}"#.into())
                };
                let _ = req.respond(resp);
            }
        });
        FakeNexus { premium, files_json, mod_json, requirements_json, games_json, browse_json, endorsed, tracked, collection_json, writes, search_json, searches, bad_md5, base, seen, payload }
    }
}

#[cfg(test)]
mod tests {
    use super::fake::*;
    use super::*;

    #[test]
    fn both_naming_styles_of_nexus_downloads_are_read() {
        let n = parse_download_name("Appearance Menu Mod 790 2.14 2026-10-21T20-49Z psSjsvvdX.zip").unwrap();
        assert_eq!((n.name.as_str(), n.mod_id, n.version.as_str(), n.modern), ("Appearance Menu Mod", 790, "2.14", true));
        assert_eq!(n.uploaded, Some(Uploaded::Minute(1_792_615_740)));
        let c = parse_download_name("CET 1.38.0 - Game version 2.5 107 1.38.0 2026-10-22T12-57Z Q6xF6lly7.zip").unwrap();
        assert_eq!((c.name.as_str(), c.mod_id, c.version.as_str(), c.uploaded), ("CET 1.38.0 - Game version 2.5", 107, "1.38.0", Some(Uploaded::Minute(1_792_673_820))));
        let b = parse_download_name("EasyTrainer 23227 beta2.1 2026-08-14T14-49Z k8Iw8mS76.zip").unwrap();
        assert_eq!((b.name.as_str(), b.mod_id, b.version.as_str()), ("EasyTrainer", 23227, "beta2.1"));
        let o = parse_download_name("Backpack Search-14264-1-2-1-1752387587.zip").unwrap();
        assert_eq!((o.name.as_str(), o.mod_id, o.version.as_str(), o.uploaded, o.modern), ("Backpack Search", 14264, "1.2.1", Some(Uploaded::Second(1_752_387_587)), false));
        let u = parse_download_name("vehiclehandling_v1.3-210-v1-3-1726943022.7z").unwrap();
        assert_eq!((u.name.as_str(), u.mod_id), ("vehiclehandling v1.3", 210));
        for no in ["notes.zip", "My Mod 3.zip", "Cool Mod 2026-10-21T20-49Z abc.zip", "Mod 5 1.0 2026-13-40T25-99Z x.zip"] {
            let r = parse_download_name(no);
            assert!(r.is_none() || r.as_ref().is_some_and(|d| d.uploaded.is_none()), "{no}: {r:?}");
        }
        assert_eq!(epoch_of(1970, 1, 1, 0, 0), 0);
        assert_eq!(epoch_of(2000, 3, 1, 0, 0), 951_868_800);
    }

    #[test]
    fn the_file_a_download_came_from_is_found_by_upload_time_then_by_version() {
        let f = |file_id, version: &str, uploaded| ModFile { file_id, name: String::new(), file_name: String::new(), version: version.into(), category: "MAIN".into(), uploaded, description: String::new(), changelog: String::new() };
        let mf = ModFiles { files: vec![f(1, "1.0", 1_000_000), f(2, "1.1", 1_792_615_765), f(3, "2.0", 1_792_700_000)], updates: vec![] };
        let by_minute = parse_download_name("X 5 1.1 2026-10-21T20-49Z abc.zip").unwrap();
        assert_eq!(mf.find_download(&by_minute).map(|x| x.file_id), Some(2), "the same minute as the upload");
        let by_second = parse_download_name("X-5-1-0-1000000.zip").unwrap();
        assert_eq!(mf.find_download(&by_second).map(|x| x.file_id), Some(1));
        let by_version = parse_download_name("X 5 2.0 2026-01-01T00-00Z abc.zip").unwrap();
        assert_eq!(mf.find_download(&by_version).map(|x| x.file_id), Some(3), "time says nothing, the version is unique");
        let nothing = parse_download_name("X 5 9.9 2026-01-01T00-00Z abc.zip").unwrap();
        assert!(mf.find_download(&nothing).is_none());
        // two files with one version and no time to tell them apart: no guess
        let twins = ModFiles { files: vec![f(1, "1.0", 5), f(2, "1.0", 6)], updates: vec![] };
        assert!(twins.find_download(&parse_download_name("X 5 1.0 2026-01-01T00-00Z abc.zip").unwrap()).is_none());
    }
    use crate::fsutil::testutil::TempDir;

    #[test]
    fn a_games_nexus_domain_is_found_by_name() {
        let g = |d: &str, n: &str| (d.to_string(), n.to_string());
        let list = vec![g("subnautica", "Subnautica"), g("needforspeedmostwanted2005", "Need for Speed: Most Wanted (2005)"), g("needforspeedmostwanted2012", "Need for Speed: Most Wanted (2012)"), g("skyrimspecialedition", "Skyrim Special Edition")];
        assert_eq!(match_domain("Subnautica", &list).as_deref(), Some("subnautica"));
        assert_eq!(match_domain("Need for Speed™ Most Wanted", &list).as_deref(), Some("needforspeedmostwanted2012"), "the newest of the same name");
        assert_eq!(match_domain("Skyrim Special Edition", &list).as_deref(), Some("skyrimspecialedition"));
        assert_eq!(match_domain("Among Us", &list), None);
        assert_eq!(match_domain("", &list), None);
    }

    #[test]
    fn mods_are_searched_by_name_and_adult_ones_are_left_out() {
        let f = start("x.zip", vec![1], false);
        *f.search_json.lock().unwrap() = r#"[{"modId":"83","name":"Ferrari SF-25","summary":"Fast <b>car</b>","version":"V1.0","author":"a","endorsements":15,"downloads":9,"thumbnailUrl":"https://staticdelivery.nexusmods.com/t.png","adultContent":false},{"modId":84,"name":"Spicy","summary":"","adultContent":true},{"name":"No id"}]"#.into();
        let nx = Nexus::new(&f.base, KEY, true).unwrap();
        let r = nx.search("needforspeedmostwanted2012", "  fer\"r*  ", 10).unwrap();
        assert_eq!(r.len(), 1, "{r:?}");
        assert_eq!((r[0].mod_id, r[0].name.as_str(), r[0].summary.as_str(), r[0].endorsements), (83, "Ferrari SF-25", "Fast car", 15));
        let sent = f.searches.lock().unwrap()[0].clone();
        assert!(sent.contains("\"name\":\"ferr\"") || sent.contains("\"name\":\"fer"), "quotes and wildcards are stripped from what is sent: {sent}");
        assert!(!sent.contains('*'));
        assert!(nx.search("needforspeedmostwanted2012", "a", 10).is_err(), "one letter is not a search");
        assert!(nx.search("../x", "ferrari", 10).is_err());
    }

    #[test]
    fn collection_links_are_understood_in_every_form() {
        let want = Some(("skyrimspecialedition".to_string(), "rqhcxy".to_string(), None));
        assert_eq!(parse_collection_link("https://www.nexusmods.com/games/skyrimspecialedition/collections/rqhcxy"), want);
        assert_eq!(parse_collection_link("https://www.nexusmods.com/games/skyrimspecialedition/collections/rqhcxy/mods?tab=x"), want);
        assert_eq!(parse_collection_link("https://next.nexusmods.com/skyrimspecialedition/collections/rqhcxy"), want);
        assert_eq!(parse_collection_link("nxm://skyrimspecialedition/collections/rqhcxy/revisions/4"), Some(("skyrimspecialedition".into(), "rqhcxy".into(), Some(4))));
        for bad in ["", "https://evil.example/games/a/collections/b", "https://www.nexusmods.com/games/a/mods/5", "https://www.nexusmods.com/games/a b/collections/x", "nxm://a/collections/", "nxm://a/collections/x y", "https://www.nexusmods.com/games/../collections/x"] {
            assert_eq!(parse_collection_link(bad), None, "{bad}");
        }
    }

    #[test]
    fn a_collection_is_read_with_its_exact_files() {
        let f = start("x.zip", vec![1], false);
        *f.collection_json.lock().unwrap() = r#"{"revisionNumber":3,"modCount":2,"collection":{"name":"Great Cities","slug":"rqhcxy","game":{"domainName":"skyrimspecialedition","name":"Skyrim Special Edition"}},
            "modFiles":[{"optional":false,"fileId":232153,"file":{"fileId":232153,"name":"Karthwasten","version":"2.1","sizeInBytes":"3609261","mod":{"modId":37471,"name":"Karthwasten Patch","game":{"domainName":"skyrimspecialedition"}}}},
                        {"optional":true,"fileId":9,"file":{"fileId":9,"name":"Extra","version":"1.0","sizeInBytes":500,"mod":{"modId":5,"name":"Extra Bits","game":{"domainName":"skyrimspecialedition"}}}}]}"#.into();
        let nx = Nexus::new(&f.base, KEY, true).unwrap();
        let c = nx.collection("skyrimspecialedition", "rqhcxy", None).unwrap();
        assert_eq!((c.name.as_str(), c.revision, c.domain.as_str(), c.game_name.as_str()), ("Great Cities", 3, "skyrimspecialedition", "Skyrim Special Edition"));
        assert_eq!(c.mods.len(), 2);
        assert_eq!((c.mods[0].mod_id, c.mods[0].file_id, c.mods[0].name.as_str(), c.mods[0].size, c.mods[0].optional), (37471, 232153, "Karthwasten Patch", 3_609_261, false));
        assert!(c.mods[1].optional);
        assert!(nx.collection("skyrimspecialedition", "bad slug!", None).is_err());
        *f.collection_json.lock().unwrap() = "null".into();
        assert!(nx.collection("skyrimspecialedition", "rqhcxy", Some(1)).is_err(), "an unknown collection is an error, not an empty list");
    }

    #[test]
    fn endorsing_and_tracking_talk_to_the_right_places() {
        let f = start("x.zip", vec![1], false);
        let nx = Nexus::new(&f.base, KEY, true).unwrap();
        assert!(nx.endorsements().unwrap().is_empty() && nx.tracked().unwrap().is_empty());
        nx.endorse("coolgame", 5, "1.2 beta", true).unwrap();
        assert_eq!(nx.endorsements().unwrap().get(&("coolgame".to_string(), 5)).map(String::as_str), Some("Endorsed"));
        nx.endorse("coolgame", 5, "1.2 beta", false).unwrap();
        assert_eq!(nx.endorsements().unwrap().get(&("coolgame".to_string(), 5)).map(String::as_str), Some("Abstained"));
        nx.track("coolgame", 5, true).unwrap();
        nx.track("coolgame", 6, true).unwrap();
        nx.track("coolgame", 5, false).unwrap();
        assert_eq!(nx.tracked().unwrap(), vec![("coolgame".to_string(), 6)]);
        let w = f.writes.lock().unwrap().clone();
        assert!(w[0].starts_with("POST /v1/games/coolgame/mods/5/endorse.json Version=1.2%20beta"), "{w:?}");
        assert!(w.iter().any(|x| x.starts_with("DELETE /v1/user/tracked_mods.json?domain_name=coolgame mod_id=5")), "{w:?}");
        assert!(nx.endorse("../x", 5, "1", true).is_err() && nx.track("coolgame", 0, true).is_err());
    }

    #[test]
    fn parses_links_from_the_website() {
        let l = parse_nxm("nxm://halfsword/mods/123/files/456?key=AbC-_%2B9&expires=1700000000&user_id=7").unwrap();
        assert_eq!((l.game.as_str(), l.mod_id, l.file_id), ("halfsword", 123, 456));
        assert_eq!(l.key.as_deref(), Some("AbC-_%2B9"));
        assert_eq!(l.expires.as_deref(), Some("1700000000"));
        let p = parse_nxm("NXM://skyrimspecialedition/mods/1/files/2/").unwrap();
        assert!(p.key.is_none() && p.expires.is_none());
    }

    #[test]
    fn rejects_anything_that_could_steer_the_request_elsewhere() {
        for bad in [
            "http://halfsword/mods/1/files/2",
            "nxm://halfsword/mods/1",
            "nxm://halfsword/mods/1/files/2/extra",
            "nxm://halfsword/mods/x/files/2",
            "nxm://halfsword/mods/1/files/-2",
            "nxm://half sword/mods/1/files/2",
            "nxm://../mods/1/files/2",
            "nxm://a.b/mods/1/files/2",
            "nxm:///mods/1/files/2",
            "nxm://g/mods/99999999999999999999999/files/2",
            "",
        ] {
            assert!(parse_nxm(bad).is_err(), "{bad}");
        }
        // hostile query values are dropped rather than forwarded
        let l = parse_nxm("nxm://g/mods/1/files/2?key=a%0d%0aHost:evil&expires=12x&key2=z").unwrap();
        assert!(l.key.is_none() || !l.key.as_ref().unwrap().contains('\n'));
        assert!(l.expires.is_none());
    }

    #[test]
    fn api_key_and_file_name_hygiene() {
        assert!(valid_api_key(KEY));
        for bad in ["", "short", "has space in the key 0123456789", "quote\"injection0123456789", "new\nline0123456789abcdef"] {
            assert!(!valid_api_key(bad), "{bad:?}");
        }
        assert_eq!(safe_file_name("../../etc/passwd", "f.zip"), "passwd");
        assert_eq!(safe_file_name("Cool Mod-12-1.zip", "f"), "Cool Mod-12-1.zip");
        assert_eq!(safe_file_name("..\\..\\evil.dll", "f"), "evil.dll");
        assert_eq!(safe_file_name(".hidden", "f"), "hidden");
        assert_eq!(safe_file_name("", "fallback.zip"), "fallback.zip");
        assert_eq!(safe_file_name("\n\t", "fb"), "fb");
        assert!(Nexus::new("https://x", "bad key", false).is_err());
    }

    #[test]
    fn talks_to_the_api_validates_downloads_and_forwards_free_account_keys() {
        let f = start("Cool Hat-1-0.pak", vec![7u8; 2048], false);
        let n = Nexus::new(&f.base, KEY, true).unwrap();
        let u = n.validate().unwrap();
        assert_eq!((u.name.as_str(), u.premium), ("tester", false));

        let link = parse_nxm("nxm://halfsword/mods/12/files/34?key=KeyPart-1&expires=1700000000").unwrap();
        assert_eq!(n.file_name(&link, "fallback.zip"), "Cool Hat-1-0.pak");
        let url = n.download_url(&link).unwrap();
        assert!(url.ends_with("/cdn/Cool%20Hat-1-0.pak"), "{url}");

        let d = TempDir::new("nx");
        let dest = d.join("got.pak");
        n.download(&url, &dest).unwrap();
        assert_eq!(std::fs::read(&dest).unwrap(), f.payload);

        let seen = f.seen.lock().unwrap();
        let dl = seen.requests.iter().find(|r| r.contains("download_link")).unwrap();
        assert!(dl.contains("/v1/games/halfsword/mods/12/files/34/download_link.json?key=KeyPart-1&expires=1700000000"), "{dl}");
        assert!(seen.apikeys.iter().zip(&seen.requests).filter(|(_, r)| r.starts_with("/v1")).all(|(k, _)| k.as_deref() == Some(KEY)));
        assert!(seen.apikeys.iter().zip(&seen.requests).filter(|(_, r)| r.starts_with("/cdn")).all(|(k, _)| k.is_none()), "the key is never sent to the CDN");
    }

    fn files_doc() -> &'static str {
        r#"{"files":[
            {"file_id":10,"name":"Main","file_name":"Cool-1-0.zip","version":"1.0","category_name":"MAIN","uploaded_timestamp":100},
            {"file_id":11,"name":"Main","file_name":"Cool-1-1.zip","version":"1.1","category_name":"MAIN","uploaded_timestamp":200},
            {"file_id":12,"name":"Main","file_name":"Cool-2-0.zip","version":"2.0","category_name":"MAIN","uploaded_timestamp":300},
            {"file_id":20,"name":"Optional","file_name":"Extras.zip","version":"1.0","category_name":"OPTIONAL","uploaded_timestamp":150},
            {"file_id":30,"name":"Other main","file_name":"Other.zip","version":"9.9","category_name":"MAIN","uploaded_timestamp":900}],
          "file_updates":[
            {"old_file_id":10,"new_file_id":11,"uploaded_timestamp":200},
            {"old_file_id":11,"new_file_id":12,"uploaded_timestamp":300},
            {"old_file_id":12,"new_file_id":10,"uploaded_timestamp":400}]}"#
    }

    #[test]
    fn updates_follow_the_authors_replacement_chain_and_nothing_else() {
        let f = start("m.zip", vec![1], false);
        *f.files_json.lock().unwrap() = files_doc().to_string();
        let n = Nexus::new(&f.base, KEY, true).unwrap();
        let mf = n.mod_files("halfsword", 5).unwrap();
        assert_eq!(mf.files.len(), 5);
        assert_eq!(mf.newer_than(10).map(|x| (x.file_id, x.version.as_str())), Some((12, "2.0")), "10 → 11 → 12, the cycle back to 10 is ignored");
        assert_eq!(mf.newer_than(11).map(|x| x.file_id), Some(12));
        assert!(mf.newer_than(20).is_none(), "an optional file is not an update of anything");
        assert!(mf.newer_than(30).is_none(), "another main file is not an update either");
        assert!(mf.newer_than(999).is_none());
        // the request is for exactly this mod and carries the key
        let seen = f.seen.lock().unwrap();
        assert!(seen.requests.iter().any(|r| r == "/v1/games/halfsword/mods/5/files.json"));
        drop(seen);
        assert!(n.mod_files("bad game", 5).is_err() && n.mod_files("g", 0).is_err() && n.mod_files("../x", 1).is_err());
        let r = NexusRef { game: "halfsword".into(), mod_id: 5, file_id: 10 };
        assert!(r.valid() && !NexusRef { game: "a b".into(), ..r.clone() }.valid());
        assert_eq!(r.files_page(12), "https://www.nexusmods.com/halfsword/mods/5?tab=files&file_id=12&nmm=1");
    }

    #[test]
    fn file_info_reports_name_and_version() {
        let f = start("Cool Hat-1-0.pak", vec![1], false);
        let n = Nexus::new(&f.base, KEY, true).unwrap();
        let link = parse_nxm("nxm://g/mods/1/files/2").unwrap();
        let info = n.file_info(&link).unwrap();
        assert_eq!((info.name.as_str(), info.version.as_str()), ("Cool Hat-1-0.pak", "1.0"));
        assert_eq!(info.md5.as_deref().map(str::len), Some(32), "the checksum comes with the file info");
    }

    #[test]
    fn errors_are_explained_in_plain_words() {
        let f = start("m.zip", vec![1], true);
        let bad = Nexus::new(&f.base, "ZZZZZZZZZZZZZZZZZZZZZZZZZZZZ", true).unwrap();
        assert!(bad.validate().unwrap_err().contains("rejected the API key"));
        let n = Nexus::new(&f.base, KEY, true).unwrap();
        let link = parse_nxm("nxm://g/mods/1/files/2").unwrap();
        let e = n.download_url(&link).unwrap_err();
        assert!(e.contains("Mod Manager Download") && e.contains("premium"), "{e}");
        // a dead server
        let dead = Nexus::new("http://127.0.0.1:1", KEY, true).unwrap();
        assert!(dead.validate().unwrap_err().contains("Could not reach"));
        // plain http is refused in production configuration
        assert!(Nexus::new("http://api.example", KEY, false).unwrap().validate().unwrap_err().contains("HTTPS"));
        assert!(n.download("ftp://x/y", Path::new("/tmp/never")).is_err());
        let d = TempDir::new("nx2");
        assert!(n.download(&format!("{}/missing", f.base), &d.join("o")).is_err());
        assert!(!d.join("o").exists(), "no partial file is left behind");
    }
}
