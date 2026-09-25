# Milestones

> **Summary:** M0 spikes, M1 MVP, M2 multilingual, M3 operable, M4 platforms + beta, v1.0 launch; scope, exit criteria and rough durations.

Durations assume one or two part-time developers; they are sizing guesses to revisit after M0.

| Milestone | Scope | Exit criteria | Est. duration |
| --- | --- | --- | --- |
| M0 — Spikes (done 2026-09-25) | FFmpeg vs GStreamer pipeline; 608/708 SEI insertion; streaming Whisper vs Parakeet latency | Captions visible in VLC/ffplay from a live SRT feed; latency numbers measured | 3–4 weeks |
| M1 — MVP | SRT/UDP in and out, source-language 608/708, blocklist + profanity filter, CUDA + CPU, Linux x64, config file | 4-hour stream runs clean; caption lag under 3 s P95 | 6–8 weeks |
| M2 — Multilingual | Parallel translation, 4 languages on CC1–CC4 and 708 services, WebVTT sidecar | 1 + 3 languages on an RTX 3060 in real time | 4–6 weeks |
| M3 — Operable | Web UI, REST API, Prometheus metrics, Docker, reconnect logic, RTMP, presets and live-reload settings, second speech backend, WebVTT/SRT outputs | 7-day soak test passes | 4–6 weeks |
| M4 — Platforms + beta | Linux ARM64 / Jetson, Windows x64, signed builds, installer; 5–10 beta sites | Beta users run weekly streams without us on the call | 6–8 weeks |
| v1.0 — Launch | Paid binaries, licence keys, warranty and support terms live | 30-day soak test passes; first paying customers | — |
| Later | Burn-in companion tool, Teletext, SCC/MCC files, DVB Subtitles, IMSC1/TTML, YouTube HTTP captions, HLS output, remote model backend, audio bleeping, speaker labels, AMD/Intel acceleration | — | — |

