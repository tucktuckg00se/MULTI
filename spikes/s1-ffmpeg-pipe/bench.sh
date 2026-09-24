#!/usr/bin/env bash
# One S1 latency/validation run. Ports 9101-9112 only.
#
#   bench.sh CODEC(h264|hevc) IN(udp|srt) SECONDS OUTDIR
#
# source.sh -> latency tap :9101 -> :9102
#   IN=udp: pipe reads udp://:9102
#   IN=srt: srt-live-transmit :9102 -> SRT listener :9110 (latency 120 ms) -> pipe caller
# pipe outputs: udp :9103 (latency tap), udp :9104 (verify.sh),
#               srt listener :9111 (srt-live-transmit -> :9105 tap), srt listener :9112 (verify.sh)
set -uo pipefail
codec=$1 in=$2 secs=$3 out=$4
here=$(cd "$(dirname "$0")" && pwd)
bin=$here/../target/release/s1-ffmpeg-pipe
harness=$here/../harness
export MULTI_MEDIA=${MULTI_MEDIA:-$harness/media}
mkdir -p "$out"
lat=120000 # SRT latency, microseconds (FFmpeg libsrt unit)
pids=()

lat_bin=$here/../target/release/latency
"$lat_bin" tap --quiet --listen 127.0.0.1:9101 --forward 127.0.0.1:9102 --out "$out/tap-in.csv" --duration $((secs + 12)) &
pids+=($!)
if [[ $in == srt ]]; then
  srt-live-transmit -q 'udp://127.0.0.1:9102' "srt://127.0.0.1:9110?mode=listener&latency=$((lat / 1000))" &
  pids+=($!)
  input="srt://127.0.0.1:9110?mode=caller&latency=$lat"
else
  input="udp://127.0.0.1:9102?fifo_size=50000&overrun_nonfatal=1"
fi
"$bin" run --input "$input" \
  --output 'udp://127.0.0.1:9103?pkt_size=1316' \
  --output 'udp://127.0.0.1:9104?pkt_size=1316' \
  --output "srt://127.0.0.1:9111?mode=listener&latency=$lat&pkt_size=1316" \
  --output "srt://127.0.0.1:9112?mode=listener&latency=$lat&pkt_size=1316" \
  ${PIPE_ARGS:-} --csv-dir "$out" --duration $((secs + 8)) >"$out/pipe.log" 2>&1 &
pipe=$!
sleep 0.5
"$lat_bin" tap --quiet --listen 127.0.0.1:9103 --out "$out/tap-out-udp.csv" --duration $((secs + 6)) &
pids+=($!)
# SRT leg bridged back to UDP: srt-live-transmit caller -> udp :9105 -> tap.
"$lat_bin" tap --quiet --listen 127.0.0.1:9105 --out "$out/tap-out-srt.csv" --duration $((secs + 6)) &
pids+=($!)
srt-live-transmit -q "srt://127.0.0.1:9111?mode=caller&latency=$((lat / 1000))" 'udp://127.0.0.1:9105' &
pids+=($!)
timeout $((secs + 2)) "$harness/source.sh" --codec "$codec" 'udp://127.0.0.1:9101?pkt_size=1316' >"$out/source.log" 2>&1 &
src=$!
sleep 8
timeout $((secs + 20)) "$harness/verify.sh" 'udp://127.0.0.1:9104' "$here/expected.txt" --duration 20 >"$out/verify-udp.txt" 2>&1 &
v1=$!
timeout $((secs + 20)) "$harness/verify.sh" "srt://127.0.0.1:9112?mode=caller&latency=$lat" "$here/expected.txt" --duration 20 >"$out/verify-srt.txt" 2>&1 &
v2=$!
wait $v1 $v2 $src
wait $pipe
kill "${pids[@]}" 2>/dev/null
wait 2>/dev/null
for f in "$out"/out[0-3].csv; do "$bin" stats "$f" --skip 30; done
echo "== black-box, UDP out"; "$lat_bin" report "$out/tap-in.csv" "$out/tap-out-udp.csv" --skip 10
echo "== black-box, SRT out (includes srt-live-transmit bridge)"; "$lat_bin" report "$out/tap-in.csv" "$out/tap-out-srt.csv" --skip 10
tail -n1 "$out/verify-udp.txt" "$out/verify-srt.txt"
