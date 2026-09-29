# M2-5 results: live vs offline WER, Nemotron vs Whisper

> **Summary:** The dropped-word fix brings live Nemotron WER within 0.3 points of an offline decode (ES 10.2 → 6.8, DE 16.3 → 10.3, EN 6.7 → 4.8). At 1× on an RTX 3090, Whisper large-v3-turbo is more accurate for ES and DE but lags about twice as much. Neither engine produced any words on S4's silence, noise and music clip.

Setup: `multi-asr` release with CUDA, fed by `feed.py` (20 ms PCM frames over the worker protocol, 1 s gaps between clips, 1 s lead-in), run under `flock /tmp/multi-gpu.lock`. WER: `wer.py` (M2-2 normaliser). References: MLS `refs.tsv` in `~/.cache/multi-testdata/mls/{es,de}` and `long.txt`. Offline: the spike's `s4-asr batch`, one stream per clip.

## Dropped short words (Nemotron 560 ms fp32; fast feed, same text as 1×)

| WER % | Offline | Live, stream closed at VAD end | Live, 1500 ms hangover |
|---|---|---|---|
| ES (735 words) | 6.5 | 10.2 | 6.8 |
| DE (682 words) | 10.6 | 16.3 | 10.3 |
| EN `long.wav` (1341 words) | 4.5 | 6.7 | 4.8 |

Hangover sweep, ES/EN: 0 ms 10.2/6.7 · 500 ms 7.3/4.9 · 1500 ms 6.8/4.8 · 3000 ms 6.7/–.

## Nemotron vs Whisper at 1× real time

| Engine | Lang | WER % | Word lag p50/p95 s | Utterance lag p50/p95 s | Peak VRAM |
|---|---|---|---|---|---|
| Nemotron 560 ms fp32 | EN | 4.8 | 0.60 / 1.07 | 0.93 / 1.15 (max 1.33) | 3.6 GB |
| Nemotron 560 ms fp32 | ES | 6.8 | 0.60 / 1.07 | 0.64 / 1.50 | 3.6 GB |
| Nemotron 560 ms fp32 | DE | 10.3 | 0.60 / 1.00 | 0.39 / 1.17 | 3.6 GB |
| Whisper turbo, 1000 ms, LA-2 | EN | 4.9 | 1.30 / 2.36 | 1.08 / 2.87 (max 3.42) | 2.1 GB |
| Whisper turbo, 1000 ms, LA-2 | ES | 5.4 | 1.38 / 2.49 | 0.30 / 0.64 | 2.1 GB |
| Whisper turbo, 1000 ms, LA-2 | DE | 7.3 | 1.56 / 2.67 | 0.31 / 0.70 | 2.1 GB |

- **Word lag:** the time a word was emitted minus the end time the engine reported for it.
- **Utterance lag, EN:** the last word of each of the 61 reference segments, measured from its forced-aligned end (`lag.py`, `long.words.tsv`); comparable to S4's lag column.
- **Utterance lag, ES/DE:** the last word of each clip, measured from the clip's end. MLS clips end right after the speech, so the VAD's end-of-speech commit dominates. That is why Whisper looks low here.
- Offline Whisper turbo WER (R1): ES 5.2, DE 6.2.
- A first 1× run used a buffered stdout pipe in `feed.py`, which added about 2 s to every lag. Those numbers were discarded; the text and WER were the same.

## Hallucination check

`nospeech.wav` from S4 (60 s silence, 60 s pink noise, 280 s of orchestral and piano music; recipe in `docs/m0/evidence/S4/models.txt`), 1×:

| Engine | Words emitted | S4 |
|---|---|---|
| Nemotron 560 ms + VAD | 0 | 0 |
| Whisper turbo + VAD + guard | 0 | 27 ("Amen. Thank you." ×13) |

Guard totals on that clip: 26 segments dropped for low confidence, 11 as filler, 0 for no-speech probability. On the speech runs the guard dropped 3 to 6 segments per language, and WER was unchanged against the guard without the interjection rule.

Files: `{nm,wt}-{en,es,de,junk}.tsv` (emission time, start_ms, end_ms, word) with `.clips.tsv` timelines; `feed.py`, `wer.py`, `lag.py`.
