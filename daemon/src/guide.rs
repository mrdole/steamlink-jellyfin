//! TV guide data for the on-TV app: Jellyfin's Live TV channels and programmes folded into one list
//! (`GET /guide`, see local.rs). Dates are epoch seconds; no date crate, just calendar arithmetic.
use serde_json::{Value, json};

/// Days since 1970-01-01 of a civil date (proleptic Gregorian).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (yoe + era * 400 + if m <= 2 { 1 } else { 0 }, m, d)
}

/// Epoch seconds -> "2026-11-21T03:00:00Z".
pub fn iso_utc(secs: i64) -> String {
    let (y, m, d) = civil_from_days(secs.div_euclid(86_400));
    let t = secs.rem_euclid(86_400);
    format!("{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z", t / 3600, t % 3600 / 60, t % 60)
}

/// Jellyfin's "2026-11-21T03:00:00.0000000Z" (UTC, fraction optional) -> epoch seconds.
pub fn parse_iso(s: &str) -> Option<i64> {
    let b = s.as_bytes();
    if b.len() < 19 || b[4] != b'-' || b[7] != b'-' || !(b[10] == b'T' || b[10] == b' ') || b[13] != b':' || b[16] != b':' {
        return None;
    }
    let n = |a: usize, z: usize| s.get(a..z).filter(|x| x.bytes().all(|c| c.is_ascii_digit())).and_then(|x| x.parse::<i64>().ok());
    let (y, mo, d, h, mi, se) = (n(0, 4)?, n(5, 7)?, n(8, 10)?, n(11, 13)?, n(14, 16)?, n(17, 19)?);
    if !(1..=12).contains(&mo) || !(1..=31).contains(&d) || h > 23 || mi > 59 || se > 60 {
        return None;
    }
    Some(days_from_civil(y, mo, d) * 86_400 + h * 3600 + mi * 60 + se)
}

/// Channels (sorted by number) each with their programmes (sorted by start). A channel with no guide data keeps
/// `programs: []`; the app shows "No information" for it.
pub fn guide_json(channels: &Value, programs: &Value, now: i64) -> Value {
    let items = |v: &Value| -> Vec<Value> { if v.is_array() { v.as_array() } else { v["Items"].as_array() }.cloned().unwrap_or_default() };
    let chan_items = items(channels);
    let prog_items = items(programs);
    // one pass over the programmes (hundreds of channels x thousands of programmes), borrowing rather than cloning
    let mut by_chan: std::collections::HashMap<&str, Vec<&Value>> = std::collections::HashMap::new();
    for p in &prog_items {
        if let Some(c) = p["ChannelId"].as_str() {
            by_chan.entry(c).or_default().push(p);
        }
    }
    let mut chans: Vec<Value> = chan_items.iter().map(|c| {
        let id = c["Id"].as_str().unwrap_or("");
        let mut progs: Vec<Value> = by_chan.get(id).map(|v| v.as_slice()).unwrap_or(&[]).iter()
            .filter_map(|p| {
                let start = parse_iso(p["StartDate"].as_str()?)?;
                let end = parse_iso(p["EndDate"].as_str()?)?;
                Some(json!({
                    "id": p["Id"], "name": p["Name"], "episode": p["EpisodeTitle"], "start": start, "end": end,
                    "overview": p["Overview"].as_str().map(|s| s.chars().take(400).collect::<String>()),
                }))
            }).collect();
        progs.sort_by_key(|p| p["start"].as_i64());
        json!({"id": c["Id"], "number": c["ChannelNumber"].as_str().or(c["Number"].as_str()), "name": c["Name"], "programs": progs})
    }).collect();
    // numeric numbers first in numeric order ("2" before "12"), anything else by text after them
    chans.sort_by_cached_key(|c| {
        let n = c["number"].as_str().unwrap_or("");
        // plain digits sort numerically first; anything else ("nan", "1.1", "ABC") after them, by text
        match n.parse::<u32>() { Ok(v) if n.bytes().all(|b| b.is_ascii_digit()) => (0u8, v, String::new()), _ => (1u8, 0, n.to_string()) }
    });
    json!({"now": now, "channels": chans})
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn iso_round_trip() {
        assert_eq!(parse_iso(&iso_utc(1_795_230_000)), Some(1_795_230_000));
        assert_eq!(parse_iso("2026-11-21T03:00:00.0000000Z"), Some(1_795_230_000));
        assert_eq!(iso_utc(1_795_230_000), "2026-11-21T03:00:00Z");
        assert_eq!(parse_iso("garbage"), None);
    }

    fn prog(id: &str, ch: &str, name: &str, s: &str, e: &str) -> Value {
        json!({"Id": id, "ChannelId": ch, "Name": name, "StartDate": s, "EndDate": e})
    }

    #[test]
    fn guide_groups_and_sorts() {
        let channels = json!({"Items": [
            {"Id": "c12", "ChannelNumber": "12", "Name": "Twelve"},
            {"Id": "c2", "ChannelNumber": "2", "Name": "Two"},
        ]});
        let programs = json!({"Items": [
            prog("p2", "c2", "Later", "2026-11-21T04:00:00.0000000Z", "2026-11-21T05:00:00.0000000Z"),
            prog("p1", "c2", "Sooner", "2026-11-21T03:00:00.0000000Z", "2026-11-21T04:00:00.0000000Z"),
            prog("p3", "cX", "Other", "2026-11-21T03:00:00.0000000Z", "2026-11-21T04:00:00.0000000Z"),
        ]});
        let g = guide_json(&channels, &programs, 1_795_230_000);
        assert_eq!(g["now"], 1_795_230_000);
        let ch = g["channels"].as_array().unwrap();
        let numbers: Vec<&str> = ch.iter().map(|c| c["number"].as_str().unwrap()).collect();
        assert_eq!(numbers, ["2", "12"]);
        let names: Vec<&str> = ch[0]["programs"].as_array().unwrap().iter().map(|p| p["name"].as_str().unwrap()).collect();
        assert_eq!(names, ["Sooner", "Later"]);
        assert_eq!(ch[0]["programs"][0]["start"], 1_795_230_000);
        assert_eq!(ch[0]["programs"][0]["end"], 1_795_233_600);
        assert_eq!(ch[1]["programs"], json!([]));
    }

    #[test]
    fn guide_odd_numbers_sort_last() {
        let channels = json!({"Items": [
            {"Id": "a", "ChannelNumber": "nan", "Name": "A"},
            {"Id": "b", "ChannelNumber": "10", "Name": "B"},
            {"Id": "c", "ChannelNumber": "inf", "Name": "C"},
            {"Id": "d", "ChannelNumber": "9", "Name": "D"},
            {"Id": "e", "ChannelNumber": "1e3", "Name": "E"},
        ]});
        let g = guide_json(&channels, &json!({"Items": []}), 0);
        let nums: Vec<&str> = g["channels"].as_array().unwrap().iter().map(|c| c["number"].as_str().unwrap()).collect();
        assert_eq!(nums, ["9", "10", "1e3", "inf", "nan"]);
    }

    #[test]
    fn guide_episode_name() {
        let channels = json!({"Items": [{"Id": "c1", "ChannelNumber": "1", "Name": "One"}]});
        let mut p = prog("p1", "c1", "The Simpsons", "2026-11-21T03:00:00Z", "2026-11-21T03:30:00Z");
        p["EpisodeTitle"] = json!("Homer's Odyssey");
        let g = guide_json(&channels, &json!({"Items": [p]}), 0);
        assert_eq!(g["channels"][0]["programs"][0]["episode"], "Homer's Odyssey");
    }
}
