# ADR-0004: Use GStreamer (gstreamer-rs) as the pipeline base

> **Summary:** Proposed: build the media pipeline on GStreamer via `gstreamer-rs`. S1 (FFmpeg) and S2 (GStreamer) tied on latency (33 ms, one frame) and both passed a clean 60-minute soak. GStreamer wins on caption tooling, the core of the product; FFmpeg wins on explicit timing control and simpler shipping.

- **Status:** proposed, awaiting product-owner decision
- **Date:** 2026-09-24 (revised: first draft recommended FFmpeg)
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

## Caption tooling available (installed versions, checked 2026-09-24)

| Caption job | FFmpeg 9.0 | GStreamer 1.28 |
|---|---|---|
| Text → 608/708 | none | `tttocea608`, `tttocea708` |
| Insert without re-encode | none (we wrote our own) | `h264ccinserter`, `h265ccinserter` |
| WebVTT / TTML | encoders + HLS/DASH muxers | `webvttenc`; TTML parse/render only |
| DVB subtitles | `dvbsub` (bitmap input) | `dvbsubenc` (bitmap input) |
| SCC / MCC files | muxers | encoders + parsers |
| Caption format conversion | none | `ccconverter`, `cea608tocea708` |
| Burn-in (for the companion tool) | libass `subtitles`, `drawtext` | `cea608overlay`, `cea708overlay`, `textoverlay` |
| Teletext | none | decode only |

## Recommendation

**GStreamer via `gstreamer-rs`.** Latency and soak results tie, so the decision rests on what the product does all day, which is captions:
- GStreamer has maintained elements for 608/708 generation, insertion, conversion and caption files; FFmpeg has none for 608/708, so on FFmpeg all of it is ours to build and maintain.
- The planned burn-in companion tool is a natural GStreamer pipeline (decode → `cea608overlay` → encode), sharing the same stack.
- `gstreamer-rs` needed no `unsafe` in 1.5k lines.

Costs we accept:
- Timing is GStreamer's clock model. S2 needed a restamping bridge (hold + slew) to avoid a 133/200 ms mux sawtooth, and its first soak found a caption-loss bug after a source stall (fixed). This bridge is the riskiest code and needs tests and a long soak in M1.
- Shipping means bundling GStreamer ≥ 1.26 with ~10 plugins and libsrt (~7 MB); on Windows, the runtime and plugin registry.
- Separate sink pipelines per output, since a `tee` spreads one sink's error to all outputs.

## If accepted

- M1 starts from S2's shape: input pipeline → restamping bridge → persistent output pipeline (parse → captions → `h26xccinserter` → `mpegtsmux`) → one sink pipeline per output.
- Fix S2's known gaps: recover from output-mux bus errors, detect a connected source that sends garbage, make the watchdog timeout configurable, and move caption work into a separate process (PRD reliability rule).
- S1's FFmpeg code stays as a reference and fallback.

Revisit if GStreamer's timing model keeps producing bugs that FFmpeg's explicit loop would avoid.
