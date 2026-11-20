#!/usr/bin/env bash
# Make a lip-sync test clip for the Steam Link player (generated, nothing downloaded):
#   tools/make-sync-test.sh [out.ts]      then: tools/sljf-test out/sync-test.ts --seconds 120
# 1080p H.264 High 23.976 fps + stereo AAC in MPEG-TS, 2 minutes. A yellow bar sweeps left to right every 2 s; at the
# moment it crosses the grey centre line a white box flashes (2 frames) and a 1 kHz beep (40 ms) sounds.
# Beep before the bar reaches the line = sound early; after = sound late. One bar width (20 px) is about 21 ms.
set -euo pipefail
OUT=${1:-$(dirname "$0")/../out/sync-test.ts}; mkdir -p "$(dirname "$OUT")"
ffmpeg -v error -y \
  -f lavfi -i "color=c=black:s=1920x1080:r=24000/1001:d=120" \
  -f lavfi -i "color=c=yellow:s=20x400:r=24000/1001:d=120" \
  -f lavfi -i "aevalsrc='0.6*sin(2*PI*1000*t)*lt(mod(t+1,2),0.04)':s=48000:d=120:c=stereo" \
  -filter_complex "[0:v]drawbox=x=958:y=0:w=4:h=1080:color=gray:t=fill,drawbox=x=760:y=60:w=400:h=220:color=white:t=fill:enable='lt(mod(t+1,2),0.084)',drawtext=text='beep + flash when the bar crosses the centre line  %{pts\:hms}':fontcolor=white:fontsize=40:x=(w-tw)/2:y=900[b];[b][1:v]overlay=x='(mod(t,2)/2)*W-10':y=340[v]" \
  -map "[v]" -map 2:a -c:v libx264 -profile:v high -pix_fmt yuv420p -b:v 4M -g 48 -c:a aac -b:a 192k -f mpegts "$OUT"
ls -l "$OUT"
