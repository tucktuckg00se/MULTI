#!/bin/bash
# S5 benchmark queue. Every GPU run holds /tmp/multi-gpu.lock (shared with other
# spikes). Outputs go to spikes/harness/media/s5/runs/ (gitignored):
#   <label>.jsonl   one record per clause x language
#   summaries.jsonl one summary per run (latency, VRAM, load time)
#
# Usage: bench.sh PHASE...   phases: quality zh latency modes context cpu soak refs
# Env: LOAD=1 runs a concurrent Whisper ASR load (see asrload) during each run.
# Each phase takes the GPU lock once for all of its runs.
set -uo pipefail
if [ -z "${S5_LOCKED:-}" ]; then
  for phase in "$@"; do
    echo "$(date +%T) waiting for lock: $phase" >&2
    S5_LOCKED=1 flock /tmp/multi-gpu.lock "$0" "$phase" || echo "FAILED phase $phase" >&2
  done
  exit 0
fi
here=$(cd "$(dirname "$0")" && pwd)
B=$here/../target/release/s5-translate
G=${MODELS:-$HOME/.cache/multi-models}/gguf
C=${MODELS:-$HOME/.cache/multi-models}/ct2
M=$here/../harness/media/s5
R=$M/runs
CL=$here/data/clauses.tsv
FL=$M/flores.en.tsv
WAV=${WAV:-/home/tucker/Documents/claude-projects/MULTI/spikes/harness/media/long.wav}
LOCK=/tmp/multi-gpu.lock
mkdir -p "$R"

declare -A GGUF=(
  [hymt18-q4]="hymt tencent_Hy-MT2-1.8B-GGUF/Hy-MT2-1.8B-Q4_K_M.gguf"
  [hymt18-q8]="hymt tencent_Hy-MT2-1.8B-GGUF/Hy-MT2-1.8B-Q8_0.gguf"
  [hymt7-q4]="hymt tencent_Hy-MT2-7B-GGUF/Hy-MT2-7B-Q4_K_M.gguf"
  [qwen35-2b-q4]="qwen35 unsloth_Qwen3.5-2B-GGUF/Qwen3.5-2B-Q4_K_M.gguf"
  [qwen35-4b-q4]="qwen35 unsloth_Qwen3.5-4B-GGUF/Qwen3.5-4B-Q4_K_M.gguf"
  [qwen35-4b-q8]="qwen35 unsloth_Qwen3.5-4B-GGUF/Qwen3.5-4B-Q8_0.gguf"
  [gemma4-e2b-q4]="gemma4 unsloth_gemma-4-E2B-it-GGUF/gemma-4-E2B-it-Q4_K_M.gguf"
  [gemma4-e4b-q4]="gemma4 unsloth_gemma-4-E4B-it-GGUF/gemma-4-E4B-it-Q4_K_M.gguf"
  [eurollm17-q8]="eurollm mradermacher_EuroLLM-1.7B-Instruct-GGUF/EuroLLM-1.7B-Instruct.Q8_0.gguf"
  [tgemma4b-q4]="tgemma mradermacher_translategemma-4b-it-GGUF/translategemma-4b-it.Q4_K_M.gguf"
  [gemma4-26b-q4]="gemma4big unsloth_gemma-4-26B-A4B-it-GGUF/gemma-4-26B-A4B-it-UD-Q4_K_M.gguf"
)
declare -A CT2=(
  [opus]="--kind opus --model $C --mode threads"
  [m2m418]="--kind m2m --model $C/m2m100_418M --mode batch"
  [m2m12b]="--kind m2m --model $C/m2m100_1.2B --mode batch"
  [madlad3b]="--kind madlad --model $C/madlad400-3b-mt --mode batch --compute bfloat16"
)
LLMS="hymt18-q4 hymt18-q8 hymt7-q4 qwen35-2b-q4 qwen35-4b-q4 qwen35-4b-q8 gemma4-e2b-q4 gemma4-e4b-q4 eurollm17-q8 tgemma4b-q4"
MTS="opus m2m418 m2m12b madlad3b"

# gpu LABEL CMD... : run CMD under the GPU lock (with ASR load if LOAD=1).
gpu() {
  local label=$1; shift
  local load=${LOAD:-0}
  [ "$load" = 1 ] && label="$label+asr"
  echo "$(date +%T) start $label" >&2
  bash -c '
    load=$1 label=$2 R=$3 B=$4 WAV=$5 C=$6; shift 6
    if [ "$load" = 1 ]; then
      "$B" asrload --model "$C/whisper-large-v3-turbo" --wav "$WAV" --minutes 90 >"$R/$label.asr.log" 2>&1 &
      apid=$!
      # wait until Whisper has finished its first window
      for _ in $(seq 120); do grep -q "first window" "$R/$label.asr.log" 2>/dev/null && break; sleep 0.5; done
      sleep 2
    fi
    nvidia-smi --query-gpu=utilization.gpu,memory.used --format=csv,noheader >"$R/$label.gpu-before.txt"
    ( while sleep 2; do nvidia-smi --query-gpu=utilization.gpu,memory.used --format=csv,noheader; done ) >"$R/$label.gpu.txt" &
    mpid=$!
    "$@" --label "$label" --out "$R/$label.jsonl" >"$R/$label.summary.json" 2>"$R/$label.log"
    rc=$?
    kill $mpid 2>/dev/null
    [ "$load" = 1 ] && kill $apid 2>/dev/null && wait $apid 2>/dev/null
    exit $rc
  ' _ "$load" "$label" "$R" "$B" "$WAV" "$C" "$@"
  local rc=$?
  if [ $rc = 0 ]; then cat "$R/$label.summary.json" >>"$R/summaries.jsonl"; else echo "FAILED $label rc=$rc" >&2; tail -3 "$R/$label.log" >&2; fi
  echo "$(date +%T) done $label" >&2
}

llm() { # llm NAME SUFFIX ARGS...
  local n=$1 sfx=$2; shift 2
  set -- ${GGUF[$n]} "$@"
  local fam=$1 file=$2; shift 2
  gpu "$n$sfx" "$B" llm --family "$fam" --model "$G/$file" "$@"
}
mt() { # mt NAME SUFFIX ARGS...
  local n=$1 sfx=$2; shift 2
  gpu "$n$sfx" "$B" mt ${CT2[$n]} "$@"
}

for phase in "$@"; do
  case $phase in
    quality) # FLORES devtest, es/fr/de/pt, each system in its parallel mode
      for n in $LLMS; do llm $n .flores --input "$FL" --limit 500 --mode batched --warmup 1 --max-ms 5000; done
      for n in $MTS; do mt $n .flores --input "$FL" --limit 500 --warmup 1; done ;;
    zh) # non-Latin quality check
      for n in hymt18-q4 hymt7-q4 qwen35-4b-q4 gemma4-e4b-q4; do llm $n .flores-zh --input "$FL" --limit 500 --langs zh --mode single --warmup 1 --max-ms 5000; done
      mt opus .flores-zh --input "$FL" --limit 500 --langs zh --warmup 1
      mt m2m418 .flores-zh --input "$FL" --limit 500 --langs zh --warmup 1
      mt madlad3b .flores-zh --input "$FL" --limit 500 --langs zh --warmup 1 ;;
    latency) # caption clauses, 4 languages in parallel
      for n in $LLMS; do llm $n .clauses --input "$CL" --mode batched; done
      for n in $MTS; do mt $n .clauses --input "$CL"; done ;;
    modes) # parallelism strategies
      for n in hymt18-q4 qwen35-4b-q4; do
        for m in single threads oneprompt; do llm $n .clauses.$m --input "$CL" --mode $m; done
      done
      mt opus .clauses.single --input "$CL" --mode single
      mt m2m418 .clauses.threads --input "$CL" --mode threads ;;
    context)
      for n in hymt18-q4 hymt7-q4 qwen35-4b-q4 gemma4-e4b-q4; do
        for k in 1 2; do llm $n .clauses.ctx$k --input "$CL" --mode batched --context $k; done
      done ;;
    cpu) # CPU only; the lock still guards timing against other GPU/CPU-heavy runs
      gpu opus.cpu "$B" mt --kind opus --model "$C" --mode threads --cpu --threads 4 --input "$CL"
      gpu m2m418.cpu "$B" mt --kind m2m --model "$C/m2m100_418M" --mode single --cpu --threads 16 --input "$CL"
      gpu hymt18-q4.cpu "$B" llm --family hymt --model "$G/tencent_Hy-MT2-1.8B-GGUF/Hy-MT2-1.8B-Q4_K_M.gguf" --ngl 0 --threads 16 --mode batched --input "$CL" --max-ms 10000 ;;
    soak)
      n=${SOAK_MODEL:-hymt18-q4}
      llm $n .soak --input "$CL" --mode batched --minutes ${SOAK_MIN:-30} --stats-every-s 60 ;;
    refs) # pseudo-references for the caption clauses from a much larger model
      llm gemma4-26b-q4 .ref --input "$CL" --langs es,fr,de,pt,zh --mode single --context 2 --warmup 1 --max-ms 20000 ;;
    *) echo "unknown phase $phase" >&2; exit 2 ;;
  esac
done
