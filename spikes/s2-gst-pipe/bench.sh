#!/usr/bin/env bash
# One S2 measurement run: source -> tap -> [UDP|SRT] -> s2-gst-pipe -> UDP out + SRT out -> taps.
#
#   bench.sh CODEC(h264|hevc) INPUT(udp|srt) CAPTIONS(none|gst|ours) SECONDS OUTDIR [BASEPORT] [-- extra run args]
#
# Ports BASE..BASE+14 (default 9220). SRT legs use latency=120 ms and are
# bridged to UDP with gst-launch (udpsrc ! srtsink / srtsrc ! udpsink, no
# demux) so the harness `latency` tap can see them; bench.sh calib measures
# that bridge pair alone. Needs MULTI_MEDIA.
set -uo pipefail
here=$(cd "$(dirname "$0")" && pwd)
spikes=$(cd "$here/.." && pwd)
bin=${S2_BIN:-$spikes/target/release/s2-gst-pipe}
lat=${LATENCY_BIN:-/home/tucker/Documents/claude-projects/MULTI/spikes/target/release/latency}
codec=$1 input=$2 caps=$3 secs=$4 out=$5 base=${6:-9220}
shift 6 2>/dev/null || shift $#
[[ ${1:-} == -- ]] && shift
extra=("$@")
mkdir -p "$out"
pids=()
cleanup() { for p in "${pids[@]}"; do kill "$p" 2>/dev/null; done; wait 2>/dev/null; }
trap cleanup EXIT
srtl=120
p() { echo $((base + $1)); }

# Taps on the output side first.
"$lat" tap --listen 127.0.0.1:$(p 10) --forward 127.0.0.1:$(p 11) --out "$out/out-udp.csv" --quiet 2>"$out/tap-out-udp.log" & pids+=($!)
"$lat" tap --listen 127.0.0.1:$(p 13) --forward 127.0.0.1:$(p 14) --out "$out/out-srt.csv" --quiet 2>"$out/tap-out-srt.log" & pids+=($!)
"$lat" tap --listen 127.0.0.1:$(p 0) --forward 127.0.0.1:$(p 1) --out "$out/in.csv" --quiet 2>"$out/tap-in.log" & pids+=($!)

if [[ $input == srt ]]; then
  gst-launch-1.0 -q udpsrc port=$(p 1) buffer-size=8388608 ! srtsink "uri=srt://:$(p 2)?mode=listener&latency=$srtl" sync=false wait-for-connection=false 2>"$out/bridge-in.log" & pids+=($!)
  inuri="srt://127.0.0.1:$(p 2)?mode=caller&latency=$srtl"
else
  inuri="udp://127.0.0.1:$(p 1)"
fi
if [[ $caps == calib ]]; then
  # Bridge pair only: udp -> srt listener -> srt caller -> udp, no pipeline.
  gst-launch-1.0 -q udpsrc port=$(p 1) buffer-size=8388608 ! srtsink "uri=srt://:$(p 12)?mode=listener&latency=$srtl" sync=false wait-for-connection=false 2>"$out/bridge-in.log" & pids+=($!)
else
  RUST_LOG=info "$bin" run --input "$inuri" --codec "$codec" --captions "$caps" \
    --output "udp://127.0.0.1:$(p 10)" --output "srt://:$(p 12)?mode=listener&latency=$srtl" \
    --csv "$out/delay.csv" --stats "$out/stats.csv" --stats-every-s 5 --duration-s $((secs + 3)) "${extra[@]}" \
    >"$out/pipe.log" 2>&1 & pids+=($!)
fi
sleep 0.5
gst-launch-1.0 -q srtsrc "uri=srt://127.0.0.1:$(p 12)?mode=caller&latency=$srtl" ! udpsink host=127.0.0.1 port=$(p 13) sync=false 2>"$out/bridge-out.log" & pids+=($!)
sleep 0.5
"$spikes/harness/source.sh" --codec "$codec" "udp://127.0.0.1:$(p 0)?pkt_size=1316" >"$out/source.log" 2>&1 & pids+=($!)

exp=$out/expected.txt
printf '%s\n' "HELLO FROM MULTI" "CAPTION FIXTURE LINE TWO" "ROLL UP TEST 123" >"$exp"
if [[ $caps != none && $caps != calib ]]; then
  sleep 4
  "$spikes/harness/verify.sh" "udp://127.0.0.1:$(p 11)" "$exp" --duration $((secs - 8)) >"$out/verify-udp.log" 2>&1 &
  v1=$!
  "$spikes/harness/verify.sh" "udp://127.0.0.1:$(p 14)" "$exp" --duration $((secs - 8)) >"$out/verify-srt.log" 2>&1 &
  v2=$!
  wait $v1; echo "verify udp: exit $?" >>"$out/summary.txt"
  wait $v2; echo "verify srt: exit $?" >>"$out/summary.txt"
  sleep 4
else
  sleep "$secs"
fi
cleanup
pids=()
{
  echo "== $codec in=$input captions=$caps ${extra[*]}"
  echo "-- black box: in -> UDP out"; "$lat" report "$out/in.csv" "$out/out-udp.csv" --skip 10 2>&1
  echo "-- black box: in -> SRT out (via srtsrc!udpsink bridge)"; "$lat" report "$out/in.csv" "$out/out-srt.csv" --skip 10 2>&1
  if [[ -f $out/delay.csv ]]; then
    echo "-- internal (tsdemux out -> mpegtsmux in), after 10 s"
    python3 "$here/pct.py" "$out/delay.csv" 10
  fi
  [[ -f $out/stats.csv ]] && { echo "-- last stats"; tail -1 "$out/stats.csv"; }
} >>"$out/summary.txt"
for v in udp srt; do
  f=$out/verify-$v.log
  [[ -f $f ]] || continue
  cap=$(sed -n 's/^captions: //p' "$f"); ts=$(dirname "$cap")/capture.ts
  [[ -f $ts ]] || continue
  cp "$ts" "$out/capture-$v.ts"
done
cat "$out/summary.txt"
