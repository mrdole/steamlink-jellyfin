//! Box channels in the daemon: the glue between the engine (engine.rs, pure) and the Player (main.rs).
//!
//!   lineup   the day's snapshot (chan.rs) loaded in a thread at the schedule day start (local time via localtime_r)
//!            or first need; a failed load keeps the old one and is retried in 10 min. Shared with local.rs for
//!            /home, /guide, /image?id=ch:N, /play?id=ch:N.
//!   tuning   one `sljf-play --channel` run per channel visit: static at once, the item on air as its `url` line
//!            (joined at its offset), then `next` lines from engine::Feed (two ahead, at most LEAD s early).
//!            Player lines carry the run number they came from, so a flipped-away player never touches the new one.
//!   failures an item that cannot be opened is skipped by the player; a player that exits is started again (the
//!            item on air, rejoined at its offset; an item that failed to open twice is skipped, static meanwhile).
//!   stick    usb::poll every 5 s; clips scanned in a thread; the Fetcher fetches the next items of the channel
//!            (idle: the next item of the last one watched); evicted to the cache budget every minute.
//! No reports to the server for channel items (no watched state, no resume points); no subtitles in 1a.
use crate::engine::{Entry, Event, Feed, Lineup, Next, Resolved, channel_id, entries, slot_filler};
use crate::fetch::{FAT_MAX, FileState, Fetcher, Job, rate_cap};
use crate::filler::{Clip, ClipLoc, pick_pools};
use crate::sched::{ChannelDef, Rules, epoch_for};
use crate::{Cfg, PLAYER_SILENT, Player, RETRY_S, TICKS, Tracks, chan, end_player, fetch, keep_awake, local, log, pick_audio, session_id, take_screen_fast, url_line, usb, video_decodable};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::process::Child;
use std::sync::Arc;
use std::sync::mpsc::{Receiver, Sender, TryRecvError, channel};
use std::time::{Duration, Instant};

/// Clock not set yet (the Link boots at 1970): no schedule.
const CLOCK_SET: i64 = 1_577_836_800;
const RELOAD_RETRY: Duration = Duration::from_secs(600);
const DAY_CHECK: Duration = Duration::from_secs(30);
const STICK_POLL: Duration = Duration::from_secs(5);
const WANT_EVERY: Duration = Duration::from_secs(10);
const EVICT_EVERY: Duration = Duration::from_secs(60);
/// A failed item lookup is not tried again for this long (each try can block up to the Api timeout).
const INFO_RETRY: Duration = Duration::from_secs(60);

// ---- local time ------------------------------------------------------------------------------------------------

#[cfg(target_pointer_width = "64")]
type TimeT = i64;
#[cfg(not(target_pointer_width = "64"))]
type TimeT = i32; // 32-bit glibc / musl: `localtime_r` is the 32-bit time_t symbol (musl 1.2 keeps it for old binaries)

/// struct tm as glibc and musl lay it out (9 ints, long gmtoff, const char *zone); only tm_gmtoff is read.
#[repr(C)]
struct Tm {
    tm_ints: [std::os::raw::c_int; 9],
    tm_gmtoff: std::os::raw::c_long,
    tm_zone: *const std::os::raw::c_char,
    /// room in case a libc's struct is larger than declared here
    _spare: [u64; 8],
}

unsafe extern "C" {
    fn localtime_r(t: *const TimeT, tm: *mut Tm) -> *mut Tm;
}

/// Local time minus UTC (s) at epoch second `t` (the Link's /etc/localtime); 0 if it cannot be read.
pub fn utc_offset(t: i64) -> i64 {
    let tt = t as TimeT;
    let mut tm = std::mem::MaybeUninit::<Tm>::zeroed();
    // SAFETY: localtime_r writes at most one struct tm into the buffer, which is larger than that
    let r = unsafe { localtime_r(&tt, tm.as_mut_ptr()) };
    if r.is_null() {
        return 0;
    }
    // SAFETY: zero-initialised and filled by localtime_r
    let off = unsafe { tm.assume_init() }.tm_gmtoff as i64;
    if off.abs() <= 14 * 3600 { off } else { 0 }
}

/// The schedule rules from the config, in a zone `utc_off` east of UTC.
pub fn rules_for(cfg: &Cfg, utc_off: i64) -> Rules {
    Rules {
        pad: cfg.pad,
        break_secs: if cfg.breaks == "off" { 0 } else { cfg.break_minutes * 60 },
        epoch: epoch_for(cfg.schedule_day_start, utc_off),
    }
}

fn now_f() -> f64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs_f64()).unwrap_or(0.0)
}

/// Cache file of an item fetched from its start.
fn job_name(id: &str) -> String {
    format!("{id}-0.ts")
}

// ---- state -----------------------------------------------------------------------------------------------------

/// What the stream URL of an item needs (looked up once per item).
#[derive(Clone)]
struct Info {
    copy: bool,
    audio: Option<i64>,
    bitrate: u64,
}

/// One `sljf-play --channel` run.
pub struct ChanPlay {
    number: u32,
    /// "10 Classic Sitcoms"
    name: String,
    /// bar title: the name, plus " · <episode>" while one plays
    bar: String,
    child: Option<Child>,
    feed: Feed,
    /// the first item's `url` line, sent at that wall time
    url: Option<(f64, String)>,
    last_line: Instant,
    /// player exits in a row (reset when an item starts)
    exits: u32,
    /// schedule end of the first item that failed to open last time (a second failure skips it)
    fail_key: Option<f64>,
}

pub struct Chans {
    pub lineup: Option<Arc<Lineup>>,
    loading: Option<Receiver<Result<Lineup, String>>>,
    retry_at: Option<Instant>,
    day_check: Instant,
    stick: Option<usb::Stick>,
    stick_at: Instant,
    clips: Vec<(chan::PoolKind, Option<u32>, Clip)>,
    scan: Option<Receiver<Vec<(chan::PoolKind, Option<u32>, Clip)>>>,
    fetcher: Option<Fetcher>,
    want_at: Instant,
    evict_at: Instant,
    info: HashMap<String, Result<Info, Instant>>,
    /// last channel watched (idle fetch-ahead)
    last: Option<u32>,
    run: u64,
    lines_tx: Sender<(u64, String)>,
    lines_rx: Receiver<(u64, String)>,
    pub play: Option<ChanPlay>,
}

impl Chans {
    pub fn new() -> Chans {
        let (lines_tx, lines_rx) = channel();
        let now = Instant::now();
        Chans {
            lineup: None, loading: None, retry_at: None, day_check: now, stick: None, stick_at: now, clips: Vec::new(), scan: None,
            fetcher: None, want_at: now, evict_at: now + EVICT_EVERY, info: HashMap::new(), last: None, run: 0, lines_tx, lines_rx, play: None,
        }
    }
}

/// The channel's entries from `t`, its filler from the stick's clips or the server's pools.
fn chan_entries(clips: &[(chan::PoolKind, Option<u32>, Clip)], l: &Lineup, ch: &ChannelDef, t: i64, n: usize) -> Vec<Entry> {
    let (com, bum) = pick_pools(clips, &l.snap.pools, ch.number);
    let bs = l.rules.break_secs;
    entries(ch, &l.rules, &|s| slot_filler(ch.number, s, &com, &bum, bs), t, n, l.utc_off)
}

fn server_id(n: &Next) -> Option<&str> {
    match n {
        Next::Video { item_id, .. } => Some(item_id),
        Next::Clip { loc: ClipLoc::Server(id), .. } => Some(id),
        _ => None,
    }
}

impl Player {
    fn lineup_channel(&self, number: u32) -> Option<(Arc<Lineup>, usize)> {
        let l = self.ch.lineup.clone()?;
        let i = l.snap.channels.iter().position(|c| c.number == number)?;
        Some((l, i))
    }

    // ---- housekeeping (every tick, playing or not) ---------------------------------------------------------------

    pub fn chan_house(&mut self) {
        self.chan_lineup();
        if self.ch.stick_at.elapsed() >= STICK_POLL {
            self.ch.stick_at = Instant::now();
            self.chan_stick();
        }
        if let Some(rx) = &self.ch.scan {
            match rx.try_recv() {
                Ok(clips) => {
                    log(&format!("usb: {} clips on the stick", clips.len()));
                    self.ch.clips = clips;
                    self.ch.scan = None;
                }
                Err(TryRecvError::Disconnected) => self.ch.scan = None,
                Err(TryRecvError::Empty) => {}
            }
        }
        if self.ch.fetcher.is_some() && self.ch.want_at.elapsed() >= WANT_EVERY {
            self.ch.want_at = Instant::now();
            // re-wanted every time: a job dropped while the cache was off comes back
            let jobs = self.chan_jobs();
            if let Some(f) = &self.ch.fetcher {
                f.want(jobs);
            }
        }
        if self.ch.evict_at.elapsed() >= EVICT_EVERY {
            self.ch.evict_at = Instant::now();
            self.chan_evict();
        }
    }

    /// Load the day's lineup at the schedule day start (or when there is none); apply a finished load.
    fn chan_lineup(&mut self) {
        if let Some(rx) = &self.ch.loading {
            match rx.try_recv() {
                Ok(Ok(l)) => {
                    log(&format!("channels: day {}, {} channels ({})", l.snap.day, l.snap.channels.len(),
                        l.snap.channels.iter().map(|c| format!("{} {}", c.number, c.name)).collect::<Vec<_>>().join(", ")));
                    let l = Arc::new(l);
                    *self.shared.lineup.lock().unwrap() = Some(l.clone());
                    self.ch.lineup = Some(l);
                    self.ch.loading = None;
                    self.ch.retry_at = None;
                }
                Ok(Err(e)) => {
                    log(&format!("channels: {e}; trying again in 10 min"));
                    self.ch.loading = None;
                    self.ch.retry_at = Some(Instant::now() + RELOAD_RETRY);
                }
                Err(TryRecvError::Disconnected) => {
                    self.ch.loading = None;
                    self.ch.retry_at = Some(Instant::now() + RELOAD_RETRY);
                }
                Err(TryRecvError::Empty) => {}
            }
        }
        if !self.cfg.channels || self.ch.loading.is_some() || (self.ch.day_check.elapsed() < DAY_CHECK && self.ch.lineup.is_some()) {
            return;
        }
        if self.ch.retry_at.is_some_and(|t| Instant::now() < t) {
            return;
        }
        self.ch.day_check = Instant::now();
        let now = now_f() as i64;
        if now < CLOCK_SET {
            return;
        }
        let off = utc_offset(now);
        let rules = rules_for(self.cfg, off);
        let day = (now - rules.epoch).div_euclid(86_400);
        if self.ch.lineup.as_ref().is_some_and(|l| l.snap.day == day) {
            return;
        }
        let (tx, rx) = channel();
        let (api, cfg) = (self.api.clone(), self.cfg);
        std::thread::spawn(move || {
            let _ = tx.send(chan::load_or_fetch(&api, cfg, day).map(|snap| Lineup { snap, rules, utc_off: off }));
        });
        self.ch.loading = Some(rx);
    }

    fn chan_stick(&mut self) {
        let had = self.ch.stick.is_some();
        usb::poll(&mut self.ch.stick);
        match (had, self.ch.stick.as_ref()) {
            (false, Some(st)) => {
                if self.ch.fetcher.is_none() {
                    self.ch.fetcher = Some(Fetcher::new(st.root.join("cache"), self.cfg.auth())); // one per process
                }
                if let Some(f) = &self.ch.fetcher {
                    f.set_max_file(if st.fat { FAT_MAX } else { u64::MAX });
                }
                // probing new clips takes up to 20 s each: never on the tick
                let (tx, rx) = channel();
                let (player, root) = (self.cfg.player.clone(), st.root.clone());
                std::thread::spawn(move || {
                    let file = root.join("clips.json");
                    let mut known: Value = std::fs::read_to_string(&file).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or(json!({}));
                    let clips = usb::scan_clips(&root, &|p| usb::run_probe(&player, p), &mut known);
                    if let Err(e) = std::fs::write(&file, known.to_string()) {
                        log(&format!("usb: cannot write clips.json: {e}"));
                    }
                    let _ = tx.send(clips);
                });
                self.ch.scan = Some(rx);
            }
            (true, None) => {
                self.ch.clips.clear();
                self.ch.scan = None;
            }
            _ => {}
        }
    }

    /// Look up what the stream URL of `id` needs (cached; a failure is not retried for INFO_RETRY).
    fn chan_info(&mut self, id: &str) -> Option<Info> {
        match self.ch.info.get(id) {
            Some(Ok(i)) => return Some(i.clone()),
            Some(Err(t)) if t.elapsed() < INFO_RETRY => return None,
            _ => {}
        }
        if self.ch.info.len() > 2000 {
            self.ch.info.clear();
        }
        let r = match self.api.get(&format!("/Users/{}/Items/{id}", self.cfg.user_id)) {
            Ok(item) => Ok(Info {
                copy: video_decodable(&item),
                audio: pick_audio(&item, None, &self.shared.lang()),
                bitrate: item["MediaSources"][0]["Bitrate"].as_u64().unwrap_or(0),
            }),
            Err(e) => {
                log(&format!("channel item {id}: {e}"));
                Err(Instant::now())
            }
        };
        self.ch.info.insert(id.to_string(), r.clone());
        r.ok()
    }

    fn chan_prefetch(&mut self, es: &[Entry]) {
        for e in es {
            if let Some(id) = server_id(&e.next) {
                self.chan_info(id);
            }
        }
    }

    /// Where an item plays from: a stick path, the cache (fetched from its start), or a server stream.
    fn chan_resolve(&self, n: &Next) -> Option<Resolved> {
        let (id, start) = match n {
            Next::Card { .. } => return None,
            Next::Clip { loc: ClipLoc::Stick(p), .. } => {
                return (!p.contains(['\n', '\r'])).then(|| Resolved { url: p.clone(), cache: None, session: None });
            }
            Next::Clip { loc: ClipLoc::Server(id), .. } => (id.as_str(), 0.0),
            Next::Video { item_id, start_secs, .. } => (item_id.as_str(), *start_secs),
        };
        let info = self.ch.info.get(id)?.as_ref().ok()?;
        let name = job_name(id);
        let cached = self.ch.fetcher.as_ref().map(|f| f.state(&name));
        if start < 0.5 && self.ch.stick.is_some() && !fetch::cache_off() && matches!(cached, Some(FileState::Growing | FileState::Complete)) {
            return Some(Resolved { url: format!("http://127.0.0.1:{}/cache?f={name}", local::PORT), cache: Some(name), session: None });
        }
        let session = session_id();
        let tr = Tracks { audio: info.audio, ..Default::default() };
        let url = self.stream_url(id, info.copy, &tr, &session, (start * TICKS) as i64, None);
        Some(Resolved { url, cache: None, session: Some(session) })
    }

    /// Fetch-ahead: the next items of the channel playing (or, idle, the next item of the last one watched).
    fn chan_jobs(&mut self) -> Vec<Job> {
        if self.ch.stick.is_none() {
            return Vec::new();
        }
        let (number, playing) = match &self.ch.play {
            Some(cp) => (cp.number, true),
            None if self.now.is_none() => match self.ch.last {
                Some(n) => (n, false),
                None => return Vec::new(),
            },
            None => return Vec::new(), // a film or episode plays: leave it the bandwidth
        };
        let Some((l, i)) = self.lineup_channel(number) else { return Vec::new() };
        let now = now_f();
        let es = chan_entries(&self.ch.clips, &l, &l.snap.channels[i], now as i64, 8);
        let ids: Vec<String> = es.iter()
            .filter(|e| e.at >= now - 1.0 && (playing || matches!(e.next, Next::Video { .. })))
            .filter_map(|e| server_id(&e.next).map(String::from))
            .take(if playing { 3 } else { 1 })
            .collect();
        let mut jobs = Vec::new();
        for id in ids {
            let Some(info) = self.chan_info(&id) else { continue };
            let tr = Tracks { audio: info.audio, ..Default::default() };
            let url = self.stream_url(&id, info.copy, &tr, &session_id(), 0, None);
            jobs.push(Job { name: job_name(&id), url, byte_rate_cap: rate_cap(info.bitrate, self.cfg.fetch_rate) });
        }
        jobs
    }

    fn chan_evict(&mut self) {
        let (Some(st), Some(f)) = (&self.ch.stick, &self.ch.fetcher) else { return };
        let dir = st.root.join("cache");
        let mut files = fetch::list_cache(&dir);
        let total: u64 = files.iter().map(|f| f.1).sum();
        let budget = usb::cache_budget(st.free_bytes + total, self.cfg.cache_gb);
        let mut in_use = f.wanted();
        if let Some(cp) = &self.ch.play {
            in_use.extend(cp.feed.in_use());
        }
        let gone = usb::evict(&mut files, budget, &in_use);
        if !gone.is_empty() {
            log(&format!("cache: evicted {}", gone.join(" ")));
            fetch::remove_files(&dir, &gone);
        }
    }

    // ---- playing a channel ---------------------------------------------------------------------------------------

    /// Tune channel `number` (from /play?id=ch:N, or a flip).
    pub fn play_channel(&mut self, number: u32) {
        let Some((l, i)) = self.lineup_channel(number) else {
            return log(&format!("channel {number}: not in today's lineup"));
        };
        self.stop(false); // whatever plays, including another channel
        self.queue.clear();
        self.qi = 0;
        self.ch.last = Some(number);
        let name = format!("{number} {}", l.snap.channels[i].name);
        self.shared.channel(&channel_id(number));
        self.shared.playing(true, false, &name);
        take_screen_fast();
        if self.chan_spawn(number, 0.0, 0.0, 0, None) {
            self.chan_osd(5);
        } else {
            self.shared.ended("");
            self.stop(true);
        }
    }

    /// Start a `sljf-play --channel` run on `number`: the item on air (or the first ending after `skip_until`),
    /// due no earlier than `delay` s from now. False if nothing could be started.
    fn chan_spawn(&mut self, number: u32, skip_until: f64, delay: f64, exits: u32, fail_key: Option<f64>) -> bool {
        let Some((l, i)) = self.lineup_channel(number) else { return false };
        let ch = &l.snap.channels[i];
        let name = format!("{number} {}", ch.name);
        self.ch.run += 1;
        let run = self.ch.run;
        let now = now_f();
        let es = chan_entries(&self.ch.clips, &l, ch, now as i64, 10);
        self.chan_prefetch(&es);
        let mut feed = Feed::new(run);
        let Some(first) = feed.first(&es, now + delay, skip_until, &mut |n| self.chan_resolve(n)) else {
            log(&format!("channel {name}: nothing playable"));
            return false;
        };
        let Some(url) = url_line(&first.url) else { return false };
        let mut cmd = self.player_cmd(&Tracks::default(), (first.start_secs * TICKS) as i64, true);
        cmd.arg("--channel").arg(format!("{:.2}", first.limit))
            .env("SLJF_STATIC", self.cfg.static_volume.to_string())
            .env("SLJF_READAHEAD_S", if self.ch.stick.is_some() { "20" } else { "60" });
        let mut child = match cmd.spawn() {
            Ok(c) => c,
            Err(e) => {
                log(&format!("channel {name}: {}: {e}", self.cfg.player));
                return false;
            }
        };
        if let Some(out) = child.stdout.take() {
            let tx = self.ch.lines_tx.clone();
            std::thread::spawn(move || {
                for line in BufReader::new(out).lines().map_while(Result::ok) {
                    if tx.send((run, line)).is_err() {
                        break;
                    }
                }
            });
        }
        log(&format!("channel {name}: run {run}, first item at {:.0} s for {:.0} s{}", first.start_secs, first.limit,
            if first.send_at > now + 1.0 { format!(", static for {:.0} s", first.send_at - now) } else { String::new() }));
        self.ch.play = Some(ChanPlay {
            number, bar: name.clone(), name, child: Some(child), feed, url: Some((first.send_at, url)),
            last_line: Instant::now(), exits, fail_key,
        });
        true
    }

    pub fn chan_write(&mut self, line: &str) {
        if let Some(w) = self.ch.play.as_mut().and_then(|cp| cp.child.as_mut()).and_then(|c| c.stdin.as_mut()) {
            let _ = w.write_all(line.as_bytes()).and_then(|_| w.flush());
        }
    }

    fn chan_osd(&mut self, secs: i32) {
        let hint = self.hint();
        let Some(bar) = self.ch.play.as_ref().map(|cp| cp.bar.replace('\t', " ")) else { return };
        self.chan_write(&format!("osd {secs}\t{bar}\t0\t{hint}\t-1\n"));
    }

    fn end_encode_bg(&self, session: String) {
        let (api, dev) = (self.api.clone(), self.cfg.device_id.clone());
        std::thread::spawn(move || api.delete(&format!("/Videos/ActiveEncodings?DeviceId={dev}&PlaySessionId={session}")));
    }

    /// Stop the channel run (no queue / screen handling: Player::stop does that).
    pub fn chan_stop(&mut self) {
        let Some(mut cp) = self.ch.play.take() else { return };
        if let Some(c) = cp.child.take() {
            end_player(c);
        }
        for s in cp.feed.sessions() {
            self.end_encode_bg(s);
        }
        log(&format!("channel {}: stopped", cp.name));
    }

    /// Remote keys while a channel plays: Left/Right flip, Up/OK the bar, Down the sleep timer, Back / Home stop
    /// (Home: the app opens the guide on this channel).
    pub fn chan_key(&mut self, k: &str) {
        log(&format!("remote {k} on a channel"));
        let Some(number) = self.ch.play.as_ref().map(|cp| cp.number) else { return };
        match k {
            "Back" => {
                self.shared.ended("");
                self.stop(true)
            }
            "GoHome" => {
                self.shared.channel(&channel_id(number));
                self.shared.ended("guide");
                self.stop(true)
            }
            "MoveLeft" | "MoveRight" => self.chan_flip(if k == "MoveRight" { 1 } else { -1 }),
            "MoveUp" | "Select" => self.chan_osd(5),
            "MoveDown" => {
                self.cycle_sleep();
                self.chan_osd(5)
            }
            _ => {}
        }
    }

    /// Channel up / down through the lineup's numbers (going round).
    pub fn chan_flip(&mut self, step: i64) {
        let Some(number) = self.ch.play.as_ref().map(|cp| cp.number) else { return };
        let Some(l) = self.ch.lineup.clone() else { return };
        let nums: Vec<u32> = l.snap.channels.iter().map(|c| c.number).collect();
        if nums.is_empty() {
            return;
        }
        let i = nums.iter().position(|&n| n == number).unwrap_or(0) as i64;
        let to = nums[(i + step).rem_euclid(nums.len() as i64) as usize];
        self.play_channel(to);
    }

    /// Called from tick() while a channel plays: player lines, the first `url`, exits, feeding.
    pub fn chan_tick(&mut self) {
        let now = now_f();
        let mut ended_sessions = Vec::new();
        let mut banner = false;
        while let Ok((run, line)) = self.ch.lines_rx.try_recv() {
            let Some(cp) = self.ch.play.as_mut() else { continue };
            if run != cp.feed.run {
                continue; // a stopped player's last lines (Feed::on_line would ignore them too)
            }
            cp.last_line = Instant::now();
            if ["item", "queue", "stream error", "cut to", "next:", "video stream", "opened", "prebuffered", "read-ahead", "static", "sync:", "no "]
                .iter().any(|p| line.starts_with(p))
            {
                log(&format!("player: {line}"));
            }
            match cp.feed.on_line(run, &line, now) {
                Event::Started(s) => {
                    cp.exits = 0;
                    cp.fail_key = None;
                    cp.bar = if s.title.is_empty() { cp.name.clone() } else { format!("{} · {}", cp.name, s.title) };
                    banner |= s.video;
                }
                Event::Ended(s) => ended_sessions.extend(s.session),
                _ => {}
            }
        }
        for s in ended_sessions {
            self.end_encode_bg(s);
        }
        if banner {
            self.chan_osd(5);
        }
        let Some(bar) = self.ch.play.as_ref().map(|cp| cp.bar.clone()) else { return };
        self.shared.playing(true, false, &bar);
        if self.sleep_at.is_some_and(|t| Instant::now() >= t) {
            log("sleep timer: stopping");
            return self.stop(true);
        }
        if self.last_awake.elapsed() > Duration::from_secs(20) {
            keep_awake();
            self.last_awake = Instant::now();
        }
        let Some(cp) = self.ch.play.as_mut() else { return };
        if cp.url.as_ref().is_some_and(|(at, _)| now >= *at) {
            let (_, line) = cp.url.take().unwrap();
            if let Some(w) = cp.child.as_mut().and_then(|c| c.stdin.as_mut()) {
                let _ = w.write_all(line.as_bytes()).and_then(|_| w.flush());
            }
            cp.last_line = Instant::now();
        }
        let code = match cp.child.as_mut().map(|c| c.try_wait()) {
            Some(Ok(Some(st))) => Some(st.code().unwrap_or(-1)),
            _ if cp.url.is_none() && cp.last_line.elapsed() > PLAYER_SILENT => Some(-2), // hung (no status lines)
            _ => None,
        };
        if let Some(code) = code {
            return self.chan_exited(code);
        }
        if !cp.feed.wants_more(now) {
            return;
        }
        let number = cp.number;
        let from = cp.feed.last_end.floor() as i64;
        let Some((l, i)) = self.lineup_channel(number) else {
            log(&format!("channel {number}: gone from the new lineup, stopping"));
            self.shared.ended("");
            return self.stop(true);
        };
        let es = chan_entries(&self.ch.clips, &l, &l.snap.channels[i], from, 6);
        self.chan_prefetch(&es);
        let Some(mut cp) = self.ch.play.take() else { return };
        let lines = cp.feed.feed(&es, now, l.utc_off, &mut |n| self.chan_resolve(n));
        self.ch.play = Some(cp);
        for line in lines {
            log(&format!("channel: {}", line.split(" http").next().unwrap_or(&line)));
            self.chan_write(&format!("{line}\n"));
        }
    }

    /// The channel's player ended by itself: start it again (rejoin on air; a first item that failed to open twice
    /// in a row is skipped, the static covers until the next one is due), or give up after RETRY_S.
    fn chan_exited(&mut self, code: i32) {
        let Some(mut cp) = self.ch.play.take() else { return };
        if let Some(c) = cp.child.take() {
            end_player(c); // reaps it (kills a hung one)
        }
        for s in cp.feed.sessions() {
            self.end_encode_bg(s);
        }
        let (skip_until, fail_key) = match cp.feed.restart_skip() {
            Some(end) if cp.fail_key == Some(end) => (end, None),
            Some(end) => (0.0, Some(end)),
            None => (0.0, None),
        };
        let exits = cp.exits + 1;
        let Some(&wait) = RETRY_S.get(exits as usize - 1) else {
            log(&format!("channel {}: player exited (code {code}) {exits} times in a row, giving up", cp.name));
            self.shared.ended("");
            return self.stop(true);
        };
        log(&format!("channel {}: player exited (code {code}), starting it again{}", cp.name,
            if skip_until > 0.0 { " after the item that cannot be opened" } else { "" }));
        let delay = if exits > 1 { wait as f64 } else { 0.0 };
        if !self.chan_spawn(cp.number, skip_until, delay, exits, fail_key) {
            self.shared.ended("");
            self.stop(true);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(extra: &str) -> Cfg {
        let v: Value = serde_json::from_str(&format!("{{\"server\":\"https://x\",\"user_id\":\"u\",\"token\":\"t\"{extra}}}")).unwrap();
        Cfg::parse(&v, "test").unwrap()
    }

    #[test]
    fn rules_from_cfg() {
        let r = rules_for(&cfg(""), 0);
        assert_eq!((r.pad, r.break_secs, r.epoch), (None, 120, epoch_for(6 * 3600, 0)));
        let r = rules_for(&cfg(",\"breaks\":\"off\",\"break_minutes\":5,\"pad\":true,\"schedule_day_start\":\"04:30\""), -7 * 3600);
        assert_eq!((r.pad, r.break_secs, r.epoch), (Some(true), 0, epoch_for(4 * 3600 + 1800, -7 * 3600)));
        assert_eq!(rules_for(&cfg(",\"break_minutes\":3"), 0).break_secs, 180);
    }

    #[test]
    fn local_offset_is_sane() {
        let off = utc_offset(1_795_230_000);
        assert!(off.abs() <= 14 * 3600 && off % 900 == 0, "{off}");
    }

    #[test]
    fn job_names_are_cache_names() {
        assert!(fetch::valid_name(&job_name("807a5f02c43bb3da713ba94b1f4bf1da")));
    }
}
