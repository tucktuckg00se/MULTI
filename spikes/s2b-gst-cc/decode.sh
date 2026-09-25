#!/usr/bin/env bash
# Decodes gst.ts / ours.ts in DIR with FFmpeg (608 fields 1/2), ccextractor
# (CC1, CC3, 708 services 1-4) and libcaption ts2srt (CC1, H.264 only).
# Writes *.srt next to them and prints each cue as "start | row / row / ...".
#   decode.sh DIR [gst|ours ...]
set -uo pipefail
dir=$1; shift
names=("$@"); [[ ${#names[@]} -eq 0 ]] && names=(gst ours)
ccx=${CCEXTRACTOR:-$HOME/.cache/multi-tools/ccx-build/ccextractor}
ts2srt=$HOME/.cache/multi-tools/libcaption/build/examples/ts2srt
cues() { # SRT -> one line per cue
  tr -d "\r" <"$1" | awk 'BEGIN{RS="";FS="\n"} {t=$2; sub(/ -->.*/,"",t); s=""; for(i=3;i<=NF;i++){l=$i; gsub(/\r/,"",l); s=s (i>3?" / ":"") l}; print t" | "s}'
}
for n in "${names[@]}"; do
  ts=$dir/$n.ts; [[ -f $ts ]] || continue
  for f in first second; do
    ffmpeg -hide_banner -loglevel error -y -data_field $f -f lavfi -i "movie=$ts[out+subcc]" -map 0:1 -c:s srt "$dir/$n-ffmpeg-$f.srt" 2>"$dir/$n-ffmpeg-$f.err"
  done
  "$ccx" "$ts" --output-field 1 -o "$dir/$n-ccx-cc1.srt" >/dev/null 2>&1
  "$ccx" "$ts" --output-field 2 -o "$dir/$n-ccx-cc3.srt" >/dev/null 2>&1
  "$ccx" "$ts" --service 1,2,3,4 -o "$dir/$n-ccx-708.srt" >/dev/null 2>&1
  if [[ $ts != *hevc* ]]; then "$ts2srt" "$ts" >"$dir/$n-libcaption.srt" 2>/dev/null; fi
  for s in "$dir/$n"-ffmpeg-first.srt "$dir/$n"-ffmpeg-second.srt "$dir/$n"-ccx-cc1.srt "$dir/$n"-ccx-cc3.srt "$dir/$n"-ccx-708*svc*.srt "$dir/$n"-libcaption.srt; do
    [[ -s $s ]] || continue
    echo "== $(basename "$s")"
    cues "$s"
  done
done
