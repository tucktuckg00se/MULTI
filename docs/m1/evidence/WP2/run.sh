#!/usr/bin/env bash
# WP2 real-model check: source.sh (long.wav) -> latency tap -> multi run (real
# multi-asr + multi-mt, CUDA) -> latency tap -> capture; then CC1 (verify.sh),
# CC3 (ffmpeg -data_field second), video delay and EN caption lag (S6 method).
#
#   run.sh OUT_DIR [SECONDS]
#
# Needs: MULTI_MEDIA, BIN_DIR (multi, multi-asr, multi-mt side by side),
# LATENCY (spikes' harness `latency` binary). Ports 9730-9734.
# Run under `flock /tmp/multi-gpu.lock`.
set -uo pipefail
out=${1:?out dir}; secs=${2:-180}
here=$(cd "$(dirname "$0")" && pwd); root=$(cd "$here/../../../.." && pwd)
spikes=$root/spikes
: "${MULTI_MEDIA:?set MULTI_MEDIA}" "${BIN_DIR:?set BIN_DIR}" "${LATENCY:?set LATENCY}"
mkdir -p "$out"; out=$(cd "$out" && pwd)
pids=()
cleanup() { for p in "${pids[@]}"; do kill "$p" 2>/dev/null; done; wait 2>/dev/null; }
trap cleanup EXIT

cat >"$out/multi.toml" <<EOF
[input]
url = "udp://127.0.0.1:9731"

[[outputs]]
url = "udp://127.0.0.1:9732"

[[outputs]]
url = "srt://127.0.0.1:9734?mode=listener"
EOF

"$LATENCY" tap --listen 127.0.0.1:9730 --forward 127.0.0.1:9731 --out "$out/in.csv" 2>"$out/tap-in.log" & pids+=($!)
"$LATENCY" tap --listen 127.0.0.1:9732 --forward 127.0.0.1:9733 --out "$out/out.csv" 2>"$out/tap-out.log" & pids+=($!)
RUST_LOG=info "$BIN_DIR/multi" run --config "$out/multi.toml" >"$out/multi.log" 2>&1 & multi=$!; pids+=($multi)
for _ in $(seq 120); do
  [[ $(grep -c 'worker ready' "$out/multi.log") -ge 2 ]] && break; sleep 0.5
done
grep 'worker ready' "$out/multi.log"
"$spikes/harness/source.sh" "udp://127.0.0.1:9730?pkt_size=1316" >"$out/source.log" 2>&1 & pids+=($!)
sleep 5
# Record the UDP output byte-exact (PTS untouched), then extract from the file.
timeout -s INT "$secs" gst-launch-1.0 -eq udpsrc port=9733 buffer-size=8388608 ! filesink location="$out/capture.ts" 2>/dev/null
kill -INT $multi; wait $multi; echo "multi exit: $?" >"$out/exit.txt"
cleanup; pids=()

ts=$out/capture.ts
"$spikes/harness/verify.sh" "$ts" --srt-out "$out/cc1.srt" >"$out/verify-udp.log" 2>&1
ffmpeg -hide_banner -loglevel fatal -y -data_field second -f lavfi -i "movie=$ts[out+subcc]" -map 0:1 -c:s srt "$out/cc3.srt"
report=$("$LATENCY" report "$out/in.csv" "$out/out.csv" --skip 10 2>&1)
off=$(sed -n 's/^pts offset: \(-\?[0-9]*\) ticks.*/\1/p' <<<"$report")
origin=$(python3 -c "print(1.421333 + ${off:-0} / 90000)")
# ffmpeg's -real_time 1 decoder emits a cue per change (true appear times);
# rtfix.py repairs its end times (S6 gotcha 1).
ffmpeg -hide_banner -loglevel fatal -y -copyts -real_time 1 -f lavfi -i "movie=$ts[out+subcc]" \
  -map 0:1 -c:s srt "$out/cc1-rt-raw.srt"
python3 "$spikes/s6-e2e/rtfix.py" "$out/cc1-rt-raw.srt" "$out/cc1-rt.srt"
{
  echo "== WP2 real workers, ${secs}s capture"
  echo "-- multi: $(cat "$out/exit.txt"); warnings/errors in log: $(grep -cE ' (WARN|ERROR) ' "$out/multi.log")"
  grep ' stats ' "$out/multi.log" | tail -1 | sed 's/^.*stats //'
  echo "-- verify (UDP, CC1): $(grep -c -- '-->' "$out/cc1.srt") cues"
  echo "-- CC3 (ES): $(grep -c -- '-->' "$out/cc3.srt") cues; sample:"; grep -v -- '-->' "$out/cc3.srt" | grep -v '^[0-9]*$' | grep . | sed -n '10,14p'
  echo "-- video delay in -> UDP out"; echo "$report"
  echo "-- EN caption lag, real-time cues (pts-origin $origin)"
  "$LATENCY" captions --srt "$out/cc1-rt.srt" --segments "$MULTI_MEDIA/long.segments.tsv" \
    --words "$root/docs/m0/evidence/S4/long.words.tsv" --wav "$MULTI_MEDIA/long.wav" \
    --pts-origin "$origin" --csv "$out/lag-en.csv" 2>&1 | tail -2
} >"$out/summary.txt"
cat "$out/summary.txt"
