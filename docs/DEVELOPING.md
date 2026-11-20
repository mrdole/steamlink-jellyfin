# Developing

## Layout

| Path | What |
|---|---|
| `player/` | `sljf-play.c` (playback), `sljf-subs.c` (subtitle + bar overlay) |
| `daemon/` | `sljfd` (Rust): `src/main.rs` Jellyfin session + player control, `src/local.rs` local/LAN HTTP |
| `app/` | On-TV app: `main.cpp` (Qt host, controller input), `main.qml` (UI), `run.sh`, `toc.txt` (tile) |
| `scripts/` | `build.sh`, `build-app.sh`, `deploy.sh`; `check.sh` (pre-merge checks, also CI), `bump-version`, `release.sh` (docs/RELEASING.md) |
| `tools/` | Test and dev tools (below) |
| `themes/` | App themes |
| `out/` | Build output (gitignored) |

Every script takes `SLJF_HOST` (ssh alias of the Link, default `steamlink`) and `SDK` (default `~/steamlink-sdk`).

## Tools

| Tool | Does |
|---|---|
| `tools/sljf-test [query or item id or file] --seconds N --start MIN [--transcode] [--subs N] [--info]` | Plays one item on the Link with the player directly (no daemon), prints the status lines. `--info` only shows the choice and URL |
| `tools/sljf-remote status | play <id> [--at MIN] | pause | unpause | seek MIN | stop | key up|down|...` | Drives the daemon through the Jellyfin server, like a Jellyfin app does |
| `tools/sljf-memwatch [MINUTES] [INTERVAL_S]` | CSV of the Link's memory and our processes, for soak tests |
| `tools/make-sync-test.sh` | Generates `out/sync-test.ts`: a bar crosses a line with a flash + beep every 2 s. Play it with `tools/sljf-test out/sync-test.ts --seconds 120 --av-offset MS` to find `av_offset_ms` |
| `app/shot.py` | Screenshot of the app on the build machine (PyQt5, offscreen), with keys scripted |

The test tools use the Link's own login (read over ssh for the run). Only one program may use the decoder: check
`tools/sljf-remote status` shows nothing playing before running `sljf-test`.

## Previewing the app without a TV

```bash
ssh -N -L 8097:127.0.0.1:8097 steamlink &
qml-qt5 out/app/jellyfin/main.qml
```

## Logs on the Link

- daemon: `/mnt/scratch/sljf/sljfd.log` (and `.1`)
- app: `/tmp/sljf-ui.log`
- `ssh steamlink /etc/init.d/startup/S98sljfd status`

## Conventions

- Anything that listens on the network is off by default and allow-listed in config.
- Nothing secret on the build machine or in argv/URLs: the token reaches the player through `SLJF_HEADERS` or ssh stdin.
- Keep the Link's own system untouched: our files live in `/mnt/scratch/sljf`, `/home/apps/jellyfin` and one init
  script.
