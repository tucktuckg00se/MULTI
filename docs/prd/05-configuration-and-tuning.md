# Configuration and tuning

> **Summary:** Every timing and quality trade-off is a setting with a default; presets low-latency/balanced/accuracy. Table of ~21 settings incl. video.delay_ms, asr.chunk_ms, web.port (8480), degrade.*.

Every timing and quality trade-off is a setting with a sensible default, so a broadcast engineer can tune MULTI for their own system instead of accepting our choices.

- Each setting lives in the config file and can be overridden by a CLI flag, the REST API or the web UI.
- Settings that are safe to change live (filters, caption offset, rows) reload without dropping the stream. The rest restart only the affected worker.
- Presets (`low-latency`, `balanced`, `accuracy`) set many values at once; `balanced` is the default. Anything set explicitly overrides the preset.
- `multi bench` suggests a preset and model for the detected hardware on first run.
- The UI shows measured caption lag next to the video delay, so the engineer can set one from the other.

| Setting | Default | Range / options | What it trades |
| --- | --- | --- | --- |
| `video.delay_ms` | 0 (pass-through) | 0–10,000 | Captions in sync vs a delayed stream |
| `captions.offset_ms` | 0 | −5,000 to +5,000 | Fine-tune caption timing against video |
| `captions.mode` | roll-up | roll-up, pop-on, paint-on | Fast and live vs cleaner reading |
| `captions.rows` | 3 | 1–4 | Screen coverage vs context |
| `captions.max_chars_per_line` | 32 | 20–42 (608 max 32) | Readability vs line breaks |
| `captions.clear_after_ms` | 4,000 | 1,000–30,000 | How long text lingers after speech stops |
| `asr.model` | chosen by `multi bench` | any registry model | Accuracy vs speed and VRAM |
| `asr.chunk_ms` | 500 | 200–3,000 | Lower lag vs accuracy |
| `asr.stability_passes` | 2 | 1–3 | Lower lag vs fewer on-screen corrections |
| `vad.threshold` | 0.5 | 0.1–0.9 | Catching quiet speech vs ignoring noise and music |
| `translate.segment` | clause | word, clause, sentence | Translation lag vs quality |
| `translate.max_wait_ms` | 800 | 200–3,000 | Upper limit on waiting for a clause to finish |
| `filter.profanity` | on | on, off, per language | — |
| `filter.mask_style` | first-letter | asterisks, first-letter, [bleep], drop | — |
| `audio.track` / `audio.channels` | first track, downmix | any track; any channel or pair | Which mic feed is transcribed |
| `srt.latency_ms` | 120 | 20–8,000 | Network resilience vs delay |
| `gpu.device` / `gpu.max_vram_mb` | auto / no limit | device index; MB | Sharing a GPU with an encoder |
| `web.bind` | 127.0.0.1 | any local address; 0.0.0.0 for all | Local-only safety vs remote access (token required when not localhost) |
| `web.port` | 8480 | 1–65535 | Avoiding clashes with other services; also serves the REST API and metrics |
| `degrade.max_lag_ms` | 5,000 | 1,000–30,000 | When to start shedding load to protect the stream |
| `degrade.order` | languages, model, source-only, pass-through | any order; per-language priority | Which captions to give up first under load |

