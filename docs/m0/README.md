# M0 — Spikes

> **Summary:** Throwaway Rust experiments that answer the questions the architecture depends on: pipeline base, caption insertion, streaming ASR, translation latency. Each spike ends in a finding; the findings feed ADRs. Exit: live captions from an SRT feed visible in VLC, with measured latency.

## Spikes

| ID | Question | Crate | Status | Finding |
|---|---|---|---|---|
| S1 | Can FFmpeg libraries pass video through and inject caption SEI without re-encode? | `spikes/s1-ffmpeg-pipe` | done | [S1](findings/S1-ffmpeg-pipeline.md) |
| S2 | Same, with GStreamer | `spikes/s2-gst-pipe` | done | [S2](findings/S2-gstreamer-pipeline.md) |
| S3 | Pure-Rust CEA-608/708 encoder + SEI builder that real decoders accept | `spikes/cc` | done | [S3](findings/S3-caption-encoder.md) |
| S4 | Which streaming ASR gives the best lag/accuracy/VRAM trade-off? | `spikes/s4-asr` | done | [S4](findings/S4-streaming-asr.md) |
| S5 | Can small local models translate a clause in ~500 ms? (stretch) | `spikes/s5-translate` | done | [S5](findings/S5-translation.md) |
| S2b | Can GStreamer's own 608/708 encoders give per-line control with no added delay? | `spikes/s2-gst-pipe` | done | [S2b](findings/S2b-gstreamer-caption-encoders.md) |
| S6 | End to end: OBS mic → SRT → captions → VLC and YouTube | `spikes/s6-e2e` | not started | — |

Status values: not started, in progress, done, dropped.

## Test harness

Everything in `spikes/harness/`. Run from the repo root.

| Tool | What it does |
|---|---|
| `fetch-media.sh` | Downloads public-domain test speech + reference transcripts into gitignored `media/` |
| `source.sh` | Plays a test stream (test pattern, burned-in timecode, speech) out over SRT or UDP, H.264 or HEVC |
| `verify.sh` | Extracts embedded captions from a stream or file and diffs them against expected text |
| `latency` (Rust bin) | Measures video pass-through delay (UDP taps) and caption lag. Usage and method: [H-latency-tool](findings/H-latency-tool.md) |
| `latency-selftest.sh` | Reproduces the latency tool's self-test numbers |

## Log

Newest first. One line per notable event, with a link if there's more.

- 2026-09-24 — S4 done: Nemotron 3.5 Streaming (sherpa-onnx) lag P95 1.21 s, WER 5.4, 0 words on silence/music; Whisper turbo more accurate (3.3) but hallucinates over music. ADR-0005 and ADR-0006 accepted.
- 2026-09-24 — S5 done: CTranslate2 + opus-mt translates 4 languages in parallel at P95 34 ms (61 ms with ASR saturating the GPU), chrF 62.6 on FLORES; LLMs 5–10× slower and follow instructions hidden in the text. CPU int8 fallback P95 347 ms.

- 2026-09-22 — S3 done: pure-Rust 608/708 encoder decodes exactly in FFmpeg, ccextractor and libcaption (H.264 + HEVC, CC1–CC4, 708 services 1–6). Found and fixed a repeated-special-character bug. Open: B-frame ordering, real players, no P16 for non-Latin scripts.
- 2026-09-22 — Latency tool done and self-tested: tap overhead 0.02 ms, known 250 ms delay read as 250.035 ms, 2.0 s caption offset read exactly. Note: source PTS starts at 1.421 s, not 0.
- 2026-09-23 — S2 done: GStreamer pass-through adds 33 ms (one frame) with our encoder, 100 ms with GStreamer's own caption elements. 60-min soak clean (0 errors, RSS flat 25.6 MB). Our `cc` encoder beats GStreamer's on control, delay and dependencies.
- 2026-09-24 — S2b done: GStreamer's tttocea708 lanes + cea708mux into h26xccinserter add no delay (33.59 vs 33.56 ms) and decode exactly in 4 languages. ADR-0003 now uses GStreamer's encoders; our cc crate becomes test oracle + fallback. Two upstream bugs found.
- 2026-09-24 — Scope: burn-in moves to a separate companion tool; Teletext lowered to P2. ADR-0004 revised to recommend GStreamer (caption tooling); ADR-0003 encoder choice pending S2b.
- 2026-09-23 — ADR-0003 accepted (own caption encoder). ADR-0004 proposed: FFmpeg libraries as pipeline base (tie on latency and soak; FFmpeg wins on timing control and shipping).
- 2026-09-23 — S1 done: FFmpeg (ffmpeg-next 9) pass-through adds 33 ms (one frame, TS demuxer); pipe itself 0.05 ms. 60-min soak clean (0 drops, RSS flat 50 MB). B-frame sources need display-order reordering (+100 ms, only for those sources).
- 2026-09-22 — M0 started: docs structure, harness and spike workspace set up.
