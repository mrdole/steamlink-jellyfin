# TV channels built on the box (channel engine): design

Status: approved by the owner, 2026-11-21 (stage 1 split into releases 1a and 1b). Scope: the public repo. Supersedes nothing; Live TV through
Jellyfin's tuner (the 2026-11-20 design) keeps working and stays the fallback.

## Why

Live TV on the Link today is a server-side channel (e.g. ErsatzTV) re-encoded live, served through Jellyfin Live TV
and pulled over the internet. A live stream cannot read ahead, so every dip in the server's upload reaches the screen
as static. Measured on the owner's setup: the channel encoder keeps real time, but the path out of the server's
network runs 0.75-1.05x for minutes; with a 12 s read-ahead (0.1.0-alpha.6) an 83 s server stall still showed 11 statics
in 10 minutes. The same episode played as a normal Jellyfin file barely lags: smaller, video copied, read-ahead.

So the box builds the channel itself: it works out what is on "now", plays that library file from the right offset,
then a bumper, then the next item. And because that needs nothing but a media server login, it turns any Steam Link
lying in a drawer into a little TV station from media people already host. The owner's words: "give this life to
people who have Steam Links lying around to make their own little box using home sources they already have".

## Goals

1. **Channels from the library, no server setup**: a collection named `Channel <number> <name>` on the media server is a
   channel. Its shows rotate marathon-style (each show's episodes in order), films play whole.
2. **Real TV feel**: what is on is decided by the clock. Tuning in joins mid-episode; every box in the house agrees on
   what is on; the TV guide shows it. Guide, Left/Right flipping, Home -> guide, and static while tuning stay as the
   user sees them today (2026-11-20 design).
3. **Seamless cuts**: within a channel, episode -> bumper -> episode with no static and no black gap in the usual case.
4. **Upload dips don't reach the screen**: the box knows the schedule, so it fetches the next items ahead (to a USB stick
   when there is one) and plays from that.
5. **Host load no heavier than binge-watching**: one ordinary direct-play (video copied) stream per box at a time
   (a few seconds of overlap at a cut, while the next item opens, is fine; so is the capped fetch-ahead), no
   server-side channel process, no transcoding for files the box can decode.
6. **Bumpers and commercials**, ideally local (USB stick), which also gives the host a break and the box time to fetch.
7. **Pluggable**: schedule sources (built-in, later "follow an ErsatzTV"), media sources (Jellyfin first, Plex later,
   several at once later), output backends (the Link first, later mpv on a Raspberry Pi / Linux PC).

## Non-goals

- Netflix, Disney+, Hulu and the like (DRM, no API, terms). YouTube only **via the library**: tools such as Pinchflat,
  ytdl-sub or Tube Archivist download playlists into a folder the media server scans; the README points at them.
  No direct YouTube playback.
- Streaming channels to other devices (Roku, phones, web): those keep using the server's Live TV (e.g. ErsatzTV) if the
  owner wants. The box is a player, not a channel server.
- An on-TV channel editor or a phone setup page (later, its own design).
- Recording, "start over", pausing a channel (it is live TV; pause may come later).

## Anti-goals (a failure even if it technically works)

- Marking episodes watched, adding resume points or changing Next Up / Continue Watching on the server by channel
  surfing.
- Static or black between items of a channel in the usual case (only on real stalls, flips, or a cut the player cannot
  make smoothly).
- Two boxes in the house showing different things on the same channel at the same time (beyond a few seconds).
- Writing a rolling video cache to the Link's internal flash (small NAND that holds the system; wear).
- Any server URL, token, house IP or the owner's lineup in the public repo.
- Making Live TV worse for anyone who does not use channels: no collections named `Channel ...` = today's behaviour.

## Constraints

- Steam Link: 1 GHz single-core ARMv7, ~256 MB RAM (~100 MB free, ~215 MB incl. page cache while idle), H.264 8-bit up
  to 1080p in hardware only (SLVideo), one program owns SLVideo/SLAudio at a time. Kernel 3.8; clock set by NTP after
  boot (1970 until then). Internal flash: `/mnt/scratch` 1.9 GB yaffs2 (sljfd and its image cache live there).
- USB stick on the Link (checked 2026-11-21 with a 62 GB PNY): seen as `/dev/block/sda1`; the kernel reads **vfat (FAT32),
  ext2/3/4, ntfs**, not exFAT; the firmware mounts USB only at boot (`/etc/init.d/startup/S01config`, looking for
  `steamlink/`) and unmounts it again, so sljfd mounts the stick itself. FAT32: files at most 4 GB.
- Normal playback today: `GET /Videos/{id}/stream.ts?...&AllowVideoStreamCopy=true&AllowAudioStreamCopy=false&StartTimeTicks=T`
  = Jellyfin remuxes to MPEG-TS, video copied, audio made AAC stereo (the Link player's audio path), starting at the
  keyframe before T. Items the Link cannot decode (HEVC, 10-bit, >1080p) or image subtitles to burn in are re-encoded
  by the server (`video_decodable`, `subtitle_tracks` in `daemon/src/main.rs`).
- `sljf-play` already takes control lines on stdin (`url ...` after `--static`, `osd ...`, `volume N`) and reports
  progress on stdout; sljfd restarts it per item today.

## How it works today (what this touches)

- `daemon/src/main.rs` `Player`: `play_current` (files), `play_live` / `play_live_static` / `start_tune` / `finish_tune`
  (Live TV through Jellyfin's tuner), `next()` (flips), `restart_at` / `retry_later` (recovery), `report` (playback
  reports to the server).
- `daemon/src/local.rs`: `GET /home` (`channels` = Jellyfin Live TV channels) and `GET /guide` built by
  `daemon/src/guide.rs` from `/LiveTv/Programs`. The app (`app/main.qml`, `Guide.qml`, `guide.js`) draws the home
  screen, LIVE TV row and guide from those.
- `player/sljf-play.c`: one stream per run; static (`sljf-static.c`) while tuning and when the picture stops.

## Approaches considered

### Where the schedule comes from
- **Follow the server's Live TV guide** (ErsatzTV's XMLTV via Jellyfin), matching each programme to a library item.
  First proposal; dropped when the owner wanted it for everyone, not only people running ErsatzTV.
- **Built-in scheduler from collections (picked, stage 1)**: needs only the box and a media server.
- **"Follow an ErsatzTV" as an optional schedule source (picked, stage 2)**: for people who already run ErsatzTV for
  its scheduler (the owner: in a container at home). ErsatzTV keeps the schedule only and never streams to the box.
- ErsatzTV (or anything) on a home box as the **streamer**: rejected. It reads each file at ~real time with a few
  seconds of buffer, so the server's upload dips reach the screen again, and it needs an always-on PC.

### How the player gets from one item to the next
- **A. A new player per item**: least new code (how films and flips work now), but 1-3 s of static at every cut while
  the next file opens on the server.
- **B. One player per channel, fed the next item ahead (picked)**: sljfd opens/fetches item N+1 while N plays and hands
  the player its location; the player reads it ahead and cuts at the end of N. Seamless cuts, instant local bumpers.
  New work: switching streams mid-run (resolution / frame-rate changes). Falls back to A for a cut it cannot make
  smoothly (static covers it), so the worst case is today's experience.
- **C. Stitch a stream on a helper box**: rejected (needs a PC; that is ErsatzTV again).

## Design

### Pieces

```
            schedule sources            media sources                 output backends
        built-in (collections) ─┐   Jellyfin (stage 1) ─┐        sljf-play on the Link (stage 1)
   follow ErsatzTV (stage 2) ───┤   USB stick folders ──┤        mpv on Pi / Linux (stage 5)
                                ▼   Plex, several (3-4)─┤                    ▲
                        ┌──────────── channel engine (sljfd, portable Rust) ──┴──────┐
                        │ channels · schedule (pure function of clock + snapshot)    │
                        │ fetch-ahead cache · source picking · guide/home JSON       │
                        └────────────────────────────────────────────────────────────┘
```

The channel engine is a set of modules in sljfd with no Link-specific code (no SLVideo, no Link paths hard-coded),
behind three small interfaces: schedule source, media source, output backend. Stage 1 builds one of each.

### Channels (built-in schedule source)

- A channel = a collection (Jellyfin BoxSet; Plex collection later) whose name matches `Channel <number> <name>`
  (case-insensitive; prefix configurable, `channel_prefix`). Number = the channel number in the guide; name = its name;
  the collection's image = its logo (else a text logo). Channels sort by number; duplicate numbers: lower item id wins,
  the other is logged and skipped.
- Contents: a series in the collection contributes all its episodes (aired order: season, episode; specials (season 0)
  left out); a season contributes its episodes; a film or video plays whole. Episodes of a series added one by one
  count as that series' episodes only.
- Rotation (marathon, like ErsatzTV's `marathon` with `shuffle_groups`): one item from each show in turn, each show
  continuing its own episode order; films in the same collection are treated as shows of one item each. The order
  of shows in a round is shuffled per round with a seeded PRNG (seed: channel number + round).
- Bumpers after each item and padding (below). Breaks for commercials (below).
- Optional `channels.json` (stage 4) adds what collections cannot say: time blocks ("these shows until 4 PM"),
  per-channel bumper folder and break style, pad on/off. Same file on the stick or in sljfd's config folder.

### What is on now: the schedule

- **A pure function** of (channel contents snapshot, date, time): no shared state between boxes, nothing that must be
  saved for correctness. Two boxes with the same snapshot compute the same thing.
- **Schedule day**: from 06:00 local to 06:00 (configurable `schedule_day_start`). At the start of a schedule day (or
  first need), sljfd snapshots each channel's contents (sorted item ids + runtimes) and builds that day's plan; the plan
  and its snapshot are kept in memory and in sljfd's cache folder (rebuilt if lost). Library changes take effect at the
  next schedule day, so the schedule does not jump mid-day. (Two boxes that snapshot at different times on a day the
  library changed can disagree until the next day: accepted, logged.)
- **Progress across days**: a show's episode position on day d continues from day d-1 (computed, not stored: the plan
  for day d starts each show where a deterministic run from a fixed epoch would have it; the builder picks the cheapest
  exact-enough method, e.g. a per-show daily episode count from the day's slot count). Requirement: an episode does
  not repeat within about a week when the show has enough episodes, and day d's first item follows day d-1's last.
- **What the timeline depends on**: only the server's data (channel collections, the `Bumpers` / `Commercials`
  collections' existence) and the box's config. Never on the USB stick: a box with a stick and a box without one, with
  the same config, show the same items at the same times. Stick clips only fill filler slots that are already fixed.
  Boxes with different `breaks` / `break_minutes` / `schedule_day_start` / `pad` settings may disagree (documented).
- **Padding**: when `pad` is on (default: on when the server has a `Bumpers` or `Commercials` collection; set
  `pad: true` to pad with stick clips only), each item is followed by filler up to the next :00 or :30
  (like the owner's ErsatzTV `pad_to_next 30`): short clips first, the last gap filled by one clip cut short, or a
  moment of the channel card when nothing fits. A box with no clips at all for a slot shows the channel card ("Up next:
  <title> at <time>") with the static's hiss off. With `pad` off, items run back to back and start at odd times.
  Items longer than 30 min pad to the next half hour after they end.
- **Time base**: the box's clock (NTP). If the clock is before 2020, or differs by more than 30 s from the media
  server's HTTP `Date` header, sljfd uses the server's time (offset kept) and logs it.
- **Joining mid-item**: start offset = now - item start. Jellyfin starts at the keyframe before it.
- **Drift**: the schedule is the truth. If an item ends early (metadata runtime longer than the file), the filler covers
  it; if it runs late (stalls), the next cut starts the next item at its scheduled offset when it is more than 5 s
  behind, otherwise from its start (filler absorbs small drift). Bumpers are never seeked into.

### Bumpers and commercials

- Pools, in this order: the USB stick `sljf/bumpers/<channel number>/`, `sljf/bumpers/all/`; then a collection
  `Bumpers` (and `Bumpers <channel number>`) on the media server. Commercials the same with `commercials`.
- Choice: random without repeats until the pool is used up (seeded per channel and day). The filler slot lengths are
  fixed by the schedule; which clips fill them may differ between boxes with different sticks.
- Stick clips must be H.264 8-bit up to 1080p with AAC, MP3 or AC3 audio in MP4, MKV or MPEG-TS. sljfd probes each new
  clip once on the box (length, codecs) with the player's own demuxer (e.g. `sljf-play --probe`) and remembers it in
  `sljf/clips.json` on the stick; other files are skipped and logged. The README gives an ffmpeg command to convert.
- **Breaks** (`breaks` setting, global default, per channel in `channels.json`): `between` (default: only after each
  item), `chapters` (at the file's chapter marks, falling back to `between` for files without chapters),
  `every N` (every N minutes, at the nearest keyframe). Break length `break_minutes` (default 2). A break inside an
  episode = cut away, commercials, come back where it left off (the schedule includes the break, so times stay true).
- Local clips (stick) start at once and give the fetcher time; server clips are fetched ahead like episodes.

### Fetch-ahead and the cache

- sljfd knows the next items of the channel being watched, so it fetches them ahead:
  - **With a USB stick** (FAT32 or ext2/3/4): sljfd mounts the first USB partition at `/mnt/sljf-usb` (when it appears;
    polls `/sys/block`) and uses it **only if it has a top-level `sljf/` folder** (the owner's opt-in; a stick without
    one is unmounted and left alone; a stick that also holds the firmware's `steamlink/` folder is fine). It uses that
    folder and keeps a rolling cache in `sljf/cache/` capped at
    `cache_gb` (default: half the free space, at most 32 GB). It downloads the **same `stream.ts` the player would play**
    (video copied, audio AAC, from the item's scheduled start) to a file, and the player plays that file while it grows.
    Target: the rest of the current item plus the next two items. Fetch speed capped at `fetch_rate` x real time
    (default 2x) to be polite to the host. Oldest out first; bumpers/commercials folders are never touched. Files over
    4 GB on FAT32: split into parts or stream that item without the cache (builder's call).
  - **Without a stick**: RAM read-ahead raised from 12 s to as much as fits safely (target 40-60 MB, about a minute at
    6-7 Mb/s), bounded by free memory measured at start.
- Before the TV is on: the next item of the last-watched channel is fetched ahead in the background (only with a
  stick), so tuning in to it starts at once. A cold channel streams directly with read-ahead until the cache catches up.
- Never on internal flash.
- Stick pulled out mid-play: the current item continues from the server stream at its scheduled offset (static
  covers the gap), the cache is dropped, nothing else changes. Stick full or read-only: no cache, logged.

### Media sources

- Interface: list collections; list a collection's playable items (with runtime, provider ids TVDB/TMDb/IMDb, chapters,
  video codec/size, subtitle tracks); a stream location for item + offset; whether the box can play it without
  re-encoding.
- **Jellyfin** (stage 1): the existing `Api` and `stream.ts` URLs. **USB stick folders** (stage 1, bumpers/commercials
  only). **Plex** (stage 4). **Several sources at once** (stage 3): collections with the same name merge into one channel;
  the same item on two sources (same provider ids + season/episode, or same file name and runtime for clips) plays
  from the faster one: sljfd keeps a running throughput per source from real fetches (plus a short probe at start) and
  prefers, in order, the stick, then any source holding above 2x real time, then the fastest.

### Playback (output backend: sljf-play on the Link)

- One `sljf-play` run per channel visit (approach B). New stdin lines (names are the builder's): `next <location>
  <offset>` = the item to cut to when the current one ends; the player opens it and starts reading it ahead before the
  cut. `cut` = cut now (a scheduled break or drift correction). The player reports `ended`/progress as today.
- A cut between two streams with the same resolution/frame rate: no static, no black. Different ones: the player
  re-creates its video stream; if that shows a gap, static covers it (no black screen).
- Items the box cannot decode are played anyway (the server re-encodes, as for films; logged), unless
  `channel_skip_reencode: true` leaves them out of channels. Subtitles follow the user's existing subtitle preference
  for text subtitles (drawn by the box); image subtitles are off on channels (no burn-in on channels).
- Loudness: no server-side loudnorm any more. The player gets a light, cheap level matcher for channel playback
  (`channel_level_audio`, default on) so bumpers and episodes from different sources sit at similar loudness; the
  user's volume applies on top.
- Server reports: channel playback sends no progress reports for the items it plays (no watched state, no resume
  points). The box still shows as a device on the server.

### Guide, home screen, flipping (app unchanged in look)

- When any channel exists, `/home` `channels` and `/guide` list the box's channels (ids `ch:<number>`) in the same JSON
  shape as today, built from the schedule (each item one programme block; filler folded into the item before it;
  breaks inside an item not shown). Programme title/episode/overview come from the item.
- The server's own Live TV channels are hidden then unless `show_server_live_tv: true` (both listed, server ones after).
  No channels at all: today's Live TV, unchanged.
- Tuning, Left/Right flips, Home -> guide on the channel being watched, static while the first item opens: as in the
  2026-11-20 design. Within a channel there is no tuning: cuts are seamless.

### Config (config.json, docs/CONFIG.md, examples/config.example.json)

| Key | Default | What |
|---|---|---|
| `channels` | `true` | Build channels from `Channel ...` collections (false = today's Live TV only) |
| `channel_prefix` | `"Channel"` | Collection name prefix that makes a channel |
| `show_server_live_tv` | `false` | Also list the server's Live TV channels when the box has its own |
| `schedule_day_start` | `"06:00"` | When a schedule day starts (library changes apply from then) |
| `pad` | auto | Pad each item to the next :00/:30 with filler (auto = on when the server has a `Bumpers` / `Commercials` collection) |
| `breaks` | `"between"` | `between`, `chapters` or `every N` |
| `break_minutes` | `2` | Length of a commercial break |
| `cache_gb` | auto | Rolling cache size on the USB stick |
| `fetch_rate` | `2` | Fetch-ahead speed cap, x real time |
| `channel_skip_reencode` | `false` | Leave items the box cannot decode out of channels |
| `channel_level_audio` | `true` | Even out loudness between items |

## Stages (each its own plan)

1. **Built-in channels on the Link, Jellyfin**: collections -> channels, schedule, guide/home, one player per channel
   with seamless cuts, bumpers/commercials from the stick or server collections, `breaks`, stick mount + fetch-ahead
   cache, RAM fallback, level matcher. Start with a throwaway test on the Link of a cut between two files (same and
   different resolution/frame rate) before building the rest. This test is a gate for approach B only: if even a
   same-resolution cut cannot be made without a gap, stage 1 goes ahead with approach A (a new player per item, static
   at each cut) and seamless cuts become their own later design.
2. **"Follow an ErsatzTV" schedule source**: reads an ErsatzTV's schedule on the LAN (its XMLTV or API: to be checked)
   and maps each item to a media source item (ErsatzTV syncs from Jellyfin/Plex and keeps their ids); its filler,
   mid-rolls and pads come through as items. To check then: how ErsatzTV lists filler and mid-roll breaks.
3. **Several media sources** with fastest-copy picking (e.g. a small Jellyfin on the LAN for commercials and copies of
   favourite shows, plus a remote one).
4. **Plex**, and `channels.json` time blocks for people without ErsatzTV.
5. **mpv backend** for a Raspberry Pi / Linux PC (bigger cache on disk; same engine and app).

Stage 1 ships as two releases (owner, 2026-11-21):
- **1a**: channels from collections, schedule, guide/home, seamless cuts (or approach A if the gate test fails),
  bumpers/commercials from the stick or server collections with `breaks: between` only, stick mount + fetch-ahead cache,
  RAM fallback.
- **1b**: `breaks: chapters` and `every N` (breaks inside episodes), and the loudness level matcher.

## Left to the builder

- The exact cross-day progress method, PRNG and seeds (within the requirements above).
- Stdin line names and the player's internal design for read-ahead of the next item and the cut.
- Cache file layout, FAT32 >4 GB handling, how the player reads a growing file.
- The level matcher's method (cheap on one 1 GHz core).
- Guide details for filler and breaks beyond "folded into the item before".
- How the stick is detected (poll vs hotplug) and unmounted cleanly on shutdown.

## Testing

- Host: `scripts/check.sh` plus unit tests for the scheduler (same snapshot + time -> same item and offset; day
  boundaries; padding sums to :00/:30; no repeat within a week for a long show; drift rules) and channel-name parsing.
- Link (camera on the TV, logs): two Links on the same channel show the same thing; joining mid-episode at the right
  point; cuts episode -> bumper -> episode without static (same and different resolution); a flip; Home -> guide;
  breaks `between` / `chapters` / `every 8`; with the stick: a 10-minute server stall (test relay) shows no static once
  the cache is ahead; without the stick: behaviour no worse than alpha.6; no watched state or resume points appear on
  the server after an hour of channels; host side: one stream at a time, no transcode for decodable items.
