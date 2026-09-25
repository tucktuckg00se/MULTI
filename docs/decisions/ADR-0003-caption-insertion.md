# ADR-0003: Captions via GStreamer's encoders, inserted as SEI in display order

> **Summary:** Captions go into every video access unit as A/53 SEI, in display order, without re-encoding. The 608/708 bytes come from GStreamer's `tttocea708` (one per language lane, 708 service + CC1/CC3), joined with `cea708mux` and inserted by `h26xccinserter`, with no `cccombiner`. S2b measured no added delay and exact decoding in four languages. Our `cc` crate stays as test oracle and fallback.

- **Status:** accepted
- **Date:** 2026-09-24 (revised twice: first chose our own encoder, then deferred to S2b)
- **Evidence:** [S2b finding](../m0/findings/S2b-gstreamer-caption-encoders.md), [S3 finding](../m0/findings/S3-caption-encoder.md), [S1 finding](../m0/findings/S1-ffmpeg-pipeline.md), [S2 finding](../m0/findings/S2-gstreamer-pipeline.md)

## Context

Captions must survive TV and web chains without re-encoding the video. Principle: use existing, maintained tools where they do the job; write our own only where they can't.

## Settled

- One A/53 SEI per access unit, before the first VCL NAL, after AUD/VPS/SPS/PPS/other SEI. Validated in FFmpeg, ccextractor, libcaption and mpv (S1–S3).
- Captions are assigned in **display (PTS) order**. B-frame sources need reordering (+100 ms, only for those sources).
- With GStreamer, use its `h264ccinserter`/`h265ccinserter` rather than our own splice (S2 already did; no measurable delay).
- Avoid `cccombiner` on the live path: it adds 66 ms waiting for late audio (S2).
- Non-Latin scripts go to WebVTT/TTML/DVB rather than 708 P16 in v1.

## Decision: who generates the 608/708 bytes

**GStreamer's encoders** (S2b), wired as:
- one `tttocea708` per language lane, carrying a 708 service plus a 608 channel (CC1 or CC3);
- lanes joined with `cea708mux`, then `h264ccinserter`/`h265ccinserter`; no `cccombiner`;
- driven one video frame at a time from the video pad probe by our small driver (~300 lines).

Measured: 33.59 ms p50 black-box vs 33.56 ms with no captions (stage cost 0.03 ms); CC1+CC3 and 708 services 1+2 in four languages decode exactly in FFmpeg, libcaption and ccextractor; per-line control for 608 via JSON input; accents, 608-redefined ASCII and `èè` all correct.

What we still own:
- **Input cleaning:** valid UTF-8/JSON only (a bad buffer stops an appsrc lane for good) and transliteration of symbols GStreamer drops to spaces (`© ® … € œ`).
- **Rate guard:** GStreamer's 608 queue has no cap (up to 23.8 s late under 96 chars/s × 2 lanes), so we cap the backlog ourselves, as `CcMux` did.
- **Lane restart** after any flow error.
- **Workarounds for two upstream bugs** in gst-plugins-rs 0.15.3 (to be reported): `roll-up-rows` is lost at READY→PAUSED (re-set it once PLAYING); GAP events add frames even when the encoder is ahead (send GAP only to lagging tracks).

Our `cc` crate is kept as the test oracle (its round-trip tests check GStreamer's output) and as a fallback if the upstream bugs block us.

## Consequences

- Burn-in is out of scope for MULTI (PRD §2): a separate companion tool will take the captioned output and burn it in.
- Unverified: rendered text in VLC, YouTube ingest and hardware decoders (S6); `gst-direct` with B-frame sources; a soak of the `gst-direct` path; four independent 608 lanes (trailed input by 22 frames, not yet explained).
