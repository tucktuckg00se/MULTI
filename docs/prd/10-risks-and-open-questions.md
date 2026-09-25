# Risks and open questions

> **Summary:** Twelve risks with mitigations (streaming ASR, small-model translation, 608 character limits, SEI stripping, filter misses, licence contamination, GPU driver sprawl, and five found in M0: upstream GStreamer caption bugs, Whisper hallucination, opus-mt sentence dropping, 708 visibility in players, AAC decoder licence) and the open-question checklist.

| Risk | Impact | Mitigation |
| --- | --- | --- |
| Whisper-class models are not truly streaming; partial results flicker or lag | Caption lag over target, rewrites on screen | Use roll-up mode, commit only stable words; evaluate streaming-native ASR in M0 |
| Translation quality from small models on short, unfinished sentences | Awkward or wrong translations | Translate at clause boundaries; accept ~0.5 s extra lag for quality |
| 608 is limited to Latin characters and ~60 chars/s | CJK, Arabic, Cyrillic cannot use 608 | Use 708 services, DVB or WebVTT for those languages; document it clearly |
| Downstream platforms strip or ignore SEI captions | Captions vanish after YouTube/CDN | Test top 5 destinations in M1; offer sidecar/WebVTT fallback |
| Profanity filter misses (new slang, mis-transcriptions, other languages) | Offensive text on air | Filter every language; ship curated lists; warranty excludes misses |
| Licence contamination (GPL-only FFmpeg parts, non-commercial models) | Can't sell binaries | Licence scan in CI; model registry records each model's licence |
| GPU driver/CUDA version sprawl across Linux, Windows, Jetson | Support load | Narrow supported matrix; containers for Linux |
| Upstream GStreamer caption bugs (`roll-up-rows` lost at start, GAP events add frames; gst-plugins-rs 0.15.3) | Wrong 708 roll-up depth; captions fall behind | Workarounds in place (ADR-0003); report upstream; our `cc` crate as fallback |
| Whisper invents text over music/silence | Fake captions on air | Default ASR is Nemotron (0 words on non-speech); Whisper only with a hallucination filter |
| opus-mt drops later sentences in multi-sentence input | Missing translated text | Split clauses into sentences before translating |
| 708-only languages invisible in some players (VLC 3 offers CC1–CC4 only) | Viewers can't find FR/DE captions | Document; optional 608 CC2/CC4 placement; WebVTT for web |
| Audio decoder licence (fdkaacdec/faad fallbacks are non-LGPL) | Can't ship binaries | Ship and require `avdec_aac` (LGPL) only |

**Open questions**

- [x] Final licence: GPL-3.0 (decided)
- [x] Implementation language: Rust (decided)
- [x] Pipeline base: GStreamer (decided, ADR-0004)
- [ ] Is "MULTI" clear to trademark in software/broadcast classes?
- [ ] Pricing for official binaries, and is the GPU Docker image free or paid?
- [ ] Which languages are in the first supported set?
- [ ] Legal entity and lawyer review for warranty and liability terms.
