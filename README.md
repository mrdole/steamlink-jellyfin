# steamlink-jellyfin

> **⚠️ EARLY ALPHA.** This works on one Steam Link against one Jellyfin server, set up by hand. Expect rough edges,
> breaking changes to config and layout, and steps that assume you are comfortable with ssh and cross-compiling.
> There are no prebuilt binaries yet. Current version: see [VERSION](VERSION) and [CHANGELOG.md](CHANGELOG.md). Please open an issue with what you tried and what happened.

A Jellyfin player for Valve's discontinued **Steam Link** box, using its own hardware video decoder. The box becomes
a small Jellyfin client at the TV: a TV-style app on its home screen, plus a "Play on" target for the Jellyfin apps
and Home Assistant.

Not affiliated with Valve or the Jellyfin project.

## What it does

- **On-TV app**: a "Jellyfin" tile on the Link's home screen. It opens on its own home screen (JELLYFIN, LIVE TV,
  STEAM). Continue Watching, Next Up, Live TV, Libraries, Search, audio and subtitle track choice. It returns to where
  you were after playback.
- **Playback** through the Link's H.264 decoder: H.264 8-bit up to 1080p is copied as is; anything else (HEVC,
  10-bit, 4K) is re-encoded by the Jellyfin server. Audio is always converted to stereo AAC by the server.
- **Subtitles**: text tracks drawn on the Link; picture tracks (PGS, VobSub) burned in by the server.
- **Remote control**: shows up in Jellyfin as a device you can cast to and control (play, pause, seek, stop, tracks,
  d-pad), which also makes it a `media_player` in Home Assistant's Jellyfin integration.
- **Live TV** channels from Jellyfin's Live TV (tested with ErsatzTV over an M3U tuner), with a TV guide, and TV
  static with a faint hiss while a channel tunes or rebuffers (both can be turned off).
- **Channels** built on the box from Jellyfin collections or playlists, scheduled from the clock, with bumpers and
  commercials from an optional USB stick: [Channels](#channels).
- **Recovery**: restarts playback where it was after network drops or server hiccups, reconnects after reboots.

## Integrations (all optional)

| Integration | Status | How to turn it on |
|---|---|---|
| Jellyfin apps ("Play on", remote) | Works | On by default |
| Home Assistant (media player, d-pad) | Works | Stock Jellyfin integration in HA: [docs](docs/integrations/home-assistant.md) |
| LAN API (send search text, e.g. from HA) | Works | `lan_allow` in config: [docs](docs/integrations/lan-api.md) |
| USB / Bluetooth keyboard | In the app: coded, untested | Plug in: [docs](docs/integrations/input.md) |
| Game controller (USB / Bluetooth) | In the app: coded, untested | Pair in the Link's menu: [docs](docs/integrations/input.md) |
| Keyboard / controller during playback | Not yet | [Roadmap](ROADMAP.md) |
| Mouse | Not yet | [Roadmap](ROADMAP.md) |

## Channels

The Link can run its own TV channels from your Jellyfin library, with no ErsatzTV or other server needed. A channel
is a list of shows that rotate marathon-style: each show's episodes in order, season 0 (specials) left out. What is
on comes from the clock, so you join mid-episode, and every box with the same channel shows the same thing.

**Make a channel.** Name a Jellyfin collection or one of your own playlists `Channel <N> <name>`, for example
`Channel 10 Classic Sitcoms`, and put whole series or films in it. Non-admin Jellyfin users usually cannot create
collections (the server answers 403), so a playlist is the easy way: make a playlist, add series or films to it.
Channels show in the TV guide and on the home screen. The prefix (`Channel`) is a setting, see
[CONFIG.md](docs/CONFIG.md#tv-channels), which lists every channel key.

**Padding.** Slots are padded to :00 / :30 with bumpers and commercials when the server has a collection or playlist
named `Bumpers` or `Commercials`. With clips only on the USB stick, set `"pad": true`. When nothing fits a gap, a
silent static card shows "Up next: <title> at <time>".

**USB stick (optional).** The stick holds bumper and commercial clips, and a cache so upcoming programmes start
without waiting on the network. Use FAT32 or ext2/3/4, not exFAT. The Link only uses a stick that has a top-level
`sljf/` folder; others are left alone. sljfd mounts it at `/mnt/sljf-usb`.

```
sljf/bumpers/all/          used on every channel
sljf/bumpers/10/           only on channel 10
sljf/commercials/all/
sljf/commercials/10/
sljf/cache/                made and managed by sljfd
```

Clips must be H.264 8-bit up to 1080p with AAC, MP3 or AC3 sound, in mp4, mkv, ts or m4v. Convert anything else with:

```bash
ffmpeg -i in.mkv -c:v libx264 -profile:v high -pix_fmt yuv420p \
  -vf "scale='min(1920,iw)':-2" -c:a aac -ac 2 out.mp4
```

(Keep the picture within 1920x1080; a clip the Link cannot play is skipped and logged once.)

**YouTube and others.** Only through your library: let a downloader such as Pinchflat, ytdl-sub or Tube Archivist
put videos into Jellyfin, then add them to a channel. Netflix and the like are DRM and cannot work.

**Not in this release:** commercial breaks inside episodes, loudness matching, subtitles on channels, a fallback
clock from the server's Date header, and skipping the re-encode of files the Link already plays.

## What you need

- A Valve Steam Link with SSH enabled (USB stick method, see [INSTALL](docs/INSTALL.md)), wired network recommended.
- A Jellyfin server reachable over HTTPS or HTTP, and an account for the Link.
- A Linux x86-64 machine to build on: Valve's [steamlink-sdk](https://github.com/ValveSoftware/steamlink-sdk)
  (~4.7 GB), rustup, Python 3, ImageMagick, fontconfig. No root needed.

## Quick start

```bash
git clone --depth 1 https://github.com/ValveSoftware/steamlink-sdk ~/steamlink-sdk
rustup target add armv7-unknown-linux-musleabihf
scripts/build.sh                                         # player + daemon -> out/
scripts/build-app.sh                                     # on-TV app      -> out/app/jellyfin/
tools/sljf-login --server https://jellyfin.example.com   # once: token goes straight to the Link
scripts/deploy.sh                                        # copy to the Link and start
```

Full steps: [docs/INSTALL.md](docs/INSTALL.md). Settings: [docs/CONFIG.md](docs/CONFIG.md).
How it works: [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md). Hacking on it: [docs/DEVELOPING.md](docs/DEVELOPING.md).
Versions and releases: [CHANGELOG.md](CHANGELOG.md), [docs/RELEASING.md](docs/RELEASING.md).

## Known limits

- One program can use the Link's decoder at a time; pause = the player is stopped and restarted (about 3 s to
  picture on resume).
- No seeking in Live TV or on a channel.
- Only stereo audio; no surround passthrough.
- Single-core 1 GHz CPU and ~256 MB RAM: the server does the heavy lifting.

## Licence

GPL-3.0-or-later, see [LICENSE](LICENSE). Third-party components and their licences: [NOTICE.md](NOTICE.md).
The Steam Link SDK is not included; you clone it from Valve.
