#!/usr/bin/env bash
# Downloads test speech into media/ (gitignored).
#
# LibriSpeech test-clean (CC BY 4.0, openslr.org/12): read English speech with
# exact transcripts. We also build one long-form file by joining a chapter's
# utterances, with its transcript, for live-stream style tests.
#
# Set MULTI_MEDIA to use a media folder elsewhere (e.g. from a git worktree).
#
# Output:
#   media/LibriSpeech/test-clean/...        original corpus
#   media/long.wav, media/long.txt          ~10 min of one speaker, 16 kHz mono
#   media/long.segments.tsv                 utterance start/end seconds + text
set -euo pipefail
cd "$(dirname "$0")"
media=${MULTI_MEDIA:-media}
mkdir -p "$media"
cd "$media"

if [[ ! -d LibriSpeech/test-clean ]]; then
  curl -fL --retry 3 -o test-clean.tar.gz https://www.openslr.org/resources/12/test-clean.tar.gz
  tar xzf test-clean.tar.gz
  rm test-clean.tar.gz
fi

# Pick the chapter with the most audio and join it into long.wav.
chapter=$(for d in LibriSpeech/test-clean/*/*/; do
  printf '%s %s\n' "$(du -sb "$d" | cut -f1)" "$d"
done | sort -rn | head -1 | cut -d' ' -f2)
echo "long-form source: $chapter"

list=$(mktemp)
trap 'rm -f "$list"' EXIT
: >long.segments.tsv
: >long.txt
t=0
while read -r id text; do
  f="$chapter/$id.flac"
  d=$(ffprobe -v error -show_entries format=duration -of csv=p=0 "$f")
  printf "file '%s'\n" "$PWD/$f" >>"$list"
  printf '%s\t%s\t%s\n' "$t" "$(awk -v a="$t" -v b="$d" 'BEGIN{printf "%.3f", a+b}')" "$text" >>long.segments.tsv
  echo "$text" >>long.txt
  t=$(awk -v a="$t" -v b="$d" 'BEGIN{printf "%.3f", a+b}')
done < <(cat "$chapter"/*.trans.txt)

ffmpeg -v error -y -f concat -safe 0 -i "$list" -ar 16000 -ac 1 long.wav
echo "media/long.wav: $(printf '%.0f' "$t") s, $(wc -l <long.txt) utterances"
