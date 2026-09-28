#!/usr/bin/env bash
# M1 4-hour soak: source.sh (long.wav looping) -> tap -> multi run (real workers) -> tap -> recorder.
# Kills the ASR worker at 1 h and the MT worker at 2 h. Usage: soak.sh OUT_DIR [SECONDS]
set -u
D=$1; T=${2:-14400}; R=$(cd "$(dirname "$0")/../../../.." && pwd)
B=$R/target/release; L=$R/spikes/target/release/latency
export MULTI_MEDIA=$R/spikes/harness/media
mkdir -p "$D"
# Run under setsid so everything started here shares one process group; on exit
# or termination, stop multi cleanly, then the whole group (taps, source, timers).
cleanup() {
  trap - EXIT INT TERM
  [ -n "${MP:-}" ] && kill -INT "$MP" 2>/dev/null && sleep 5
  # Stop the rest of the group, but not this shell (exit status stays meaningful).
  trap '' TERM; kill -TERM -- -$$ 2>/dev/null
}
trap cleanup EXIT INT TERM
$B/multi config default | sed -e 's#^url = "srt://0.0.0.0:9000?mode=listener"#url = "udp://127.0.0.1:9801"#' \
  -e 's#^url = "srt://0.0.0.0:9001?mode=listener"#url = "udp://127.0.0.1:9802?pkt_size=1316"#' > "$D/soak.toml"
$L tap --quiet --listen 127.0.0.1:9800 --forward 127.0.0.1:9801 --out "$D/in.csv" --duration "$T" &
$L tap --quiet --listen 127.0.0.1:9802 --forward 127.0.0.1:9803 --out "$D/out.csv" --duration "$T" &
$B/multi run -c "$D/soak.toml" --models-dir "$HOME/.cache/multi-models" > "$D/multi.log" 2>&1 &
MP=$!
sleep 15
timeout "$T" "$R/spikes/harness/source.sh" 'udp://127.0.0.1:9800?pkt_size=1316' > "$D/source.log" 2>&1 &
# Record byte-exact with GStreamer. Do NOT remux with ffmpeg: a -copyts remux
# shifts ffmpeg's real_time caption cue times by +1.4 s (video PTS unchanged),
# which inflates measured caption lag (found in the 2026-09-28 soak).
record() { timeout --foreground -s INT 600 gst-launch-1.0 -eq udpsrc port=9803 buffer-size=8388608 ! filesink location="$D/$1.ts"; }
( sleep 30; record first ) &
[ "$T" -gt 1300 ] && ( sleep $((T - 660)); record last ) &
[ "$T" -gt 3700 ] && ( sleep 3600; echo "$(date +%s) kill asr pid=$(pgrep -f "$B/multi-asr" | head -1)" >> "$D/events.log"; pkill -9 -f "$B/multi-asr" ) &
[ "$T" -gt 7300 ] && ( sleep 7200; echo "$(date +%s) kill mt pid=$(pgrep -f "$B/multi-mt" | head -1)" >> "$D/events.log"; pkill -9 -f "$B/multi-mt" ) &
echo "t_s,rss_multi_mb,rss_asr_mb,rss_mt_mb,gpu_mem_mb" > "$D/resources.csv"
start=$(date +%s)
while kill -0 $MP 2>/dev/null && [ $(( $(date +%s) - start )) -lt "$T" ]; do
  rss() { ps -o rss= -p $(pgrep -f "$1" | head -1) 2>/dev/null | awk '{printf "%d", $1/1024}'; }
  echo "$(( $(date +%s) - start )),$(rss "$B/multi run"),$(rss "$B/multi-asr"),$(rss "$B/multi-mt"),$(nvidia-smi --query-gpu=memory.used --format=csv,noheader,nounits | head -1)" >> "$D/resources.csv"
  sleep 60
done
kill -INT $MP; wait $MP; echo "multi exit=$?" >> "$D/events.log"; MP=
