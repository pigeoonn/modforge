//! Proton and Wine compatibility: which compatibility tools are installed, getting the newest
//! GE-Proton (checked against the checksum its author publishes), and the tweaks that most often
//! fix a game that will not start or freezes.

use crate::fsutil::{self, home_dir};
use serde_json::Value;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// A compatibility tool Steam (or Gantry) can run a game with.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct Tool {
    /// The name Steam stores in its settings.
    pub internal: String,
    pub display: String,
    /// The `proton` script.
    pub script: PathBuf,
}

/// Steam's own names for the Proton builds it ships.
fn official_internal(dir_name: &str) -> Option<String> {
    let n = dir_name.to_lowercase();
    if n == "proton - experimental" {
        return Some("proton_experimental".into());
    }
    if n.starts_with("proton hotfix") {
        return Some("proton_hotfix".into());
    }
    let rest = n.strip_prefix("proton ")?;
    let major: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
    (!major.is_empty()).then(|| format!("proton_{major}"))
}

fn vdf_value(text: &str, key: &str) -> Option<String> {
    let re = regex::Regex::new(&format!(r#""{}"\s+"([^"]*)""#, regex::escape(key))).ok()?;
    re.captures(text).map(|c| c[1].to_string())
}

/// The tool described by a folder (a `proton` script, and usually a `compatibilitytool.vdf`).
fn tool_in(dir: &Path, official: bool) -> Option<Tool> {
    let script = dir.join("proton");
    if !script.is_file() {
        return None;
    }
    let folder = dir.file_name()?.to_string_lossy().to_string();
    let vdf = fs::read_to_string(dir.join("compatibilitytool.vdf")).unwrap_or_default();
    let declared = regex::Regex::new(r#""compat_tools"\s*\{\s*"([^"]+)""#).ok().and_then(|re| re.captures(&vdf).map(|c| c[1].to_string()));
    let internal = declared.or_else(|| if official { official_internal(&folder) } else { Some(folder.clone()) })?;
    let display = vdf_value(&vdf, "display_name").unwrap_or(folder);
    Some(Tool { internal, display, script })
}

/// Every compatibility tool found: Steam's own, the user's `compatibilitytools.d` and the system's.
pub fn tools() -> Vec<Tool> {
    let home = home_dir();
    let official: Vec<PathBuf> = crate::engine::RigPaths::steam_roots().into_iter().map(|r| r.join("steamapps/common")).collect();
    let custom = [home.join(".local/share/Steam/compatibilitytools.d"), home.join(".steam/root/compatibilitytools.d"), PathBuf::from("/usr/share/steam/compatibilitytools.d")];
    tools_in(&official, &custom)
}

/// Tools in Steam's `common` folders (official builds) and in `compatibilitytools.d`-style folders.
pub fn tools_in(official: &[PathBuf], custom: &[PathBuf]) -> Vec<Tool> {
    let mut found: Vec<Tool> = Vec::new();
    let mut seen = std::collections::HashSet::new();
    let mut add = |t: Tool| {
        if seen.insert(fs::canonicalize(&t.script).unwrap_or_else(|_| t.script.clone())) {
            found.push(t);
        }
    };
    for lib in official {
        for e in fs::read_dir(lib).into_iter().flatten().flatten() {
            if e.file_name().to_string_lossy().to_lowercase().starts_with("proton") {
                if let Some(t) = tool_in(&e.path(), true) {
                    add(t);
                }
            }
        }
    }
    for base in custom {
        for e in fs::read_dir(base).into_iter().flatten().flatten() {
            if let Some(t) = tool_in(&e.path(), false) {
                add(t);
            }
        }
    }
    found.sort_by_key(|t| t.display.to_lowercase());
    found
}

// ───────────────────────────── GE-Proton ─────────────────────────────

const GE_API: &str = "https://api.github.com/repos/GloriousEggroll/proton-ge-custom/releases/latest";
const GE_DOWNLOADS: &str = "https://github.com/GloriousEggroll/proton-ge-custom/releases/download/";

#[derive(Clone, Debug)]
pub struct GeSource {
    pub api_url: String,
    pub allowed_prefix: String,
}

impl Default for GeSource {
    fn default() -> Self {
        Self { api_url: GE_API.into(), allowed_prefix: GE_DOWNLOADS.into() }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GeRelease {
    pub tag: String,
    pub tar_url: String,
    pub sum_url: String,
    pub size: u64,
}

fn tag_ok(t: &str) -> bool {
    t.starts_with("GE-Proton") && t.len() < 40 && t.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '.')
}

/// The 64-bit PC build and its checksum file out of a release's asset list.
pub fn pick_ge(release: &Value, allowed: &str) -> Option<GeRelease> {
    let tag = release.get("tag_name")?.as_str()?.to_string();
    if !tag_ok(&tag) {
        return None;
    }
    let asset = |name: &str| {
        release.get("assets")?.as_array()?.iter().find_map(|a| {
            (a.get("name")?.as_str()? == name).then(|| (a.get("browser_download_url").and_then(Value::as_str).unwrap_or("").to_string(), a.get("size").and_then(Value::as_u64).unwrap_or(0)))
        })
    };
    let (tar_url, size) = asset(&format!("{tag}-x86_64.tar.gz"))?;
    let (sum_url, _) = asset(&format!("{tag}-x86_64.sha512sum"))?;
    (tar_url.starts_with(allowed) && sum_url.starts_with(allowed)).then_some(GeRelease { tag, tar_url, sum_url, size })
}

fn curl_bytes(url: &str, max_secs: &str) -> Result<Vec<u8>, String> {
    let out = Command::new("curl")
        .args(["-fsSL", "--max-time", max_secs, "-H", "User-Agent: ModForge", "-H", "Accept: application/vnd.github+json"])
        .arg(url)
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("Could not run curl ({e})."))?;
    if !out.status.success() || out.stdout.is_empty() {
        return Err("Could not reach GitHub.".into());
    }
    Ok(out.stdout)
}

pub fn latest_ge(src: &GeSource) -> Result<GeRelease, String> {
    let v: Value = serde_json::from_slice(&curl_bytes(&src.api_url, "30")?).map_err(|_| "Could not read the GE-Proton release list.".to_string())?;
    pick_ge(&v, &src.allowed_prefix).ok_or_else(|| "GitHub did not list a GE-Proton download for this computer.".to_string())
}

/// The hex checksum at the start of a `.sha512sum` file ("<hash>  <file name>").
pub fn expected_sum(text: &str) -> Option<String> {
    let h = text.split_whitespace().next()?;
    (h.len() == 128 && h.chars().all(|c| c.is_ascii_hexdigit())).then(|| h.to_lowercase())
}

fn sha512_of(p: &Path) -> Result<String, String> {
    let out = Command::new("sha512sum").arg(p).stdin(Stdio::null()).output().map_err(|e| format!("Could not run sha512sum ({e})."))?;
    String::from_utf8_lossy(&out.stdout).split_whitespace().next().map(|h| h.trim_start_matches('\\').to_lowercase()).ok_or_else(|| "Could not compute the checksum.".to_string())
}

/// Download `rel`, check it against its published checksum, and unpack it into `tools_dir`
/// (`compatibilitytools.d`). Nothing is left behind when something fails.
pub fn install_ge(rel: &GeRelease, tools_dir: &Path, cache_dir: &Path, say: &dyn Fn(&str)) -> Result<PathBuf, String> {
    let target = tools_dir.join(&rel.tag);
    let alt = tools_dir.join(format!("{}-x86_64", rel.tag));
    if target.exists() || alt.exists() {
        return Err(format!("{} is already installed.", rel.tag));
    }
    fs::create_dir_all(cache_dir).map_err(|e| e.to_string())?;
    fs::create_dir_all(tools_dir).map_err(|e| e.to_string())?;
    let expected = expected_sum(&String::from_utf8_lossy(&curl_bytes(&rel.sum_url, "30")?)).ok_or("The checksum file is not in the expected format.")?;
    let tar = cache_dir.join(format!("{}.tar.gz.part-{}", rel.tag, fsutil::unique_token()));
    say(&format!("Downloading {} ({:.0} MB)...", rel.tag, rel.size as f64 / 1_048_576.0));
    let ok = Command::new("curl")
        .args(["-fL", "--retry", "2", "--connect-timeout", "15", "--max-time", "3600", "-sS", "-o"])
        .arg(&tar)
        .arg(&rel.tar_url)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    let cleanup = |p: &Path| {
        let _ = fs::remove_file(p);
    };
    if !ok {
        cleanup(&tar);
        return Err("The download failed (check your connection).".into());
    }
    say("Checking the download...");
    if sha512_of(&tar)? != expected {
        cleanup(&tar);
        return Err("The download does not match its checksum, so it was thrown away.".into());
    }
    say("Unpacking...");
    let staging = tools_dir.join(format!(".gantry-unpack-{}", fsutil::unique_token()));
    fs::create_dir_all(&staging).map_err(|e| e.to_string())?;
    let status = Command::new("tar").args(["--no-same-owner", "-xzf"]).arg(&tar).arg("-C").arg(&staging).stdin(Stdio::null()).stderr(Stdio::null()).status();
    cleanup(&tar);
    let result = (|| {
        if !status.map(|s| s.success()).unwrap_or(false) {
            return Err("The archive could not be unpacked.".to_string());
        }
        // the archive holds one folder with the build's name
        let mut dirs = fs::read_dir(&staging).map_err(|e| e.to_string())?.flatten().filter(|e| e.path().is_dir());
        let top = dirs.next().ok_or("The archive was empty.")?.path();
        if !top.join("proton").is_file() {
            return Err("The archive does not contain a Proton build.".into());
        }
        fs::rename(&top, &target).map_err(|e| format!("Could not install it: {e}"))?;
        Ok(target.clone())
    })();
    let _ = fs::remove_dir_all(&staging);
    result
}

/// The tweaks that most often help a game that will not start or freezes: `(id, text, env line)`.
pub const PRESETS: &[(&str, &str, &str)] = &[
    ("no_ntsync", "PROTON_NO_NTSYNC=1", "Wine's NT sync off (try this when a game freezes)"),
    ("no_esync", "PROTON_NO_ESYNC=1", "Eventfd sync off"),
    ("no_fsync", "PROTON_NO_FSYNC=1", "Futex sync off"),
    ("wined3d", "PROTON_USE_WINED3D=1", "OpenGL instead of Vulkan for DirectX"),
    ("wayland", "PROTON_ENABLE_WAYLAND=1", "Native Wayland window"),
    ("hide_nvidia", "PROTON_HIDE_NVIDIA_GPU=1", "Hide the NVIDIA GPU from the game"),
    ("large_address", "PROTON_FORCE_LARGE_ADDRESS_AWARE=1", "Let 32-bit games use more memory"),
];

/// Is the preset's line in these environment lines?
pub fn preset_on(env_vars: &str, line: &str) -> bool {
    env_vars.lines().any(|l| l.trim() == line)
}

/// Add or remove one preset line (other lines stay as they are).
pub fn toggle_preset(env_vars: &str, line: &str, on: bool) -> String {
    let mut lines: Vec<&str> = env_vars.lines().map(str::trim).filter(|l| !l.is_empty() && *l != line).collect();
    if on {
        lines.push(line);
    }
    lines.join("\n")
}

#[cfg(test)]
pub mod fake {
    use super::*;

    pub struct Fake {
        pub src: GeSource,
        pub hits: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
    }

    /// A GitHub stand-in serving a release with the given archive and checksum text.
    pub fn start(tag: &str, tar: Vec<u8>, sum: String) -> Fake {
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let base = format!("http://{}", server.server_addr().to_ip().unwrap());
        let hits = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let (h2, b2, tag2) = (hits.clone(), base.clone(), tag.to_string());
        std::thread::spawn(move || {
            for req in server.incoming_requests() {
                let url = req.url().to_string();
                h2.lock().unwrap().push(url.clone());
                let body = if url == "/release" {
                    let t = &tag2;
                    format!(r#"{{"tag_name":"{t}","assets":[{{"name":"{t}-x86_64.tar.gz","size":{n},"browser_download_url":"{b2}/dl/{t}-x86_64.tar.gz"}},{{"name":"{t}-x86_64.sha512sum","size":159,"browser_download_url":"{b2}/dl/{t}-x86_64.sha512sum"}}]}}"#, n = tar.len()).into_bytes()
                } else if url.ends_with(".tar.gz") {
                    tar.clone()
                } else if url.ends_with(".sha512sum") {
                    sum.clone().into_bytes()
                } else {
                    let _ = req.respond(tiny_http::Response::from_data(Vec::new()).with_status_code(404));
                    continue;
                };
                let _ = req.respond(tiny_http::Response::from_data(body));
            }
        });
        Fake { src: GeSource { api_url: format!("{base}/release"), allowed_prefix: format!("{base}/dl/") }, hits }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fsutil::testutil::TempDir;

    #[test]
    fn steams_own_names_for_its_proton_builds_are_recognised() {
        assert_eq!(official_internal("Proton - Experimental").as_deref(), Some("proton_experimental"));
        assert_eq!(official_internal("Proton 9.0 (Beta)").as_deref(), Some("proton_9"));
        assert_eq!(official_internal("Proton 8.0").as_deref(), Some("proton_8"));
        assert_eq!(official_internal("Proton Hotfix").as_deref(), Some("proton_hotfix"));
        assert_eq!(official_internal("SteamLinuxRuntime"), None);
    }

    #[test]
    fn tools_are_read_from_their_folders() {
        let d = TempDir::new("tools");
        let ge = d.join("GE-Proton11-7-x86_64");
        fs::create_dir_all(&ge).unwrap();
        fs::write(ge.join("proton"), "#!/usr/bin/env python3").unwrap();
        fs::write(ge.join("compatibilitytool.vdf"), "\"compatibilitytools\"\n{\n  \"compat_tools\"\n  {\n    \"GE-Proton11-7-x86_64\" // Internal name\n    {\n      \"install_path\" \".\"\n      \"display_name\" \"GE-Proton11-7\"\n    }\n  }\n}\n").unwrap();
        let t = tool_in(&ge, false).unwrap();
        assert_eq!((t.internal.as_str(), t.display.as_str()), ("GE-Proton11-7-x86_64", "GE-Proton11-7"));
        let off = d.join("Proton - Experimental");
        fs::create_dir_all(&off).unwrap();
        fs::write(off.join("proton"), "x").unwrap();
        assert_eq!(tool_in(&off, true).unwrap().internal, "proton_experimental");
        assert!(tool_in(&d.join("nothing"), false).is_none());
    }

    #[test]
    fn the_right_ge_build_is_picked_and_only_from_github() {
        let v = serde_json::json!({"tag_name":"GE-Proton11-11","assets":[
            {"name":"GE-Proton11-11-aarch64.tar.gz","size":1,"browser_download_url":"https://github.com/GloriousEggroll/proton-ge-custom/releases/download/GE-Proton11-11/GE-Proton11-11-aarch64.tar.gz"},
            {"name":"GE-Proton11-11-x86_64.tar.gz","size":564789552,"browser_download_url":"https://github.com/GloriousEggroll/proton-ge-custom/releases/download/GE-Proton11-11/GE-Proton11-11-x86_64.tar.gz"},
            {"name":"GE-Proton11-11-x86_64.sha512sum","size":159,"browser_download_url":"https://github.com/GloriousEggroll/proton-ge-custom/releases/download/GE-Proton11-11/GE-Proton11-11-x86_64.sha512sum"}]});
        let r = pick_ge(&v, GE_DOWNLOADS).unwrap();
        assert_eq!((r.tag.as_str(), r.size), ("GE-Proton11-11", 564_789_552));
        assert!(r.tar_url.ends_with("x86_64.tar.gz") && r.sum_url.ends_with("x86_64.sha512sum"));
        assert!(pick_ge(&v, "https://elsewhere.example/").is_none());
        let mut bad = v.clone();
        bad["tag_name"] = "../../etc".into();
        assert!(pick_ge(&bad, GE_DOWNLOADS).is_none());
        let hash = "a".repeat(128);
        assert_eq!(expected_sum(&format!("{hash}  GE-Proton11-11-x86_64.tar.gz\n")).as_deref(), Some(hash.as_str()));
        assert_eq!(expected_sum("not a checksum"), None);
    }

    fn tar_gz(d: &TempDir, tag: &str, with_proton: bool) -> Vec<u8> {
        let src = d.join("src");
        fs::create_dir_all(src.join(tag)).unwrap();
        if with_proton {
            fs::write(src.join(tag).join("proton"), "#!/usr/bin/env python3").unwrap();
        }
        fs::write(src.join(tag).join("compatibilitytool.vdf"), "x").unwrap();
        let out = d.join("t.tar.gz");
        assert!(Command::new("tar").args(["-czf"]).arg(&out).arg("-C").arg(&src).arg(tag).status().unwrap().success());
        fs::read(out).unwrap()
    }

    #[test]
    fn a_ge_build_is_installed_only_when_its_checksum_matches() {
        if crate::fsutil::find_in_path("tar").is_none() || crate::fsutil::find_in_path("sha512sum").is_none() {
            eprintln!("skipping: tar or sha512sum missing");
            return;
        }
        let d = TempDir::new("ge");
        let tag = "GE-Proton99-1";
        let tar = tar_gz(&d, tag, true);
        let hash = sha512_of(&{
            let p = d.join("hash-me.tar.gz");
            fs::write(&p, &tar).unwrap();
            p
        })
        .unwrap();
        let tools = d.join("compatibilitytools.d");
        let cache = d.join("cache");
        let said = std::sync::Mutex::new(Vec::<String>::new());
        let say = |m: &str| said.lock().unwrap().push(m.to_string());

        // a wrong checksum: nothing is installed and nothing is left behind
        let bad = fake::start(tag, tar.clone(), format!("{}  x\n", "0".repeat(128)));
        let rel = latest_ge(&bad.src).unwrap();
        let e = install_ge(&rel, &tools, &cache, &say).unwrap_err();
        assert!(e.contains("checksum"), "{e}");
        assert!(!tools.join(tag).exists());
        assert_eq!(fs::read_dir(&cache).unwrap().count(), 0, "the bad download is deleted");
        assert_eq!(fs::read_dir(&tools).unwrap().count(), 0, "no half-unpacked folder");

        // the right one installs, shows up as a tool, and cannot be installed twice
        let good = fake::start(tag, tar, format!("{hash}  {tag}-x86_64.tar.gz\n"));
        let rel = latest_ge(&good.src).unwrap();
        let dir = install_ge(&rel, &tools, &cache, &say).unwrap();
        assert!(dir.join("proton").is_file());
        assert!(install_ge(&rel, &tools, &cache, &say).unwrap_err().contains("already installed"));
        assert!(said.lock().unwrap().iter().any(|m| m.starts_with("Downloading")));

        // an archive that is not a Proton build is refused
        let tar2 = tar_gz(&TempDir::new("ge2"), "GE-Proton99-2", false);
        let hash2 = {
            let p = d.join("h2.tar.gz");
            fs::write(&p, &tar2).unwrap();
            sha512_of(&p).unwrap()
        };
        let odd = fake::start("GE-Proton99-2", tar2, format!("{hash2}  x\n"));
        let rel = latest_ge(&odd.src).unwrap();
        assert!(install_ge(&rel, &tools, &cache, &say).unwrap_err().contains("does not contain a Proton"));
        assert!(!tools.join("GE-Proton99-2").exists());
    }

    #[test]
    fn presets_are_added_and_removed_without_touching_other_lines() {
        let env = "FOO=1\nPROTON_NO_NTSYNC=1\n";
        assert!(preset_on(env, "PROTON_NO_NTSYNC=1") && !preset_on(env, "PROTON_NO_ESYNC=1"));
        assert_eq!(toggle_preset(env, "PROTON_NO_NTSYNC=1", false), "FOO=1");
        assert_eq!(toggle_preset(env, "PROTON_NO_ESYNC=1", true), "FOO=1\nPROTON_NO_NTSYNC=1\nPROTON_NO_ESYNC=1");
        assert_eq!(toggle_preset("", "PROTON_USE_WINED3D=1", true), "PROTON_USE_WINED3D=1");
        assert_eq!(toggle_preset(&toggle_preset(env, "X=1", true), "X=1", true), "FOO=1\nPROTON_NO_NTSYNC=1\nX=1", "no duplicates");
    }
}
