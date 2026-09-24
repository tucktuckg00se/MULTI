#!/usr/bin/env bash
# Soak: source -> tap -> SRT (gst bridge) -> s2-gst-pipe (captions ours) -> UDP out + SRT out -> taps.
#   soak.sh SECONDS OUTDIR [BASEPORT=9280] [CAPTIONS=ours]
# Stats every 30 s (RSS, counters), per-frame delay CSV, then a 20 s verify.sh sample.
set -uo pipefail
here=$(cd "$(dirname "$0")" && pwd); spikes=$(cd "$here/.." && pwd)
bin=${S2_BIN:-$spikes/target/release/s2-gst-pipe}
lat=${LATENCY_BIN:-/home/tucker/Documents/claude-projects/MULTI/spikes/target/release/latency}
secs=$1 out=$2 base=${3:-9280} caps=${4:-ours}
P() { echo $((base + $1)); }
mkdir -p "$out"; pids=()
cleanup() { for p in "${pids[@]}"; do kill "$p" 2>/dev/null; done; wait 2>/dev/null; }
trap cleanup EXIT
"$lat" tap --listen 127.0.0.1:$(P 10) --forward 127.0.0.1:$(P 11) --out "$out/out-udp.csv" --quiet 2>"$out/tap-out-udp.log" & pids+=($!)
"$lat" tap --listen 127.0.0.1:$(P 13) --out "$out/out-srt.csv" --quiet 2>"$out/tap-out-srt.log" & pids+=($!)
"$lat" tap --listen 127.0.0.1:$(P 0) --forward 127.0.0.1:$(P 1) --out "$out/in.csv" --quiet 2>"$out/tap-in.log" & pids+=($!)
gst-launch-1.0 -q udpsrc port=$(P 1) buffer-size=8388608 ! srtsink "uri=srt://:$(P 2)?mode=listener&latency=120" sync=false wait-for-connection=false 2>"$out/bridge-in.log" & pids+=($!)
RUST_LOG=info "$bin" run --input "srt://127.0.0.1:$(P 2)?mode=caller&latency=120" --captions "$caps" \
  --output "udp://127.0.0.1:$(P 10)" --output "srt://:$(P 12)?mode=listener&latency=120" \
  --csv "$out/delay.csv" --stats "$out/stats.csv" --stats-every-s 30 --duration-s $((secs + 5)) >"$out/pipe.log" 2>&1 & pp=$!; pids+=($pp)
sleep 0.5
gst-launch-1.0 -q srtsrc "uri=srt://127.0.0.1:$(P 12)?mode=caller&latency=120" ! udpsink host=127.0.0.1 port=$(P 13) sync=false 2>"$out/bridge-out.log" & pids+=($!)
sleep 0.5
"$spikes/harness/source.sh" "udp://127.0.0.1:$(P 0)?pkt_size=1316" >"$out/source.log" 2>&1 & pids+=($!)
sleep $((secs - 25))
printf '%s\n' "HELLO FROM MULTI" "CAPTION FIXTURE LINE TWO" "ROLL UP TEST 123" >"$out/expected.txt"
"$spikes/harness/verify.sh" "udp://127.0.0.1:$(P 11)" "$out/expected.txt" --duration 20 >"$out/verify-end.log" 2>&1
wait $pp; echo "pipeline exit $?" >"$out/exit.txt"
