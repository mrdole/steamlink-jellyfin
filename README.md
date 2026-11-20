# steamlink-jellyfin

> **⚠️ EARLY ALPHA.** This works on one Steam Link against one Jellyfin server, set up by hand. Expect rough edges,
> breaking changes to config and layout, and steps that assume you are comfortable with ssh and cross-compiling.
> There are no prebuilt binaries yet. Please open an issue with what you tried and what happened.

A Jellyfin player for Valve's discontinued **Steam Link** box, using its own hardware video decoder. The box becomes
a small Jellyfin client at the TV: a TV-style app on its home screen, plus a "Play on" target for the Jellyfin apps
and Home Assistant.

Not affiliated with Valve or the Jellyfin project.

## What it does

- **On-TV app**: a "Jellyfin" tile on the Link's home screen. Continue Watching, Next Up, Live TV, Libraries, Search,
  audio and subtitle track choice. It returns to where you were after playback.
- **Playback** through the Link's H.264 decoder: H.264 8-bit up to 1080p is copied as is; anything else (HEVC,
  10-bit, 4K) is re-encoded by the Jellyfin server. Audio is always converted to stereo AAC by the server.
- **Subtitles**: text tracks drawn on the Link; picture tracks (PGS, VobSub) burned in by the server.
- **Remote control**: shows up in Jellyfin as a device you can cast to and control (play, pause, seek, stop, tracks,
  d-pad), which also makes it a `media_player` in Home Assistant's Jellyfin integration.
- **Live TV** channels from Jellyfin's Live TV (tested with ErsatzTV over an M3U tuner).
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

## Known limits

- One program can use the Link's decoder at a time; pause = the player is stopped and restarted (about 3 s to
  picture on resume).
- No volume control (use the TV), no program guide (Live TV shows what is on now), no seeking in Live TV.
- Only stereo audio; no surround passthrough.
- Single-core 1 GHz CPU and ~256 MB RAM: the server does the heavy lifting.

## Licence

GPL-3.0-or-later, see [LICENSE](LICENSE). Third-party components and their licences: [NOTICE.md](NOTICE.md).
The Steam Link SDK is not included; you clone it from Valve.
