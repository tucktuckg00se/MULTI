#!/usr/bin/env bash
# B-frame test: H.264 with N B-frames -> multi run (fake workers) -> record -> CC1 word order.
set -u
bf=$1; D=$2; R=/home/tucker/Documents/claude-projects/MULTI
F=$R/target/release/multi-fake-worker
cfg=$D/bf$bf.toml
$R/target/release/multi config default | sed -e 's#^url = "srt://0.0.0.0:9000?mode=listener"#url = "udp://127.0.0.1:9790"#' -e 's#^url = "srt://0.0.0.0:9001?mode=listener"#url = "udp://127.0.0.1:9791?pkt_size=1316"#' > $cfg
$R/target/release/multi run -c $cfg --asr-worker "$F asr" --mt-worker "$F mt" > $D/run-bf$bf.log 2>&1 &
mp=$!
sleep 3
ffmpeg -hide_banner -loglevel error -y -i 'udp://127.0.0.1:9791?fifo_size=100000&overrun_nonfatal=1' -t 25 -c copy -f mpegts $D/out-bf$bf.ts &
rp=$!
timeout 32 ffmpeg -hide_banner -loglevel error -re -f lavfi -i "testsrc2=size=1280x720:rate=30" -stream_loop -1 -i $R/spikes/harness/media/long.wav \
  -map 0:v -map 1:a -c:v libx264 -preset veryfast -bf $bf -g 30 -keyint_min 30 -sc_threshold 0 -b:v 3M -pix_fmt yuv420p \
  -c:a aac -b:a 128k -ar 48000 -ac 2 -f mpegts 'udp://127.0.0.1:9790?pkt_size=1316'
wait $rp; kill -INT $mp; wait $mp
echo "== bf=$bf  has_b_frames=$(ffprobe -v error -select_streams v -show_entries stream=has_b_frames -of csv=p=0 $D/out-bf$bf.ts)  exit_of_multi_logged_errors=$(grep -c ERROR $D/run-bf$bf.log)"
ffmpeg -hide_banner -loglevel error -y -f lavfi -i "movie=$D/out-bf$bf.ts[out+subcc]" -map 0:1 -c:s srt $D/cc1-bf$bf.srt
python3 - "$D/cc1-bf$bf.srt" <<'PY'
import re,sys
S=["alpha","bravo","charlie","delta","echo","foxtrot"]
words=[w for w in re.findall(r"[a-z]+", open(sys.argv[1]).read().lower()) if w in S]
# collapse roll-up repeats: keep a word only when it differs from the previous one
seq=[w for i,w in enumerate(words) if i==0 or w!=words[i-1]]
bad=sum(1 for a,b in zip(seq,seq[1:]) if S.index(b)!=(S.index(a)+1)%6)
print(f"CC1 words={len(seq)} out_of_order_steps={bad}  first: {' '.join(seq[:8])}")
PY
