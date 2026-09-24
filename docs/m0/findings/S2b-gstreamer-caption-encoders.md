# S2b: GStreamer's own caption encoders (tttocea608, tttocea708)

> **Summary:** Use them. Driven frame by frame from the video probe, with no `cccombiner`, `tttocea708` adds no black-box delay (33.59 vs 33.56 ms p50). CC1, CC3 and 708 services 1–2 decode exactly in four languages, and each caption can start its own row. We still need a small driver and workarounds for 2 upstream bugs, so keep `cc` as oracle and fallback.

| Requirement | GStreamer (gst-plugins-rs 0.15.3) | Our `cc` |
|---|---|---|
| Line breaks per caption | 608 **pass** (JSON `lines`, `carriage_return`). 708 **partial**: text only; a `rstranscribe/final-transcript` event starts a row, and `\n` is ignored in roll-up | **pass** |
| Roll-up 2/3/4 | 608 **pass**. 708 **bug**: always 2 rows unless `roll-up-rows` is set again once PLAYING | **pass** |
| Pop-on / paint-on | **pass**. Cuts lines at 32 columns, and a new pop-on waits for the previous buffer's end | 608 pass; 708 roll-up only |
| Row/column, colour | 608 **pass** per line; 708 per element | partial (base row) |
| Added delay, no cccombiner | **pass**: stage +0.03 ms; 33.59 ms p50 (1 lane), 33.65 (2 lanes) | **pass**: 33.57 ms |
| CC1+CC3+svc1+svc2, 4 languages | **pass**, 16/16 lines exact | **pass**, 16/16 |
| es/fr/de/pt, `* _ ~ \| { } \ ^`, `èè` | **pass** in FFmpeg, libcaption, ccextractor | **pass** |
| Other symbols | **partial**: `© ® … € œ` → space (608), `®` → space (708) | kept or transliterated |
| Overload, 96 chars/s × 2 lanes | no loss; unbounded queue, 608 up to 23.8 s late | capped: 9/90 lines dropped, 29 s late |
| Bad input | invalid UTF-8/JSON → `FlowError`, which **stops an appsrc lane for good**; emoji and control characters → spaces | dropped; `&str` API |
| 608 control codes | sent once (≈14% more text/s) | sent twice (usual practice) |

## Method

- **Offline:** `spikes/s2b-gst-cc`. `GstCc` runs `tttocea608|tttocea708` [→ `ccconverter`] [→ `cea708mux`] → appsink.
  - Each video frame: GAPs to idle tracks, then that frame's `cc_data` is pulled.
  - One track is pushed synchronously; several use appsrc threads (one thread would deadlock the aggregator).
  - `run.sh NAME` renders the same video with GStreamer and `CcMux` bytes and decodes both in FFmpeg, ccextractor and libcaption.
- **Live:** `s2-gst-pipe --captions gst-direct [--lanes 2]` calls `GstCc` from the vparse probe, using (b)'s display-slot logic, then `h264ccinserter`. Measured with S2's `bench.sh`.

## Details

1. **Lines** ([evidence](../evidence/S2b/line-control.txt)).
   - JSON placed rows and indents exactly (PAC row 12 indent 4, row 2 indent 8), and switched mode and colour/underline per line.
   - Roll-up wraps at 32 columns on word boundaries, like ours.
   - In `tttocea708`, buffers without the event run together on one row, and a second pop-on row gets a leading space.
2. **Delay** ([evidence](../evidence/S2b/latency.txt), UDP→UDP, p50/p95/p99/max ms).

   | | none | ours | gst-direct |
   |---|---|---|---|
   | H.264 | 33.56 / 34.83 / 38.57 / 39.45 | 33.57 / 36.45 / 38.58 / 41.39 | 33.59 / 34.49 / 38.69 / 40.00 |
   | H.264, 2 lanes | | | 33.65 / 35.41 / 37.87 / 39.57 |
   | HEVC | 33.56 / – / 70.66 / 79.19 | | 33.68 / – / 72.56 / 82.11 |

   - Caption stage p50/max: 0.03/0.14 ms (1 lane), 0.07/3.35 ms (2 lanes; `cea708mux` thread, wait capped at 5 ms).
   - verify.sh and ccextractor passed on every channel.
   - S2's +66 ms was `cccombiner`, the only element that attaches caption meta; skipping it takes our own probe glue.
3. **Languages** ([evidence](../evidence/S2b/multilang.txt)).
   - 2× `tttocea708` (svc1+CC1, svc2+CC3) → `cea708mux`: exact.
   - 4 separate languages (`tttocea608` → `ccconverter`, with `capssetter field=1` for CC3) are also exact, but trailed the input by 22 frames. The cause is not traced, so this layout is not for the live path.
4. **Characters** ([evidence](../evidence/S2b/chars-compare.txt)).
   - `èè` survives because the element inserts a mode code after special characters (libcaption's trick).
   - Unmapped characters become a space (`unwrap_or(Code::Space)` in `tttocea608/translate.rs`). `©` and `®` should map but don't.
5. **Bursts** ([evidence](../evidence/S2b/burst.txt)).
   - Both recover once input stops (GStreamer drained at 54.4 s, ours at 59.8 s).
   - The 608 queue inside `tttocea708` has no cap or metric and falls behind its own 708 service.
6. **Robustness** ([evidence](../evidence/S2b/robustness.txt)).
   - No crash or stuck state.
   - One caption per frame and a 2000-character text all arrive, late.
   - A JSON row > 14 is dropped with a warning; an unknown style fails the buffer.
7. **Timing** ([evidence](../evidence/S2b/timing.txt)).
   - Text needs PTS and duration, so we stamp each ASR line at the next video slot with a duration of one frame.
   - `tttocea608` stamps any overflow on the buffer's last frame, so the driver re-paces output through a FIFO.
   - Pop-on needs its display time up front: a replacement pushed at 2 s showed at 6.0 s behind a 5 s caption.

## Upstream (gst-plugins-rs 0.15.3, `video/closedcaption/src/`)

- **Bug 1:** in `tttocea708/imp.rs`, the READY→PAUSED reset never calls `set_roll_up_count` (FlushStop does). The window stays at 2 rows in both ccextractor and GStreamer's own `cea708overlay` ([evidence](../evidence/S2b/708-rollup-rows-bug.txt)). Workaround: set the property again once PLAYING.
- **Bug 2:** in `tttocea708/translate.rs`, `generate()` emits a frame for every GAP, even when the encoder is already ahead. With a GAP every frame the lag never shrinks (+59 frames after two pop-ons). Workaround: send a GAP only when the track is behind.
- **Limits:**
  - no JSON input on `tttocea708`
  - `tttocea608` writes CC1 only
  - no queue-depth property
  - pop-on idle GAPs make it send an EDM every ~1.5 frames.

## Recommendation

**GStreamer's encoders:** one `tttocea708` per language lane (708 service + CC1/CC3), `cea708mux` for 2 or more lanes, driven per frame by our wrapper, with no `cccombiner`. They pass ADR-0003's test: per-line control at no added delay, and they are maintained upstream.

What we keep writing:
- the driver (GAP policy, re-pacing, rows workaround)
- input cleaning (transliterate before characters turn into spaces)
- an input-rate/backlog guard
- a lane restart on flow error.

Keep `cc` as the test oracle and fallback, and report both bugs upstream. Revisit a split (ours for 608) if the unbounded 608 queue or the lost symbols show up in S6 or in the field.

**Open:**
- `gst-direct` with B-frames (same slot logic as (b), which passed)
- a soak run
- 708 rendering on real players (S6).
