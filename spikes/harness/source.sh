#!/usr/bin/env bash
# Plays a live test stream: test pattern with burned-in timecode + looped speech.
#
#   source.sh [--codec h264|hevc] [--audio FILE] [OUTPUT_URL]
#
# OUTPUT_URL defaults to an SRT listener on port 9000:
#   srt://127.0.0.1:9000?mode=listener
# Examples:
#   source.sh udp://127.0.0.1:5000?pkt_size=1316
#   source.sh --codec hevc 'srt://127.0.0.1:9000?mode=caller'
#
# Video: 1280x720 30 fps, 1 s GOP, no B-frames, zero-latency tuning.
# Audio defaults to $MULTI_MEDIA/long.wav (MULTI_MEDIA defaults to ./media).
# PTS starts at 0 and audio starts at 0, so stream time t = position in the audio
# file (mod its length), which the latency tool relies on.
set -euo pipefail
cd "$(dirname "$0")"

codec=h264
audio=${MULTI_MEDIA:-media}/long.wav
while [[ $# -gt 0 ]]; do
  case $1 in
    --codec) codec=$2; shift 2 ;;
    --audio) audio=$2; shift 2 ;;
    -h|--help) sed -n '2,15p' "$0"; exit 0 ;;
    *) break ;;
  esac
done
out=${1:-"srt://127.0.0.1:9000?mode=listener"}

[[ -f $audio ]] || { echo "missing $audio; run fetch-media.sh first" >&2; exit 1; }

case $codec in
  h264) venc=(-c:v libx264 -preset veryfast -tune zerolatency -bf 0 -g 30 -keyint_min 30 -sc_threshold 0 -b:v 3M) ;;
  hevc) venc=(-c:v libx265 -preset veryfast -tune zerolatency -x265-params "bframes=0:keyint=30:min-keyint=30:scenecut=0:log-level=error" -b:v 3M) ;;
  *) echo "unknown codec $codec" >&2; exit 1 ;;
esac

exec ffmpeg -hide_banner -loglevel warning -re \
  -f lavfi -i "testsrc2=size=1280x720:rate=30" \
  -stream_loop -1 -i "$audio" \
  -filter_complex "[0:v]drawtext=font=monospace:fontsize=48:fontcolor=white:box=1:boxcolor=black@0.6:x=40:y=40:text='%{pts\:hms}  f%{frame_num}'[v]" \
  -map "[v]" -map 1:a \
  "${venc[@]}" -pix_fmt yuv420p \
  -c:a aac -b:a 128k -ar 48000 -ac 2 \
  -f mpegts -mpegts_flags +resend_headers "$out"
