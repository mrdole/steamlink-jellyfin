# Home Assistant

Nothing to install on the Link. Home Assistant's stock **Jellyfin** integration talks to your Jellyfin server, and the
server relays commands to the Link.

## Set up

1. In Home Assistant: Settings -> Devices & services -> Add integration -> **Jellyfin**. Server URL, and the same
   Jellyfin account the Link uses (`tools/sljf-login`). HA becomes another Jellyfin device: if the account only allows
   listed devices, allow it in the Jellyfin dashboard.
2. With the Link's daemon running you get, named after `device_name` in config:
   - `media_player.steam_link`: play/pause, seek, stop, browse media and play it on the TV;
   - `remote.steam_link`: d-pad and navigation for the on-TV app.

## Examples

Play something (content id = Jellyfin item id):

```yaml
action: media_player.play_media
target: {entity_id: media_player.steam_link}
data: {media_content_id: "<jellyfin item id>", media_content_type: "movie"}
```

D-pad buttons (Jellyfin command names: `MoveUp`, `MoveDown`, `MoveLeft`, `MoveRight`, `Select`, `Back`, `GoHome`;
`GoHome` also opens the app when the Link shows its own menu):

```yaml
action: remote.send_command
target: {entity_id: remote.steam_link}
data: {command: Select}
```

Typing a search from your phone needs the [LAN API](lan-api.md): see
[examples/home-assistant/steamlink-search.yaml](../../examples/home-assistant/steamlink-search.yaml).

## Notes

- HA starts items at 0:00 (no resume point); seek afterwards, or start from the on-TV app to resume.
- Volume: the Link has none; control the TV or receiver from HA instead.
- Power: a smart plug on the Link plus your TV's integration (or HDMI-CEC) can make a "Watch on Steam Link" script.
