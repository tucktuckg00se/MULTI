# S2: GStreamer pass-through pipeline with caption insertion

> **Summary:** It works. H.264/HEVC goes from SRT/UDP to several SRT/UDP outputs, undecoded. The 608+708 captions decode in FFmpeg, ccextractor and mpv, and the pipeline survives source restarts. With our encoder (b), the black-box delay is one frame: 33.6 ms p50, 34.2 ms p99 (H.264, UDP), the same as with no captions. GStreamer's own caption path (a) adds 66 ms. Each SRT hop adds its 120 ms `latency`.

## Run and design

`spikes/s2-gst-pipe`: `s2-gst-pipe run --input 'srt://…?mode=caller&latency=120' --codec h264|hevc --captions none|gst|ours [--lanes 2] --output udp://… --output 'srt://:…?mode=listener' --csv delay.csv --stats stats.csv`. Scripts: `bench.sh` (latency matrix with `latency` taps, verify.sh, ccextractor), `reconnect.sh`, `soak.sh`.

- **Input (rebuilt on bus error, EOS or 2 s of silence):** `srtsrc|udpsrc → tsdemux ignore-pcr → appsink` for video and audio. Audio goes through a parser only.
- **Bridge:** re-stamps onto the output pipeline's running time.
  - Sessions share one A/V offset. A new session starts on a restart or when PTS jumps back more than 1 s or forward more than 3 s.
  - The first 500 ms of a session is held, and the offset is set from the earliest arrival.
  - The offset then slews by at most 5 ms per frame when frames are early, and 1 ms per frame when they are more than 100 ms late.
  - DTS is kept strictly increasing.
- **Output (never restarted):** `appsrc → h26xparse → [captions] → h26xccinserter → mpegtsmux → appsink`, then one `appsrc ! srtsink|udpsink` pipeline per output, each restarted on its own. A `tee` would let one failing sink stop them all.
- **(b):** a pad probe feeds `cc::CcMux`, one display slot per frame: the previous slot plus the PTS step rounded to whole frames. It attaches `GstVideoCaptionMeta` (cc_data), which the inserter writes as SEI.
- **(a):** `appsrc text → tttocea708 (cea608-channel=1) → cccombiner`. With 2 lanes, `cea708mux` joins two tttocea708 elements.

## Latency (black box, `latency report --skip 10`, 45 s per cell, p50 / p95 / p99 / max ms)

| In → out | Codec | none | (b) ours | (a) gst |
|---|---|---|---|---|
| UDP → UDP | H.264 | 33.5 / 33.9 / 34.2 / 39.6 | 33.6 / 33.9 / 34.2 / 35.4 | 99.8 / 100.1 / 100.2 / 100.2 |
| UDP → UDP | HEVC | 33.6 / 38.6 / 72.9 / 77.6 | 33.6 / 39.4 / 73.9 / 80.8 | 43.4 / 68.9 / 80.7 / 92.6 |
| SRT → UDP | H.264 | 153.7 / 162.6 / 163.6 / 168.0 | 153.8 / 162.9 / 164.1 / 167.5 | 219.9 / 220.2 / 220.9 / 226.0 |
| SRT → UDP | HEVC | 154.8 / 163.6 / 195.2 / 202.0 | 154.9 / 164.7 / 195.2 / 201.9 | 184.3 / 186.7 / 195.9 / 212.3 |
| UDP → SRT | H.264 | 153.6 / 154.0 / 154.8 / 158.2 | 153.6 / 154.1 / 154.6 / 158.2 | 219.9 / 220.2 / 220.3 / 220.4 |

- **SRT:** the bridge pair on its own (latency=120) measures 120.2 / 121.0 / 123.3 / 126.5 ms. Subtract it from the SRT rows.
- **Internal delay with (b):** 0.07 / 0.10 / 0.13 / 0.20 ms from tsdemux to mux, of which the caption probe is 0.015 ms. The 33.5 ms floor is tsdemux closing a PES only when the next one starts. For comparison, an `ffmpeg -c copy` relay measured 133 ms p50 / 200 ms p95.
- **HEVC p99 of about 73 ms:** it comes from the source, since it appears with no captions too.
- **(a)'s +66 ms:** cccombiner's reported latency gives mpegtsmux a 66.7 ms deadline, so the mux waits for the audio that arrives late (about 180 ms PES). HEVC frames already arrived about 45 ms late, so they waited less. With cccombiner `latency=100` it is 133 / 200 / 201 / 204 ms.

## (a) vs (b)

| | (b) `cc` crate + meta | (a) tttocea708 + cccombiner |
|---|---|---|
| Added delay | none measurable | +66 ms |
| Line control | exact roll-up lines | text reflows into one 32-column flow; plain text has no line breaks (tttocea608's JSON input has them) |
| Channels | CC1–4 and 708 services 1–6 in one mux; CC1+CC3+svc 1+2 tested | 608 compatibility bytes on CC1/CC3 only; services via cea708mux (tested) |
| B-frames (x264 `-bf 2`) | correct | text scrambled until the inserter gets `caption-meta-order=display` |
| Runtime | gst-plugins-bad `closedcaption` | adds gst-plugins-rs `rsclosedcaption` (MPL, 2.3 MB, links pango/cairo/X11) |

- **Both approaches pass** verify.sh (UDP and SRT, H.264 and HEVC), ccextractor 608 and 708, the two-lane test and the B-frame test.
- **Decode check:** every frame of all 16 captures carries A53 CC, with 0 decode errors ([ffprobe](../evidence/S2/ffprobe-check.txt)).
- **mpv 0.41** shows the CC track headlessly.
- **Verdict:** (b) gives full control at no cost. (a) matches it on channels and 708, but not on pacing, line control or delay.

## Reconnect (source killed at 15 s, restarted at 20 s with PTS from its start)

UDP, SRT caller and SRT listener pass with (b), and UDP and SRT caller/HEVC pass with (a). The process stays up and the outputs are not restarted.

- **During the gap:** no packets at all; PAT, PMT and PCR stop. The SRT output connection stays open.
- **After the gap:** the same PIDs, with continuity counters unbroken (`cc_err=0`). PTS/PCR jump forward (4.5 s for a 5.5 s gap) and never go back to 0. The next frame is the source's IDR with SPS/PPS. Every packet decodes and captions continue.
- **srtsrc:** as a caller it only *warns* ("Trying to reconnect") and goes silent, so the watchdog rebuilds it. As a listener it posts EOS when the caller leaves.

## Soak

The soak ran for the full 60 min on build 1a2abae: H.264 source → tap → SRT in (latency 120) → (b) → UDP and SRT out ([soak2](../evidence/S2/soak2.txt), [stats](../evidence/S2/soak2-stats.csv)).

- **Black box to UDP (one SRT hop):** 153.8 / 162.7 / 163.6 / 178.1 ms p50 / p95 / p99 / max. The p50 of every 10-minute window was 153.8–154.0 ms.
- **Stability:** drift −0.008 ms/min, and output PTS stayed within 1 ms of the running time. 107,980 frames in = out = captioned, with 0 restarts, errors or drops. verify.sh passed on the last 20 s.
- **Memory:** RSS peaked at 25.6 MB and ended at 17.2 MB, with no growth. The taps ran with `--quiet`, so stream-error counters were not recorded for the soak.
- **The first soak attempt found a bug** ([soak1](../evidence/S2/soak1-starved.txt)). A load spike (load average about 11) made the source stall for 78 s, then catch up. The pipeline stayed up and needed 3 watchdog restarts. But about 3% of frames lost captions, because the bridge slewed PTS off a fixed caption grid. Fixed in 1a2abae (gotcha 5).

## Reliability review of `main.rs` and the video path

- **Panics and blocking:** there are no `unwrap`, `expect` or indexing panics outside tests. Probes and callbacks only log and count, poisoned locks skip the frame, and nothing blocks a streaming thread (appsrc is leaky, the CSV writer uses `try_send`). The process exits only on bad arguments or when the *first* input build fails. Later rebuild failures are retried with backoff; before fix 11e693a they ended the process.
- **Bus handling:** an input error or EOS rebuilds the input pipeline. Each output sink restarts on its own. Output mux errors are only logged: the mux is fed by appsrc and never failed, but it has no recovery.
- **Gaps:** the output mux has no recovery; the 2 s watchdog is fixed; a connected source sending garbage is not detected; captions are not a separate process yet (PRD P-06).

## gstreamer-rs

- **Ergonomics and safety:** builders, typed caps and closures cover everything, with 0 `unsafe` in 1.5 k lines. `set_property` panics on a wrong type or name, so check or wrap it outside setup code.
- **Debugging:** `GST_DEBUG` finds problems fast, but failures are silent without it (a preroll deadlock, appsrc leaky drops). The bugs were all in live-aggregator timing, preroll, and PTS/DTS.
- **Deployment:** (b) needs about 10 LGPL plugins (core, base, good, bad) plus libsrt (MPL): about 7 MB, or distro packages on Linux x86-64 and ARM64. Windows has MSVC installers, but shipping means bundling the runtime (about 20–40 MB) and pinning ≥ 1.26 for `h26xccinserter`. Details in [gst-deploy](../evidence/S2/gst-deploy.txt).

## Gotchas

1. **Preroll deadlock:** udpsrc is not live, and two appsinks behind tsdemux deadlock in preroll. Use `async=false`.
2. **appsrc drops the startup burst:** the default `max-bytes` of 200 kB with `leaky-type=downstream` silently drops it.
3. **Stamping ahead of the running time:** live `mpegtsmux`/`cccombiner` hold such a buffer, which costs 67–200 ms.
4. **Monotonic clamp:** it belongs on DTS. Clamping PTS broke B-frames: (b) captioned only 118 of 266 frames.
5. **PTS slewing breaks a fixed caption grid:** two frames land on one slot. Use relative slots (found in the soak).
6. **ffmpeg probe quirk:** a recording from the very first output packet makes ffmpeg's probe print about 25 "non-existing PPS" messages, and verify.sh's movie= filter fails. All frames still decode. The cause is not found.

## Recommendation

If GStreamer is chosen, use approach (b) (our `cc` crate plus `h26xccinserter`, no Rust caption plugin) behind the two-pipeline bridge with its running-time lock. It isolates input restarts and failures of individual outputs, and adds nothing beyond tsdemux's one frame. Compare this table with S1's (same `latency` taps). The costs are deployment weight and aggregator subtleties.
