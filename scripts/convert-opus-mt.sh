#!/usr/bin/env bash
# Converts a Hugging Face opus-mt model to CTranslate2 (float16).
#   convert-opus-mt.sh <hf-repo> <revision> <out-dir>
# `multi models pull` runs this for registry entries with `convert` (it embeds
# a copy, so installed binaries do not need the source tree). The Python venv
# lives in $MULTI_VENV (multi sets <models>/.venv; default: next to out-dir).
# $MULTI_PYTHON picks the interpreter (default python3).
set -euo pipefail

if [ $# -ne 3 ]; then
  echo "usage: $0 <hf-repo> <revision> <out-dir>" >&2
  exit 2
fi
repo=$1 rev=$2 out=$3
venv=${MULTI_VENV:-$(dirname "$out")/.venv}
py=${MULTI_PYTHON:-python3}

if ! command -v "$py" >/dev/null 2>&1; then
  echo "error: $py not found; converting opus-mt models needs Python 3.10+ with venv (e.g. apt install python3-venv)" >&2
  exit 3
fi

# Pinned tool versions (S5 used the same ctranslate2 and transformers).
pkgs=(ctranslate2==4.8.2 transformers==4.57.6 sentencepiece==0.2.2 sacremoses==0.2.0)
torch=torch==2.14.0

if [ ! -x "$venv/bin/ct2-transformers-converter" ]; then
  echo "setting up the conversion environment in $venv (one-off, ~1 GB)" >&2
  "$py" -m venv "$venv"
  "$venv/bin/pip" install --quiet --upgrade pip
  "$venv/bin/pip" install --quiet --index-url https://download.pytorch.org/whl/cpu "$torch"
  "$venv/bin/pip" install --quiet "${pkgs[@]}"
fi

rm -rf "$out"
"$venv/bin/ct2-transformers-converter" --model "$repo" --revision "$rev" \
  --output_dir "$out" --quantization float16 --copy_files source.spm target.spm
