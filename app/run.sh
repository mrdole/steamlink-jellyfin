#!/bin/sh
# Jellyfin tile on the Steam Link: show the app; when it hands the screen to the player (exit 42), wait until
# playback is over and show the app again (--restore: back on the screen and row the user left). Any other exit goes
# back to the Link's menu.
cd "$(dirname "$0")" || exit 1
API=http://127.0.0.1:8097
ARG=
while true; do
    ./sljf-ui $ARG >/tmp/sljf-ui.log 2>&1
    [ $? -eq 42 ] || break
    ARG=--restore
    sleep 3
    while wget -q -O - "$API/events?since=0" 2>/dev/null | grep -q '"playing":true'; do sleep 1; done
done
