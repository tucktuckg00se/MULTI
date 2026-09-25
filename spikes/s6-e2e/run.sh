#!/usr/bin/env bash
# S6 harness run: source.sh -> latency tap -> s6-e2e -> latency tap -> verify.sh,
# plus an SRT-listener output check, ccextractor (CC1, CC3, 708 svc 1-4),
# video delay and EN caption lag.
#
#   run.sh OUT_DIR [SECONDS] [--codec h264|hevc]
#
# Ports 9611-9616. Needs MULTI_MEDIA and SHERPA_ONNX_LIB_DIR as for the build.
set -uo pipefail
out=${1:?out dir}; secs=${2:-120}; codec=h264
[[ ${3:-} == --codec ]] && codec=${4:-h264}
here=$(cd "$(dirname "$0")" && pwd); spikes=$(dirname "$here")
bin=$spikes/target/release/s6-e2e; lat=$spikes/target/release/latency
ccx=${CCEXTRACTOR:-$HOME/.cache/multi-tools/ccx-build/ccextractor}
: "${MULTI_MEDIA:?set MULTI_MEDIA}"
mkdir -p "$out"; out=$(cd "$out" && pwd)
pids=()
cleanup() { for p in "${pids[@]}"; do kill "$p" 2>/dev/null; done; wait 2>/dev/null; }
trap cleanup EXIT

"$lat" tap --listen 127.0.0.1:9611 --forward 127.0.0.1:9612 --out "$out/in.csv" 2>"$out/tap-in.log" & pids+=($!)
"$lat" tap --listen 127.0.0.1:9613 --forward 127.0.0.1:9614 --out "$out/out.csv" 2>"$out/tap-out.log" & pids+=($!)
RUST_LOG=info "$bin" --input udp://127.0.0.1:9612 --codec "$codec" \
  --output udp://127.0.0.1:9613 --output 'srt://127.0.0.1:9615?mode=listener' \
  --words-log "$out/words.tsv" --emit-log "$out/emit.tsv" --duration-s $((secs + 12)) \
  >"$out/s6.log" 2>&1 & s6=$!; pids+=($s6)
# Start the source once ASR and the three translators are loaded.
for _ in $(seq 60); do
  [[ $(grep -c 'ASR ready\|translator ready' "$out/s6.log") -ge 4 ]] && break; sleep 0.5
done
grep 'ASR ready' "$out/s6.log" | tail -1
"$spikes/harness/source.sh" --codec "$codec" "udp://127.0.0.1:9611?pkt_size=1316" >"$out/source.log" 2>&1 & pids+=($!)
# verify.sh fails on a recording that starts at the first output packet (S2 gotcha 6).
sleep 5
"$spikes/harness/verify.sh" 'srt://127.0.0.1:9615?mode=caller' --duration 8 >"$out/verify-srt.log" 2>&1 &
# Record the UDP output byte-exact (PTS untouched), then extract from the file.
timeout -s INT "$secs" gst-launch-1.0 -eq udpsrc port=9614 buffer-size=8388608 ! filesink location="$out/capture.ts" 2>/dev/null
wait $s6
cleanup; pids=()
ts=$out/capture.ts
"$spikes/harness/verify.sh" "$ts" --srt-out "$out/cc1.srt" >"$out/verify-udp.log" 2>&1
{
  echo "== s6 $codec ${secs}s"
  echo "-- verify (UDP, CC1): $(grep -c . "$out/cc1.srt") srt lines; SRT output: $(grep -c . "$(dirname "$(sed -n 's/^captions: //p' "$out/verify-srt.log")")/"*.srt 2>/dev/null | head -1)"
  echo "-- video delay in -> UDP out"; "$lat" report "$out/in.csv" "$out/out.csv" --skip 10 2>&1
  echo "-- warnings/errors in s6.log: $(grep -cE ' (WARN|ERROR) ' "$out/s6.log")"
  grep ' stats' "$out/s6.log" | sed -n '1p;$p'
} >"$out/summary.txt"
if [[ -f $ts && -x $ccx ]]; then
  "$ccx" "$ts" -o "$out/ccx-cc1.srt" >"$out/ccx-cc1.log" 2>&1
  "$ccx" "$ts" --output-field 2 -o "$out/ccx-cc3.srt" >"$out/ccx-cc3.log" 2>&1
  "$ccx" "$ts" --service 1,2,3,4 -o "$out/ccx.srt" >"$out/ccx-708.log" 2>&1
  for f in "$out"/ccx*.srt; do echo "-- $(basename "$f"): $(grep -c -- '-->' "$f") cues" >>"$out/summary.txt"; done
fi
off=$(sed -n 's/^pts offset: \(-\?[0-9]*\) ticks.*/\1/p' <("$lat" report "$out/in.csv" "$out/out.csv" --skip 10 2>&1))
origin=$(python3 -c "print(1.421333 + ${off:-0} / 90000)")
# verify.sh's SRT gives a roll-up row the start time of its first word, so
# words streamed into a row look early. ffmpeg's -real_time 1 decoder emits a
# cue per change (true appear times; end times fixed by rtfix.py).
ffmpeg -hide_banner -loglevel fatal -y -copyts -real_time 1 -f lavfi -i "movie=$out/capture.ts[out+subcc]" \
  -map 0:1 -c:s srt "$out/cc1-rt-raw.srt"
python3 "$here/rtfix.py" "$out/cc1-rt-raw.srt" "$out/cc1-rt.srt"
refw=$spikes/../docs/m0/evidence/S4/long.words.tsv
lagc() { "$lat" captions --srt "$1" --segments "$MULTI_MEDIA/long.segments.tsv" --words "$refw" \
  --wav "$MULTI_MEDIA/long.wav" --pts-origin "$origin" --csv "$2" 2>&1 | tail -2; }
shift_s=$(python3 -c "print($(grep -o 'new input session.*in_pts=[0-9]*' "$out/s6.log" | head -1 | sed 's/.*in_pts=//')/1e9 - $(sed -n 2p "$out/in.csv" | cut -d, -f2)/90000)")
{
  echo "-- EN caption lag, real-time cues (pts-origin $origin)"; lagc "$out/cc1-rt.srt" "$out/lag-en.csv"
  echo "-- EN caption lag, verify.sh cues (biased early for streamed words)"; lagc "$out/cc1.srt" "$out/lag-en-verify.csv"
  echo "-- ASR word timestamps (tsdemux shift $shift_s s)"; python3 "$here/tsacc.py" "$out/words.tsv" "$refw" "$shift_s"
  echo "-- translated lag: EN clause push -> translation push (first 10 s skipped)"
  python3 "$here/lag.py" "$out/emit.tsv" 10
} >>"$out/summary.txt"
cat "$out/summary.txt"
