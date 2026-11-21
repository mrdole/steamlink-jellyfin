# How it works

## The box

Valve Steam Link: single-core ARMv7 at 1 GHz, ~256 MB RAM, Linux 3.8, a Marvell media pipeline exposed by Valve's SDK
as `SLVideo` (H.264 only, up to 1080p, the decoder displays frames itself) and `SLAudio` (16-bit PCM only). The SDK
ships Qt 5.14, SDL2 and an old toolchain; it has no FFmpeg. The system OpenSSL (1.0.1c) and CA list (2013) are too
old for modern HTTPS. `/mnt/scratch` and `/home` survive reboots; `/etc` is an overlay that does too.

## Pieces

| Piece | Source | On the Link | Role |
|---|---|---|---|
| `sljfd` | `daemon/` (Rust, static musl) | `/mnt/scratch/sljf/sljfd` | Jellyfin session (websocket + REST), queue, track choice, starts/stops the player, local API for the app, optional LAN API |
| `sljf-play` | `player/` (C) | `/mnt/scratch/sljf/sljf-play` | Reads the stream with a static minimal FFmpeg + mbedTLS, feeds H.264 to SLVideo and decoded PCM to SLAudio, A/V sync, subtitles and on-screen bar on an SLVideo overlay |
| `sljf-ui` + `main.qml` | `app/` (Qt Quick) | `/home/apps/jellyfin/` | The on-TV app (the "Jellyfin" tile); talks only to sljfd on 127.0.0.1:8097 |
| `S98sljfd` | `daemon/sljfd.init` | `/etc/init.d/startup/` | Boot hook; restarts the daemon if it dies |

## Playback

- sljfd asks Jellyfin for `/Videos/<id>/stream.ts`: **video copied** when it is H.264 8-bit <= 1080p, otherwise
  re-encoded to H.264 by the server; **audio always converted** to stereo AAC 48 kHz (most libraries have AC3/E-AC3
  5.1, and decoding that would eat the single core).
- sljf-play keeps about 1 s queued in each output and prints one status line a second (`pos=`, queue levels, a-v).
- **Live TV read-ahead**: with `SLJF_PREBUFFER_MS` (sljfd sets 3000 for Live TV) a reader thread keeps that much
  (+1 s) read ahead in a FIFO and the main loop only feeds the decoders from it, so a network read that waits (a live
  stream at its edge, a server sending in bursts) never holds up feeding. Status lines then add `ahead` (FIFO span,
  per stream `v`/`a`, packets) and `read max` (longest read wait in that second). Files read inline.
- **A/V sync**: SLVideo and SLAudio take no timestamps. Once a second the player compares where each output is (pts
  of the last submitted unit minus what is still queued) and inserts silence or drops samples. `av_offset_ms` adds a
  per-TV offset. The frame rate is re-measured from packet timestamps because the probed rate can be wrong.
- **Live TV tuning and static**: when a channel is chosen (OK in the app or the guide, a Left/Right flip, a reconnect
  after a drop) sljfd takes the screen at once and starts `sljf-play --static` (snow + hiss at `static_volume`), then
  asks Jellyfin to open the channel (PlaybackInfo, up to ~15 s while the tuner starts it) in a background thread. When
  that returns it writes `url <URL>` to the waiting player's stdin, and the static stays until the first picture. A
  flip or stop during the wait abandons the tune and closes its server stream when the request returns; a run of
  flips waits 600 ms after the last press before tuning. With `SLJF_STATIC` set the player also shows static when the
  video queue has been empty for 700 ms mid-stream. `live_tv_static: false` keeps the old blocking flow.
- **Volume**: software gain on the decoded 16-bit samples (one multiply and shift each, squared 0-100 curve) before
  SLAudio, since the Link always outputs full scale. `SLJF_VOLUME` sets it at start, a `volume N` line on stdin
  changes it live (heard within the ~1 s queued). sljfd keeps the level (config `volume`, then Jellyfin's
  `SetVolume` / `VolumeUp` / `VolumeDown` / `Mute` / `Unmute` / `ToggleMute`) and reports `VolumeLevel` / `IsMuted`.
- **Pause** stops the player and restarts it at the same position (starving the decoder is not a pause: its clock keeps
  running). While paused, `sljf-play --card` shows the bar with PAUSED.
- **Subtitles**: text tracks are fetched as SRT in the background and drawn by sljf-play; picture tracks are burned
  in by the server. The overlay is one 1280x260 ARGB strip shared with the on-screen bar.
- **Recovery**: a player that dies or goes silent is restarted at its position with backoff (2 s .. 60 s, ~5 min in
  all); the websocket has its own timeouts and reconnects; right after boot TLS waits for NTP.
- **Screen**: the Link's menu is parked (via `/tmp/launch_cmdline.txt`) only while something is loaded, and the
  power manager is kept from sleeping during playback.

## Local API (127.0.0.1:8097, for the app)

`/home` (rows), `/events` (state for the app, including `after` = "home" / "guide" / "" and `channel`: why playback
ended and which channel was on), `/play`, `/key`, and `/guide?hours=N` (1..6, default 3): Live TV channels in
number order with their programmes from Jellyfin's `/LiveTv/Programs`, times in epoch seconds; 502 when Jellyfin
cannot be reached.

## TLS

The player links FFmpeg against mbedTLS (TLS 1.2) with `/dev/urandom` as entropy source: kernel 3.8 has no
`getrandom()`, and mbedTLS's default `/dev/random` blocks for minutes on the Link. The daemon uses rustls with
built-in roots. `out/ca.pem` (the build machine's CA bundle) is copied to the Link for the player.

## The on-TV app

Qt Quick UI in `main.qml` with three screens: the home screen (tiles JELLYFIN, LIVE TV, STEAM), the Jellyfin home
(Continue Watching, Next Up, Live TV, Libraries, Search with an on-screen keyboard, a play dialog with audio/subtitle
choice) and the TV guide (`Guide.qml`, layout math in `guide.js`). STEAM quits the app and the Link's menu comes back.
When playback starts it saves where you were to `/tmp/sljf-ui.state` and exits with code 42 (the player needs the
screen); `run.sh` starts it again with `--restore` when playback ends. On restore the app reads `/events`: `after` =
"home" (Home during a film) opens the home screen, "guide" (Home during Live TV) the guide on `channel`, anything else
where you were.

## Prior art

moonlight-qt's Steam Link build (SLVideo/SLAudio usage) and garbear/kodi-steamlink were the references for the SDK.
