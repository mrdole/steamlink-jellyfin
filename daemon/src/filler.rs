//! Gap filler: what plays between the end of an item and the next half-hour slot (bumpers, commercials).
//! Pure: no I/O. Deterministic for a given seed, so every box gets the same slot lengths.
use crate::chan::{PoolKind, ServerClip};

#[derive(Debug, Clone, PartialEq)]
pub enum ClipLoc {
    /// Absolute path on the stick.
    Stick(String),
    /// Jellyfin item id.
    Server(String),
}

#[derive(Debug, Clone, PartialEq)]
pub struct Clip {
    pub loc: ClipLoc,
    pub secs: f64,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Fill {
    /// `clip` indexes the pool slice used; `limit` is the seconds to play (clip.secs when played whole).
    Clip { clip: usize, limit: f64 },
    Card { secs: f64 },
}

/// Below this many seconds left, nothing is played.
const MIN_CUT: f64 = 2.0;

fn splitmix64(state: &mut u64) -> u64 {
    *state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut z = *state;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

pub fn seed(number: u32, slot_start: i64) -> u64 {
    let mut st = ((number as u64) << 32) ^ slot_start as u64;
    splitmix64(&mut st)
}

fn shuffled(pool: &[Clip], st: &mut u64) -> Vec<usize> {
    let mut v: Vec<usize> = (0..pool.len()).filter(|&i| pool[i].secs > 0.0).collect();
    let n = v.len();
    for i in (1..n).rev() {
        let j = (splitmix64(st) % (i as u64 + 1)) as usize;
        v.swap(i, j);
    }
    v
}

/// Walks a shuffled pool; reshuffles only after every clip was used once.
struct Walker {
    n: usize,
    order: Vec<usize>,
    pos: usize,
}

impl Walker {
    fn new(pool: &[Clip], st: &mut u64) -> Walker {
        let order = shuffled(pool, st);
        Walker { n: order.len(), order, pos: 0 }
    }
    /// Next clip that fits in `left` whole, scanning the rest of the current round only (no repeats).
    /// Wraps into a new round once the current one is used up.
    fn next_fitting(&mut self, pool: &[Clip], left: f64, st: &mut u64) -> Option<usize> {
        if self.n == 0 {
            return None;
        }
        for round in 0..2 {
            if let Some(off) = self.order[self.pos..].iter().position(|&i| pool[i].secs <= left + 1e-9) {
                let idx = self.order[self.pos + off];
                self.order.swap(self.pos, self.pos + off);
                self.pos += 1;
                return Some(idx);
            }
            if round == 0 {
                if self.pos == 0 {
                    return None;
                }
                self.order = shuffled(pool, st);
                self.pos = 0;
            }
        }
        None
    }
    /// Next clip in order regardless of fit (to be cut).
    fn next_any(&mut self, pool: &[Clip], st: &mut u64) -> Option<usize> {
        if self.n == 0 {
            return None;
        }
        if self.pos >= self.n {
            self.order = shuffled(pool, st);
            self.pos = 0;
        }
        self.pos += 1;
        Some(self.order[self.pos - 1])
    }
}

/// Fills `gap_secs`: commercials (up to `break_secs`), then bumpers. The bool is true for commercials.
pub fn fill_gap(gap_secs: f64, commercials: &[Clip], bumpers: &[Clip], break_secs: f64, seed: u64) -> Vec<(bool, Fill)> {
    let mut out = Vec::new();
    if gap_secs < MIN_CUT {
        return out;
    }
    if !commercials.iter().chain(bumpers).any(|c| c.secs > 0.0) {
        return vec![(false, Fill::Card { secs: gap_secs })];
    }
    let mut st = seed;
    let mut left = gap_secs;

    let mut cw = Walker::new(commercials, &mut st);
    let mut c_used = 0.0;
    while left >= MIN_CUT {
        let room = (break_secs - c_used).min(left);
        let Some(i) = cw.next_fitting(commercials, room, &mut st) else { break };
        let s = commercials[i].secs;
        out.push((true, Fill::Clip { clip: i, limit: s }));
        c_used += s;
        left -= s;
    }
    let mut bw = Walker::new(bumpers, &mut st);
    while left >= MIN_CUT && bw.n > 0 {
        if let Some(i) = bw.next_fitting(bumpers, left, &mut st) {
            let s = bumpers[i].secs;
            out.push((false, Fill::Clip { clip: i, limit: s }));
            left -= s;
        } else if let Some(i) = bw.next_any(bumpers, &mut st) {
            out.push((false, Fill::Clip { clip: i, limit: left }));
            left = 0.0;
        } else {
            break;
        }
    }
    out
}

fn from_server(v: &[ServerClip]) -> Vec<Clip> {
    v.iter().map(|c| Clip { loc: ClipLoc::Server(c.id.clone()), secs: c.secs }).collect()
}

/// (commercials, bumpers) for a channel: stick clips for this number if any, else stick "all", else the server's
/// pools for this number, else the server's global pools; decided independently per kind.
pub fn pick_pools(
    stick: &[(PoolKind, Option<u32>, Clip)],
    server: &[(PoolKind, Option<u32>, Vec<ServerClip>)],
    number: u32,
) -> (Vec<Clip>, Vec<Clip>) {
    let one = |kind: PoolKind| -> Vec<Clip> {
        let st = |n: Option<u32>| -> Vec<Clip> {
            stick.iter().filter(|(k, num, _)| *k == kind && *num == n).map(|(_, _, c)| c.clone()).collect()
        };
        let sv = |n: Option<u32>| -> Vec<Clip> {
            server.iter().filter(|(k, num, _)| *k == kind && *num == n).flat_map(|(_, _, v)| from_server(v)).collect()
        };
        [st(Some(number)), st(None), sv(Some(number)), sv(None)].into_iter().find(|v| !v.is_empty()).unwrap_or_default()
    };
    (one(PoolKind::Commercials), one(PoolKind::Bumpers))
}

/// "probe ok 12.480 1280x720 h264 aac" -> (secs, w, h, true); anything else (e.g. "probe bad ...") -> None.
pub fn probe_line(line: &str) -> Option<(f64, u32, u32, bool)> {
    let mut it = line.split_whitespace();
    if it.next()? != "probe" || it.next()? != "ok" {
        return None;
    }
    let secs: f64 = it.next()?.parse().ok()?;
    let (w, h) = it.next()?.split_once('x')?;
    Some((secs, w.parse().ok()?, h.parse().ok()?, true))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn clips(secs: &[f64]) -> Vec<Clip> {
        secs.iter().enumerate().map(|(i, &s)| Clip { loc: ClipLoc::Stick(format!("/c/{i}.mp4")), secs: s }).collect()
    }
    fn total(f: &[(bool, Fill)]) -> f64 {
        f.iter().map(|(_, x)| match x { Fill::Clip { limit, .. } => *limit, Fill::Card { secs } => *secs }).sum()
    }

    #[test]
    fn fill_exact_gap() {
        let b = clips(&[10.0, 20.0, 30.0]);
        let f = fill_gap(60.0, &[], &b, 0.0, 7);
        let t = total(&f);
        assert!(t <= 60.0 + 1e-9 && t >= 58.0, "{t}");
    }

    #[test]
    fn fill_cuts_last() {
        let b = clips(&[30.0]);
        let f = fill_gap(45.0, &[], &b, 0.0, 1);
        assert_eq!(f, vec![(false, Fill::Clip { clip: 0, limit: 30.0 }), (false, Fill::Clip { clip: 0, limit: 15.0 })]);
    }

    #[test]
    fn fill_commercials_first() {
        let c = clips(&[15.0, 15.0, 15.0]);
        let b = clips(&[10.0]);
        let f = fill_gap(120.0, &c, &b, 30.0, 3);
        assert!(f[0].0 && f[1].0 && !f[2].0, "{f:?}");
        assert_eq!(f.iter().filter(|x| x.0).count(), 2);
    }

    #[test]
    fn fill_no_clips_card() {
        assert_eq!(fill_gap(42.0, &[], &[], 30.0, 1), vec![(false, Fill::Card { secs: 42.0 })]);
    }

    #[test]
    fn fill_tiny_gap_nothing() {
        assert!(fill_gap(1.5, &clips(&[5.0]), &clips(&[5.0]), 30.0, 1).is_empty());
    }

    #[test]
    fn fill_deterministic_no_repeat_inside_pool() {
        let b = clips(&[5.0, 6.0, 7.0, 8.0]);
        let a = fill_gap(26.0, &[], &b, 0.0, 99);
        assert_eq!(a, fill_gap(26.0, &[], &b, 0.0, 99));
        let mut idx: Vec<usize> = a.iter().filter_map(|(_, f)| if let Fill::Clip { clip, .. } = f { Some(*clip) } else { None }).collect();
        idx.sort();
        idx.dedup();
        assert_eq!(idx.len(), a.len());
    }

    #[test]
    fn seed_differs() {
        assert_ne!(seed(1, 1000), seed(2, 1000));
        assert_ne!(seed(1, 1000), seed(1, 2800));
    }

    fn sc(p: &str) -> Clip {
        Clip { loc: ClipLoc::Stick(p.into()), secs: 5.0 }
    }

    #[test]
    fn pools_prefer_stick() {
        let stick = vec![(PoolKind::Bumpers, None, sc("/b/all.mp4"))];
        let server = vec![(PoolKind::Bumpers, None, vec![ServerClip { id: "x".into(), secs: 9.0 }]),
                          (PoolKind::Commercials, None, vec![ServerClip { id: "c".into(), secs: 9.0 }])];
        let (c, b) = pick_pools(&stick, &server, 4);
        assert_eq!(b, vec![sc("/b/all.mp4")]);
        assert_eq!(c, vec![Clip { loc: ClipLoc::Server("c".into()), secs: 9.0 }]);
    }

    #[test]
    fn pools_channel_specific_or_global() {
        let stick = vec![(PoolKind::Bumpers, None, sc("/b/all.mp4")), (PoolKind::Bumpers, Some(4), sc("/b/4.mp4"))];
        assert_eq!(pick_pools(&stick, &[], 4).1, vec![sc("/b/4.mp4")]);
        assert_eq!(pick_pools(&stick, &[], 5).1, vec![sc("/b/all.mp4")]);
        let server = vec![(PoolKind::Commercials, None, vec![ServerClip { id: "g".into(), secs: 9.0 }]),
                          (PoolKind::Commercials, Some(4), vec![ServerClip { id: "n".into(), secs: 9.0 }])];
        assert_eq!(pick_pools(&[], &server, 4).0[0].loc, ClipLoc::Server("n".into()));
        assert_eq!(pick_pools(&[], &server, 5).0[0].loc, ClipLoc::Server("g".into()));
    }

    #[test]
    fn probe_line_parses() {
        assert_eq!(probe_line("probe ok 12.480 1280x720 h264 aac"), Some((12.48, 1280, 720, true)));
        assert_eq!(probe_line("probe bad hevc"), None);
        assert_eq!(probe_line("hello"), None);
    }
}
