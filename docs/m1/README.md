# M1 — First real version

> **Summary:** **Done 2026-09-28** ([results](findings/M1-results.md)). Turns the M0 spikes into product code in `crates/`: SRT/UDP in, SRT/UDP/RTMP out, English speech captioned in EN plus ES/FR/DE translations on 608/708, word filters, ASR and translation in supervised worker processes, a TOML config, and a web GUI for settings, status and live captions. CPU-only CI on GitHub.

## Work packages

One branch and PR per package; CI must pass before merge.

| WP | Contents | Status |
|---|---|---|
| 1 Skeleton | Workspace, `multi-core` (config with PRD defaults, text types, worker IPC framing), `multi` CLI stub, CI | done |
| 2 Media | `multi-media`: GStreamer input, restamping bridge, caption lanes, SRT/UDP/RTMP outputs, reconnect; fake-worker integration test | done |
| 3 Workers | `multi-asr`, `multi-mt` worker binaries; supervisor with heartbeats and restart | done |
| 4 Caption quality | Segmenter, word filters, text cleaning, backlog cap, lane restart, degrade policy, CC2/CC4 option | done |
| 5 Web GUI | Control layer, API + live events, embedded page: settings, status, live captions | done |
| 6 Validate | B-frames and 4-hour soak **done** ([summary](evidence/WP6/soak-summary.txt)); live OBS test from the GUI and YouTube RTMP test still to do; findings | done |
| 7 Models | Model registry (PRD MD-4); `multi models list/pull/verify/remove`; storage folders; hosted pre-converted opus-mt; clear error and GUI warning when a language's model is missing | done |
| 8 GUI follow-ups | Input audio level meter and a warning after ~10 s of silence; warn when a language has no installed model or its script can't go on 608/708; fix output mode label ("waits for the receiver") | done |
| 9 Web login and HTTPS | Username + password login for the web GUI (argon2 hash in config, `multi passwd` or first-run setup, rate-limited attempts, expiring session cookie, Log out); API token kept for scripts; HTTPS built in or a documented reverse-proxy setup (Caddy/nginx), since passwords over plain HTTP can be read on the network | done |
| 10 Per-output control | `enabled` flag and Start/Stop per output (GUI Outputs list + API); add, remove or change an output without restarting the pipeline or interrupting the others. Outputs already run as separate sink pipelines, so this is mostly control plumbing | done |
| 11 Fallback picture on input loss | While the input is gone, send black or a slate image plus silence, encoded to match the stream's codec and resolution, so downstream (YouTube, decoders) stays connected; configurable timeout. Makes PRD IO-8 "keep output alive" real | done |
| 12 Models in the GUI and more languages | **Two lists:** (1) *installed* models, what's on disk; the GUI's model and language choices only offer these; (2) the *catalogue* of models you can pull: the built-in registry plus a user-editable file (e.g. `~/.config/multi/models.toml`) merged on top, so users can add their own models with source, checksum and licence. **GUI Models section:** list, pull (with progress and licence shown), verify, remove, the same as `multi models`. **More languages:** grow the catalogue from 4 translation targets to every permissively licensed opus-mt English pair (dozens), each marked with the caption formats that can carry it (608/708 Latin only; others wait for WebVTT/TTML). Source language stays English in M1 | moved to M2 (M2-1) |

## Warnings and audio level (WP8)

- **Input audio meter** in the Status section: RMS level over 250 ms from the audio tap (−60 to 0 dBFS), with the peak underneath. `Stats` carries `audio_rms_dbfs`, `audio_peak_dbfs` and `audio_silent_s`.
- **Warnings** (`/api/status` → `warnings`) never block saving or starting. They show as amber banners:
  - Input audio below −60 dBFS for over 10 s while video flows (e.g. a muted or unassigned OBS mic).
  - A language in a non-Latin script on 608/708 (needs WebVTT/TTML, not in M1).
  - A Latin-script language whose letters 608 can't all show (on 608 only).
  - A model the configuration needs is not installed, or a language has no model in the registry (WP7, `multi::models::missing_models`; only for the default workers).

## Exit criteria

All met except FR/DE on CC2/CC4 (dropped); see [M1 results](findings/M1-results.md).

- `multi models pull` installs the default model set on a clean machine, with checksums verified.

- 4-hour live run: no video interruptions, memory flat, no errors.
- EN caption lag P95 ≤ 1.5 s; translated ≤ 2.5 s.
- Killing the ASR or translation worker never interrupts video; captions return within 5 s.
- A blocklisted word never appears in any caption language.
- Captions visible on YouTube via RTMP; EN/ES in VLC; FR/DE via the CC2/CC4 option. *(YouTube: met, English only. CC2/CC4: dropped — GStreamer's encoders support CC1/CC3 only.)*
- Set up, start and monitor a stream using only the web GUI.
- CI green on `main`.

## Code layout

| Path | What |
|---|---|
| `crates/multi-media` | GStreamer pipeline: input, restamping bridge, caption lanes (`CaptionHandle`), outputs, audio tap, `Stats` |
| `crates/multi-core` | Config (`Config::validate` reports issues by setting path), `Word`/`Clause`/`Translation`, worker IPC framing; caption quality: `segment`, `filter` (lists in `data/`), `clean`, `degrade`, `quality` (run-loop glue) |
| `crates/multi` | The `multi` binary: `multi run`, `multi serve`, `multi config default`, `multi config check`; `supervisor` (worker processes), `run` (wiring), `service` (pipeline lifecycle, status, events), `web` (GUI + API, page in `crates/multi/web/`), `segment` (wall-clock adapter over `multi_core::segment`) |
| `crates/multi-asr` | ASR worker: Nemotron 3.5 Streaming (sherpa-onnx) or Whisper (whisper.cpp, M2-5) + Silero VAD |
| `crates/multi-mt` | Translation worker: opus-mt via CTranslate2 (ct2rs) |
| `crates/multi-fake-worker` | Scripted worker for tests (no models) |
| `spikes/` | M0 reference code; not built by the root workspace |

## Workers

ASR and translation run in child processes so a crash, hang or CUDA fault never touches video. Each speaks `multi_core::ipc` on stdin/stdout and logs on stderr (the supervisor re-logs each line with the worker's name).

**Protocol.** Main → worker: PCM frames (`start_ms` + 16 kHz mono i16) for ASR, `Translate { clause, langs }` for MT, `Shutdown`. Worker → main: `Ready` once models are loaded, `Heartbeat` every 500 ms from a separate thread (also during loading and decoding), `Words` (times on the incoming PCM timeline), one `Translated` or `Error` per requested language. A worker withholds heartbeats if one decode or translation runs over 10 s, so a stuck model looks hung.

**Restart policy** (`multi::supervisor::Policy`): no frame for 3 s, process exit, or broken framing → kill and restart after 0.5 s, doubling to 10 s; the delay resets once a worker has been ready for 60 s. Nothing is written to a new worker until it says `Ready`. `send` never blocks: frames wait in a bounded queue (256) that drops the oldest when full, counted in `status().dropped_in`. `status()` gives state (starting, ready, restarting, failed, stopped), restarts, last error, drops and pid. Shutdown: `Shutdown`, 2 s grace, then kill; each worker drops its models on their own threads before exiting (S6 gotcha 6).

**MT details.** One thread, model and queue (8) per language: a slow or failed language gets an `Error` and never delays the others. fp16 on CUDA, int8 on CPU; sentence split; decode length cap `4 × words + 16` (≤ 256); a 2 s deadline from arrival stops decoding and returns an `Error`.

**Build and run** (models from `multi models pull`, see Models; the M0 copies in `~/.cache/multi-models/` use the same layout):

```sh
# GPU: sherpa-onnx CUDA prebuilt + CTranslate2 with CUDA (CUDA_PATH/arch in .cargo/config.toml)
export SHERPA_ONNX_LIB_DIR=~/.cache/multi-tools/sherpa/sherpa-onnx-v1.13.8-cuda-13.x-cudnn-9.x-onnxruntime1.28.2-linux-x64-gpu/lib
cargo build --release -p multi-asr -p multi-mt --features multi-mt/cuda,multi-asr/cuda
target/release/multi-asr --model ~/.cache/multi-models/sherpa/sherpa-onnx-nemotron-3.5-asr-streaming-0.6b-560ms-2026-06-11-fp32 --device auto
target/release/multi-mt --models ~/.cache/multi-models/ct2 --langs es,fr,de --device auto
# Real-model check (evidence/WP3): 1x ASR on long.wav, MT latency, kill -9 recovery
cargo run --release -p multi --example wp3_check -- --asr-model-dir … --mt-models ~/.cache/multi-models/ct2 \
  --wav spikes/harness/media/long.wav --reference spikes/harness/media/long.txt
```

**Measured** ([evidence/WP3](evidence/WP3), RTX 3090): ASR at 1× on `long.wav` (8 min): 1355 words, WER 5.8% (simple normaliser), word lag P50 0.56 s / P95 0.96 s, no drops. MT (20 clauses × es/fr/de): P50 16 ms, P95 22 ms. After `kill -9`: ASR words resume in 1.8 s (0.5 s backoff + 1.2 s load), MT in 0.9 s; the frame in flight at the kill is lost.

Without `SHERPA_ONNX_LIB_DIR` the sherpa-onnx build script downloads its CPU prebuilt; without `--features cuda`, CTranslate2 is built CPU-only. `--device auto` tries CUDA and logs a warning when it falls back to CPU. `multi-asr` and `multi-mt` are not default members, so plain `cargo build/test/clippy` skip them; CI checks them in a separate `workers` job (CPU-only, cached). Supervisor tests (`crates/multi/tests/supervisor.rs`) drive the fake worker through crash, hang, garbage output, blocked sends and shutdown.

## Media

`multi run -c multi.toml` starts the pipeline, the ASR worker and the MT worker, logs a `stats` line every 10 s, and stops on Ctrl-C (media first, then workers). Worker binaries default to `multi-asr`/`multi-mt` next to `multi`, with model folders resolved from the registry in the models directory (see Models); `--asr-worker "<path> [args]"` and `--mt-worker "<path> [args]"` replace them (tests use `multi-fake-worker`).

```text
input -> tsdemux -> bridge -> h26xparse -> [caption lanes] -> h26xccinserter -> mpegtsmux -> SRT/UDP outputs
                                                                                   `-> ES -> flvmux -> RTMP outputs
            `-> audio tap: aacparse -> avdec_aac -> 16 kHz mono -> 100 ms PCM frames -> ASR -> segmenter -> source lane
                                                                                            `-> clause -> MT -> other lanes
```

- **Input** `input.url`: `srt://` (caller or listener; `srt.latency_ms` unless the URL sets `latency`), `udp://` (unicast or multicast), `rtp://` (MPEG-TS over RTP). H.264 or HEVC, detected from the stream, parsed but never decoded. The input is rebuilt on error, EOS, or 2 s of silence after data (srtsrc's silent caller, S2); the bridge ([S2](../m0/findings/S2-gstreamer-pipeline.md)) re-stamps onto one continuous output timeline, so a source restarting at PTS 0 continues seamlessly.
- **Outputs** `outputs[].url`: `srt://`, `udp://` (MPEG-TS) and `rtmp(s)://` (FLV, H.264 + AAC; the SEI keeps the captions; needs both video and audio). Each output is its own pipeline, restarted alone with backoff (0.5 s doubling to 10 s); URLs are logged with passphrases and stream keys masked. Optional `name`; `enabled = false` (default true) keeps an output configured but stopped.
- **Per-output control (WP10)**: the outputs are a set keyed by a stable id that changes while running (`Media::outputs()` → `OutputControl`: `add`, `remove`, `set_enabled`, `set_name`, `arrange`). The fan-out probes take a snapshot of the set per buffer (brief read lock to clone an `Arc`, no lock held while pushing); sink pipelines are built and torn down outside the sink's lock. Stopping an output tears its pipeline down; starting builds it again at the next control-loop poll (about every 100 ms while the input is up). Stats carry each output's `id`, `name` and `enabled`.
- **Fallback picture (WP11)** `video.fallback` = `black` (default) | `image` | `off`, `video.fallback_image` (PNG/JPEG, required for `image`), `video.fallback_after_ms` (default 1000, 200–10000). After that long without input data, a generated stream feeds the bridge instead of the input: `videotestsrc pattern=black` or `pngdec|jpegdec ! imagefreeze` (scaled, borders kept), encoded to the last input's codec, size, frame rate and (H.264) profile, as read from the output parser's caps: `openh264enc` for H.264 (BSD; x264enc is GPL and in -ugly), `x265enc` zerolatency for HEVC; keyframe every second, no B-frames; plus `audiotestsrc wave=silence ! avenc_aac` at the input's AAC rate and channels (non-AAC input audio: video only). Fallback samples are re-stamped to running time and the bridge opens a session *without* the startup hold, so output PTS carry on from the last input frame. Captions keep flowing (empty). When the input returns its buffers are dropped until its first IDR/IRAP; then the bridge is reset (a normal held input session) and the fallback torn down. No input seen yet = no fallback. A missing encoder is logged once and the outputs stay idle as before; a failing fallback pipeline is torn down and retried after 10 s; an unreadable image falls back to black. Stats: `fallback_active`, `fallback_activations`; the GUI Input tile shows "Fallback (no input)". Tested by `crates/multi/tests/fallback.rs` (ports 9760–9763, H.264 and HEVC, source stopped 8 s): longest output gap ≈ 1.03–1.08 s (at the loss; the return costs the 0.5 s hold), PTS strictly increasing and PTS span = wall span, FFmpeg decodes across both switches with no errors, CC1 captions back after recovery.
- **Caption lanes** from `languages` (default EN CC1+708 s1, ES CC3+s2, FR s3, DE s4) with `captions.mode`, `rows` and `offset_ms` (≥ 0; negative needs video delay, not built). S2b's `GstCc` with both upstream workarounds. CC2/CC4 are refused (see Caption quality).
- **Audio tap**: first audio track (or `audio.track`), `audio.channel` or downmix, `avdec_aac` only (startup fails without gst-libav); PCM timestamps are the input audio PTS.
- **Segmenter**: see Caption quality.
- **Stats** (`Media::stats()`): frames in/out, sessions, input restarts/errors, per-output id, name, enabled, state and errors, caption frames, per-lane pushed/dropped/queued, audio tap chunks/drops.

Tested in CI by `crates/multi/tests/pipeline.rs` (real GStreamer, fake workers, ports 9720–9722): fake words on CC1, `[es] …` on CC3, captions over RTMP (FFmpeg as RTMP server), video and captions continue after `kill -9` of the ASR worker and after a source restart, clean Ctrl-C exit. **Measured** ([evidence/WP2](evidence/WP2), real workers, RTX 3090, 180 s): video delay p50 33.6 ms / p99 34.2 ms; EN caption lag p50 0.96 s / p95 1.08 s; ES on CC3; no drops or restarts.

## Web GUI

`multi serve -c multi.toml` (same `--asr-worker`/`--mt-worker`/`--models-dir` options as `multi run`) serves the GUI at **http://127.0.0.1:8480/** (`web.bind`, `web.port`; **https://** when TLS is on, see below). The config file is created with defaults if missing. The pipeline starts when you click Start, or at launch if `web.autostart = true`. Ctrl-C/SIGTERM stops the pipeline, then the server.

One dashboard page, top to bottom, with a left sidebar that links to each section and to each settings group (the section in view is highlighted; on narrow screens the sidebar becomes a row of links):

- **Status**: Start/Stop, input live/no signal, frames in/out (and fps), caption lag (audio position minus the end of the latest ASR word), outputs (one row per configured output with Start/Stop and a Stopped state), worker state and restarts, lanes, GPU/VRAM (`nvidia-smi` every 5 s), and the last 50 errors.
- **Live captions**: one rolling panel per language, from `/api/events`.
- **Settings** (each group, e.g. Input, Outputs, Languages, collapses on its own and starts closed; a group opens automatically when an error or warning links to one of its fields; the save bar shows while any group is open or there are unsaved changes): every config section with help text from PRD §5, helper fields for SRT/UDP/RTP/RTMP URLs, a language table, and errors shown next to the field (`Issue.path`). Save writes the TOML atomically (mode 0600). A banner lists saved settings that wait for a restart. Rule list in `service::RULES`: `web.token`, `web.autostart`, `web.username` and `web.password_hash` apply live, `web.bind`/`web.port`/`web.tls`/`web.tls_cert`/`web.tls_key` need `multi serve` restarted, and everything else restarts the pipeline. When stopped, everything applies at the next Start.
- **API**: `GET/PUT /api/config` (PUT gives 422 with `{issues:[{path,message}]}`), `GET /api/config/default`, `POST /api/start`, `POST /api/stop`, `POST /api/outputs/{index}/start` and `/stop` (set `outputs[index].enabled`, save atomically, apply live; 404 for an unknown index), `GET /api/status`, `GET /api/events` (SSE: `{"type":"stats",…}` each second, `{"type":"caption","lang","text","new_row"}` per line), `GET /api/me` (`{via: session|token|local, user, password_set, tls}`). POST/PUT need the header `X-Multi: 1` (blocks cross-site forms).
- **Sign-in** (WP9, `crates/multi/src/auth.rs`): people sign in at `/login` with `web.username` (default `admin`) and a password stored only as an argon2id PHC string in `web.password_hash`. Set it with `multi passwd -c multi.toml [--username NAME]` (asks twice without echo, at least 8 characters, writes the file atomically with mode 0600; `--password-stdin` for scripts; restart a running `multi serve` afterwards), or on the first-run page `/setup`. Sign-in gives a session cookie: 256 random bits, kept in memory only (a restart signs everyone out), HttpOnly, SameSite=Strict, Secure under HTTPS, ending after 12 h idle or 7 days. **Log out** in the header (`POST /logout`) ends it. **Once a password is set, every client signs in, this machine included.** Failed sign-ins are limited per client address: after 5, the address waits 30 s, doubling with each further failure up to 15 min, and the page says how long.
- **First run**: with no password, a browser on the same machine (loopback bind, loopback client) has full access as before and sees a banner linking to `/setup`. `/setup` answers only loopback clients whose `Host` is local, refuses a foreign `Origin`, and is disabled once a password exists; anyone else is told to use `multi passwd` or this machine. Behind a reverse proxy on the same host every client looks local, so set a password first.
- **Scripts**: `Authorization: Bearer <token>` (`MULTI_WEB_TOKEN`, which wins, or `web.token`) still works, compared in constant time. On a non-loopback bind `multi serve` refuses to start without a password or a token.
- **HTTPS**: `web.tls = "auto"` (default: on when `web.bind` is not a loopback address), `"on"` or `"off"`. With TLS on there is no plain-HTTP listener (rustls, ring provider; works offline). Without `web.tls_cert`/`web.tls_key` (PEM, relative to the config file), a self-signed certificate for localhost, 127.0.0.1, ::1, the host name and the bind address is made on first use as `multi-web-cert.pem`/`multi-web-key.pem` (0600) next to the config file and reused afterwards; its SHA-256 fingerprint is logged at every start. Browsers warn about a self-signed certificate ("not private"/"potential security risk"): compare the fingerprint in the browser's certificate viewer with the log line, then accept it once per browser. Use your own certificate (e.g. from an internal CA) to avoid the warning. `"off"` on a network address logs a warning: passwords would cross the network in plain text.
- **Security**: GETs mask stream keys, SRT passphrases/stream ids, the token and the password hash. A masked value sent back keeps the stored one; the API never changes the password hash (a save keeps the one on disk). On a loopback bind the `Host` header must be local (blocks DNS rebinding). POST/PUT under `/api` need `X-Multi: 1`; `/login`, `/logout` and `/setup` refuse a foreign `Origin`/`Sec-Fetch-Site`.

Tested by unit tests in `web.rs`/`service.rs`/`auth.rs` (config round trip, 422 paths, masking incl. the password hash, token and host rules, argon2 hash/verify, sign-in success/failure, rate limit, session idle/absolute expiry on an injected clock, logout, `/setup` only from loopback and only once, Bearer with a password, HTTPS with a generated certificate and no plain-HTTP answer) and `crates/multi/tests/web.rs` (start/stop with fake workers, then `multi serve` end to end: PUT config, start, `source.sh` input, a caption arrives over SSE, clean SIGINT exit; `multi passwd --password-stdin` writes a hash that verifies; ports 9740–9745).

## Caption quality

WP4. Text path: ASR words → `clean` → segmenter → filter → source lane; clause (clean, unfiltered, so MT sees real words) → MT → stale check → `clean` → filter → lane.

- **Segmenter** (`multi_core::segment`, explicit ms clock): a clause closes on `.`/`!`/`?`, on a gap ≥ 350 ms between word timestamps (VAD proxy), after `translate.max_wait_ms` without words, or at 24 words (cut after the last comma). At least 3 words unless the pause is ≥ 1 s or the stream ends. Replaying S4's `long.words.tsv` (8 min, no punctuation) with 560 ms chunking: 180 clauses, mean 7.5 words; 43% close on a word-gap pause, 56% on the timer, but 48 of those 56 points are at a real pause (the timer only beat the gap detector, which sees the gap when the next word arrives); 8% are mid-speech timer cuts (S6: ~70% on the timer). Nemotron's sparse punctuation (S6 gotcha 4) makes punctuation closes rare.
- **Filter** (`multi_core::filter`, FL-4): whole-word, case- and accent-insensitive, `*` wildcard, phrases; allowlist wins. Built-in LDNOOBW lists for en/es/fr/de (CC-BY-4.0, [data/README.md](../../crates/multi-core/data/README.md)); each lane uses its own list plus the source language's, plus `filter.blocklist`. Mask styles `asterisks`, `first-letter`, `bleep`, `drop`. Applied to source text and to every translation. A unit test runs every list entry through case, accent and punctuation variants in all styles; the pipeline test blocklists a fake-ASR word and checks it never reaches CC1 or CC3. `Config::validate` rejects unusable entries (`*` alone, symbols).
- **Cleaning** (`multi_core::clean`): valid UTF-8, no control characters, `©`→`(c)`, `®`→`(R)`, `™`→`TM`, `…`→`...`, `€`→`EUR`, `œ`→`oe`, curly quotes and dashes to ASCII, emoji and zero-width characters removed.
- **Lanes**: text older than 6 s is dropped on arrival (a translation's age counts from its clause closing: `CaptionHandle::push_aged`) or while queued, counted in `LaneStats::stale`; the existing chars-based backlog cap stays. A failed push into the encoder (flow error) now drops that text and rebuilds the encoder, like bus errors and stalls already did (WP2). `LaneStats::oldest_ms` gives queue age.
- **CC2/CC4: not supported, refused by `Config::validate`.** CC2/CC4 share field 1/2 with CC1/CC3; `tttocea708`'s `cea608-channel` supports only 1 and 3 (GStreamer 1.28.7), `tttocea608` writes CC1 only, and two encoders cannot share one field's two bytes per frame without a 608 multiplexer that interleaves control and text codes; S2b also found separate 608 lanes trail by 22 frames. FR/DE stay on 708 services. Doing it needs our own 608 encoder/mux (revisit with S2b's `cc` crate).
- **Degrade** (`multi_core::degrade`, pure): lane lag = max(queue age, last translation latency, age of the oldest outstanding translation). Above `degrade.max_lag_ms`, pause the lowest-priority translated lane (one per 3 s); below half the limit for 10 s, resume the highest-priority paused lane. Paused lanes are left out of MT requests. The source lane is never paused; `model` and `pass-through` steps are not built. Checked once a second from the run loop; changes logged as warnings; a `caption quality` line (clause split, masked, stale, paused) follows each stats line.

## Models

Models are never bundled; `multi models` fetches them into the models directory and checks them against a registry compiled into the binary ([`crates/multi-core/data/models.toml`](../../crates/multi-core/data/models.toml), parsed by `multi_core::models`). Design: [PRD §7](../prd/07-proposed-architecture-and-technology.md) "Model storage and download".

M2-1 adds the user catalogue (`~/.config/multi/models.toml`), the expanded opus-mt catalogue and the GUI Models group: see [M2 Models](../m2/README.md#models-m2-1).

**Directory:** `--models-dir` > `$MULTI_MODELS` > `$XDG_DATA_HOME/multi/models` > `~/.local/share/multi/models`. Inside: each model's `dir` from the registry (`sherpa/…`, `ct2/opus-mt-en-es`), `.tmp/` for downloads and conversions in progress, `.venv/` for the conversion tools. The layout matches the M0 cache, so `--models-dir ~/.cache/multi-models` keeps working. Offline sites copy the folder and run `verify`.

```sh
multi models list                  # registry, installed/missing, disk and VRAM MB, licence
multi models pull                  # default set; or: multi models pull silero-vad opus-mt-en-es
multi models verify [ids…]         # SHA-256 against the registry, or manifest.json for converted models
multi models remove <id>
```

- **Downloads** (`files`, `archive`): HTTPS only (ureq + rustls), written to `<file>.part`, resumed with HTTP Range after an interruption (3 retries, and across runs), size and SHA-256 checked, then renamed into place; a mismatch deletes the `.part`. Archives (`.tar.bz2`) are checked, extracted to `.tmp/`, their files checked against the registry, then moved into place.
- **Conversion** (`convert`, opus-mt): `scripts/convert-opus-mt.sh <hf-repo> <revision> <out-dir>` (embedded in the binary) makes `<models>/.venv` with pinned ctranslate2 4.8.2, transformers 4.57.6, torch 2.14.0 (CPU) and runs `ct2-transformers-converter --quantization float16`. `pull` then writes `manifest.json` (repo, revision, SHA-256 and size of each file). Needs Python 3 with venv (`$MULTI_PYTHON` picks one); without it `pull` says so. Models copied in by hand without a manifest verify as "present, unverified". Hosted pre-converted copies (PRD) are not built yet.
- **Workers:** `multi run`/`serve` look up `asr.model` (an id, or `<model>-<chunk_ms>ms`, so the default `nemotron-3.5-streaming` + 560 is `nemotron-3.5-streaming-560ms`; its int8 `cpu_variant` is used when only that is installed), `silero-vad` (`--vad-model`), and the `en->xx` model per target language (`multi-mt --model xx=<dir>`). A missing model fails the start naming `multi models pull <id>`. `multi::models::missing_models(config, dir)` returns the same as config `Issue`s (`asr.model`, `languages[i].code`, also for a language with no registry model) for the GUI warnings (WP8).

| id | kind | source | disk MB | licence |
|---|---|---|---|---|
| `nemotron-3.5-streaming-560ms` * | asr | files, [csukuangfj2 sherpa-onnx export](https://huggingface.co/csukuangfj2/sherpa-onnx-nemotron-3.5-asr-streaming-0.6b-560ms-2026-06-11) @ `2072aba9` | 2594 | OpenMDW-1.1 (NVIDIA) |
| `nemotron-3.5-streaming-560ms-int8` * | asr | archive, sherpa-onnx release `asr-models` | 682 (475 download) | OpenMDW-1.1 (NVIDIA) |
| `silero-vad` * | vad | file, sherpa-onnx release `asr-models` | 1 | MIT |
| `opus-mt-en-es` / `-fr` * | mt | convert, Helsinki-NLP @ `5bc4493d` / `dd7f6540` | 152 / 146 | Apache-2.0 |
| `opus-mt-en-de` * | mt | convert, Helsinki-NLP @ `6183067f` | 144 | CC-BY-4.0 |
| `opus-mt-tc-big-en-pt` * | mt | convert, Helsinki-NLP @ `9f2863d8` | 448 | CC-BY-4.0 |

\* default set. `pull` prints each model's licence and attribution; CC-BY models need the attribution in MULTI's notices. Every registry entry must have a licence and attribution (checked when parsing). Tests (`crates/multi/tests/models.rs`, no network): local HTTP server with fake files for pull, checksum mismatch, interrupted and earlier-run resume, archives, verify/remove, and verify against a fake manifest. Local check: [evidence/WP7](evidence/WP7).

## Log

Newest first.

- 2026-09-28 — **M1 done.** Live OBS test (login, fallback picture, per-output control) and YouTube RTMP test passed; YouTube shows English only. See [M1 results](findings/M1-results.md).
- 2026-09-28 — WP9 in review: username/password sign-in (argon2id, `multi passwd`, loopback-only `/setup`), sessions with expiry, per-IP back-off, Log out, Bearer token kept for scripts, built-in HTTPS (`web.tls`, self-signed certificate made on first use).
- 2026-09-28 — Added WP12 (models in the GUI, user-editable catalogue, more languages) to the to-do.
- 2026-09-28 — WP6: B-frame test passed; 4-hour soak passed (video never interrupted, +33.8 ms, 0 errors, memory flat, worker kill recovery 1.5 s / 0.8 s, EN caption lag p95 1.20–1.28 s). Added WP9–WP11 to the to-do.
- 2026-09-27 — WP7 in review: model registry, `multi models list/pull/verify/remove`, XDG models directory, workers resolve models from the registry, `missing_models` for WP8. Local pull/verify in [evidence/WP7](evidence/WP7).
- 2026-09-27 — WP4 in review: segmenter, word filter, text cleaning, age cap, flow-error rebuild, degrade policy; CC2/CC4 refused. No real-model run (worker binaries not built here).
- 2026-09-25 — WP5 in review: `service` control layer (`multi run` now uses it), `multi serve` web GUI and API.
- 2026-09-25 — WP2 in review: `multi-media`, `multi run` wiring, pipeline integration test. Real-model numbers in [evidence/WP2](evidence/WP2).
- 2026-09-25 — WP3 in review: workers, supervisor, fake worker, CI `workers` job. Real-model numbers in [evidence/WP3](evidence/WP3).
- 2026-09-25 — M1 started: WP1 skeleton.
