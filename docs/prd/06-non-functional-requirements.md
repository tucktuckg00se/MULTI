# Non-functional requirements

> **Summary:** Video delay is the engineer's choice (default 0 ms pass-through). Latency budget ~1.7 s source / ~2.2 s translated; measured in M0: 1.13 s / ≈1.97 s, video +33.7 ms. Platform tiers. Reliability rules: video never stops for captions, crash isolation, graceful degradation, 30-day soak.

Captions can only appear after the words are spoken, so how much to delay the video is the engineer's call. It is one setting, `video.delay_ms`.

- **0 ms (default, pass-through):** video goes out with under 250 ms added; captions trail speech by the caption lag below. This matches how live TV captioning already behaves.
- **Around the measured caption lag (e.g. 2,500 ms):** captions land on the right frames, at the cost of a delayed stream.
- **Anything in between**, plus `captions.offset_ms` for fine-tuning. The UI suggests a value from measured lag.

**Latency budget, speech to caption on screen, balanced preset**

Measured in M0 on an RTX 3090 ([S6](../m0/findings/S6-end-to-end.md)); stages without their own number were measured only as part of the total.

| Stage | Target (P95) | Measured (M0, P95) |
| --- | --- | --- |
| Audio buffering + VAD | 300 ms | in total below |
| Streaming speech-to-text (partial result) | 800 ms | in total below |
| Translation per language (parallel) | 500 ms | 38 ms max (11 ms mean) |
| Filter + caption formatting | 50 ms | in total below |
| Insertion + 608/708 encoding rate limit | 500 ms | in total below |
| **Total, source language** | **~1.7 s (limit 2.5 s)** | **1.13 s** |
| **Total, translated language** | **~2.2 s (limit 3 s)** | **≈1.97 s** (mostly the 800 ms clause timer) |
| Video pass-through (not in caption path) | < 250 ms | 33.7 ms p50, 34.0 ms p95 |

**Platforms**

| Platform | Tier | Acceleration |
| --- | --- | --- |
| Linux x86-64 (Ubuntu 22.04+/Debian 12+, glibc) | Tier 1 | NVIDIA CUDA, CPU |
| Linux ARM64 (Jetson Orin, Raspberry Pi 5, Ampere) | Tier 1 | CUDA on Jetson, CPU elsewhere |
| Windows 10/11 x86-64 | Tier 2 | NVIDIA CUDA, CPU |
| Windows ARM64 | Tier 3 (community) | CPU |
| macOS Apple Silicon | Tier 3 (community) | Metal, if backend supports it |

Tier 1 = built and tested on every release; Tier 2 = built every release, tested before major releases; Tier 3 = builds from source, best effort.

**Reliability (priority 1) and performance (priority 2)**

- **Video never stops because of captions.** A failure in any caption stage leaves video flowing untouched; the stream continues without captions, an alert fires, and the stage restarts within 5 s.
- **Crash isolation.** Speech and translation run as supervised worker processes, so a crash inside a C/C++ inference library cannot take down the video path.
- **Graceful degradation under load.** When caption lag passes a limit, MULTI sheds work in a configurable order: drop the lowest-priority languages, switch to a smaller model, go source-language only, and finally pass video through uncaptioned. It recovers automatically when load drops.
- **Soak testing.** 7-day run per milestone, 30-day run before each release: no unplanned restarts, memory growth under 5%.
- **Defensive code.** No panics on the video path (enforced by lint); every stream parser (MPEG-TS, SEI, SRT, RTP) is fuzzed in CI; a bad config reload is rejected and the running config kept.
- **Supervision.** Health endpoint and watchdog; systemd and Windows service units restart the process if it ever exits.
- Minimum reference hardware: one 1080p stream, 1 source + 3 translated languages on an 8 GB NVIDIA GPU (RTX 3060 class); one stream, source language only, on a Jetson Orin Nano.
- Accuracy target: word error rate within 3 points of the chosen model's published benchmark on our test set.

**Security and privacy**

- No telemetry by default; no network calls except configured streams and optional model download.
- Web UI and API bound to localhost by default, with token auth when exposed.

