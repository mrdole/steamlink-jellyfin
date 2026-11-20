# Integrations

Everything beyond "play from the on-TV app" is optional. Each integration is independent: turn on only what you use.

| Integration | Needs on the Link | Doc |
|---|---|---|
| Jellyfin apps: "Play on", remote, track menus | nothing (always on) | below |
| Home Assistant: media player, d-pad, play/pause/seek/stop | nothing (goes through Jellyfin) | [home-assistant.md](home-assistant.md) |
| Search text from another device (HA text box, scripts) | `lan_allow` in config | [lan-api.md](lan-api.md) |
| Keyboard, game controller | plug in / pair | [input.md](input.md) |

## How remote control reaches the Link

```
Jellyfin app / Home Assistant ──> Jellyfin server ──websocket──> sljfd (on the Link)
other device on the LAN ──HTTP (lan_allow only)──────────────────> sljfd :8098 /text
keyboard / controller ──> sljf-ui (on-TV app, only while it is on screen)
```

sljfd registers as a Jellyfin session that supports: `PlayState`, `PlayNext`, `PlayMediaSource`, `MoveUp`, `MoveDown`,
`MoveLeft`, `MoveRight`, `Select`, `Back`, `GoHome`, `SetAudioStreamIndex`, `SetSubtitleStreamIndex`, `SendString`.

## Jellyfin apps

In any Jellyfin app signed in to the same server, the cast/"Play on" menu lists "Steam Link". Play, pause, seek, stop,
next/previous, and the audio and subtitle menus work. A Jellyfin app's remote (d-pad) drives the on-TV app; during
playback the keys mean:

| Key | During playback |
|---|---|
| OK (Select) | pause / resume; during the next-episode countdown: play the next one now |
| Back | stop (the app comes back); during the countdown: finish this one, then stop |
| Home | stop |
| Left / Right | back 10 s / forward 30 s (presses add up into one seek); Live TV: channel down / up |
| Up | show the on-screen bar |
| Down | sleep timer: off -> 30 -> 60 -> 90 min -> off |

## Writing a new integration

New inputs should end in the daemon's existing paths rather than adding new ones: a navigation key
(`up down left right select back home`) goes to the on-TV app through `Shared::key`, a playback command through
`Player::command` / `remote_key`, and text through `Cmd::Search` (see `daemon/src/main.rs` and `daemon/src/local.rs`).
Keep anything that listens on the network off by default and behind an allow-list in config, like `lan_allow`.
