# Milestones

> **Summary:** M0 spikes, M1 MVP, M2 multilingual, M3 operable, M4 platforms + beta, v1.0 launch; scope, exit criteria and rough durations.

Durations assume one or two part-time developers; they are sizing guesses to revisit after M0.

| Milestone | Scope | Exit criteria | Est. duration |
| --- | --- | --- | --- |
| M0 — Spikes (done 2026-09-25) | FFmpeg vs GStreamer pipeline; 608/708 SEI insertion; streaming Whisper vs Parakeet latency | Captions visible in VLC/ffplay from a live SRT feed; latency numbers measured | 3–4 weeks |
| M1 — First real version (done 2026-09-28) | SRT/UDP in, SRT/UDP/RTMP out, EN speech → EN/ES/FR/DE captions (608 + 708), supervised workers, word filter, models CLI, web GUI with login/HTTPS, per-output control, fallback picture (pulled forward from M2/M3) | 4-hour soak clean; EN lag p95 ≤ 1.5 s; live and YouTube tests ([results](../m1/findings/M1-results.md)) | done |
| M2 — More languages | Models in the GUI + user-editable catalogue; many more translation languages; non-English speakers; HLS + WebVTT web output with a language picker; YouTube per-language captions if possible ([M2 plan](../m2/README.md)) | Viewers choose among ≥ 4 caption languages on the web; ES/FR/DE/PT speech captioned at usable accuracy | — |
| M3 — Operable | Prometheus metrics, Docker, presets, second speech backend in the GUI, remaining live-reload settings (web UI, REST API, RTMP and reconnect were done in M1) | 7-day soak test passes | — |
| M4 — Platforms + beta | Linux ARM64 / Jetson, Windows x64, signed builds, installer; 5–10 beta sites | Beta users run weekly streams without us on the call | 6–8 weeks |
| v1.0 — Launch | Paid binaries, licence keys, warranty and support terms live | 30-day soak test passes; first paying customers | — |
| Later | Burn-in companion tool, Teletext, SCC/MCC files, DVB Subtitles, IMSC1/TTML, YouTube HTTP captions, remote model backend, audio bleeping, speaker labels, AMD/Intel acceleration | — | — |

