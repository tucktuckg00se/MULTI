# ADR-0004: Use FFmpeg's libraries (ffmpeg-next) as the pipeline base

> **Summary:** Proposed: build the media pipeline on FFmpeg's libraries via `ffmpeg-next`. S1 (FFmpeg) and S2 (GStreamer) tied on latency (33 ms, one frame) and both passed a clean 60-minute soak. FFmpeg wins on explicit timing control and simpler cross-platform shipping; GStreamer wins on no `unsafe` and lower memory.

- **Status:** proposed, awaiting product-owner decision
- **Date:** 2026-09-23
- **Evidence:** [S1 finding](../m0/findings/S1-ffmpeg-pipeline.md), [S2 finding](../m0/findings/S2-gstreamer-pipeline.md); raw data in `docs/m0/evidence/S1/` and `S2/`

## Context

MULTI needs SRT/RTP/UDP in and out, video passed through without decoding, SEI insertion on every frame, and many outputs, on Linux x64/ARM64 and later Windows. Reliability first, speed second (ADR-0001, PRD §2).

## Measured head to head (same host, same source, same latency tool)

| | S1: FFmpeg (ffmpeg-next 9) | S2: GStreamer (gstreamer-rs, 1.28) |
|---|---|---|
| Black-box delay, H.264 UDP→UDP, p50 / p99 / max | 33.4 / 33.9 / 34.3 ms | 33.6 / 34.2 / 35.4 ms |
| Where the delay comes from | TS demuxer waits for next PES (1 frame) | Same |
| 60-min soak | 108,168 frames, 0 drops, drift 0.000 ms/min | 107,980 frames, 0 errors, drift −0.008 ms/min |
| Memory | RSS flat, 50 MB | RSS flat, 25.6 MB |
| Source drop / restart | Survives UDP, SRT; codec change detected | Survives UDP, SRT caller and listener |
| `unsafe` needed | 5 blocks (interrupt callback on output open, codec_tag, video_delay, a Sync impl) | 0 |
| Timing model | Our packet loop; timing is explicit | Pipeline clock; needed a custom hold+slew bridge to avoid a 133/200 ms mux sawtooth |
| Failure isolation | Our threads per output | Needed separate sink pipelines, since a `tee` spreads one sink's error to all outputs |
| Runtime to ship | libav* shared libs (LGPL build) | ~10 plugins + libsrt (~7 MB), GStreamer ≥ 1.26; plugin registry to bundle on Windows |
| Known gaps | Codec change reuses PMT version 0 (downstream keeps old decoder); UDP outage not logged | srtsrc caller goes silent on drop (needs watchdog); output-mux errors only logged |

## Recommendation

**FFmpeg libraries via `ffmpeg-next`.** Latency and soak results are a tie, so the decision rests on reliability and operability:
- Timing is explicit in our own packet loop. GStreamer's clock model produced the only timing problems seen (the mux sawtooth and the caption-loss bug found in S2's first soak) and needed a custom bridge to tame.
- One set of libraries to build and ship on Linux x64, ARM64 and Windows, instead of a plugin runtime and registry.
- FFmpeg covers the later formats (RTMP, HLS, DVB) in the same API.
- The 5 `unsafe` blocks are small and wrapped; they would live in one module with tests.

## If accepted

- M1 starts from S1's code shape: demux → SEI splice → per-output writer threads.
- Fix S1's known gaps in M1: bump PMT version or change PIDs on codec change, add UDP input timeout, drop (not flush) held B-frames at session end, send null packets/PCR during input gaps.
- Keep S2's lesson: isolate outputs so one failing destination never stops the others.

Revisit if `ffmpeg-next` stops tracking FFmpeg releases or if a later milestone needs GStreamer-only elements.
