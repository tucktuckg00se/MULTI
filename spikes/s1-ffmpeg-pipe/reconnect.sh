#!/usr/bin/env bash
# S1 reconnect test: source runs 20 s, stops 5 s, restarts (PTS from 0) for
# 20 s, stops 1 s, restarts for 12 s. Downstream records UDP and SRT outputs.
# Ports 9101-9111.   reconnect.sh CODEC IN(udp|srt) OUTDIR
set -uo pipefail
codec=$1 in=$2 out=$3
here=$(cd "$(dirname "$0")" && pwd)
bin=$here/../target/release/s1-ffmpeg-pipe
harness=$here/../harness
export MULTI_MEDIA=${MULTI_MEDIA:-$harness/media}
mkdir -p "$out"
if [[ $in == srt ]]; then
  input='srt://127.0.0.1:9110?mode=caller&latency=120000'
  src_url='srt://127.0.0.1:9110?mode=listener&latency=120000'
else
  input='udp://127.0.0.1:9102?fifo_size=1000000&overrun_nonfatal=1'
  src_url='udp://127.0.0.1:9102?pkt_size=1316'
fi
"$bin" run --input "$input" \
  --output 'udp://127.0.0.1:9103?pkt_size=1316' \
  --output 'udp://127.0.0.1:9104?pkt_size=1316' \
  --output 'srt://127.0.0.1:9111?mode=listener&latency=120000&pkt_size=1316' \
  --csv-dir "$out" --duration 70 >"$out/pipe.log" 2>&1 &
pipe=$!
sleep 0.5
"$bin" relay --listen 127.0.0.1:9103 --log "$out/out-udp.csv" --duration 68 &
tap=$!
ffmpeg -hide_banner -loglevel warning -y -i 'udp://127.0.0.1:9104?timeout=30000000' -t 64 -c copy -f mpegts "$out/rec-udp.ts" >"$out/rec-udp.log" 2>&1 &
r1=$!
ffmpeg -hide_banner -loglevel warning -y -i 'srt://127.0.0.1:9111?mode=caller&latency=120000' -t 64 -c copy -f mpegts "$out/rec-srt.ts" >"$out/rec-srt.log" 2>&1 &
r2=$!
run() { echo "$(date +%s.%N) source start $1 s" >>"$out/events.txt"; timeout "$1" "$harness/source.sh" --codec "$codec" "$src_url" >>"$out/source.log" 2>&1; echo "$(date +%s.%N) source stop" >>"$out/events.txt"; }
run 20; sleep 5; run 20; sleep 1; run 12
wait $r1 $r2 $pipe
kill $tap 2>/dev/null
