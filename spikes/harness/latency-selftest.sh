#!/usr/bin/env bash
# Self-test for the `latency` tool: runs known pipelines and reports what the tool measures.
#
#   latency-selftest.sh [OUT_DIR] [--seconds N] [--only stage,stage]
#
# Stages (ports 9400-9499 only):
#   tap2tap   source -> tap -> tap                         (tap overhead as seen downstream)
#   relay     source -> tap -> ffmpeg -c copy relay -> tap -> ffmpeg null sink
#   relay0    same relay with -max_interleave_delta 0 -flush_packets 1
#   delay250  source -> tap -> `latency delay --ms 250` -> tap (known delay)
#   offset    source -> tap -> relay with -output_ts_offset 10 -> tap (PTS shift detection)
#   relay-mid1   relay with -max_interleave_delta 1 (don't wait for audio to interleave)
#   relay-vonly  relay of the video stream only (-map 0:v)
#   captions  synthetic SRT = each utterance's text at segment end + 2.0 s (known lag)
# Each report skips the first 10 s (relay/probe warm-up).
set -euo pipefail
here=$(cd "$(dirname "$0")" && pwd)
out=${1:-/tmp/latency-selftest}
secs=40
only=""
shift || true
while [[ $# -gt 0 ]]; do
  case $1 in
    --seconds) secs=$2; shift 2 ;;
    --only) only=$2; shift 2 ;;  # comma-separated stage names
    *) echo "unknown arg $1" >&2; exit 1 ;;
  esac
done
media=${MULTI_MEDIA:-$here/media}
mkdir -p "$out"
(cd "$here/.." && cargo build --release --offline -q -p harness)
L=$here/../target/release/latency

run_stage() { # name base_port none|delay MS|ffmpeg [relay output args...]
  local name=$1 p=$2 mode=$3; shift 2
  [[ -n $only && ",$only," != *",$name,"* ]] && return 0
  local src=$p a=$((p + 1)) b=$((p + 2)) c=$((p + 3))
  echo "== $name"
  local pids=()
  "$L" tap --listen 127.0.0.1:$src --forward 127.0.0.1:$a --out "$out/$name.in.csv" --duration $((secs + 2)) --quiet 2>"$out/$name.tap-in.log" & pids+=($!)
  "$L" tap --listen 127.0.0.1:$b --forward 127.0.0.1:$c --out "$out/$name.out.csv" --duration $((secs + 2)) --quiet 2>"$out/$name.tap-out.log" & pids+=($!)
  timeout -k 2 $((secs + 3)) ffmpeg -nostdin -hide_banner -loglevel fatal -i "udp://127.0.0.1:$c?timeout=10000000" -f null - & pids+=($!)
  case $1 in
    none) "$L" tap --listen 127.0.0.1:$a --forward 127.0.0.1:$b --out "$out/$name.mid.csv" --duration $((secs + 2)) --quiet 2>"$out/$name.tap-mid.log" & pids+=($!) ;;
    delay) "$L" delay --listen 127.0.0.1:$a --forward 127.0.0.1:$b --ms "$2" --duration $((secs + 2)) 2>"$out/$name.delay.log" & pids+=($!) ;;
    ffmpeg) shift
      timeout -k 2 $((secs + 3)) ffmpeg -nostdin -hide_banner -loglevel error -i "udp://127.0.0.1:$a?timeout=10000000" -c copy "$@" -f mpegts "udp://127.0.0.1:$b?pkt_size=1316" 2>"$out/$name.relay.log" & pids+=($!) ;;
  esac
  sleep 0.5
  timeout -k 2 "$secs" "$here/source.sh" --codec h264 "udp://127.0.0.1:$src?pkt_size=1316" 2>/dev/null || true
  wait "${pids[@]}" 2>/dev/null || true
  if [[ $mode == none ]]; then
    # Tap overhead: the first tap vs the second (in -> mid).
    "$L" report "$out/$name.in.csv" "$out/$name.mid.csv" --skip 10 | tee "$out/$name.report.txt"
  else
    "$L" report "$out/$name.in.csv" "$out/$name.out.csv" --skip 10 --pairs "$out/$name.pairs.csv" | tee "$out/$name.report.txt"
  fi
  grep -h 'forward overhead\|cc_err' "$out"/$name.tap-*.log | sed 's/^/  /' | tee -a "$out/$name.report.txt"
}

run_stage tap2tap 9410 none
run_stage relay 9420 ffmpeg
run_stage relay0 9430 ffmpeg -max_interleave_delta 0 -flush_packets 1
run_stage delay250 9440 delay 250
run_stage offset 9450 ffmpeg -output_ts_offset 10
run_stage relay-mid1 9460 ffmpeg -max_interleave_delta 1
run_stage relay-vonly 9470 ffmpeg -map 0:v

[[ -n $only && ",$only," != *",captions,"* ]] && exit 0

echo "== captions (synthetic +2.0 s)"
seg=$media/long.segments.tsv
# Two loops of the audio, so loop mapping is exercised; cue i starts at end_i + 2.0 s.
loop=$(tail -1 "$seg" | cut -f2)
awk -F'\t' -v L="$loop" -v off=2.0 '
  function ts(t,  h,m,s) { h=int(t/3600); m=int((t-h*3600)/60); s=t-h*3600-m*60; return sprintf("%02d:%02d:%06.3f", h, m, s) }
  { st[NR]=$2+off; tx[NR]=$3; n=NR }
  END { k=0; for (l=0; l<2; l++) for (i=1; i<=n; i++) { a=st[i]+l*L; b=(i<n ? st[i+1] : st[1]+L)+l*L; k++;
        s1=ts(a); s2=ts(b); gsub(/\./, ",", s1); gsub(/\./, ",", s2); printf "%d\n%s --> %s\n%s\n\n", k, s1, s2, tx[i] } }' "$seg" >"$out/synthetic-plus2.srt"
"$L" captions --srt "$out/synthetic-plus2.srt" --segments "$seg" --wav "$media/long.wav" --csv "$out/captions-plus2.csv" | tee "$out/captions-plus2.txt"
# Same with ASR-style damage: in each cue the last word of 4+ letters gets its final
# letter replaced, and the 4+ letter word before it is dropped.
awk 'BEGIN{RS=""; ORS="\n\n"} { n=split($0, L, "\n"); m=split(L[3], w, " "); j=0; i=0;
       for (k=m; k>=1; k--) if (length(w[k])>=4) { if (!j) j=k; else { i=k; break } }
       if (j) w[j]=substr(w[j],1,length(w[j])-1) "X"; if (i) w[i]="";
       s=""; for (k=1;k<=m;k++) if (w[k]!="") s=s (s==""?"":" ") w[k];
       print L[1] "\n" L[2] "\n" s }' "$out/synthetic-plus2.srt" >"$out/synthetic-plus2-noisy.srt"
"$L" captions --srt "$out/synthetic-plus2-noisy.srt" --segments "$seg" --wav "$media/long.wav" --csv "$out/captions-plus2-noisy.csv" | tee "$out/captions-plus2-noisy.txt"
# --words: a word-level reference whose last word ends 0.3 s before each segment end
# (as if the segment had 0.3 s of trailing silence) must raise every lag by 0.3 s.
awk -F'\t' '{ n=split($3, w, " "); printf "%s\t%.3f\t%.3f\n", w[n], $2-0.5, $2-0.3 }' "$seg" >"$out/words-minus0.3.tsv"
"$L" captions --srt "$out/synthetic-plus2.srt" --segments "$seg" --wav "$media/long.wav" --words "$out/words-minus0.3.tsv" | tee "$out/captions-plus2-words.txt"
