#!/usr/bin/env bash
# Source-restart test: the source is killed at 15 s and restarted (PTS back at
# its start value) at 20 s; the pipeline must stay up and the output stay valid.
#
#   reconnect.sh MODE(udp|srt-caller|srt-listener) CAPTIONS OUTDIR [BASEPORT=9260] [CODEC=h264]
#
# Output is tapped (latency tap) and recorded; afterwards: pipeline log
# summary, PTS/continuity across the gap, ffmpeg decode errors, verify.sh.
set -uo pipefail
here=$(cd "$(dirname "$0")" && pwd)
spikes=$(cd "$here/.." && pwd)
bin=${S2_BIN:-$spikes/target/release/s2-gst-pipe}
lat=${LATENCY_BIN:-/home/tucker/Documents/claude-projects/MULTI/spikes/target/release/latency}
mode=$1 caps=$2 out=$3 base=${4:-9260} codec=${5:-h264}
mkdir -p "$out"; printf "%s\n" "HELLO FROM MULTI" "CAPTION FIXTURE LINE TWO" "ROLL UP TEST 123" >"$out/expected.txt"
pids=()
cleanup() { for p in "${pids[@]}"; do kill "$p" 2>/dev/null; done; wait 2>/dev/null; }
trap cleanup EXIT
P() { echo $((base + $1)); }
case $mode in
  udp) inuri="udp://127.0.0.1:$(P 1)"; srcuri="udp://127.0.0.1:$(P 1)?pkt_size=1316" ;;
  srt-caller) inuri="srt://127.0.0.1:$(P 1)?mode=caller&latency=120"; srcuri="srt://127.0.0.1:$(P 1)?mode=listener&latency=120000" ;;
  srt-listener) inuri="srt://:$(P 1)?mode=listener&latency=120"; srcuri="srt://127.0.0.1:$(P 1)?mode=caller&latency=120000" ;;
esac
"$lat" tap --listen 127.0.0.1:$(P 10) --forward 127.0.0.1:$(P 11) --out "$out/out.csv" 2>"$out/tap-out.log" & pids+=($!)
gst-launch-1.0 -q udpsrc port=$(P 11) buffer-size=8388608 ! filesink location="$out/capture.ts" & pids+=($!)
RUST_LOG=info "$bin" run --input "$inuri" --codec "$codec" --captions "$caps" --output "udp://127.0.0.1:$(P 10)" \
  --csv "$out/delay.csv" --stats "$out/stats.csv" --stats-every-s 1 --duration-s 42 >"$out/pipe.log" 2>&1 & pp=$!; pids+=($pp)
sleep 1
start=$(date +%s.%N)
"$spikes/harness/source.sh" --codec "$codec" "$srcuri" >"$out/source1.log" 2>&1 & s1=$!
sleep 15; kill $s1; echo "t=15 s: source killed" >"$out/events.txt"
sleep 5
"$spikes/harness/source.sh" --codec "$codec" "$srcuri" >"$out/source2.log" 2>&1 & s2=$!; pids+=($s2)
echo "t=20 s: source restarted (PTS from its start again)" >>"$out/events.txt"
sleep 18
kill $s2 2>/dev/null
wait $pp; echo "pipeline exit: $?" >>"$out/events.txt"
sleep 1
cleanup; pids=()
{
  echo "== reconnect $mode captions=$caps codec=$codec"
  cat "$out/events.txt"
  echo "-- pipeline warnings/errors/sessions"
  sed 's/\x1b\[[0-9;]*m//g' "$out/pipe.log" | grep -E "WARN|ERROR|new input session" | cut -c1-260 | grep -v "Could not get/set settings" | head -30
  echo "-- stats (uptime_s,rss_kb,video_in,video_out,audio_in,sessions,input_restarts,input_errors,output_errors)"
  awk -F, 'NR==1||NR%5==0{print $2","$3","$4","$5","$6","$7","$8","$9","$10}' "$out/stats.csv"
  echo "-- output tap stderr (last lines)"; tail -4 "$out/tap-out.log"
  echo "-- output PTS/wall gaps > 100 ms"
  python3 - "$out/out.csv" <<'PY'
import csv, sys
r = list(csv.DictReader(open(sys.argv[1])))
for a, b in zip(r, r[1:]):
    dw = (int(b["wallclock_ns"]) - int(a["wallclock_ns"])) / 1e6
    dp = (int(b["pts_90k"]) - int(a["pts_90k"])) / 90
    if dw > 100 or abs(dp - 33.33) > 1:
        print(f"frame {a['frame_seq']}->{b['frame_seq']}: wall gap {dw:.0f} ms, PTS step {dp:.1f} ms")
print(f"frames {len(r)}")
PY
  echo "-- decode check (ffprobe: video packets vs decoded frames, main-decoder errors)"
  np=$(ffprobe -v quiet -select_streams v -show_entries packet=pts -of csv=p=0 "$out/capture.ts" | wc -l)
  nf=$(ffprobe -v error -select_streams v -show_entries frame=pts -of csv=p=0 "$out/capture.ts" 2>"$out/ffprobe-err.txt" | wc -l)
  echo "packets=$np decoded_frames=$nf ffprobe_error_lines=$(grep -vc 'Last message' "$out/ffprobe-err.txt")"
  sort "$out/ffprobe-err.txt" | uniq -c | sort -rn | head -4
  echo "-- verify.sh (on the recording from 3 s on; see finding for the file-start probe quirk)"
  ffmpeg -v error -y -ss 3 -i "$out/capture.ts" -c copy "$out/capture-from3s.ts"
  "$spikes/harness/verify.sh" "$out/capture-from3s.ts" "$out/expected.txt" 2>&1 | tail -2
} >"$out/summary.txt" 2>&1
cat "$out/summary.txt"
