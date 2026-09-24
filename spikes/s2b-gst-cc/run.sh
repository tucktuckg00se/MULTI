#!/usr/bin/env bash
# Renders and decodes scenarios: run.sh NAME... -> ${S2B_OUT:-/tmp/s2b}/NAME/{render.log,decoded.txt,gst-cc.txt,...}
set -uo pipefail
here=$(cd "$(dirname "$0")" && pwd)
bin=$here/../target/release/s2b-gst-cc
for n in "$@"; do
  out=${S2B_OUT:-/tmp/s2b}/$n
  rm -rf "$out"; mkdir -p "$out"
  "$bin" render "$here/scenarios/$n.json" --out "$out" >/dev/null 2>"$out/stderr.txt" || { echo "$n: render failed"; cat "$out/stderr.txt"; continue; }
  "$here/decode.sh" "$out" >"$out/decoded.txt"
  echo "== $n"; head -1 "$out/render.log"
done
