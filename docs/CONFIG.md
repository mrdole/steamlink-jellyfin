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
| `audio_lang` | `eng` | ISO 639-2 language for picking the audio track when the Jellyfin user has no preference |
| `player` | `/mnt/scratch/sljf/sljf-play` | Path of the player binary |
| `ca` | `/mnt/scratch/sljf/ca.pem` | CA bundle used for HTTPS |

## Optional integrations

| Key | Default | What |
|---|---|---|
| `lan_allow` | `[]` (off) | IP addresses allowed to use the [LAN API](integrations/lan-api.md). Empty = the LAN port is not opened |
| `lan_port` | `8098` | Port for the LAN API |

Integrations that need no config on the Link: the Jellyfin apps and [Home Assistant](integrations/home-assistant.md)
(both talk to the Link through your Jellyfin server), [keyboard and game controllers](integrations/input.md).

## App theme

Build-time, not in config.json: `THEME=<name> scripts/build-app.sh` uses `themes/<name>.yaml` (default `gruvbox`).
