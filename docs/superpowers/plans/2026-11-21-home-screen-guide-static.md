# Home Screen, TV Guide and Live TV Static Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A Roku-like home screen (JELLYFIN / LIVE TV / STEAM) at the top of the on-TV app, a TV guide screen, and TV static with a faint hiss for the whole Live TV tuning wait and on drops.

**Architecture:** The app (`app/main.qml`) gets two new screens: the home screen (a level above today's Jellyfin home) and the guide (`app/Guide.qml`, layout math in `app/guide.js`). sljfd gets a `/guide` endpoint, two config keys, `after`/`channel` fields in `/events`, and a non-blocking Live TV tune that starts `sljf-play --static` at once and hands it the stream URL on stdin when the server has opened the channel. The player draws static on its own full-screen overlay (`player/sljf-static.c`, noise/hiss generators in `player/static-gen.c`).

**Tech Stack:** Rust (sljfd, serde_json, no new crates), C99 + Valve SDK SLVideo/SLAudio (player), Qt Quick 2.12 QML + JS (app), node (host tests of `guide.js`), cc (host tests of `static-gen.c`).

**Spec:** `docs/superpowers/specs/2026-11-20-home-screen-and-live-tv-static-design.md`

## Global Constraints

- Config: `live_tv_static` default `true`; `static_volume` default `8`, percent of full scale, clamp 0-100, `0` = silent static.
- Static never over real video or sound: hidden at the first displayed video frame; hiss stops when programme audio plays.
- No black gap longer than ~0.5 s between choosing/flipping a channel and static appearing.
- Home goes to the home screen everywhere, except while watching Live TV, where it opens the guide (playback stops). Back on the home screen does nothing; STEAM is the only way to the Link's menu.
- Guide: about 2 h from the current half hour; no scrolling into the past; OK on any block tunes that channel now; channels without data show one "No information" block; opens focused on the last watched channel.
- Static only for Live TV (films/episodes unchanged). CPU for static small next to decoding.
- Nothing network-facing added outside 127.0.0.1; no server URLs, tokens or house details in the repo.
- Every PR: CHANGELOG line under `## [Unreleased]`; `scripts/check.sh` and `scripts/build.sh` + `scripts/build-app.sh` pass; tested on a Link (say how in the PR).

## Review Focus

1. A flip (Left/Right) or Back/Home pressed while a channel is still opening on the server: the old tune must be abandoned and its server live stream closed when it returns (no leaked tuner slot), and the new channel tuned. Pinned in Task 6 (Link test with two quick flips; log shows `close live stream` for the abandoned tune).
2. The channel fails to open (server error, ErsatzTV down): static stays during retries, then the app comes back as today, no static left on screen. Pinned in Task 6 (Link test with a bogus channel id via `/play`).
3. A channel with no guide data, or the guide fetch failing (Jellyfin down): guide shows channels with "No information", or the app's usual "JELLYFIN IS NOT ANSWERING" note, never a blank screen. Pinned in Tasks 2 and 7.
4. Restore after playback for every way playback can end: Back (where you were), Home on Live TV (guide on that channel), Home on a film (home screen), playback ending by itself (where you were). Pinned in Task 7 (shot.py runs with `after` values).
5. A/V sync after a rebuffer with hiss fed into the programme audio stream: back within normal sync (|a-v| < 100 ms) within 2 corrections. Pinned in Task 5 (relay test).

---

### Task 1: Config keys and `/events` fields (daemon)

**Files:**
- Modify: `daemon/src/main.rs` (Cfg, Cfg::load, remote_key, next/play_live where the channel is known, module doc)
- Modify: `daemon/src/local.rs` (State, Shared, `/events`, module doc)
- Modify: `docs/CONFIG.md`, `examples/config.example.json`

**Interfaces:**
- Produces: `Cfg.live_tv_static: bool`, `Cfg.static_volume: u8`; `fn Cfg::parse(v: &Value, path: &str) -> Result<Cfg, String>` (load = read + parse); `Shared::ended(&self, after: &str)` sets `State.after` ("home" | "guide" | "") and bumps seq; `Shared::channel(&self, id: &str)` sets `State.channel`; `fn events_json(s: &State, since: u64) -> Value` in local.rs; `/events` JSON gains `"after": String, "channel": String|null`. `after` is cleared by `Shared::playing(true, ..)`.

- [ ] **Step 1: Write failing tests** in `daemon/src/main.rs` `#[cfg(test)] mod tests`:
  - `cfg_static_defaults`: parse `{"server":"https://x","user_id":"u","token":"t"}` → `live_tv_static == true`, `static_volume == 8`.
  - `cfg_static_set_and_clamped`: `{"...","live_tv_static":false,"static_volume":250}` → `false`, `100`; `"static_volume":0` → `0`.
  - in `local.rs` tests: `events_after_and_channel`: a `State` with `after = "guide"`, `channel = Some("abc")` → `events_json(&s, 0)["after"] == "guide"` and `["channel"] == "abc"`; after `Shared::playing(true, false, "x")` the state's `after` is `""`.
- [ ] **Step 2:** `cd daemon && cargo test` → the new tests fail to compile (no such fields/functions).
- [ ] **Step 3: Implement** the fields, `Cfg::parse`, `Shared::ended` / `Shared::channel`, `events_json` (move the JSON building out of the `/events` route). In `remote_key`: `"GoHome"` → `self.shared.ended(if live { "guide" } else { "home" })` then `stop(true)`; `"Back"` unchanged (`ended("")`). Call `self.shared.channel(id)` whenever a Live TV channel starts playing (play_live and the flip path).
- [ ] **Step 4:** `cargo test` → all pass; `scripts/check.sh` → all ok.
- [ ] **Step 5: Docs:** CONFIG.md rows `live_tv_static` (`true`, "Static on screen with a faint hiss while a Live TV channel tunes, and when it drops or rebuffers") and `static_volume` (`8`, "Hiss loudness with the static, percent of full scale; 0 = silent"); example config gets both keys.
- [ ] **Step 6: Commit** `git commit -m "sljfd: live_tv_static / static_volume config, after + channel in /events"`

### Task 2: `/guide` endpoint (daemon)

**Files:**
- Create: `daemon/src/guide.rs`
- Modify: `daemon/src/main.rs` (`mod guide;`), `daemon/src/local.rs` (route `/guide`, module doc)

**Interfaces:**
- Produces: `pub fn iso_utc(secs: i64) -> String` ("2026-11-21T03:00:00Z"); `pub fn parse_iso(s: &str) -> Option<i64>` (Jellyfin dates like "2026-11-21T03:00:00.0000000Z" → epoch s); `pub fn guide_json(channels: &Value, programs: &Value, now: i64) -> Value` → `{"now": i64, "channels": [{"id","number","name","programs":[{"id","name","episode","start","end","overview"}]}]}`; channels sorted by number (numeric when the number parses, else text), programmes sorted by start, start/end in epoch seconds, overview ≤ 400 chars. Route: `GET /guide?hours=N` (N 1..6, default 3) → Jellyfin `/LiveTv/Channels?UserId={uid}&EnableImageTypes=Primary&ImageTypeLimit=1` and `/LiveTv/Programs?UserId={uid}&MinEndDate={iso(now)}&MaxStartDate={iso(now+N h)}&SortBy=StartDate&Fields=Overview&EnableImageTypes=None&Limit=2000`; Jellyfin unreachable → `502 "jellyfin not answering"` (like `/home`).

- [ ] **Step 1: Write failing tests** in `guide.rs`:
  - `iso_round_trip`: `parse_iso(&iso_utc(1_795_230_000)) == Some(1_795_230_000)`; `parse_iso("2026-11-21T03:00:00.0000000Z") == Some(1_795_230_000)`; `iso_utc(1_795_230_000) == "2026-11-21T03:00:00Z"`; `parse_iso("garbage") == None`.
  - `guide_groups_and_sorts`: two channels numbered "12" and "2", three programmes (two on "2" given out of order, none on "12") → channels order `["2","12"]`, channel "2" programmes ordered by start, channel "12" `programs == []`.
  - `guide_episode_name`: a programme with `"EpisodeTitle":"Homer's Odyssey"` → `"episode":"Homer's Odyssey"`.
- [ ] **Step 2:** `cargo test guide` → fails (module missing).
- [ ] **Step 3: Implement** `guide.rs` (days-from-civil date math, no new crates) and the route.
- [ ] **Step 4:** `cargo test` → pass. On a Link (after Task 8's deploy or `scripts/deploy.sh` now): `ssh steamlink 'wget -q -O - "http://127.0.0.1:8097/guide?hours=2"' | python3 -m json.tool | head -40` → channels with programmes, `now` ≈ `date +%s`.
- [ ] **Step 5: Commit** `git commit -m "sljfd: /guide endpoint (channels + programmes for the next N hours)"`

### Task 3: Guide layout math (app JS, host-tested)

**Files:**
- Create: `app/guide.js` (`.pragma library`), `app/test/guide.test.js`
- Modify: `scripts/check.sh` (run `node app/test/guide.test.js` when node exists; `bad` on failure), `scripts/build-app.sh` and `scripts/deploy.sh` (copy `guide.js`)

**Interfaces:**
- Produces (all times epoch seconds): `windowStart(now: number, slots: number) -> number` (half hour at/below `now`, plus `slots` × 1800; `slots` ≥ 0); `SPAN = 7200`; `blocks(programs: Array, start: number, end: number) -> Array<{x0: number, x1: number, p: object|null}>` with x0/x1 as fractions 0..1 of the window, clipped to it, gaps between programmes and an empty channel filled by `{p: null}` ("No information"); `focusIndex(blocks: Array, t: number) -> number` (block covering t, else the last block starting before t, else 0); `nextTime(block, start) -> number` (the time to move focus to after Left/Right: the block's start clipped to the window start).

- [ ] **Step 1: Write failing tests** in `app/test/guide.test.js` (loads `guide.js` with `vm` after stripping the `.pragma` line; plain `assert`):
  - `windowStart(1800*100 + 900, 0) === 1800*100`; `windowStart(1800*100, 2) === 1800*102`.
  - programmes 0-1800 and 1800-5400 in window 0-7200 → blocks `[{0,0.25,p1},{0.25,0.75,p2},{0.75,1,null}]`.
  - a programme starting before the window (−600..1200) is clipped to x0 = 0.
  - `blocks([], 0, 7200)` → one `{0,1,null}`.
  - `focusIndex(that, 2000) === 1`.
- [ ] **Step 2:** `node app/test/guide.test.js` → fails (no guide.js).
- [ ] **Step 3: Implement** `app/guide.js`; add it to check.sh, build-app.sh (`cp`) and deploy.sh (scp list).
- [ ] **Step 4:** `node app/test/guide.test.js` → prints `ok`; `scripts/check.sh` shows `ok   app/guide.js tests`.
- [ ] **Step 5: Commit** `git commit -m "app: TV guide layout math (guide.js) with host tests"`

### Task 4: Static noise and hiss generators (C, host-tested)

**Files:**
- Create: `player/static-gen.c`, `player/static-gen.h`, `player/test/static-gen-test.c`
- Modify: `scripts/check.sh` (compile with `cc -std=c99 -O2` and run when cc exists)

**Interfaces:**
- Produces: `#define STATIC_W 320`, `#define STATIC_H 180`; `void static_noise(uint32_t *px, int w, int h, int pitch_bytes, uint32_t *seed)` — ARGB grey (A = 0xFF, R = G = B), a cheap xorshift PRNG, slight horizontal banding allowed; `struct hiss { uint32_t seed; float lp; };` `void static_hiss(int16_t *lr, int frames, int volume_pct, struct hiss *h)` — interleaved stereo, white noise through a one-pole low-pass, peak ≤ 32767 × volume_pct / 100, volume 0 → all zeros.

- [ ] **Step 1: Write failing tests** in `static-gen-test.c`: noise buffer 320×180 → every pixel `>> 24 == 0xFF` and R == G == B; not all pixels equal; same seed twice → same buffer; hiss 960 frames at 8 % → every |sample| ≤ 32767*8/100 and RMS > 0; at 0 % → all zero.
- [ ] **Step 2:** `cc -std=c99 player/test/static-gen-test.c player/static-gen.c -o /tmp/sgt && /tmp/sgt` → link error (functions missing).
- [ ] **Step 3: Implement** `static-gen.c`; add the compile+run to check.sh (`ok   player static generators`).
- [ ] **Step 4:** the command above prints `ok`; `scripts/check.sh` all ok.
- [ ] **Step 5: Commit** `git commit -m "player: static noise + hiss generators with host tests"`

### Task 5: Static in the player (`--static`, SLJF_STATIC, rebuffer)

**Files:**
- Create: `player/sljf-static.c`
- Modify: `player/sljf-play.c` (usage doc, main, read_ctl, the feed loop), `player/sljf-subs.c` (`subs_suspend`), `scripts/build.sh` (link `sljf-static.c static-gen.c`)

**Interfaces:**
- Consumes: Task 4 `static_noise`, `static_hiss`.
- Produces (player CLI, used by Task 6): `sljf-play --static` with env `SLJF_STATIC=<0-100>` plus the usual env: shows static at once, then waits for a stdin line `url <URL>` and plays that URL (static stays until the first frame); end of stdin or SIGTERM before a URL → exit 0. `sljf-play <url>` with `SLJF_STATIC` set: static during open/prebuffer and rebuffers. Without `SLJF_STATIC`: behaviour unchanged.
- C: `int static_show(CSLVideoContext *v, int volume)` (own full-screen overlay STATIC_W×STATIC_H, display area 0,0,1,1; creates its own SLAudio 48 kHz stereo stream when no programme stream exists yet); `void static_tick(double now_s)` (new noise ~15/s, keeps ≤ 100 ms of hiss queued); `void static_hide(void)`; `int static_visible(void)`; `void static_give_audio(CSLAudioContext **c, CSLAudioStream **s)` (hands its stream to the player so programme audio uses the same stream, no second stream); `void subs_suspend(int on)` in sljf-subs.c (hide the strip and skip drawing while on).

- [ ] **Step 1: Restructure main** so SLVideo context creation happens before `avformat_open_input` when static is on, and the URL can come from stdin (`read_ctl` recognises `url ` lines and stores the URL; other lines still go to `osd_command`).
- [ ] **Step 2: Implement sljf-static.c** and wire it: show at start; `static_tick` in the open wait, the prebuffer wait and the main loop; hide when `v_first >= 0 && vfed - vq/1000.0 > 0.2` (first frame on screen); show again (picture + hiss into the programme stream while `aq` < 60 ms, treated like the sync code's inserted silence at sljf-play.c:352) when the video queue has been empty for 700 ms after playback started; print `static on` / `static off` lines (sljfd logs them).
- [ ] **Step 3: Build:** `scripts/build.sh` → no warnings for the player.
- [ ] **Step 4: Link checks** (nothing else playing: `tools/sljf-remote status`), photos of the TV:
  - `ssh steamlink 'SLJF_STATIC=8 /mnt/scratch/sljf/sljf-play --static'` for 5 s then Ctrl-C → snow on the TV, faint hiss (owner's ear), exit 0, `static on` printed.
  - `tools/sljf-test` on a Live TV channel with `SLJF_STATIC=8` → static until picture, `static off` before the first status line with `fail=0`.
  - Rebuffer with the burst/stall test relay used for the Live TV stutter fix (`SLJF_PREBUFFER_MS=3000 SLJF_STATIC=8`) → `static on` at each starve, `static off` on recovery, `sync:` lines show |a-v| < 100 ms within 2 corrections after each.
  - A film via `tools/sljf-test` without `SLJF_STATIC` → no `static` lines.
- [ ] **Step 5: Commit** `git commit -m "player: TV static with faint hiss while tuning and on rebuffer (--static, SLJF_STATIC)"`

### Task 6: Non-blocking Live TV tune with static (daemon)

**Files:**
- Modify: `daemon/src/main.rs` (Now, play_live, restart_at/retry_later for live, stop, next, tick, spawn)

**Interfaces:**
- Consumes: Task 5 CLI (`--static`, `url <URL>` line), Task 1 `Cfg.live_tv_static`, `Cfg.static_volume`, `Shared::channel`.
- Produces: `Now.tune: Option<Receiver<Result<(Live, bool), String>>>` (a server open in progress); `fn spawn_waiting(&self) -> Result<(Child, Receiver<String>), String>` (`player --static` with all spawn() env + `SLJF_STATIC`, `SLJF_PREBUFFER_MS=3000`); `fn start_tune(&mut self, id: &str)` (open_live in a thread, result on the channel); `fn finish_tune(&mut self, r: Result<(Live,bool),String>)` called from tick (builds the stream URL with a new session and writes `url <URL>\n` to the waiting player's stdin; error → the existing retry/stop path); `fn abandon_tune(&self, rx: Receiver<...>)` (thread waits for the result and closes that live stream).

- [ ] **Step 1: Write a failing unit test** `live_url_line`: `fn url_line(url: &str) -> Option<String>` returns `Some("url <url>\n")`, and `None` for a URL containing `\n` or `\r` — the only pure piece; the rest is verified on the Link.
- [ ] **Step 2: Implement:** when `live_tv_static` is on, play_live calls `shared.playing(true, ..)`, `take_screen()`, `spawn_waiting()`, `start_tune(id)` and returns (the app leaves the screen at once; no STARTING wait). Flip (`next`) / stop / Back / Home during a tune → `abandon_tune`. Live retries (`retry_later` while `live`) keep a waiting static player up during the backoff instead of no child. With `live_tv_static` off: today's blocking behaviour, unchanged.
- [ ] **Step 3:** `cargo test` + `scripts/check.sh` → pass; `scripts/build.sh` → builds.
- [ ] **Step 4: Link checks** (deploy with `scripts/deploy.sh` from a clean worktree; photos of the TV):
  - OK on a channel in the app → static within 0.5 s (no STARTING screen), picture after the server opens it, log `play: live ...`.
  - Two quick Right presses during the wait → lands on the second channel; log shows the first tune abandoned and `close live stream` for it; Jellyfin dashboard shows one active stream.
  - `/play?id=<a non-channel bogus id>` and a channel with ErsatzTV unreachable (owner may skip) → static during retries, then the app back, no static left.
  - `live_tv_static: false` in config → old behaviour (STARTING screen, black on flip).
- [ ] **Step 5: Commit** `git commit -m "sljfd: Live TV tunes in the background behind static; flips during the wait cancel it"`

### Task 7: Home screen and TV guide (app)

**Files:**
- Create: `app/Guide.qml`
- Modify: `app/main.qml` (screen state, key(), poll/restore, loadHome), `app/main.cpp` only if `Key_Home` mapping needs it (it exists), `scripts/build-app.sh` + `scripts/deploy.sh` (copy `Guide.qml`), `app/shot.py` (`after:` and `guide` helpers if useful)

**Interfaces:**
- Consumes: Task 1 `/events` `after`, `channel`; Task 2 `/guide`; Task 3 `guide.js`; existing `/play?id=` for tuning.
- Produces: root property `screen: "home" | "jellyfin" | "guide"` (Jellyfin pages/search/detail only while `screen === "jellyfin"`); `function openGuide(channelId)`; Guide.qml signals `tune(string channelId)` and `back()`; saved state includes `screen` and the guide's focused channel.

- [ ] **Step 1: Home screen:** header "STEAM LINK" + clock, three tiles JELLYFIN / LIVE TV / STEAM (LIVE TV only when `/home` has channels), hints `ARROWS MOVE  OK SELECT`, theme colours/font from theme.js. Keys: Left/Right move, OK: JELLYFIN → `screen = "jellyfin"` (depth 0), LIVE TV → `openGuide(lastChannel)`, STEAM → `Qt.quit()`. Back on home → nothing. `key("home")` anywhere → home screen. Back at the Jellyfin home (depth 0, not searching) → home screen instead of `Qt.quit()`.
- [ ] **Step 2: Guide.qml:** top strip (focused programme title, time "8:30–9:00 PM", episode, overview), time ruler for the window with a "now" line, rows of channels (logo via `/image?id=`, number, name) with programme blocks from `guide.js` `blocks()`. Up/Down change channel (scroll rows), Left/Right move between blocks; Right past the window end shifts the window by one half-hour slot, Left never before `windowStart(now, 0)`. OK → `tune(channelId)` → app calls `/play?id=channelId`. Back → where it was opened from (the home screen when it was opened by Home during Live TV); Home → home screen. Fetch `/guide?hours=3` on open and when the window passes the loaded range; failure → the app's existing note text.
- [ ] **Step 3: Restore:** on start (`--restore` or not) read `/events` once: `after === "guide"` → `openGuide(channel)`; `after === "home"` → home screen; else `restoreState()` as today (which now includes `screen`). Normal start and `open_app_at_boot` → home screen.
- [ ] **Step 4: Build + screenshots** (`scripts/build-app.sh`; `ssh -N -L 8097:127.0.0.1:8097 steamlink &`): `app/shot.py /tmp/home.png` → home screen with 3 tiles; `app/shot.py /tmp/guide.png right select wait:2000` → guide grid with programme blocks and the top strip; `app/shot.py /tmp/jf.png select back` → back on the home screen; `... home` from a Jellyfin page → home screen. Read each PNG and check it.
- [ ] **Step 5: Commit** `git commit -m "app: home screen (Jellyfin, Live TV, Steam) and TV guide"`

### Task 8: Docs, CHANGELOG, end-to-end on the Link, PR

**Files:**
- Modify: `CHANGELOG.md` (`### Added` under [Unreleased]: home screen, TV guide, Live TV static with hiss + the two config keys; `### Changed`: Home now goes to the home screen / guide, Back on the Jellyfin home goes to the home screen, Live TV no longer shows STARTING), `README.md` (What it does), `docs/ARCHITECTURE.md` (static player mode, tune flow, `/guide`), `docs/integrations/home-assistant.md` (GoHome meaning), `docs/DEVELOPING.md` (node/cc host tests)

- [ ] **Step 1:** Docs + CHANGELOG as above.
- [ ] **Step 2:** `scripts/check.sh`, `scripts/build.sh`, `scripts/build-app.sh` → clean.
- [ ] **Step 3: End-to-end on the Link** (`scripts/deploy.sh`, photos of the TV for the PR description): reboot → home screen; JELLYFIN → Jellyfin home, Back → home; LIVE TV → guide on the last channel; OK → static → picture; Left/Right → static → next channel; Home → guide on that channel; Home → home screen; a film → Home → home screen, Back during a film → where you were; STEAM → the Link's menu, remote Home → the app's home screen.
- [ ] **Step 4: PR:** push the branch, `gh pr create` with the repo's template (how it was tested on hardware), CI green → squash merge. Release as the next alpha with `scripts/bump-version` + `scripts/release.sh` (user-facing).
- [ ] **Step 5:** the owner judges what only a person can (hiss loudness by ear, guide readability from the couch).
