#!/usr/bin/env bash
# S1 soak: steady H.264 source through the pipe for SECONDS, sampling RSS.
# Ports 9121-9126.   soak.sh SECONDS OUTDIR
set -uo pipefail
secs=$1 out=$2
here=$(cd "$(dirname "$0")" && pwd)
harness=$here/../harness
export MULTI_MEDIA=${MULTI_MEDIA:-$harness/media}
mkdir -p "$out"
bin=$out/s1-ffmpeg-pipe
cp "$here/../target/release/s1-ffmpeg-pipe" "$bin"
lat_bin=$here/../target/release/latency
pids=()
"$lat_bin" tap --quiet --listen 127.0.0.1:9121 --forward 127.0.0.1:9122 --out "$out/tap-in.csv" --duration $((secs + 20)) & pids+=($!)
"$bin" run --input 'udp://127.0.0.1:9122?fifo_size=50000&overrun_nonfatal=1' \
  --output 'udp://127.0.0.1:9123?pkt_size=1316' \
  --output 'srt://127.0.0.1:9124?mode=listener&latency=120000&pkt_size=1316' \
  --output 'udp://127.0.0.1:9126?pkt_size=1316' \
  --csv-dir "$out" --duration $((secs + 10)) >"$out/pipe.log" 2>&1 &
pipe=$!
sleep 0.5
"$lat_bin" tap --quiet --listen 127.0.0.1:9123 --out "$out/tap-out-udp.csv" --duration $((secs + 15)) & pids+=($!)
"$lat_bin" tap --quiet --listen 127.0.0.1:9125 --out "$out/tap-out-srt.csv" --duration $((secs + 15)) & pids+=($!)
srt-live-transmit -q 'srt://127.0.0.1:9124?mode=caller&latency=120' 'udp://127.0.0.1:9125' & pids+=($!)
timeout $((secs + 5)) "$harness/source.sh" 'udp://127.0.0.1:9121?pkt_size=1316' >"$out/source.log" 2>&1 &
src=$!
echo "t_s,rss_kib,threads,fds" >"$out/rss.csv"
t0=$(date +%s)
while kill -0 $pipe 2>/dev/null; do
  now=$(( $(date +%s) - t0 ))
  rss=$(awk '/VmRSS/{print $2}' /proc/$pipe/status 2>/dev/null)
  thr=$(awk '/Threads/{print $2}' /proc/$pipe/status 2>/dev/null)
  fds=$(ls /proc/$pipe/fd 2>/dev/null | wc -l)
  [[ -n $rss ]] && echo "$now,$rss,$thr,$fds" >>"$out/rss.csv"
  if [[ $now -ge $((secs - 40)) && ! -f $out/verify.txt ]]; then
    timeout 60 "$harness/verify.sh" 'udp://127.0.0.1:9126' "$here/expected.txt" --duration 20 >"$out/verify.txt" 2>&1 &
  fi
  sleep 10
done
wait $src 2>/dev/null
kill "${pids[@]}" 2>/dev/null
wait 2>/dev/null
rm -f "$bin"
"$lat_bin" report "$out/tap-in.csv" "$out/tap-out-udp.csv" --skip 10 >"$out/report-udp.txt" 2>&1
"$lat_bin" report "$out/tap-in.csv" "$out/tap-out-srt.csv" --skip 10 >"$out/report-srt.txt" 2>&1
