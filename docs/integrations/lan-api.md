# LAN API

An optional HTTP port on the Link for things on your network that are not Jellyfin apps, such as a Home Assistant text
box. **Off by default.** It has one request today and no authentication beyond an IP allow-list, so only list
machines you trust.

## Turn it on

In `/mnt/scratch/sljf/config.json` on the Link:

```json
"lan_allow": ["192.0.2.10", "192.0.2.20"],
"lan_port": 8098
```

Restart: `ssh steamlink /etc/init.d/startup/S98sljfd restart`. The log says `lan: /text on :8098 for ...`. Requests
from other addresses get 403 and a `lan: refused <ip>` log line.

## Requests

| Request | Does |
|---|---|
| `GET` or `POST /text?q=TEXT` | Opens the on-TV app's Search with TEXT typed and the first result focused (opens the app if the Link shows its own menu). Ignored while something plays. |

```bash
curl "http://<link-ip>:8098/text?q=the%20matrix"
```

Home Assistant example (text box -> search on the TV):
[examples/home-assistant/steamlink-search.yaml](../../examples/home-assistant/steamlink-search.yaml).

## Local interface (on the Link only)

The on-TV app uses a richer interface on `127.0.0.1:8097` (home rows, items, search, images, play, control, key,
events). It is not reachable from the network; its routes are listed at the top of `daemon/src/local.rs`. To try it
from your computer: `ssh -N -L 8097:127.0.0.1:8097 steamlink`, then `curl http://127.0.0.1:8097/home`.
