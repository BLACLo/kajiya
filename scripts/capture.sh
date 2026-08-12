#!/usr/bin/env bash
# Capture the centered kajiya view window as a PNG.
# Usage: capture.sh out.png [WxH]   (default window size: 960x540)
set -euo pipefail

OUT="${1:?usage: capture.sh out.png [WxH]}"
GEO="${2:-960x540}"
W="${GEO%x*}"
H="${GEO#*x}"

DISPLAY="${DISPLAY:-:0}"
export DISPLAY

SCREEN="$(xdpyinfo -display "$DISPLAY" | awk '/dimensions/ {print $2}')"
SW="${SCREEN%x*}"
SH="${SCREEN#*x}"
X=$(( (SW - W) / 2 ))
Y=$(( (SH - H) / 2 ))
[ "$X" -lt 0 ] && X=0
[ "$Y" -lt 0 ] && Y=0

ffmpeg -y -loglevel error -f x11grab -video_size "${GEO}" -i "${DISPLAY}+${X},${Y}" -frames:v 1 "$OUT"
echo "saved: $OUT (window ${GEO} at +${X},+${Y})"
