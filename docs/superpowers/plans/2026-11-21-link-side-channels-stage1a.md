# TV channels built on the box, release 1a: implementation plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Collections named `Channel <N> <name>` on Jellyfin become TV channels on the Steam Link: scheduled by the
clock, played as normal library files with seamless cuts, bumpers/commercials between items, a fetch-ahead cache on a
USB stick, shown in the existing guide.

**Architecture:** New pure modules in sljfd (channel parsing, scheduler, filler) plus I/O modules (Jellyfin snapshot,
USB stick, fetcher + local `/cache` endpoint) and glue in `Player` (main.rs). `sljf-play` gains a channel mode that
plays a queue of items into one SLVideo/SLAudio stream (`next ...` lines on stdin) and reports `item N start/end`.
The app is unchanged except where channel ids reach it (`ch:<N>`).

**Tech Stack:** Rust 2024 (serde_json, ureq; no new crates), C99 + FFmpeg 7.1 + Steam Link SDK (SLVideo/SLAudio),
Qt Quick app untouched.

**Spec:** `docs/superpowers/specs/2026-11-21-link-side-channels-design.md` (release 1a only; 1b = breaks inside
episodes + loudness matcher, not in this plan).

## Global Constraints

- No server URL, token, house IP or the owner's lineup anywhere in the repo (tests use made-up ids and names).
- Never write video to the Link's internal flash; small JSON (snapshot, ≤ 1 write per channel per day) in
  `/mnt/scratch/sljf/cache` is fine.
- The timeline depends only on server data + config, never on the USB stick.
- Channel playback sends **no** progress/stopped reports to Jellyfin (no watched state, no resume points).
- No channels (no matching collections, or `channels: false`) = today's behaviour exactly, including Live TV.
- Host load: one stream per box at a time (a few seconds of overlap at a cut is fine), fetch capped at
  `fetch_rate` x the item's bitrate.
- `scripts/check.sh` passes after every task; new host tests are wired into it.
- C: `-std=c99 -Wall -Wextra` clean; Rust: no warnings in `cargo check`.
- Keep the existing file style: comments say why, log lines in plain words (`log(...)` in sljfd, `printf` lines in the
  player that sljfd parses).

## Review Focus

1. **Collection empty / series with zero episodes / every item missing RunTimeTicks**: the channel is skipped with a
   log line, never a divide-by-zero or an endless loop in the scheduler. Test in Task 2 + Task 3.
2. **The clock jumps (NTP sets it from 1970 after boot, or the box sleeps)**: sljfd recomputes "now" from the plan
   rather than trusting elapsed time, and a time before 2020 means "not ready" (no channel playback, a log line).
   Test in Task 2 (`now_before_2020_is_none`).
3. **USB stick pulled mid-play or full**: the current item continues from the server at its scheduled offset, cache
   off, no crash; `/cache` requests for a file that vanished return 404 and the player exits 3 so sljfd retries
   direct. Test in Task 6 (`cache_serve_missing_file_404`) + Link check in Task 9.
4. **A flip during an item boundary or while the next item is being opened**: the old player is stopped, the
   fetcher job for the old channel is cancelled, no stale `next` line reaches the new player. Test in Task 7
   (`stale_item_lines_ignored`).
5. **An item the Link cannot decode (HEVC) or an open failure mid-channel**: sljfd uses the re-encode URL (as for
   films); an open failure skips to the filler / next item at its scheduled time instead of ending the channel.
   Test in Task 7 (`open_failure_moves_on`).

---

### Task 1: Player channel mode, part 1: items as a struct (no behaviour change)

Refactor `player/sljf-play.c` so one playable item's state lives in a struct, as groundwork for a queue.

**Files:**
- Modify: `player/sljf-play.c` (the per-stream state in `main()` and the reader/FIFO statics)
- Create: `player/item.h` (the struct, so later helpers and tests can include it)

**Interfaces:**
- Produces: `struct item { AVFormatContext *fmt; int vi, ai; AVBSFContext *bsf; AVCodecContext *adec; SwrContext *swr;
  AVPacket *fifo[FIFO_MAX]; double fifo_ts[FIFO_MAX]; int fifo_head, fifo_n; double last_ts; int rd_eof, rd_err;
  volatile sig_atomic_t rd_quit; pthread_t rd_th; int threaded; double limit_s; double first_ts; int seq; int kind; }`
  (`kind`: 0 video, 1 card), `int item_open(struct item *it, const char *url, AVDictionary *opts)`,
  `void item_close(struct item *it)`, reader thread taking `struct item *`. One global mutex/cond pair stays.

- [ ] **Step 1:** Move FIFO, reader, demuxer, bitstream filter, audio decoder and resampler state into `struct item`;
  `main()` uses one `struct item cur`. Behaviour, output lines and exit codes stay byte-identical.
- [ ] **Step 2:** Build on the host for syntax only: `cc -std=c99 -Wall -Wextra -fsyntax-only -I player
  $(pkg-config --cflags libavformat) -I /mnt/fast/steamlink-sdk/rootfs/usr/include/SDL2 player/sljf-play.c` if headers
  resolve; otherwise `SDK=/mnt/fast/steamlink-sdk scripts/build.sh`. Expected: no warnings.
- [ ] **Step 3:** `scripts/check.sh`. Expected: all `ok`.
- [ ] **Step 4:** Commit `player: per-item state in a struct (no behaviour change)`.

### Task 2: Scheduler (pure)

**Files:**
- Create: `daemon/src/sched.rs` (+ `mod sched;` in main.rs)

**Interfaces:**
- Produces:
  ```rust
  pub struct Ep { pub id: String, pub secs: u32, pub title: String, pub episode: String, pub overview: String }
  pub struct Show { pub id: String, pub eps: Vec<Ep> }            // a film = a Show with one Ep
  pub struct ChannelDef { pub number: u32, pub name: String, pub collection_id: String, pub shows: Vec<Show>,
                          pub filler: bool }                    // filler: server has Bumpers/Commercials pools for it
  pub struct Rules { pub pad: Option<bool>, pub break_secs: u32, pub epoch: i64 }
  pub struct Slot { pub start: i64, pub item_end: i64, pub end: i64, pub ep: usize, pub show: usize }
  pub fn slots(ch: &ChannelDef, r: &Rules, from: i64, until: i64) -> Vec<Slot>  // slots overlapping [from, until)
  pub fn now(ch: &ChannelDef, r: &Rules, t: i64) -> Option<(Slot, i64)>     // slot on air at t + offset into it
  pub fn epoch_for(day_start_local_secs: i64, utc_offset_secs: i64) -> i64  // 2026-01-01 at day start, as epoch s
  ```
- Rules (from the spec): the timeline is one continuous run from `epoch`. Rounds r = 0, 1, …: show order = the shows
  sorted by id, then shuffled with splitmix64 seeded `(number as u64) << 32 | r`; each show contributes its next
  episode (per-show counter, wrapping). Slot length: `item_secs + break` where `break = break_secs` when filler is on
  (`pad == Some(true)` or (`pad == None` and `ch.filler`)), else 0; when padding is on, the slot end rounds up to
  the next multiple of 1800 s from epoch (epoch is on a half hour). Episodes with `secs == 0` are left out at
  `ChannelDef` build time (Task 3), a channel with no episodes returns `None` / empty. `now()` for t < epoch or t
  before 2020-01-01 returns `None`. Simulation from epoch is O(items since epoch); cache per channel the last
  (t, round, counters) and continue from it when t moves forward (a `Cursor` struct private to the module).

- [ ] **Step 1: Write the failing tests** in `sched.rs`:
  - `same_input_same_answer`: two calls of `now()` with the same def + t give equal slots.
  - `pads_to_half_hour`: with filler on, every `slot.start % 1800 == epoch % 1800` and `end - start` is a multiple of 1800.
  - `no_pad_back_to_back`: filler off: `slots[i+1].start == slots[i].end`, `end - start == secs`.
  - `offset_inside_slot`: t = slot.start + 125 → offset 125.
  - `rotates_shows`: 3 shows, first 3 slots of round 0 are 3 different shows.
  - `episodes_in_order`: a show's consecutive appearances play ep 0, 1, 2 … and wrap after the last.
  - `no_repeat_within_week`: one show with 400 eps of 1320 s, padded: no ep id repeats within 7 days of slots.
  - `empty_channel_none`, `now_before_2020_is_none`, `cursor_matches_fresh`: `now()` at t2 after t1 equals a fresh
    computation at t2.
- [ ] **Step 2:** `cd daemon && cargo test sched` → FAIL (module missing).
- [ ] **Step 3:** Implement `sched.rs` as above.
- [ ] **Step 4:** `cargo test sched` → PASS; `scripts/check.sh` → ok.
- [ ] **Step 5:** Commit `sljfd: channel scheduler (pure)`.

### Task 3: Config keys, channel collections and the daily snapshot

**Files:**
- Create: `daemon/src/chan.rs`
- Modify: `daemon/src/main.rs` (`Cfg` fields + parse + tests), `docs/CONFIG.md`, `examples/config.example.json`

**Interfaces:**
- Consumes: `sched::{ChannelDef, Show, Ep}`; `Api::get`.
- Produces:
  ```rust
  // Cfg: channels: bool (true), channel_prefix: String ("Channel"), show_server_live_tv: bool (false),
  //      schedule_day_start: i64 seconds after local midnight ("06:00" -> 21600; bad value -> 21600 + log),
  //      pad: Option<bool> (absent = auto), breaks: String ("between"; "off" also accepted; others -> "between" + log),
  //      break_minutes: u32 (2, clamp 0..=10), cache_gb: Option<u32> (absent = auto), fetch_rate: f64 (2.0, clamp 1..=8)
  pub fn parse_channel_name(name: &str, prefix: &str) -> Option<(u32, String)>  // "Channel 10 Classic Sitcoms" -> (10, "Classic Sitcoms")
  pub fn pool_name(name: &str) -> Option<(PoolKind, Option<u32>)>             // "Bumpers" / "Bumpers 14" / "Commercials" ...
  pub enum PoolKind { Bumpers, Commercials }
  pub struct ServerClip { pub id: String, pub secs: f64 }
  pub struct Snapshot { pub day: i64, pub channels: Vec<ChannelDef>, pub pools: Vec<(PoolKind, Option<u32>, Vec<ServerClip>)> }
  pub fn load_snapshot(api: &Api, cfg: &Cfg, day: i64) -> Result<Snapshot, String>
  pub fn snapshot_from_json(collections: &Value, children: &dyn Fn(&str) -> Value, episodes: &dyn Fn(&str) -> Value,
                            prefix: &str, day: i64) -> Snapshot                // pure core of load_snapshot, testable
  ```
- Jellyfin calls: collections `GET /Users/{uid}/Items?IncludeItemTypes=BoxSet&Recursive=true&Fields=` ; children
  `GET /Users/{uid}/Items?ParentId={cid}&Fields=Overview` ; a series' episodes
  `GET /Shows/{sid}/Episodes?UserId={uid}&Fields=Overview` (skip `ParentIndexNumber == 0`, sort by season, episode);
  a season child contributes its episodes (`SeasonId`), Movie/Video/Episode children are one-item shows. Episode
  title = `Name`, episode = `S{season}E{n}` or empty, secs = `RunTimeTicks / 10_000_000`.
  Duplicate channel numbers: the collection with the lower id wins, the other is logged. A channel's `filler` = any
  pool `Bumpers`/`Commercials` that is global or has its number.
- Persist: `/mnt/scratch/sljf/cache/channels.json` = the snapshot (overwritten at most once per schedule day);
  `load_snapshot` is used when the day changes or when no file for today exists.

- [ ] **Step 1: Write failing tests**: `channel_name_parses` (`"Channel 10 Classic Sitcoms"`, `"channel 7"` -> (7, "Channel 7"),
  `"Chan 3 X"` -> None, `"Channel x Y"` -> None), `pool_names` (`"Bumpers"`, `"Commercials 14"`, `"Bumpers14"` -> None),
  `snapshot_builds_channels` (fixture JSON: two collections, a series with seasons 0..2 → specials dropped, a movie,
  an episode with no RunTimeTicks dropped, duplicate number → one channel), `empty_collection_skipped`,
  `cfg_channel_defaults`, `cfg_channel_values_clamped` (break_minutes 99 -> 10, fetch_rate 0 -> 1, day start "25:00" -> 21600).
- [ ] **Step 2:** `cargo test chan cfg_channel` → FAIL.
- [ ] **Step 3:** Implement; document every key in `docs/CONFIG.md` (the spec's table, minus `channel_skip_reencode`
  and `channel_level_audio`, which are not in 1a) and add them to `examples/config.example.json`.
- [ ] **Step 4:** `cargo test` → PASS; `scripts/check.sh` → ok.
- [ ] **Step 5:** Commit `sljfd: channel collections, config keys, daily snapshot`.

### Task 4: Player channel mode, part 2: the item queue and seamless cuts (gate)

**Files:**
- Modify: `player/sljf-play.c`, `player/item.h`
- Create: `player/cutplan.c`, `player/cutplan.h`, `player/test/cutplan-test.c` (pure decisions, host-tested)
- Modify: `scripts/check.sh` (build + run `cutplan-test` like `live-buf-test`)

**Interfaces:**
- Consumes: `struct item` (Task 1).
- Produces: usage `sljf-play --channel` = like `--static` (static at once, first item from stdin), then a queue:
  - stdin `next v <limit_s> <start_s> <url>`: a video item (limit 0 = to its end; start_s = its offset, for logs)
  - stdin `next card <secs> <text…>`: silent static with the bar showing `<text>` for `secs`
  - stdin `url <url>` stays as the first item (seq 1); each `next` line gets seq 2, 3, … in arrival order
  - stdout `item <seq> start` (first frame of the item shown, or card begun), `item <seq> end` (handed over or done),
    `queue empty` (current item fully read and nothing queued; sljfd is late)
  - `cutplan.h`: `enum cut { CUT_SAME, CUT_RESET, CUT_CARD }; enum cut cut_kind(int cur_w, int cur_h, int nxt_w,
    int nxt_h, int nxt_is_card);` and `int item_should_open_next(double cur_play_left_s, int cur_rd_eof,
    int nxt_queued, int nxt_opened);` (open the next when the current reader hit its end or the current item has
    < 25 s left to play, i.e. its limit or end minus what has been fed, and it is queued and not yet opened;
    by time to the cut, not by unread seconds: final review R13)
- Cut rules: the next item is opened (its own reader thread, its own FIFO) as soon as `item_should_open_next` says
  so; when the current item's FIFO is empty after its reader's end (or its `limit_s` is reached: stop feeding it),
  - `CUT_SAME` (same width/height): keep feeding the same SLVideo stream with the next item's packets; reset
    `v_first`, `a_first`, `a_end` (they recompute from the next item's timestamps with the running frame count),
    new audio decoder + resampler for the next item, no sync measurement for 3 s after the cut;
  - `CUT_RESET`: wait until the video queue has played out, free and recreate the SLVideo stream (static silent
    meanwhile if the gap exceeds 300 ms);
  - `CUT_CARD`: show the static at volume 0 with `osd` text for `secs`, the SLVideo stream stays.
  Exit code rules unchanged; a stream error of the current item with a next item queued: log it, cut to the next
  (sljfd decides what that means for the schedule).

- [ ] **Step 1: Write failing host test** `player/test/cutplan-test.c`: `cut_kind(1920,1080,1920,1080,0)==CUT_SAME`,
  `cut_kind(1920,1080,1280,720,0)==CUT_RESET`, `cut_kind(1920,1080,0,0,1)==CUT_CARD`,
  `item_should_open_next(30,0,1,0)==0`, `(26,0,1,0)==0`, `(24,0,1,0)==1`, `(50,1,1,0)==1`, `(5,0,0,0)==0`, `(5,0,1,1)==0`.
- [ ] **Step 2:** Wire it into `scripts/check.sh` (same pattern as `live-buf-test`); `scripts/check.sh` → FAIL on it.
- [ ] **Step 3:** Implement `cutplan.c`, then the queue and cuts in `sljf-play.c` as above.
- [ ] **Step 4:** `scripts/check.sh` → ok; `SDK=/mnt/fast/steamlink-sdk scripts/build.sh` → builds, no warnings.
- [ ] **Step 5: Gate on the Link** (nobody watches the bench Link): copy `out/sljf-play` to `/tmp` on the Link, play two
  ~60 s pieces of two library episodes of the same resolution through `--channel` (URLs built like
  `tools/sljf-test --info` shows; headers via `SLJF_HEADERS` read from the Link's config over ssh), the second as a
  `next v 40 0 <url>` line, plus a third of a different resolution, plus a `next card 5 Up next` between.
  Pass = `item 2 start` with no `static on` between `item 1 end` and `item 2 start` for the same-resolution cut, a
  webcam frame (`~/Projects/home-network/tools/steamlink/look`) during item 2, no `stream error`, a/v fixes ≤ 2 after
  the cut. **If the same-resolution cut shows a gap or breaks**, stop and report: the rest of the plan then uses one
  player per item (Task 7 starts `sljf-play` per item with `--static`; the `next` protocol is not used).
- [ ] **Step 6:** Commit `player: --channel mode, item queue with seamless cuts`.

### Task 5: Filler (pure fill + stick clip probing)

**Files:**
- Create: `daemon/src/filler.rs`
- Modify: `player/sljf-play.c` (`--probe <file>`)

**Interfaces:**
- Consumes: `chan::{PoolKind, ServerClip}`, `sched::Slot`.
- Produces:
  ```rust
  pub enum ClipLoc { Stick(String /* absolute path */), Server(String /* item id */) }
  pub struct Clip { pub loc: ClipLoc, pub secs: f64 }
  pub enum Fill { Clip { clip: usize, limit: f64 }, Card { secs: f64 } }     // index into the pool slice used
  pub fn fill_gap(gap_secs: f64, commercials: &[Clip], bumpers: &[Clip], break_secs: f64, seed: u64) -> Vec<(bool /*commercial*/, Fill)>
  pub fn seed(number: u32, slot_start: i64) -> u64
  pub fn pick_pools(stick: &[(PoolKind, Option<u32>, Clip)], server: &[(PoolKind, Option<u32>, Vec<ServerClip>)],
                    number: u32) -> (Vec<Clip>, Vec<Clip>)  // (commercials, bumpers): stick clips for this channel or
                                                            // "all" if any, else the server pools for it / global
  pub fn probe_line(line: &str) -> Option<(f64, u32, u32, bool)>   // "probe ok 12.480 1280x720 h264 aac" -> (secs,w,h,ok)
  ```
  Fill: commercials (shuffled by seed, no repeat inside the gap) up to `break_secs`, then bumpers until the gap is
  used; a clip longer than what is left plays cut (`limit` = what is left) when ≥ 2 s are left; < 2 s left = nothing;
  no clips at all = one `Card { secs: gap }`.
- `sljf-play --probe <file>`: prints `probe ok <secs> <w>x<h> <vcodec> <acodec>` when video is h264 8-bit ≤ 1920x1088
  and audio is aac/mp3/ac3 (or absent), else `probe bad <why>`; exit 0. sljfd runs it for new stick files under
  `sljf/bumpers/{all,<N>}/` and `sljf/commercials/{all,<N>}/` (extensions mp4, mkv, ts, m4v) and keeps results in
  `sljf/clips.json` keyed by relative path + size + mtime; bad files are logged once.

- [ ] **Step 1: Failing tests**: `fill_exact_gap` (bumpers 10/20/30 s, gap 60 → total ≤ 60, ≥ 58), `fill_cuts_last`
  (one 30 s bumper, gap 45 → [30 full, 15 limited]), `fill_commercials_first` (break 30, commercials 15 s x3 → first
  two fills are commercials), `fill_no_clips_card`, `fill_tiny_gap_nothing` (gap 1.5 → empty), `pools_prefer_stick`,
  `pools_channel_specific_or_global`, `probe_line_parses` (+ `probe bad` → None).
- [ ] **Step 2:** `cargo test filler` → FAIL. **Step 3:** implement (+ `--probe` in the player).
- [ ] **Step 4:** `cargo test` + `scripts/check.sh` → ok; player builds.
- [ ] **Step 5:** Commit `sljfd: filler (bumpers, commercials) + sljf-play --probe`.

### Task 6: USB stick, fetcher and the `/cache` endpoint

**Files:**
- Create: `daemon/src/usb.rs`, `daemon/src/fetch.rs`
- Modify: `daemon/src/local.rs` (`GET /cache?f=NAME`), `player/sljf-play.c` + `player/live-buf.c/.h`
  (`SLJF_READAHEAD_S` env, default 12, max 120; a FIFO byte cap of 48 MB alongside `FIFO_MAX`)

**Interfaces:**
- Produces:
  ```rust
  // usb.rs
  pub const MOUNT: &str = "/mnt/sljf-usb";
  pub struct Stick { pub root: PathBuf /* MOUNT/sljf */, pub free_bytes: u64, pub fat: bool }
  pub fn poll(state: &mut Option<Stick>)        // every 5 s from tick(): /sys/block/sda appears -> mount
                                                // /dev/block/sda1 (vfat, then ext4/ext3/ext2, "-o noatime") at MOUNT;
                                                // keep only with a top-level sljf/ dir, else umount; gone -> umount -l
  pub fn cache_budget(free_bytes: u64, cache_gb: Option<u32>) -> u64   // auto = free/2, max 32 GiB
  pub fn evict(files: &mut Vec<(String, u64 /*size*/, u64 /*mtime*/)>, budget: u64, in_use: &[String]) -> Vec<String>
  // fetch.rs
  pub struct Job { pub name: String /* "<item id>-<start s>.ts" */, pub url: String, pub byte_rate_cap: u64 }
  pub struct Fetcher { /* one worker thread, a queue, the active job, a cancel flag */ }
  impl Fetcher { pub fn new(cache_dir: PathBuf, headers: String) -> Fetcher;
                 pub fn want(&self, jobs: Vec<Job>);       // replaces the queue (a flip drops the old channel's jobs)
                 pub fn state(&self, name: &str) -> FileState; }
  pub enum FileState { Missing, Growing, Complete, Failed }
  pub fn rate_cap(bitrate_bps: u64, fetch_rate: f64) -> u64     // bytes/s; unknown bitrate (0) -> 2 MB/s
  ```
  Files download to `NAME.part`, renamed to `NAME` when complete. Items over 3.9 GB on a FAT stick are not cached
  (played direct). Write errors: job `Failed`, cache marked off until the stick is re-inserted, logged.
- `/cache?f=NAME` (local port only, names `[A-Za-z0-9._-]+`): streams `NAME` or `NAME.part`, waiting (100 ms steps)
  while the fetcher says `Growing`, ends at `Complete`; 404 when missing/failed. No Range support.
- The player: `SLJF_READAHEAD_S` replaces the fixed 12 s in `livebuf_ahead`; sljfd sets 60 for channel items without
  a stick and 20 with one (the cache does the long buffering).

- [ ] **Step 1: Failing tests**: `budget_auto_half_capped`, `evict_oldest_not_in_use`, `rate_cap_values`
  (8 Mb/s x2 → 2_000_000), `cache_serve_growing_file` (temp dir: writer thread appends in 3 steps then marks
  complete; a reader through the serve function gets all bytes), `cache_serve_missing_file_404`, `cache_name_rejects`
  (`../x`, `a/b`). Player: extend `player/test/live-buf-test.c` for the env-driven ahead (12 default, clamp 120).
- [ ] **Step 2:** `cargo test usb fetch cache` → FAIL. **Step 3:** implement. (Mounting is checked on the Link in
  Task 9, not in host tests.)
- [ ] **Step 4:** `cargo test` + `scripts/check.sh` → ok.
- [ ] **Step 5:** Commit `sljfd: USB stick cache, fetch-ahead, /cache endpoint; player read-ahead setting`.

### Task 7: Channel playback in sljfd

**Files:**
- Create: `daemon/src/engine.rs` (pure-ish: what to queue next, guide/home JSON from the plan)
- Modify: `daemon/src/main.rs` (`Player`: `play_channel`, tick handling, flips, Home, reports), `daemon/src/local.rs`
  (`/home` channels, `/guide`, `/image?id=ch:N`, `/play?id=ch:N`)

**Interfaces:**
- Consumes: Tasks 2, 3, 5, 6 and the player protocol of Task 4.
- Produces:
  ```rust
  pub enum Next { Video { item_id: String, start_secs: f64, limit_secs: f64, slot_start: i64 }, Card { secs: f64, text: String } }
  pub fn upcoming(ch: &ChannelDef, r: &Rules, fill: &dyn Fn(&Slot) -> Vec<(bool, Fill)>, t: i64, n: usize) -> Vec<(i64 /*wall start*/, Next)>
      // the item on air at t (with its offset) then the following n entries: episode, its filler, next episode ...
  pub fn guide_json(snap: &Snapshot, r: &Rules, from: i64, until: i64, now: i64) -> Value   // same shape as guide::guide_json
  pub fn home_channels(snap: &Snapshot, r: &Rules, now: i64) -> Value                        // same shape as /home channels
  pub fn channel_id(number: u32) -> String   // "ch:<number>"
  ```
- Behaviour:
  - Channels active = `cfg.channels` and the snapshot has ≥ 1 channel. Then `/home` `channels` and `/guide` come from
    the engine (server Live TV hidden unless `show_server_live_tv`, then listed after), `/image?id=ch:N` serves the
    collection's Primary image, `/play?id=ch:N` tunes it. Otherwise everything is as today.
  - Tuning: take the screen as `play_live_static` does, start `sljf-play --channel` with `SLJF_STATIC`,
    `SLJF_READAHEAD_S`, `SLJF_PREBUFFER_MS=3000`, then `url` = the item on air (offset = now - slot start, as
    `StartTimeTicks`), then keep 2 entries queued ahead with `next` lines. Video URLs: with a stick and a cacheable
    item, `http://127.0.0.1:8097/cache?f=<job name>` (fetcher job queued first); else the normal `stream_url`
    (copy when `video_decodable`, re-encode otherwise; channels: no subtitles, no audio choice beyond `pick_audio`).
    Stick clips play by path; server clips like episodes.
  - Item lines: `item N start` updates the bar title/`shared.playing` and the queue; `item N end` → queue the next
    entry. Drift: when an item starts more than 5 s after its scheduled start, its `start_secs` is moved to its
    scheduled offset before sending (entries are sent ≤ 20 s before needed, so recompute then). Bumpers never seek.
  - Flips (Left/Right) go through the channel numbers in order; `stale_item_lines_ignored`: lines from a stopped
    player never touch the new one (a generation counter on the child).
  - Open failure / `stream error` of an item: log, skip it; the next entry plays at its scheduled time (static covers).
  - Home while watching a channel → guide on that channel (`shared.ended("guide")`, `shared.channel("ch:N")`).
  - No `report()` calls for channel items; the session still exists.
  - Daily: at `schedule_day_start` (local, via `localtime_r` declared `extern "C"`, fallback UTC) reload the
    snapshot; a changed timeline takes over at the next item boundary.
  - Idle with a stick: fetch the next item of the last-watched channel.
- [ ] **Step 1: Failing tests** in `engine.rs`: `upcoming_starts_on_air_with_offset`, `upcoming_includes_filler_between`,
  `guide_json_shape` (keys `now`, `channels[].{id,number,name,programs[].{id,name,episode,start,end,overview}}`,
  ids `ch:10`), `home_channels_shape`, `channel_ids_round_trip`, and in main.rs tests `stale_item_lines_ignored`,
  `open_failure_moves_on` (pure helpers over a small state struct, not a real child).
- [ ] **Step 2:** `cargo test engine` → FAIL. **Step 3:** implement.
- [ ] **Step 4:** `cargo test` + `scripts/check.sh` → ok; `SDK=/mnt/fast/steamlink-sdk scripts/build.sh` builds.
- [ ] **Step 5:** Commit `sljfd: channels built on the box (tune, queue, flips, guide)`.

### Task 8: Docs

**Files:** Modify `README.md` (a "Channels" section: how to make one, bumpers/commercials folders on a stick with the
`sljf/` opt-in, the clip format and a conversion command, YouTube via library downloaders), `docs/ARCHITECTURE.md`
(engine modules, player channel mode, `/cache`), `docs/integrations/lan-api.md` only if an endpoint is user-visible
(it is not: `/cache` is local), `CHANGELOG.md` `[Unreleased]`.

- [ ] **Step 1:** Write them (plain, no house details). **Step 2:** `scripts/check.sh` → ok. **Step 3:** Commit
  `docs: channels built on the box`.

### Task 9: On the Link (bench, webcam)

- [ ] **Step 1:** Make a test setup on the server **only if the owner agreed to it** (collections are visible to the
  server's users); otherwise use what exists. Needed: a `Channel 90 Test` collection with two short series, a
  `Bumpers` collection or stick clips. (Ask the desk/owner before creating anything on the server.)
- [ ] **Step 2:** `SLJF_UPSTREAM=<this worktree> ~/Projects/home-network/tools/steamlink/house-deploy --any-ref`.
- [ ] **Step 3:** Checks with logs (`/mnt/scratch/sljf/sljfd.log`, player lines) and webcam frames: tune → joins
  mid-item at the right offset (log the slot start, offset and the player's first `pos=`); an item boundary →
  bumper → next item, no static, `item N start` lines; a flip and back; Home → guide shows the channel and its
  programmes; stick: create `sljf/` on the stick (owner's opt-in given), mount + cache files appear and grow; pull
  test optional; no watched state on the server for the items played.
- [ ] **Step 4:** Note results in the PR body (no house details).
