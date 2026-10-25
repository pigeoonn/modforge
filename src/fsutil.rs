//! Filesystem primitives shared by every module: atomic writes, zero-copy
//! file placement, same-partition moves and a few process/path helpers.
//!
//! Every mutation of a user-visible file goes through here so a crash or a
//! power loss can never leave a half-written config, mods.txt or PAK behind.

use std::env;
use std::ffi::OsString;
use std::fs::{self, File};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

static UNIQ: AtomicU64 = AtomicU64::new(0);

/// Marker embedded in every temporary file/dir name so watchers can ignore our own staging noise.
pub const TMP_MARK: &str = ".rigtmp";

pub fn home_dir() -> PathBuf {
    let var = if cfg!(windows) { "USERPROFILE" } else { "HOME" };
    env::var_os(var).filter(|h| !h.is_empty()).map(PathBuf::from).unwrap_or_else(env::temp_dir)
}

/// The Windows user folder inside a Wine prefix (`drive_c/users/<name>`). Proton always uses
/// `steamuser`; a plain Wine prefix (Lutris, Heroic, Bottles) is named after the Linux account.
/// A prefix that has no user folder yet gets the `steamuser` path, so callers never see an odd one.
pub fn wine_user_dir(prefix: &Path) -> PathBuf {
    let me = env::var("USER").or_else(|_| env::var("LOGNAME")).unwrap_or_default();
    wine_user_dir_for(prefix, &me)
}

fn wine_user_dir_for(prefix: &Path, account: &str) -> PathBuf {
    let users = prefix.join("drive_c/users");
    let steam = users.join("steamuser");
    if steam.is_dir() {
        return steam;
    }
    let mut found: Vec<PathBuf> = fs::read_dir(&users)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.path().is_dir() && !e.file_name().to_string_lossy().eq_ignore_ascii_case("public"))
        .map(|e| e.path())
        .collect();
    found.sort();
    found
        .iter()
        .find(|p| !account.is_empty() && p.file_name().map(|n| n.to_string_lossy() == account).unwrap_or(false))
        .or_else(|| found.first())
        .cloned()
        .unwrap_or(steam)
}

/// The path as text with `/` between folders on every system (what the journal, the interface and
/// the tests use). A backslash is a legal file name character elsewhere, so only Windows is changed.
pub fn slashed(p: &Path) -> String {
    let s = p.to_string_lossy();
    if cfg!(windows) {
        s.replace('\\', "/")
    } else {
        s.into_owned()
    }
}

/// `fs::canonicalize` without the `\\?\` prefix Windows puts in front, so the result can be
/// compared with, and passed on as, an ordinary path.
pub fn canonical(p: &Path) -> io::Result<PathBuf> {
    let c = fs::canonicalize(p)?;
    let s = c.to_string_lossy();
    Ok(match s.strip_prefix(r"\\?\") {
        Some(rest) if cfg!(windows) && !rest.starts_with("UNC") => PathBuf::from(rest),
        _ => c,
    })
}

/// `2025-03-02T03:15:43Z` (or with fractions / an offset suffix we ignore) as milliseconds since 1970; 0 when it is not a date.
pub fn iso_ms(s: &str) -> u64 {
    let b = s.as_bytes();
    let num = |a: usize, n: usize| -> Option<i64> { s.get(a..a + n)?.parse::<i64>().ok() };
    if b.len() < 10 || b[4] != b'-' || b[7] != b'-' {
        return 0;
    }
    let (Some(y), Some(m), Some(d)) = (num(0, 4), num(5, 2), num(8, 2)) else { return 0 };
    let (h, mi, sec) = if b.len() >= 19 && (b[10] == b'T' || b[10] == b' ') { (num(11, 2).unwrap_or(0), num(14, 2).unwrap_or(0), num(17, 2).unwrap_or(0)) } else { (0, 0, 0) };
    if !(1..=12).contains(&m) || !(1..=31).contains(&d) || y < 1970 {
        return 0;
    }
    // days from the civil date (Howard Hinnant's algorithm)
    let (yy, mm) = if m <= 2 { (y - 1, m + 9) } else { (y, m - 3) };
    let era = yy.div_euclid(400);
    let yoe = yy - era * 400;
    let doy = (153 * mm + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    ((days * 86_400 + h * 3600 + mi * 60 + sec) * 1000).max(0) as u64
}

pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Process-unique, collision-free token (pid + clock + counter).
pub fn unique_token() -> String {
    format!(
        "{}-{}-{}",
        std::process::id(),
        now_ms(),
        UNIQ.fetch_add(1, Ordering::Relaxed)
    )
}

pub fn is_tmp_path(p: &Path) -> bool {
    p.file_name()
        .map(|n| n.to_string_lossy().contains(TMP_MARK))
        .unwrap_or(false)
}

fn tmp_sibling(dst: &Path) -> PathBuf {
    let mut name: OsString = dst.file_name().unwrap_or_default().to_os_string();
    name.push(format!("{}-{}", TMP_MARK, unique_token()));
    dst.with_file_name(name)
}

/// Write `data` to `dst` atomically: temp file in the same directory, fsync, rename.
/// Preserves the permission bits of an existing destination.
pub fn atomic_write(dst: &Path, data: impl AsRef<[u8]>) -> io::Result<()> {
    if let Some(parent) = dst.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent)?;
        }
    }
    let tmp = tmp_sibling(dst);
    let res = (|| {
        let mut f = File::create(&tmp)?;
        f.write_all(data.as_ref())?;
        f.sync_all()?;
        if let Ok(meta) = fs::metadata(dst) {
            let _ = fs::set_permissions(&tmp, meta.permissions());
        }
        fs::rename(&tmp, dst)
    })();
    if res.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    res
}

/// The copy of a file as it was before its last change (see `atomic_write_kept`).
pub fn kept_copy(p: &Path) -> PathBuf {
    let mut name: OsString = p.file_name().unwrap_or_default().to_os_string();
    name.push(".bak");
    p.with_file_name(name)
}

/// `atomic_write`, keeping the file as it was before as its `kept_copy`: a file that is damaged later (a full disk, a
/// crash of the computer, a hand edit) can then be recovered by `read_json_kept`.
pub fn atomic_write_kept(dst: &Path, data: impl AsRef<[u8]>) -> io::Result<()> {
    if fs::metadata(dst).map(|m| m.is_file() && m.len() > 0).unwrap_or(false) {
        let _ = fs::copy(dst, kept_copy(dst));
    } else {
        // the first version is its own copy, so there is something to go back to from the start
        let _ = atomic_write(&kept_copy(dst), data.as_ref());
    }
    atomic_write(dst, data)
}

/// What reading a kept file found.
#[derive(Debug, PartialEq, Eq)]
pub enum Loaded<T> {
    Fine(T),
    Missing,
    /// The file could not be read; the copy from before its last change was used (and put back).
    Recovered(T),
    /// The file could not be read and there was no good copy; it was moved aside to this path, never deleted.
    Lost(PathBuf),
}

impl<T> Loaded<T> {
    pub fn value(self) -> Option<T> {
        match self {
            Loaded::Fine(v) | Loaded::Recovered(v) => Some(v),
            _ => None,
        }
    }
}

/// Files that had to be recovered (true) or could not be (false) since this was last asked.
static REPAIRED: std::sync::Mutex<Vec<(PathBuf, bool)>> = std::sync::Mutex::new(Vec::new());

pub fn take_repaired() -> Vec<(PathBuf, bool)> {
    std::mem::take(&mut *REPAIRED.lock().unwrap_or_else(|e| e.into_inner()))
}

/// Read a JSON file written with `atomic_write_kept`. One that cannot be read is moved aside (`<name>.corrupt-<ms>`)
/// and its kept copy is used and put back in its place. Both are noted for `take_repaired`.
pub fn read_json_kept<T: serde::de::DeserializeOwned>(p: &Path) -> Loaded<T> {
    let Ok(bytes) = fs::read(p) else {
        // a change that stopped between the copy and the write leaves only the copy
        return match fs::read(kept_copy(p)).ok().and_then(|b| serde_json::from_slice::<T>(&b).ok()) {
            Some(v) if !p.exists() && fs::copy(kept_copy(p), p).is_ok() => {
                REPAIRED.lock().unwrap_or_else(|e| e.into_inner()).push((p.to_path_buf(), true));
                Loaded::Recovered(v)
            }
            _ => Loaded::Missing,
        };
    };
    if let Ok(v) = serde_json::from_slice(&bytes) {
        return Loaded::Fine(v);
    }
    let mut name: OsString = p.file_name().unwrap_or_default().to_os_string();
    name.push(format!(".corrupt-{}", now_ms()));
    let aside = p.with_file_name(name);
    let _ = fs::rename(p, &aside);
    let kept = kept_copy(p);
    match fs::read(&kept).ok().and_then(|b| serde_json::from_slice::<T>(&b).ok()) {
        Some(v) => {
            let _ = fs::copy(&kept, p);
            REPAIRED.lock().unwrap_or_else(|e| e.into_inner()).push((p.to_path_buf(), true));
            Loaded::Recovered(v)
        }
        None => {
            REPAIRED.lock().unwrap_or_else(|e| e.into_inner()).push((p.to_path_buf(), false));
            Loaded::Lost(aside)
        }
    }
}

/// Like `atomic_write`, but the file is created readable by its owner only (secrets such as API
/// keys). The temp file already has mode 0600, so the contents are never briefly world-readable.
pub fn atomic_write_private(dst: &Path, data: impl AsRef<[u8]>) -> io::Result<()> {
    if let Some(parent) = dst.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent)?;
        }
    }
    let tmp = tmp_sibling(dst);
    let res = (|| {
        let mut opts = fs::OpenOptions::new();
        opts.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        let mut f = opts.open(&tmp)?;
        f.write_all(data.as_ref())?;
        f.sync_all()?;
        fs::rename(&tmp, dst)
    })();
    if res.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    res
}

/// Copy `src` to `dst` atomically. `fs::copy` uses copy_file_range/reflink on Linux,
/// so the payload never crosses user space.
pub fn atomic_copy(src: &Path, dst: &Path) -> io::Result<u64> {
    if let Some(parent) = dst.parent() {
        fs::create_dir_all(parent)?;
    }
    let tmp = tmp_sibling(dst);
    match fs::copy(src, &tmp).and_then(|n| fs::rename(&tmp, dst).map(|_| n)) {
        Ok(n) => Ok(n),
        Err(e) => {
            let _ = fs::remove_file(&tmp);
            Err(e)
        }
    }
}

fn is_cross_device(e: &io::Error) -> bool {
    #[cfg(unix)]
    return e.raw_os_error() == Some(libc::EXDEV);
    #[cfg(not(unix))]
    return e.raw_os_error() == Some(17); // ERROR_NOT_SAME_DEVICE
}

/// Create a symbolic link at `link` pointing to `target` (relative targets are relative to `link`).
pub fn symlink(target: impl AsRef<Path>, link: impl AsRef<Path>) -> io::Result<()> {
    let (target, link) = (target.as_ref(), link.as_ref());
    #[cfg(unix)]
    return std::os::unix::fs::symlink(target, link);
    #[cfg(windows)]
    {
        let resolved = link.parent().map(|p| p.join(target)).unwrap_or_else(|| target.to_path_buf());
        return if resolved.is_dir() { std::os::windows::fs::symlink_dir(target, link) } else { std::os::windows::fs::symlink_file(target, link) };
    }
}

/// Mark a file executable.
#[cfg(unix)]
pub fn set_executable(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o755))
}

/// Mark a file executable (files have no such bit here).
#[cfg(not(unix))]
pub fn set_executable(_path: &Path) -> io::Result<()> {
    Ok(())
}

/// `(inode, modification time in seconds, size)`: a cheap identity of a file's content.
/// Without inodes (Windows) the first field is 0.
#[cfg(unix)]
pub fn file_signature(m: &fs::Metadata) -> (u64, i64, u64) {
    use std::os::unix::fs::MetadataExt;
    (m.ino(), m.mtime(), m.size())
}

#[cfg(not(unix))]
pub fn file_signature(m: &fs::Metadata) -> (u64, i64, u64) {
    (0, mtime_ns(m) / 1_000_000_000, m.len())
}

/// Modification time in nanoseconds since the Unix epoch (0 when unknown).
pub fn mtime_ns(m: &fs::Metadata) -> i64 {
    m.modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_nanos() as i64)
        .unwrap_or(0)
}

/// Do both paths name the very same file (one hard link of it)?
pub fn is_same_file(a: &Path, b: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        match (fs::metadata(a), fs::metadata(b)) {
            (Ok(x), Ok(y)) => x.ino() == y.ino() && x.dev() == y.dev(),
            _ => false,
        }
    }
    #[cfg(not(unix))]
    {
        same_file::is_same_file(a, b).unwrap_or(false)
    }
}

/// Can files be created in this directory by the current user?
/// Free room, in bytes, on the drive that holds `dir` (None where the system does not say).
pub fn free_bytes(dir: &Path) -> Option<u64> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        let c = std::ffi::CString::new(dir.as_os_str().as_bytes()).ok()?;
        let mut s: libc::statvfs = unsafe { std::mem::zeroed() };
        if unsafe { libc::statvfs(c.as_ptr(), &mut s) } != 0 {
            return None;
        }
        Some((s.f_bavail as u64).saturating_mul(s.f_frsize as u64))
    }
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        #[link(name = "kernel32")]
        extern "system" {
            fn GetDiskFreeSpaceExW(dir: *const u16, available: *mut u64, total: *mut u64, free: *mut u64) -> i32;
        }
        let mut wide: Vec<u16> = dir.as_os_str().encode_wide().collect();
        wide.push(0);
        let mut available = 0u64;
        // the space this user may use (quotas count), for the drive the folder is on
        let ok = unsafe { GetDiskFreeSpaceExW(wide.as_ptr(), &mut available, std::ptr::null_mut(), std::ptr::null_mut()) };
        (ok != 0).then_some(available)
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = dir;
        None
    }
}

/// A size for a message: "2.4 GB", "310 MB".
pub fn size_text(n: u64) -> String {
    if n >= 1 << 30 {
        format!("{:.1} GB", n as f64 / (1u64 << 30) as f64)
    } else {
        format!("{} MB", n.div_ceil(1 << 20))
    }
}

pub fn can_write(dir: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        std::ffi::CString::new(dir.as_os_str().as_bytes())
            .map(|c| unsafe { libc::access(c.as_ptr(), libc::W_OK) == 0 })
            .unwrap_or(false)
    }
    #[cfg(not(unix))]
    {
        fs::metadata(dir).map(|m| m.is_dir() && !m.permissions().readonly()).unwrap_or(false)
    }
}

/// Move a file. Same partition: a single atomic `rename(2)` (instant, any size).
/// Different partition: atomic copy followed by removing the source.
pub fn move_file(src: &Path, dst: &Path) -> io::Result<()> {
    if let Some(parent) = dst.parent() {
        fs::create_dir_all(parent)?;
    }
    match fs::rename(src, dst) {
        Ok(()) => Ok(()),
        Err(e) if is_cross_device(&e) => {
            atomic_copy(src, dst)?;
            fs::remove_file(src)
        }
        Err(e) => Err(e),
    }
}

/// Place a file at `dst`, either moving or copying the source.
pub fn place_file(src: &Path, dst: &Path, consume_src: bool) -> io::Result<()> {
    if consume_src {
        move_file(src, dst)
    } else {
        atomic_copy(src, dst).map(|_| ())
    }
}

/// Recursively copy `src` into `dst`, overwriting files (like `cp -rf src/. dst`).
/// Symlinks are recreated as symlinks. Returns the number of files written.
pub fn copy_dir_all(src: &Path, dst: &Path) -> io::Result<usize> {
    fs::create_dir_all(dst)?;
    let mut count = 0;
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let from = entry.path();
        let to = dst.join(entry.file_name());
        let ft = entry.file_type()?;
        if ft.is_dir() {
            count += copy_dir_all(&from, &to)?;
        } else if ft.is_symlink() {
            let target = fs::read_link(&from)?;
            let _ = fs::remove_file(&to);
            symlink(&target, &to)?;
            count += 1;
        } else {
            atomic_copy(&from, &to)?;
            count += 1;
        }
    }
    Ok(count)
}

/// Move a directory tree: `rename(2)` when possible, otherwise copy + remove.
pub fn move_dir(src: &Path, dst: &Path) -> io::Result<()> {
    if let Some(parent) = dst.parent() {
        fs::create_dir_all(parent)?;
    }
    match fs::rename(src, dst) {
        Ok(()) => Ok(()),
        Err(_) => {
            copy_dir_all(src, dst)?;
            fs::remove_dir_all(src)
        }
    }
}

/// Replace `dst` (a directory) with `staged` atomically from the caller's point of view:
/// the previous contents are only deleted after the new tree is in place.
pub fn swap_dir(staged: &Path, dst: &Path) -> io::Result<()> {
    let old = tmp_sibling(dst);
    let had_old = dst.exists();
    if had_old {
        fs::rename(dst, &old)?;
    }
    if let Err(e) = fs::rename(staged, dst) {
        if had_old {
            let _ = fs::rename(&old, dst);
        }
        return Err(e);
    }
    if had_old {
        let _ = fs::remove_dir_all(&old);
    }
    Ok(())
}

/// Locate an executable on PATH without spawning `which`.
pub fn find_in_path(bin: &str) -> Option<PathBuf> {
    let paths = env::var_os("PATH")?;
    let names: Vec<String> = if cfg!(windows) {
        vec![bin.to_string(), format!("{bin}.exe"), format!("{bin}.cmd"), format!("{bin}.bat")]
    } else {
        vec![bin.to_string()]
    };
    env::split_paths(&paths).flat_map(|d| names.iter().map(move |n| d.join(n))).find(|p| {
        fs::metadata(p)
            .map(|m| {
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    m.is_file() && m.permissions().mode() & 0o111 != 0
                }
                #[cfg(not(unix))]
                {
                    m.is_file()
                }
            })
            .unwrap_or(false)
    })
}

/// Percent-encode everything outside the RFC 3986 unreserved set.
pub fn url_encode(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for b in input.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{:02X}", b));
        }
    }
    out
}

/// Decode `%XX` and `+` (form encoding).
pub fn url_decode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() => {
                let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).ok();
                if let Some(v) = hex.and_then(|h| u8::from_str_radix(h, 16).ok()) {
                    out.push(v);
                    i += 3;
                    continue;
                }
                out.push(b'%');
            }
            b'+' => out.push(b' '),
            c => out.push(c),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Fill `buf` from the system's random number generator.
#[cfg(windows)]
fn fill_random(buf: &mut [u8]) -> bool {
    use windows_sys::Win32::Security::Cryptography::{BCryptGenRandom, BCRYPT_USE_SYSTEM_PREFERRED_RNG};
    // 0 is STATUS_SUCCESS
    unsafe { BCryptGenRandom(std::ptr::null_mut(), buf.as_mut_ptr(), buf.len() as u32, BCRYPT_USE_SYSTEM_PREFERRED_RNG) == 0 }
}

#[cfg(not(windows))]
fn fill_random(buf: &mut [u8]) -> bool {
    use std::io::Read;
    File::open("/dev/urandom").and_then(|mut f| f.read_exact(buf)).is_ok()
}

/// Cryptographically random hex token from the kernel RNG.
pub fn random_token() -> String {
    let mut buf = [0u8; 24];
    if !fill_random(&mut buf) {
        // Extremely unlikely; fall back to clock + pid + address entropy.
        let seed = now_ms() ^ (u64::from(std::process::id()) << 32) ^ (&buf as *const _ as u64);
        for (i, b) in buf.iter_mut().enumerate() {
            *b = (seed.rotate_left(i as u32 * 7) >> (i % 8)) as u8 ^ (i as u8).wrapping_mul(167);
        }
    }
    buf.iter().map(|b| format!("{:02x}", b)).collect()
}

/// Local wall-clock `HH:MM:SS`.
pub fn local_hms() -> String {
    #[cfg(not(unix))]
    {
        // no libc here: UTC is the best portable answer without a time crate
        let s = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0) % 86_400;
        return format!("{:02}:{:02}:{:02}", s / 3600, (s / 60) % 60, s % 60);
    }
    #[cfg(unix)]
    unsafe {
        let t = libc::time(std::ptr::null_mut());
        let mut tm: libc::tm = std::mem::zeroed();
        if libc::localtime_r(&t, &mut tm).is_null() {
            let s = t.rem_euclid(86_400);
            return format!("{:02}:{:02}:{:02}", s / 3600, (s / 60) % 60, s % 60);
        }
        format!("{:02}:{:02}:{:02}", tm.tm_hour, tm.tm_min, tm.tm_sec)
    }
}

/// Kill a whole process tree: SIGKILL to the process group (and, as a fallback, the pid itself);
/// `taskkill /T /F` on Windows.
pub fn kill_process_tree(pid: u32) {
    if pid == 0 {
        return;
    }
    #[cfg(unix)]
    unsafe {
        libc::kill(-(pid as i32), libc::SIGKILL);
        libc::kill(pid as i32, libc::SIGKILL);
    }
    #[cfg(not(unix))]
    {
        let _ = std::process::Command::new("taskkill")
            .args(["/PID", &pid.to_string(), "/T", "/F"])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
    }
}

/// Ask a process to end (`force` = without giving it a chance to clean up).
pub fn terminate_pid(pid: i32, force: bool) {
    #[cfg(unix)]
    unsafe {
        libc::kill(pid, if force { libc::SIGKILL } else { libc::SIGTERM });
    }
    #[cfg(not(unix))]
    {
        let mut c = std::process::Command::new("taskkill");
        c.args(["/PID", &pid.to_string()]);
        if force {
            c.arg("/F");
        }
        let _ = c.stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).status();
    }
}

/// Read at most the last `max_bytes` of a file without loading all of it.
pub fn read_tail(path: &Path, max_bytes: u64) -> io::Result<String> {
    use std::io::{Read, Seek, SeekFrom};
    let mut f = File::open(path)?;
    let len = f.metadata()?.len();
    let start = len.saturating_sub(max_bytes);
    f.seek(SeekFrom::Start(start))?;
    let mut buf = Vec::with_capacity((len - start) as usize);
    f.read_to_end(&mut buf)?;
    let mut s = String::from_utf8_lossy(&buf).into_owned();
    if start > 0 {
        // drop the (probably partial) first line
        if let Some(nl) = s.find('\n') {
            s = s[nl + 1..].to_string();
        }
    }
    Ok(s)
}

#[cfg(test)]
pub mod testutil {
    use super::*;

    /// Self-cleaning scratch directory for tests.
    pub struct TempDir(pub PathBuf);
    impl TempDir {
        pub fn new(tag: &str) -> Self {
            let p = env::temp_dir().join(format!("rigtest_{}_{}", tag, unique_token()));
            fs::create_dir_all(&p).unwrap();
            TempDir(p)
        }
        pub fn path(&self) -> &Path {
            &self.0
        }
        pub fn join(&self, s: &str) -> PathBuf {
            self.0.join(s)
        }
    }
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::testutil::TempDir;
    use super::*;

    #[test]
    fn atomic_write_replaces_and_leaves_no_tmp() {
        let d = TempDir::new("aw");
        let f = d.join("a/b/c.txt");
        atomic_write(&f, "one").unwrap();
        atomic_write(&f, "two").unwrap();
        assert_eq!(fs::read_to_string(&f).unwrap(), "two");
        let leftovers: Vec<_> = fs::read_dir(f.parent().unwrap())
            .unwrap()
            .flatten()
            .filter(|e| is_tmp_path(&e.path()))
            .collect();
        assert!(leftovers.is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn private_files_are_owner_only_and_replace_atomically() {
        use std::os::unix::fs::PermissionsExt;
        let d = TempDir::new("priv");
        let f = d.join("cfg/secret");
        atomic_write_private(&f, "one").unwrap();
        atomic_write_private(&f, "two").unwrap();
        assert_eq!(fs::read_to_string(&f).unwrap(), "two");
        assert_eq!(fs::metadata(&f).unwrap().permissions().mode() & 0o777, 0o600);
        assert!(fs::read_dir(f.parent().unwrap()).unwrap().flatten().all(|e| !is_tmp_path(&e.path())));
    }

    #[test]
    fn move_and_copy_semantics() {
        let d = TempDir::new("mv");
        let s = d.join("src.bin");
        fs::write(&s, vec![7u8; 100_000]).unwrap();
        atomic_copy(&s, &d.join("x/copy.bin")).unwrap();
        assert!(s.exists());
        move_file(&s, &d.join("y/moved.bin")).unwrap();
        assert!(!s.exists());
        assert_eq!(fs::read(d.join("y/moved.bin")).unwrap().len(), 100_000);
    }

    #[cfg(unix)]
    #[test]
    fn copy_dir_and_swap() {
        let d = TempDir::new("cd");
        fs::create_dir_all(d.join("s/sub")).unwrap();
        fs::write(d.join("s/sub/f.txt"), "hi").unwrap();
        std::os::unix::fs::symlink("sub", d.join("s/link")).unwrap();
        assert_eq!(copy_dir_all(&d.join("s"), &d.join("t")).unwrap(), 2);
        assert_eq!(fs::read_to_string(d.join("t/sub/f.txt")).unwrap(), "hi");
        assert!(fs::symlink_metadata(d.join("t/link")).unwrap().file_type().is_symlink());

        fs::create_dir_all(d.join("old")).unwrap();
        fs::write(d.join("old/stale"), "x").unwrap();
        swap_dir(&d.join("t"), &d.join("old")).unwrap();
        assert!(d.join("old/sub/f.txt").is_file());
        assert!(!d.join("old/stale").exists());
        assert!(!d.join("t").exists());
    }

    #[test]
    fn url_codec_roundtrip() {
        let raw = "Half Sword & Co/ü?.zip";
        assert_eq!(url_decode(&url_encode(raw)), raw);
        assert_eq!(url_decode("a+b%20c%zz%4"), "a b c%zz%4");
        assert_eq!(url_decode("100%"), "100%");
    }

    #[test]
    fn tail_reads_only_end() {
        let d = TempDir::new("tail");
        let f = d.join("l.log");
        let body: String = (0..5000).map(|i| format!("line {}\n", i)).collect();
        fs::write(&f, &body).unwrap();
        let t = read_tail(&f, 200).unwrap();
        assert!(t.ends_with("line 4999\n"));
        assert!(t.len() <= 200);
        assert!(t.starts_with("line "));
    }

    #[test]
    fn tokens_are_unique_and_hex() {
        let a = random_token();
        let b = random_token();
        assert_ne!(a, b);
        assert_eq!(a.len(), 48);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(unique_token(), unique_token());
    }

    #[test]
    fn the_wine_user_is_steamuser_for_proton_and_the_account_for_plain_wine() {
        let d = TempDir::new("wineuser");
        let user = |p: &Path, n: &str| fs::create_dir_all(p.join("drive_c/users").join(n)).unwrap();
        // no user folder yet: the Proton name, so nothing odd is ever invented
        let none = d.join("none");
        assert_eq!(wine_user_dir_for(&none, "gamer"), none.join("drive_c/users/steamuser"));
        // Proton
        let proton = d.join("proton");
        user(&proton, "steamuser");
        user(&proton, "Public");
        assert_eq!(wine_user_dir_for(&proton, "gamer"), proton.join("drive_c/users/steamuser"));
        // plain Wine (Lutris, Heroic, Bottles): the Linux account, never Public
        let wine = d.join("wine");
        user(&wine, "Public");
        user(&wine, "aaron");
        user(&wine, "gamer");
        assert_eq!(wine_user_dir_for(&wine, "gamer"), wine.join("drive_c/users/gamer"));
        assert_eq!(wine_user_dir_for(&wine, "someone-else"), wine.join("drive_c/users/aaron"), "a lone other name is still better than a path that cannot exist");
        let only_public = d.join("public");
        user(&only_public, "public");
        assert_eq!(wine_user_dir_for(&only_public, ""), only_public.join("drive_c/users/steamuser"));
    }
}
