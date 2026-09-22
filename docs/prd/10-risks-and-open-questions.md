# Risks and open questions

> **Summary:** Seven risks with mitigations (streaming ASR, small-model translation, 608 character limits, SEI stripping, filter misses, licence contamination, GPU driver sprawl) and the open-question checklist.

| Risk | Impact | Mitigation |
| --- | --- | --- |
| Whisper-class models are not truly streaming; partial results flicker or lag | Caption lag over target, rewrites on screen | Use roll-up mode, commit only stable words; evaluate streaming-native ASR in M0 |
| Translation quality from small models on short, unfinished sentences | Awkward or wrong translations | Translate at clause boundaries; accept ~0.5 s extra lag for quality |
| 608 is limited to Latin characters and ~60 chars/s | CJK, Arabic, Cyrillic cannot use 608 | Use 708 services, DVB or WebVTT for those languages; document it clearly |
| Downstream platforms strip or ignore SEI captions | Captions vanish after YouTube/CDN | Test top 5 destinations in M1; offer sidecar/WebVTT fallback |
| Profanity filter misses (new slang, mis-transcriptions, other languages) | Offensive text on air | Filter every language; ship curated lists; warranty excludes misses |
| Licence contamination (GPL-only FFmpeg parts, non-commercial models) | Can't sell binaries | Licence scan in CI; model registry records each model's licence |
| GPU driver/CUDA version sprawl across Linux, Windows, Jetson | Support load | Narrow supported matrix; containers for Linux |

**Open questions**

- [x] Final licence: GPL-3.0 (decided)
- [x] Implementation language: Rust (decided)
- [ ] Pipeline base: FFmpeg libraries or GStreamer (decide after M0 spike)?
- [ ] Is "MULTI" clear to trademark in software/broadcast classes?
- [ ] Pricing for official binaries, and is the GPU Docker image free or paid?
- [ ] Which languages are in the first supported set?
- [ ] Legal entity and lawyer review for warranty and liability terms.
