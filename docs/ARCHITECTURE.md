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
- **Pause** stops the player and restarts it at the same position (starving the decoder is not a pause: its clock keeps
  running). While paused, `sljf-play --card` shows the bar with PAUSED.
- **Subtitles**: text tracks are fetched as SRT in the background and drawn by sljf-play; picture tracks are burned
  in by the server. The overlay is one 1280x260 ARGB strip shared with the on-screen bar.
- **Recovery**: a player that dies or goes silent is restarted at its position with backoff (2 s .. 60 s, ~5 min in
  all); the websocket has its own timeouts and reconnects; right after boot TLS waits for NTP.
- **Screen**: the Link's menu is parked (via `/tmp/launch_cmdline.txt`) only while something is loaded, and the
  power manager is kept from sleeping during playback.

## TLS

The player links FFmpeg against mbedTLS (TLS 1.2) with `/dev/urandom` as entropy source: kernel 3.8 has no
`getrandom()`, and mbedTLS's default `/dev/random` blocks for minutes on the Link. The daemon uses rustls with
built-in roots. `out/ca.pem` (the build machine's CA bundle) is copied to the Link for the player.

## The on-TV app

Qt Quick UI in `main.qml`: Continue Watching, Next Up, Live TV, Libraries, Search with an on-screen keyboard, a play
dialog with audio/subtitle choice. When playback starts it saves where you were to `/tmp/sljf-ui.state` and exits
with code 42 (the player needs the screen); `run.sh` starts it again with `--restore` when playback ends.

## Prior art

moonlight-qt's Steam Link build (SLVideo/SLAudio usage) and garbear/kodi-steamlink were the references for the SDK.
