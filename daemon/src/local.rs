//! Local-only HTTP interface (127.0.0.1:8097) for the on-TV app (app/): the app never talks to the
//! internet itself, so the Link's old system TLS is not involved and the Jellyfin token stays in this process.
//!
//!   GET /home                      {resume:[..], nextup:[..], channels:[..], libraries:[..]}  (channels = Live TV)
//!   GET /items?parent=ID&start=N   children of a library / series / season (60 per page)
//!   GET /search?q=TEXT
//!   GET /image?id=ID&w=300[&type=Primary|Backdrop|Thumb]     JPEG, cached in /mnt/scratch/sljf/cache
//!   GET /detail?id=ID              tracks for the play dialog: {audio:[{index,name,lang}], audio_default,
//!                                  subs:[{index,name,lang,text}], sub_default (-1 = off)}
//!   GET /play?id=ID[&from=start][&audio=N][&sub=N]   play (resumes unless from=start; an episode queues the following
//!                                  ones; audio/sub = stream index, sub=-1 off; the rest of the queue keeps the languages)
//!   GET /control?cmd=pause|unpause|playpause|stop|next|prev|back10|fwd30
//!   GET /key?k=up|down|left|right|select|back|home          inject a navigation key (tests)
//!   GET /events?since=N            long poll (25 s): {seq, keys:[..], playing, paused, title, search}
//!   GET /text?q=TEXT               open Search with TEXT typed (tests; other machines use the LAN port below)
//!
//! LAN API (optional, off unless config `lan_allow` lists addresses): port `lan_port` (default 8098), only from those
//! addresses, one request:
//!   GET|POST /text?q=TEXT          same as /text above: e.g. a Home Assistant text box (phone keyboard) via rest_command
use crate::{Api, Cfg, SubPref, is_text_sub, log, pick_audio, pick_sub, streams};
use serde_json::{Value, json};
use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

pub const PORT: u16 = 8097;
pub const LAN_PORT: u16 = 8098;
pub const CACHE: &str = "/mnt/scratch/sljf/cache";

pub enum Cmd {
    Play { ids: Vec<String>, start: usize, ticks: i64, audio: Option<i64>, lang: Option<String>, sub: Option<i64>, sub_lang: Option<String> },
    Control(String),
    Search(String),
}

#[derive(Default)]
pub struct State {
    pub seq: u64,
    pub keys: VecDeque<(u64, String)>,
    pub playing: bool,
    pub paused: bool,
    pub title: String,
    /// text to search for (seq it came with, when): from the LAN API or Jellyfin SendString
    pub search: Option<(u64, Instant, String)>,
}

#[derive(Default)]
pub struct Shared {
    pub st: Mutex<State>,
    pub cv: Condvar,
    lang: Mutex<String>, // audio language for picking tracks (user setting or config audio_lang)
}

impl Shared {
    pub fn set_lang(&self, l: &str) {
        *self.lang.lock().unwrap() = l.to_string();
    }
    pub fn lang(&self) -> String {
        self.lang.lock().unwrap().clone()
    }
    /// A navigation key for the app (from Jellyfin GeneralCommands, e.g. a Jellyfin app or Home Assistant's remote, or /key).
    pub fn key(&self, k: &str) {
        let mut s = self.st.lock().unwrap();
        s.seq += 1;
        let seq = s.seq;
        s.keys.push_back((seq, k.to_string()));
        while s.keys.len() > 64 {
            s.keys.pop_front();
        }
        self.cv.notify_all();
    }
    pub fn search(&self, text: &str) {
        let mut s = self.st.lock().unwrap();
        s.seq += 1;
        s.search = Some((s.seq, Instant::now(), text.to_string()));
        self.cv.notify_all();
    }
    pub fn playing(&self, playing: bool, paused: bool, title: &str) {
        let mut s = self.st.lock().unwrap();
        if s.playing != playing || s.paused != paused || s.title != title {
            s.playing = playing;
            s.paused = paused;
            s.title = title.to_string();
            if playing {
                s.search = None; // the app restarted after playback must not search again
            }
            s.seq += 1;
            self.cv.notify_all();
        }
    }
}

fn query(q: &str, key: &str) -> Option<String> {
    q.split('&').find_map(|kv| kv.strip_prefix(key).and_then(|v| v.strip_prefix('=')).map(String::from))
}

/// Keep only what an id / number / small word can contain, so nothing from the app is pasted raw into a server URL.
fn safe(s: &str) -> String {
    s.chars().filter(|c| c.is_ascii_alphanumeric() || *c == '-').take(64).collect()
}

fn slim(v: &Value) -> Value {
    let ud = &v["UserData"];
    let kind = v["Type"].as_str().unwrap_or("");
    json!({
        "id": v["Id"], "name": v["Name"], "type": kind, "year": v["ProductionYear"],
        "series": v["SeriesName"], "season": v["ParentIndexNumber"], "episode": v["IndexNumber"],
        "minutes": v["RunTimeTicks"].as_i64().map(|t| t / 600_000_000),
        "folder": v["IsFolder"].as_bool().unwrap_or(false) || matches!(kind, "CollectionFolder" | "Series" | "Season" | "BoxSet" | "Folder"),
        "played": ud["Played"].as_bool().unwrap_or(false),
        "pct": ud["PlayedPercentage"].as_f64().unwrap_or(0.0).round(),
        "resume": ud["PlaybackPositionTicks"].as_i64().unwrap_or(0) > 0,
        "unplayed": ud["UnplayedItemCount"],
        "number": v["ChannelNumber"].as_str().or(v["Number"].as_str()),
        "program": v["CurrentProgram"]["Name"],
        "overview": v["Overview"].as_str().map(|s| s.chars().take(400).collect::<String>()),
        // episodes carry a 16:9 still; in rows of mixed things the series poster reads better
        "poster": if kind == "Episode" { v["SeriesId"].clone() } else { v["Id"].clone() },
    })
}

fn list(api: &Api, path: &str) -> Value {
    match api.get(path) {
        Ok(v) => {
            let items = if v.is_array() { v } else { v["Items"].clone() };
            Value::Array(items.as_array().map(|a| a.iter().map(slim).collect()).unwrap_or_default())
        }
        Err(e) => {
            log(&format!("local: {e}"));
            json!([])
        }
    }
}

const FIELDS: &str = "Fields=Overview&EnableImageTypes=Primary&ImageTypeLimit=1";

fn route(cfg: &Cfg, api: &Api, tx: &Sender<Cmd>, sh: &Shared, path: &str, q: &str) -> (u16, &'static str, Vec<u8>) {
    let uid = &cfg.user_id;
    let ok = |v: Value| (200, "application/json", v.to_string().into_bytes());
    match path {
        // no libraries = no server: say so (the app shows it and tries again) rather than an empty home screen
        "/home" if api.get(&format!("/Users/{uid}/Views")).is_err() => (502, "text/plain", b"jellyfin not answering".to_vec()),
        "/home" => ok(json!({
            "resume": list(api, &format!("/Users/{uid}/Items/Resume?Limit=12&MediaTypes=Video&{FIELDS}")),
            "nextup": list(api, &format!("/Shows/NextUp?UserId={uid}&Limit=12&{FIELDS}")),
            "channels": list(api, &format!("/LiveTv/Channels?UserId={uid}&AddCurrentProgram=true&EnableImageTypes=Primary&ImageTypeLimit=1")),
            "libraries": list(api, &format!("/Users/{uid}/Views")),
        })),
        "/items" => {
            let parent = safe(&query(q, "parent").unwrap_or_default());
            let start = safe(&query(q, "start").unwrap_or_default());
            ok(list(api, &format!(
                "/Users/{uid}/Items?ParentId={parent}&SortBy=ParentIndexNumber,IndexNumber,SortName&StartIndex={}&Limit=60&{FIELDS}",
                if start.is_empty() { "0" } else { &start }
            )))
        }
        "/search" => {
            // the app sends it percent-encoded; pass only characters that are safe inside a query value
            let term: String = query(q, "q").unwrap_or_default().chars()
                .filter(|c| c.is_ascii_alphanumeric() || "%+-._~".contains(*c)).take(120).collect();
            // shows and movies first: a title search otherwise fills up with episodes that share a word
            let find = |types: &str, n: u32| list(api, &format!(
                "/Users/{uid}/Items?SearchTerm={term}&Recursive=true&IncludeItemTypes={types}&Limit={n}&{FIELDS}"
            ));
            let mut all = find("Series,Movie", 20).as_array().cloned().unwrap_or_default();
            // an episode match shows as its series (OK opens seasons -> episodes), not as a loose episode
            let mut more: Vec<String> = Vec::new();
            for e in find("Episode", 30).as_array().cloned().unwrap_or_default() {
                let sid = e["poster"].as_str().map(safe).unwrap_or_default();
                if !sid.is_empty() && !more.contains(&sid) && !all.iter().any(|a| a["id"].as_str() == Some(sid.as_str())) {
                    more.push(sid);
                }
            }
            if !more.is_empty() {
                all.extend(list(api, &format!("/Users/{uid}/Items?Ids={}&{FIELDS}", more.join(",")))
                    .as_array().cloned().unwrap_or_default());
            }
            ok(Value::Array(all))
        }
        "/image" => {
            let id = safe(&query(q, "id").unwrap_or_default());
            let w = safe(&query(q, "w").unwrap_or_default()).parse::<u32>().unwrap_or(300).clamp(80, 1280);
            let kind = match query(q, "type").as_deref() { Some("Backdrop") => "Backdrop", Some("Thumb") => "Thumb", _ => "Primary" };
            let file = format!("{CACHE}/{id}-{kind}-{w}.jpg");
            if let Ok(b) = std::fs::read(&file) {
                return (200, "image/jpeg", b);
            }
            match api.get_bytes(&format!("/Items/{id}/Images/{kind}?maxWidth={w}&quality=80&format=Jpg")) {
                Ok(b) => {
                    let _ = std::fs::create_dir_all(CACHE);
                    let _ = std::fs::write(&file, &b);
                    (200, "image/jpeg", b)
                }
                Err(_) => (404, "text/plain", b"no image".to_vec()),
            }
        }
        "/detail" => {
            let id = safe(&query(q, "id").unwrap_or_default());
            let item = match api.get(&format!("/Users/{uid}/Items/{id}")) {
                Ok(v) => v,
                Err(e) => return (502, "text/plain", e.into_bytes()),
            };
            let track = |s: &Value| {
                // "English - Dolby Digital+ - 5.1 - Default - Original": the flags say nothing on this screen
                let name = s["DisplayTitle"].as_str().unwrap_or("?").split(" - ")
                    .filter(|p| !matches!(*p, "Default" | "Original" | "External")).collect::<Vec<_>>().join(" - ");
                json!({"index": s["Index"], "name": name, "lang": s["Language"]})
            };
            let audio: Vec<Value> = streams(&item, "Audio").iter().map(track).collect();
            let subs: Vec<Value> = streams(&item, "Subtitle").iter().map(|s| {
                let mut t = track(s);
                t["text"] = json!(is_text_sub(s));
                t
            }).collect();
            ok(json!({"audio": audio, "audio_default": pick_audio(&item, None, &sh.lang()),
                      "subs": subs, "sub_default": pick_sub(&item, None, &SubPref::Default).unwrap_or(-1)}))
        }
        "/play" => {
            let id = safe(&query(q, "id").unwrap_or_default());
            let item = match api.get(&format!("/Users/{uid}/Items/{id}")) {
                Ok(v) => v,
                Err(e) => return (502, "text/plain", e.into_bytes()),
            };
            let ticks = if query(q, "from").as_deref() == Some("start") { 0 } else { item["UserData"]["PlaybackPositionTicks"].as_i64().unwrap_or(0) };
            let mut ids = vec![id.clone()];
            let mut start = 0;
            if item["Type"] == "TvChannel" {
                // the queue is the channel list, so next / previous on the remote change channel
                if let Ok(v) = api.get(&format!("/LiveTv/Channels?UserId={uid}")) {
                    let all: Vec<String> = v["Items"].as_array().map(|a| a.iter().filter_map(|x| x["Id"].as_str().map(String::from)).collect()).unwrap_or_default();
                    if let Some(i) = all.iter().position(|x| *x == id) {
                        ids = all;
                        start = i;
                    }
                }
            }
            if item["Type"] == "Episode" {
                if let Some(series) = item["SeriesId"].as_str() {
                    if let Ok(v) = api.get(&format!("/Shows/{series}/Episodes?UserId={uid}&StartItemId={id}&Limit=30")) {
                        let rest: Vec<String> = v["Items"].as_array().map(|a| a.iter().filter_map(|x| x["Id"].as_str().map(String::from)).collect()).unwrap_or_default();
                        if rest.first() == Some(&id) {
                            ids = rest;
                        }
                    }
                }
            }
            let audio = query(q, "audio").and_then(|s| s.parse::<i64>().ok());
            let lang = audio.and_then(|a| streams(&item, "Audio").into_iter().find(|s| s["Index"].as_i64() == Some(a)))
                .and_then(|s| s["Language"].as_str().map(String::from)).filter(|l| l != "und");
            let sub = query(q, "sub").and_then(|s| s.parse::<i64>().ok());
            let sub_lang = sub.and_then(|i| streams(&item, "Subtitle").into_iter().find(|s| s["Index"].as_i64() == Some(i)))
                .and_then(|s| s["Language"].as_str().map(String::from));
            let _ = tx.send(Cmd::Play { ids, start, ticks, audio, lang, sub, sub_lang });
            ok(json!({"ok": true}))
        }
        "/control" => {
            let _ = tx.send(Cmd::Control(safe(&query(q, "cmd").unwrap_or_default())));
            ok(json!({"ok": true}))
        }
        "/text" => {
            let _ = tx.send(Cmd::Search(search_text(&query(q, "q").unwrap_or_default())));
            ok(json!({"ok": true}))
        }
        "/key" => {
            sh.key(&safe(&query(q, "k").unwrap_or_default()));
            ok(json!({"ok": true}))
        }
        "/events" => {
            let since = query(q, "since").and_then(|s| s.parse::<u64>().ok()).unwrap_or(0);
            let until = Instant::now() + Duration::from_secs(25);
            let mut s = sh.st.lock().unwrap();
            while s.seq <= since && Instant::now() < until {
                s = sh.cv.wait_timeout(s, Duration::from_secs(1)).unwrap().0;
            }
            // a client that is ahead of us (the daemon restarted) starts over
            let since = if since > s.seq { 0 } else { since };
            let keys: Vec<&str> = s.keys.iter().filter(|(n, _)| *n > since).map(|(_, k)| k.as_str()).collect();
            // a fresh app (since 0) takes a search only from the last 30 s: it was started for it
            let search = s.search.as_ref().filter(|(n, t, _)| *n > since && (since > 0 || t.elapsed() < Duration::from_secs(30)))
                .map(|(_, _, txt)| txt.clone());
            ok(json!({"seq": s.seq, "keys": keys, "playing": s.playing, "paused": s.paused, "title": s.title, "search": search}))
        }
        _ => (404, "text/plain", b"not found".to_vec()),
    }
}

/// Percent-decoded query value as search text: printable characters only, at most 60.
pub fn search_text(v: &str) -> String {
    let b = v.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'+' => out.push(b' '),
            b'%' if i + 2 < b.len() => {
                match u8::from_str_radix(std::str::from_utf8(&b[i + 1..i + 3]).unwrap_or("zz"), 16) {
                    Ok(c) => { out.push(c); i += 2; }
                    Err(_) => out.push(b'%'),
                }
            }
            c => out.push(c),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).chars().filter(|c| !c.is_control()).take(60).collect::<String>().trim().to_string()
}

/// The LAN port: only /text, only from `lan_allow`.
fn lan_handle(cfg: &Cfg, tx: &Sender<Cmd>, mut conn: TcpStream) {
    let peer = conn.peer_addr().map(|a| a.ip().to_string()).unwrap_or_default();
    let _ = conn.set_read_timeout(Some(Duration::from_secs(5)));
    let mut line = String::new();
    {
        let mut r = BufReader::new(&conn);
        if r.read_line(&mut line).is_err() {
            return;
        }
        let mut h = String::new();
        while r.read_line(&mut h).map(|n| n > 2).unwrap_or(false) {
            h.clear();
        }
    }
    let target = line.split_whitespace().nth(1).unwrap_or("/");
    let (path, q) = target.split_once('?').unwrap_or((target, ""));
    let (code, body) = if !cfg.lan_allow.iter().any(|a| *a == peer) {
        log(&format!("lan: refused {peer}"));
        (403, "not allowed")
    } else if path == "/text" {
        let _ = tx.send(Cmd::Search(search_text(&query(q, "q").unwrap_or_default())));
        (200, "{\"ok\":true}")
    } else {
        (404, "not found")
    };
    let _ = conn.write_all(format!("HTTP/1.1 {code} {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        if code == 200 { "OK" } else { "Error" }, body.len()).as_bytes());
}

pub fn lan_start(cfg: &'static Cfg, tx: Sender<Cmd>) {
    std::thread::spawn(move || {
        let listener = loop {
            match TcpListener::bind(("0.0.0.0", cfg.lan_port)) {
                Ok(l) => break l,
                Err(e) => {
                    log(&format!("lan: cannot listen on {}: {e}", cfg.lan_port));
                    std::thread::sleep(Duration::from_secs(10));
                }
            }
        };
        log(&format!("lan: /text on :{} for {}", cfg.lan_port, cfg.lan_allow.join(" ")));
        for conn in listener.incoming().flatten() {
            let tx = tx.clone();
            std::thread::spawn(move || lan_handle(cfg, &tx, conn));
        }
    });
}

fn handle(cfg: &Cfg, api: &Api, tx: &Sender<Cmd>, sh: &Shared, mut conn: TcpStream) {
    let _ = conn.set_read_timeout(Some(Duration::from_secs(5)));
    let mut line = String::new();
    {
        let mut r = BufReader::new(&conn);
        if r.read_line(&mut line).is_err() {
            return;
        }
        let mut h = String::new(); // drain the headers
        while r.read_line(&mut h).map(|n| n > 2).unwrap_or(false) {
            h.clear();
        }
    }
    let target = line.split_whitespace().nth(1).unwrap_or("/");
    let (path, q) = target.split_once('?').unwrap_or((target, ""));
    let (code, ctype, body) = route(cfg, api, tx, sh, path, q);
    let head = format!(
        "HTTP/1.1 {code} {}\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\nCache-Control: {}\r\nConnection: close\r\n\r\n",
        if code == 200 { "OK" } else { "Error" },
        body.len(),
        if ctype == "image/jpeg" { "max-age=86400" } else { "no-store" }
    );
    let _ = conn.write_all(head.as_bytes());
    let _ = conn.write_all(&body);
}

pub fn start(cfg: &'static Cfg, tx: Sender<Cmd>, sh: Arc<Shared>) {
    let api = Api::new(cfg);
    std::thread::spawn(move || {
        let listener = loop {
            match TcpListener::bind(("127.0.0.1", PORT)) {
                Ok(l) => break l,
                Err(e) => {
                    log(&format!("local: cannot listen on {PORT}: {e}"));
                    std::thread::sleep(Duration::from_secs(10));
                }
            }
        };
        log(&format!("local interface on 127.0.0.1:{PORT}"));
        for conn in listener.incoming().flatten() {
            let (api, tx, sh) = (api.clone(), tx.clone(), sh.clone());
            std::thread::spawn(move || handle(cfg, &api, &tx, &sh, conn));
        }
    });
}
