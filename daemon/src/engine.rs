//! Channel engine: what a box channel plays next, and its guide / home-screen JSON. Pure: no I/O, no clock.
//!
//! `entries` lays a channel's schedule out as one unbroken run of entries (an episode, then the filler that pads its
//! slot, then the next episode, ...). `Feed` turns those into the stdin lines of one `sljf-play --channel` run: the
//! first item as the `url` line, then `next` lines sent at most LEAD s before they are needed, two at a time, with
//! the schedule kept true (an item that would start more than DRIFT s late is joined at its scheduled offset or cut
//! short; one that would start early waits behind a card).
use crate::chan::Snapshot;
use crate::filler::{Clip, ClipLoc, Fill, fill_gap, seed};
use crate::sched::{ChannelDef, Ep, Rules, Slot};
use serde_json::{Value, json};
use std::collections::VecDeque;

/// An item more than this late (s) is joined at its scheduled offset; this early, a card fills the wait.
pub const DRIFT: f64 = 5.0;
/// `next` lines go to the player at most this long (s) before the item is due.
pub const LEAD: f64 = 20.0;
/// Items still queued in the player (not started) at most.
pub const AHEAD: usize = 2;
/// The `url` of a first item that is due later is sent this long (s) before it, so it is open in time.
pub const OPEN_AHEAD: f64 = 2.0;
/// Less than this left of an entry: not worth playing.
const MIN_PLAY: f64 = 2.0;
/// A filler gap is never filled past this (s); the rest is a card.
pub const MAX_GAP: f64 = 3.0 * 3600.0;

#[derive(Debug, Clone, PartialEq)]
pub enum Next {
    /// An episode or film. `limit_secs` = how long to play from `start_secs`.
    Video { item_id: String, start_secs: f64, limit_secs: f64, slot_start: i64 },
    /// A bumper / commercial: played from its start (never seeked into), cut at `limit_secs`.
    Clip { loc: ClipLoc, limit_secs: f64 },
    /// Silent static with the bar showing `text`.
    Card { secs: f64, text: String },
}

/// One stretch of a channel's timeline, as scheduled.
#[derive(Debug, Clone, PartialEq)]
pub struct Entry {
    /// Wall-clock start (epoch s; filler starts at fractions of a second).
    pub at: f64,
    pub len: f64,
    pub next: Next,
    /// Bar title for an episode / film ("Series · S1E2 Title"); "" for filler.
    pub title: String,
}

/// A day's channels with the rules and the zone they were built with.
pub struct Lineup {
    pub snap: Snapshot,
    pub rules: Rules,
    /// Local time minus UTC (s) when the lineup was built: card times.
    pub utc_off: i64,
}

pub fn channel_id(number: u32) -> String {
    format!("ch:{number}")
}

/// "ch:10" (also percent-encoded "ch%3A10") -> 10.
pub fn parse_channel_id(s: &str) -> Option<u32> {
    let n = s.strip_prefix("ch:").or_else(|| s.strip_prefix("ch%3A")).or_else(|| s.strip_prefix("ch%3a"))?;
    if n.is_empty() || !n.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    n.parse().ok()
}

/// The guide's programme name: the series, else the title.
pub fn prog_name(ep: &Ep) -> &str {
    if ep.series.is_empty() { &ep.title } else { &ep.series }
}

/// The guide's episode line ("S1E2 Title"), None for a film.
fn prog_episode(ep: &Ep) -> Option<String> {
    (!ep.series.is_empty()).then(|| format!("{} {}", ep.episode, ep.title).trim().to_string())
}

/// "Series · S1E2 Title" or the film's title.
pub fn ep_title(ep: &Ep) -> String {
    match prog_episode(ep) {
        Some(e) => format!("{} · {e}", ep.series),
        None => ep.title.clone(),
    }
}

/// "HH:MM" local of epoch second `t`.
pub fn hhmm(t: f64, utc_off: i64) -> String {
    let l = (t.round() as i64 + utc_off).rem_euclid(86_400);
    format!("{:02}:{:02}", l / 3600, l % 3600 / 60)
}

fn card_text(title: &str, at: f64, utc_off: i64) -> String {
    format!("Up next: {title} at {}", hhmm(at, utc_off))
}

/// One line for the player's stdin: no control characters.
fn clean(s: &str) -> String {
    s.chars().map(|c| if c.is_control() { ' ' } else { c }).collect()
}

/// The filler of one slot's gap (capped at MAX_GAP) from the pools; cards come without text (`entries` adds it).
pub fn slot_filler(number: u32, s: &Slot, commercials: &[Clip], bumpers: &[Clip], break_secs: u32) -> Vec<Next> {
    let gap = ((s.end - s.item_end) as f64).min(MAX_GAP);
    fill_gap(gap, commercials, bumpers, break_secs as f64, seed(number, s.start))
        .into_iter()
        .map(|(com, f)| match f {
            Fill::Clip { clip, limit } => {
                let pool = if com { commercials } else { bumpers };
                Next::Clip { loc: pool[clip].loc.clone(), limit_secs: limit }
            }
            Fill::Card { secs } => Next::Card { secs, text: String::new() },
        })
        .collect()
}

fn len_of(n: &Next) -> f64 {
    match n {
        Next::Video { limit_secs, .. } | Next::Clip { limit_secs, .. } => *limit_secs,
        Next::Card { secs, .. } => *secs,
    }
}

fn with_len(n: &Next, len: f64) -> Next {
    match n {
        Next::Video { item_id, start_secs, slot_start, .. } => Next::Video { item_id: item_id.clone(), start_secs: *start_secs, limit_secs: len, slot_start: *slot_start },
        Next::Clip { loc, .. } => Next::Clip { loc: loc.clone(), limit_secs: len },
        Next::Card { text, .. } => Next::Card { secs: len, text: text.clone() },
    }
}

/// The entry on air at `t`, then the `n` following (as scheduled: no offset applied).
pub fn entries(ch: &ChannelDef, r: &Rules, fill: &dyn Fn(&Slot) -> Vec<Next>, t: i64, n: usize, utc_off: i64) -> Vec<Entry> {
    let mut horizon: i64 = 6 * 3600;
    loop {
        let ss = crate::sched::slots(ch, r, t, t + horizon);
        let mut out: Vec<Entry> = Vec::new();
        for s in &ss {
            let ep = &ch.shows[s.show].eps[s.ep];
            let len = (s.item_end - s.start) as f64;
            out.push(Entry {
                at: s.start as f64,
                len,
                next: Next::Video { item_id: ep.id.clone(), start_secs: 0.0, limit_secs: len, slot_start: s.start },
                title: ep_title(ep),
            });
            let end = s.end as f64;
            let mut at = s.item_end as f64;
            for f in fill(s) {
                let len = len_of(&f).min(end - at);
                if !len.is_finite() || len < 0.01 {
                    continue;
                }
                out.push(Entry { at, len, next: with_len(&f, len), title: String::new() });
                at += len;
            }
            if end - at >= MIN_PLAY {
                out.push(Entry { at, len: end - at, next: Next::Card { secs: end - at, text: String::new() }, title: String::new() });
            }
        }
        out.retain(|e| e.at + e.len > t as f64);
        if out.len() > n || ss.is_empty() || horizon > 4 * 86_400 {
            out.truncate(n + 1);
            for i in 0..out.len() {
                if let Next::Card { secs, .. } = out[i].next {
                    let end = out[i].at + out[i].len;
                    let (title, at) = next_video(&out[i + 1..]).unwrap_or_else(|| {
                        crate::sched::now(ch, r, end.ceil() as i64)
                            .map(|(s, _)| (prog_name(&ch.shows[s.show].eps[s.ep]).to_string(), s.start as f64))
                            .unwrap_or_default()
                    });
                    out[i].next = Next::Card { secs, text: card_text(&title, at, utc_off) };
                }
            }
            return out;
        }
        horizon *= 4;
    }
}

/// The programme name and start of the first episode / film in `es`.
fn next_video(es: &[Entry]) -> Option<(String, f64)> {
    es.iter().find(|e| matches!(e.next, Next::Video { .. })).map(|e| {
        let name = e.title.split(" · ").next().unwrap_or("").to_string();
        (name, e.at)
    })
}

/// `e` started at `start_at`: as scheduled when it is at most `slack` s late, else joined late (an episode seeks
/// ahead, a clip or card is cut short). None when less than MIN_PLAY s of it would be left. Returns the play length.
pub fn place(e: &Entry, start_at: f64, slack: f64) -> Option<(Next, f64)> {
    let late = start_at - e.at;
    if late <= slack {
        return Some((e.next.clone(), e.len));
    }
    let left = e.len - late;
    if left < MIN_PLAY {
        return None;
    }
    let next = match &e.next {
        Next::Video { item_id, start_secs, slot_start, .. } => {
            Next::Video { item_id: item_id.clone(), start_secs: start_secs + late, limit_secs: left, slot_start: *slot_start }
        }
        other => with_len(other, left),
    };
    Some((next, left))
}

/// The item on air at `t` (with its offset), then the `n` following entries: episode, its filler, next episode ...
/// (The plan's view of `entries`; the daemon feeds from `entries` + `Feed`, which also keep the schedule true.)
#[allow(dead_code)]
pub fn upcoming(ch: &ChannelDef, r: &Rules, fill: &dyn Fn(&Slot) -> Vec<Next>, t: i64, n: usize, utc_off: i64) -> Vec<(i64, Next)> {
    let mut it = entries(ch, r, fill, t, n + 1, utc_off).into_iter();
    let mut out = Vec::new();
    if let Some((next, _)) = it.next().and_then(|e| place(&e, t as f64, 0.0)) {
        out.push((t, next));
    }
    out.extend(it.take(n).map(|e| (e.at.floor() as i64, e.next)));
    out
}

/// Same shape as guide::guide_json: one programme per slot (its filler folded in).
pub fn guide_json(snap: &Snapshot, r: &Rules, from: i64, until: i64, now: i64) -> Value {
    let chans: Vec<Value> = snap.channels.iter().map(|ch| {
        let progs: Vec<Value> = crate::sched::slots(ch, r, from, until).iter().map(|s| {
            let ep = &ch.shows[s.show].eps[s.ep];
            json!({
                "id": format!("{}:{}", channel_id(ch.number), s.start), "name": prog_name(ep), "episode": prog_episode(ep),
                "start": s.start, "end": s.end, "overview": ep.overview.chars().take(400).collect::<String>(),
            })
        }).collect();
        json!({"id": channel_id(ch.number), "number": ch.number.to_string(), "name": ch.name, "programs": progs})
    }).collect();
    json!({"now": now, "channels": chans})
}

/// Same shape as the /home `channels` list (local.rs `slim` of a TvChannel).
pub fn home_channels(snap: &Snapshot, r: &Rules, now: i64) -> Value {
    Value::Array(snap.channels.iter().map(|ch| {
        let ep = crate::sched::now(ch, r, now).map(|(s, _)| &ch.shows[s.show].eps[s.ep]);
        let id = channel_id(ch.number);
        json!({
            "id": id, "name": ch.name, "type": "TvChannel", "year": null, "series": null, "season": null, "episode": null,
            "minutes": null, "folder": false, "played": false, "pct": 0.0, "resume": false, "unplayed": null,
            "number": ch.number.to_string(), "program": ep.map(prog_name),
            "overview": ep.map(|e| e.overview.chars().take(400).collect::<String>()), "poster": id,
        })
    }).collect())
}

// ---- feeding one sljf-play --channel run ------------------------------------------------------------------------

/// An item handed to the player.
#[derive(Debug, Clone, PartialEq)]
pub struct Sent {
    /// The player's item number (the `url` item is 1, then `next` lines in order).
    pub seq: u32,
    /// When it should start (wall s) and how long it plays.
    pub at: f64,
    pub len: f64,
    /// End of its stretch of the schedule (a card put in to wait: the end of what came before).
    pub sched_end: f64,
    /// When the player said `item N start`.
    pub started: Option<f64>,
    pub title: String,
    pub video: bool,
    /// Cache file it plays from (keep it from eviction), server play session (end its encode afterwards).
    pub cache: Option<String>,
    pub session: Option<String>,
}

/// Where an item plays from (built by the daemon: stream URL, /cache URL or stick path).
#[derive(Debug, Clone, PartialEq)]
pub struct Resolved {
    pub url: String,
    pub cache: Option<String>,
    pub session: Option<String>,
}

/// The first item of a run: its URL (the `url` line, due at `send_at`), and `--channel <limit>`, SLJF_START.
#[derive(Debug, Clone, PartialEq)]
pub struct First {
    pub url: String,
    pub limit: f64,
    pub start_secs: f64,
    pub send_at: f64,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    Started(Sent),
    Ended(Sent),
    OpenFailed(u32),
    Starved,
    /// From an earlier run (another generation): ignored.
    Stale,
    Other,
}

pub struct Feed {
    /// Which player run this feed belongs to: lines of another run are ignored.
    pub run: u64,
    pub sent: VecDeque<Sent>,
    seq: u32,
    /// End of the schedule handed out so far.
    pub last_end: f64,
    /// The player said `queue empty`: send the next at once.
    starved: bool,
}

impl Feed {
    pub fn new(run: u64) -> Feed {
        Feed { run, sent: VecDeque::new(), seq: 0, last_end: 0.0, starved: false }
    }

    fn push(&mut self, at: f64, len: f64, sched_end: f64, title: &str, video: bool, r: Option<&Resolved>) {
        self.seq += 1;
        self.sent.push_back(Sent {
            seq: self.seq, at, len, sched_end, started: None, title: title.to_string(), video,
            cache: r.and_then(|r| r.cache.clone()), session: r.and_then(|r| r.session.clone()),
        });
    }

    /// Picks the first item (the entry on air at `now`, or the first after `skip_until`; cards are waited out behind
    /// the static) and registers it as item 1.
    pub fn first(&mut self, es: &[Entry], now: f64, skip_until: f64, resolve: &mut dyn FnMut(&Next) -> Option<Resolved>) -> Option<First> {
        for e in es {
            let end = e.at + e.len;
            if end <= skip_until + 0.01 || end <= now + MIN_PLAY || matches!(e.next, Next::Card { .. }) {
                continue;
            }
            let start_at = now.max(e.at);
            let late = start_at - e.at;
            let mut pick = None;
            // a cached copy from the start is worth up to DRIFT s of lateness (the filler absorbs it)
            if late > 0.0 && late <= DRIFT {
                if let Some((nx, play)) = place(e, start_at, DRIFT) {
                    pick = resolve(&nx).filter(|r| r.cache.is_some()).map(|r| (nx, play, r));
                }
            }
            if pick.is_none() {
                if let Some((nx, play)) = place(e, start_at, 0.0) {
                    pick = resolve(&nx).map(|r| (nx, play, r));
                }
            }
            let Some((nx, play, r)) = pick else { continue };
            let (limit, start_secs) = match &nx {
                Next::Video { start_secs, limit_secs, .. } => (*limit_secs, *start_secs),
                Next::Clip { limit_secs, .. } => (*limit_secs, 0.0),
                Next::Card { .. } => continue,
            };
            self.push(start_at, play, end, &e.title, matches!(nx, Next::Video { .. }), Some(&r));
            self.last_end = end;
            let send_at = if e.at > now { e.at - OPEN_AHEAD } else { now };
            return Some(First { url: r.url, limit, start_secs, send_at });
        }
        None
    }

    pub fn on_line(&mut self, run: u64, line: &str, now: f64) -> Event {
        if run != self.run {
            return Event::Stale;
        }
        let w: Vec<&str> = line.split_whitespace().collect();
        let num = |s: &str| s.trim_end_matches(':').parse::<u32>().ok();
        match w.as_slice() {
            ["item", n, "start"] => {
                let Some(n) = num(n) else { return Event::Other };
                let Some(i) = self.sent.iter().position(|s| s.seq == n) else { return Event::Other };
                self.sent[i].started = Some(now);
                let s = self.sent[i].clone();
                self.sent.retain(|x| x.seq >= n); // anything before it is over (its end line was lost)
                Event::Started(s)
            }
            ["item", n, "end"] => {
                let Some(n) = num(n) else { return Event::Other };
                match self.sent.iter().position(|s| s.seq == n) {
                    Some(i) => Event::Ended(self.sent.remove(i).unwrap()),
                    None => Event::Other,
                }
            }
            ["item", n, "open", f, ..] if n.ends_with(':') && f.starts_with("failed") => num(n).map(Event::OpenFailed).unwrap_or(Event::Other),
            ["queue", "empty"] => {
                self.starved = true;
                Event::Starved
            }
            _ => Event::Other,
        }
    }

    /// Items handed over that have not started.
    pub fn pending(&self) -> usize {
        self.sent.iter().filter(|s| s.started.is_none()).count()
    }

    /// When the last item handed over should end (an item not started yet starts no earlier than `now`).
    fn pred_end(&self, now: f64) -> Option<f64> {
        let mut cur: Option<f64> = None;
        for s in &self.sent {
            let start = s.started.unwrap_or_else(|| cur.unwrap_or(s.at).max(now));
            cur = Some(start + s.len);
        }
        cur
    }

    /// Whether `feed` may send something now (cheap: lets the caller skip building entries).
    pub fn wants_more(&self, now: f64) -> bool {
        self.pending() < AHEAD && (self.starved || self.pred_end(now).unwrap_or(now) - now <= LEAD)
    }

    /// The `next` lines to send now, from the entries following `last_end`.
    pub fn feed(&mut self, es: &[Entry], now: f64, utc_off: i64, resolve: &mut dyn FnMut(&Next) -> Option<Resolved>) -> Vec<String> {
        let mut lines = Vec::new();
        let mut i = 0;
        while i < es.len() {
            let e = &es[i];
            let end = e.at + e.len;
            if end <= self.last_end + 0.01 {
                i += 1;
                continue;
            }
            if self.pending() >= AHEAD {
                break;
            }
            let p = self.pred_end(now).unwrap_or(now).max(now);
            if !self.starved && p - now > LEAD {
                break;
            }
            if p - e.at < -DRIFT {
                // early (an item before it ended early or failed): static with the bar until it is due
                let secs = e.at - p;
                let (title, at) = next_video(&es[i..]).unwrap_or_default();
                lines.push(format!("next card {secs:.2} {}", clean(&card_text(&title, at, utc_off))));
                let last = self.last_end;
                self.push(p, secs, last, "", false, None);
                continue; // `e` itself is now due right after the card
            }
            self.last_end = end;
            i += 1;
            let Some((nx, play)) = place(e, p, DRIFT) else { continue }; // too late: dropped
            let (line, r) = match &nx {
                Next::Card { secs, text } => (format!("next card {secs:.2} {}", clean(text)), None),
                Next::Video { start_secs, limit_secs, .. } => match resolve(&nx) {
                    Some(r) => (format!("next v {limit_secs:.2} {start_secs:.2} {}", r.url), Some(r)),
                    None => continue,
                },
                Next::Clip { limit_secs, .. } => match resolve(&nx) {
                    Some(r) => (format!("next v {limit_secs:.2} 0.00 {}", r.url), Some(r)),
                    None => continue,
                },
            };
            if line.contains(['\n', '\r']) {
                continue;
            }
            lines.push(line);
            self.push(p, play, end, &e.title, matches!(nx, Next::Video { .. }), r.as_ref());
        }
        if !lines.is_empty() {
            self.starved = false; // (a "queue empty" counts once: up to AHEAD items go at once)
        }
        lines
    }

    /// The player exited: Some(end of the item it never started) = that item failed to open; None = rejoin on air.
    pub fn restart_skip(&self) -> Option<f64> {
        self.sent.front().filter(|s| s.started.is_none()).map(|s| s.sched_end)
    }

    pub fn in_use(&self) -> Vec<String> {
        self.sent.iter().filter_map(|s| s.cache.clone()).collect()
    }

    pub fn sessions(&self) -> Vec<String> {
        self.sent.iter().filter_map(|s| s.session.clone()).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sched::{Show, epoch_for};

    const EPOCH: i64 = 1_767_225_600 + 6 * 3600; // 2026-01-01 06:00 UTC

    fn ep(id: &str, secs: u32, series: &str) -> Ep {
        Ep { id: id.into(), secs, title: format!("T{id}"), episode: "S1E1".into(), overview: format!("about {id}"), series: series.into() }
    }

    fn chan(number: u32, eps: Vec<Ep>) -> ChannelDef {
        ChannelDef { number, name: format!("Name{number}"), collection_id: format!("col{number}"), shows: vec![Show { id: "s".into(), eps }], filler: true }
    }

    fn rules() -> Rules {
        Rules { pad: None, break_secs: 0, epoch: epoch_for(6 * 3600, 0) }
    }

    fn one() -> ChannelDef {
        chan(10, vec![ep("a", 1320, "Show"), ep("b", 1320, "Show"), ep("c", 1320, "Show")])
    }

    fn bump(path: &str, secs: f64) -> Next {
        Next::Clip { loc: ClipLoc::Stick(path.into()), limit_secs: secs }
    }

    /// 480 s gaps: a 300 s bumper and a card for the rest.
    fn fill(_: &Slot) -> Vec<Next> {
        vec![bump("/b1", 300.0), Next::Card { secs: 180.0, text: String::new() }]
    }

    fn no_fill(_: &Slot) -> Vec<Next> {
        Vec::new()
    }

    #[test]
    fn upcoming_starts_on_air_with_offset() {
        assert_eq!(rules().epoch, EPOCH);
        let up = upcoming(&one(), &rules(), &fill, EPOCH + 100, 3, 0);
        assert_eq!(up[0], (EPOCH + 100, Next::Video { item_id: "a".into(), start_secs: 100.0, limit_secs: 1220.0, slot_start: EPOCH }));
        assert_eq!(up.len(), 4);
        // on air in the filler: a bumper is cut short from its start, never seeked into
        let up = upcoming(&one(), &rules(), &fill, EPOCH + 1320 + 100, 1, 0);
        assert_eq!(up[0], (EPOCH + 1420, bump("/b1", 200.0)));
    }

    #[test]
    fn upcoming_includes_filler_between() {
        let up = upcoming(&one(), &rules(), &fill, EPOCH + 10, 4, 0);
        assert_eq!(up[1], (EPOCH + 1320, bump("/b1", 300.0)));
        assert_eq!(up[2], (EPOCH + 1620, Next::Card { secs: 180.0, text: "Up next: Show at 06:30".into() }));
        assert!(matches!(&up[3], (t, Next::Video { item_id, start_secs, .. }) if *t == EPOCH + 1800 && item_id == "b" && *start_secs == 0.0));
        assert!(matches!(&up[4], (t, Next::Clip { .. }) if *t == EPOCH + 1800 + 1320));
        // contiguous, as entries
        let es = entries(&one(), &rules(), &fill, EPOCH, 9, 0);
        for w in es.windows(2) {
            assert!((w[0].at + w[0].len - w[1].at).abs() < 1e-6, "{w:?}");
        }
        // an unfilled gap becomes a card
        let es = entries(&one(), &rules(), &no_fill, EPOCH, 2, -7 * 3600);
        assert_eq!(es[1].next, Next::Card { secs: 480.0, text: "Up next: Show at 23:30".into() });
    }

    #[test]
    fn slot_filler_caps_huge_gaps() {
        let s = Slot { start: EPOCH, item_end: EPOCH + 60, end: EPOCH + 60 + 6 * 3600, ep: 0, show: 0 };
        let b = [Clip { loc: ClipLoc::Server("x".into()), secs: 10.0 }];
        let v = slot_filler(10, &s, &[], &b, 0);
        let total: f64 = v.iter().map(|n| match n { Next::Clip { limit_secs, .. } => *limit_secs, Next::Card { secs, .. } => *secs, _ => 0.0 }).sum();
        assert!((total - MAX_GAP).abs() < 1e-6, "{total}");
        assert!(matches!(&v[0], Next::Clip { loc: ClipLoc::Server(id), .. } if id == "x"));
        // the rest of the slot is still covered (by a card) in entries
        let ch = chan(10, vec![ep("a", 60, "")]);
        let r = Rules { pad: Some(true), break_secs: 0, epoch: EPOCH };
        let es = entries(&ch, &r, &|s| slot_filler(10, s, &[], &b, 0), EPOCH, 3, 0);
        assert!(es.windows(2).all(|w| (w[0].at + w[0].len - w[1].at).abs() < 1e-6));
    }

    fn snap() -> Snapshot {
        Snapshot { day: 0, channels: vec![chan(3, vec![ep("x", 1500, "")]), one()], pools: vec![] }
    }

    #[test]
    fn guide_json_shape() {
        let now = EPOCH + 60;
        let g = guide_json(&snap(), &rules(), now, now + 3 * 3600, now);
        assert_eq!(g["now"], now);
        let ch = g["channels"].as_array().unwrap();
        assert_eq!(ch.len(), 2);
        assert_eq!(ch[0]["id"], "ch:3");
        assert_eq!(ch[1]["id"], "ch:10");
        assert_eq!(ch[1]["number"], "10");
        assert_eq!(ch[1]["name"], "Name10");
        let p = ch[1]["programs"].as_array().unwrap();
        assert_eq!(p.len(), 7);
        for k in ["id", "name", "episode", "start", "end", "overview"] {
            assert!(p[0].get(k).is_some(), "{k}");
        }
        assert_eq!(p[0]["name"], "Show");
        assert_eq!(p[0]["episode"], "S1E1 Ta");
        assert_eq!(p[0]["start"], EPOCH);
        assert_eq!(p[0]["end"], EPOCH + 1800); // filler folded in
        assert_eq!(p[0]["overview"], "about a");
        assert_eq!(p[1]["start"], EPOCH + 1800);
        // a film: its title, no episode
        let f = &ch[0]["programs"][0];
        assert_eq!(f["name"], "Tx");
        assert!(f["episode"].is_null());
    }

    #[test]
    fn home_channels_shape() {
        let h = home_channels(&snap(), &rules(), EPOCH + 1900);
        let a = h.as_array().unwrap();
        assert_eq!(a.len(), 2);
        assert_eq!(a[1]["id"], "ch:10");
        assert_eq!(a[1]["type"], "TvChannel");
        assert_eq!(a[1]["number"], "10");
        assert_eq!(a[1]["name"], "Name10");
        assert_eq!(a[1]["program"], "Show");
        assert_eq!(a[1]["poster"], "ch:10");
        assert_eq!(a[1]["folder"], false);
    }

    #[test]
    fn channel_ids_round_trip() {
        for n in [0, 7, 10, 4_000_000] {
            assert_eq!(parse_channel_id(&channel_id(n)), Some(n));
        }
        assert_eq!(parse_channel_id("ch%3A7"), Some(7));
        assert_eq!(parse_channel_id("ch%3a7"), Some(7));
        for bad in ["", "ch:", "ch:x", "ch:-1", "7", "abc", "ch:1:2", "ch::1"] {
            assert_eq!(parse_channel_id(bad), None, "{bad}");
        }
    }

    #[test]
    fn hhmm_local() {
        assert_eq!(hhmm(EPOCH as f64, 0), "06:00");
        assert_eq!(hhmm(EPOCH as f64 + 1799.6, 3600), "07:30");
        assert_eq!(hhmm(EPOCH as f64, -7 * 3600), "23:00");
    }

    // ---- Feed ----

    fn res(n: &Next) -> Option<Resolved> {
        match n {
            Next::Video { item_id, .. } => Some(Resolved { url: format!("http://s/{item_id}"), cache: None, session: Some(format!("ses-{item_id}")) }),
            Next::Clip { loc: ClipLoc::Stick(p), .. } => Some(Resolved { url: p.clone(), cache: None, session: None }),
            Next::Clip { loc: ClipLoc::Server(id), .. } => Some(Resolved { url: format!("http://s/{id}"), cache: None, session: None }),
            Next::Card { .. } => None,
        }
    }

    /// Bumpers of 100 s, then a card for the rest of the 480 s gap.
    fn fill3(_: &Slot) -> Vec<Next> {
        vec![bump("/b1", 100.0), bump("/b2", 100.0), Next::Card { secs: 280.0, text: String::new() }]
    }

    fn es_from(t: f64) -> Vec<Entry> {
        entries(&one(), &rules(), &fill3, t.floor() as i64, 8, 0)
    }

    /// A feed whose item 1 (episode a, joined 100 s in) has started.
    fn started_feed(run: u64) -> (Feed, f64) {
        let mut f = Feed::new(run);
        let now = EPOCH as f64 + 100.0;
        let first = f.first(&es_from(now), now, 0.0, &mut res).unwrap();
        assert_eq!(first, First { url: "http://s/a".into(), limit: 1220.0, start_secs: 100.0, send_at: now });
        assert_eq!(f.on_line(run, "item 1 start", now + 1.0), Event::Started(f.sent[0].clone()));
        (f, now + 1.0)
    }

    #[test]
    fn feed_sends_ahead_within_lead_two_at_a_time() {
        let (mut f, t) = started_feed(1);
        assert!(!f.wants_more(t));
        assert!(f.feed(&es_from(t), t, 0, &mut res).is_empty()); // the episode has 20 min left
        let t = EPOCH as f64 + 1320.0 - 15.0;
        assert!(f.wants_more(t));
        let lines = f.feed(&es_from(f.last_end), t, 0, &mut res);
        // item 1 started 1 s late: ends at 1321, the bumper follows (within DRIFT: as scheduled); b2 is not due yet
        assert_eq!(lines, vec!["next v 100.00 0.00 /b1".to_string()]);
        assert_eq!(f.pending(), 1);
        f.on_line(1, "item 1 end", EPOCH as f64 + 1321.0);
        f.on_line(1, "item 2 start", EPOCH as f64 + 1321.0);
        let lines = f.feed(&es_from(f.last_end), EPOCH as f64 + 1405.0, 0, &mut res);
        assert_eq!(lines, vec!["next v 100.00 0.00 /b2".to_string()]);
        f.on_line(1, "item 2 end", EPOCH as f64 + 1421.0);
        f.on_line(1, "item 3 start", EPOCH as f64 + 1421.0);
        let lines = f.feed(&es_from(f.last_end), EPOCH as f64 + 1505.0, 0, &mut res);
        assert_eq!(lines, vec!["next card 280.00 Up next: Show at 06:30".to_string()]);
        // "queue empty" (the player read its item to the end): sent at once, two at most
        let (mut f, t) = started_feed(1);
        f.on_line(1, "queue empty", t + 1200.0);
        let lines = f.feed(&es_from(f.last_end), t + 1200.0, 0, &mut res);
        assert_eq!(lines, vec!["next v 100.00 0.00 /b1".to_string(), "next v 100.00 0.00 /b2".to_string()]);
        assert_eq!(f.pending(), AHEAD);
        assert!(!f.wants_more(t + 1200.0));
    }

    #[test]
    fn late_episode_joined_at_its_offset_bumper_cut_short() {
        let (mut f, _) = started_feed(1);
        // item 1 plays until 1340 (stalled 20 s): the bumper due at 1320 is cut short, never seeked
        f.sent[0].len = 1239.0;
        let t = EPOCH as f64 + 1330.0;
        let lines = f.feed(&es_from(f.last_end), t, 0, &mut res);
        assert_eq!(lines[0], "next v 80.00 0.00 /b1");
        // an episode due 30 s ago is joined 30 s in
        let mut f = Feed::new(2);
        let es = es_from(EPOCH as f64 + 1700.0);
        f.last_end = EPOCH as f64 + 1800.0;
        let lines = f.feed(&es, EPOCH as f64 + 1830.0, 0, &mut res);
        assert_eq!(lines[0], "next v 1290.00 30.00 http://s/b");
    }

    #[test]
    fn early_item_waits_behind_a_card() {
        let (mut f, _) = started_feed(1);
        // item 1 ends at 600 instead of 1320 (the file is shorter): the bumpers play early, then a card waits for
        // the schedule, so the next episode starts on time
        let t = EPOCH as f64 + 540.0;
        assert_eq!(f.on_line(1, "queue empty", t), Event::Starved);
        assert!(f.wants_more(t));
        assert_eq!(f.feed(&es_from(f.last_end), t, 0, &mut res).len(), 2);
        let e = EPOCH as f64;
        for (l, at) in [("item 1 end", 600.0), ("item 2 start", 600.0), ("item 2 end", 700.0), ("item 3 start", 700.0)] {
            f.on_line(1, l, e + at);
        }
        let lines = f.feed(&es_from(f.last_end), e + 785.0, 0, &mut res);
        assert_eq!(lines, vec!["next card 720.00 Up next: Show at 06:30".to_string()]);
        f.on_line(1, "item 3 end", e + 800.0);
        f.on_line(1, "item 4 start", e + 800.0);
        let lines = f.feed(&es_from(f.last_end), e + 1505.0, 0, &mut res);
        assert_eq!(lines, vec!["next card 280.00 Up next: Show at 06:30".to_string()]);
        f.on_line(1, "item 4 end", e + 1520.0);
        f.on_line(1, "item 5 start", e + 1520.0);
        let lines = f.feed(&es_from(f.last_end), e + 1790.0, 0, &mut res);
        assert_eq!(lines, vec!["next v 1320.00 0.00 http://s/b".to_string()]);
        assert_eq!(f.sent.back().unwrap().at, e + 1800.0);
    }

    #[test]
    fn stale_item_lines_ignored() {
        let (mut f, t) = started_feed(5);
        let before = f.sent.clone();
        for l in ["item 1 end", "item 2 start", "queue empty", "item 1: open failed, skipped"] {
            assert_eq!(f.on_line(4, l, t), Event::Stale, "{l}");
        }
        assert_eq!(f.sent, before);
        assert!(!f.wants_more(t)); // the old run's "queue empty" did not make this one hurry
        assert_eq!(f.on_line(5, "item 1 end", t), Event::Ended(before[0].clone()));
        assert!(f.sent.is_empty());
    }

    #[test]
    fn open_failure_moves_on() {
        let (mut f, _) = started_feed(1);
        let e = EPOCH as f64;
        f.feed(&es_from(f.last_end), e + 1310.0, 0, &mut res); // b1 = item 2
        f.on_line(1, "item 1 end", e + 1321.0);
        f.on_line(1, "item 2 start", e + 1321.0);
        assert_eq!(f.feed(&es_from(f.last_end), e + 1405.0, 0, &mut res).len(), 1); // b2 = item 3
        // item 3 cannot be opened: skipped; what follows would start 99 s early, so a card waits that out and the
        // channel goes on
        assert_eq!(f.on_line(1, "item 3: open failed, skipped", e + 1406.0), Event::OpenFailed(3));
        assert!(matches!(f.on_line(1, "item 3 end", e + 1406.0), Event::Ended(s) if s.seq == 3));
        let lines = f.feed(&es_from(f.last_end), e + 1410.0, 0, &mut res);
        assert_eq!(lines, vec!["next card 99.00 Up next: Show at 06:30".to_string()]);
        f.on_line(1, "item 2 end", e + 1421.0);
        f.on_line(1, "item 4 start", e + 1421.0);
        let lines = f.feed(&es_from(f.last_end), e + 1505.0, 0, &mut res);
        assert_eq!(lines, vec!["next card 280.00 Up next: Show at 06:30".to_string()]);
        f.on_line(1, "item 4 end", e + 1520.0);
        f.on_line(1, "item 5 start", e + 1520.0);
        let lines = f.feed(&es_from(f.last_end), e + 1785.0, 0, &mut res);
        assert_eq!(lines, vec!["next v 1320.00 0.00 http://s/b".to_string()]);
        assert_eq!(f.sent.back().unwrap().at, e + 1800.0);
        // the first item failing to open: the player exits; the restart skips it (when it failed before)
        let mut g = Feed::new(2);
        let now = EPOCH as f64 + 100.0;
        g.first(&es_from(now), now, 0.0, &mut res).unwrap();
        assert_eq!(g.restart_skip(), Some(EPOCH as f64 + 1320.0));
        let mut h = Feed::new(3);
        let first = h.first(&es_from(now), now, EPOCH as f64 + 1320.0, &mut res).unwrap();
        assert_eq!(first, First { url: "/b1".into(), limit: 100.0, start_secs: 0.0, send_at: EPOCH as f64 + 1320.0 - OPEN_AHEAD });
        // an item that had started and then broke: rejoin on air
        assert_eq!(started_feed(4).0.restart_skip(), None);
    }

    #[test]
    fn first_waits_out_a_card_and_skips_unresolvable() {
        let mut f = Feed::new(1);
        let now = EPOCH as f64 + 1320.0 + 250.0; // in the card of the first gap
        let first = f.first(&es_from(now), now, 0.0, &mut res).unwrap();
        assert_eq!(first.url, "http://s/b");
        assert_eq!(first.send_at, EPOCH as f64 + 1800.0 - OPEN_AHEAD);
        let mut g = Feed::new(1);
        let mut only_clips = |n: &Next| if matches!(n, Next::Video { .. }) { None } else { res(n) };
        let first = g.first(&es_from(EPOCH as f64), EPOCH as f64, 0.0, &mut only_clips).unwrap();
        assert_eq!(first.url, "/b1");
        // a cached copy from the start beats a seek of up to DRIFT s
        let mut c = Feed::new(1);
        let mut cached = |n: &Next| match n {
            Next::Video { start_secs, .. } if *start_secs == 0.0 => Some(Resolved { url: "http://127.0.0.1/cache?f=a-0.ts".into(), cache: Some("a-0.ts".into()), session: None }),
            _ => res(n),
        };
        let first = c.first(&es_from(EPOCH as f64), EPOCH as f64 + 3.0, 0.0, &mut cached).unwrap();
        assert_eq!((first.start_secs, first.url.contains("cache")), (0.0, true));
        assert_eq!(c.in_use(), vec!["a-0.ts".to_string()]);
    }
}
