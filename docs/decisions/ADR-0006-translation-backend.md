# ADR-0006: Translate with CTranslate2 + opus-mt; small LLMs only as an optional tier

> **Summary:** Default translation is CTranslate2 running opus-mt (one model per language pair), fp16 on GPU and int8 on CPU. Four languages in parallel take P95 34 ms (61 ms with ASR saturating the GPU) at quality equal to small LLMs, which were 5–10× slower and obeyed instructions hidden in the speech.

- **Status:** accepted
- **Date:** 2026-09-24
- **Evidence:** [S5 finding](../m0/findings/S5-translation.md); raw data in `docs/m0/evidence/S5/`

## Measured (300 caption clauses, ES/FR/DE/PT in parallel)

| System | P50 / P95 ms | + ASR load P95 | FLORES chrF | VRAM MiB |
|---|---|---|---|---|
| opus-mt fp16 (default) | 21 / 34 | 61 | 62.6 | 1574 |
| MADLAD-400 3B | 144 / 231 | 414 | 64.9 | 6706 |
| Hy-MT2 1.8B Q4 | 123 / 189 | 513 | 61.4 | 1874 |
| Gemma 4 E4B Q4 | 227 / 340 | 808 | 64.6 | 3986 |

## Decision

- Default backend: CTranslate2 + opus-mt, greedy decoding, split clauses into sentences first (opus-mt drops later sentences otherwise), cap output length and a 2 s deadline.
- Optional quality tier: MADLAD-400 3B for Latin-script targets; a small LLM (Hy-MT2 1.8B or Qwen3.5 4B) only for non-Latin WebVTT tracks, with its chat template tested per model.
- `translate.segment` = clause; `translate.max_wait_ms` = 800.
- The word filter (FL-4) runs **after** translation, since models treat profanity inconsistently.
- Translation runs in its own supervised worker (JSON lines in/out); on failure, captions fall back to the source language.

## Open

Context from previous clauses, a 30-minute soak, and COMET scoring were not run. Two opus-mt pairs (en-de, tc-big-en-pt) are CC-BY-4.0 and need attribution.
