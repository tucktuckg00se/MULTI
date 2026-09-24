# ADR-0003: Insert captions as SEI in display order; prefer existing caption tools

> **Summary:** Captions go into every video access unit as A/53 SEI, in display order, without re-encoding. Settled. Which code produces the 608/708 bytes is **pending S2b**: prefer GStreamer's `tttocea608`/`tttocea708` if they meet our needs, and keep our own `cc` encoder only where they fall short.

- **Status:** accepted for insertion; encoder choice pending S2b
- **Date:** 2026-09-24 (revised from 2026-09-23)
- **Evidence:** [S3 finding](../m0/findings/S3-caption-encoder.md), [S1 finding](../m0/findings/S1-ffmpeg-pipeline.md), [S2 finding](../m0/findings/S2-gstreamer-pipeline.md)

## Context

Captions must survive TV and web chains without re-encoding the video. Principle: use existing, maintained tools where they do the job; write our own only where they can't.

## Settled

- One A/53 SEI per access unit, before the first VCL NAL, after AUD/VPS/SPS/PPS/other SEI. Validated in FFmpeg, ccextractor, libcaption and mpv (S1–S3).
- Captions are assigned in **display (PTS) order**. B-frame sources need reordering (+100 ms, only for those sources).
- With GStreamer, use its `h264ccinserter`/`h265ccinserter` rather than our own splice (S2 already did; no measurable delay).
- Avoid `cccombiner` on the live path: it adds 66 ms waiting for late audio (S2).
- Non-Latin scripts go to WebVTT/TTML/DVB rather than 708 P16 in v1.

## Pending: who generates the 608/708 bytes

| Option | Known | Unknown |
|---|---|---|
| GStreamer `tttocea608`/`tttocea708` | Maintained upstream; handle channels and 708 services | S2 fed plain text and got one reflowed 32-column stream. Not tested: structured (JSON) input with explicit lines, and feeding `h26xccinserter` directly without `cccombiner` |
| Our `cc` crate (S3) | Exact line and pacing control; exact on 3 decoders; zero added delay | We maintain 608/708 correctness ourselves |

**S2b** tests the unknowns. If GStreamer's encoders give per-line control at no added delay, use them and keep `cc` only as a test oracle and fallback. Our code having been written already is not a reason to ship it.

## Consequences

- Burn-in is out of scope for MULTI (PRD §2): a separate companion tool will take the captioned output and burn it in.
- Unverified: rendered text in VLC, YouTube ingest and hardware decoders (S6).
