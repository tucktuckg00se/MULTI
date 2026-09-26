# M1 — First real version

> **Summary:** Turns the M0 spikes into product code in `crates/`: SRT/UDP in, SRT/UDP/RTMP out, English speech captioned in EN plus ES/FR/DE translations on 608/708, word filters, ASR and translation in supervised worker processes, a TOML config, and a web GUI for settings, status and live captions. CPU-only CI on GitHub.

## Work packages

One branch and PR per package; CI must pass before merge.

| WP | Contents | Status |
|---|---|---|
| 1 Skeleton | Workspace, `multi-core` (config with PRD defaults, text types, worker IPC framing), `multi` CLI stub, CI | in progress |
| 2 Media | `multi-media`: GStreamer input, restamping bridge, caption lanes, SRT/UDP/RTMP outputs, reconnect; fake-worker integration test | in review |
| 3 Workers | `multi-asr`, `multi-mt` worker binaries; supervisor with heartbeats and restart | in review |
| 4 Caption quality | Segmenter, word filters, text cleaning, backlog cap, lane restart, degrade policy, CC2/CC4 option | not started |
| 5 Web GUI | Control layer, API + live events, embedded page: settings, status, live captions | not started |
| 6 Validate | B-frames, 4-hour soak, live OBS test from the GUI, YouTube RTMP test, findings | not started |

## Exit criteria

- 4-hour live run: no video interruptions, memory flat, no errors.
- EN caption lag P95 ≤ 1.5 s; translated ≤ 2.5 s.
- Killing the ASR or translation worker never interrupts video; captions return within 5 s.
- A blocklisted word never appears in any caption language.
- Captions visible on YouTube via RTMP; EN/ES in VLC; FR/DE via the CC2/CC4 option.
- Set up, start and monitor a stream using only the web GUI.
- CI green on `main`.

## Code layout

| Path | What |
|---|---|
| `crates/multi-media` | GStreamer pipeline: input, restamping bridge, caption lanes (`CaptionHandle`), outputs, audio tap, `Stats` |
| `crates/multi-core` | Config (`Config::validate` reports issues by setting path), `Word`/`Clause`/`Translation`, worker IPC framing |
| `crates/multi` | The `multi` binary: `multi run`, `multi config default`, `multi config check`; `supervisor` (worker processes), `run` (wiring), `segment` (placeholder segmenter) |
| `crates/multi-asr` | ASR worker: Nemotron 3.5 Streaming 560 ms (sherpa-onnx) + Silero VAD |
| `crates/multi-mt` | Translation worker: opus-mt via CTranslate2 (ct2rs) |
| `crates/multi-fake-worker` | Scripted worker for tests (no models) |
| `spikes/` | M0 reference code; not built by the root workspace |

## Workers

ASR and translation run in child processes so a crash, hang or CUDA fault never touches video. Each speaks `multi_core::ipc` on stdin/stdout and logs on stderr (the supervisor re-logs each line with the worker's name).

**Protocol.** Main → worker: PCM frames (`start_ms` + 16 kHz mono i16) for ASR, `Translate { clause, langs }` for MT, `Shutdown`. Worker → main: `Ready` once models are loaded, `Heartbeat` every 500 ms from a separate thread (also during loading and decoding), `Words` (times on the incoming PCM timeline), one `Translated` or `Error` per requested language. A worker withholds heartbeats if one decode or translation runs over 10 s, so a stuck model looks hung.

**Restart policy** (`multi::supervisor::Policy`): no frame for 3 s, process exit, or broken framing → kill and restart after 0.5 s, doubling to 10 s; the delay resets once a worker has been ready for 60 s. Nothing is written to a new worker until it says `Ready`. `send` never blocks: frames wait in a bounded queue (256) that drops the oldest when full, counted in `status().dropped_in`. `status()` gives state (starting, ready, restarting, failed, stopped), restarts, last error, drops and pid. Shutdown: `Shutdown`, 2 s grace, then kill; each worker drops its models on their own threads before exiting (S6 gotcha 6).

**MT details.** One thread, model and queue (8) per language: a slow or failed language gets an `Error` and never delays the others. fp16 on CUDA, int8 on CPU; sentence split; decode length cap `4 × words + 16` (≤ 256); a 2 s deadline from arrival stops decoding and returns an `Error`.

**Build and run** (models in `~/.cache/multi-models/`, see [S4](../m0/evidence/S4/models.txt) and [S5](../m0/evidence/S5/models.txt) for sources and licences):

```sh
# GPU: sherpa-onnx CUDA prebuilt + CTranslate2 with CUDA (CUDA_PATH/arch in .cargo/config.toml)
export SHERPA_ONNX_LIB_DIR=~/.cache/multi-tools/sherpa/sherpa-onnx-v1.13.8-cuda-13.x-cudnn-9.x-onnxruntime1.28.2-linux-x64-gpu/lib
cargo build --release -p multi-asr -p multi-mt --features multi-mt/cuda
target/release/multi-asr --model-dir ~/.cache/multi-models/sherpa/sherpa-onnx-nemotron-3.5-asr-streaming-0.6b-560ms-2026-06-11-fp32 --device auto
target/release/multi-mt --models ~/.cache/multi-models/ct2 --langs es,fr,de --device auto
# Real-model check (evidence/WP3): 1x ASR on long.wav, MT latency, kill -9 recovery
cargo run --release -p multi --example wp3_check -- --asr-model-dir … --mt-models ~/.cache/multi-models/ct2 \
  --wav spikes/harness/media/long.wav --reference spikes/harness/media/long.txt
```

**Measured** ([evidence/WP3](evidence/WP3), RTX 3090): ASR at 1× on `long.wav` (8 min): 1355 words, WER 5.8% (simple normaliser), word lag P50 0.56 s / P95 0.96 s, no drops. MT (20 clauses × es/fr/de): P50 16 ms, P95 22 ms. After `kill -9`: ASR words resume in 1.8 s (0.5 s backoff + 1.2 s load), MT in 0.9 s; the frame in flight at the kill is lost.

Without `SHERPA_ONNX_LIB_DIR` the sherpa-onnx build script downloads its CPU prebuilt; without `--features cuda`, CTranslate2 is built CPU-only. `--device auto` tries CUDA and logs a warning when it falls back to CPU. `multi-asr` and `multi-mt` are not default members, so plain `cargo build/test/clippy` skip them; CI checks them in a separate `workers` job (CPU-only, cached). Supervisor tests (`crates/multi/tests/supervisor.rs`) drive the fake worker through crash, hang, garbage output, blocked sends and shutdown.

## Media

`multi run -c multi.toml` starts the pipeline, the ASR worker and the MT worker, logs a `stats` line every 10 s, and stops on Ctrl-C (media first, then workers). Worker binaries default to `multi-asr`/`multi-mt` next to `multi`, with models from `--models-dir` (`$MULTI_MODELS`, else `~/.cache/multi-models`); `--asr-worker "<path> [args]"` and `--mt-worker "<path> [args]"` replace them (tests use `multi-fake-worker`).

```text
input -> tsdemux -> bridge -> h26xparse -> [caption lanes] -> h26xccinserter -> mpegtsmux -> SRT/UDP outputs
                                                                                   `-> ES -> flvmux -> RTMP outputs
            `-> audio tap: aacparse -> avdec_aac -> 16 kHz mono -> 100 ms PCM frames -> ASR -> segmenter -> source lane
                                                                                            `-> clause -> MT -> other lanes
```

- **Input** `input.url`: `srt://` (caller or listener; `srt.latency_ms` unless the URL sets `latency`), `udp://` (unicast or multicast), `rtp://` (MPEG-TS over RTP). H.264 or HEVC, detected from the stream, parsed but never decoded. The input is rebuilt on error, EOS, or 2 s of silence after data (srtsrc's silent caller, S2); the bridge ([S2](../m0/findings/S2-gstreamer-pipeline.md)) re-stamps onto one continuous output timeline, so a source restarting at PTS 0 continues seamlessly.
- **Outputs** `outputs[].url`: `srt://`, `udp://` (MPEG-TS) and `rtmp(s)://` (FLV, H.264 + AAC; the SEI keeps the captions; needs both video and audio). Each output is its own pipeline, restarted alone with backoff (0.5 s doubling to 10 s); URLs are logged with passphrases and stream keys masked.
- **Caption lanes** from `languages` (default EN CC1+708 s1, ES CC3+s2, FR s3, DE s4) with `captions.mode`, `rows` and `offset_ms` (≥ 0; negative needs video delay, not built). S2b's `GstCc` with both upstream workarounds. CC2/CC4 are refused at startup (WP4).
- **Audio tap**: first audio track (or `audio.track`), `audio.channel` or downmix, `avdec_aac` only (startup fails without gst-libav); PCM timestamps are the input audio PTS.
- **Segmenter** (placeholder until WP4): clause closes on punctuation, a `translate.max_wait_ms` pause, or 24 words.
- **Stats** (`Media::stats()`): frames in/out, sessions, input restarts/errors, per-output state and errors, caption frames, per-lane pushed/dropped/queued, audio tap chunks/drops.

Tested in CI by `crates/multi/tests/pipeline.rs` (real GStreamer, fake workers, ports 9720–9722): fake words on CC1, `[es] …` on CC3, captions over RTMP (FFmpeg as RTMP server), video and captions continue after `kill -9` of the ASR worker and after a source restart, clean Ctrl-C exit. **Measured** ([evidence/WP2](evidence/WP2), real workers, RTX 3090, 180 s): video delay p50 33.6 ms / p99 34.2 ms; EN caption lag p50 0.96 s / p95 1.08 s; ES on CC3; no drops or restarts.

## Log

Newest first.

- 2026-09-25 — WP2 in review: `multi-media`, `multi run` wiring, pipeline integration test. Real-model numbers in [evidence/WP2](evidence/WP2).
- 2026-09-25 — WP3 in review: workers, supervisor, fake worker, CI `workers` job. Real-model numbers in [evidence/WP3](evidence/WP3).
- 2026-09-25 — M1 started: WP1 skeleton.
