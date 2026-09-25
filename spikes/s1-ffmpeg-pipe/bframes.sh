#!/usr/bin/env bash
# B-frame check: source like source.sh but with B-frames, through the pipe to
# a file, then verify.sh. Ports 9141-9142.
#   bframes.sh CODEC(h264|hevc) OUTDIR [extra pipe args, e.g. --reorder 0]
set -uo pipefail
codec=$1 out=$2; shift 2
here=$(cd "$(dirname "$0")" && pwd)
bin=$here/../target/release/s1-ffmpeg-pipe
media=${MULTI_MEDIA:-$here/../harness/media}
mkdir -p "$out"
case $codec in
  h264) venc=(-c:v libx264 -preset veryfast -bf 2 -g 30 -keyint_min 30 -sc_threshold 0 -b:v 3M) ;;
  hevc) venc=(-c:v libx265 -preset veryfast -x265-params "bframes=2:keyint=30:min-keyint=30:scenecut=0:log-level=error" -b:v 3M) ;;
esac
"$bin" run --input 'udp://127.0.0.1:9141?fifo_size=50000&overrun_nonfatal=1' --output "file:$out/out.ts" \
  --csv-dir "$out" --duration 40 "$@" >"$out/pipe.log" 2>&1 &
pipe=$!
sleep 0.5
timeout 36 ffmpeg -hide_banner -loglevel warning -re -f lavfi -i "testsrc2=size=1280x720:rate=30" \
  -stream_loop -1 -i "$media/long.wav" -map 0:v -map 1:a "${venc[@]}" -pix_fmt yuv420p \
  -c:a aac -b:a 128k -ar 48000 -ac 2 -f mpegts 'udp://127.0.0.1:9141?pkt_size=1316' >"$out/source.log" 2>&1
wait $pipe
"$here/../harness/verify.sh" "$out/out.ts" "$here/expected.txt" >"$out/verify.txt" 2>&1
echo "verify exit=$?"
