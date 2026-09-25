# S5: live clause translation with small local models

> **Summary:** Yes, easily. CTranslate2 + opus-mt (one model per pair) translates a clause into ES/FR/DE/PT in parallel in **P95 34 ms**, or 61 ms next to a saturating Whisper load. Its chrF matches the small LLMs (FLORES 62.6 vs 57–65), it needs 1.6 GB VRAM, and it runs on CPU (P95 347 ms). Recommended: opus-mt with sentence splitting. Best small LLM: Hy-MT2-1.8B Q4 (Apache-2.0), P95 189 ms, 513 ms under full ASR load.

## Candidates and licences

Revisions and URLs are in [models.txt](../evidence/S5/models.txt).

- opus-mt (Apache-2.0; en-de and tc-big-en-pt are **CC-BY-4.0**, attribution required).
- M2M-100 418M/1.2B (MIT).
- MADLAD-400 3B (Apache-2.0; needs bf16, fp16 gives garbage).
- Hy-MT2 1.8B/7B (Apache-2.0; unlike HY-MT1.5, no EU/UK/KR exclusion).
- Qwen3.5 2B/4B, Gemma 4 E2B/E4B, EuroLLM 1.7B (Apache-2.0).
- TranslateGemma 4B: **Gemma Terms, custom and gated; not shippable by default**.
- NLLB, Seamless, Tower and Aya are non-commercial and were not tested.

Both crates (llama-cpp-2 0.1.157, ct2rs 0.10.1) build from source against **CUDA 13.4** with no patches. GPU use was confirmed per run from VRAM.

## Results

Setup: 300 caption clauses, 4 languages in parallel, latency per whole request in ms.

- **+load:** Whisper large-v3-turbo (CT2 fp16, beam 5) on the same GPU decoding 30 s windows back to back, GPU at 99%. This is harsher than streaming ASR.
- **chrF++:** average over ES/FR/DE/PT.
- **FLORES:** devtest, first 500 sentences, with human references.
- **Clause chrF:** scored against gemma-4-26B pseudo-references, which favour the Gemma rows.
- **LLMs:** 4 sequences decoded in one llama.cpp batch.

| System | P50 / P95 | +load P50 / P95 | FLORES | Clause | VRAM MiB |
|---|---|---|---|---|---|
| **opus-mt fp16** (4 threads, split) | **21 / 34** | **37 / 61** | 62.6 | 64.2 | 1574 |
| M2M-100 418M | 40 / 63 | 70 / 116 | 56.4 | 53.3 | 1358 |
| MADLAD-400 3B bf16 | 144 / 231 | 268 / 414 | **64.9** | 69.0 | 6706 |
| **Hy-MT2 1.8B Q4_K_M** | 123 / 189 | 327 / 513 | 61.4 | 62.7 | 1874 |
| Hy-MT2 1.8B Q8_0 | 135 / 212 | 348 / 572 | 60.4 | 61.7 | 2612 |
| Hy-MT2 7B Q4_K_M | 270 / 418 | 551 / 871 | 64.5 | 70.7 | 5480 |
| Qwen3.5 4B Q4_K_M | 231 / 350 | 495 / 759 | 62.2 | 67.2 | 3740 |
| Gemma 4 E4B Q4_K_M | 227 / 340 | 525 / 808 | 64.6 | 73.4 | 3986 |

More rows (M2M 1.2B, Qwen 2B, Gemma E2B, EuroLLM, TranslateGemma, and per-language P50/P95/max/TTFT) are in [results.csv](../evidence/S5/results.csv).

- **Load time:** 0.5–1.4 s (MADLAD 7 s).
- **Hy-MT2-1.8B:** time to first token is about 16 ms. Q8 was never better than Q4.
- **Parallel modes, Hy-MT2-1.8B P95:** one batch 189 ms, 4 contexts on 4 threads 284 ms, sequential 408 ms. One prompt for all four languages fails because Hy-MT2 ignores the output format (chrF 1.8).
- **Cost to ASR:** Whisper throughput alone is 120× real time. It drops to 85× next to Hy-MT2-1.8B and to 64–76× next to the CTranslate2 models.
- **CPU only:** opus-mt int8 gives P95 347 ms for all four languages, near real time. Hy-MT2-1.8B takes 1.2 s.
- **Chinese (quality only):** Qwen3.5-4B 31.3, Hy-MT2-1.8B 28.2, opus-mt 21.3.

## Quality notes

Sources: the [spot-check](../evidence/S5/spotcheck.txt) (24 clauses × 6 systems) and [failures.csv](../evidence/S5/failures.csv).

- **opus-mt:**
  - Drops later sentences in a clause: "Cold, is it? Bless your sweet face." keeps only the question. Splitting at sentence ends (`--split`) fixes this at no latency cost.
  - Translates idioms literally.
  - Keeps fragments as fragments.
- **LLMs:**
  - EuroLLM, Qwen 2B and TranslateGemma continue unfinished fragments into whole sentences.
  - "Translate this sentence into German." makes Hy-MT2-7B, Qwen3.5-4B and Gemma 4 write German in the ES track.
  - EuroLLM adds commentary and leaves some lines untranslated.
  - Hy-MT2-1.8B had no flagged failures but gets rare words wrong.
- **M2M-418M:** loops on filler words ("oui, oui, …") until it hits the length cap.
- **Profanity:** opus-mt and Hy-MT2 soften it; Gemma and MADLAD keep it. The FL-4 filter must run after translation.
- **Names and spoken numbers:** names and times are mostly kept, but spoken numbers break ("one twelve to one oh four" → "12:1").
- **Runaway control:**
  - Output is capped at min(3×source tokens + 16, 200) tokens plus a 2 s deadline.
  - Only 3 of all clause outputs hit a cap.
  - A wrong chat template (Hy-MT2-7B's older Hunyuan format) caused 241 runaway outputs before it was fixed, so every model's template must be tested.

## Recommendation

- **Backend:** CTranslate2 + opus-mt per pair, fp16 on GPU and int8 on CPU as the fallback. Use greedy decoding, split sentences, and set `max_decoding_length = 4×words + 16`. It cannot follow instructions in the text. Record the CC-BY attribution in the registry.
- **Optional quality tier:** MADLAD-3B for Latin targets. Hy-MT2-1.8B Q4 or Qwen3.5-4B for zh/ar WebVTT.
- **`translate.segment` = `clause`.** Translation takes under 100 ms, so the clause boundary sets the lag.
- **`translate.max_wait_ms` = 800** (unchanged).
- **Own process:** one supervised worker per backend, with languages as threads.
  - The worker reads JSON lines `{id, text, langs}` and writes `{id, lang, text, ms}`.
  - The parent enforces the deadline, restarts the worker on crash or hang, and shows source-only captions meanwhile.
  - `spikes/s5-translate` already has this shape.

## Open items (cut for scope)

- COMET scoring.
- Previous-clause context test.
- 30-minute soak for memory growth.
- Realistic streaming-ASR load (this spike used a saturating one).
- Arabic.
- Two bench mode runs that failed on a flag clash (opus sequential, M2M threads).
