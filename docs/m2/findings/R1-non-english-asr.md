# R1: Non-English source speech

> **Summary:** Nemotron 3.5 Streaming is "transcription-ready" for ES, FR, DE and PT. On about 5 min per language of MLS read speech it scored WER ES 6.5, FR 9.9, DE 10.4, PT 6.5, against 5.4 on English in M0. Whisper large-v3-turbo (LocalAgreement-2) did better on ES, FR and DE (5.2 / 8.1 / 6.2) and worse on PT (8.8), with about 3 s more lag. opus-mt has direct pairs for every X→EN and EN→X we need except PT→EN and EN→JA.

## Languages (model card)

- **Transcription-ready (19 locales):** en es fr it pt nl de tr ru ar hi ja ko vi uk (plus variants).
- **Broad-coverage (13):** pl sv cs nb da bg fi hr sk zh hu ro et.
- **Adaptation-ready (8, need fine-tuning):** el lt lv mt sl he th nn.
- The card's FLEURS WER at the 1.12 s chunk with a language prompt: ES 4.11, FR 9.03, DE 8.31, PT 5.48.
- The card says MLS and FLEURS are both in the training data, so scores on either set may be optimistic.

Source: https://huggingface.co/nvidia/nemotron-3.5-asr-streaming-0.6b (OpenMDW-1.1)

## Measurement

Evidence: [asr-wer-mls.tsv](../evidence/R1/asr-wer-mls.tsv) (includes utterance IDs)

- **Data:** MLS test split (CC-BY-4.0), 20–21 utterances per language (≈305 s each) from 4–12 speakers. Stored in `~/.cache/multi-testdata/mls`.
- **Tool:** the M0 `s4-asr batch` engine (chunked streaming, same sherpa and whisper paths as S4), run under `flock`, RTX 3090. Nemotron's cache-aware decoding gives the same output whatever the feed rate, so batch WER = 1× WER. Lag comes from M0 S4.
- **WER:** Whisper `BasicTextNormalizer` (case and punctuation stripped).

| Lang | Nemotron 560 ms `--lang xx` | Nemotron `auto` | Whisper turbo LA-2 |
|---|---|---|---|
| ES | 6.5 | 6.9 | **5.2** |
| FR | 9.9 | 9.9 | **8.1** |
| DE | 10.4 | 11.1 | **6.2** |
| PT | **6.5** | 6.9 | 8.8 |
| EN (M0 S4, other set) | 5.4 | — | 3.3 |
| RTF | 0.024 | 0.024 | 0.113 |

Caveats:
- With about 700–900 reference words per language, the numbers are ±~2 points.
- MLS is read audiobook speech, not live talk.
- DE Nemotron errors are mostly compound-word substitutions.
- An explicit language prompt beats `auto` slightly, so the source language should be set, not detected.

## opus-mt pairs (Helsinki-NLP)

Evidence: [opus-mt-pairs.txt](../evidence/R1/opus-mt-pairs.txt)

- **X→EN:** es, fr, de, zh, ar, ja, ru and hi all have direct pairs. PT→EN has none; use `opus-mt-ROMANCE-en` or `opus-mt-mul-en` (Apache-2.0).
- **EN→X:** es, fr, de, pt (`tc-big-en-pt`), zh, ar, ru and hi exist. **EN→JA has no usable model**: `en-jap` is Bible-only.
- **Among ES/FR/DE:** all six directions exist (Apache-2.0).
- **PT↔ES/FR:** only `opus-mt-tc-big-itc-itc` (multi-target).
- **PT↔DE:** none.
- **Licences:** Apache-2.0 or CC-BY-4.0 throughout, so they are commercial-OK. CC-BY needs attribution.

## Recommendation per source language

| Source | ASR | Translation |
|---|---|---|
| ES | Nemotron (default, low lag); Whisper as accuracy option | Direct to EN/FR/DE; to PT via EN |
| FR | Nemotron; Whisper option | Direct to EN/ES/DE; to PT via EN |
| DE | **Whisper recommended** if its lag is acceptable (10.4 vs 6.2 WER); otherwise Nemotron | Direct to EN/ES/FR; to PT via EN |
| PT | Nemotron | Via EN (ROMANCE-en, then EN→X). No direct PT→X pairs |
| Other transcription-ready (it nl ru ar hi ja…) | Nemotron (untested here) | Via EN |

**Rule:** use the direct pair when one exists and scores at least as well as the pivot; otherwise pivot through English. A pivot adds one MT hop. Always ask for the source language explicitly.
