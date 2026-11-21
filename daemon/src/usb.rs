//! USB stick on the Link: mount, cache budget, eviction, and the clip scan for bumpers/commercials.
use crate::chan::PoolKind;
use crate::filler::{Clip, ClipLoc, probe_line};
use crate::log;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::sync::mpsc;
use std::time::{Duration, UNIX_EPOCH};

pub const MOUNT: &str = "/mnt/sljf-usb";
const DEV_SYS: &str = "/sys/block/sda";
const DEV: &str = "/dev/block/sda1";
const GIB: u64 = 1 << 30;
const PROBE_TIMEOUT: Duration = Duration::from_secs(20);
/// Free space is re-read every this many polls (5 s each).
const FREE_EVERY: u32 = 12;

pub struct Stick {
    /// MOUNT/sljf
    pub root: PathBuf,
    pub free_bytes: u64,
    pub fat: bool,
}

/// Poll bookkeeping that is not part of the public state.
struct Aux {
    /// The stick in sda has no sljf/ dir: left unmounted until it is pulled out.
    rejected: bool,
    polls: u32,
}
static AUX: Mutex<Aux> = Mutex::new(Aux { rejected: false, polls: 0 });

/// Filesystem type of the entry mounted at `mount`, from /proc/mounts text.
fn parse_mounts(text: &str, mount: &str) -> Option<String> {
    text.lines().find_map(|l| {
        let mut it = l.split_whitespace();
        let (_dev, mp, ty) = (it.next()?, it.next()?, it.next()?);
        (mp == mount).then(|| ty.to_string())
    })
}

fn mounted_type() -> Option<String> {
    parse_mounts(&std::fs::read_to_string("/proc/mounts").ok()?, MOUNT)
}

/// Free bytes at MOUNT via busybox `df -k`.
fn free_bytes() -> Option<u64> {
    let out = Command::new("df").args(["-k", MOUNT]).stderr(Stdio::null()).output().ok()?;
    let text = String::from_utf8_lossy(&out.stdout).into_owned();
    // last line: Filesystem 1K-blocks Used Available Use% Mounted-on
    let f: Vec<&str> = text.lines().last()?.split_whitespace().collect();
    let n = f.len();
    if n < 4 {
        return None;
    }
    f[n - 3].parse::<u64>().ok().map(|k| k * 1024)
}

fn umount(lazy: bool) {
    let mut c = Command::new("umount");
    if lazy {
        c.arg("-l");
    }
    let _ = c.arg(MOUNT).stdout(Stdio::null()).stderr(Stdio::null()).status();
}

fn try_mount() -> Option<String> {
    let _ = std::fs::create_dir_all(MOUNT);
    for ty in ["vfat", "ext4", "ext3", "ext2"] {
        let ok = Command::new("mount")
            .args(["-t", ty, "-o", "noatime", DEV, MOUNT])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if ok {
            return Some(ty.to_string());
        }
    }
    None
}

/// Called every 5 s: mounts a stick that appeared (kept only if it has a top-level sljf/ dir), drops one that went.
/// Cheap when nothing changed: one stat, and free space is re-read once a minute.
pub fn poll(state: &mut Option<Stick>) {
    let mut aux = AUX.lock().unwrap();
    if !Path::new(DEV_SYS).exists() {
        if state.take().is_some() {
            log("usb: stick removed");
            umount(true);
        } else if mounted_type().is_some() {
            umount(true); // leftover mount from before a restart
        }
        aux.rejected = false;
        aux.polls = 0;
        crate::fetch::set_cache_off(true); // no stick: nothing may be written (re-enabled by the next good mount)
        return;
    }
    if let Some(s) = state.as_mut() {
        aux.polls += 1;
        if aux.polls % FREE_EVERY == 0 {
            if let Some(f) = free_bytes() {
                s.free_bytes = f;
            }
        }
        return;
    }
    if aux.rejected {
        return;
    }
    // adopt a mount left by a previous run, else mount now
    let ty = match mounted_type() {
        Some(t) => t,
        None => match try_mount() {
            Some(t) => t,
            None => {
                log("usb: stick present but not mountable (vfat/ext2/3/4 only)");
                aux.rejected = true;
                crate::fetch::set_cache_off(true);
                return;
            }
        },
    };
    let root = Path::new(MOUNT).join("sljf");
    if !root.is_dir() {
        log("usb: stick has no sljf/ folder, ignored");
        umount(false);
        aux.rejected = true;
        crate::fetch::set_cache_off(true);
        return;
    }
    let free = free_bytes().unwrap_or(0);
    log(&format!("usb: stick mounted ({ty}, {} MB free)", free / 1_000_000));
    aux.polls = 0;
    crate::fetch::on_mount(&root.join("cache")); // retry failed jobs, drop orphaned .part files
    crate::fetch::set_cache_off(false);
    *state = Some(Stick { root, free_bytes: free, fat: ty == "vfat" });
}

/// Bytes the cache may use: `cache_gb` GiB when configured (never more than is free), else half the free space, max 32 GiB.
pub fn cache_budget(free_bytes: u64, cache_gb: Option<u32>) -> u64 {
    match cache_gb {
        Some(g) => (g as u64 * GIB).min(free_bytes),
        None => (free_bytes / 2).min(32 * GIB),
    }
}

/// Picks the oldest (mtime) files that are not in use until the rest fits `budget`; removes them from `files` and
/// returns their names (the caller deletes them). The caller must put the item currently playing from /cache into
/// `in_use` (the fetcher's `wanted()` only lists queued and downloading files).
pub fn evict(files: &mut Vec<(String, u64, u64)>, budget: u64, in_use: &[String]) -> Vec<String> {
    let mut total: u64 = files.iter().map(|f| f.1).sum();
    if total <= budget {
        return Vec::new();
    }
    let mut order: Vec<usize> = (0..files.len()).filter(|&i| !in_use.contains(&files[i].0)).collect();
    order.sort_by_key(|&i| files[i].2);
    let mut gone = Vec::new();
    for i in order {
        if total <= budget {
            break;
        }
        total -= files[i].1;
        gone.push(files[i].0.clone());
    }
    files.retain(|f| !gone.contains(&f.0));
    gone
}

/// Runs `<player> --probe <file>` (20 s limit) and returns its first stdout line that starts with "probe ".
pub fn run_probe(player: &str, file: &Path) -> Option<String> {
    use std::io::{BufRead, BufReader};
    let mut child = Command::new(player).arg("--probe").arg(file).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::null()).spawn().ok()?;
    let out = child.stdout.take()?;
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        for l in BufReader::new(out).lines().map_while(Result::ok) {
            if l.starts_with("probe ") {
                let _ = tx.send(l);
                return;
            }
        }
    });
    let got = rx.recv_timeout(PROBE_TIMEOUT).ok();
    let _ = child.kill();
    let _ = child.wait();
    got
}

const EXTS: [&str; 4] = ["mp4", "mkv", "ts", "m4v"];

fn is_video(p: &Path) -> bool {
    p.extension().and_then(|e| e.to_str()).map(|e| EXTS.iter().any(|x| e.eq_ignore_ascii_case(x))).unwrap_or(false)
}

fn sorted_entries(dir: &Path) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = std::fs::read_dir(dir).map(|r| r.flatten().map(|e| e.path()).collect()).unwrap_or_default();
    v.sort();
    v
}

/// Scans sljf/{bumpers,commercials}/{all,<N>}/ on the stick. `known` (clips.json) maps "relpath|size|mtime" to
/// {"secs":f} or {"bad":true}; unchanged files reuse it, new/changed ones are probed (a probe that fails or gives
/// nothing is bad, logged once). `known` is rewritten to the files seen. `stick_root` is the mount point
/// (containing sljf/) or the sljf/ dir itself.
pub fn scan_clips(stick_root: &Path, probe: &dyn Fn(&Path) -> Option<String>, known: &mut Value) -> Vec<(PoolKind, Option<u32>, Clip)> {
    let base = if stick_root.join("sljf").is_dir() { stick_root.join("sljf") } else { stick_root.to_path_buf() };
    let old = known.as_object().cloned().unwrap_or_default();
    let mut fresh = serde_json::Map::new();
    let mut out = Vec::new();
    for (sub, kind) in [("bumpers", PoolKind::Bumpers), ("commercials", PoolKind::Commercials)] {
        for dir in sorted_entries(&base.join(sub)) {
            if !dir.is_dir() {
                continue;
            }
            let dname = dir.file_name().and_then(|n| n.to_str()).unwrap_or("");
            let number = if dname == "all" {
                None
            } else if let Ok(n) = dname.parse::<u32>() {
                Some(n)
            } else {
                continue;
            };
            for f in sorted_entries(&dir) {
                if !f.is_file() || !is_video(&f) {
                    continue;
                }
                let Ok(md) = f.metadata() else { continue };
                let mtime = md.modified().ok().and_then(|t| t.duration_since(UNIX_EPOCH).ok()).map(|d| d.as_secs()).unwrap_or(0);
                let rel = f.strip_prefix(&base).unwrap_or(&f).to_string_lossy().into_owned();
                let key = format!("{rel}|{}|{mtime}", md.len());
                let entry = match old.get(&key) {
                    Some(e) => e.clone(),
                    None => match probe(&f).as_deref().and_then(probe_line) {
                        Some((secs, ..)) => json!({"secs": secs}),
                        None => {
                            log(&format!("usb: unusable clip {rel}, skipped"));
                            json!({"bad": true})
                        }
                    },
                };
                if let Some(secs) = entry["secs"].as_f64() {
                    out.push((kind, number, Clip { loc: ClipLoc::Stick(f.to_string_lossy().into_owned()), secs }));
                }
                fresh.insert(key, entry);
            }
        }
    }
    *known = Value::Object(fresh);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::filler::ClipLoc;
    use serde_json::json;
    use std::cell::RefCell;

    const GIB: u64 = 1 << 30;

    #[test]
    fn budget_auto_half_capped() {
        assert_eq!(cache_budget(10 * GIB, None), 5 * GIB);
        assert_eq!(cache_budget(100 * GIB, None), 32 * GIB);
        assert_eq!(cache_budget(0, None), 0);
        assert_eq!(cache_budget(100 * GIB, Some(8)), 8 * GIB);
        assert_eq!(cache_budget(4 * GIB, Some(8)), 4 * GIB); // never more than is free
        assert_eq!(cache_budget(100 * GIB, Some(0)), 0);
    }

    #[test]
    fn evict_oldest_not_in_use() {
        let mut f = vec![("a".to_string(), 10, 300), ("b".to_string(), 10, 100), ("c".to_string(), 10, 200), ("d".to_string(), 10, 50)];
        // d is oldest but in use; b goes first, then c
        let gone = evict(&mut f, 25, &["d".to_string()]);
        assert_eq!(gone, vec!["b", "c"]);
        let names: Vec<_> = f.iter().map(|x| x.0.as_str()).collect();
        assert_eq!(names, vec!["a", "d"]);
        // already fits: nothing deleted
        assert!(evict(&mut f, 100, &[]).is_empty());
        // everything in use: nothing can go even though over budget
        let mut g = vec![("x".to_string(), 50, 1)];
        assert!(evict(&mut g, 10, &["x".to_string()]).is_empty());
    }

    fn tmp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("sljf-usb-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn touch(p: &Path, body: &str) {
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, body).unwrap();
    }

    #[test]
    fn scan_finds_probes_once_and_reuses() {
        let root = tmp("scan");
        let b = root.join("sljf/bumpers");
        let c = root.join("sljf/commercials");
        touch(&b.join("all/one.MP4"), "1");
        touch(&b.join("5/two.mkv"), "22");
        touch(&b.join("all/notes.txt"), "x");
        touch(&b.join("weird/skip.mp4"), "x");
        touch(&c.join("all/bad.ts"), "bad");
        touch(&c.join("12/ad.m4v"), "4444");
        let calls = RefCell::new(Vec::<String>::new());
        let probe = |p: &Path| {
            calls.borrow_mut().push(p.file_name().unwrap().to_string_lossy().into_owned());
            if p.ends_with("bad.ts") { Some("probe bad hevc".to_string()) } else { Some("probe ok 10.5 640x360 h264 aac".to_string()) }
        };
        let mut known = json!({});
        let got = scan_clips(&root, &probe, &mut known);
        assert_eq!(calls.borrow().len(), 4); // txt and weird/ never probed
        assert_eq!(got.len(), 3);
        assert!(got.iter().any(|(k, n, cl)| *k == PoolKind::Bumpers && n.is_none() && cl.secs == 10.5 && matches!(&cl.loc, ClipLoc::Stick(s) if s.ends_with("all/one.MP4"))));
        assert!(got.iter().any(|(k, n, _)| *k == PoolKind::Bumpers && *n == Some(5)));
        assert!(got.iter().any(|(k, n, _)| *k == PoolKind::Commercials && *n == Some(12)));
        // second scan: all four results (including the bad one) come from `known`
        calls.borrow_mut().clear();
        let again = scan_clips(&root, &probe, &mut known);
        assert!(calls.borrow().is_empty());
        assert_eq!(again.len(), 3);
        // a changed file (size) is probed again; a vanished file leaves known
        touch(&b.join("5/two.mkv"), "222222");
        std::fs::remove_file(c.join("12/ad.m4v")).unwrap();
        let third = scan_clips(&root, &probe, &mut known);
        assert_eq!(calls.borrow().as_slice(), ["two.mkv"]);
        assert_eq!(third.len(), 2);
        assert_eq!(known.as_object().unwrap().len(), 3);
        // a probe that gives nothing (timeout) counts as bad, once
        touch(&b.join("all/hang.mp4"), "h");
        calls.borrow_mut().clear();
        let nothing = |_: &Path| None;
        let r = scan_clips(&root, &nothing, &mut known);
        assert_eq!(r.len(), 2);
        let r2 = scan_clips(&root, &|_: &Path| panic!("reprobed"), &mut known);
        assert_eq!(r2.len(), 2);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn scan_without_dirs_is_empty() {
        let root = tmp("scan-empty");
        let mut known = json!({});
        assert!(scan_clips(&root, &|_: &Path| None, &mut known).is_empty());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn run_probe_takes_first_probe_line_and_times_out_on_missing() {
        let dir = tmp("probe");
        let script = dir.join("fake.sh");
        std::fs::write(&script, "#!/bin/sh\necho noise\necho \"probe ok 3.0 10x10 h264 aac $2\"\n").unwrap();
        std::process::Command::new("chmod").arg("+x").arg(&script).status().unwrap();
        let got = run_probe(script.to_str().unwrap(), Path::new("f.mp4")).unwrap();
        assert!(got.starts_with("probe ok 3.0"), "{got}");
        assert!(run_probe("/nonexistent/player", Path::new("f.mp4")).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
