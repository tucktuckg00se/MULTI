#!/usr/bin/env bash
# Calibrates the SRT bridge used by bench.sh, with no pipe in between:
# tap :9131 -> :9132 -> srt-live-transmit -> SRT :9133 (latency 120 ms)
#   -> srt-live-transmit caller -> udp :9134 -> tap.   srt-bridge-cal.sh SECONDS OUTDIR
set -uo pipefail
secs=$1 out=$2
here=$(cd "$(dirname "$0")" && pwd)
lat_bin=$here/../target/release/latency
export MULTI_MEDIA=${MULTI_MEDIA:-$here/../harness/media}
mkdir -p "$out"
"$lat_bin" tap --quiet --listen 127.0.0.1:9131 --forward 127.0.0.1:9132 --out "$out/tap-in.csv" --duration $((secs + 6)) & a=$!
"$lat_bin" tap --quiet --listen 127.0.0.1:9134 --out "$out/tap-out.csv" --duration $((secs + 6)) & b=$!
srt-live-transmit -q 'udp://127.0.0.1:9132' 'srt://127.0.0.1:9133?mode=listener&latency=120' & c=$!
sleep 0.5
srt-live-transmit -q 'srt://127.0.0.1:9133?mode=caller&latency=120' 'udp://127.0.0.1:9134' & d=$!
timeout "$secs" "$here/../harness/source.sh" 'udp://127.0.0.1:9131?pkt_size=1316' >/dev/null 2>&1
wait $a $b; kill $c $d 2>/dev/null
"$lat_bin" report "$out/tap-in.csv" "$out/tap-out.csv" --skip 10
