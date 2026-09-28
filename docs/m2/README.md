# M2 — More languages

> **Summary:** Multi-language captions viewers can choose, many more languages, non-English speakers, and model management in the GUI. Starts with a research spike (R1) on YouTube captions, HLS + WebVTT, and non-English speech accuracy; then four work packages.

## Work packages

| WP | Contents | Status |
|---|---|---|
| R1 Research | YouTube multi-language live captions (HTTP caption ingestion, HLS ingest); HLS + WebVTT subtitle renditions from GStreamer 1.28; Nemotron vs Whisper accuracy for ES/FR/DE/PT speech; available opus-mt pairs | in progress |
| M2-1 Models + languages | Installed models vs catalogue (built-in registry + user-editable `~/.config/multi/models.toml`); GUI Models section (list, pull with progress and licence, verify, remove); GUI model/language choices limited to installed models; catalogue grown to every permissively licensed opus-mt English pair, each marked with the caption formats that can carry it | not started |
| M2-2 Non-English speakers | Selectable source language; ASR model per language (per R1); translation direct or pivoting through English; segmenter and filter per source language | not started |
| M2-3 Web output: HLS + WebVTT | `hls` output served by MULTI: video pass-through plus one WebVTT subtitle rendition per language (any script); viewer page with a language picker | not started |
| M2-4 YouTube per-language | Only if R1 shows YouTube accepts per-language live captions over HTTP; otherwise document the limit and point multi-language viewers to M2-3 | not started |

Order: R1 → M2-1 and M2-3 in parallel → M2-2 → M2-4.

## Why

M1's YouTube test showed captions working but only one track (English): embedded 608/708 on YouTube isn't enough for multi-language viewers. Non-Latin languages (Arabic, Chinese, Japanese…) can't go on 608/708 at all, so WebVTT output unlocks them.

## Log

Newest first.

- 2026-09-28 — M2 started: R1 research spike.
