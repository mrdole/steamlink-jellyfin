# Home screen, TV guide and Live TV static: design

Status: reviewed, waiting for approval, 2026-11-20. Scope: the public repo only. Casting from the YouTube app is a separate design.

## Why

A Steam Link without a controller starts on its own "connect a controller" menu, which nothing else can drive.
v0.1.0-alpha.3 added `open_app_at_boot`, which opens the Jellyfin app instead. The owner wants the box to feel
like a Roku: a simple home screen of big tiles at power-on, driven by the Jellyfin / Home Assistant remote, with
Live TV one click away. Static while a channel tunes or buffers should make Live TV feel like a real TV.

## Goals

1. **Home screen** at the top of the on-TV app: tiles JELLYFIN, LIVE TV, STEAM, in the app's theme, d-pad + OK.
2. **TV guide**: a guide grid (channels down the side, about the next 2 hours across, programme blocks) as its own
   screen in the app. The LIVE TV tile opens it; **Home while watching Live TV opens it** (playback stops), focused on
   the channel that was playing. OK on a channel tunes it. Home from the guide goes to the home screen.
3. **STEAM tile** hands the screen back to the Link's own menu (the app exits, as Back on the app's home does today).
4. **Static (TV snow)** with a faint hiss for the WHOLE wait from pressing OK / flipping a channel until its picture
   shows (including the server opening the channel, up to ~15 s), and whenever Live TV drops or rebuffers. Both on by
   default, both settings in config.json.
5. **Channel flipping while watching** (Left/Right, as today) feels like a TV: static between channels instead of a
   black screen, and the guide afterwards focuses the channel last watched.

## Non-goals

- The guide drawn over a playing channel (the owner chose a separate screen; the player would have to draw it).
- YouTube (separate design), an on-TV settings screen, tiles for other apps in `/home/apps`, static for films/episodes.

## Anti-goals (a failure even if it technically works)

- Static or hiss over real video or sound: it must go away at the first decoded video frame (and when sound resumes).
- A loud hiss. "Really quiet but present": clearly below normal programme loudness.
- Getting stuck: there must always be a way from the home screen to the Link's menu (STEAM) and back (Home key).
- Slower boot or a slower app start; extra CPU during normal playback.

## How it works today (what this touches)

- `app/main.qml` (Qt Quick, one file): `depth` 0 = the Jellyfin home (rows Continue Watching, Next Up, LIVE TV,
  Libraries), n = pages. `key("home")` goes to depth 0; `key("back")` at depth 0 calls `Qt.quit()`, `run.sh`
  then exits and the Link's menu comes back. Playback: the app saves its state and exits 42; `run.sh` restarts it with
  `--restore` after playback.
- Live TV channels come from sljfd `GET /home` (`channels`); the LIVE TV row is built from them.
- sljfd `GoHome` opens the app when the Link shows its own menu; `open_app_at_boot` does the same once per boot.
- `player/sljf-play.c` + `player/sljf-subs.c`: the player draws subtitles and the on-screen bar on an SLVideo overlay
  (one overlay shown at a time, 1280x260 strip at the bottom). sljfd sets `SLJF_PREBUFFER_MS` for Live TV; the
  player logs `read-ahead ran out` when the stream runs dry. Each channel change restarts sljf-play.

## Approaches considered

- **A. A new top level inside the existing app (picked).** One more `depth` (the home screen above the Jellyfin home),
  reusing the remote handling, theme, channel data and the restore-after-playback state. Small change.
- B. A separate launcher program listing `/home/apps` entries. More general, but duplicates input, theme and restore
  logic for three tiles, and two Qt programs swapping the screen add start-up time.
- Static: drawn by the **player** (it owns the screen while tuning and buffering, and knows exactly when the first
  frame shows) rather than by the app or sljfd. The app is not running during playback.

## Design

### Home screen (app)

- New level above the Jellyfin home. The app starts there (normal start and `open_app_at_boot`).
- Tiles: JELLYFIN (goes to today's home, depth 0), LIVE TV (the TV guide), STEAM (exit to the Link's menu).
  The LIVE TV tile is left out when the server has no Live TV channels.
- Keys: Home (remote, keyboard, controller Start) goes to the home screen, except while watching Live TV, where it
  opens the TV guide (below). Home during a film/episode stops it and the app comes back on the home screen. Back during
  playback still returns to where the user was. sljfd tells the app why playback ended (e.g. a flag in `/events` or
  the state file read with `--restore`): "home" -> home screen, "guide" -> guide on channel X, otherwise restore. Back from the Jellyfin home goes to
  the home screen; Back on the home screen does nothing (STEAM is the way out). Look: header "STEAM LINK" + clock like
  the other pages, three large tiles in a row, hints line `ARROWS MOVE  OK SELECT`.
- Restore after playback keeps working for every level (home screen, Jellyfin pages, the TV guide).
- sljfd's `GoHome` when the app is open sends the existing "home" key, which now means the home screen.

### TV guide (app + sljfd)

- Its own screen in the app. Rows = channels (logo, number, name) in channel-number order; columns = time from the
  current half hour to about 2 hours ahead, with a time ruler and a "now" line; each programme a block sized by
  length, with its title (and episode name when it fits). The focused block shows its title, time and description in a
  strip at the top. Up/Down move between channels (paging the rows), Left/Right between programmes in time (scrolling
  the time window later/earlier, not into the past beyond "now").
- OK on any block of a channel tunes that channel now (future programmes are not recorded or scheduled).
- Opened from: the LIVE TV tile, Home while watching Live TV, and the LIVE TV row's channels stay as they are.
- Focus when opened: the channel last watched (after Left/Right flips), else the first channel; time = now.
  sljfd exposes the last channel id and the app prefers it.
- Back from the guide: to where it was opened from (home screen, or the Jellyfin home for the row); after Home during
  Live TV, Back goes to the home screen. Home from the guide: the home screen.
- Data: a new sljfd local endpoint (e.g. `GET /guide?hours=N`) built from Jellyfin's Live TV programmes
  (`/LiveTv/Programs` for the channels, from now to now+N h), refreshed when the guide opens and when the time window
  moves past what is loaded. Channels without guide data show one block "No information".
- The LIVE TV row on the Jellyfin home stays.

### Static (player)

- sljfd passes `SLJF_STATIC=<volume 0-100>` to sljf-play for Live TV when `live_tv_static` is true; no variable =
  no static (films, episodes, or the setting off).
- Covers the whole tuning wait. Today `play_live` (daemon) first calls `open_live` (PlaybackInfo, up to ~15 s while
  ErsatzTV starts the channel), and only then takes the screen and starts sljf-play; on a flip `next()` stops the old
  player first, so the screen is black for that wait. New: as soon as a Live TV channel is chosen (OK in the app, a
  flip, a reconnect after a drop: `restart_at` / `retry_later`), sljfd takes the screen and starts sljf-play in a
  static-only mode (like `--card`), then hands it the stream URL once `open_live` returns (stdin line), or replaces
  it with a normal player that starts with static, whichever proves simpler and has no black gap longer than ~0.5 s.
  The app gives up the screen at once for Live TV (exit 42) instead of showing STARTING. `open_live` must not block
  the daemon's command handling while it waits (a flip during the wait cancels it and tunes the new channel).
- Picture: a full-screen overlay filled with grey noise at low resolution (about 320x180, scaled up by the overlay;
  chunky analogue snow), new noise about 15 times a second. Shown until the first decoded video frame is displayed,
  and again while video has run dry mid-stream (the same condition as `read-ahead ran out`, after a short grace so a
  single late frame does not flash it). Subtitles / the bar are not drawn while static shows.
- Sound: white noise (softened, e.g. low-passed) at `static_volume` percent of full scale, through the player's
  audio output while no programme audio is playing; stops when programme audio starts. 0 = silent static.
- Open failures (channel will not start): static stays while sljfd retries as today; when it gives up, the app comes
  back as it does now.
- CPU: noise generation must stay small next to decoding (a cheap PRNG over 320x180 bytes per frame).

### Config (config.json, docs/CONFIG.md, examples/config.example.json)

| Key | Default | What |
|---|---|---|
| `live_tv_static` | `true` | Static on screen while a Live TV channel tunes or buffers |
| `static_volume` | `8` | Hiss loudness with the static, percent of full scale; `0` = silent |

## Left to the builder

- Guide paging details, how many rows show at once, block text truncation.
- Exact tile sizes and art (text tiles with an icon are fine), noise texture details
  (scanline roll, flicker), the low-pass and the precise percent scale, the rebuffer grace time.
- Static-only mode then URL on stdin vs a replacing player (constraint above: no black gap over ~0.5 s).
- Whether the overlay for static reuses sljf-subs' overlays or gets its own (only one overlay shows at a time).

## Testing

- Host: `scripts/check.sh` (daemon config parsing tests for the two keys).
- Link (a camera pointed at the TV, logs): boot -> home screen; each tile; Home and Back from every level;
  TV guide: grid fills from Jellyfin, d-pad through channels and time, OK tunes, Home while watching Live TV opens it on
that channel (focus = last watched after flips), Back/Home from it; static for the whole wait at channel start
and on Left/Right flips (no black screen), including a flip during the wait; Home during playback -> home screen, Back
-> where you were; rebuffer static
  with the test relay used for the Live TV stutter fix (bursts + 3 s stalls); `live_tv_static: false` -> none;
  `static_volume: 0` -> silent; a film shows no static. Hiss loudness judged by ear by the owner.
