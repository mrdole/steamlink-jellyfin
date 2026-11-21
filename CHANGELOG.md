# Changelog

All notable changes. Format: [Keep a Changelog](https://keepachangelog.com/en/1.1.0/); versions:
[SemVer](https://semver.org/), `0.x` with `-alpha.N` / `-beta.N` / `-rc.N` while the project is young (config and
layout may change in any `0.x` release). Each PR adds its line under **[Unreleased]**; `scripts/bump-version`
moves those lines into the new version's section.

## [Unreleased]

### Added
- Volume: the player turns the sound down in software (the Link always sends full-scale audio over HDMI). Start
  level from `volume` in config (0-100, default 100 = as before); Jellyfin remotes and Home Assistant's volume slider
  and mute change it while playing (`SetVolume`, `VolumeUp`/`VolumeDown` in steps of 5, `Mute`/`Unmute`/`ToggleMute`),
  kept until sljfd restarts, and the level is reported back.

## [0.1.0-alpha.3] - 2026-11-20

### Added
- `open_app_at_boot` (config, off by default): the on-TV app opens by itself after the Link starts, in place of the
  Link's "connect a controller" menu, so a Link without a controller can be used from the Jellyfin or Home Assistant
  remote right away.

## [0.1.0-alpha.2] - 2026-11-20

### Fixed
- Live TV stutter: network reads now run in their own thread. A read that waited for data (the server sending in
  bursts) stopped the player feeding the decoders, which ran dry with seconds of stream already read ahead (picture
  freezes, constant sync corrections). Status lines show `read max` and the per-stream read-ahead.

## [0.1.0-alpha.1] - 2026-11-20

First public release. Early alpha: tested on one Steam Link against one Jellyfin server.

### Added
- `sljf-play`: hardware-decoded playback (SLVideo/SLAudio) from Jellyfin streams with a static minimal FFmpeg and
  mbedTLS; A/V sync with a per-TV offset; frame rate measured from the stream; text subtitles and an on-screen bar
  drawn on an overlay; `--card` (paused bar) and `--version`.
- `sljfd`: Jellyfin session with remote control (play, pause, seek, stop, next/previous, tracks, d-pad, SendString),
  queue with next-episode countdown and "still watching", sleep timer, audio/subtitle track choice, Live TV,
  automatic recovery after network drops and reboots; local API for the on-TV app.
- On-TV app (`sljf-ui`, Qt Quick): Continue Watching, Next Up, Live TV, Libraries, Search (series from episode
  matches), play dialog with audio/subtitle rows, back to where you were after playback; themes in `themes/` or
  any theme file (`THEME=/path.yaml`).
- Optional LAN API (`lan_allow`, `lan_port`): `/text` opens Search with text, e.g. from Home Assistant.
- Build, deploy and test scripts for Valve's steamlink-sdk; docs for install, config, architecture and integrations
  (Jellyfin apps, Home Assistant, LAN API, keyboard/controller).
- One release version (`VERSION`) used by the daemon, player, tools and Jellyfin device info; `scripts/check.sh`,
  `scripts/bump-version`, `scripts/release.sh`, CI.

[Unreleased]: https://github.com/mrdole/steamlink-jellyfin/compare/v0.1.0-alpha.3...HEAD
[0.1.0-alpha.3]: https://github.com/mrdole/steamlink-jellyfin/releases/tag/v0.1.0-alpha.3
[0.1.0-alpha.2]: https://github.com/mrdole/steamlink-jellyfin/releases/tag/v0.1.0-alpha.2
[0.1.0-alpha.1]: https://github.com/mrdole/steamlink-jellyfin/releases/tag/v0.1.0-alpha.1
