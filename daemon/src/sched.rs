//! Channel scheduler: what is on a TV channel at wall-clock time t. A pure, deterministic function of the
//! channel definition and the rules, so every box that builds the same channel agrees without talking to
//! the others. Times are epoch seconds; the timeline is one unbroken run of slots starting at `Rules::epoch`.
//!
//! Each round puts every show on once, in an order shuffled from a seed made of the channel number and the
//! round, so the order is stable per box and differs between rounds. Each show hands out its episodes in
//! order and wraps. Simulating from the epoch costs O(items since epoch), so `Scheduler` keeps a cursor per
//! channel and carries on from it while t moves forward.
use std::collections::HashMap;
use std::sync::Mutex;

pub struct Ep {
    pub id: String,
    pub secs: u32,
    pub title: String,
    /// "S1E2" ("" for a film)
    pub episode: String,
    pub overview: String,
    /// series name ("" for a film): the guide's programme name
    pub series: String,
}

/// A film is a Show with one Ep.
pub struct Show {
    pub id: String,
    pub eps: Vec<Ep>,
}

pub struct ChannelDef {
    pub number: u32,
    pub name: String,
    pub collection_id: String,
    pub shows: Vec<Show>,
    /// The server has Bumpers/Commercials pools for this channel.
    pub filler: bool,
}

pub struct Rules {
    /// Some(true)/Some(false) forces padding on/off; None follows `ChannelDef::filler`.
    pub pad: Option<bool>,
    pub break_secs: u32,
    /// Start of the timeline; must sit on a half hour so padded slots land on the clock's half-hour grid.
    pub epoch: i64,
}

/// `start..item_end` is the episode, `item_end..end` is the gap that Task 5 fills with bumpers/commercials.
#[derive(Debug, Clone, PartialEq)]
pub struct Slot {
    pub start: i64,
    pub item_end: i64,
    pub end: i64,
    pub ep: usize,
    pub show: usize,
}

const HALF_HOUR: i64 = 1800;
/// Before this the box's clock has not been set yet (it reads 1970), so there is no schedule to show.
const MIN_T: i64 = 1_577_836_800; // 2020-01-01T00:00:00Z
/// 2026-01-01T00:00:00Z
const EPOCH_2026: i64 = 1_767_225_600;

/// Epoch seconds of 2026-01-01 at the given local day start (e.g. 6 h = 21600) in a zone `utc_offset_secs` east of UTC.
pub fn epoch_for(day_start_local_secs: i64, utc_offset_secs: i64) -> i64 {
    EPOCH_2026 + day_start_local_secs - utc_offset_secs
}

fn splitmix64(state: &mut u64) -> u64 {
    *state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut z = *state;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// Show indexes (into `ch.shows`, skipping shows with no episodes) in this round's order: sorted by id, then
/// Fisher-Yates shuffled by splitmix64. Sorting first makes the result independent of the order Jellyfin listed them.
fn round_order(ch: &ChannelDef, round: u64) -> Vec<usize> {
    let mut order: Vec<usize> = (0..ch.shows.len()).filter(|&i| !ch.shows[i].eps.is_empty()).collect();
    order.sort_by(|&a, &b| ch.shows[a].id.cmp(&ch.shows[b].id));
    let mut st = ((ch.number as u64) << 32) | round;
    for i in (1..order.len()).rev() {
        let j = (splitmix64(&mut st) % (i as u64 + 1)) as usize;
        order.swap(i, j);
    }
    order
}

/// Simulation state sitting between two slots: where the next slot starts and which show/episode it takes.
/// Cloning one is how `Scheduler` resumes without replaying from the epoch.
#[derive(Clone)]
struct Cursor {
    next_start: i64,
    round: u64,
    pos: usize,
    order: Vec<usize>,
    counters: Vec<usize>,
}

impl Cursor {
    fn new(ch: &ChannelDef, r: &Rules) -> Option<Cursor> {
        if !ch.shows.iter().any(|s| s.eps.iter().any(|e| e.secs > 0)) {
            return None;
        }
        Some(Cursor { next_start: r.epoch, round: 0, pos: 0, order: round_order(ch, 0), counters: vec![0; ch.shows.len()] })
    }

    fn step(&mut self, ch: &ChannelDef, r: &Rules) -> Slot {
        if self.pos >= self.order.len() {
            self.round += 1;
            self.pos = 0;
            self.order = round_order(ch, self.round);
        }
        let show = self.order[self.pos];
        self.pos += 1;
        let eps = &ch.shows[show].eps;
        let ep = self.counters[show] % eps.len();
        self.counters[show] += 1;
        let start = self.next_start;
        let item_end = start + eps[ep].secs as i64;
        let end = if pad_on(ch, r) {
            let len = item_end + r.break_secs as i64 - r.epoch;
            r.epoch + (len + HALF_HOUR - 1).div_euclid(HALF_HOUR) * HALF_HOUR
        } else {
            item_end
        };
        self.next_start = end;
        Slot { start, item_end, end, ep, show }
    }
}

fn pad_on(ch: &ChannelDef, r: &Rules) -> bool {
    r.pad.unwrap_or(ch.filler)
}

/// Slots overlapping [from, until), simulated fresh from the epoch.
pub fn slots(ch: &ChannelDef, r: &Rules, from: i64, until: i64) -> Vec<Slot> {
    let mut out = Vec::new();
    if from >= until || until <= r.epoch {
        return out;
    }
    let Some(mut c) = Cursor::new(ch, r) else { return out };
    while c.next_start < until {
        let s = c.step(ch, r);
        if s.end > from {
            out.push(s);
        }
    }
    out
}

/// Replay from `c` until the slot containing t. Returns the slot, the offset into it and the cursor just
/// after it (no cloning per step: the caller clones once if it wants to keep it).
fn now_from(mut c: Cursor, ch: &ChannelDef, r: &Rules, t: i64) -> Option<(Slot, i64, Cursor)> {
    loop {
        let s = c.step(ch, r);
        if t < s.end {
            let off = t - s.start;
            return Some((s, off, c));
        }
    }
}

fn valid_t(r: &Rules, t: i64) -> bool {
    t >= MIN_T && t >= r.epoch
}

/// The slot on air at t and how many seconds into it t is; None on an empty channel or before the epoch / a set clock.
pub fn now(ch: &ChannelDef, r: &Rules, t: i64) -> Option<(Slot, i64)> {
    if !valid_t(r, t) {
        return None;
    }
    now_from(Cursor::new(ch, r)?, ch, r, t).map(|(s, o, _)| (s, o))
}

/// Per-channel cursor cache for the daemon's polling: asking for a later t resumes from the last answer.
/// The key is the channel number plus epoch and padding mode; a rebuilt channel (different shows) must call
/// `forget` so a stale cursor is not reused.
#[derive(Default)]
#[allow(dead_code)] // not used yet: engine.rs replays `slots` from the epoch (~1 ms per call on the host in 2027)
pub struct Scheduler {
    /// Value: (end of the last answered slot, cursor just after it).
    cache: Mutex<HashMap<(u32, i64, bool), (i64, Cursor)>>,
}

#[allow(dead_code)]
impl Scheduler {
    pub fn now(&self, ch: &ChannelDef, r: &Rules, t: i64) -> Option<(Slot, i64)> {
        if !valid_t(r, t) {
            return None;
        }
        let key = (ch.number, r.epoch, pad_on(ch, r));
        let mut cache = self.cache.lock().unwrap();
        let start = match cache.get(&key) {
            Some((from_t, c)) if *from_t <= t => c.clone(),
            _ => Cursor::new(ch, r)?,
        };
        let (s, off, after) = now_from(start, ch, r, t)?;
        cache.insert(key, (s.end, after));
        Some((s, off))
    }

    pub fn forget(&self, number: u32) {
        self.cache.lock().unwrap().retain(|k, _| k.0 != number);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const EPOCH: i64 = 1_767_225_600 + 6 * 3600;

    fn ep(id: &str, secs: u32) -> Ep {
        Ep { id: id.into(), secs, title: id.into(), episode: String::new(), overview: String::new(), series: String::new() }
    }

    fn show(id: &str, n: usize, secs: u32) -> Show {
        Show { id: id.into(), eps: (0..n).map(|i| ep(&format!("{id}-{i}"), secs)).collect() }
    }

    fn chan(shows: Vec<Show>, filler: bool) -> ChannelDef {
        ChannelDef { number: 7, name: "Test".into(), collection_id: "c".into(), shows, filler }
    }

    fn rules() -> Rules {
        Rules { pad: None, break_secs: 300, epoch: EPOCH }
    }

    fn three() -> ChannelDef {
        chan(vec![show("a", 5, 1320), show("b", 5, 1500), show("c", 5, 2700)], true)
    }

    #[test]
    fn same_input_same_answer() {
        let t = EPOCH + 5 * 86_400 + 77;
        assert_eq!(now(&three(), &rules(), t), now(&three(), &rules(), t));
        assert!(now(&three(), &rules(), t).is_some());
    }

    #[test]
    fn pads_to_half_hour() {
        let r = rules();
        let v = slots(&three(), &r, EPOCH, EPOCH + 3 * 86_400);
        assert!(v.len() > 50);
        for s in &v {
            assert_eq!(s.start % 1800, EPOCH % 1800);
            assert_eq!((s.end - s.start) % 1800, 0);
            assert!(s.item_end <= s.end && s.item_end > s.start);
        }
    }

    #[test]
    fn no_pad_back_to_back() {
        let ch = chan(vec![show("a", 5, 1320), show("b", 5, 1500)], false);
        let r = rules();
        let v = slots(&ch, &r, EPOCH, EPOCH + 86_400);
        for w in v.windows(2) {
            assert_eq!(w[1].start, w[0].end);
        }
        for s in &v {
            assert_eq!(s.end - s.start, ch.shows[s.show].eps[s.ep].secs as i64);
            assert_eq!(s.item_end, s.end);
        }
    }

    #[test]
    fn pad_override_beats_filler_flag() {
        let ch = chan(vec![show("a", 5, 1320)], false);
        let r = Rules { pad: Some(true), ..rules() };
        assert_eq!(slots(&ch, &r, EPOCH, EPOCH + 1)[0].end - EPOCH, 1800);
        let ch = chan(vec![show("a", 5, 1320)], true);
        let r = Rules { pad: Some(false), ..rules() };
        assert_eq!(slots(&ch, &r, EPOCH, EPOCH + 1)[0].end - EPOCH, 1320);
    }

    #[test]
    fn offset_inside_slot() {
        let v = slots(&three(), &rules(), EPOCH, EPOCH + 86_400);
        let s = &v[4];
        let (got, off) = now(&three(), &rules(), s.start + 125).unwrap();
        assert_eq!(&got, s);
        assert_eq!(off, 125);
    }

    #[test]
    fn rotates_shows() {
        let v = slots(&three(), &rules(), EPOCH, EPOCH + 86_400);
        let mut first: Vec<usize> = v[..3].iter().map(|s| s.show).collect();
        first.sort();
        assert_eq!(first, vec![0, 1, 2]);
    }

    #[test]
    fn episodes_in_order() {
        let v = slots(&three(), &rules(), EPOCH, EPOCH + 10 * 86_400);
        for sh in 0..3 {
            let eps: Vec<usize> = v.iter().filter(|s| s.show == sh).map(|s| s.ep).collect();
            assert!(eps.len() > 10);
            for (i, e) in eps.iter().enumerate() {
                assert_eq!(*e, i % 5);
            }
        }
    }

    #[test]
    fn no_repeat_within_week() {
        let ch = chan(vec![show("a", 400, 1320)], true);
        let v = slots(&ch, &rules(), EPOCH, EPOCH + 7 * 86_400);
        let mut seen = std::collections::HashSet::new();
        for s in &v {
            assert!(seen.insert(s.ep), "episode {} repeated", s.ep);
        }
    }

    #[test]
    fn empty_channel_none() {
        let ch = chan(vec![], true);
        assert!(now(&ch, &rules(), EPOCH + 100).is_none());
        assert!(slots(&ch, &rules(), EPOCH, EPOCH + 9999).is_empty());
        let ch = chan(vec![Show { id: "x".into(), eps: vec![] }], true);
        assert!(now(&ch, &rules(), EPOCH + 100).is_none());
    }

    #[test]
    fn now_before_2020_is_none() {
        let r = Rules { epoch: 0, ..rules() };
        assert!(now(&three(), &r, 1_000_000).is_none());
        assert!(now(&three(), &rules(), EPOCH - 1).is_none());
        assert!(Scheduler::default().now(&three(), &rules(), 5).is_none());
    }

    #[test]
    fn cursor_matches_fresh() {
        let sch = Scheduler::default();
        let (ch, r) = (three(), rules());
        let t1 = EPOCH + 3 * 86_400 + 11;
        let t2 = t1 + 2 * 86_400 + 4000;
        assert_eq!(sch.now(&ch, &r, t1), now(&ch, &r, t1));
        assert_eq!(sch.now(&ch, &r, t2), now(&ch, &r, t2));
        // going backwards falls back to a fresh run
        assert_eq!(sch.now(&ch, &r, t1), now(&ch, &r, t1));
        assert_eq!(sch.now(&ch, &r, t2 + 1), now(&ch, &r, t2 + 1));
    }

    fn order_of(ch: &ChannelDef, round: u64) -> String {
        round_order(ch, round).iter().map(|&i| ch.shows[i].id.as_str()).collect()
    }

    #[test]
    fn golden_round_order() {
        let ch = chan(["a", "b", "c", "d", "e"].iter().map(|i| show(i, 2, 1200)).collect(), true);
        assert_eq!(order_of(&ch, 0), "cbdae");
        assert_eq!(order_of(&ch, 1), "deabc");
        // input order of the shows does not matter
        let rev = chan(["e", "c", "a", "d", "b"].iter().map(|i| show(i, 2, 1200)).collect(), true);
        assert_eq!(order_of(&rev, 0), order_of(&ch, 0));
        assert_eq!(order_of(&rev, 1), order_of(&ch, 1));
    }

    #[test]
    fn slot_edges() {
        let (ch, r) = (three(), rules());
        let v = slots(&ch, &r, EPOCH, EPOCH + 86_400);
        let s = &v[3];
        let (last, off) = now(&ch, &r, s.end - 1).unwrap();
        assert_eq!((&last, off), (s, s.end - 1 - s.start));
        let (next, off) = now(&ch, &r, s.end).unwrap();
        assert_eq!((&next, off), (&v[4], 0));
        let (first, off) = now(&ch, &r, EPOCH).unwrap();
        assert_eq!((&first, off), (&v[0], 0));
    }

    #[test]
    fn single_show_single_ep_repeats() {
        let ch = chan(vec![show("a", 1, 1320)], true);
        let v = slots(&ch, &rules(), EPOCH, EPOCH + 86_400);
        assert!(v.len() > 40);
        assert!(v.iter().all(|s| s.ep == 0 && s.show == 0));
    }

    #[test]
    fn cursor_matches_fresh_at_edges_and_after_forget() {
        let sch = Scheduler::default();
        let (ch, r) = (three(), rules());
        let v = slots(&ch, &r, EPOCH, EPOCH + 3 * 86_400);
        for s in v.iter().step_by(7) {
            for t in [s.start, s.end - 1, s.end] {
                assert_eq!(sch.now(&ch, &r, t), now(&ch, &r, t), "t={t}");
            }
        }
        sch.forget(ch.number);
        let t = v[20].end;
        assert_eq!(sch.now(&ch, &r, t), now(&ch, &r, t));
        assert_eq!(sch.now(&ch, &r, t + 1), now(&ch, &r, t + 1));
    }

    #[test]
    fn epoch_for_zones() {
        assert_eq!(epoch_for(6 * 3600, 0), 1_767_225_600 + 21_600);
        assert_eq!(epoch_for(0, 3600), 1_767_225_600 - 3600);
    }
}
