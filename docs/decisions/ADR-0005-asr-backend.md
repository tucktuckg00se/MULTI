# ADR-0005: Default ASR is Nemotron 3.5 Streaming via sherpa-onnx; Whisper turbo as accuracy option

> **Summary:** Default speech recognition is Nemotron 3.5 ASR Streaming 0.6B via sherpa-onnx (ONNX Runtime, CUDA 13), 560 ms chunks, Silero VAD in front. It never revised committed text, emitted 0 words on silence/noise/music, and has the tightest lag tail (P95 1.21 s). Whisper large-v3-turbo with LocalAgreement-2 is the higher-accuracy option but needs a hallucination filter.

- **Status:** accepted
- **Date:** 2026-09-24
- **Evidence:** [S4 finding](../m0/findings/S4-streaming-asr.md); raw data in `docs/m0/evidence/S4/`

## Measured (long.wav at 1× real time, VAD on)

| Setting | Lag P50 / P95 / max (s) | WER clean | WER music 5 dB | Words on no-speech input | VRAM |
|---|---|---|---|---|---|
| Nemotron 560 ms (default) | 0.84 / 1.21 / 1.33 | 5.4 | 8.5 | 0 | 3.6 GB |
| Whisper turbo 1000 ms, LA-2 | 1.13 / 2.85 / 2.88 | 3.3 | 4.4 | 27 (104 without VAD) | 2.1 GB |
| Parakeet v3 500 ms, LA-2 | 0.55 / 0.89 / 1.52 | 4.6 | – | – | 4.6 GB |

## Decision

- Default: Nemotron 3.5 Streaming, fp32 on GPU, int8 on CPU (CPU RTF 0.22, so real-time CPU fallback works). `asr.chunk_ms` = 560, `asr.stability_passes` = 2 (only used by Whisper/Parakeet).
- Reliability over accuracy: append-only output and no hallucinations matter more on live TV than 2 points of WER.
- Whisper turbo is offered as an "accuracy" preset only with a hallucination filter.
- ASR runs in a supervised child process (restart on crash or missed heartbeat), sending committed words with audio timestamps as newline-delimited JSON.

## Open before M1 sign-off

Non-English accuracy of Nemotron (not scored in S4), a 30-minute soak, and caption lag on CPU at 1×. Parakeet v3 (CC-BY-4.0) and Nemotron (OpenMDW-1.1) licences need attribution/notice handling in the model registry.
