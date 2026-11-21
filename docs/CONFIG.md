# Configuration

One file on the Link: `/mnt/scratch/sljf/config.json`, mode 600 (it holds the Jellyfin token). `tools/sljf-login`
writes the required keys; add optional ones by hand and restart the daemon:

```bash
ssh steamlink 'vi /mnt/scratch/sljf/config.json && /etc/init.d/startup/S98sljfd restart'
```

Example with every key: [examples/config.example.json](../examples/config.example.json).

## Keys

| Key | Default | What |
|---|---|---|
| `server` | (required) | Jellyfin URL, `https://...` |
| `user_id` | (required) | Written by sljf-login |
| `token` | (required) | Written by sljf-login. Revoke by removing the device in Jellyfin |
| `device_id` | `steamlink` | Jellyfin DeviceId; give each Link its own |
| `device_name` | `Steam Link` | Name shown in Jellyfin and Home Assistant |
| `av_offset_ms` | `0` | Lip-sync for your TV: positive plays the sound later. Find it with the sync clip (DEVELOPING.md); 60 was right for one Vizio |
| `volume` | `100` | Start volume 0-100 (100 = untouched; the Link always sends full-scale sound over HDMI). Squared, so the steps sound even: 50 = -12 dB, 40 = -16 dB, 20 = -28 dB. A Jellyfin remote or Home Assistant changes it until sljfd restarts |
| `audio_lang` | `eng` | ISO 639-2 language for picking the audio track when the Jellyfin user has no preference |
| `player` | `/mnt/scratch/sljf/sljf-play` | Path of the player binary |
| `ca` | `/mnt/scratch/sljf/ca.pem` | CA bundle used for HTTPS |

## Optional integrations

| Key | Default | What |
|---|---|---|
| `lan_allow` | `[]` (off) | IP addresses allowed to use the [LAN API](integrations/lan-api.md). Empty = the LAN port is not opened |
| `lan_port` | `8098` | Port for the LAN API |
| `open_app_at_boot` | `false` | Open the on-TV app after the Link starts, in place of its menu ("connect a controller" prompt). Handy without a controller: the Jellyfin remote / Home Assistant then drives the app. Once per boot; leaving the app shows the Link's menu as usual |
| `live_tv_static` | `true` | Static on screen with a faint hiss while a Live TV channel tunes, and when it drops or rebuffers |
| `static_volume` | `8` | Hiss loudness with the static, percent of full scale before `volume` is applied (the hiss is turned down and muted with the programme sound); 0 = silent |

Integrations that need no config on the Link: the Jellyfin apps and [Home Assistant](integrations/home-assistant.md)
(both talk to the Link through your Jellyfin server), [keyboard and game controllers](integrations/input.md).

## App theme

Build-time, not in config.json: `THEME=<name> scripts/build-app.sh` uses `themes/<name>.yaml` (default `gruvbox`); `THEME=/path/to/my-theme.yaml` uses a theme kept outside the repo.
