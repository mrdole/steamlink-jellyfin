//! Cache fetcher: downloads upcoming items to the stick, and serves them (growing) over /cache.
use crate::{UA, log};
use std::collections::{HashMap, VecDeque};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::{Duration, Instant};

/// FAT32 cannot hold a file of 4 GiB or more; items over this are played direct.
pub const FAT_MAX: u64 = 3_900_000_000;
const CHUNK: usize = 64 * 1024;
/// How long /cache waits for a file the fetcher says it is working on to show up.
const APPEAR_WAIT: Duration = Duration::from_secs(5);
/// A Growing file with no new bytes for this long ends the /cache stream (a stalled download).
const IDLE_LIMIT: Duration = Duration::from_secs(60);
const STEP: Duration = Duration::from_millis(100);

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum FileState {
    Missing,
    Growing,
    Complete,
    Failed,
}

#[derive(Debug, Clone)]
pub struct Job {
    /// "<item id>-<start s>.ts"
    pub name: String,
    pub url: String,
    pub byte_rate_cap: u64,
}

/// Bytes/s to fetch at: `fetch_rate` x the stream's own rate; unknown bitrate (0) or a nonsense rate -> 2 MB/s.
pub fn rate_cap(bitrate_bps: u64, fetch_rate: f64) -> u64 {
    if bitrate_bps == 0 || !fetch_rate.is_finite() || fetch_rate <= 0.0 {
        return 2_000_000;
    }
    (fetch_rate * bitrate_bps as f64 / 8.0) as u64
}

/// `[A-Za-z0-9._-]+`, no leading dot.
pub fn valid_name(name: &str) -> bool {
    !name.is_empty() && !name.starts_with('.') && name.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

// ---- process-wide state read by /cache ------------------------------------------------------------------------

fn registry() -> &'static Mutex<HashMap<String, FileState>> {
    static R: OnceLock<Mutex<HashMap<String, FileState>>> = OnceLock::new();
    R.get_or_init(Default::default)
}

fn cache_dir_cell() -> &'static Mutex<Option<PathBuf>> {
    static D: OnceLock<Mutex<Option<PathBuf>>> = OnceLock::new();
    D.get_or_init(Default::default)
}

/// On only while a stick with sljf/ is mounted (usb::poll); off at start, so nothing is written to internal flash.
static CACHE_OFF: AtomicBool = AtomicBool::new(true);

/// Where the fetcher writes (None until a Fetcher exists / after the stick went).
pub fn cache_dir() -> Option<PathBuf> {
    cache_dir_cell().lock().unwrap().clone()
}

/// Off while the stick is absent or rejected and after a write error (until the stick is re-inserted); usb::poll
/// turns it on after a successful mount.
pub fn set_cache_off(off: bool) {
    CACHE_OFF.store(off, Ordering::SeqCst);
}

pub fn cache_off() -> bool {
    CACHE_OFF.load(Ordering::SeqCst)
}

fn set_state(name: &str, st: FileState) {
    let mut r = registry().lock().unwrap();
    if st == FileState::Missing {
        r.remove(name);
    } else {
        r.insert(name.to_string(), st);
    }
}

/// State of `name` as the fetcher knows it; a file on disk that it never heard of (from before a restart) is Complete.
pub fn state_of(name: &str) -> FileState {
    if let Some(s) = registry().lock().unwrap().get(name) {
        return *s;
    }
    match cache_dir() {
        Some(d) if valid_name(name) && d.join(name).is_file() => FileState::Complete,
        _ => FileState::Missing,
    }
}

fn http_head(code: u16) -> String {
    match code {
        200 => "HTTP/1.1 200 OK\r\nContent-Type: video/mp2t\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n".into(),
        c => format!("HTTP/1.1 {c} Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"),
    }
}

/// Streams `name` from `dir` to `out`: the whole file, then (while the fetcher says Growing) whatever gets appended,
/// ending once it is Complete. Writes the 200 head itself; on Err(404) nothing was written (the caller answers).
/// No Content-Length: the body ends when the connection closes. Returns the body bytes sent.
pub fn serve(dir: &Path, name: &str, state: &dyn Fn(&str) -> FileState, out: &mut dyn Write) -> Result<u64, u16> {
    serve_with(dir, name, state, out, IDLE_LIMIT)
}

/// `serve` with the no-growth limit injectable: a Growing file that gets no new bytes for `idle` ends the stream.
pub fn serve_with(dir: &Path, name: &str, state: &dyn Fn(&str) -> FileState, out: &mut dyn Write, idle: Duration) -> Result<u64, u16> {
    if !valid_name(name) {
        return Err(404);
    }
    let part = dir.join(format!("{name}.part"));
    let full = dir.join(name);
    let start = Instant::now();
    let mut file = loop {
        match state(name) {
            FileState::Missing | FileState::Failed => return Err(404),
            st => {
                if let Ok(f) = std::fs::File::open(&full).or_else(|_| std::fs::File::open(&part)) {
                    break f;
                }
                // Complete but absent: gone. Growing but not created yet: give it a moment.
                if st == FileState::Complete || start.elapsed() > APPEAR_WAIT {
                    return Err(404);
                }
                std::thread::sleep(STEP);
            }
        }
    };
    if out.write_all(http_head(200).as_bytes()).is_err() {
        return Ok(0);
    }
    let mut sent = 0u64;
    let mut buf = vec![0u8; CHUNK];
    let mut last_growth = Instant::now();
    loop {
        let st = state(name); // read before the file so data written just before it flipped is not missed
        let n = file.read(&mut buf).unwrap_or(0);
        if n > 0 {
            if out.write_all(&buf[..n]).is_err() || out.flush().is_err() {
                return Ok(sent); // client went away
            }
            sent += n as u64;
            last_growth = Instant::now();
            continue;
        }
        match st {
            FileState::Growing if last_growth.elapsed() < idle => std::thread::sleep(STEP),
            FileState::Growing => return Ok(sent), // stalled
            _ => return Ok(sent), // Complete, or failed/cancelled: the stream just ends
        }
    }
}

// ---- the fetcher -----------------------------------------------------------------------------------------------

#[derive(Default)]
struct Queue {
    jobs: VecDeque<Job>,
    /// Name being downloaded now.
    active: Option<String>,
    /// Set by `want` when the active job is no longer wanted.
    cancel: bool,
}

struct Inner {
    q: Mutex<Queue>,
    cv: Condvar,
    dir: PathBuf,
    headers: String,
    agent: ureq::Agent,
    /// Largest file the stick can hold (FAT_MAX on FAT, else unlimited).
    max_file: AtomicU64,
}

pub struct Fetcher {
    inner: Arc<Inner>,
}

enum End {
    Done,
    Cancelled,
    /// Network / HTTP trouble: this job failed, the cache is fine.
    Net(String),
    /// Too big for this stick (FAT): this job only is skipped.
    TooBig,
    /// Could not write to the stick: cache off until re-insert.
    Disk(String),
}

impl Fetcher {
    /// `headers` is the Authorization header value (cfg.auth()).
    pub fn new(cache_dir: PathBuf, headers: String) -> Fetcher {
        *cache_dir_cell().lock().unwrap() = Some(cache_dir.clone());
        // Same agent construction as Api in main.rs (webpki roots via rustls, the Link's system CAs are too old);
        // no global timeout (items are gigabytes long), but a stalled read times out after 30 s.
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .timeout_connect(Some(Duration::from_secs(15)))
            .timeout_recv_response(Some(Duration::from_secs(60)))
            .timeout_per_read(Some(Duration::from_secs(30)))
            .timeout_global(None)
            .http_status_as_error(false)
            .user_agent(UA)
            .build()
            .into();
        let inner = Arc::new(Inner { q: Mutex::new(Queue::default()), cv: Condvar::new(), dir: cache_dir, headers, agent, max_file: AtomicU64::new(u64::MAX) });
        let w = inner.clone();
        std::thread::spawn(move || worker(&w));
        Fetcher { inner }
    }

    /// Largest file the stick takes: `FAT_MAX` for a FAT stick, `u64::MAX` otherwise. Bigger items are skipped
    /// (state Failed for that job only, the cache stays on) and played direct.
    pub fn set_max_file(&self, max: u64) {
        self.inner.max_file.store(max, Ordering::SeqCst);
    }

    /// Replaces the queue (a flip drops the old channel's jobs); cancels the active download if it is not in `jobs`.
    /// The caller must keep the item currently playing from /cache in `in_use` for `usb::evict` (`wanted()` does not
    /// list it once its download is done).
    pub fn want(&self, jobs: Vec<Job>) {
        let mut q = self.inner.q.lock().unwrap();
        q.cancel = match &q.active {
            Some(a) => !jobs.iter().any(|j| &j.name == a),
            None => false,
        };
        let active = q.active.clone();
        q.jobs = jobs.into_iter().filter(|j| Some(&j.name) != active.as_ref()).collect();
        self.inner.cv.notify_all();
    }

    pub fn state(&self, name: &str) -> FileState {
        state_of(name)
    }

    /// Names of the files being used right now (queue + active) that eviction must keep.
    pub fn wanted(&self) -> Vec<String> {
        let q = self.inner.q.lock().unwrap();
        q.jobs.iter().map(|j| j.name.clone()).chain(q.active.clone()).collect()
    }
}

fn worker(inner: &Inner) {
    loop {
        let job = {
            let mut q = inner.q.lock().unwrap();
            loop {
                if !cache_off() {
                    if let Some(j) = q.jobs.pop_front() {
                        q.active = Some(j.name.clone());
                        q.cancel = false;
                        break j;
                    }
                }
                q = inner.cv.wait_timeout(q, Duration::from_secs(5)).unwrap().0;
            }
        };
        run_job(inner, &job);
        inner.q.lock().unwrap().active = None;
    }
}

fn run_job(inner: &Inner, job: &Job) {
    if !valid_name(&job.name) {
        return;
    }
    if cache_off() || !dir_ready(&inner.dir) {
        return; // no verified stick: never write (the caller wants it again later)
    }
    let full = inner.dir.join(&job.name);
    let part = inner.dir.join(format!("{}.part", job.name));
    if full.is_file() {
        set_state(&job.name, FileState::Complete);
        return;
    }
    if registry().lock().unwrap().get(&job.name) == Some(&FileState::Failed) {
        return; // not retried until a flip/restart forgets it
    }
    set_state(&job.name, FileState::Growing);
    match download(inner, job, &part) {
        End::Done => match std::fs::rename(&part, &full) {
            Ok(()) => set_state(&job.name, FileState::Complete),
            Err(e) => disk_failed(&job.name, &part, &e.to_string()),
        },
        End::Cancelled => {
            let _ = std::fs::remove_file(&part);
            set_state(&job.name, FileState::Missing);
        }
        End::Net(e) => {
            log(&format!("fetch: {} failed: {e}", job.name));
            let _ = std::fs::remove_file(&part);
            set_state(&job.name, FileState::Failed);
        }
        End::TooBig => {
            log(&format!("fetch: {} too big for FAT, played direct", job.name));
            let _ = std::fs::remove_file(&part);
            set_state(&job.name, FileState::Failed);
        }
        End::Disk(e) => disk_failed(&job.name, &part, &e),
    }
}

/// EFBIG: the file hit the filesystem's size limit (a per-job problem, not a broken stick).
fn is_efbig(e: &std::io::Error) -> bool {
    e.raw_os_error() == Some(27)
}

/// True when an item of (announced) size `len` cannot be cached under `max`.
pub fn too_big(len: Option<u64>, max: u64) -> bool {
    len.is_some_and(|l| l > max)
}

/// The stick is mounted when MOUNT's device differs from its parent directory's (/mnt tmpfs).
pub fn on_stick(mount_dev: u64, parent_dev: u64) -> bool {
    mount_dev != parent_dev
}

/// `dir` (MOUNT/sljf/cache) may be written: MOUNT is a real mount and `dir`'s parent (sljf/) exists on it.
/// Creates only the cache/ subdir itself, never anything above it.
fn dir_ready(dir: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    let mount = Path::new(crate::usb::MOUNT);
    let dev = |p: &Path| std::fs::metadata(p).ok().map(|m| m.dev());
    let (Some(md), Some(pd)) = (dev(mount), mount.parent().and_then(dev)) else { return false };
    if !on_stick(md, pd) {
        return false;
    }
    let Some(parent) = dir.parent() else { return false };
    if dev(parent) != Some(md) || !parent.is_dir() {
        return false;
    }
    if std::fs::symlink_metadata(dir).is_err() && std::fs::create_dir(dir).is_err() {
        return false;
    }
    // the cache dir itself: a real directory on the stick (not a symlink out of it)
    std::fs::symlink_metadata(dir).is_ok_and(|m| m.file_type().is_dir() && m.dev() == md)
}

/// A stick was mounted: forget failed jobs (retry them) and delete orphaned .part files from an interrupted run.
pub fn on_mount(dir: &Path) {
    clear_failed();
    remove_orphan_parts(dir);
}

pub fn clear_failed() {
    registry().lock().unwrap().retain(|_, s| *s != FileState::Failed);
}

pub fn remove_orphan_parts(dir: &Path) {
    for e in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        if e.file_name().to_string_lossy().ends_with(".part") {
            let _ = std::fs::remove_file(e.path());
        }
    }
}

fn disk_failed(name: &str, part: &Path, e: &str) {
    log(&format!("fetch: write error on {name}: {e}; cache off until the stick is re-inserted"));
    let _ = std::fs::remove_file(part);
    set_state(name, FileState::Failed);
    set_cache_off(true);
}

fn cancelled(inner: &Inner) -> bool {
    inner.q.lock().unwrap().cancel
}

fn download(inner: &Inner, job: &Job, part: &Path) -> End {
    let resp = match inner.agent.get(&job.url).header("Authorization", &inner.headers).call() {
        Ok(r) => r,
        Err(e) => return End::Net(e.to_string()),
    };
    let code = resp.status().as_u16();
    if code >= 300 {
        return End::Net(format!("http {code}"));
    }
    let expect: Option<u64> = resp.headers().get("content-length").and_then(|v| v.to_str().ok()).and_then(|s| s.parse().ok());
    if too_big(expect, inner.max_file.load(Ordering::SeqCst)) {
        return End::TooBig;
    }
    let mut body = resp.into_body();
    let mut rd = body.as_reader();
    let mut f = match std::fs::File::create(part) {
        Ok(f) => f,
        Err(e) => return End::Disk(e.to_string()),
    };
    let mut buf = vec![0u8; CHUNK];
    let mut total = 0u64;
    let t0 = Instant::now();
    loop {
        if cancelled(inner) {
            return End::Cancelled;
        }
        let n = match rd.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) => return End::Net(e.to_string()),
        };
        if total + n as u64 > inner.max_file.load(Ordering::SeqCst) {
            return End::TooBig; // length was not announced
        }
        if let Err(e) = f.write_all(&buf[..n]) {
            return if is_efbig(&e) { End::TooBig } else { End::Disk(e.to_string()) };
        }
        total += n as u64;
        // keep bytes/elapsed <= cap, checking cancel while we wait
        let cap = job.byte_rate_cap.max(1);
        while (total as f64) / t0.elapsed().as_secs_f64().max(1e-3) > cap as f64 {
            if cancelled(inner) {
                return End::Cancelled;
            }
            let ahead = (total as f64 / cap as f64 - t0.elapsed().as_secs_f64()).max(0.0);
            std::thread::sleep(Duration::from_secs_f64(ahead.min(0.1)));
        }
    }
    if let Some(e) = expect {
        if e != total {
            return End::Net(format!("short body: {total} of {e} bytes"));
        }
    }
    if let Err(e) = f.sync_all() {
        return End::Disk(e.to_string());
    }
    End::Done
}

/// Names in `dir` that are finished cache files with their size and mtime (for `usb::evict`); .part files excluded.
pub fn list_cache(dir: &Path) -> Vec<(String, u64, u64)> {
    let mut v = Vec::new();
    for e in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        if name.ends_with(".part") || !valid_name(&name) {
            continue;
        }
        if let Ok(md) = e.metadata() {
            let m = md.modified().ok().and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map(|d| d.as_secs()).unwrap_or(0);
            v.push((name, md.len(), m));
        }
    }
    v
}

/// Deletes evicted files and forgets them.
pub fn remove_files(dir: &Path, names: &[String]) {
    let mut r = registry().lock().unwrap();
    for n in names {
        let _ = std::fs::remove_file(dir.join(n));
        r.remove(n);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU8, Ordering};

    fn tmp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("sljf-fetch-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn rate_cap_values() {
        assert_eq!(rate_cap(8_000_000, 2.0), 2_000_000);
        assert_eq!(rate_cap(0, 2.0), 2_000_000);
        assert_eq!(rate_cap(4_000_000, 3.0), 1_500_000);
        assert_eq!(rate_cap(8_000_000, 0.0), 2_000_000); // nonsense rate: fall back too
    }

    #[test]
    fn cache_name_rejects() {
        assert!(valid_name("abc123-0.ts"));
        assert!(valid_name("a_b-c.9.ts"));
        assert!(!valid_name("../x"));
        assert!(!valid_name("a/b"));
        assert!(!valid_name(".hidden"));
        assert!(!valid_name(""));
        assert!(!valid_name("a b"));
        assert!(!valid_name("a\\b"));
        let mut out = Vec::new();
        assert_eq!(serve(Path::new("/nonexistent"), "../x", &|_| FileState::Complete, &mut out), Err(404));
        assert_eq!(serve(Path::new("/nonexistent"), "a/b", &|_| FileState::Complete, &mut out), Err(404));
        assert!(out.is_empty());
    }

    #[test]
    fn cache_serve_missing_file_404() {
        let d = tmp("missing");
        let mut out = Vec::new();
        assert_eq!(serve(&d, "nope.ts", &|_| FileState::Missing, &mut out), Err(404));
        assert_eq!(serve(&d, "nope.ts", &|_| FileState::Failed, &mut out), Err(404));
        assert_eq!(serve(&d, "nope.ts", &|_| FileState::Complete, &mut out), Err(404)); // claimed complete but absent
        assert!(out.is_empty());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn cache_serve_complete_file() {
        let d = tmp("complete");
        std::fs::write(d.join("a.ts"), b"hello world").unwrap();
        let mut out = Vec::new();
        assert_eq!(serve(&d, "a.ts", &|_| FileState::Complete, &mut out), Ok(11));
        let s = String::from_utf8_lossy(&out).into_owned();
        assert!(s.starts_with("HTTP/1.1 200"), "{s}");
        assert!(!s.contains("Content-Length"));
        assert!(s.contains("Connection: close"));
        assert!(s.ends_with("hello world"));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn cache_serve_stalled_growing_file_ends() {
        let d = tmp("stall");
        std::fs::write(d.join("s.ts.part"), b"abc").unwrap();
        let mut out = Vec::new();
        let t = Instant::now();
        let r = serve_with(&d, "s.ts", &|_| FileState::Growing, &mut out, Duration::from_millis(400));
        assert_eq!(r, Ok(3));
        assert!(t.elapsed() < Duration::from_secs(3));
        assert!(out.ends_with(b"abc"));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn fat_size_decision() {
        assert!(too_big(Some(FAT_MAX + 1), FAT_MAX));
        assert!(!too_big(Some(FAT_MAX), FAT_MAX));
        assert!(!too_big(None, FAT_MAX));
        assert!(!too_big(Some(u64::MAX - 1), u64::MAX));
        assert!(is_efbig(&std::io::Error::from_raw_os_error(27)));
        assert!(!is_efbig(&std::io::Error::from_raw_os_error(28)));
    }

    #[test]
    fn stick_detection_and_failed_clearing() {
        assert!(on_stick(5, 2));
        assert!(!on_stick(2, 2)); // MOUNT is just a dir on /mnt
        assert!(!dir_ready(Path::new("/nonexistent/sljf/cache")));
        set_state("fc-fail.ts", FileState::Failed);
        set_state("fc-grow.ts", FileState::Growing);
        clear_failed();
        assert_eq!(registry().lock().unwrap().get("fc-fail.ts"), None);
        assert_eq!(registry().lock().unwrap().get("fc-grow.ts"), Some(&FileState::Growing));
        set_state("fc-grow.ts", FileState::Missing);
        let d = tmp("orphan");
        std::fs::write(d.join("x.ts.part"), b"1").unwrap();
        std::fs::write(d.join("y.ts"), b"2").unwrap();
        on_mount(&d);
        assert!(!d.join("x.ts.part").exists() && d.join("y.ts").exists());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn cache_serve_growing_file() {
        let d = tmp("growing");
        let st = Arc::new(AtomicU8::new(1)); // 1 growing, 2 complete
        let (d2, st2) = (d.clone(), st.clone());
        let writer = std::thread::spawn(move || {
            let part = d2.join("g.ts.part");
            for chunk in ["aaaa", "bbbb", "cc"] {
                let mut f = std::fs::OpenOptions::new().create(true).append(true).open(&part).unwrap();
                f.write_all(chunk.as_bytes()).unwrap();
                drop(f);
                std::thread::sleep(std::time::Duration::from_millis(250));
            }
            std::fs::rename(&part, d2.join("g.ts")).unwrap();
            st2.store(2, Ordering::SeqCst);
        });
        let mut out = Vec::new();
        let state = |_: &str| if st.load(Ordering::SeqCst) == 2 { FileState::Complete } else { FileState::Growing };
        assert_eq!(serve(&d, "g.ts", &state, &mut out), Ok(10));
        writer.join().unwrap();
        let s = String::from_utf8_lossy(&out).into_owned();
        assert!(s.ends_with("aaaabbbbcc"), "{s}");
        let _ = std::fs::remove_dir_all(&d);
    }
}
