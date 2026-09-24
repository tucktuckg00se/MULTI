#!/usr/bin/env bash
# S4 benchmark driver. Every measurement runs under the shared GPU lock.
#
#   bench.sh live NAME [s4-asr engine args...]   # 1x real time over long.wav -> lag, RTF, VRAM
#   bench.sh wer  NAME [s4-asr engine args...]   # fast streaming: LibriSpeech subset (clean,
#                                                # pink5, music5) + noisy long.wav variants
#   bench.sh soak NAME LOOPS [args...]           # LOOPS x long.wav at 1x, resource trace
#   bench.sh junk NAME [args...]                 # silence + noise + music only: any word = junk
#   bench.sh mls  NAME LANG [args...]            # 40 MLS test utterances in LANG (es, fr, de)
#   bench.sh scotus NAME [args...]               # 10 min of a Supreme Court argument (real-world)
#
# Env: S4 (work dir, default ~/.cache/multi-tools/s4), MULTI_MEDIA, PY (venv python).
set -euo pipefail
here=$(cd "$(dirname "$0")" && pwd)
spikes=$(cd "$here/../.." && pwd)
S4=${S4:-$HOME/.cache/multi-tools/s4}
MULTI_MEDIA=${MULTI_MEDIA:-/home/tucker/Documents/claude-projects/MULTI/spikes/harness/media}
PY=${PY:-$HOME/.cache/multi-tools/s4-venv/bin/python}
B=${B:-$spikes/target/release/s4-asr}
LAT=$spikes/target/release/latency
LOCK=/tmp/multi-gpu.lock
mode=$1 name=$2; shift 2
out=$S4/runs/$name; mkdir -p "$out"

lag() { # srt -> lag json (segment ends, and word ends)
  "$LAT" captions --srt "$1" --segments "$MULTI_MEDIA/long.segments.tsv" --wav "$MULTI_MEDIA/long.wav" \
    --pts-origin 0 --csv "${1%.srt}.lag.csv" --json > "${1%.srt}.lag.json"
  "$LAT" captions --srt "$1" --segments "$MULTI_MEDIA/long.segments.tsv" --wav "$MULTI_MEDIA/long.wav" \
    --words "$S4/long.words.tsv" --pts-origin 0 --csv "${1%.srt}.lagw.csv" --json > "${1%.srt}.lagw.json"
}

case $mode in
live)
  flock "$LOCK" "$B" live --wav "$MULTI_MEDIA/long.wav" --out "$out/live" "$@" > /dev/null 2> "$out/live.log"
  lag "$out/live.srt"
  "$PY" "$here/wer.py" "$MULTI_MEDIA/long.txt" "$out/live.txt" --json > "$out/live.wer.json"
  ;;
wer)
  for v in ls ls_pink5 ls_music5; do
    flock "$LOCK" "$B" batch --list "$S4/media/$v/list.tsv" --out "$out/$v.hyp.tsv" "$@" > /dev/null 2> "$out/$v.log"
    "$PY" "$here/wer.py" "$S4/media/ls/ref.tsv" "$out/$v.hyp.tsv" --json > "$out/$v.wer.json"
  done
  for v in pink10 pink5 pink0 music10 music5 music0; do
    flock "$LOCK" "$B" live --fast --wav "$S4/media/long_$v.wav" --out "$out/long_$v" "$@" > /dev/null 2> "$out/long_$v.log"
    "$PY" "$here/wer.py" "$MULTI_MEDIA/long.txt" "$out/long_$v.txt" --json > "$out/long_$v.wer.json"
  done
  ;;
soak)
  loops=$1; shift
  flock "$LOCK" "$B" live --loops "$loops" --wav "$MULTI_MEDIA/long.wav" --out "$out/soak" "$@" > /dev/null 2> "$out/soak.log"
  ;;
junk)
  flock "$LOCK" "$B" live --fast --wav "$S4/media/nospeech.wav" --out "$out/junk" "$@" > /dev/null 2> "$out/junk.log"
  ;;
mls)
  lang=$1; shift
  flock "$LOCK" "$B" batch --list "$S4/media/mls_$lang/list.tsv" --out "$out/mls_$lang.hyp.tsv" "$@" > /dev/null 2> "$out/mls_$lang.log"
  "$PY" "$here/wer.py" "$S4/media/mls_$lang/ref.tsv" "$out/mls_$lang.hyp.tsv" --lang "$lang" --json > "$out/mls_$lang.wer.json"
  ;;
scotus)
  flock "$LOCK" "$B" live --fast --wav "$S4/media/scotus/clip.wav" --out "$out/scotus" "$@" > /dev/null 2> "$out/scotus.log"
  "$PY" "$here/wer.py" "$S4/media/scotus/ref.txt" "$out/scotus.txt" --json > "$out/scotus.wer.json"
  ;;
*) echo "unknown mode $mode" >&2; exit 2 ;;
esac
echo "$mode $name done"
