//! Finding and fetching the UE4SS release.
//!
//! The release asset names change with every build (`…-1164-g5e627997.zip`, then `…-1181-…`),
//! so a hard-coded URL goes stale. The latest `experimental-latest` release is looked up
//! through GitHub's API instead, the `zDEV-UE4SS_v*` archive is preferred (it carries the
//! VTable/MemberVar layout templates for every UE version), downloads are only accepted from
//! the UE4SS repository, and previously downloaded archives keep working offline.

use serde_json::Value;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

const GITHUB_API: &str = "https://api.github.com/repos/UE4SS-RE/RE-UE4SS/releases/tags/experimental-latest";
const GITHUB_DOWNLOADS: &str = "https://github.com/UE4SS-RE/RE-UE4SS/releases/download/";
/// A file the user (or a test) can drop into the cache to force a specific build.
pub const MANUAL_OVERRIDE: &str = "UE4SS_Custom.zip";

#[derive(Clone, Debug)]
pub struct Source {
    pub api_url: String,
    /// Download addresses must start with this (the project's own release area).
    pub allowed_prefix: String,
}

impl Default for Source {
    fn default() -> Self {
        Self { api_url: GITHUB_API.into(), allowed_prefix: GITHUB_DOWNLOADS.into() }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Asset {
    pub name: String,
    pub url: String,
    pub size: u64,
}

fn name_ok(n: &str) -> bool {
    n.ends_with(".zip") && !n.contains('/') && !n.contains("..") && n.chars().all(|c| c.is_ascii_alphanumeric() || "._-+".contains(c))
}

/// The archive to use out of a release's asset list: `zDEV-UE4SS_v*` first, else `UE4SS_v*`.
pub fn pick_asset(release: &Value, allowed_prefix: &str) -> Option<Asset> {
    let assets = release.get("assets")?.as_array()?;
    let cand = |prefix: &str| {
        assets.iter().find_map(|a| {
            let name = a.get("name")?.as_str()?;
            let url = a.get("browser_download_url")?.as_str()?;
            (name.starts_with(prefix) && name_ok(name) && url.starts_with(allowed_prefix)).then(|| Asset {
                name: name.to_string(),
                url: url.to_string(),
                size: a.get("size").and_then(Value::as_u64).unwrap_or(0),
            })
        })
    };
    cand("zDEV-UE4SS_v").or_else(|| cand("UE4SS_v"))
}

fn fetch_latest(src: &Source) -> Result<Asset, String> {
    let out = Command::new("curl")
        .args(["-sSL", "--max-time", "30", "-H", "Accept: application/vnd.github+json", "-H", "User-Agent: ModForge"])
        .arg(&src.api_url)
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("Could not run curl ({}).", e))?;
    let v: Value = serde_json::from_slice(&out.stdout).map_err(|_| "Could not reach GitHub to look up the latest UE4SS.".to_string())?;
    pick_asset(&v, &src.allowed_prefix).ok_or_else(|| {
        let why = v.get("message").and_then(Value::as_str).unwrap_or("no matching download in the release");
        format!("GitHub did not list a UE4SS download ({}).", why)
    })
}

/// Downloaded UE4SS archives in `dir`, newest first.
pub fn cached_archives(dir: &Path) -> Vec<PathBuf> {
    let mut v: Vec<(std::time::SystemTime, PathBuf)> = fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| {
            let n = e.file_name().to_string_lossy().to_string();
            n.ends_with(".zip") && (n.starts_with("zDEV-UE4SS_v") || n.starts_with("UE4SS_v"))
        })
        .filter_map(|e| Some((e.metadata().ok()?.modified().ok()?, e.path())))
        .collect();
    v.sort_by_key(|x| std::cmp::Reverse(x.0));
    v.into_iter().map(|(_, p)| p).collect()
}

/// An archive ready to install: a manual override, the latest download (cached by name), or
/// when GitHub cannot be reached the newest one fetched earlier. `say` reports progress.
pub fn ensure_zip(cache_dir: &Path, src: &Source, say: &dyn Fn(&str)) -> Result<PathBuf, String> {
    let manual = cache_dir.join(MANUAL_OVERRIDE);
    if manual.is_file() {
        return Ok(manual);
    }
    fs::create_dir_all(cache_dir).map_err(|e| e.to_string())?;
    let asset = match fetch_latest(src) {
        Ok(a) => a,
        Err(e) => {
            return match cached_archives(cache_dir).into_iter().next() {
                Some(old) => {
                    say(&format!("{} Using the copy downloaded earlier ({}).", e, old.file_name().unwrap_or_default().to_string_lossy()));
                    Ok(old)
                }
                None => Err(e),
            }
        }
    };
    let dest = cache_dir.join(&asset.name);
    if dest.is_file() {
        return Ok(dest);
    }
    say(&format!("Downloading {} ({:.0} MB)...", asset.name, asset.size as f64 / 1_048_576.0));
    let part = dest.with_extension(format!("zip{}-{}", crate::fsutil::TMP_MARK, crate::fsutil::unique_token()));
    let ok = Command::new("curl")
        .args(["-fL", "--retry", "2", "--connect-timeout", "15", "--max-time", "1800", "-sS", "-o"])
        .arg(&part)
        .arg(&asset.url)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if !ok || fs::metadata(&part).map(|m| m.len()).unwrap_or(0) == 0 {
        let _ = fs::remove_file(&part);
        return Err("Download of UE4SS failed (check your connection).".into());
    }
    fs::rename(&part, &dest).map_err(|e| {
        let _ = fs::remove_file(&part);
        format!("Could not store the UE4SS archive: {}", e)
    })?;
    for old in cached_archives(cache_dir).into_iter().filter(|p| *p != dest) {
        let _ = fs::remove_file(old); // superseded builds are not kept
    }
    Ok(dest)
}

/// Drop bulky parts of the archive nobody needs at runtime (debug symbols, docs) before copying.
pub fn prune_stage(stage: &Path) {
    for e in walkdir::WalkDir::new(stage).max_depth(3).into_iter().flatten() {
        let n = e.file_name().to_string_lossy().to_lowercase();
        if e.file_type().is_file() && n.ends_with(".pdb") {
            let _ = fs::remove_file(e.path());
        }
    }
    if let Ok(rd) = fs::read_dir(stage.join("ue4ss")) {
        for e in rd.flatten() {
            if e.file_type().map(|t| t.is_dir()).unwrap_or(false) && e.file_name().to_string_lossy().eq_ignore_ascii_case("docs") {
                let _ = fs::remove_dir_all(e.path());
            }
        }
    }
}

/// A local stand-in for the GitHub release API and its downloads, for tests.
#[cfg(test)]
mod fakes {
    use super::Source;
    use std::sync::{Arc, Mutex};

    pub struct Fake {
        pub src: Source,
        pub hits: Arc<Mutex<Vec<String>>>,
    }

    pub fn fake_release(asset: &str, payload: Vec<u8>) -> Fake {
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let base = format!("http://{}", server.server_addr().to_ip().unwrap());
        let hits = Arc::new(Mutex::new(Vec::new()));
        let (h2, b2, name) = (hits.clone(), base.clone(), asset.to_string());
        std::thread::spawn(move || {
            for req in server.incoming_requests() {
                let url = req.url().to_string();
                h2.lock().unwrap().push(url.clone());
                let resp = if url == "/release" {
                    let body = format!(r#"{{"assets":[{{"name":"{n}","size":{s},"browser_download_url":"{b}/dl/{n}"}}]}}"#, n = name, s = payload.len(), b = b2);
                    tiny_http::Response::from_data(body.into_bytes())
                } else if url.starts_with("/dl/") {
                    tiny_http::Response::from_data(payload.clone())
                } else {
                    tiny_http::Response::from_data(Vec::new()).with_status_code(404)
                };
                let _ = req.respond(resp);
            }
        });
        Fake { src: Source { api_url: format!("{}/release", base), allowed_prefix: format!("{}/dl/", base) }, hits }
    }
}

#[cfg(test)]
pub use fakes::fake_release;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fsutil::testutil::TempDir;
    use std::sync::Mutex;
    /// The asset list exactly as GitHub returned it for experimental-latest.
    const RELEASE: &str = r#"{"name":"experimental-latest","assets":[
      {"name":"UE4SS_v3.0.1-1181-g83330635.zip","size":8748315,"browser_download_url":"https://github.com/UE4SS-RE/RE-UE4SS/releases/download/experimental-latest/UE4SS_v3.0.1-1181-g83330635.zip"},
      {"name":"zCustomGameConfigs.zip","size":423121,"browser_download_url":"https://github.com/UE4SS-RE/RE-UE4SS/releases/download/experimental-latest/zCustomGameConfigs.zip"},
      {"name":"zDEV-UE4SS_v3.0.1-1181-g83330635.zip","size":52670731,"browser_download_url":"https://github.com/UE4SS-RE/RE-UE4SS/releases/download/experimental-latest/zDEV-UE4SS_v3.0.1-1181-g83330635.zip"},
      {"name":"zMapGenBP.zip","size":27568,"browser_download_url":"https://github.com/UE4SS-RE/RE-UE4SS/releases/download/experimental-latest/zMapGenBP.zip"}]}"#;

    #[test]
    fn picks_the_dev_archive_with_layout_templates_from_the_real_asset_list() {
        let v: Value = serde_json::from_str(RELEASE).unwrap();
        let a = pick_asset(&v, GITHUB_DOWNLOADS).unwrap();
        assert_eq!(a.name, "zDEV-UE4SS_v3.0.1-1181-g83330635.zip");
        assert_eq!(a.size, 52_670_731);
        // without a zDEV build the plain release is used; helper archives never are
        let mut plain = v.clone();
        plain["assets"].as_array_mut().unwrap().retain(|x| !x["name"].as_str().unwrap().starts_with("zDEV"));
        assert_eq!(pick_asset(&plain, GITHUB_DOWNLOADS).unwrap().name, "UE4SS_v3.0.1-1181-g83330635.zip");
        let helpers: Value = serde_json::from_str(r#"{"assets":[{"name":"zMapGenBP.zip","browser_download_url":"https://github.com/UE4SS-RE/RE-UE4SS/releases/download/x/zMapGenBP.zip"}]}"#).unwrap();
        assert!(pick_asset(&helpers, GITHUB_DOWNLOADS).is_none());
    }

    #[test]
    fn refuses_downloads_from_anywhere_but_the_project_and_odd_file_names() {
        let evil = r#"{"assets":[
          {"name":"zDEV-UE4SS_v1.zip","size":1,"browser_download_url":"https://evil.example/UE4SS-RE/zDEV-UE4SS_v1.zip"},
          {"name":"zDEV-UE4SS_v/../x.zip","size":1,"browser_download_url":"https://github.com/UE4SS-RE/RE-UE4SS/releases/download/a/x.zip"},
          {"name":"zDEV-UE4SS_v1 .zip","size":1,"browser_download_url":"https://github.com/UE4SS-RE/RE-UE4SS/releases/download/a/y.zip"}]}"#;
        assert!(pick_asset(&serde_json::from_str(evil).unwrap(), GITHUB_DOWNLOADS).is_none());
        assert!(pick_asset(&serde_json::from_str("{}").unwrap(), GITHUB_DOWNLOADS).is_none());
        assert!(pick_asset(&serde_json::from_str(r#"{"message":"API rate limit exceeded"}"#).unwrap(), GITHUB_DOWNLOADS).is_none());
    }

    #[test]
    fn downloads_once_caches_by_name_prunes_old_builds_and_works_offline_afterwards() {
        let d = TempDir::new("ue4ssdl");
        let cache = d.join("cache");
        let f = fake_release("zDEV-UE4SS_v9.9.9-1-gabc.zip", vec![5u8; 3000]);
        let say = |_: &str| {};
        // an older build is lying around
        fs::create_dir_all(&cache).unwrap();
        fs::write(cache.join("zDEV-UE4SS_v3.0.1-1181-g1.zip"), b"old").unwrap();

        let p = ensure_zip(&cache, &f.src, &say).unwrap();
        assert_eq!(p.file_name().unwrap(), "zDEV-UE4SS_v9.9.9-1-gabc.zip");
        assert_eq!(fs::read(&p).unwrap().len(), 3000);
        assert!(!cache.join("zDEV-UE4SS_v3.0.1-1181-g1.zip").exists(), "superseded archive removed");
        assert!(fs::read_dir(&cache).unwrap().flatten().all(|e| !crate::fsutil::is_tmp_path(&e.path())));

        // second call: same file, no second download
        let before = f.hits.lock().unwrap().iter().filter(|u| u.starts_with("/dl/")).count();
        assert_eq!(ensure_zip(&cache, &f.src, &say).unwrap(), p);
        assert_eq!(f.hits.lock().unwrap().iter().filter(|u| u.starts_with("/dl/")).count(), before);

        // GitHub unreachable: the earlier download is used and the user is told
        let dead = Source { api_url: "http://127.0.0.1:1/release".into(), allowed_prefix: "http://127.0.0.1:1/".into() };
        let said = Mutex::new(String::new());
        let got = ensure_zip(&cache, &dead, &|m: &str| said.lock().unwrap().push_str(m)).unwrap();
        assert_eq!(got, p);
        assert!(said.lock().unwrap().contains("Using the copy downloaded earlier"));
        // nothing cached and nothing reachable: a clear error
        let empty = d.join("empty");
        assert!(ensure_zip(&empty, &dead, &say).unwrap_err().contains("Could not reach GitHub"));

        // a manual override always wins and needs no network
        fs::write(cache.join(MANUAL_OVERRIDE), b"mine").unwrap();
        assert_eq!(ensure_zip(&cache, &dead, &say).unwrap(), cache.join(MANUAL_OVERRIDE));
    }

    #[test]
    fn a_failed_download_leaves_nothing_behind() {
        let d = TempDir::new("ue4ssdl2");
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let base = format!("http://{}", server.server_addr().to_ip().unwrap());
        let b2 = base.clone();
        std::thread::spawn(move || {
            for req in server.incoming_requests() {
                let resp = if req.url() == "/release" {
                    tiny_http::Response::from_data(format!(r#"{{"assets":[{{"name":"UE4SS_v1.zip","size":9,"browser_download_url":"{}/dl/UE4SS_v1.zip"}}]}}"#, b2).into_bytes())
                } else {
                    tiny_http::Response::from_data(Vec::new()).with_status_code(404)
                };
                let _ = req.respond(resp);
            }
        });
        let src = Source { api_url: format!("{}/release", base), allowed_prefix: format!("{}/dl/", base) };
        let e = ensure_zip(&d.join("c"), &src, &|_: &str| {}).unwrap_err();
        assert!(e.contains("Download of UE4SS failed"), "{e}");
        assert_eq!(fs::read_dir(d.join("c")).unwrap().count(), 0);
    }

    #[test]
    fn pruning_drops_symbols_and_docs_but_keeps_everything_needed() {
        let d = TempDir::new("ue4ssprune");
        fs::create_dir_all(d.join("ue4ss/Docs")).unwrap();
        fs::create_dir_all(d.join("ue4ss/VTableLayoutTemplates")).unwrap();
        fs::write(d.join("ue4ss/UE4SS.pdb"), vec![0u8; 100]).unwrap();
        fs::write(d.join("ue4ss/UE4SS.dll"), b"d").unwrap();
        fs::write(d.join("ue4ss/Docs/a.md"), b"x").unwrap();
        fs::write(d.join("ue4ss/VTableLayoutTemplates/VTableLayout_5_04_Template.ini"), b"x").unwrap();
        fs::write(d.join("dwmapi.dll"), b"p").unwrap();
        prune_stage(d.path());
        assert!(!d.join("ue4ss/UE4SS.pdb").exists() && !d.join("ue4ss/Docs").exists());
        assert!(d.join("ue4ss/UE4SS.dll").is_file() && d.join("dwmapi.dll").is_file());
        assert!(d.join("ue4ss/VTableLayoutTemplates/VTableLayout_5_04_Template.ini").is_file());
    }
}
