//! "Why is it slow": what a play session cost (memory, processor, and frame times when MangoHud wrote a log)
//! and which enabled mods are the likely suspects. Everything here is a hint to compare against, not a verdict.

use crate::deploy::LibMod;
use serde::{Deserialize, Serialize};
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

/// Frame statistics from a MangoHud log. All numbers are whole so a session can be stored and compared.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FrameStats {
    pub frames: u64,
    pub secs: u64,
    pub avg_fps: u32,
    /// Frame rate over the slowest 1% / 0.1% of frames.
    pub low1_fps: u32,
    pub low01_fps: u32,
    pub worst_ms: u32,
    /// Frames that took more than three times the usual and over 40 ms: what a player feels as a stutter.
    pub spikes: u32,
    /// Average load in percent, 0 when the log has no such column.
    pub cpu_load: u32,
    pub gpu_load: u32,
}

/// What one session cost, as far as Gantry could see.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Summary {
    pub peak_mb: u64,
    /// Processor use of the game, in percent of one core.
    pub avg_cpu: u32,
    pub peak_cpu: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub frames: Option<FrameStats>,
}

/// Collects memory and processor samples while the game runs.
#[derive(Default)]
pub struct Tracker {
    last: Option<(u64, u64)>,
    peak_rss_kb: u64,
    cpu_sum: f64,
    cpu_n: u32,
    cpu_peak: f64,
}

/// Clock ticks per second that `/proc/<pid>/stat` counts in (USER_HZ is 100 on Linux).
const TICKS_PER_SEC: f64 = 100.0;

impl Tracker {
    /// One sample: the game's total resident memory (KiB) and the processor time it has used (clock ticks).
    pub fn add(&mut self, now_ms: u64, rss_kb: u64, ticks: u64) {
        self.peak_rss_kb = self.peak_rss_kb.max(rss_kb);
        if let Some((t0, k0)) = self.last {
            // a process that ended makes the total drop: skip that interval
            if ticks >= k0 && now_ms > t0 {
                let pct = (ticks - k0) as f64 / (TICKS_PER_SEC * (now_ms - t0) as f64 / 1000.0) * 100.0;
                self.cpu_sum += pct;
                self.cpu_n += 1;
                self.cpu_peak = self.cpu_peak.max(pct);
            }
        }
        self.last = Some((now_ms, ticks));
    }

    pub fn summary(&self) -> Option<Summary> {
        if self.peak_rss_kb == 0 && self.cpu_n == 0 {
            return None;
        }
        Some(Summary {
            peak_mb: self.peak_rss_kb / 1024,
            avg_cpu: if self.cpu_n == 0 { 0 } else { (self.cpu_sum / self.cpu_n as f64).round() as u32 },
            peak_cpu: self.cpu_peak.round() as u32,
            frames: None,
        })
    }
}

/// Read a MangoHud log (CSV). `None` when it is not one, or holds no frames.
pub fn parse_log(text: &str) -> Option<FrameStats> {
    parse_lines(text.lines().map(|l| Ok::<_, std::io::Error>(l.to_string())))
}

fn parse_lines<I: Iterator<Item = std::io::Result<String>>>(lines: I) -> Option<FrameStats> {
    let mut cols: Option<Cols> = None;
    let mut times: Vec<f64> = Vec::new();
    let (mut cpu_sum, mut gpu_sum, mut load_n) = (0.0, 0.0, 0u64);
    for line in lines.map_while(Result::ok) {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let cells: Vec<&str> = line.split(',').map(str::trim).collect();
        let Some(c) = &cols else {
            // the header line is the one that starts with "fps"; the lines before it describe the machine
            if cells[0].eq_ignore_ascii_case("fps") {
                cols = Some(Cols::from_header(&cells));
            }
            continue;
        };
        let get = |i: Option<usize>| i.and_then(|i| cells.get(i)).and_then(|v| v.parse::<f64>().ok());
        let ft = match (get(c.frametime), get(c.fps)) {
            (Some(ms), _) if ms > 0.0 && ms.is_finite() => ms,
            (_, Some(fps)) if fps > 0.0 && fps.is_finite() => 1000.0 / fps,
            _ => continue,
        };
        times.push(ft);
        if let (Some(cpu), Some(gpu)) = (get(c.cpu), get(c.gpu)) {
            cpu_sum += cpu;
            gpu_sum += gpu;
            load_n += 1;
        }
    }
    if times.is_empty() {
        return None;
    }
    let total_ms: f64 = times.iter().sum();
    let mut sorted = times.clone();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let n = sorted.len();
    let median = sorted[n / 2];
    let worst_avg = |share: f64| -> f64 {
        let k = ((n as f64 * share).ceil() as usize).clamp(1, n);
        sorted[n - k..].iter().sum::<f64>() / k as f64
    };
    let fps = |ms: f64| if ms > 0.0 { (1000.0 / ms).round() as u32 } else { 0 };
    Some(FrameStats {
        frames: n as u64,
        secs: (total_ms / 1000.0).round() as u64,
        avg_fps: fps(total_ms / n as f64),
        low1_fps: fps(worst_avg(0.01)),
        low01_fps: fps(worst_avg(0.001)),
        worst_ms: sorted[n - 1].round() as u32,
        spikes: times.iter().filter(|&&t| t > 3.0 * median && t > 40.0).count() as u32,
        cpu_load: if load_n == 0 { 0 } else { (cpu_sum / load_n as f64).round() as u32 },
        gpu_load: if load_n == 0 { 0 } else { (gpu_sum / load_n as f64).round() as u32 },
    })
}

struct Cols {
    fps: Option<usize>,
    frametime: Option<usize>,
    cpu: Option<usize>,
    gpu: Option<usize>,
}

impl Cols {
    fn from_header(cells: &[&str]) -> Cols {
        let at = |name: &str| cells.iter().position(|c| c.eq_ignore_ascii_case(name));
        Cols { fps: at("fps"), frametime: at("frametime"), cpu: at("cpu_load"), gpu: at("gpu_load") }
    }
}

/// Logs bigger than this are not read (an hour at 144 fps is about 40 MB).
const MAX_LOG: u64 = 300_000_000;

pub fn parse_file(path: &Path) -> Result<FrameStats, String> {
    let meta = fs::metadata(path).map_err(|e| e.to_string())?;
    if !meta.is_file() || meta.len() > MAX_LOG {
        return Err("That file is too large to be a MangoHud log.".into());
    }
    let f = fs::File::open(path).map_err(|e| e.to_string())?;
    parse_lines(BufReader::new(f).lines()).ok_or_else(|| "This is not a MangoHud log.".to_string())
}

/// Where MangoHud may have put its logs: the folder named in its config, then the usual places.
pub fn log_dirs(home: &Path, config_text: Option<&str>) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = Vec::new();
    for line in config_text.unwrap_or("").lines() {
        if let Some(v) = line.trim().strip_prefix("output_folder") {
            let v = v.trim_start().trim_start_matches('=').trim();
            if !v.is_empty() {
                out.push(match v.strip_prefix("~/") {
                    Some(rest) => home.join(rest),
                    None => PathBuf::from(v),
                });
            }
        }
    }
    out.push(home.to_path_buf());
    out.push(home.join("MangoHud"));
    out.push(home.join(".local/share/MangoHud"));
    out.push(PathBuf::from("/tmp/MangoHud"));
    out
}

/// The log MangoHud wrote during `[from_ms, to_ms]` (a little slack either side): the newest one in the folders.
pub fn find_log(dirs: &[PathBuf], from_ms: u64, to_ms: u64) -> Option<PathBuf> {
    let (lo, hi) = (from_ms.saturating_sub(60_000), to_ms + 120_000);
    let mut best: Option<(u64, PathBuf)> = None;
    for dir in dirs {
        for e in fs::read_dir(dir).into_iter().flatten().flatten() {
            let path = e.path();
            if path.extension().and_then(|x| x.to_str()) != Some("csv") || path.to_string_lossy().ends_with("_summary.csv") {
                continue;
            }
            let Some(mtime) = e.metadata().ok().filter(|m| m.is_file()).and_then(|m| m.modified().ok()).and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map(|d| d.as_millis() as u64) else { continue };
            if mtime < lo || mtime > hi || best.as_ref().map(|(t, _)| mtime <= *t).unwrap_or(false) {
                continue;
            }
            // cheap check before the whole file is read: MangoHud's second block starts with "fps"
            let Ok(f) = fs::File::open(&path) else { continue };
            if BufReader::new(f).lines().take(4).map_while(Result::ok).any(|l| l.trim_start().to_lowercase().starts_with("fps")) {
                best = Some((mtime, path));
            }
        }
    }
    best.map(|(_, p)| p)
}

/// An enabled mod that is a likely suspect when the game runs badly.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Heavy {
    pub id: String,
    pub name: String,
    pub size: u64,
    /// Files that run code inside the game (plugins, scripts).
    pub scripts: usize,
}

const SCRIPT_EXTS: [&str; 7] = ["dll", "asi", "lua", "py", "cs", "js", "pyc"];
const BIG_MOD: u64 = 400 * 1024 * 1024;

/// Enabled mods that are big or run code, the ones that cost most first.
pub fn heavy(mods: &[LibMod]) -> Vec<Heavy> {
    let mut out: Vec<Heavy> = mods
        .iter()
        .filter(|m| m.enabled)
        .map(|m| Heavy {
            id: m.id.clone(),
            name: m.name.clone(),
            size: m.size,
            scripts: m.files.iter().filter(|f| Path::new(f).extension().and_then(|x| x.to_str()).map(|x| SCRIPT_EXTS.contains(&x.to_lowercase().as_str())).unwrap_or(false)).count(),
        })
        .filter(|h| h.size >= BIG_MOD || h.scripts > 0)
        .collect();
    out.sort_by_key(|h| std::cmp::Reverse((h.size >= BIG_MOD, h.size, h.scripts)));
    out.truncate(6);
    out
}

/// What the numbers suggest, as codes the interface turns into sentences.
pub fn tips(s: &Summary) -> Vec<&'static str> {
    let mut out = Vec::new();
    if let Some(f) = &s.frames {
        if f.spikes >= 3 {
            out.push("spikes");
        }
        if f.avg_fps > 0 && f.low1_fps * 2 < f.avg_fps {
            out.push("dips");
        }
        if f.gpu_load > 0 && f.cpu_load > 0 {
            if f.gpu_load >= 95 {
                out.push("gpu");
            } else if f.gpu_load < 60 && f.avg_fps < 60 {
                out.push("cpu");
            }
        }
        if out.is_empty() && f.avg_fps >= 60 {
            out.push("smooth");
        }
    }
    if s.peak_mb >= 12_000 {
        out.push("memory");
    }
    out
}

/// Averages over the sessions that have a summary, split by whether mods were on.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct Group {
    pub sessions: u32,
    pub avg_fps: u32,
    pub peak_mb: u64,
    pub avg_cpu: u32,
}

pub fn compare(sessions: &[(usize, Summary)]) -> (Group, Group) {
    let group = |modded: bool| {
        let mine: Vec<&Summary> = sessions.iter().filter(|(m, _)| (*m > 0) == modded).map(|(_, s)| s).collect();
        let n = mine.len() as u64;
        if n == 0 {
            return Group::default();
        }
        let fps: Vec<u64> = mine.iter().filter_map(|s| s.frames.as_ref().map(|f| f.avg_fps as u64)).collect();
        Group {
            sessions: n as u32,
            avg_fps: if fps.is_empty() { 0 } else { (fps.iter().sum::<u64>() / fps.len() as u64) as u32 },
            peak_mb: mine.iter().map(|s| s.peak_mb).sum::<u64>() / n,
            avg_cpu: (mine.iter().map(|s| s.avg_cpu as u64).sum::<u64>() / n) as u32,
        }
    };
    (group(true), group(false))
}

/// Detailed mod startup benchmark diagnostics.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ImpactMod {
    pub id: String,
    pub name: String,
    pub file_count: usize,
    pub size_mb: f64,
    pub impact_score: u32,
    pub category: String,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct StartupBenchmark {
    pub appid: String,
    pub game_name: String,
    pub mod_count: usize,
    pub total_files: usize,
    pub total_size_mb: f64,
    pub loose_files: usize,
    pub plugins_count: usize,
    pub vanilla_est_ms: u64,
    pub modded_est_ms: u64,
    pub overhead_ms: u64,
    pub overhead_rating: String,
    pub disk_io_overhead_ms: u64,
    pub hook_overhead_ms: u64,
    pub top_impact_mods: Vec<ImpactMod>,
    pub recommendations: Vec<String>,
}

/// Calculate startup performance benchmark and latency overhead based on deployed mod assets.
pub fn benchmark_startup(appid: &str, game_name: &str, mods: &[LibMod]) -> StartupBenchmark {
    let enabled: Vec<&LibMod> = mods.iter().filter(|m| m.enabled).collect();
    let mod_count = enabled.len();
    let mut total_files = 0;
    let mut total_size_bytes = 0u64;
    let mut loose_files = 0;
    let mut plugins_count = 0;
    let mut hooks_count = 0;

    let mut impact_mods: Vec<ImpactMod> = Vec::new();

    for m in &enabled {
        total_files += m.files.len();
        total_size_bytes += m.size;
        let mut m_textures = 0;
        let mut m_scripts = 0;
        let mut m_plugins = 0;
        let mut m_hooks = 0;

        for f in &m.files {
            let ext = Path::new(f).extension().and_then(|x| x.to_str()).map(|x| x.to_lowercase()).unwrap_or_default();
            match ext.as_str() {
                "dds" | "png" | "tga" | "jpg" | "jpeg" => { m_textures += 1; loose_files += 1; }
                "dll" | "so" | "asi" => { m_hooks += 1; hooks_count += 1; }
                "esp" | "esm" | "esl" | "pck" | "vpk" | "pak" | "ba2" | "bsa" => { m_plugins += 1; plugins_count += 1; }
                "lua" | "pex" | "cs" | "gd" | "py" => { m_scripts += 1; loose_files += 1; }
                _ => { loose_files += 1; }
            }
        }

        let cat = if m_hooks > 0 {
            "Hooks / DLLs"
        } else if m_scripts > 0 {
            "Scripts / Logic"
        } else if m_plugins > 0 {
            "Plugins / Packages"
        } else if m_textures > 0 {
            "Textures / Visuals"
        } else {
            "General Assets"
        };

        let size_mb = m.size as f64 / (1024.0 * 1024.0);
        let score = ((size_mb * 0.4) as u32) + ((m.files.len() as f64 * 0.15) as u32) + (m_hooks as u32 * 50) + (m_scripts as u32 * 10);
        impact_mods.push(ImpactMod {
            id: m.id.clone(),
            name: m.name.clone(),
            file_count: m.files.len(),
            size_mb: (size_mb * 10.0).round() / 10.0,
            impact_score: score,
            category: cat.to_string(),
        });
    }

    impact_mods.sort_by_key(|m| std::cmp::Reverse(m.impact_score));
    impact_mods.truncate(8);

    let vanilla_est_ms = 3500;
    let disk_io_overhead_ms = ((total_size_bytes as f64 / (1024.0 * 1024.0)) * 0.8 + (loose_files as f64 * 0.3)).round() as u64;
    let hook_overhead_ms = (hooks_count as u64 * 120) + (plugins_count as u64 * 25);
    let overhead_ms = disk_io_overhead_ms + hook_overhead_ms;
    let modded_est_ms = vanilla_est_ms + overhead_ms;

    let overhead_rating = if overhead_ms < 1500 {
        "Instant (<1.5s)"
    } else if overhead_ms < 4000 {
        "Optimal (1.5-4s)"
    } else if overhead_ms < 8000 {
        "Moderate (4-8s)"
    } else {
        "Heavy (>8s)"
    };

    let mut recommendations = Vec::new();
    if loose_files > 5000 {
        recommendations.push(format!("Over {} loose files detected: game engine directory scans will cause noticeable startup delay.", loose_files));
    }
    if total_size_bytes > 5 * 1024 * 1024 * 1024 {
        recommendations.push("Over 5 GB of mod assets loaded into memory: ensure fast SSD storage and sufficient RAM headroom.".into());
    }
    if hooks_count > 3 {
        recommendations.push(format!("{} native DLL/ASI hooks injected: inspect script extenders to prevent startup crashes.", hooks_count));
    }
    if recommendations.is_empty() {
        recommendations.push("Mod load profile is streamlined with optimal startup overhead.".into());
    }

    StartupBenchmark {
        appid: appid.to_string(),
        game_name: game_name.to_string(),
        mod_count,
        total_files,
        total_size_mb: ((total_size_bytes as f64 / (1024.0 * 1024.0)) * 10.0).round() / 10.0,
        loose_files,
        plugins_count,
        vanilla_est_ms,
        modded_est_ms,
        overhead_ms,
        overhead_rating: overhead_rating.to_string(),
        disk_io_overhead_ms,
        hook_overhead_ms,
        top_impact_mods: impact_mods,
        recommendations,
    }
}


#[cfg(test)]
mod tests {
    use super::*;
    use crate::fsutil::testutil::TempDir;

    const HEADER: &str = "os,cpu,gpu,ram,kernel,driver,cpuscheduler\nLinux,Some CPU,Some GPU,16000000,6.9.1,Mesa,schedutil\nfps,frametime,cpu_load,gpu_load,elapsed\n";

    fn log(frames: &[(f64, f64, u32, u32)]) -> String {
        let mut s = HEADER.to_string();
        for (i, (fps, ft, cpu, gpu)) in frames.iter().enumerate() {
            s.push_str(&format!("{fps},{ft},{cpu},{gpu},{}\n", i * 16_000_000));
        }
        s
    }

    #[test]
    fn a_steady_log_has_no_spikes_and_close_lows() {
        let frames: Vec<(f64, f64, u32, u32)> = (0..1000).map(|_| (100.0, 10.0, 40, 90)).collect();
        let f = parse_log(&log(&frames)).unwrap();
        assert_eq!((f.frames, f.avg_fps, f.low1_fps, f.low01_fps, f.spikes, f.worst_ms), (1000, 100, 100, 100, 0, 10));
        assert_eq!((f.cpu_load, f.gpu_load, f.secs), (40, 90, 10));
    }

    #[test]
    fn stutters_pull_the_lows_down_and_are_counted() {
        let mut frames: Vec<(f64, f64, u32, u32)> = (0..990).map(|_| (100.0, 10.0, 40, 90)).collect();
        frames.extend((0..10).map(|_| (10.0, 100.0, 40, 90)));
        let f = parse_log(&log(&frames)).unwrap();
        assert_eq!(f.spikes, 10);
        assert_eq!(f.worst_ms, 100);
        assert_eq!(f.low1_fps, 10, "the slowest 1% are all 100 ms frames");
        assert!(f.avg_fps < 100 && f.avg_fps > 80, "{}", f.avg_fps);
        assert_eq!(f.low01_fps, 10);
    }

    #[test]
    fn a_log_without_a_frametime_column_uses_the_frame_rate() {
        let text = "os,cpu\nLinux,x\nfps,cpu_load,gpu_load\n50,10,20\n50,10,20\nbad line\n,,\n";
        let f = parse_log(text).unwrap();
        assert_eq!((f.frames, f.avg_fps, f.worst_ms), (2, 50, 20));
    }

    #[test]
    fn things_that_are_not_logs_are_refused() {
        assert!(parse_log("").is_none());
        assert!(parse_log("a,b,c\n1,2,3\n").is_none());
        assert!(parse_log("0.1% Min FPS,1% Min FPS,97% Percentile FPS\n10,20,30\n").is_none(), "the summary file is not a log");
        assert!(parse_log(HEADER).is_none(), "a header with no frames");
        assert!(parse_log("fps,frametime\nx,y\n0,0\n-5,-5\nNaN,NaN\n").is_none());
    }

    #[test]
    fn the_tracker_averages_cpu_between_samples_and_ignores_a_drop() {
        let mut t = Tracker::default();
        assert!(t.summary().is_none());
        t.add(0, 2_048_000, 0);
        t.add(2_000, 3_072_000, 200); // 200 ticks in 2 s = one full core
        t.add(4_000, 2_500_000, 300); // half a core
        t.add(6_000, 2_500_000, 50); // a process ended: the total fell, nothing counted
        let s = t.summary().unwrap();
        assert_eq!((s.peak_mb, s.avg_cpu, s.peak_cpu), (3000, 75, 100));
    }

    #[test]
    fn logs_are_found_by_time_and_the_summary_file_is_skipped() {
        let d = TempDir::new("perflogs");
        let now = crate::fsutil::now_ms();
        fs::write(d.join("game_2026.csv"), log(&[(60.0, 16.6, 1, 1)])).unwrap();
        fs::write(d.join("game_2026_summary.csv"), "0.1% Min FPS,1% Min FPS\n1,2\n").unwrap();
        fs::write(d.join("other.csv"), "a,b\n1,2\n").unwrap();
        fs::write(d.join("notes.txt"), "x").unwrap();
        let dirs = vec![d.path().to_path_buf(), d.join("missing")];
        let found = find_log(&dirs, now - 600_000, now + 1_000).unwrap();
        assert_eq!(found.file_name().unwrap(), "game_2026.csv");
        assert!(find_log(&dirs, now - 10_000_000, now - 5_000_000).is_none(), "written long after that session");
        assert_eq!(parse_file(&found).unwrap().avg_fps, 60);
        assert!(parse_file(&d.join("other.csv")).is_err());
    }

    #[test]
    fn mangohud_folders_come_from_its_config_first() {
        let home = Path::new("/home/x");
        let dirs = log_dirs(home, Some("fps_limit=60\noutput_folder = ~/logs\n"));
        assert_eq!(dirs[0], Path::new("/home/x/logs"));
        assert!(dirs.contains(&PathBuf::from("/tmp/MangoHud")));
        assert_eq!(log_dirs(home, None)[0], home);
        assert_eq!(log_dirs(home, Some("output_folder=/data/mh"))[0], Path::new("/data/mh"));
    }

    fn lib_mod(name: &str, on: bool, size: u64, files: &[&str]) -> LibMod {
        serde_json::from_value(serde_json::json!({ "id": name.to_lowercase(), "name": name, "enabled": on, "size": size, "files": files })).unwrap()
    }

    #[test]
    fn heavy_mods_are_the_big_or_code_running_ones_that_are_switched_on() {
        let mods = vec![
            lib_mod("Textures", true, 3_000_000_000, &["a.dds"]),
            lib_mod("Script Pack", true, 5_000, &["x/Hook.DLL", "y.lua", "z.txt"]),
            lib_mod("Tiny", true, 100, &["a.esp"]),
            lib_mod("Off Big", false, 9_000_000_000, &["a.dds"]),
            lib_mod("Medium", true, 600_000_000, &["m.pak"]),
        ];
        let h = heavy(&mods);
        assert_eq!(h.iter().map(|x| x.name.as_str()).collect::<Vec<_>>(), ["Textures", "Medium", "Script Pack"]);
        assert_eq!(h[2].scripts, 2);
    }

    #[test]
    fn tips_follow_the_numbers() {
        let f = |avg, low1, spikes, cpu, gpu| Summary { peak_mb: 4000, avg_cpu: 100, peak_cpu: 200, frames: Some(FrameStats { avg_fps: avg, low1_fps: low1, spikes, cpu_load: cpu, gpu_load: gpu, frames: 1000, ..Default::default() }) };
        assert_eq!(tips(&f(90, 80, 0, 30, 70)), ["smooth"]);
        assert_eq!(tips(&f(90, 30, 5, 30, 70)), ["spikes", "dips"]);
        assert_eq!(tips(&f(40, 35, 0, 90, 40)), ["cpu"]);
        assert_eq!(tips(&f(40, 35, 0, 40, 99)), ["gpu"]);
        let mut big = f(90, 80, 0, 0, 0);
        big.peak_mb = 14_000;
        assert_eq!(tips(&big), ["smooth", "memory"]);
        assert!(tips(&Summary { peak_mb: 1000, ..Default::default() }).is_empty());
    }

    #[test]
    fn modded_and_plain_sessions_are_averaged_apart() {
        let s = |fps: u32, mb: u64, cpu: u32| Summary { peak_mb: mb, avg_cpu: cpu, peak_cpu: cpu, frames: (fps > 0).then(|| FrameStats { avg_fps: fps, ..Default::default() }) };
        let (modded, plain) = compare(&[(3, s(50, 6000, 120)), (2, s(70, 8000, 140)), (0, s(100, 4000, 80)), (0, s(0, 5000, 100))]);
        assert_eq!((modded.sessions, modded.avg_fps, modded.peak_mb, modded.avg_cpu), (2, 60, 7000, 130));
        assert_eq!((plain.sessions, plain.avg_fps, plain.peak_mb), (2, 100, 4500), "a session without frames adds nothing to the fps");
        assert_eq!(compare(&[]).0.sessions, 0);
    }

    #[test]
    fn benchmark_startup_computes_latency_and_top_impact_mods() {
        let mods = vec![
            lib_mod("Texture Overhaul", true, 500 * 1024 * 1024, &["textures/diffuse.dds", "textures/normal.dds"]),
            lib_mod("Core Engine Extender", true, 20 * 1024 * 1024, &["bin/engine.dll", "bin/loader.asi"]),
            lib_mod("Compatibility Patch", true, 1 * 1024 * 1024, &["data/patch.esp"]),
            lib_mod("Disabled Heavy Mod", false, 1024 * 1024 * 1024, &["heavy.pak"]),
        ];

        let bm = benchmark_startup("12345", "Test Game", &mods);
        assert_eq!(bm.appid, "12345");
        assert_eq!(bm.game_name, "Test Game");
        assert_eq!(bm.mod_count, 3);
        assert_eq!(bm.total_files, 5);
        assert!(bm.total_size_mb >= 520.0);
        assert_eq!(bm.plugins_count, 1);
        assert!(bm.overhead_ms > 0);
        assert_eq!(bm.modded_est_ms, bm.vanilla_est_ms + bm.overhead_ms);
        assert_eq!(bm.top_impact_mods.len(), 3);
        assert_eq!(bm.top_impact_mods[0].name, "Texture Overhaul");
        assert_eq!(bm.top_impact_mods[0].category, "Textures / Visuals");
        assert_eq!(bm.top_impact_mods[1].name, "Core Engine Extender");
        assert_eq!(bm.top_impact_mods[1].category, "Hooks / DLLs");
    }
}
