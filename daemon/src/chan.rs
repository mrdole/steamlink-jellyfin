//! Channel collections and the daily snapshot. Jellyfin collections (BoxSets) named "Channel 10 Classic Sitcoms"
//! become TV channels; collections named "Bumpers" / "Commercials" (optionally with a channel number) are filler
//! pools. `load_snapshot` reads them once per schedule day; `snapshot_from_json` is the pure core.
use crate::sched::{ChannelDef, Ep, Show};
use crate::{log, Api, Cfg};
use serde_json::{json, Value};

pub const CHANNELS_FILE: &str = "/mnt/scratch/sljf/cache/channels.json";

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum PoolKind {
    Bumpers,
    Commercials,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ServerClip {
    pub id: String,
    pub secs: f64,
}

pub struct Snapshot {
    pub day: i64,
    pub channels: Vec<ChannelDef>,
    pub pools: Vec<(PoolKind, Option<u32>, Vec<ServerClip>)>,
}

/// "Channel 10 Classic Sitcoms" -> (10, "Classic Sitcoms"); "channel 7" -> (7, "Channel 7").
pub fn parse_channel_name(name: &str, prefix: &str) -> Option<(u32, String)> {
    let name = name.trim();
    let prefix = prefix.trim();
    let head = name.get(..prefix.len())?;
    if prefix.is_empty() || !head.eq_ignore_ascii_case(prefix) {
        return None;
    }
    let rest = &name[prefix.len()..];
    if !rest.starts_with(char::is_whitespace) {
        return None;
    }
    let rest = rest.trim_start();
    let (num, title) = match rest.find(char::is_whitespace) {
        Some(i) => (&rest[..i], rest[i..].trim()),
        None => (rest, ""),
    };
    if num.is_empty() || !num.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let n: u32 = num.parse().ok()?;
    let title = if title.is_empty() { format!("{prefix} {n}") } else { title.to_string() };
    Some((n, title))
}

/// "Bumpers", "Bumpers 14", "Commercials", "Commercials 14" (case-insensitive); "Bumpers14" is not a pool.
pub fn pool_name(name: &str) -> Option<(PoolKind, Option<u32>)> {
    let name = name.trim();
    for (word, kind) in [("bumpers", PoolKind::Bumpers), ("commercials", PoolKind::Commercials)] {
        let Some(head) = name.get(..word.len()) else { continue };
        if !head.eq_ignore_ascii_case(word) {
            continue;
        }
        let rest = &name[word.len()..];
        if rest.is_empty() {
            return Some((kind, None));
        }
        if !rest.starts_with(char::is_whitespace) {
            return None;
        }
        let num = rest.trim();
        if !num.is_empty() && num.bytes().all(|b| b.is_ascii_digit()) {
            return num.parse().ok().map(|n| (kind, Some(n)));
        }
        return None;
    }
    None
}

fn secs_of(item: &Value) -> Option<u32> {
    let t = item["RunTimeTicks"].as_i64()?;
    let s = t / 10_000_000;
    if s > 0 { u32::try_from(s).ok() } else { None }
}

fn ep_from(item: &Value) -> Option<Ep> {
    let id = item["Id"].as_str()?.to_string();
    let secs = secs_of(item)?;
    let episode = match (item["ParentIndexNumber"].as_i64(), item["IndexNumber"].as_i64()) {
        (Some(s), Some(e)) => format!("S{s}E{e}"),
        _ => String::new(),
    };
    Some(Ep {
        id,
        secs,
        title: item["Name"].as_str().unwrap_or("").to_string(),
        episode,
        overview: item["Overview"].as_str().unwrap_or("").to_string(),
    })
}

fn items(v: &Value) -> &[Value] {
    v["Items"].as_array().map(|a| a.as_slice()).unwrap_or(&[])
}

/// Episodes of a series (from `episodes_json`), specials dropped, in season/episode order, optionally only one season.
fn series_eps(episodes_json: &Value, season_id: Option<&str>) -> Vec<Ep> {
    let mut v: Vec<(i64, i64, Ep)> = items(episodes_json)
        .iter()
        .filter(|e| e["ParentIndexNumber"].as_i64() != Some(0))
        .filter(|e| season_id.is_none_or(|s| e["SeasonId"].as_str() == Some(s)))
        .filter_map(|e| {
            let ep = ep_from(e)?;
            Some((e["ParentIndexNumber"].as_i64().unwrap_or(0), e["IndexNumber"].as_i64().unwrap_or(0), ep))
        })
        .collect();
    v.sort_by_key(|(s, n, _)| (*s, *n));
    v.into_iter().map(|(_, _, e)| e).collect()
}

fn shows_of(children_json: &Value, episodes: &dyn Fn(&str) -> Value) -> Vec<Show> {
    let mut shows = Vec::new();
    for c in items(children_json) {
        let Some(id) = c["Id"].as_str() else { continue };
        let eps = match c["Type"].as_str().unwrap_or("") {
            "Series" => series_eps(&episodes(id), None),
            "Season" => match c["SeriesId"].as_str() {
                Some(sid) => series_eps(&episodes(sid), Some(id)),
                None => Vec::new(),
            },
            "Movie" | "Video" | "Episode" => ep_from(c).into_iter().collect(),
            _ => Vec::new(),
        };
        if !eps.is_empty() {
            shows.push(Show { id: id.to_string(), eps });
        }
    }
    shows
}

fn clips_of(children_json: &Value) -> Vec<ServerClip> {
    items(children_json)
        .iter()
        .filter_map(|c| {
            let id = c["Id"].as_str()?.to_string();
            let t = c["RunTimeTicks"].as_i64()?;
            (t > 0).then(|| ServerClip { id, secs: t as f64 / 10_000_000.0 })
        })
        .collect()
}

pub fn snapshot_from_json(
    collections: &Value,
    children: &dyn Fn(&str) -> Value,
    episodes: &dyn Fn(&str) -> Value,
    prefix: &str,
    day: i64,
) -> Snapshot {
    let mut cols: Vec<(&str, &str)> = items(collections)
        .iter()
        .filter_map(|c| Some((c["Id"].as_str()?, c["Name"].as_str()?)))
        .collect();
    cols.sort();
    let mut channels: Vec<ChannelDef> = Vec::new();
    let mut pools = Vec::new();
    for (id, name) in cols {
        if let Some((number, title)) = parse_channel_name(name, prefix) {
            if channels.iter().any(|c| c.number == number) {
                log(&format!("channels: duplicate number {number} ({id}) ignored"));
                continue;
            }
            let shows = shows_of(&children(id), episodes);
            if shows.is_empty() {
                log(&format!("channels: {name} has nothing playable, skipped"));
                continue;
            }
            channels.push(ChannelDef { number, name: title, collection_id: id.to_string(), shows, filler: false });
        } else if let Some((kind, n)) = pool_name(name) {
            let clips = clips_of(&children(id));
            if !clips.is_empty() {
                pools.push((kind, n, clips));
            }
        }
    }
    channels.sort_by_key(|c| c.number);
    for c in &mut channels {
        c.filler = pools.iter().any(|(_, n, _)| n.is_none() || *n == Some(c.number));
    }
    Snapshot { day, channels, pools }
}

pub fn load_snapshot(api: &Api, cfg: &Cfg, day: i64) -> Result<Snapshot, String> {
    let uid = &cfg.user_id;
    let cols = api.get(&format!("/Users/{uid}/Items?IncludeItemTypes=BoxSet&Recursive=true&Fields="))?;
    let children = |cid: &str| {
        api.get(&format!("/Users/{uid}/Items?ParentId={cid}&Fields=Overview")).unwrap_or_else(|e| {
            log(&format!("channels: {e}"));
            Value::Null
        })
    };
    let episodes = |sid: &str| {
        api.get(&format!("/Shows/{sid}/Episodes?UserId={uid}&Fields=Overview")).unwrap_or_else(|e| {
            log(&format!("channels: {e}"));
            Value::Null
        })
    };
    let snap = snapshot_from_json(&cols, &children, &episodes, &cfg.channel_prefix, day);
    let _ = std::fs::create_dir_all(crate::local::CACHE);
    if let Err(e) = std::fs::write(CHANNELS_FILE, snapshot_to_json(&snap).to_string()) {
        log(&format!("channels: cannot write {CHANNELS_FILE}: {e}"));
    }
    Ok(snap)
}

fn kind_str(k: PoolKind) -> &'static str {
    match k {
        PoolKind::Bumpers => "bumpers",
        PoolKind::Commercials => "commercials",
    }
}

pub fn snapshot_to_json(s: &Snapshot) -> Value {
    let channels: Vec<Value> = s
        .channels
        .iter()
        .map(|c| {
            let shows: Vec<Value> = c
                .shows
                .iter()
                .map(|sh| {
                    let eps: Vec<Value> = sh
                        .eps
                        .iter()
                        .map(|e| json!({"id": e.id, "secs": e.secs, "title": e.title, "episode": e.episode, "overview": e.overview}))
                        .collect();
                    json!({"id": sh.id, "eps": eps})
                })
                .collect();
            json!({"number": c.number, "name": c.name, "collection_id": c.collection_id, "filler": c.filler, "shows": shows})
        })
        .collect();
    let pools: Vec<Value> = s
        .pools
        .iter()
        .map(|(k, n, clips)| {
            let cl: Vec<Value> = clips.iter().map(|c| json!({"id": c.id, "secs": c.secs})).collect();
            json!({"kind": kind_str(*k), "number": n, "clips": cl})
        })
        .collect();
    json!({"day": s.day, "channels": channels, "pools": pools})
}

pub fn snapshot_from_saved(v: &Value) -> Option<Snapshot> {
    let day = v["day"].as_i64()?;
    let mut channels = Vec::new();
    for c in v["channels"].as_array()? {
        let mut shows = Vec::new();
        for sh in c["shows"].as_array()? {
            let mut eps = Vec::new();
            for e in sh["eps"].as_array()? {
                eps.push(Ep {
                    id: e["id"].as_str()?.to_string(),
                    secs: u32::try_from(e["secs"].as_u64()?).ok()?,
                    title: e["title"].as_str()?.to_string(),
                    episode: e["episode"].as_str()?.to_string(),
                    overview: e["overview"].as_str()?.to_string(),
                });
            }
            shows.push(Show { id: sh["id"].as_str()?.to_string(), eps });
        }
        channels.push(ChannelDef {
            number: u32::try_from(c["number"].as_u64()?).ok()?,
            name: c["name"].as_str()?.to_string(),
            collection_id: c["collection_id"].as_str()?.to_string(),
            shows,
            filler: c["filler"].as_bool()?,
        });
    }
    let mut pools = Vec::new();
    for p in v["pools"].as_array()? {
        let kind = match p["kind"].as_str()? {
            "bumpers" => PoolKind::Bumpers,
            "commercials" => PoolKind::Commercials,
            _ => return None,
        };
        let n = match &p["number"] {
            Value::Null => None,
            x => Some(u32::try_from(x.as_u64()?).ok()?),
        };
        let mut clips = Vec::new();
        for c in p["clips"].as_array()? {
            clips.push(ServerClip { id: c["id"].as_str()?.to_string(), secs: c["secs"].as_f64()? });
        }
        pools.push((kind, n, clips));
    }
    Some(Snapshot { day, channels, pools })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn j(s: &str) -> Value {
        serde_json::from_str(s).unwrap()
    }

    #[test]
    fn channel_name_parses() {
        assert_eq!(parse_channel_name("Channel 10 Classic Sitcoms", "Channel"), Some((10, "Classic Sitcoms".into())));
        assert_eq!(parse_channel_name("channel 7", "Channel"), Some((7, "Channel 7".into())));
        assert_eq!(parse_channel_name("Chan 3 X", "Channel"), None);
        assert_eq!(parse_channel_name("Channel x Y", "Channel"), None);
        assert_eq!(parse_channel_name("Channel10 Y", "Channel"), None);
    }

    #[test]
    fn pool_names() {
        assert_eq!(pool_name("Bumpers"), Some((PoolKind::Bumpers, None)));
        assert_eq!(pool_name("Commercials 14"), Some((PoolKind::Commercials, Some(14))));
        assert_eq!(pool_name("Bumpers14"), None);
        assert_eq!(pool_name("Bumpers x"), None);
    }

    fn fixture() -> Snapshot {
        let cols = j(r#"{"Items":[
            {"Id":"c2","Name":"Channel 10 Dup"},
            {"Id":"c1","Name":"Channel 10 Classic Sitcoms"},
            {"Id":"c3","Name":"Channel 2 Movies"},
            {"Id":"c4","Name":"Bumpers"},
            {"Id":"c5","Name":"Commercials 2"},
            {"Id":"c6","Name":"Other"}]}"#);
        let children = |cid: &str| match cid {
            "c1" => j(r#"{"Items":[{"Id":"s1","Type":"Series","Name":"Show"},
                {"Id":"sea9","Type":"Season","SeriesId":"s9","Name":"Season 2"}]}"#),
            "c2" => j(r#"{"Items":[{"Id":"m9","Type":"Movie","Name":"X","RunTimeTicks":6000000000}]}"#),
            "c3" => j(r#"{"Items":[{"Id":"m1","Type":"Movie","Name":"Film","Overview":"o","RunTimeTicks":54000000000}]}"#),
            "c4" => j(r#"{"Items":[{"Id":"b1","RunTimeTicks":100000000},{"Id":"b2"}]}"#),
            "c5" => j(r#"{"Items":[{"Id":"k1","RunTimeTicks":300000000}]}"#),
            _ => Value::Null,
        };
        let episodes = |sid: &str| match sid {
            "s1" => j(r#"{"Items":[
                {"Id":"e3","Name":"C","ParentIndexNumber":2,"IndexNumber":1,"RunTimeTicks":13200000000},
                {"Id":"e0","Name":"Special","ParentIndexNumber":0,"IndexNumber":1,"RunTimeTicks":13200000000},
                {"Id":"e2","Name":"B","ParentIndexNumber":1,"IndexNumber":2,"RunTimeTicks":13200000000},
                {"Id":"e1","Name":"A","ParentIndexNumber":1,"IndexNumber":1,"RunTimeTicks":13200000000},
                {"Id":"eX","Name":"NoLen","ParentIndexNumber":1,"IndexNumber":3}]}"#),
            "s9" => j(r#"{"Items":[
                {"Id":"f1","Name":"F1","ParentIndexNumber":1,"IndexNumber":1,"SeasonId":"sea8","RunTimeTicks":9000000000},
                {"Id":"f2","Name":"F2","ParentIndexNumber":2,"IndexNumber":1,"SeasonId":"sea9","RunTimeTicks":9000000000}]}"#),
            _ => Value::Null,
        };
        snapshot_from_json(&cols, &children, &episodes, "Channel", 5)
    }

    #[test]
    fn snapshot_builds_channels() {
        let s = fixture();
        assert_eq!(s.day, 5);
        assert_eq!(s.channels.iter().map(|c| c.number).collect::<Vec<_>>(), vec![2, 10]);
        let c10 = &s.channels[1];
        assert_eq!(c10.collection_id, "c1");
        assert_eq!(c10.name, "Classic Sitcoms");
        assert_eq!(c10.shows.len(), 2);
        let ids: Vec<&str> = c10.shows[0].eps.iter().map(|e| e.id.as_str()).collect();
        assert_eq!(ids, vec!["e1", "e2", "e3"]);
        assert_eq!(c10.shows[0].eps[0].episode, "S1E1");
        assert_eq!(c10.shows[0].eps[0].secs, 1320);
        assert_eq!(c10.shows[1].id, "sea9");
        assert_eq!(c10.shows[1].eps.len(), 1);
        assert_eq!(c10.shows[1].eps[0].id, "f2");
        let c2 = &s.channels[0];
        assert_eq!(c2.shows[0].eps[0].secs, 5400);
        assert_eq!(c2.shows[0].eps[0].episode, "");
        assert!(c2.filler && c10.filler);
        assert_eq!(s.pools.len(), 2);
        assert_eq!(s.pools[0].2, vec![ServerClip { id: "b1".into(), secs: 10.0 }]);
    }

    #[test]
    fn empty_collection_skipped() {
        let cols = j(r#"{"Items":[{"Id":"c1","Name":"Channel 4 Empty"}]}"#);
        let s = snapshot_from_json(&cols, &|_| j(r#"{"Items":[]}"#), &|_| Value::Null, "Channel", 1);
        assert!(s.channels.is_empty());
    }

    #[test]
    fn filler_only_for_matching_pool() {
        let cols = j(r#"{"Items":[{"Id":"c1","Name":"Channel 4 A"},{"Id":"c2","Name":"Commercials 9"}]}"#);
        let ch = |cid: &str| match cid {
            "c1" => j(r#"{"Items":[{"Id":"m","Type":"Movie","RunTimeTicks":600000000}]}"#),
            _ => j(r#"{"Items":[{"Id":"k","RunTimeTicks":600000000}]}"#),
        };
        let s = snapshot_from_json(&cols, &ch, &|_| Value::Null, "Channel", 1);
        assert!(!s.channels[0].filler);
    }

    #[test]
    fn snapshot_round_trips() {
        let s = fixture();
        let v = snapshot_to_json(&s);
        let r = snapshot_from_saved(&v).unwrap();
        assert_eq!(snapshot_to_json(&r), v);
        assert_eq!(r.channels[1].shows[0].eps[2].title, "C");
        assert_eq!(r.pools[1].1, Some(2));
        assert!(snapshot_from_saved(&j("{}")).is_none());
    }
}
