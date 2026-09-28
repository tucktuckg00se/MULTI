# M2 — More languages

> **Summary:** Multi-language captions viewers can choose, many more languages, non-English speakers, and model management in the GUI. Starts with a research spike (R1) on YouTube captions, HLS + WebVTT, and non-English speech accuracy; then four work packages.

## Work packages

| WP | Contents | Status |
|---|---|---|
| R1 Research | Done. Findings: [YouTube captions](findings/R1-youtube-captions.md), [HLS + WebVTT](findings/R1-hls-webvtt.md), [non-English ASR](findings/R1-non-english-asr.md) | done |
| M2-1 Models + languages | Installed models vs catalogue (built-in registry + user-editable `~/.config/multi/models.toml`); GUI Models section (list, pull with progress and licence, verify, remove); GUI model/language choices limited to installed models; catalogue grown to every permissively licensed opus-mt English pair, each marked with the caption formats that can carry it | not started |
| M2-2 Non-English speakers | Selectable source language; ASR per language per R1: Nemotron with the language set explicitly (WER ES 6.5, FR 9.9, DE 10.4, PT 6.5 on MLS), Whisper turbo as the accuracy option (recommended for DE); translation direct or pivoting through English; segmenter and filter per source language | not started |
| M2-3 Web output: HLS + WebVTT | `hls` output served by MULTI: video pass-through plus one WebVTT subtitle rendition per language (any script); viewer page with a language picker. Per R1: GStreamer has no HLS element for WebVTT; MULTI writes the WebVTT segments and playlists itself (with `X-TIMESTAMP-MAP`) next to an A/V-only HLS | not started |
| M2-4 YouTube per-language | R1: YouTube allows **one** live caption track per broadcast (embedded 608/708 and HTTP POST alike). So: a caption language per YouTube output (one broadcast per language, video stream-copied), and optionally uploading every language's captions to the recording after the stream (`captions.insert`, needs OAuth) | not started |

Order: R1 → M2-1 and M2-3 in parallel → M2-2 → M2-4.

## Why

M1's YouTube test showed captions working but only one track (English): embedded 608/708 on YouTube isn't enough for multi-language viewers. Non-Latin languages (Arabic, Chinese, Japanese…) can't go on 608/708 at all, so WebVTT output unlocks them.

## Log

Newest first.

- 2026-09-28 — R1 done: YouTube allows one live caption track per broadcast; HLS + WebVTT needs our own VTT segments/playlists; Nemotron is usable for ES/FR/DE/PT, Whisper better for DE.
- 2026-09-28 — M2 started: R1 research spike.
