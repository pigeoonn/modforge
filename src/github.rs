//! Mods published as GitHub releases: paste a repository link, pick a file of its newest
//! release, and it is installed like any other mod. Downloads are only taken from the
//! repository's own release area.

use serde_json::Value;
use std::process::{Command, Stdio};

pub const API: &str = "https://api.github.com";

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct Asset {
    pub name: String,
    pub url: String,
    pub size: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct Release {
    pub owner: String,
    pub repo: String,
    pub tag: String,
    pub title: String,
    pub assets: Vec<Asset>,
}

fn part_ok(s: &str) -> bool {
    !s.is_empty() && s.len() <= 100 && s.chars().all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c)) && !s.starts_with('.')
}

/// `owner/repo` out of a link or the short form.
pub fn parse_repo(input: &str) -> Option<(String, String)> {
    // a link copied from a page often ends in "#readme" or "?tab=readme-ov-file"
    let t = input.trim().split(['#', '?']).next().unwrap_or("").trim_end_matches('/');
    let t = t.strip_prefix("https://").or_else(|| t.strip_prefix("http://")).unwrap_or(t);
    let t = t.strip_prefix("www.").unwrap_or(t);
    let path = match t.strip_prefix("github.com/") {
        Some(p) => p,
        None if !t.contains('.') || t.matches('/').count() == 1 && !t.contains("://") && !t.split('/').next()?.contains('.') => t,
        None => return None,
    };
    let mut it = path.split('/');
    let (o, r) = (it.next()?, it.next()?);
    let r = r.strip_suffix(".git").unwrap_or(r);
    (part_ok(o) && part_ok(r)).then(|| (o.to_string(), r.to_string()))
}

/// Where a mod came from on GitHub: the repository, the release it was taken from and the file of that release.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct GithubRef {
    pub owner: String,
    pub repo: String,
    pub tag: String,
    pub asset: String,
}

impl GithubRef {
    /// The reference a download address stands for (`https://github.com/<owner>/<repo>/releases/download/<tag>/<file>`).
    pub fn from_download(url: &str) -> Option<GithubRef> {
        let rest = url.strip_prefix("https://github.com/")?;
        let mut it = rest.split('/');
        let (owner, repo, releases, download, tag, asset) = (it.next()?, it.next()?, it.next()?, it.next()?, it.next()?, it.next()?);
        if releases != "releases" || download != "download" || it.next().is_some() || !part_ok(owner) || !part_ok(repo) || tag.is_empty() || asset.is_empty() {
            return None;
        }
        Some(GithubRef { owner: owner.into(), repo: repo.into(), tag: tag.into(), asset: asset.into() })
    }

    /// Is this the same file of the same repository (whatever the release)?
    pub fn same_file_as(&self, other: &GithubRef) -> bool {
        self.owner.eq_ignore_ascii_case(&other.owner) && self.repo.eq_ignore_ascii_case(&other.repo) && shape(&self.asset) == shape(&other.asset)
    }

    /// `owner/repo`.
    pub fn repo_name(&self) -> String {
        format!("{}/{}", self.owner, self.repo)
    }
}

/// A file name with its version taken out, to tell "Cool-1.2.zip" and "Cool-1.3.zip" are the same file of a new release.
fn shape(name: &str) -> String {
    let lower = name.to_lowercase();
    let mut out = String::new();
    let mut chars = lower.chars().peekable();
    while let Some(c) = chars.next() {
        if c.is_ascii_digit() {
            while chars.peek().map(|n| n.is_ascii_digit() || *n == '.').unwrap_or(false) {
                chars.next();
            }
            out.push('#');
        } else if c.is_alphanumeric() || c == '.' {
            out.push(c);
        } else if !out.ends_with('_') {
            out.push('_');
        }
    }
    // a "v" in front of a version ("v1.2") is part of the version
    out.replace("_v#", "_#").replace("v#", "#").replace("__", "_").trim_matches('_').to_string()
}

/// The file of a newer release that stands for `old` (the file the mod was installed from): the one with the same shape
/// of name, or the only mod file the release has. `None` when it cannot be told.
pub fn matching_asset(old: &str, assets: &[Asset]) -> Option<Asset> {
    let files = mod_files(assets);
    let want = shape(old);
    let same: Vec<&Asset> = files.iter().filter(|a| shape(&a.name) == want).collect();
    match (same.as_slice(), files.as_slice()) {
        ([one], _) => Some((*one).clone()),
        ([], [only]) => Some(only.clone()),
        _ => None,
    }
}

/// Files of a release that are mods or contain mods.
pub fn mod_files(assets: &[Asset]) -> Vec<Asset> {
    let ok = |n: &str| {
        let l = n.to_lowercase();
        [".zip", ".7z", ".rar", ".dll", ".pak"].iter().any(|e| l.ends_with(e)) && !l.contains("source") && !l.contains("linux") && !l.contains("macos") && !l.contains("-osx")
    };
    assets.iter().filter(|a| ok(&a.name)).cloned().collect()
}

pub fn parse_release(v: &Value, owner: &str, repo: &str, allowed: Option<&str>) -> Option<Release> {
    let prefix = allowed.map(str::to_string).unwrap_or_else(|| format!("https://github.com/{owner}/{repo}/releases/download/"));
    let assets = v
        .get("assets")?
        .as_array()?
        .iter()
        .filter_map(|a| {
            let url = a.get("browser_download_url")?.as_str()?.to_string();
            let name = a.get("name")?.as_str()?.to_string();
            (url.starts_with(&prefix) && !name.contains('/') && !name.contains("..")).then(|| Asset { name, url, size: a.get("size").and_then(Value::as_u64).unwrap_or(0) })
        })
        .collect();
    Some(Release { owner: owner.into(), repo: repo.into(), tag: v.get("tag_name").and_then(Value::as_str).unwrap_or("").to_string(), title: v.get("name").and_then(Value::as_str).unwrap_or("").to_string(), assets })
}

/// Is `url` a download from the release area of `owner/repo`?
pub fn download_ok(url: &str, owner: &str, repo: &str, allow_prefix: Option<&str>) -> bool {
    let std_prefix = format!("https://github.com/{owner}/{repo}/releases/download/");
    url.starts_with(&std_prefix) || allow_prefix.map(|p| url.starts_with(p)).unwrap_or(false)
}

/// What GitHub's own error text means for the person, if it is an error reply.
fn explain(v: &Value) -> Option<String> {
    let m = v.get("message")?.as_str()?.to_lowercase();
    if m.contains("rate limit") {
        Some("GitHub is limiting requests from this connection; try again in a while.".into())
    } else if m.contains("not found") {
        Some("GitHub does not know that repository (it may be private or the name is wrong).".into())
    } else {
        None
    }
}

fn get(url: &str) -> Result<Value, String> {
    let out = Command::new("curl")
        .args(["-sSL", "--max-time", "30", "-H", "Accept: application/vnd.github+json", "-H", "User-Agent: ModForge"])
        .arg(url)
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("Could not run curl ({e})."))?;
    serde_json::from_slice(&out.stdout).map_err(|_| "Could not reach GitHub.".to_string())
}

/// The newest release of a repository (pre-releases count when there is no full release).
pub fn latest(owner: &str, repo: &str, api: &str, allowed: Option<&str>) -> Result<Release, String> {
    let v = get(&format!("{api}/repos/{owner}/{repo}/releases/latest"))?;
    if v.get("tag_name").is_none() {
        // "no latest release" is a 404 too, so only a rate limit ends it here
        if let Some(e) = explain(&v).filter(|e| e.contains("limiting")) {
            return Err(e);
        }
        let list = get(&format!("{api}/repos/{owner}/{repo}/releases"))?;
        if let Some(e) = explain(&list) {
            return Err(e);
        }
        let first = list.as_array().and_then(|a| a.first()).cloned();
        return first.and_then(|f| parse_release(&f, owner, repo, allowed)).ok_or_else(|| "That repository has no releases.".to_string());
    }
    parse_release(&v, owner, repo, allowed).ok_or_else(|| "That repository has no releases.".to_string())
}

/// The release with that tag, or the newest one when it is gone (an old release can be deleted).
pub fn release_or_latest(owner: &str, repo: &str, tag: &str, api: &str, allowed: Option<&str>) -> Result<Release, String> {
    if part_ok(tag) {
        if let Some(r) = get(&format!("{api}/repos/{owner}/{repo}/releases/tags/{tag}")).ok().filter(|v| v.get("tag_name").is_some()).and_then(|v| parse_release(&v, owner, repo, allowed)) {
            return Ok(r);
        }
    }
    latest(owner, repo, api, allowed)
}

#[cfg(test)]
pub mod fake {
    /// A GitHub API stand-in: one repository with one release and its files.
    pub fn start(owner: &str, repo: &str, files: Vec<(&str, Vec<u8>)>) -> (String, std::sync::Arc<std::sync::Mutex<Vec<String>>>) {
        start_tagged(owner, repo, "v1", files)
    }

    /// A repository whose only release is a pre-release: `releases/latest` says 404, the list has it.
    pub fn start_prerelease_only(owner: &str, repo: &str, tag: &str) -> (String, std::sync::Arc<std::sync::Mutex<Vec<String>>>) {
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let base = format!("http://{}", server.server_addr().to_ip().unwrap());
        let hits = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let (h2, o, r, tag) = (hits.clone(), owner.to_string(), repo.to_string(), tag.to_string());
        std::thread::spawn(move || {
            for req in server.incoming_requests() {
                let url = req.url().to_string();
                h2.lock().unwrap().push(url.clone());
                if url.starts_with(&format!("/repos/{o}/{r}/releases?")) {
                    let body = format!(r#"[{{"tag_name":"{tag}","prerelease":true,"draft":false,"html_url":"https://github.com/{o}/{r}/releases/tag/{tag}","assets":[]}},{{"tag_name":"v99.9.9","draft":true,"html_url":"https://github.com/{o}/{r}/releases/tag/v99.9.9","assets":[]}}]"#);
                    let _ = req.respond(tiny_http::Response::from_string(body));
                } else {
                    let _ = req.respond(tiny_http::Response::from_string(r#"{"message":"Not Found"}"#).with_status_code(404));
                }
            }
        });
        (base, hits)
    }

    /// One repository of [`start_many`]: `owner/repo`, the tag of its release and the release's files.
    pub type Repo<'a> = (&'a str, &'a str, Vec<(&'a str, Vec<u8>)>);
    type OwnedRepo = (String, String, Vec<(String, Vec<u8>)>);

    /// Several repositories, each with one release.
    pub fn start_many(repos: Vec<Repo>) -> (String, std::sync::Arc<std::sync::Mutex<Vec<String>>>) {
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let base = format!("http://{}", server.server_addr().to_ip().unwrap());
        let hits = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let (h2, b2) = (hits.clone(), base.clone());
        let repos: Vec<OwnedRepo> = repos.into_iter().map(|(r, t, f)| (r.to_string(), t.to_string(), f.into_iter().map(|(n, d)| (n.to_string(), d)).collect())).collect();
        std::thread::spawn(move || {
            for req in server.incoming_requests() {
                let url = req.url().to_string();
                h2.lock().unwrap().push(url.clone());
                let latest = repos.iter().find(|(r, t, _)| url == format!("/repos/{r}/releases/latest") || url == format!("/repos/{r}/releases/tags/{t}"));
                let file = repos.iter().flat_map(|(r, _, f)| f.iter().map(move |x| (r, x))).find(|(r, (n, _))| url == format!("/dl/{r}/{n}"));
                let body = if let Some((r, tag, files)) = latest {
                    let assets: Vec<String> = files.iter().map(|(n, d)| format!(r#"{{"name":"{n}","size":{},"browser_download_url":"{b2}/dl/{r}/{n}"}}"#, d.len())).collect();
                    format!(r#"{{"tag_name":"{tag}","name":"Release","html_url":"https://github.com/{r}/releases/tag/{tag}","assets":[{}]}}"#, assets.join(",")).into_bytes()
                } else if let Some((_, (_, d))) = file {
                    d.clone()
                } else {
                    let _ = req.respond(tiny_http::Response::from_string(r#"{"message":"Not Found"}"#).with_status_code(404));
                    continue;
                };
                let _ = req.respond(tiny_http::Response::from_data(body));
            }
        });
        (base, hits)
    }

    pub fn start_tagged(owner: &str, repo: &str, tag: &str, files: Vec<(&str, Vec<u8>)>) -> (String, std::sync::Arc<std::sync::Mutex<Vec<String>>>) {
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let base = format!("http://{}", server.server_addr().to_ip().unwrap());
        let hits = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let (h2, b2, o, r, tag) = (hits.clone(), base.clone(), owner.to_string(), repo.to_string(), tag.to_string());
        let files: Vec<(String, Vec<u8>)> = files.into_iter().map(|(n, d)| (n.to_string(), d)).collect();
        std::thread::spawn(move || {
            for req in server.incoming_requests() {
                let url = req.url().to_string();
                h2.lock().unwrap().push(url.clone());
                let body = if url == format!("/repos/{o}/{r}/releases/latest") {
                    let assets: Vec<String> = files.iter().map(|(n, d)| format!(r#"{{"name":"{n}","size":{},"browser_download_url":"{b2}/dl/{n}"}}"#, d.len())).collect();
                    format!(r#"{{"tag_name":"{tag}","name":"First","html_url":"https://github.com/{o}/{r}/releases/tag/{tag}","assets":[{}]}}"#, assets.join(",")).into_bytes()
                } else if let Some(f) = files.iter().find(|(n, _)| url == format!("/dl/{n}")) {
                    f.1.clone()
                } else {
                    let _ = req.respond(tiny_http::Response::from_data(Vec::new()).with_status_code(404));
                    continue;
                };
                let _ = req.respond(tiny_http::Response::from_data(body));
            }
        });
        (base, hits)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repositories_are_understood_from_links_and_the_short_form() {
        let want = Some(("owner".to_string(), "Cool-Mod".to_string()));
        for input in ["https://github.com/owner/Cool-Mod", "https://github.com/owner/Cool-Mod/", "github.com/owner/Cool-Mod.git", "https://github.com/owner/Cool-Mod/releases/tag/v1", "owner/Cool-Mod", " https://www.github.com/owner/Cool-Mod "] {
            assert_eq!(parse_repo(input), want, "{input}");
        }
        for input in ["https://github.com/owner/Cool-Mod#readme", "https://github.com/owner/Cool-Mod?tab=readme-ov-file", "https://github.com/owner/Cool-Mod/releases?q=1#top"] {
            assert_eq!(parse_repo(input), want, "{input}");
        }
        for bad in ["", "owner", "https://evil.example/owner/repo", "https://github.com/owner", "https://github.com/../x", "a b/c", "https://github.com/ow ner/repo", "x.y/z"] {
            assert_eq!(parse_repo(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn a_download_address_becomes_a_reference_and_nothing_else_does() {
        let r = GithubRef::from_download("https://github.com/owner/Cool-Mod/releases/download/v1.2.0/Cool-Mod-1.2.0.zip").unwrap();
        assert_eq!((r.repo_name().as_str(), r.tag.as_str(), r.asset.as_str()), ("owner/Cool-Mod", "v1.2.0", "Cool-Mod-1.2.0.zip"));
        for bad in ["https://evil.example/owner/repo/releases/download/v1/x.zip", "https://github.com/owner/repo/archive/v1.zip", "https://github.com/owner/repo/releases/download/v1", "https://github.com/../x/releases/download/v1/a.zip", "https://github.com/o/r/releases/download/v1/a/b.zip", ""] {
            assert!(GithubRef::from_download(bad).is_none(), "{bad}");
        }
    }

    #[test]
    fn the_same_file_of_a_newer_release_is_recognised_by_the_shape_of_its_name() {
        let a = |n: &str| Asset { name: n.into(), url: format!("https://github.com/o/r/releases/download/v2/{n}"), size: 1 };
        assert_eq!(shape("Cool-Mod-1.2.0.zip"), shape("Cool-Mod-v1.10.3.zip"));
        assert_ne!(shape("Cool-Mod-1.2.0.zip"), shape("Cool-Mod-Extras-1.2.0.zip"));
        let release = [a("Cool-Mod-1.3.0.zip"), a("Cool-Mod-Extras-1.3.0.zip"), a("source.zip"), a("notes.txt")];
        assert_eq!(matching_asset("Cool-Mod-1.2.0.zip", &release).unwrap().name, "Cool-Mod-1.3.0.zip");
        assert_eq!(matching_asset("Cool-Mod-Extras-1.2.0.zip", &release).unwrap().name, "Cool-Mod-Extras-1.3.0.zip");
        // the file was renamed, but there is only one mod file: that one
        assert_eq!(matching_asset("old-name.zip", &[a("brand-new-name-2.0.zip")]).unwrap().name, "brand-new-name-2.0.zip");
        // several candidates and none the same: it cannot be told
        assert!(matching_asset("old-name.zip", &[a("one.zip"), a("two.zip")]).is_none());
        assert!(matching_asset("x.zip", &[]).is_none());
    }

    #[test]
    fn only_mod_files_of_the_repositorys_own_release_area_are_offered() {
        let v = serde_json::json!({"tag_name":"v2.1","name":"Big update","assets":[
            {"name":"CoolMod-2.1.zip","size":10,"browser_download_url":"https://github.com/owner/repo/releases/download/v2.1/CoolMod-2.1.zip"},
            {"name":"CoolMod-linux.tar.gz","size":10,"browser_download_url":"https://github.com/owner/repo/releases/download/v2.1/CoolMod-linux.tar.gz"},
            {"name":"source.zip","size":10,"browser_download_url":"https://github.com/owner/repo/releases/download/v2.1/source.zip"},
            {"name":"plugin.dll","size":10,"browser_download_url":"https://github.com/owner/repo/releases/download/v2.1/plugin.dll"},
            {"name":"sneaky.zip","size":10,"browser_download_url":"https://evil.example/sneaky.zip"}]});
        let rel = parse_release(&v, "owner", "repo", None).unwrap();
        assert_eq!((rel.tag.as_str(), rel.assets.len()), ("v2.1", 4), "the foreign address is dropped");
        let names: Vec<String> = mod_files(&rel.assets).into_iter().map(|a| a.name).collect();
        assert_eq!(names, vec!["CoolMod-2.1.zip", "plugin.dll"]);
        assert!(download_ok("https://github.com/owner/repo/releases/download/v2.1/x.zip", "owner", "repo", None));
        assert!(!download_ok("https://github.com/owner/other/releases/download/v2.1/x.zip", "owner", "repo", None));
        assert!(!download_ok("https://evil.example/x.zip", "owner", "repo", None));
    }
}
