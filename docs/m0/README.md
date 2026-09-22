# M0 — Spikes

> **Summary:** Throwaway Rust experiments that answer the questions the architecture depends on: pipeline base, caption insertion, streaming ASR, translation latency. Each spike ends in a finding; the findings feed ADRs. Exit: live captions from an SRT feed visible in VLC, with measured latency.

## Spikes

| ID | Question | Crate | Status | Finding |
|---|---|---|---|---|
| S1 | Can FFmpeg libraries pass video through and inject caption SEI without re-encode? | `spikes/s1-ffmpeg-pipe` | not started | — |
| S2 | Same, with GStreamer | `spikes/s2-gst-pipe` | not started | — |
| S3 | Pure-Rust CEA-608/708 encoder + SEI builder that real decoders accept | `spikes/cc` | not started | — |
| S4 | Which streaming ASR gives the best lag/accuracy/VRAM trade-off? | `spikes/s4-asr` | not started | — |
| S5 | Can small local models translate a clause in ~500 ms? (stretch) | `spikes/s5-translate` | not started | — |
| S6 | End to end: OBS mic → SRT → captions → VLC and YouTube | `spikes/s6-e2e` | not started | — |

Status values: not started, in progress, done, dropped.

## Test harness

Everything in `spikes/harness/`. Run from the repo root.

| Tool | What it does |
|---|---|
| `fetch-media.sh` | Downloads public-domain test speech + reference transcripts into gitignored `media/` |
| `source.sh` | Plays a test stream (test pattern, burned-in timecode, speech) out over SRT or UDP, H.264 or HEVC |
| `verify.sh` | Extracts embedded captions from a stream or file and diffs them against expected text |
| `latency` (Rust bin) | Measures video pass-through delay and caption lag |

## Log

Newest first. One line per notable event, with a link if there's more.

- 2026-09-22 — M0 started: docs structure, harness and spike workspace set up.
