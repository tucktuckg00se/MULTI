S5 evidence (live translation). Everything here is small text; model files and
FLORES-200 are not committed (spikes/s5-translate/fetch-refs.sh fetches FLORES).

models.txt       every model: source URL, exact revision, file, size, licence
results.csv      one row per run x language: chrF++ / COMET, latency P50/P95/max per
                 language and per whole request (all languages), TTFT, tok/s,
                 load time, VRAM, capped outputs, failure-mode counts
                 run names: <system>.<set>[.<mode>][+asr|+asr30]
                   set: flores = FLORES-200 devtest first 500 sentences (real refs)
                        clauses = 300 caption clauses (pseudo-refs, see below)
                   +asr   = with Whisper large-v3-turbo decoding 30 s windows back
                            to back on the same GPU (saturated, worst case)
failures.csv     every clause output flagged by the heuristics in score.py
spotcheck.txt    24 clauses side by side for 6 systems and 4 languages (+zh ref)
summaries.jsonl  raw per-run summaries printed by the binary (load, VRAM, RSS)
asr-load.txt     Whisper throughput alone vs next to each translator
                 CPU-only rows: opus.cpu, m2m418.cpu, hymt18-q4.cpu (no chrF: same
                 models as the GPU rows)
env.txt          versions: driver, CUDA, crates, llama.cpp, CTranslate2

Pseudo-references for the caption clauses were made by gemma-4-26B-A4B-it
(UD-Q4_K_M, same llama.cpp) with 2 previous clauses as context. Scores against
them favour the Gemma family and are relative only; FLORES rows use human refs.

Reproduce (repo root, GPU lock taken per phase by bench.sh):
  spikes/s5-translate/build.sh build
  spikes/s5-translate/fetch-refs.sh
  spikes/s5-translate/bench.sh refs quality zh latency modes context split cpu
  LOAD=1 spikes/s5-translate/bench.sh latency
  python3 spikes/s5-translate/score.py spikes/harness/media/s5/runs docs/m0/evidence/S5
  (COMET via --comet was not run: scope cut; chrF++ only.)
