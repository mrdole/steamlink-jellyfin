# Roadmap

Early alpha. Rough order; nothing here is promised.

## Toward a beta

- [ ] Prebuilt release tarball (player, daemon, app) and a one-step installer, so users skip the 4.7 GB SDK
- [ ] Keyboard and game controller confirmed on real hardware in the app
- [ ] Keyboard / controller **during playback** (daemon reads evdev or SDL, optional in config)
- [ ] Mouse support in the app (or a deliberate "no")
- [ ] Settings screen in the app (lip-sync offset, audio language, subtitles default) instead of editing config.json
- [ ] Login from the TV (Jellyfin Quick Connect) instead of `tools/sljf-login` from a computer
- [ ] LAN API: token auth as an alternative to the IP allow-list; more requests (play, control, key, status)
- [ ] Explain or fix short decoder-queue dips on some Live TV channels

## Later

- [ ] Program guide for Live TV
- [ ] Bluetooth headphones (A2DP worked in an experiment; reconnect and volume jumps unsolved)
- [ ] HDMI-CEC: TV on / input switch when playback starts
- [ ] More themes
