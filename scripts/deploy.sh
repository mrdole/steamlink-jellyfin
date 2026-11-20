#!/usr/bin/env bash
# Copy the player + daemon (+ the on-TV app if built) to the Steam Link and (re)start the daemon.   scripts/deploy.sh
# Needs scripts/build.sh output in out/ and key login over ssh (host alias SLJF_HOST, default `steamlink`).
# Login (once): tools/sljf-login.
set -euo pipefail
HERE=$(cd "$(dirname "$0")/.." && pwd)
H=${SLJF_HOST:-steamlink}
R=/mnt/scratch/sljf
for f in sljf-play sljfd ca.pem sub.ttf VERSION; do [ -f "$HERE/out/$f" ] || { echo "out/$f missing: run scripts/build.sh"; exit 1; }; done
ssh "$H" "mkdir -p $R; [ -x /etc/init.d/startup/S98sljfd ] && /etc/init.d/startup/S98sljfd stop; true" 2>/dev/null
scp -q -O "$HERE/out/sljf-play" "$HERE/out/sljfd" "$HERE/out/ca.pem" "$HERE/out/sub.ttf" "$HERE/out/VERSION" "$H:$R/" 2>/dev/null
scp -q -O "$HERE/daemon/sljfd.init" "$H":/etc/init.d/startup/S98sljfd 2>/dev/null
ssh "$H" "chmod 755 /etc/init.d/startup/S98sljfd $R/sljfd $R/sljf-play; /etc/init.d/startup/S98sljfd start </dev/null >/dev/null 2>&1" 2>/dev/null
sleep 5
ssh "$H" "echo installed: \$(cat $R/VERSION); /etc/init.d/startup/S98sljfd status" 2>/dev/null
if [ -d "$HERE/out/app/jellyfin" ]; then   # the on-TV app (build-app.sh): the "Jellyfin" tile on the Link's home screen
  # a running app cannot be overwritten ("text file busy"): close it first, the Link goes back to its menu
  ssh "$H" "killall sljf-ui 2>/dev/null; sleep 1; mkdir -p /home/apps/jellyfin/fonts" 2>/dev/null
  scp -q -O "$HERE"/out/app/jellyfin/{sljf-ui,main.qml,theme.js,toc.txt,icon.png,run.sh} "$H":/home/apps/jellyfin/ 2>/dev/null
  scp -q -O "$HERE"/out/app/jellyfin/fonts/* "$H":/home/apps/jellyfin/fonts/ 2>/dev/null
  ssh "$H" "chmod 755 /home/apps/jellyfin/sljf-ui /home/apps/jellyfin/run.sh" 2>/dev/null
fi
