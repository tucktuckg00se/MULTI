# ADR-0003: Generate captions with our own Rust encoder, inserted as SEI in display order

> **Summary:** MULTI encodes CEA-608/708 itself (the `cc` crate) and splices one A/53 SEI into every video access unit, in display order, without re-encoding. It decodes exactly in FFmpeg, ccextractor, libcaption and mpv, adds no measurable delay, and beats GStreamer's built-in caption elements on control, delay and dependencies.

- **Status:** accepted
- **Date:** 2026-09-23
- **Evidence:** [S3 finding](../m0/findings/S3-caption-encoder.md), [S1 finding](../m0/findings/S1-ffmpeg-pipeline.md), [S2 finding](../m0/findings/S2-gstreamer-pipeline.md); raw data in `docs/m0/evidence/S1–S3/`

## Context

Captions must reach viewers through TV and web chains without re-encoding the video. The options were to write our own 608/708 encoder or rely on a media framework's caption elements.

## Options considered

| Option | For | Against |
|---|---|---|
| Own encoder (`cc` crate) + SEI splice | Exact roll-up lines and pacing; any mix of CC1–CC4 and 708 services 1–6; no added delay (S2: 33.6 ms vs 33.5 ms without captions); pure Rust, no extra runtime deps | We own 608/708 correctness (mitigated by round-trip tests against three decoders) |
| GStreamer `tttocea608`/`tttocea708` + `cccombiner` | Maintained upstream | Reflows text into one 32-column stream, no per-line control; adds 66 ms (cccombiner latency); pulls in pango, cairo and X11 |
| FFmpeg re-encode with `a53cc` side data | Simple | Re-encodes video: quality loss, GPU/CPU cost, latency; violates the no-re-encode requirement |

## Decision

- Use the `cc` crate for all 608/708 generation: `CcMux` per stream, `next_frame()` once per frame, `h264_sei_nal`/`hevc_sei_nal` to wrap.
- Insert the SEI into each access unit before the first VCL NAL (after AUD/VPS/SPS/PPS/other SEI).
- Assign captions in **display (PTS) order**. For sources with B-frames, hold `video_delay` frames to reorder (S1: +100 ms p50, only for B-frame sources); zero-B-frame sources pay nothing.
- Non-Latin scripts (Cyrillic, Arabic, CJK) go to WebVTT/TTML/DVB, not 708 P16, for v1.

## Consequences

- The round-trip tests (FFmpeg, ccextractor, libcaption) become part of CI in M1.
- Still unverified: rendered text in VLC, YouTube ingest and hardware decoders (S6).
- Revisit only if a real downstream device rejects our SEI.
