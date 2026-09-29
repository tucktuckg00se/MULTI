# M2 — More languages

> **Summary:** Multi-language captions viewers can choose, many more languages, non-English speakers, and model management in the GUI. Starts with a research spike (R1) on YouTube captions, HLS + WebVTT, and non-English speech accuracy; then four work packages.

## Work packages

| WP | Contents | Status |
|---|---|---|
| R1 Research | Done. Findings: [YouTube captions](findings/R1-youtube-captions.md), [HLS + WebVTT](findings/R1-hls-webvtt.md), [non-English ASR](findings/R1-non-english-asr.md) | done |
| M2-1 Models + languages | Installed models vs catalogue (built-in registry + user-editable `~/.config/multi/models.toml`); GUI Models section (list, pull with progress and licence, verify, remove); GUI model/language choices limited to installed models; catalogue grown to every permissively licensed opus-mt English pair, each marked with the caption formats that can carry it. See [Models](#models-m2-1) | in review |
| M2-2 Non-English speakers | Selectable source language; ASR per language per R1: Nemotron with the language set explicitly (WER ES 6.5, FR 9.9, DE 10.4, PT 6.5 on MLS), Whisper turbo as the accuracy option (recommended for DE); translation direct or pivoting through English; segmenter and filter per source language | not started |
| M2-3 Web output: HLS + WebVTT | `hls` output served by MULTI: video pass-through plus one WebVTT subtitle rendition per language (any script); viewer page with a language picker. Per R1: GStreamer has no HLS element for WebVTT; MULTI writes the WebVTT segments and playlists itself (with `X-TIMESTAMP-MAP`) next to an A/V-only HLS | in review |
| M2-4 YouTube per-language | R1: YouTube allows **one** live caption track per broadcast (embedded 608/708 and HTTP POST alike). So: a caption language per YouTube output (one broadcast per language, video stream-copied), and optionally uploading every language's captions to the recording after the stream (`captions.insert`, needs OAuth) | not started |

Order: R1 → M2-1 and M2-3 in parallel → M2-2 → M2-4.

## Models (M2-1)

**Two lists.** *Installed* = on disk in the models directory. *Catalogue* = the built-in registry ([`models.toml`](../../crates/multi-core/data/models.toml)) merged with a user file of the same schema: `--catalogue <path>` > `$MULTI_CATALOGUE` > `$XDG_CONFIG_HOME/multi/models.toml` > `~/.config/multi/models.toml`. A user entry with a known `id` replaces the built-in one, a new `id` is added; each entry gets the registry checks (licence, attribution, source, SHA-256, no `..`), and a bad one is reported and skipped (`multi models list`, GUI banner), never fatal. `multi models list` has a `from` column (builtin/user).

**Catalogue.** 120 opus-mt `convert` entries (revisions pinned, Apache-2.0 or CC-BY-4.0, CC-BY attribution in the entry): every en→X pair with an ISO 639-1 target (78 languages, plain and `tc-big` where both exist), and X→en for the 23 Nemotron languages that have one (plus `opus-mt-ROMANCE-en` as pt→en). Skipped: 3-letter/low-resource targets (mostly JW300/Bible data), group models (`en-gem`, `en-zle`…), `en-jap`. Source list: [evidence/M2-1](evidence/M2-1/opus-mt-catalogue.txt). A `language` table gives each code a name and script (81 languages with a model, 65 Latin; also Cyrillic, Arabic, CJK, Hangul, Devanagari, Greek, Hebrew, Armenian, Malayalam, Ethiopic). Formats follow the script: 608 and 708 Latin only (`latin_extended` adds the 608 missing-letters note; the list matches `Config::warnings`), WebVTT any script. Multi-target models carry `target_token` (`>>cmn_Hans<<`); `multi run` passes it as `multi-mt --prefix lang=token`. The worker picks an installed model first when several serve one pair.

**API** (`web_models.rs`, behind the guard; DELETE now also needs `X-Multi: 1`): `GET /api/models` (catalogue + status, size, licence, languages, formats, in-use, pull queue), `POST /api/models/{id}/pull` (background, one at a time, others queue; 202), `POST /api/models/{id}/verify`, `DELETE /api/models/{id}` (409 when the running pipeline uses it). Progress: SSE `/api/models/events` (`queued`, `progress` {phase download/extract/convert/verify, done, total bytes}, `done`, `failed`, `removed`).

**GUI.** Models group: installed (size, licence, in-use, verify/remove), catalogue with filter, licence and attribution before pulling, pull button and progress bar. Languages group: a dropdown of the installed ASR languages (spoken) or installed translation targets; a configured language without a model shows "Install model", linking to its catalogue row; each row shows the formats that can carry it. `#sec-a,b` in the URL opens those groups.

**Tests:** catalogue merge and bad entries, expanded registry (licence, script, pinned revision per entry), installed-first model choice (multi-core); API list/pull/verify/remove against the local HTTP server and in-use resolution (`crates/multi/tests/models.rs`).

## HLS web output (M2-3)

An output `hls://<name>?segment_s=2&window=6` (defaults shown; `segment_s` 1–10, `window` 3–30) serves HLS with one WebVTT subtitle rendition per language, in any script. It sits in the live output set like SRT/UDP/RTMP (start, stop, add, remove without touching the others).

- **Audio/video:** `hlssink2` fed from the elementary streams entering the output muxer (the same branch as RTMP): stream-copied MPEG-TS segments `v%05d.ts` and `video.m3u8`. No keyframe requests (there is no encoder), so segments are cut on the first keyframe after `segment_s`: the 1 s GOP input gives 2.0 s segments; longer GOPs mean longer segments. H.264 plays everywhere; HEVC-in-TS is written but most browsers can't play it. Like RTMP, it needs both audio and video.
- **Subtitles:** MULTI writes `sub_<lang>_<seq>.vtt` and `sub_<lang>.m3u8` itself (`multi-media/src/hls.rs`), one VTT segment per A/V segment, same sequence numbers and durations. Each carries `X-TIMESTAMP-MAP=MPEGTS:<first video PTS of the A/V segment>,LOCAL:00:00:00.000`. Cue text is the lanes' text (after cleaning and the word filter); a cue shows from the line's display time until the next line or `captions.clear_after_ms`, rolling up to `captions.rows` rows (max 3); a cue crossing a segment boundary is written clipped into both. Caption times are mapped to PTS through the last video buffer sent to HLS, plus `mpegtsmux`'s fixed 3600 s offset (measured).
- **Languages:** every language with `webvtt = true` (the default). A language may now be WebVTT-only (no `cc608`, no `cea708_service`): the way to carry non-Latin scripts. Validation needs at least one carrier; a WebVTT-only language without an `hls://` output gets a warning.
- **Master playlist:** one video variant (`CLOSED-CAPTIONS=NONE`, so players don't also offer the embedded 608) plus `#EXT-X-MEDIA:TYPE=SUBTITLES,GROUP-ID="subs"` per language with its endonym as `NAME`, `DEFAULT=YES` on the source language.
- **Files:** `$MULTI_HLS_DIR`, else `$XDG_RUNTIME_DIR/multi/hls/<name>/`, else `/tmp/multi-hls/<name>/`; cleaned when the output starts and removed when it stops.
- **Serving:** `GET /hls/<name>/<file>` (playlists and segments of configured HLS outputs only) and `GET /watch/<name>`: a viewer page with hls.js and a caption language menu (`?lang=xx` preselects one; embedded 608/708 parsing is off).
- **Access:** `outputs[i].public = true` (default false, HLS outputs only) opens `/watch/<name>` and `/hls/<name>/…` of **that output** to anyone who can reach the server, without signing in. Nothing else becomes public: the GUI, the API and other outputs still need sign-in. With `public = false` the normal guard applies to viewers too.
- **hls.js:** 1.7.3, **Apache-2.0**, vendored into the binary (`crates/multi/web/hls.min.js`, licence in `hls.js-LICENSE.txt`); no CDN.

Measured (`crates/multi/tests/hls.rs`, fake workers, 2 s segments): a caption event reaches a published VTT segment in median 0.97 s, max 1.98 s; a player 3 segments behind the live edge is 6.0 s behind the newest segment, so ≈7–8 s glass to glass. A UDP output alongside never gapped more than 34 ms, including while the HLS output was stopped and started. Checked in headless Chromium: video plays with the English cues, the menu lists the endonyms (العربية renders).

Open: an HLS pipeline restart restarts segment numbering at 0 (players reload); `BANDWIDTH` is estimated from the first segment.

## Why

M1's YouTube test showed captions working but only one track (English): embedded 608/708 on YouTube isn't enough for multi-language viewers. Non-Latin languages (Arabic, Chinese, Japanese…) can't go on 608/708 at all, so WebVTT output unlocks them.

## Log

Newest first.

- 2026-09-29 — M2-3 in review: `hls://` output (hlssink2 + MULTI-written WebVTT and playlists), `/watch/<name>` viewer with hls.js, WebVTT-only languages, public viewing per output.
- 2026-09-29 — M2-1 in review: user catalogue, 120-entry opus-mt catalogue with scripts and formats, models API and GUI.
- 2026-09-28 — R1 done: YouTube allows one live caption track per broadcast; HLS + WebVTT needs our own VTT segments/playlists; Nemotron is usable for ES/FR/DE/PT, Whisper better for DE.
- 2026-09-28 — M2 started: R1 research spike.
