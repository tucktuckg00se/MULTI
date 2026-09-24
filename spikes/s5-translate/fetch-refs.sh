#!/bin/bash
# Fetches FLORES-200 devtest (CC-BY-SA-4.0, used for evaluation only; not
# committed) and writes s5 input TSVs into spikes/harness/media/s5/:
#   flores.en.tsv            id doc kind text   (1012 English source sentences)
#   flores.ref.<lang>.txt    one reference per line, same order
set -euo pipefail
here=$(cd "$(dirname "$0")" && pwd)
out=${S5_MEDIA:-$here/../harness/media/s5}
mkdir -p "$out"
cd "$out"
url=https://dl.fbaipublicfiles.com/nllb/flores200_dataset.tar.gz
[ -f flores200_dataset.tar.gz ] || curl -sSfLo flores200_dataset.tar.gz "$url"
sha256sum flores200_dataset.tar.gz
tar xzf flores200_dataset.tar.gz ./flores200_dataset/devtest
d=flores200_dataset/devtest
{ printf 'id\tdoc\tkind\ttext\n'; awk '{printf "%d\tflores\tflores\t%s\n", NR, $0}' $d/eng_Latn.devtest; } >flores.en.tsv
for p in es:spa_Latn fr:fra_Latn de:deu_Latn pt:por_Latn zh:zho_Hans; do
  cp "$d/${p#*:}.devtest" "flores.ref.${p%%:*}.txt"
done
wc -l flores.en.tsv flores.ref.*.txt
