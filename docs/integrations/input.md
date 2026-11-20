# Keyboard, game controller, mouse

The on-TV app (`sljf-ui`, `app/main.cpp`) turns every input into one of seven navigation keys:
`up down left right select back home`.

| Input | Status | Mapping |
|---|---|---|
| USB or Bluetooth keyboard | Coded, untested on real hardware | Arrows move, Enter selects, Esc = back, Home = home; typing and Backspace work in Search |
| Game controller (USB, or Bluetooth paired in the Link's own menu) | Coded, untested on real hardware | D-pad or left stick move, A selects, B or Back = back, Start = home |
| Mouse | Not supported | The pointer is parked off screen |
| Any input **during playback** | Not supported yet | While a video plays the app is closed; use a Jellyfin app or Home Assistant as the remote |

If you try a keyboard or controller, please report what worked in an issue (device, USB or Bluetooth).

## Adding an input

- In the app: map it to a Qt key in `app/main.cpp` (`keyForButton` for controllers, or post a `QKeyEvent`);
  `main.qml` handles the rest in `Keys.onPressed`.
- During playback (planned, see [ROADMAP](../../ROADMAP.md)): the daemon must read the device itself
  (evdev or SDL) and call `remote_key` with the same names the Jellyfin remote uses (`Select`, `Back`, `GoHome`,
  `MoveUp`...). It should be optional in config, like `lan_allow`.
