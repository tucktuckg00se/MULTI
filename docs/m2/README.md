# M2 — More languages

> **Summary:** Multi-language captions viewers can choose, many more languages, non-English speakers, and model management in the GUI. Starts with a research spike (R1) on YouTube captions, HLS + WebVTT, and non-English speech accuracy; then four work packages.

## Work packages

| WP | Contents | Status |
|---|---|---|
| R1 Research | Done. Findings: [YouTube captions](findings/R1-youtube-captions.md), [HLS + WebVTT](findings/R1-hls-webvtt.md), [non-English ASR](findings/R1-non-english-asr.md) | done |
| M2-1 Models + languages | Installed models vs catalogue (built-in registry + user-editable `~/.config/multi/models.toml`); GUI Models section (list, pull with progress and licence, verify, remove); GUI model/language choices limited to installed models; catalogue grown to every permissively licensed opus-mt English pair, each marked with the caption formats that can carry it. See [Models](#models-m2-1) | in review |
| M2-2 Non-English speakers | Any source language the ASR model transcribes (Nemotron: 15 codes, passed explicitly); translation direct or through English; segmenter punctuation for ES/FR/CJK; Whisper deferred (no backend). See [Non-English speakers](#non-english-speakers-m2-2) | in review |
| M2-3 Web output: HLS + WebVTT | `hls` output served by MULTI: video pass-through plus one WebVTT subtitle rendition per language (any script); viewer page with a language picker. Per R1: GStreamer has no HLS element for WebVTT; MULTI writes the WebVTT segments and playlists itself (with `X-TIMESTAMP-MAP`) next to an A/V-only HLS | in review |
| M2-5 Speech models | Dropped short words fixed (streams outlive short pauses); `Engine` trait with Nemotron (sherpa-onnx) and Whisper (whisper.cpp + LocalAgreement-2 + hallucination guard); catalogue of Nemotron 160/560/1120 ms fp32/int8 and Whisper turbo/small; GUI model dropdown. See [Speech models](#speech-models-m2-5) | in review |
| M2-4 YouTube per-language | R1: YouTube allows **one** live caption track per broadcast (embedded 608/708 and HTTP POST alike). So: a caption language per YouTube output (one broadcast per language, video stream-copied), and optionally uploading every language's captions to the recording after the stream (`captions.insert`, needs OAuth) | not started |

Order: R1 → M2-1 and M2-3 in parallel → M2-2 → M2-4.

## Models (M2-1)

**Two lists.** *Installed* = on disk in the models directory. *Catalogue* = the built-in registry ([`models.toml`](../../crates/multi-core/data/models.toml)) merged with a user file of the same schema: `--catalogue <path>` > `$MULTI_CATALOGUE` > `$XDG_CONFIG_HOME/multi/models.toml` > `~/.config/multi/models.toml`. A user entry with a known `id` replaces the built-in one, a new `id` is added; each entry gets the registry checks (licence, attribution, source, SHA-256, no `..`), and a bad one is reported and skipped (`multi models list`, GUI banner), never fatal. `multi models list` has a `from` column (builtin/user).

**Catalogue.** 120 opus-mt `convert` entries (revisions pinned, Apache-2.0 or CC-BY-4.0, CC-BY attribution in the entry): every en→X pair with an ISO 639-1 target (78 languages, plain and `tc-big` where both exist), and X→en for the 23 Nemotron languages that have one (plus `opus-mt-ROMANCE-en` as pt→en). Skipped: 3-letter/low-resource targets (mostly JW300/Bible data), group models (`en-gem`, `en-zle`…), `en-jap`. Source list: [evidence/M2-1](evidence/M2-1/opus-mt-catalogue.txt). A `language` table gives each code a name and script (81 languages with a model, 65 Latin; also Cyrillic, Arabic, CJK, Hangul, Devanagari, Greek, Hebrew, Armenian, Malayalam, Ethiopic). Formats follow the script: 608 and 708 Latin only (`latin_extended` adds the 608 missing-letters note; the list matches `Config::warnings`), WebVTT any script. Multi-target models carry `target_token` (`>>cmn_Hans<<`); `multi run` passes it as `multi-mt --prefix lang=token`. The worker picks an installed model first when several serve one pair.

**API** (`web_models.rs`, behind the guard; DELETE now also needs `X-Multi: 1`): `GET /api/models` (catalogue + status, size, licence, languages, formats, in-use, pull queue), `POST /api/models/{id}/pull` (background, one at a time, others queue; 202), `POST /api/models/{id}/verify`, `DELETE /api/models/{id}` (409 when the running pipeline uses it). Progress: SSE `/api/models/events` (`queued`, `progress` {phase download/extract/convert/verify, done, total bytes}, `done`, `failed`, `removed`).

**GUI.** Models group: installed (size, licence, in-use, verify/remove), catalogue with filter, licence and attribution before pulling, pull button and progress bar. Languages group: a dropdown of the installed ASR languages (spoken) or installed translation targets; a configured language without a model shows "Install model", linking to its catalogue row; each row shows the formats that can carry it. `#sec-a,b` in the URL opens those groups.

**Tests:** catalogue merge and bad entries, expanded registry (licence, script, pinned revision per entry), installed-first model choice (multi-core); API list/pull/verify/remove against the local HTTP server and in-use resolution (`crates/multi/tests/models.rs`).

## Non-English speakers (M2-2)

**Source language.** The language with `source = true` may be any language the ASR model lists in the catalogue (`languages`). Nemotron 3.5 Streaming: the model card's 19 transcription-ready locales, 15 codes (en es fr it pt nl de tr ru ar hi ja ko vi uk); the broad-coverage and adaptation-ready ones are no longer offered. `multi run` always passes it as `multi-asr --lang xx` (R1: explicit beats `auto` by 0.4–0.7 WER points). `Config::validate` rejects any other source at `languages[i].code` (checked against the built-in catalogue; a user-catalogue ASR model is checked when `multi run` resolves it). The GUI offers as spoken languages only those of installed ASR models. **Whisper** (R1: better for DE) is not added: `multi-asr` has no Whisper backend (needs a non-streaming decoder plus LocalAgreement); the catalogue has a note with its languages.

**Routing** (`Registry::route`): per target, a direct `src→tgt` model if installed; else through English (`src→en`, then `en→tgt`) if both are installed; if neither is complete, the direct pair when the catalogue has one, else the pivot pair, and each missing model gets the usual warning with its `multi models pull` command (marked "translating through English"). The source gets no MT. Catalogue additions (Apache-2.0, pinned): `opus-mt-{es-fr,es-de,fr-es,fr-de,de-es,de-fr}` and `opus-mt-ROMANCE-en` as pt→en (`mul-en` noted as a user-catalogue fallback). The GUI's target list includes pivot targets and links the missing half.

**Where the pivot runs: in `multi-mt`.** `multi run` passes `--source xx --pivot fr,de`; the worker adds an `en` lane (`src→en`) if English isn't a target, and that lane hands its English text to the pivot targets' `en→X` lanes. Chosen over looping through `run.rs` because the second hop stays inside the worker's existing lane/queue/deadline machinery (deadline still counted from arrival) and `run.rs`, the quality tracker and the IPC protocol don't change; one English translation serves every pivot target and a requested `en` output.

**Text.** The segmenter ends sentences on `. ! ? 。 ！ ？ …` before closing quotes (`fin.»`, `好。」`), cuts long clauses at `, ， 、`, attaches opening marks `¿ ¡ « „ 「` to the next word, and joins CJK words without spaces. The word filter was already per lane (own list + the source's), so a Spanish source lane uses the Spanish list and each translation its own. ASR word timing is unchanged.

**Measured** (RTX 3090, release + CUDA, `multi run` end to end; [evidence/M2-2](evidence/M2-2/)). The 20 MLS Spanish clips from R1 (326 s, 1 s gaps) through `source.sh` → UDP → `multi run`, ES source, EN + FR direct, DE through English:

- **ASR WER 9.9 %** (735 reference words, same normaliser; R1's batch Nemotron output on the same clips scores 6.5 % with it). The difference is mostly dropped short words (`que`, `y`, `el`) and a few merged words: a live-path loss, see Open.
- 135 clauses, each translated to all three; MT time median/max: EN 5/20 ms, FR (direct) 7/29 ms, DE (two hops) 11/45 ms.
- Sample: *nada. Quien tenga corazón* → EN *Nothing. Whoever has a heart* · FR *Rien. Qui a le cœur* · DE *Nichts. Wer ein Herz hat*.

**Target tokens (M2-1) were broken; fixed.** `multi-mt` prepended `>>por<<` / `>>cmn_Hans<<` to the text, so SentencePiece split it and the model never saw the token: EN→PT (`opus-mt-tc-big-en-pt`, in the default set) gave *"Por ⁇ Procurando por alguns"*, *">>por ⁇ Ser de dor"*; EN→ZH gave runs of `⁇`. The worker now adds the token as one vocabulary entry before the SentencePiece pieces. Checked with real translations (ES speech → EN → PT/ZH): PT *"Procurando por alguns"*, *"Nada. Quem tem coração"*; ZH in Simplified script *"释放, 焦虑, 让世界变得如此"*, no `⁇` in 76 clauses. The token values themselves were right. ZH quality on short clauses is weak (repetitions such as *来,来,来*).

**Tests:** source-language validation (config), routing direct/pivot/PT (multi-core), lane resolution with fake installed models and missing-model messages (multi), worker arguments (`--lang`, `--source`, `--pivot`), `split`/`lane_langs` (multi-mt), Spanish/French/Chinese punctuation (segmenter), and `tests/source_lang.rs`: ES source with the fake workers on ports 9780–9781 (source words on CC1 untranslated, `[en]` on CC3, no `[es]` request).

Open: ~~live ASR WER is ~3 points worse than batch~~ (fixed in M2-5: live now within 0.3 of batch); clauses on read speech are short (70 % close on the timer), which hurts translation; the `en` pivot lane loads even when English isn't shown (≈200 MB VRAM); ~~Whisper backend~~ (M2-5).

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

## Speech models (M2-5)

**Dropped short words: cause and fix.** Reproduced with `multi-asr` alone (`evidence/M2-5/feed.py` feeds clips over the worker protocol) against an offline decode of the same audio (whole clip, one stream). The missing words were whole short phrases right after a pause (*que así resulta*, *y*, *el*), not word tails or onsets. The worker opened a fresh recognizer stream per VAD segment and closed it 0.5 s into every pause (Silero `min_silence`), also where Silero ended speech mid-sentence; a fresh Nemotron stream decodes little of its first second, so a short phrase after a pause was lost. The tail flush (0.6 s padding + `input_finished`) and the 0.6 s pre-roll were already right. **Fix:** a stream stays open, fed the real audio, until `--hangover-ms` (default 1500) of silence after the VAD end; speech resuming inside it continues the same stream. A stream older than 60 s is closed at the next VAD end.

| WER (M2-2 normaliser) | Offline | Live before | Live after |
|---|---|---|---|
| ES, 20 MLS clips (735 words) | 6.5 | 10.2 | 6.8 |
| DE, 20 MLS clips (682 words) | 10.6 | 16.3 | 10.3 |
| EN, `long.wav` (1341 words) | 4.5 | 6.7 | 4.8 |

Hangover sweep, ES/EN WER: 0 ms 10.2/6.7, 500 ms 7.3/4.9, 1500 ms 6.8/4.8, 3000 ms 6.7/–. Lag is unchanged (words still commit when the next starts or after 400 ms of quiet).

**Engines.** `multi-asr` has an `Engine` trait (`warm_up`, `push`, `finish`) and `--engine`, taken from the catalogue entry:
- `sherpa-streaming`: Nemotron 3.5 Streaming through sherpa-onnx, as before plus the fix. `--model` is the export folder (`--model-dir` still accepted), `--chunk-ms` its export chunk.
- `whisper`: whisper.cpp through `whisper-rs` 0.16 (cargo feature `whisper`, on by default; `--features cuda` for GPU), ported from S4: while the VAD reports speech the buffer is re-transcribed every `--chunk-ms` (1000) and the prefix the last `--passes` agree on is committed (LocalAgreement-2, from `asr.stability_passes`). Language always explicit.
- Both sit behind the same Silero VAD gate: nothing is decoded outside speech. A failed decode is reported as a worker error; five in a row end the worker (the supervisor restarts it).

**Hallucination guard (Whisper, `guard.rs`).** Per decoder segment on every pass: dropped if no-speech probability > 0.6 or average token log-probability < −1.0; dropped if every sentence is a known filler (*Thank you.*, *Thanks for watching*, *Subtitles by…*, Amara/ZDF credits, ES/DE/FR/PT/IT/RU forms, lone interjections like *Oh.*). An n-gram repeated back to back (one word ×4, 2–8 words ×3) collapses to one copy, within a pass and across the words committed in the last 30 s.

**Catalogue.** ASR entries carry `engine`, `chunk_ms`, `lag_ms`, a one-line `note`, `languages`, licence and `vram_mb`. Added Nemotron 160 and 1120 ms: fp32 from Hugging Face `csukuangfj2` (revisions pinned, SHA-256 from the Hub's LFS metadata) and int8 from the sherpa-onnx `asr-models` release (SHA-256 from GitHub's asset digests); the copies in `~/.cache/multi-models/sherpa` match. Whisper `large-v3-turbo` and `small`: ggml files from `ggerganov/whisper.cpp` (MIT) @ `5359861c`, SHA-256 match the local copies; languages are the 98 of Whisper's 100 with an ISO 639-1 code (45 codes added to the language table with their scripts). An entry without `engine` takes it from its backend, so older user catalogues still load. `asr.chunk_ms` follows the chosen Nemotron model; for Whisper it is the pass interval.

**GUI and validation.** Speech recognition's model is a dropdown of installed ASR models (id, languages, lag, VRAM; the note and engine below it; "Install more" opens Models). Picking a Nemotron model sets `asr.chunk_ms`; the spoken-language choices follow the model. `Config::validate` checks the spoken language against the chosen catalogue model; `multi run`/`serve` report an ASR model that isn't installed (its CPU variant is used if only that is). `multi run` passes `--engine`, `--model` (folder or `.bin`), `--chunk-ms`, and `--passes` for Whisper.

**CI.** The `workers` job now also builds whisper.cpp for CPU (`GGML_NATIVE=OFF`, so cached objects don't depend on the runner's CPU); the target dir is cached, so it rebuilds only when `Cargo.lock` changes. Locally the CPU whisper.cpp build adds about 1 min, the CUDA build about 4 min at 6 jobs.

**Measured** (RTX 3090, release + CUDA, `multi-asr` alone fed at 1× real time; [evidence/M2-5](evidence/M2-5/results.md)):

| Engine | WER EN / ES / DE | Lag after the utterance, EN p50/p95 | Word lag p50/p95 (all) | Silence/music clip | VRAM |
|---|---|---|---|---|---|
| Nemotron 560 ms fp32 | 4.8 / 6.8 / 10.3 | 0.93 / 1.15 s | 0.6 / 1.0–1.1 s | 0 words | 3.6 GB |
| Whisper turbo, LA-2, 1 s | 4.9 / 5.4 / 7.3 | 1.08 / 2.87 s | 1.3–1.6 / 2.4–2.7 s | 0 words (S4: 27) | 2.1 GB |

Whisper stays the accuracy option for ES/DE (R1 offline: 5.2/6.2), at about twice the lag; for EN the two are level. The guard dropped 37 segments on the music clip (26 low confidence, 11 filler) and 3–6 per language on speech, with WER unchanged.

**Tests:** engine selection and arguments (`multi-asr` argument plan; `run.rs` worker arguments for both engines), LocalAgreement with a scripted recogniser, the guard (fillers, confidence, loops within a pass and across commits), catalogue ASR entries (engine, languages, licence, pinned revision; bad entries rejected), ASR resolution with sparse fake installs (chunk follows the model, CPU variant, Whisper `.bin`, not installed, wrong language), spoken language validated against the chosen model.

Open: Whisper re-reads up to 12–25 s of audio per pass on long sentences; the guard's thresholds and word lists are heuristics tuned on S4's music clip and these runs; Whisper small and CPU Whisper not measured.

## Why

M1's YouTube test showed captions working but only one track (English): embedded 608/708 on YouTube isn't enough for multi-language viewers. Non-Latin languages (Arabic, Chinese, Japanese…) can't go on 608/708 at all, so WebVTT output unlocks them.

## Log

Newest first.

- 2026-09-29 — M2-5 in review: dropped short words fixed (ES live WER 10.2 → 6.8), Whisper engine with LocalAgreement and hallucination guard, ASR catalogue and GUI dropdown.
- 2026-09-29 — M2-2 in review: source language any Nemotron language (validated, passed explicitly), translation direct or through English in `multi-mt`, CJK/Spanish punctuation; ES live WER 9.9.
- 2026-09-29 — M2-3 in review: `hls://` output (hlssink2 + MULTI-written WebVTT and playlists), `/watch/<name>` viewer with hls.js, WebVTT-only languages, public viewing per output.
- 2026-09-29 — M2-1 in review: user catalogue, 120-entry opus-mt catalogue with scripts and formats, models API and GUI.
- 2026-09-28 — R1 done: YouTube allows one live caption track per broadcast; HLS + WebVTT needs our own VTT segments/playlists; Nemotron is usable for ES/FR/DE/PT, Whisper better for DE.
- 2026-09-28 — M2 started: R1 research spike.
