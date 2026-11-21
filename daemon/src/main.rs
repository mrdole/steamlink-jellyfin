//! sljfd: Jellyfin remote-controlled player daemon for the Valve Steam Link (docs/ARCHITECTURE.md).
//!
//! Runs on the Link. Logs in to Jellyfin as its own device (config.json, written by sljf-login), keeps Jellyfin's
//! websocket open and obeys Play / Playstate messages (from the Jellyfin apps, or Home Assistant's Jellyfin
//! integration, which shows this device as a media_player). Playback itself is the C program sljf-play: this daemon
//! starts it with a stream URL (video copied when it is H.264 8-bit <=1080p, otherwise re-encoded by the server;
//! audio always stereo AAC), reads its status lines for the position, and reports Playing/Progress/Stopped.
//!
//!   pause/resume  sljf-play is stopped and later started again at the same position (the Link's video decoder
//!                 cannot be paused by starving it); while paused `sljf-play --card` shows the on-screen bar
//!   on-screen bar sljf-play draws it from "osd" lines on its stdin (title, progress, hints); see remote_key()
//!   seek          sljf-play is restarted at the new position (Jellyfin cuts the stream server-side)
//!   screen        taken from the Link's menu only while something is loaded (/tmp/launch_cmdline.txt + /tmp/sljf.hold)
//!   Live TV       with live_tv_static: `sljf-play --static` shows static at once while the channel opens on the server
//!                 in a thread (start_tune); its stream URL then goes to that player as a `url` line (finish_tune).
//!                 A flip / stop during the wait abandons the open (abandon_tune closes the stream when it returns)
//!
//! Usage: sljfd /mnt/scratch/sljf/config.json
//! config.json: {"server","user_id","token","device_id","device_name","av_offset_ms","player","ca","audio_lang"}  (mode 600)
//!   audio_lang: ISO 639-2 language used to pick the audio track when the Jellyfin user has no preference (default eng)
//!   lan_allow, lan_port: optional LAN API (docs/integrations/lan-api.md); off while lan_allow is empty
//!   open_app_at_boot: open the on-TV app once the Link's menu is up after a boot (default false)
//!   live_tv_static, static_volume: Live TV static (default true) and its hiss loudness, percent 0-100 (default 8)
use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{Receiver, TryRecvError, channel};
use std::time::{Duration, Instant};
use tungstenite::client::IntoClientRequest;
use tungstenite::stream::MaybeTlsStream;
use tungstenite::{Message, WebSocket};

mod guide;
mod local;

/// Release version: daemon/Cargo.toml = VERSION at the repo root (scripts/bump-version).
const VERSION: &str = env!("CARGO_PKG_VERSION");
const UA: &str = concat!("SteamLinkJellyfin/", env!("CARGO_PKG_VERSION"));
/// Episodes played on by themselves, with no key pressed, before the next one waits for OK ("Still watching?").
const STILL_WATCHING_EPS: u32 = 3;
/// The "Next episode in N s" countdown starts this long before the end of an episode.
const COUNTDOWN_S: i64 = 15;
/// Left/right presses add up; the seek happens this long after the last one (each seek restarts the stream).
const SEEK_WAIT: Duration = Duration::from_millis(1200);
/// Sleep timer steps (minutes), cycled with Down on the remote during playback.
const SLEEP_STEPS: [u64; 3] = [30, 60, 90];
/// Waits before restarting a player that died (network drop, server hiccup); then playback is given up (~5 min).
const RETRY_S: [u64; 10] = [2, 4, 8, 15, 30, 30, 30, 60, 60, 60];
/// A player that prints nothing (it prints a status line each second) for this long is hung: restart it.
const PLAYER_SILENT: Duration = Duration::from_secs(30);
/// A player whose position has not moved for this long (not paused) is stuck too, e.g. on the last frame.
const PLAYER_STUCK: Duration = Duration::from_secs(45);
/// Live TV flips (static on) while a channel is still opening: the next one is opened this long after the last
/// Left/Right press, so a quick run of presses opens only the channel it lands on (each open holds one of the server's
/// tuner slots until it is closed). A single flip opens at once.
const TUNE_SETTLE: Duration = Duration::from_millis(600);
/// take_screen_fast: after the app and the Link's menu are gone, this long before the static player opens the screen.
const SCREEN_GRACE: Duration = Duration::from_millis(100);
/// Text subtitle tracks for sljf-play, fetched in the background: /tmp/sljf-sub-<item>-<index>.srt (tmpfs)
fn sub_path(item: &str, index: i64) -> String {
    format!("/tmp/sljf-sub-{item}-{index}.srt")
}
const TICKS: f64 = 10_000_000.0; // Jellyfin ticks per second
/// VolumeUp / VolumeDown step (volume is 0-100, sljf-play turns it into a gain)
const VOLUME_STEP: u8 = 5;

/// `volume` from config.json: 0-100, anything above is 100, missing or not a number = 100 (untouched).
fn cfg_volume(v: &Value) -> u8 {
    v.as_i64().or_else(|| v.as_f64().map(|f| f.round() as i64)).map(|x| x.clamp(0, 100) as u8).unwrap_or(100)
}

/// Jellyfin's remote volume commands: the new (volume, muted), or None for anything else.
/// SetVolume's Arguments.Volume comes as a string ("40"); a number is taken too.
fn volume_command(name: &str, args: &Value, vol: u8, muted: bool) -> Option<(u8, bool)> {
    Some(match name {
        "SetVolume" => {
            let a = &args["Volume"];
            let x = a.as_str().and_then(|s| s.trim().parse::<f64>().ok()).or_else(|| a.as_f64())?;
            ((x.round() as i64).clamp(0, 100) as u8, muted)
        }
        "VolumeUp" => (vol.saturating_add(VOLUME_STEP).min(100), false),
        "VolumeDown" => (vol.saturating_sub(VOLUME_STEP), muted),
        "Mute" => (vol, true),
        "Unmute" => (vol, false),
        "ToggleMute" => (vol, !muted),
        _ => return None,
    })
}
const HOLD: &str = "/tmp/sljf.hold";
const APP: &str = "/home/apps/jellyfin/run.sh";

struct Cfg {
    server: String,
    user_id: String,
    token: String,
    device_id: String,
    device_name: String,
    av_offset_ms: i64,
    player: String,
    ca: String,
    audio_lang: String,
    /// who may use the optional LAN API (local::lan_start); empty = LAN API off
    lan_allow: Vec<String>,
    lan_port: u16,
    /// open the on-TV app once per boot, in place of the Link's menu (controller prompt)
    open_app_at_boot: bool,
    /// static on screen (with a faint hiss) while a Live TV channel tunes or drops: the channel opens behind a
    /// `sljf-play --static` (play_live_static); off = the old blocking open with a black screen
    live_tv_static: bool,
    /// loudness of that hiss, percent of full scale (0-100; 0 = silent): SLJF_STATIC for Live TV players
    static_volume: u8,
    /// start volume 0-100 for sljf-play (100 = untouched); a remote changes it until sljfd restarts
    volume: u8,
}

impl Cfg {
    fn load(path: &str) -> Result<Cfg, String> {
        let text = std::fs::read_to_string(path).map_err(|e| format!("{path}: {e}"))?;
        let v: Value = serde_json::from_str(&text).map_err(|e| format!("{path}: {e}"))?;
        Cfg::parse(&v, path)
    }
    fn parse(v: &Value, path: &str) -> Result<Cfg, String> {
        let s = |k: &str, d: &str| v[k].as_str().unwrap_or(d).to_string();
        let c = Cfg {
            server: s("server", "").trim_end_matches('/').to_string(),
            user_id: s("user_id", ""),
            token: s("token", ""),
            device_id: s("device_id", "steamlink"),
            device_name: s("device_name", "Steam Link"),
            av_offset_ms: v["av_offset_ms"].as_i64().unwrap_or(0),
            player: s("player", "/mnt/scratch/sljf/sljf-play"),
            ca: s("ca", "/mnt/scratch/sljf/ca.pem"),
            audio_lang: s("audio_lang", "eng"),
            lan_allow: v["lan_allow"].as_array().map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect())
                .unwrap_or_default(),
            lan_port: v["lan_port"].as_u64().and_then(|p| u16::try_from(p).ok()).unwrap_or(local::LAN_PORT),
            open_app_at_boot: v["open_app_at_boot"].as_bool().unwrap_or(false),
            live_tv_static: v["live_tv_static"].as_bool().unwrap_or(true),
            static_volume: v["static_volume"].as_u64().map(|n| n.min(100) as u8).unwrap_or(8),
            volume: cfg_volume(&v["volume"]),
        };
        if c.server.is_empty() || c.user_id.is_empty() || c.token.is_empty() {
            return Err(format!("{path}: server, user_id and token are required (run sljf-login)"));
        }
        Ok(c)
    }
    fn auth(&self) -> String {
        format!(
            "MediaBrowser Client=\"Steam Link\", Device=\"{}\", DeviceId=\"{}\", Version=\"{}\", Token=\"{}\"",
            self.device_name, self.device_id, VERSION, self.token
        )
    }
}

fn log(msg: &str) {
    let t = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    println!("{:02}:{:02}:{:02} {}", (t / 3600) % 24, (t / 60) % 60, t % 60, msg);
}

#[derive(Clone)]
struct Api<'a> {
    cfg: &'a Cfg,
    agent: ureq::Agent,
    slow: ureq::Agent, // calls that may take long (opening a Live TV channel)
}

impl<'a> Api<'a> {
    fn new(cfg: &'a Cfg) -> Api<'a> {
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .timeout_global(Some(Duration::from_secs(15)))
            .http_status_as_error(false)
            .user_agent(UA)
            .build()
            .into();
        let slow: ureq::Agent = ureq::Agent::config_builder()
            .timeout_global(Some(Duration::from_secs(90)))
            .http_status_as_error(false)
            .user_agent(UA)
            .build()
            .into();
        Api { cfg, agent, slow }
    }
    fn get(&self, path: &str) -> Result<Value, String> {
        let mut r = self
            .agent
            .get(format!("{}{}", self.cfg.server, path))
            .header("Authorization", self.cfg.auth())
            .call()
            .map_err(|e| format!("GET {path}: {e}"))?;
        let code = r.status().as_u16();
        let body = r.body_mut().read_to_string().unwrap_or_default();
        if code >= 300 {
            return Err(format!("GET {path}: http {code}"));
        }
        serde_json::from_str(&body).map_err(|e| format!("GET {path}: {e}"))
    }
    fn get_bytes(&self, path: &str) -> Result<Vec<u8>, String> {
        self.get_bytes_with(&self.agent, path)
    }
    fn get_bytes_with(&self, agent: &ureq::Agent, path: &str) -> Result<Vec<u8>, String> {
        let mut r = agent
            .get(format!("{}{}", self.cfg.server, path))
            .header("Authorization", self.cfg.auth())
            .call()
            .map_err(|e| format!("GET {path}: {e}"))?;
        if r.status().as_u16() >= 300 {
            return Err(format!("GET {path}: http {}", r.status().as_u16()));
        }
        r.body_mut().with_config().limit(4 * 1024 * 1024).read_to_vec().map_err(|e| format!("GET {path}: {e}"))
    }
    fn post(&self, path: &str, body: &Value) -> Result<(), String> {
        let r = self
            .agent
            .post(format!("{}{}", self.cfg.server, path))
            .header("Authorization", self.cfg.auth())
            .header("Content-Type", "application/json")
            .send(body.to_string())
            .map_err(|e| format!("POST {path}: {e}"))?;
        let code = r.status().as_u16();
        if code >= 300 { Err(format!("POST {path}: http {code}")) } else { Ok(()) }
    }
    fn post_json(&self, path: &str, body: &Value) -> Result<Value, String> {
        let mut r = self
            .slow
            .post(format!("{}{}", self.cfg.server, path))
            .header("Authorization", self.cfg.auth())
            .header("Content-Type", "application/json")
            .send(body.to_string())
            .map_err(|e| format!("POST {path}: {e}"))?;
        let code = r.status().as_u16();
        let text = r.body_mut().read_to_string().unwrap_or_default();
        if code >= 300 {
            return Err(format!("POST {path}: http {code}"));
        }
        serde_json::from_str(&text).map_err(|e| format!("POST {path}: {e}"))
    }
    fn delete(&self, path: &str) {
        let _ = self.agent.delete(format!("{}{}", self.cfg.server, path)).header("Authorization", self.cfg.auth()).call();
    }
}

/// The item's streams of one type ("Audio" / "Subtitle"), from its first media source.
pub fn streams(item: &Value, kind: &str) -> Vec<Value> {
    item["MediaSources"][0]["MediaStreams"].as_array().or(item["MediaStreams"].as_array())
        .map(|a| a.iter().filter(|s| s["Type"] == kind).cloned().collect()).unwrap_or_default()
}

/// Which audio track to play: the one asked for if the item has it; else Jellyfin's per-user default when it is in
/// `lang` (or has no language); else the first `lang` track that is not a commentary; else Jellyfin's default.
pub fn pick_audio(item: &Value, asked: Option<i64>, lang: &str) -> Option<i64> {
    let a = streams(item, "Audio");
    if a.is_empty() {
        return None;
    }
    let idx = |s: &Value| s["Index"].as_i64();
    if let Some(i) = asked {
        if a.iter().any(|s| idx(s) == Some(i)) {
            return Some(i);
        }
    }
    let default = item["MediaSources"][0]["DefaultAudioStreamIndex"].as_i64()
        .or_else(|| a.iter().find(|s| s["IsDefault"] == true).and_then(idx)).or_else(|| idx(&a[0]));
    let lang_of = |i: Option<i64>| a.iter().find(|s| idx(s) == i).and_then(|s| s["Language"].as_str()).unwrap_or("und").to_string();
    let dl = lang_of(default);
    if dl == lang || dl == "und" {
        return default;
    }
    let commentary = |s: &Value| s["Title"].as_str().unwrap_or("").to_lowercase().contains("comment");
    a.iter().find(|s| s["Language"].as_str() == Some(lang) && !commentary(s)).and_then(idx).or(default)
}

/// What the Link's decoder takes as-is: H.264, 8-bit, up to 1080p (anything else the server re-encodes).
fn video_decodable(item: &Value) -> bool {
    let a = streams(item, "Video");
    let Some(v) = a.first() else { return false };
    v["Codec"] == "h264" && !v["Profile"].as_str().unwrap_or("").contains("10") && v["Height"].as_i64().unwrap_or(0) <= 1088
}

/// Text subtitles can be drawn on the Link (as SRT); image ones (PGS, VobSub) only burned in by the server.
pub fn is_text_sub(s: &Value) -> bool {
    s["IsTextSubtitleStream"].as_bool().unwrap_or_else(|| {
        matches!(s["Codec"].as_str().unwrap_or("").to_lowercase().as_str(), "subrip" | "srt" | "ass" | "ssa" | "mov_text" | "webvtt" | "vtt" | "text")
    })
}

/// What subtitles to show for the next item of a queue.
#[derive(Clone, Debug, PartialEq)]
pub enum SubPref {
    Default,      // Jellyfin's per-user default (usually forced or default-flagged tracks only)
    Off,
    Lang(String), // a track picked by hand: later items get the same language (full subs, text before image)
}

/// The subtitle track to show (None = off): the one asked for, else by the queue's preference.
pub fn pick_sub(item: &Value, asked: Option<i64>, pref: &SubPref) -> Option<i64> {
    let subs = streams(item, "Subtitle");
    let idx = |s: &Value| s["Index"].as_i64();
    if let Some(i) = asked {
        return if i >= 0 && subs.iter().any(|s| idx(s) == Some(i)) { Some(i) } else { None };
    }
    match pref {
        SubPref::Off => None,
        SubPref::Default => item["MediaSources"][0]["DefaultSubtitleStreamIndex"].as_i64().filter(|i| *i >= 0),
        SubPref::Lang(l) => {
            let mut c: Vec<&Value> = subs.iter().filter(|s| s["Language"].as_str() == Some(l.as_str())).collect();
            c.sort_by_key(|s| (s["IsForced"] == true, !is_text_sub(s)));
            c.first().and_then(|s| idx(s))
        }
    }
}

/// A background subtitle download finished (subtitle_tracks).
pub struct SubFetch {
    item: String,
    index: i64,
    secs: f64,
    result: Result<usize, String>,
}

/// Tracks of what is playing: audio index, subtitle index, and how the subtitles get on screen.
#[derive(Clone, Default)]
struct Tracks {
    audio: Option<i64>,
    sub: Option<i64>,
    burn: bool,           // subtitles burned into the video by the server (video re-encoded)
    sub_path: Option<String>, // subtitles drawn by sljf-play from this SRT (it may still be downloading)
}

fn sh(cmd: &str) {
    let _ = Command::new("sh").arg("-c").arg(cmd).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).status();
}

/// Free the screen the way the Link's own launcher does: shell.sh runs /tmp/launch_cmdline.txt when the menu
/// (`shell`) exits, then starts the menu again. The parked loop ends when HOLD goes away.
fn take_screen() {
    if std::path::Path::new(HOLD).exists() {
        return;
    }
    sh(&format!(
        "touch {HOLD}; echo 'while [ -e {HOLD} ]; do sleep 1; done' >/tmp/launch_cmdline.txt; killall shell; sleep 2"
    ));
}
/// take_screen for Live TV with static: no fixed 2 s wait. The static goes up as soon as the on-TV app (it leaves when
/// it sees `playing`) and the Link's menu are gone, at most after the same 2 s.
fn take_screen_fast() {
    if std::path::Path::new(HOLD).exists() {
        return;
    }
    sh(&format!(
        "touch {HOLD}; echo 'while [ -e {HOLD} ]; do sleep 1; done' >/tmp/launch_cmdline.txt; killall shell"
    ));
    let up = |name: &str| Command::new("pidof").arg(name).stdout(Stdio::null()).stderr(Stdio::null()).status()
        .map(|s| s.success()).unwrap_or(false);
    let t = Instant::now();
    while t.elapsed() < Duration::from_secs(2) && (up("shell") || up("sljf-ui")) {
        std::thread::sleep(Duration::from_millis(50));
    }
    std::thread::sleep(SCREEN_GRACE);
}
/// The stdin line that hands a waiting `sljf-play --static` its stream URL; None if the URL would break the line.
fn url_line(url: &str) -> Option<String> {
    if url.contains(['\n', '\r']) { None } else { Some(format!("url {url}\n")) }
}
/// Show the on-TV app in place of the Link's menu (same launcher route as take_screen); no-op if it is open.
fn open_app() {
    sh(&format!("pidof sljf-ui >/dev/null || {{ [ -x {APP} ] && echo {APP} >/tmp/launch_cmdline.txt && killall shell; }}"));
}
/// open_app_at_boot: wait for the Link's menu (`shell`) to come up, then open the app once per boot.
/// /tmp is a tmpfs, so the marker is gone after a reboot; a daemon restart (deploy, crash) does not reopen it.
fn open_app_at_boot() {
    const MARK: &str = "/tmp/sljf.app-at-boot";
    if std::path::Path::new(MARK).exists() {
        return;
    }
    // only right after a boot: a daemon restarted later (deploy, crash) must not reopen the app after someone left it
    let uptime = std::fs::read_to_string("/proc/uptime").ok()
        .and_then(|t| t.split_whitespace().next().and_then(|x| x.parse::<f64>().ok())).unwrap_or(0.0);
    if uptime > 300.0 {
        return;
    }
    std::thread::spawn(|| {
        for _ in 0..180 {
            let up = Command::new("pidof").arg("shell").stdout(Stdio::null()).stderr(Stdio::null()).status()
                .map(|s| s.success()).unwrap_or(false);
            if up {
                std::thread::sleep(Duration::from_secs(3)); // let the menu finish starting
                if !std::path::Path::new(HOLD).exists() {
                    let _ = std::fs::write(MARK, "");
                    log("open_app_at_boot: opening the app");
                    open_app();
                }
                return;
            }
            std::thread::sleep(Duration::from_secs(1));
        }
        log("open_app_at_boot: the Link's menu did not come up in 3 min, app not opened");
    });
}
fn release_screen() {
    let _ = std::fs::remove_file(HOLD);
}
/// The Link's powermanager treats "no controller input" as idle and would sleep mid-playback.
fn keep_awake() {
    sh("dbus-send --system --dest=system.powermanager /powermanager system.powermanager.SetActivity uint32:1");
}

fn session_id() -> String {
    let mut b = [0u8; 16];
    if let Ok(mut f) = std::fs::File::open("/dev/urandom") {
        let _ = std::io::Read::read_exact(&mut f, &mut b);
    }
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// A Jellyfin live stream (Live TV channel): opened per play, closed at stop so the tuner slot is freed.
#[derive(Clone)]
struct Live {
    ms_id: String,
    live_id: String,
}

/// A Live TV channel being opened on the server in a thread (start_tune): (stream, video copyable) or the error.
type Tune = Receiver<Result<(Live, bool), String>>;

/// Open a Live TV channel on the server (a tuner slot: ErsatzTV starts the channel; can take ~15 s).
fn open_live(api: &Api, id: &str) -> Result<(Live, bool), String> {
    let body = json!({
        "UserId": api.cfg.user_id, "AutoOpenLiveStream": true, "EnableDirectPlay": false, "EnableDirectStream": true,
        "EnableTranscoding": true, "MaxStreamingBitrate": 20_000_000,
        "DeviceProfile": {"MaxStreamingBitrate": 20_000_000, "DirectPlayProfiles": [],
            "TranscodingProfiles": [{"Container": "ts", "Type": "Video", "VideoCodec": "h264", "AudioCodec": "aac",
                                     "Protocol": "http", "Context": "Streaming", "MaxAudioChannels": "2"}]},
    });
    let r = api.post_json(&format!("/Items/{id}/PlaybackInfo"), &body)?;
    let ms = &r["MediaSources"][0];
    let (Some(ms_id), Some(live_id)) = (ms["Id"].as_str(), ms["LiveStreamId"].as_str()) else {
        return Err(format!("no live stream ({})", r["ErrorCode"].as_str().unwrap_or("no media source")));
    };
    Ok((Live { ms_id: ms_id.into(), live_id: live_id.into() }, video_decodable(ms)))
}
fn close_live(api: &Api, l: &Live) {
    if let Err(e) = api.post(&format!("/LiveStreams/Close?liveStreamId={}", l.live_id), &json!({})) {
        log(&format!("close live stream: {e}"));
    }
}

/// End a sljf-play that has no `Now` (the static kept up across a flip): end of stdin, then TERM, then KILL.
fn end_player(mut c: Child) {
    drop(c.stdin.take());
    sh(&format!("kill -TERM {}", c.id()));
    let t = Instant::now();
    while t.elapsed() < Duration::from_secs(4) {
        if let Ok(Some(_)) = c.try_wait() {
            return;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let _ = c.kill();
    let _ = c.wait();
}

struct Now {
    item_id: String,
    chan: bool,           // a Live TV channel (`live` is None while it is being opened)
    live: Option<Live>,   // Live TV: no position to seek to, restart = open the channel again
    tune: Option<Tune>,   // static on: the channel is being opened on the server (child = the waiting static player)
    tune_at: Option<Instant>, // static on, after a flip: open the channel then (TUNE_SETTLE)
    waiting: bool,        // child is a `sljf-play --static` that has no stream URL yet (no watchdog)
    name: String,
    title: String,        // for the on-screen bar: "Series · S3E5 Episode", "Film (1984)", "11 Channel · Program"
    runtime: i64,
    copy: bool,           // video copied (H.264 the Link can decode, and no burned-in subtitles)
    tr: Tracks,
    session: String,
    start_ticks: i64, // position the current sljf-play was started at
    pos_ticks: i64,
    paused: bool,
    eof: bool,
    restarts: u32,
    retry_at: Option<Instant>, // a restart is waiting (backoff after a failure)
    last_line: Instant,        // last output from sljf-play (watchdog)
    pos_moved: Instant,        // last time the position changed (a player stuck on its last frame still prints lines)
    child: Option<Child>, // None while paused or waiting to retry
    lines: Option<Receiver<String>>,
    last_report: Instant,
}

struct Player {
    cfg: &'static Cfg,
    api: Api<'static>, // 'static: Live TV opens run in threads (start_tune, abandon_tune)
    standby: Option<(Child, Receiver<String>)>, // static player kept up across a Live TV flip, for play_live_static
    settle: bool, // that flip came while a channel was still opening (a run of presses): open after TUNE_SETTLE
    queue: Vec<String>,
    qi: usize,
    now: Option<Now>,
    last_awake: Instant,
    asked_audio: Option<i64>,       // audio track picked for the first item of the queue (play dialog / remote)
    lang: String,                   // audio language for the rest of the queue: the picked track's, else the user's
    asked_sub: Option<i64>,         // subtitle track picked for the first item (-1 = off)
    sub_pref: SubPref,              // subtitles for the rest of the queue
    cmds: Receiver<local::Cmd>,     // from the on-TV app (local.rs)
    sub_tx: std::sync::mpsc::Sender<SubFetch>, // background subtitle downloads report here
    sub_rx: Receiver<SubFetch>,
    shared: std::sync::Arc<local::Shared>,
    card: Option<Child>,            // `sljf-play --card` while paused (the bar on its own)
    seek_to: Option<(i64, Instant)>, // left/right presses waiting to become one seek (target, last press)
    sleep_at: Option<Instant>,      // sleep timer: stop playback then
    auto_eps: u32,                  // episodes started by themselves since the last key press
    no_next: bool,                  // Back during the countdown: stop after this item
    next_label: Option<(usize, String)>, // queue index + "S3E6 Title" of the next item, for the countdown
    volume: u8,                     // 0-100, from config.json, then the remote's SetVolume / VolumeUp / VolumeDown
    muted: bool,
}

/// "Series · S3E5 Episode", "Film (1984)" or "11 Channel · Program" for the on-screen bar
fn bar_title(item: &Value) -> String {
    let name = item["Name"].as_str().unwrap_or("?");
    let t = match item["Type"].as_str() {
        Some("Episode") => {
            let se = match (item["ParentIndexNumber"].as_i64(), item["IndexNumber"].as_i64()) {
                (Some(s), Some(e)) => format!("S{s}E{e} "),
                (None, Some(e)) => format!("E{e} "),
                _ => String::new(),
            };
            match item["SeriesName"].as_str() {
                Some(series) => format!("{series} · {se}{name}"),
                None => format!("{se}{name}"),
            }
        }
        Some("TvChannel") => {
            let num = item["ChannelNumber"].as_str().or(item["Number"].as_str()).unwrap_or("");
            match item["CurrentProgram"]["Name"].as_str() {
                Some(p) => format!("{num} {name} · {p}"),
                None => format!("{num} {name}"),
            }
        }
        _ => match item["ProductionYear"].as_i64() {
            Some(y) => format!("{name} ({y})"),
            None => name.to_string(),
        },
    };
    t.trim().replace(['\t', '\n', '\r'], " ")
}

fn short_episode(item: &Value) -> String {
    let name = item["Name"].as_str().unwrap_or("?");
    match (item["ParentIndexNumber"].as_i64(), item["IndexNumber"].as_i64()) {
        (Some(s), Some(e)) => format!("S{s}E{e} {name}"),
        _ => name.to_string(),
    }
    .replace(['\t', '\n', '\r'], " ")
}

impl Player {
    fn stream_url(&self, item: &str, copy: bool, tr: &Tracks, session: &str, ticks: i64, live: Option<&Live>) -> String {
        if let Some(l) = live {
            // ErsatzTV channels are H.264 + AAC stereo in MPEG-TS already: copy both when the video fits the decoder
            let mut u = format!(
                "{}/Videos/{item}/stream.ts?MediaSourceId={}&LiveStreamId={}&DeviceId={}&PlaySessionId={session}&Container=ts\
                 &VideoCodec=h264&AudioCodec=aac&AudioChannels=2&MaxAudioChannels=2&AudioSampleRate=48000&AudioBitrate=192000\
                 &AllowVideoStreamCopy={copy}&AllowAudioStreamCopy=true",
                self.cfg.server, l.ms_id, l.live_id, self.cfg.device_id
            );
            if !copy {
                u.push_str("&MaxWidth=1920&MaxHeight=1080&VideoBitrate=8000000&Profile=high&Level=41&MaxFramerate=30");
            }
            return u;
        }
        let mut u = format!(
            "{}/Videos/{item}/stream.ts?MediaSourceId={item}&DeviceId={}&PlaySessionId={session}&Container=ts\
             &VideoCodec=h264&AudioCodec=aac&AudioChannels=2&MaxAudioChannels=2&AudioSampleRate=48000\
             &AudioBitrate=192000&AllowVideoStreamCopy={copy}&AllowAudioStreamCopy=false&StartTimeTicks={ticks}",
            self.cfg.server, self.cfg.device_id
        );
        if let Some(a) = tr.audio {
            u.push_str(&format!("&AudioStreamIndex={a}"));
        }
        if let (true, Some(s)) = (tr.burn, tr.sub) {
            u.push_str(&format!("&SubtitleStreamIndex={s}&SubtitleMethod=Encode"));
        }
        if !copy {
            u.push_str("&MaxWidth=1920&MaxHeight=1080&VideoBitrate=8000000&Profile=high&Level=41&MaxFramerate=30");
        }
        u
    }

    /// sljf-play with the environment every run gets (stream URL or `--static` still to be added).
    fn player_cmd(&self, tr: &Tracks, ticks: i64, live: bool) -> Command {
        let mut c = Command::new(&self.cfg.player);
        c.env("SLJF_START", format!("{:.3}", ticks as f64 / TICKS))
            // live TV comes at exactly real time: a 3 s cushion against network hiccups (files arrive faster)
            .env("SLJF_PREBUFFER_MS", if live { "3000" } else { "0" })
            .env("SLJF_SUBS", tr.sub_path.as_deref().unwrap_or(""))
            .env("SLJF_HEADERS", format!("Authorization: {}\r\n", self.cfg.auth()))
            .env("SLJF_CA", &self.cfg.ca)
            .env("SLJF_UA", UA)
            .env("SLJF_AV_OFFSET_MS", self.cfg.av_offset_ms.to_string())
            .env("SLJF_VOLUME", self.heard_volume().to_string())
            .stdin(Stdio::piped()) // "osd" lines: the on-screen bar, "volume N"; `url` for a waiting --static
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        if live && self.cfg.live_tv_static {
            c.env("SLJF_STATIC", self.cfg.static_volume.to_string()); // static while tuning and on rebuffer
        }
        c
    }

    fn start_player(&self, mut cmd: Command) -> Result<(Child, Receiver<String>), String> {
        let mut child = cmd.spawn().map_err(|e| format!("{}: {e}", self.cfg.player))?;
        let out = child.stdout.take().ok_or("no stdout")?;
        let (tx, rx) = channel();
        std::thread::spawn(move || {
            for line in BufReader::new(out).lines().map_while(Result::ok) {
                if tx.send(line).is_err() {
                    break;
                }
            }
        });
        Ok((child, rx))
    }

    fn spawn(&self, item: &str, copy: bool, tr: &Tracks, ticks: i64, live: Option<&Live>) -> Result<(Child, Receiver<String>, String), String> {
        let session = session_id();
        let url = self.stream_url(item, copy, tr, &session, ticks, live);
        let mut cmd = self.player_cmd(tr, ticks, live.is_some());
        cmd.arg(&url);
        let (child, rx) = self.start_player(cmd)?;
        Ok((child, rx, session))
    }

    /// Live TV with static: `sljf-play --static`, static on screen until a `url` line arrives on its stdin.
    fn spawn_waiting(&self) -> Result<(Child, Receiver<String>), String> {
        let mut cmd = self.player_cmd(&Tracks::default(), 0, true);
        cmd.arg("--static");
        self.start_player(cmd)
    }

    fn kill_standby(&mut self) {
        if let Some((c, _)) = self.standby.take() {
            end_player(c);
        }
    }

    fn body(&self, n: &Now, event: Option<&str>) -> Value {
        let mut b = json!({
            "ItemId": n.item_id, "PlaySessionId": n.session,
            "MediaSourceId": n.live.as_ref().map(|l| l.ms_id.clone()).unwrap_or_else(|| n.item_id.clone()),
            "LiveStreamId": n.live.as_ref().map(|l| l.live_id.clone()),
            "PositionTicks": n.pos_ticks, "IsPaused": n.paused, "IsMuted": self.muted, "VolumeLevel": self.volume, "CanSeek": !n.chan,
            "PlayMethod": if n.copy { "DirectStream" } else { "Transcode" }, "RepeatMode": "RepeatNone",
            "NowPlayingQueue": self.queue.iter().enumerate()
                .map(|(i, id)| json!({"Id": id, "PlaylistItemId": format!("q{i}")})).collect::<Vec<_>>(),
            "PlaylistItemId": format!("q{}", self.qi),
        });
        if let Some(a) = n.tr.audio {
            b["AudioStreamIndex"] = json!(a);
        }
        b["SubtitleStreamIndex"] = json!(n.tr.sub.unwrap_or(-1));
        if let Some(e) = event {
            b["EventName"] = json!(e);
        }
        b
    }

    fn report(&mut self, what: &str, event: Option<&str>) {
        if let Some(n) = &self.now {
            if n.session.is_empty() {
                return; // a Live TV channel still opening for the first time: nothing started on the server yet
            }
            let path = match what {
                "start" => "/Sessions/Playing",
                "stop" => "/Sessions/Playing/Stopped",
                _ => "/Sessions/Playing/Progress",
            };
            let body = self.body(n, event);
            if let Err(e) = self.api.post(path, &body) {
                log(&format!("report {what}: {e}"));
            }
        }
        if let Some(n) = &mut self.now {
            n.last_report = Instant::now();
        }
    }

    fn kill_child(&mut self) {
        self.kill_card();
        if let Some(n) = &mut self.now {
            let Some(mut child) = n.child.take() else { return };
            n.lines = None;
            sh(&format!("kill -TERM {}", child.id()));
            let t = Instant::now();
            while t.elapsed() < Duration::from_secs(4) {
                if let Ok(Some(_)) = child.try_wait() {
                    break;
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            let _ = child.kill();
            let _ = child.wait();
        }
        self.end_encode();
    }

    /// Tell the server to end the encode of the current play session (after its player is gone).
    fn end_encode(&self) {
        if let Some(n) = self.now.as_ref() {
            self.end_encode_of(&n.session);
        }
    }
    fn end_encode_of(&self, session: &str) {
        if !session.is_empty() {
            self.api.delete(&format!("/Videos/ActiveEncodings?DeviceId={}&PlaySessionId={session}", self.cfg.device_id));
        }
    }

    /// Load queue[qi] at `ticks` and start playing.
    fn play_current(&mut self, ticks: i64) {
        self.no_next = false;
        self.seek_to = None;
        let Some(id) = self.queue.get(self.qi).cloned() else { return self.stop(true) };
        let item = match self.api.get(&format!("/Users/{}/Items/{id}", self.cfg.user_id)) {
            Ok(v) => v,
            Err(e) => {
                log(&format!("play {id}: {e}"));
                return self.stop(true);
            }
        };
        if item["Type"] == "TvChannel" {
            return self.play_live(&id, &item);
        }
        self.kill_standby(); // a static kept up by a flip, and the queue's next item is not a channel
        let decodable = video_decodable(&item);
        let audio = pick_audio(&item, self.asked_audio.take(), &self.lang);
        let sub = pick_sub(&item, self.asked_sub.take(), &self.sub_pref);
        let tr = self.subtitle_tracks(&id, &item, audio, sub);
        let copy = decodable && !tr.burn;
        let name = match item["SeriesName"].as_str() {
            Some(s) => format!("{} ({s})", item["Name"].as_str().unwrap_or("?")),
            None => item["Name"].as_str().unwrap_or("?").to_string(),
        };
        let audio_name = streams(&item, "Audio").iter().find(|s| s["Index"].as_i64() == audio)
            .map(|s| s["DisplayTitle"].as_str().unwrap_or("?").to_string()).unwrap_or_else(|| "none".into());
        // tell the on-TV app first, so it has left the screen (take_screen waits 2 s) before the player opens it
        self.shared.playing(true, false, &name);
        take_screen();
        match self.spawn(&id, copy, &tr, ticks, None) {
            Ok((child, lines, session)) => {
                log(&format!("play: {name} [{id}] at {:.0} s, video {}, audio #{} {audio_name}, subtitles {}", ticks as f64 / TICKS,
                    if copy { "copied" } else { "re-encoded by the server" }, audio.map(|a| a.to_string()).unwrap_or("-".into()),
                    match (tr.sub, tr.burn) { (None, _) => "off".to_string(), (Some(s), true) => format!("#{s} burned in"), (Some(s), false) => format!("#{s} drawn") }));
                self.now = Some(Now {
                    item_id: id, chan: false, live: None, tune: None, tune_at: None, waiting: false, name, title: bar_title(&item), runtime: item["RunTimeTicks"].as_i64().unwrap_or(0), copy, tr, session,
                    start_ticks: ticks, pos_ticks: ticks, paused: false, eof: false, restarts: 0,
                    retry_at: None, last_line: Instant::now(), pos_moved: Instant::now(), child: Some(child), lines: Some(lines), last_report: Instant::now(),
                });
                self.report("start", None);
                self.osd(5, None);
            }
            Err(e) => {
                log(&format!("play: {e}"));
                self.stop(true);
            }
        }
    }

    fn open_live(&self, id: &str) -> Result<(Live, bool), String> {
        open_live(&self.api, id)
    }
    fn close_live(&self, l: &Live) {
        close_live(&self.api, l)
    }

    /// Play a Live TV channel. Static on: play_live_static; off: open it first (black screen meanwhile), then play.
    fn play_live(&mut self, id: &str, item: &Value) {
        let name = format!("{} {}", item["ChannelNumber"].as_str().or(item["Number"].as_str()).unwrap_or(""), item["Name"].as_str().unwrap_or("?"))
            .trim().to_string();
        if self.cfg.live_tv_static {
            return self.play_live_static(id, item, name);
        }
        // opening can take ~15 s (ErsatzTV starts the channel): the app keeps showing STARTING meanwhile
        let (live, copy) = match self.open_live(id) {
            Ok(v) => v,
            Err(e) => {
                log(&format!("live {name}: {e}"));
                return self.stop(true);
            }
        };
        self.shared.channel(id);
        self.shared.playing(true, false, &name);
        take_screen();
        let tr = Tracks::default();
        match self.spawn(id, copy, &tr, 0, Some(&live)) {
            Ok((child, lines, session)) => {
                log(&format!("play: live {name} [{id}], video {}", if copy { "copied" } else { "re-encoded by the server" }));
                self.now = Some(Now {
                    item_id: id.into(), chan: true, live: Some(live), tune: None, tune_at: None, waiting: false,
                    name, title: bar_title(item), runtime: 0, copy, tr, session, start_ticks: 0, pos_ticks: 0,
                    paused: false, eof: false, restarts: 0, retry_at: None, last_line: Instant::now(), pos_moved: Instant::now(),
                    child: Some(child), lines: Some(lines), last_report: Instant::now(),
                });
                self.report("start", None);
                self.osd(5, None);
            }
            Err(e) => {
                log(&format!("play: {e}"));
                self.close_live(&live);
                self.stop(true);
            }
        }
    }

    /// Live TV with static: the app gives up the screen at once (it leaves when it sees `playing`), static goes up,
    /// and the channel opens in the background; commands keep working meanwhile (tick -> finish_tune).
    fn play_live_static(&mut self, id: &str, item: &Value, name: String) {
        self.shared.channel(id);
        self.shared.playing(true, false, &name);
        let settle = self.standby.is_some() && std::mem::take(&mut self.settle);
        let (child, lines) = match self.standby.take() {
            Some((c, l)) => (Some(c), Some(l)), // a flip: the static is up already
            None => {
                take_screen_fast();
                match self.spawn_waiting() {
                    Ok((c, l)) => (Some(c), Some(l)),
                    Err(e) => {
                        log(&format!("static: {e}")); // tune anyway; finish_tune starts a normal player
                        (None, None)
                    }
                }
            }
        };
        log(&format!("live {name} [{id}]: tuning behind the static"));
        let now = Instant::now();
        self.now = Some(Now {
            item_id: id.into(), chan: true, live: None, tune: None, tune_at: settle.then(|| now + TUNE_SETTLE), waiting: true,
            name, title: bar_title(item), runtime: 0, copy: false, tr: Tracks::default(), session: String::new(),
            start_ticks: 0, pos_ticks: 0, paused: false, eof: false, restarts: 0, retry_at: None, last_line: now, pos_moved: now,
            child, lines, last_report: now,
        });
        if !settle {
            self.start_tune(id);
        }
    }

    /// Open channel `id` on the server in a thread; tick() hands the result to finish_tune.
    fn start_tune(&mut self, id: &str) {
        if let Some(old) = self.now.as_mut().and_then(|n| n.tune.take()) {
            self.abandon_tune(old); // (not expected: callers start one open at a time)
        }
        let (tx, rx) = channel();
        let (api, id) = (self.api.clone(), id.to_string());
        std::thread::spawn(move || {
            let _ = tx.send(open_live(&api, &id));
        });
        if let Some(n) = self.now.as_mut() {
            n.tune = Some(rx);
            n.tune_at = None;
        }
    }

    /// The channel opened (or failed): hand its URL to the waiting static player, or retry / stop as without static.
    fn finish_tune(&mut self, r: Result<(Live, bool), String>) {
        let Some(n) = self.now.as_mut() else { return };
        n.tune = None;
        let (live, copy) = match r {
            Ok(v) => v,
            Err(e) => {
                let why = format!("live {}: {e}", n.name);
                if n.session.is_empty() {
                    log(&why); // never played: stop, as a failed open without static does
                    return self.stop(true);
                }
                let k = n.restarts + 1; // reopening after a drop: retry with the static kept up
                return self.retry_later(k, &why);
            }
        };
        let first = n.session.is_empty();
        let (id, tr, name) = (n.item_id.clone(), n.tr.clone(), n.name.clone());
        let session = session_id();
        let url = self.stream_url(&id, copy, &tr, &session, 0, Some(&live));
        let n = self.now.as_mut().unwrap();
        let sent = n.waiting && url_line(&url).zip(n.child.as_mut().and_then(|c| c.stdin.as_mut()))
            .map(|(line, w)| w.write_all(line.as_bytes()).and_then(|_| w.flush()).is_ok()).unwrap_or(false);
        if sent {
            n.session = session;
        } else {
            // no waiting player (it died, or could not start): a normal one, which shows static itself while it opens
            self.kill_child();
            match self.spawn(&id, copy, &tr, 0, Some(&live)) {
                Ok((child, lines, session)) => {
                    let n = self.now.as_mut().unwrap();
                    n.child = Some(child);
                    n.lines = Some(lines);
                    n.session = session;
                }
                Err(e) => {
                    self.close_live(&live);
                    let why = format!("play: {e}");
                    if first {
                        log(&why);
                        return self.stop(true);
                    }
                    let k = self.now.as_ref().map(|n| n.restarts).unwrap_or(0) + 1;
                    return self.retry_later(k, &why);
                }
            }
        }
        let n = self.now.as_mut().unwrap();
        n.live = Some(live);
        n.copy = copy;
        n.waiting = false;
        n.eof = false;
        n.start_ticks = 0;
        n.pos_ticks = 0;
        n.retry_at = None;
        n.last_line = Instant::now();
        n.pos_moved = Instant::now();
        log(&format!("play: live {name} [{id}], video {}", if copy { "copied" } else { "re-encoded by the server" }));
        if first { self.report("start", None) } else { self.report("progress", Some("timeupdate")) }
        self.osd(5, None);
    }

    /// A channel open nobody waits for any more (flip, stop): close its stream when it returns (frees the tuner slot).
    fn abandon_tune(&self, rx: Tune) {
        let api = self.api.clone();
        std::thread::spawn(move || {
            if let Ok(Ok((l, _))) = rx.recv() {
                log(&format!("close live stream {} (its tune was abandoned)", l.live_id));
                close_live(&api, &l);
            }
        });
    }

    /// Live TV with static: open the channel again behind the static (stream died, retry after a failure, resume).
    fn retune(&mut self, counted: bool) {
        let keep = self.now.as_ref().map(|n| n.waiting && n.child.is_some()).unwrap_or(false);
        if keep {
            self.kill_card();
        } else {
            self.kill_child();
        }
        let spawned = (!keep).then(|| self.spawn_waiting());
        let Some(n) = self.now.as_mut() else { return };
        match spawned {
            Some(Ok((c, l))) => {
                n.child = Some(c);
                n.lines = Some(l);
            }
            Some(Err(e)) => log(&format!("static: {e}")),
            None => {}
        }
        n.waiting = true;
        if !counted {
            n.restarts = 0;
        }
        n.paused = false;
        n.eof = false;
        n.retry_at = None;
        n.start_ticks = 0;
        n.pos_ticks = 0;
        n.last_line = Instant::now();
        n.pos_moved = Instant::now();
        let (old_tune, old_live, id, name) = (n.tune.take(), n.live.take(), n.item_id.clone(), n.name.clone());
        if let Some(rx) = old_tune {
            self.abandon_tune(rx);
        }
        if let Some(l) = old_live {
            self.close_live(&l); // the static is up already: this call can take a moment
        }
        log(&format!("live {name} [{id}]: tuning again behind the static"));
        self.start_tune(&id);
    }

    /// How subtitle track `sub` gets on screen: text tracks are drawn by sljf-play from an SRT that is fetched in the
    /// background (the server may first need ~30 s to extract it from the file; playback does not wait, the player
    /// picks the file up when it appears); image tracks are burned in by the server (which then re-encodes the video).
    /// If the fetch fails, tick() switches to burning in at the current position.
    fn subtitle_tracks(&self, id: &str, item: &Value, audio: Option<i64>, sub: Option<i64>) -> Tracks {
        let mut tr = Tracks { audio, sub, ..Default::default() };
        let keep = sub.map(|i| sub_path(id, i));
        if let Ok(rd) = std::fs::read_dir("/tmp") {    // earlier items' tracks
            for e in rd.flatten() {
                let p = e.path().to_string_lossy().to_string();
                if p.starts_with("/tmp/sljf-sub-") && Some(&p) != keep.as_ref() {
                    let _ = std::fs::remove_file(&p);
                }
            }
        }
        let Some(i) = sub else { return tr };
        let text = streams(item, "Subtitle").iter().find(|s| s["Index"].as_i64() == Some(i)).map(is_text_sub).unwrap_or(false);
        if !text {
            tr.burn = true;
            return tr;
        }
        let path = sub_path(id, i);
        tr.sub_path = Some(path.clone());
        if std::path::Path::new(&path).exists() {
            return tr; // fetched for this item already (restart, resume)
        }
        let url = format!("{}/Videos/{id}/{id}/Subtitles/{i}/0/Stream.srt", self.cfg.server);
        let (auth, tx, id) = (self.cfg.auth(), self.sub_tx.clone(), id.to_string());
        std::thread::spawn(move || {
            let t = Instant::now();
            let agent: ureq::Agent = ureq::Agent::config_builder().timeout_global(Some(Duration::from_secs(150)))
                .http_status_as_error(false).user_agent(UA).build().into();
            // Cloudflare in front of Jellyfin gives up after ~100 s (http 524) while the server is still extracting the
            // track from a big file; it carries on and caches it, so asking again soon gets it (seen: 125 s -> 524,
            // then 11 s). Try up to 6 times before falling back to burning in.
            let mut r = Err(String::new());
            for attempt in 0..6 {
                if attempt > 0 {
                    std::thread::sleep(Duration::from_secs(15));
                }
                r = agent.get(&url).header("Authorization", &auth).call().map_err(|e| e.to_string()).and_then(|mut r| {
                    if r.status().as_u16() >= 300 {
                        return Err(format!("http {}", r.status().as_u16()));
                    }
                    r.body_mut().with_config().limit(8 * 1024 * 1024).read_to_vec().map_err(|e| e.to_string())
                });
                if r.is_ok() || r.as_ref().err().map(|e| e.contains("http 404")).unwrap_or(false) {
                    break;
                }
            }
            let r = r.and_then(|b| {
                let tmp = format!("{path}.part");
                std::fs::write(&tmp, &b).and_then(|_| std::fs::rename(&tmp, &path)).map_err(|e| e.to_string())?;
                Ok(b.len())
            });
            let _ = tx.send(SubFetch { item: id, index: i, secs: t.elapsed().as_secs_f64(), result: r });
        });
        tr
    }

    /// Change tracks of what is playing (Jellyfin's track menus): restart there with the new choice.
    fn set_track(&mut self, audio: Option<i64>, sub: Option<Option<i64>>) {
        let Some(n) = self.now.as_ref() else { return };
        let (id, pos, paused, mut tr) = (n.item_id.clone(), n.pos_ticks, n.paused, n.tr.clone());
        if let Some(a) = audio {
            tr.audio = Some(a);
        }
        if let Some(s) = sub {
            let item = match self.api.get(&format!("/Users/{}/Items/{id}", self.cfg.user_id)) {
                Ok(v) => v,
                Err(e) => return log(&format!("subtitle change: {e}")),
            };
            let s = s.filter(|i| *i >= 0);
            self.sub_pref = match s.and_then(|i| streams(&item, "Subtitle").into_iter().find(|x| x["Index"].as_i64() == Some(i))) {
                Some(x) => SubPref::Lang(x["Language"].as_str().unwrap_or("und").to_string()),
                None => SubPref::Off,
            };
            tr = self.subtitle_tracks(&id, &item, tr.audio, s);
        }
        let decodable = self.decodable(&id);
        if let Some(n) = self.now.as_mut() {
            n.copy = decodable && !tr.burn;
            n.tr = tr;
        }
        if !paused {
            self.restart_at(pos, false);
        }
    }

    fn decodable(&self, id: &str) -> bool {
        self.api.get(&format!("/Users/{}/Items/{id}", self.cfg.user_id)).map(|i| video_decodable(&i)).unwrap_or(false)
    }

    /// Restart sljf-play for the same item at another position (seek, resume, or the stream died: `counted`).
    fn restart_at(&mut self, ticks: i64, counted: bool) {
        if self.cfg.live_tv_static && self.now.as_ref().map(|n| n.chan).unwrap_or(false) {
            return self.retune(counted);
        }
        self.kill_child();
        let Some(mut n) = self.now.take() else { return };
        let ticks = ticks.clamp(0, if n.runtime > 0 { (n.runtime - 5 * TICKS as i64).max(0) } else { i64::MAX });
        let restarts = if counted { n.restarts } else { 0 }; // failures so far (retry_later counted this one)
        let mut failed = None;
        if n.chan {
            // live: the old stream may be gone (that may be why we are here); open the channel again, at "now"
            // (also after a failed reopen, when there is no old stream left)
            if let Some(old) = n.live.take() {
                self.close_live(&old);
            }
            match self.open_live(&n.item_id) {
                Ok((l, _)) => n.live = Some(l),
                Err(e) => failed = Some(format!("live {}: {e}", n.name)),
            }
        }
        let ticks = if n.chan { 0 } else { ticks };
        if failed.is_none() {
            match self.spawn(&n.item_id, n.copy, &n.tr, ticks, n.live.as_ref()) {
                Ok((child, lines, session)) => {
                    self.now = Some(Now {
                        session, start_ticks: ticks, pos_ticks: ticks, paused: false, eof: false, restarts, retry_at: None,
                        last_line: Instant::now(), pos_moved: Instant::now(), child: Some(child), lines: Some(lines), last_report: Instant::now(), ..n
                    });
                    return self.report("progress", Some("timeupdate"));
                }
                Err(e) => failed = Some(format!("restart: {e}")),
            }
        }
        let e = failed.unwrap_or_default();
        n.pos_ticks = ticks;
        self.now = Some(n);
        if counted { self.retry_later(restarts + 1, &e) } else { log(&e); self.stop(true) }
    }

    /// After failure number `k` in a row: try again after a growing wait, or give up when the waits are used up.
    /// Live TV with static: a static player stays up during the wait (the one that waited for this open, or a new one).
    fn retry_later(&mut self, k: u32, why: &str) {
        let with_static = self.cfg.live_tv_static && self.now.as_ref().map(|n| n.chan).unwrap_or(false);
        let Some(n) = self.now.as_mut() else { return };
        match RETRY_S.get(k.saturating_sub(1) as usize) {
            Some(&s) => {
                log(&format!("{why}: retrying at {:.0} s in {s} s ({k}/{})", n.pos_ticks as f64 / TICKS, RETRY_S.len()));
                n.restarts = k;
                n.retry_at = Some(Instant::now() + Duration::from_secs(s));
                if !with_static {
                    n.child = None;
                    n.lines = None;
                    return;
                }
                if n.waiting && n.child.is_some() {
                    return; // the static that waited for the failed open stays
                }
                self.kill_child(); // (callers have stopped the player already: this reaps / clears it)
                let spawned = self.spawn_waiting();
                let Some(n) = self.now.as_mut() else { return };
                n.waiting = true;
                match spawned {
                    Ok((c, l)) => {
                        n.child = Some(c);
                        n.lines = Some(l);
                    }
                    Err(e) => log(&format!("static: {e}")),
                }
            }
            None => {
                log(&format!("{why}: giving up"));
                self.stop(true);
            }
        }
    }

    fn stop(&mut self, clear_queue: bool) {
        self.kill_standby();
        if self.now.is_some() {
            self.kill_child();
            self.report("stop", None);
            if let Some((rx, name)) = self.now.as_mut().and_then(|n| n.tune.take().map(|t| (t, n.name.clone()))) {
                log(&format!("live {name}: tune abandoned"));
                self.abandon_tune(rx);
            }
            if let Some(l) = self.now.as_ref().and_then(|n| n.live.clone()) {
                self.close_live(&l);
            }
            if let Some(n) = &self.now {
                log(&format!("stopped: {} at {:.0} s", n.name, n.pos_ticks as f64 / TICKS));
            }
            self.now = None;
        }
        self.kill_card();
        self.seek_to = None;
        if clear_queue {
            self.queue.clear();
            self.qi = 0;
            self.sleep_at = None;
            self.auto_eps = 0;
            self.next_label = None;
            release_screen();
        }
    }

    fn pause(&mut self, want: Option<bool>) {
        let Some((paused, pos, waiting)) = self.now.as_ref().map(|n| (n.paused, n.pos_ticks, n.waiting)) else { return };
        if waiting {
            return log("live TV: tuning, nothing to pause");
        }
        let p = want.unwrap_or(!paused);
        if p == paused {
            return;
        }
        if p {
            // PAUSED on the bar at once, from the running player: stopping it and the server call take ~2 s, and a
            // remote press with nothing on screen gets pressed again (= resume) (felt slow in testing)
            if let Some(n) = &mut self.now {
                n.paused = true;
                n.retry_at = None;
            }
            self.osd(-1, Some(pos));
            self.kill_child();
            self.show_card();
            log(&format!("paused at {:.0} s", pos as f64 / TICKS));
            self.report("progress", Some("pause"));
        } else {
            log("resuming");
            self.restart_at(pos, false); // reports progress (unpaused)
            self.osd(4, None);
        }
    }

    /// Seconds left of an episode that another one follows, in its last COUNTDOWN_S (else None).
    fn countdown(&self) -> Option<i64> {
        let n = self.now.as_ref()?;
        if n.chan || n.paused || n.runtime <= 0 || self.qi + 1 >= self.queue.len() || n.child.is_none() {
            return None;
        }
        let left = (n.runtime - n.pos_ticks) / TICKS as i64;
        (0..=COUNTDOWN_S).contains(&left).then_some(left)
    }

    /// The hint at the right of the bar: what OK/Back do now, paused, the sleep timer.
    fn hint(&self) -> String {
        let mut h: Vec<String> = Vec::new();
        if self.now.as_ref().map(|n| n.paused).unwrap_or(false) {
            h.push("PAUSED".into());
        }
        if let Some(left) = self.countdown() {
            let next = self.next_label.as_ref().map(|(_, l)| l.as_str()).unwrap_or("next episode");
            if self.no_next {
                h.push("Stops after this one".into());
            } else if self.auto_eps >= STILL_WATCHING_EPS {
                h.push("Still watching? OK plays the next one".into());
            } else {
                h.push(format!("Next: {next} in {left} s · OK plays it · Back cancels"));
            }
        } else if self.no_next {
            h.push("Stops after this one".into());
        }
        if let Some(t) = self.sleep_at {
            let m = t.saturating_duration_since(Instant::now()).as_secs().div_ceil(60);
            h.push(format!("Sleep {m} min"));
        }
        h.join("   ")
    }

    /// Show the on-screen bar for `secs` (-1: until the next one), at position `pos` (None: where it is playing).
    fn osd(&mut self, secs: i32, pos: Option<i64>) {
        let hint = self.hint();
        let Some(n) = self.now.as_mut() else { return };
        let dur = if n.chan { 0.0 } else { n.runtime as f64 / TICKS };
        let p = pos.map(|p| format!("{:.0}", p as f64 / TICKS)).unwrap_or_else(|| "-1".into());
        let line = format!("osd {secs}\t{}\t{dur:.0}\t{hint}\t{p}\n", n.title);
        let pipe = match n.child.as_mut() {
            Some(c) => c.stdin.as_mut(),
            None => self.card.as_mut().and_then(|c| c.stdin.as_mut()),
        };
        if let Some(w) = pipe {
            let _ = w.write_all(line.as_bytes()).and_then(|_| w.flush());
        }
    }

    /// The volume sljf-play plays at: 0 while muted.
    fn heard_volume(&self) -> u8 {
        if self.muted { 0 } else { self.volume }
    }

    /// A remote volume command: remember it (until sljfd restarts), tell the running sljf-play, report it.
    fn set_volume(&mut self, name: &str, args: &Value) {
        let Some((v, m)) = volume_command(name, args, self.volume, self.muted) else { return };
        (self.volume, self.muted) = (v, m);
        log(&format!("volume {v}{}", if m { " (muted)" } else { "" }));
        let line = format!("volume {}\n", self.heard_volume());
        if let Some(w) = self.now.as_mut().and_then(|n| n.child.as_mut()).and_then(|c| c.stdin.as_mut()) {
            let _ = w.write_all(line.as_bytes()).and_then(|_| w.flush());
        }
        self.report("progress", Some("VolumeChange"));
    }

    /// Paused: the bar on its own (`sljf-play --card`), with the paused position (or a seek target).
    fn show_card(&mut self) {
        if self.card.is_none() {
            match Command::new(&self.cfg.player).arg("--card").stdin(Stdio::piped()).stdout(Stdio::null()).stderr(Stdio::null()).spawn() {
                Ok(c) => self.card = Some(c),
                Err(e) => return log(&format!("card: {e}")),
            }
        }
        let pos = self.seek_to.map(|(t, _)| t).or(self.now.as_ref().map(|n| n.pos_ticks));
        self.osd(-1, pos);
    }

    fn kill_card(&mut self) {
        if let Some(mut c) = self.card.take() {
            drop(c.stdin.take()); // end of stdin ends it
            sh(&format!("kill -TERM {}", c.id()));
            let t = Instant::now();
            while t.elapsed() < Duration::from_secs(2) {
                if let Ok(Some(_)) = c.try_wait() {
                    return;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            let _ = c.kill();
            let _ = c.wait();
        }
    }

    /// The d-pad and OK/Back while something plays (the on-TV app is closed then, so these come here):
    ///   OK      pause / resume; during the countdown: the next episode now
    ///   Back    stop (the app comes back where you were); during the countdown: stop after this episode
    ///   Home    stop
    ///   Left/Right  back 10 s / forward 30 s (presses add up, one seek after SEEK_WAIT); Live TV: channel down/up
    ///   Up      show the bar
    ///   Down    sleep timer: off -> 30 -> 60 -> 90 min -> off
    fn remote_key(&mut self, k: &str) {
        log(&format!("remote {k} during playback"));
        self.auto_eps = 0;
        let (live, paused, pos, runtime) = match self.now.as_ref() {
            Some(n) => (n.chan, n.paused, n.pos_ticks, n.runtime),
            None => return,
        };
        let bar = |p: &mut Self, at: Option<i64>| if paused { p.show_card() } else { p.osd(5, at) };
        if self.countdown().is_some() && !self.no_next {
            match k {
                "Select" => return self.next(1),
                "Back" => {
                    self.no_next = true;
                    return self.osd(5, None);
                }
                _ => {}
            }
        }
        match k {
            "Select" => self.pause(None),
            "Back" => {
                self.shared.ended("");
                self.stop(true)
            }
            "GoHome" => {
                // the app reads this when it comes back: Live TV returns to the guide, a film or episode to home
                self.shared.ended(if live { "guide" } else { "home" });
                self.stop(true)
            }
            "MoveUp" => bar(self, None),
            "MoveDown" => {
                let left = self.sleep_at.map(|t| t.saturating_duration_since(Instant::now()).as_secs() / 60);
                self.sleep_at = match left {
                    None => Some(SLEEP_STEPS[0]),
                    Some(m) => SLEEP_STEPS.iter().copied().find(|&s| s > m + 1),
                }
                .map(|m| Instant::now() + Duration::from_secs(m * 60));
                log(&format!("sleep timer: {}", self.sleep_at.map(|t| format!("{} min", t.saturating_duration_since(Instant::now()).as_secs().div_ceil(60))).unwrap_or_else(|| "off".into())));
                let at = self.seek_to.map(|(t, _)| t);
                bar(self, at)
            }
            "MoveLeft" | "MoveRight" if live => self.next(if k == "MoveRight" { 1 } else { -1 }),
            "MoveLeft" | "MoveRight" => {
                let base = self.seek_to.map(|(t, _)| t).unwrap_or(pos);
                let step = if k == "MoveRight" { 30 } else { -10 } * TICKS as i64;
                let max = if runtime > 0 { (runtime - 5 * TICKS as i64).max(0) } else { i64::MAX };
                let target = (base + step).clamp(0, max);
                self.seek_to = Some((target, Instant::now()));
                bar(self, Some(target))
            }
            _ => {}
        }
    }

    fn next(&mut self, step: i64) {
        let mut qi = self.qi as i64 + step;
        let chan = self.now.as_ref().map(|n| n.chan).unwrap_or(false);
        if chan && !self.queue.is_empty() {
            qi = qi.rem_euclid(self.queue.len() as i64); // channel up/down goes round
        }
        if qi < 0 || qi >= self.queue.len() as i64 {
            return self.stop(true);
        }
        if chan && self.cfg.live_tv_static {
            // channel flip: static first (the waiting one of a tune still in progress, else a new one in place of the
            // picture), then the slow part: stopping the old channel, opening the new one (play_live_static)
            let busy = self.now.as_ref().map(|n| n.tune.is_some() || n.tune_at.is_some()).unwrap_or(false);
            let waiting = self.now.as_mut().filter(|n| n.waiting && n.child.is_some() && n.lines.is_some())
                .map(|n| (n.child.take().unwrap(), n.lines.take().unwrap()));
            let standby = match waiting {
                Some(w) => Some(w),
                None => {
                    // as kill_child, but the server call that ends the old encode waits until the static is up
                    self.kill_card();
                    let old = self.now.as_mut().and_then(|n| { n.lines = None; n.child.take() });
                    let had = old.is_some();
                    if let Some(c) = old {
                        end_player(c);
                    }
                    let s = self.spawn_waiting().map_err(|e| log(&format!("static: {e}"))).ok();
                    if had {
                        self.end_encode();
                    }
                    s
                }
            };
            self.stop(false); // also abandons a tune in progress
            self.standby = standby;
            self.settle = busy;
            self.qi = qi as usize;
            self.play_current(0);
            self.settle = false;
            return self.kill_standby(); // not used: the item did not load, or is not a channel
        }
        self.stop(false);
        self.qi = qi as usize;
        self.play_current(0);
    }

    fn command(&mut self, kind: &str, data: &Value) {
        if kind == "Play" || kind == "Playstate" {
            self.auto_eps = 0; // someone is there
        }
        match kind {
            "Play" => {
                let ids: Vec<String> = data["ItemIds"].as_array().map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)).collect()).unwrap_or_default();
                if ids.is_empty() {
                    return;
                }
                let cmd = data["PlayCommand"].as_str().unwrap_or("PlayNow");
                if self.now.is_some() && (cmd == "PlayNext" || cmd == "PlayLast") {
                    let at = if cmd == "PlayNext" { self.qi + 1 } else { self.queue.len() };
                    self.queue.splice(at..at, ids);
                    return;
                }
                self.stop(false);
                self.queue = ids;
                self.qi = (data["StartIndex"].as_u64().unwrap_or(0) as usize).min(self.queue.len() - 1);
                self.asked_audio = data["AudioStreamIndex"].as_i64();
                self.lang = data["AudioLanguage"].as_str().map(String::from).unwrap_or_else(|| self.shared.lang());
                self.asked_sub = data["SubtitleStreamIndex"].as_i64();
                self.sub_pref = match (self.asked_sub, data["SubtitleLanguage"].as_str()) {
                    (None, _) => SubPref::Default,
                    (Some(i), _) if i < 0 => SubPref::Off,
                    (Some(_), Some(l)) => SubPref::Lang(l.to_string()),
                    (Some(_), None) => SubPref::Default,
                };
                self.play_current(data["StartPositionTicks"].as_i64().unwrap_or(0));
            }
            "Playstate" => {
                let pos = self.now.as_ref().map(|n| n.pos_ticks).unwrap_or(0);
                match data["Command"].as_str().unwrap_or("") {
                    "Stop" => self.stop(true),
                    "Pause" => self.pause(Some(true)),
                    "Unpause" => self.pause(Some(false)),
                    "PlayPause" => self.pause(None),
                    "Seek" | "Rewind" | "FastForward" if self.now.as_ref().map(|n| n.chan).unwrap_or(false) => {
                        log("live TV: no seeking")
                    }
                    "Seek" => {
                        if let Some(t) = data["SeekPositionTicks"].as_i64() {
                            log(&format!("seek to {:.0} s", t as f64 / TICKS));
                            self.restart_at(t, false);
                        }
                    }
                    "Rewind" => self.restart_at(pos - 10 * TICKS as i64, false),
                    "FastForward" => self.restart_at(pos + 30 * TICKS as i64, false),
                    "NextTrack" => self.next(1),
                    "PreviousTrack" => self.next(-1),
                    other => log(&format!("playstate {other}: not handled")),
                }
            }
            // navigation for the on-TV app (Jellyfin apps and Home Assistant send these through the server)
            // while something plays the on-TV app is closed (the player has the screen), so the d-pad keys would go
            // nowhere: Back / Home stop playback (the app comes back where you were), OK pauses and resumes
            "GeneralCommand" if self.now.is_some()
                && matches!(data["Name"].as_str(), Some("Back" | "GoHome" | "Select" | "MoveUp" | "MoveDown" | "MoveLeft" | "MoveRight")) =>
            {
                self.remote_key(data["Name"].as_str().unwrap_or(""))
            }
            "GeneralCommand" => match data["Name"].as_str().unwrap_or("") {
                "MoveUp" => self.shared.key("up"),
                "MoveDown" => self.shared.key("down"),
                "MoveLeft" => self.shared.key("left"),
                "MoveRight" => self.shared.key("right"),
                "Select" => self.shared.key("select"),
                "SetAudioStreamIndex" => {
                    if let Some(i) = data["Arguments"]["Index"].as_str().and_then(|s| s.parse::<i64>().ok()) {
                        log(&format!("audio track -> #{i}"));
                        self.set_track(Some(i), None);
                    }
                }
                "SetSubtitleStreamIndex" => {
                    if let Some(i) = data["Arguments"]["Index"].as_str().and_then(|s| s.parse::<i64>().ok()) {
                        log(&format!("subtitle track -> #{i}"));
                        self.set_track(None, Some(Some(i)));
                    }
                }
                "Back" => self.shared.key("back"),
                n @ ("SetVolume" | "VolumeUp" | "VolumeDown" | "Mute" | "Unmute" | "ToggleMute") => self.set_volume(n, &data["Arguments"]),
                "SendString" => self.search_text(&local::search_text(data["Arguments"]["String"].as_str().unwrap_or(""))),
                "GoHome" => {
                    // "home" on the remote also opens the app when the Link is showing its own menu
                    if self.now.is_none() {
                        open_app();
                    }
                    self.shared.key("home")
                }
                other => log(&format!("general command {other}: not handled")),
            },
            _ => {}
        }
    }

    /// Text typed somewhere else (the LAN API, a Jellyfin app's SendString): the app opens Search with it.
    /// Ignored while something plays (the app is closed then and the text would replace what you are watching).
    fn search_text(&mut self, t: &str) {
        if t.is_empty() {
            return;
        }
        if self.now.is_some() {
            return log(&format!("search \"{t}\": ignored, something is playing"));
        }
        log(&format!("search \"{t}\""));
        open_app();
        self.shared.search(t);
    }

    /// Called about once a second: read sljf-play's status, notice its exit, report progress.
    fn tick(&mut self) {
        while let Ok(c) = self.cmds.try_recv() {
            match c {
                local::Cmd::Play { ids, start, ticks, audio, lang, sub, sub_lang } => self.command("Play", &json!({
                    "ItemIds": ids, "StartIndex": start, "StartPositionTicks": ticks, "AudioStreamIndex": audio, "AudioLanguage": lang,
                    "SubtitleStreamIndex": sub, "SubtitleLanguage": sub_lang})),
                local::Cmd::Control(c) => {
                    let name = match c.as_str() {
                        "pause" => "Pause", "unpause" => "Unpause", "playpause" => "PlayPause", "stop" => "Stop",
                        "next" => "NextTrack", "prev" => "PreviousTrack", "back10" => "Rewind", "fwd30" => "FastForward",
                        _ => continue,
                    };
                    self.command("Playstate", &json!({"Command": name}));
                }
                local::Cmd::Search(t) => self.search_text(&t),
            }
        }
        while let Ok(f) = self.sub_rx.try_recv() {
            match f.result {
                Ok(len) => log(&format!("subtitles #{}: {} kB in {:.1} s", f.index, len / 1024, f.secs)),
                Err(e) => {
                    log(&format!("subtitles #{}: {e} after {:.1} s", f.index, f.secs));
                    let current = self.now.as_ref().map(|n| n.item_id == f.item && n.tr.sub == Some(f.index) && !n.tr.burn).unwrap_or(false);
                    if current {
                        log("burning the subtitles in instead");
                        let (pos, paused) = self.now.as_ref().map(|n| (n.pos_ticks, n.paused)).unwrap_or((0, true));
                        if let Some(n) = self.now.as_mut() {
                            n.tr.burn = true;
                            n.tr.sub_path = None;
                            n.copy = false;
                        }
                        if !paused {
                            self.restart_at(pos, false);
                        }
                    }
                }
            }
        }
        // Live TV with static: the channel opened (or failed) on the server, or a flip has settled
        let tuned = self.now.as_ref().and_then(|n| n.tune.as_ref()).and_then(|rx| match rx.try_recv() {
            Ok(r) => Some(r),
            Err(TryRecvError::Empty) => None,
            Err(TryRecvError::Disconnected) => Some(Err("open thread ended".into())),
        });
        if let Some(r) = tuned {
            self.finish_tune(r);
        }
        if let Some(id) = self.now.as_ref().filter(|n| n.tune_at.is_some_and(|t| Instant::now() >= t)).map(|n| n.item_id.clone()) {
            self.start_tune(&id);
        }
        match &self.now {
            Some(n) => self.shared.playing(true, n.paused, &n.name),
            None => self.shared.playing(false, false, ""),
        }
        let mut exited = None;
        if let Some(n) = &mut self.now {
            while let Some(line) = n.lines.as_ref().and_then(|l| l.try_recv().ok()) {
                n.last_line = Instant::now();
                // one status line (queues, sync, cpu, read-ahead) every 30 s, so a stutter can be seen afterwards
                if line.starts_with("t=") && line[2..].trim_start().split('.').next().and_then(|x| x.parse::<u64>().ok()).map(|t| t % 30 == 0).unwrap_or(false) {
                    log(&format!("player: {line}"));
                }
                if let Some(i) = line.find("pos=") {
                    if let Some(p) = line[i + 4..].split_whitespace().next().and_then(|s| s.parse::<f64>().ok()) {
                        let t = n.start_ticks + (p * TICKS) as i64;
                        if t != n.pos_ticks {
                            n.pos_moved = Instant::now();
                        }
                        n.pos_ticks = t;
                        if p > 20.0 {
                            n.restarts = 0;
                        }
                    }
                } else if line.starts_with("end of stream") {
                    n.eof = true;
                } else if ["opened", "video h264", "frame rate", "played out", "sync:", "subs:", "prebuffered", "rebuffer", "read-ahead", "stream error", "open failed", "no ", "video is", "SL", "audio decoder", "static"].iter().any(|p| line.starts_with(p)) {
                    log(&format!("player: {line}"));
                }
            }
            if let Some(Ok(Some(st))) = n.child.as_mut().map(|c| c.try_wait()) {
                exited = Some((st.code().unwrap_or(-1), n.eof, n.pos_ticks, n.restarts, n.runtime));
            } else if n.child.is_some() && !n.waiting && n.last_line.elapsed() > PLAYER_SILENT {
                exited = Some((-2, false, n.pos_ticks, n.restarts, n.runtime)); // hung: kill_child below
            } else if n.child.is_some() && !n.waiting && n.pos_moved.elapsed() > PLAYER_STUCK {
                exited = Some((-3, false, n.pos_ticks, n.restarts, n.runtime)); // picture frozen: as hung
            }
        }
        if let Some((code, ..)) = exited.filter(|_| self.now.as_ref().map(|n| n.waiting).unwrap_or(false)) {
            // the static player died while the channel opens / a retry waits: reap it; the open or retry goes on
            // (finish_tune then starts a normal player, retune a new static one)
            log(&format!("static player exited (code {code}) while waiting for the channel"));
            self.kill_child();
            exited = None;
        }
        let due = self.now.as_ref().and_then(|n| n.retry_at).map(|t| Instant::now() >= t).unwrap_or(false);
        if due {
            let pos = self.now.as_ref().map(|n| n.pos_ticks).unwrap_or(0);
            if let Some(n) = self.now.as_mut() {
                n.retry_at = None;
            }
            return self.restart_at(pos, true);
        }
        if let Some((code, eof, pos, restarts, runtime)) = exited {
            let live = self.now.as_ref().map(|n| n.chan).unwrap_or(false);
            let near_end = runtime > 0 && pos > runtime - 30 * TICKS as i64;
            if !live && ((eof && code == 0) || near_end) {
                let more = self.qi + 1 < self.queue.len();
                if more && self.no_next {
                    log("item finished: stopping (Back during the countdown)");
                    self.stop(true);
                } else if more && self.auto_eps >= STILL_WATCHING_EPS {
                    log(&format!("item finished: {} episodes with no key pressed, stopping (still watching?)", self.auto_eps));
                    self.stop(true);
                } else {
                    log("item finished");
                    if more {
                        self.auto_eps += 1;
                    }
                    self.next(1);
                }
            } else {
                let why = if code == -2 { "player silent for 30 s".to_string() } else if code == -3 { "position stuck for 45 s".to_string() } else { format!("player exited (code {code})") };
                if live && self.cfg.live_tv_static {
                    // reap it (or kill a hung one), static up (retry_later), and only then the server call that ends
                    // the encode: it can block up to the Api timeout when the server is unreachable
                    self.kill_card();
                    let old = self.now.as_mut().and_then(|n| { n.lines = None; n.child.take() });
                    let session = self.now.as_ref().map(|n| n.session.clone()).unwrap_or_default();
                    if let Some(c) = old {
                        end_player(c);
                    }
                    self.retry_later(restarts + 1, &why);
                    self.end_encode_of(&session);
                } else {
                    self.kill_child(); // reaps it (or kills a hung one) and ends the server's encode
                    self.retry_later(restarts + 1, &why);
                }
            }
            return;
        }
        if self.sleep_at.map(|t| Instant::now() >= t).unwrap_or(false) && self.now.is_some() {
            log("sleep timer: stopping");
            return self.stop(true);
        }
        if let Some((target, at)) = self.seek_to {
            if at.elapsed() >= SEEK_WAIT {
                self.seek_to = None;
                let paused = self.now.as_ref().map(|n| n.paused).unwrap_or(false);
                log(&format!("remote seek to {:.0} s{}", target as f64 / TICKS, if paused { " (paused)" } else { "" }));
                if paused {
                    if let Some(n) = self.now.as_mut() {
                        n.pos_ticks = target;
                    }
                    self.report("progress", Some("timeupdate"));
                    self.show_card();
                } else {
                    self.restart_at(target, false);
                    self.osd(4, None);
                }
                return;
            }
        }
        if let Some(left) = self.countdown() {
            if self.next_label.as_ref().map(|(i, _)| *i != self.qi + 1).unwrap_or(true) {
                let label = self.queue.get(self.qi + 1).and_then(|id| self.api.get(&format!("/Users/{}/Items/{id}", self.cfg.user_id)).ok())
                    .map(|v| short_episode(&v)).unwrap_or_else(|| "next episode".into());
                self.next_label = Some((self.qi + 1, label));
            }
            let _ = left;
            self.osd(2, None);
        }
        if self.now.is_some() {
            if self.last_awake.elapsed() > Duration::from_secs(20) {
                keep_awake();
                self.last_awake = Instant::now();
            }
            if self.now.as_ref().map(|n| n.last_report.elapsed() > Duration::from_secs(5)).unwrap_or(false) {
                self.report("progress", Some("timeupdate"));
            }
        }
    }
}

fn set_timeout(ws: &mut WebSocket<MaybeTlsStream<std::net::TcpStream>>, d: Duration) {
    match ws.get_mut() {
        MaybeTlsStream::Plain(s) => { let _ = s.set_read_timeout(Some(d)); }
        MaybeTlsStream::Rustls(s) => { let _ = s.get_mut().set_read_timeout(Some(d)); }
        _ => {}
    }
}

/// One websocket connection: returns when it drops.
fn serve(p: &mut Player) -> Result<(), String> {
    let cfg = p.cfg;
    // the user's audio language (Jellyfin settings), else audio_lang from config.json
    if let Ok(u) = p.api.get(&format!("/Users/{}", cfg.user_id)) {
        let l = u["Configuration"]["AudioLanguagePreference"].as_str().unwrap_or("");
        p.shared.set_lang(if l.is_empty() { &cfg.audio_lang } else { l });
    }
    p.api.post(
        "/Sessions/Capabilities/Full",
        &json!({"PlayableMediaTypes": ["Video"], "SupportedCommands": ["PlayState", "PlayNext", "PlayMediaSource", "MoveUp", "MoveDown", "MoveLeft",
                                      "MoveRight", "Select", "Back", "GoHome", "SetAudioStreamIndex", "SetSubtitleStreamIndex", "SendString",
                                      "SetVolume", "VolumeUp", "VolumeDown", "Mute", "Unmute", "ToggleMute"],
                "SupportsMediaControl": true, "SupportsPersistentIdentifier": true}),
    )?;
    let ws_url = format!("{}/socket?deviceId={}", cfg.server.replacen("http", "ws", 1), cfg.device_id);
    let mut req = ws_url.as_str().into_client_request().map_err(|e| e.to_string())?;
    req.headers_mut().insert("Authorization", cfg.auth().parse().map_err(|_| "bad auth header")?);
    req.headers_mut().insert("User-Agent", UA.parse().unwrap());
    // own TCP connection, so a dead network cannot hang the connect or the TLS handshake (tungstenite::connect has no timeout)
    let host = cfg.server.split("://").nth(1).unwrap_or("").split('/').next().unwrap_or("").to_string();
    let addr = if host.contains(':') { host.clone() } else { format!("{host}:{}", if cfg.server.starts_with("https") { 443 } else { 80 }) };
    let sa = std::net::ToSocketAddrs::to_socket_addrs(&addr).map_err(|e| format!("websocket: {addr}: {e}"))?.next()
        .ok_or(format!("websocket: {addr}: no address"))?;
    let tcp = std::net::TcpStream::connect_timeout(&sa, Duration::from_secs(10)).map_err(|e| format!("websocket: {e}"))?;
    let _ = tcp.set_read_timeout(Some(Duration::from_secs(15)));
    let _ = tcp.set_write_timeout(Some(Duration::from_secs(15)));
    let (mut ws, _) = tungstenite::client_tls(req, tcp).map_err(|e| format!("websocket: {e}"))?;
    set_timeout(&mut ws, Duration::from_millis(700));
    log("connected to Jellyfin, waiting for commands");
    let mut keepalive = Duration::from_secs(30);
    let mut last_ka = Instant::now();
    let mut last_rx = Instant::now(); // the server answers our keepalives: silence = the connection is dead
    loop {
        if last_rx.elapsed() > keepalive * 3 + Duration::from_secs(10) {
            return Err(format!("websocket: nothing from Jellyfin for {} s", last_rx.elapsed().as_secs()));
        }
        let r = ws.read();
        if r.is_ok() {
            last_rx = Instant::now();
        }
        match r {
            Ok(Message::Text(t)) => {
                let v: Value = serde_json::from_str(t.as_str()).unwrap_or(Value::Null);
                let kind = v["MessageType"].as_str().unwrap_or("");
                match kind {
                    "ForceKeepAlive" => {
                        keepalive = Duration::from_secs((v["Data"].as_u64().unwrap_or(60) / 2).max(5));
                        let _ = ws.send(Message::text(r#"{"MessageType":"KeepAlive"}"#));
                    }
                    "KeepAlive" => {}
                    "Play" | "Playstate" | "GeneralCommand" => p.command(kind, &v["Data"]),
                    _ => {}
                }
            }
            Ok(Message::Close(_)) => return Err("server closed the websocket".into()),
            Ok(_) => {}
            Err(tungstenite::Error::Io(e)) if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) => {}
            Err(e) => return Err(format!("websocket: {e}")),
        }
        if last_ka.elapsed() > keepalive {
            ws.send(Message::text(r#"{"MessageType":"KeepAlive"}"#)).map_err(|e| format!("websocket: {e}"))?;
            last_ka = Instant::now();
        }
        p.tick();
    }
}

/// The poster cache (local.rs) only grows: keep the newest files under 150 MB (of 1.8 GB on /mnt/scratch).
fn prune_cache() {
    let Ok(rd) = std::fs::read_dir(local::CACHE) else { return };
    let mut files: Vec<(std::time::SystemTime, u64, std::path::PathBuf)> = rd.flatten()
        .filter_map(|e| e.metadata().ok().map(|m| (m.modified().unwrap_or(std::time::UNIX_EPOCH), m.len(), e.path()))).collect();
    files.sort_by(|a, b| b.0.cmp(&a.0));
    let (mut total, mut removed) = (0u64, 0);
    for (_, len, path) in files {
        total += len;
        if total > 150 * 1024 * 1024 && std::fs::remove_file(path).is_ok() {
            removed += 1;
        }
    }
    if removed > 0 {
        log(&format!("cache: removed {removed} old images"));
    }
}

fn main() {
    let path = std::env::args().nth(1).unwrap_or("/mnt/scratch/sljf/config.json".into());
    let cfg: &'static Cfg = match Cfg::load(&path) {
        Ok(c) => Box::leak(Box::new(c)),
        Err(e) => {
            eprintln!("sljfd: {e}");
            std::process::exit(2);
        }
    };
    let _ = rustls::crypto::ring::default_provider().install_default();
    log(&format!("sljfd {VERSION} starting: {} as \"{}\" ({})", cfg.server, cfg.device_name, cfg.device_id));
    sh("killall sljf-play 2>/dev/null");
    release_screen(); // left over from a crash
    prune_cache();
    let (tx, cmds) = channel();
    let (sub_tx, sub_rx) = channel();
    let shared = std::sync::Arc::new(local::Shared::default());
    if !cfg.lan_allow.is_empty() {
        local::lan_start(cfg, tx.clone());
    }
    local::start(cfg, tx, shared.clone());
    if cfg.open_app_at_boot {
        open_app_at_boot();
    }
    shared.set_lang(&cfg.audio_lang);
    let mut p = Player {
        cfg, api: Api::new(cfg), standby: None, settle: false, queue: vec![], qi: 0, now: None, last_awake: Instant::now(),
        asked_audio: None, lang: cfg.audio_lang.clone(), asked_sub: None, sub_pref: SubPref::Default, cmds, sub_tx, sub_rx, shared,
        card: None, seek_to: None, sleep_at: None, auto_eps: 0, no_next: false, next_label: None,
        volume: cfg.volume, muted: false,
    };
    let mut wait = 2;
    loop {
        let t = Instant::now();
        if let Err(e) = serve(&mut p) {
            log(&e);
        }
        // keep playing through a short websocket drop; only the remote control is gone meanwhile
        if t.elapsed() > Duration::from_secs(60) {
            wait = 2;
        }
        let until = Instant::now() + Duration::from_secs(wait);
        while Instant::now() < until {
            p.tick();
            std::thread::sleep(Duration::from_millis(500));
        }
        // right after boot the clock is still 1970 until NTP sets it, and TLS refuses the server's certificate
        // ("not valid yet"): keep trying every 2 s then, so the Link is back as soon as the time is right
        let clock_set = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() > 1_700_000_000).unwrap_or(false);
        wait = if clock_set { (wait * 2).min(60) } else { 2 };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(extra: &str) -> Cfg {
        let v: Value = serde_json::from_str(&format!("{{\"server\":\"https://x\",\"user_id\":\"u\",\"token\":\"t\"{extra}}}")).unwrap();
        Cfg::parse(&v, "test").unwrap()
    }

    #[test]
    fn cfg_static_defaults() {
        let c = parse("");
        assert!(c.live_tv_static);
        assert_eq!(c.static_volume, 8);
    }

    #[test]
    fn cfg_static_set_and_clamped() {
        let c = parse(",\"live_tv_static\":false,\"static_volume\":250");
        assert!(!c.live_tv_static);
        assert_eq!(c.static_volume, 100);
        assert_eq!(parse(",\"static_volume\":0").static_volume, 0);
    }

    #[test]
    fn live_url_line() {
        assert_eq!(url_line("https://x/Videos/1/stream.ts?a=1&b=2").as_deref(), Some("url https://x/Videos/1/stream.ts?a=1&b=2\n"));
        assert_eq!(url_line("https://x/a\nosd 5"), None);
        assert_eq!(url_line("https://x/a\r"), None);
    }
}

#[cfg(test)]
mod volume_tests {
    use super::*;

    #[test]
    fn config_volume_clamps() {
        let v: Value = serde_json::from_str(r#"{"a": 40, "b": 150, "c": -3, "d": "x", "e": 37.6}"#).unwrap();
        assert_eq!(cfg_volume(&v["a"]), 40);
        assert_eq!(cfg_volume(&v["b"]), 100);
        assert_eq!(cfg_volume(&v["c"]), 0);
        assert_eq!(cfg_volume(&v["d"]), 100);
        assert_eq!(cfg_volume(&v["e"]), 38);
        assert_eq!(cfg_volume(&v["missing"]), 100);
    }

    #[test]
    fn volume_steps() {
        let none = Value::Null;
        assert_eq!(volume_command("VolumeUp", &none, 40, false), Some((45, false)));
        assert_eq!(volume_command("VolumeUp", &none, 98, false), Some((100, false)));
        assert_eq!(volume_command("VolumeUp", &none, 40, true), Some((45, false)));
        assert_eq!(volume_command("VolumeDown", &none, 40, false), Some((35, false)));
        assert_eq!(volume_command("VolumeDown", &none, 3, false), Some((0, false)));
        assert_eq!(volume_command("VolumeDown", &none, 40, true), Some((35, true)));
    }

    #[test]
    fn set_volume_and_mute() {
        let a = |s: &str| serde_json::from_str::<Value>(s).unwrap();
        assert_eq!(volume_command("SetVolume", &a(r#"{"Volume": "40"}"#), 100, false), Some((40, false)));
        assert_eq!(volume_command("SetVolume", &a(r#"{"Volume": 250}"#), 100, true), Some((100, true)));
        assert_eq!(volume_command("SetVolume", &a(r#"{"Volume": "-1"}"#), 50, false), Some((0, false)));
        assert_eq!(volume_command("SetVolume", &a(r#"{}"#), 50, false), None);
        assert_eq!(volume_command("Mute", &Value::Null, 50, false), Some((50, true)));
        assert_eq!(volume_command("Unmute", &Value::Null, 50, true), Some((50, false)));
        assert_eq!(volume_command("ToggleMute", &Value::Null, 50, true), Some((50, false)));
        assert_eq!(volume_command("ToggleMute", &Value::Null, 50, false), Some((50, true)));
        assert_eq!(volume_command("Back", &Value::Null, 50, false), None);
    }
}
