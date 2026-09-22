#!/usr/bin/env bash
# Extracts embedded CEA-608 captions (CC1) and optionally checks them.
#
#   verify.sh INPUT [EXPECTED.txt] [--duration SECONDS]
#
# INPUT is a file or a stream URL (a stream is recorded for --duration seconds,
# default 20). Writes the captions as SRT next to the recording and prints the
# caption text. With EXPECTED.txt, every non-empty line of it must appear in
# the captions (case and spacing ignored); exits 1 if any line is missing.
#
# Only CEA-608 is decoded here (FFmpeg's decoder). CEA-708 needs ccextractor.
set -euo pipefail

input=${1:?usage: verify.sh INPUT [EXPECTED.txt] [--duration SECONDS]}
shift
expected=""
duration=20
while [[ $# -gt 0 ]]; do
  case $1 in
    --duration) duration=$2; shift 2 ;;
    *) expected=$1; shift ;;
  esac
done

work=$(mktemp -d)
if [[ -f $input ]]; then
  file=$input
else
  file=$work/capture.ts
  echo "recording $duration s from $input"
  ffmpeg -hide_banner -loglevel error -y -i "$input" -t "$duration" -c copy -f mpegts "$file"
fi

srt=$work/captions.srt
ffmpeg -hide_banner -loglevel error -y -f lavfi -i "movie=$(printf '%q' "$file")[out+subcc]" -map 0:1 -c:s srt "$srt"
echo "captions: $srt"

text=$work/captions.txt
grep -vE '^[0-9]+$|-->|^\s*$' "$srt" | tr -s ' ' >"$text" || true
if [[ ! -s $text ]]; then
  echo "FAIL: no captions found" >&2
  exit 1
fi
cat "$text"

[[ -z $expected ]] && exit 0

norm() { tr '[:lower:]' '[:upper:]' | tr -s '[:space:]' ' ' | sed 's/^ //; s/ $//'; }
all=$(norm <"$text")
missing=0
while IFS= read -r line; do
  want=$(norm <<<"$line")
  [[ -z $want ]] && continue
  if [[ $all != *"$want"* ]]; then
    echo "MISSING: $line" >&2
    missing=1
  fi
done <"$expected"
[[ $missing == 0 ]] && echo "PASS: all expected lines present"
exit $missing
