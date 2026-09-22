# Goals and non-goals

> **Summary:** Reliability first, speed second. v1.0 goals: <3 s caption lag, 4+ languages, fully local, everything tunable, profanity/blocklist filter, Linux x64+ARM64. Non-goals: audio bleeping, dubbing, VOD, certification.

**Guiding principle: stability and reliability first, speed second.** When the two conflict, MULTI keeps the stream up and accepts later captions. Low latency still matters and has hard targets, but never at the cost of dropping video or crashing.

**Goals (v1.0)**

1. Caption a live stream in real time with speech-to-caption lag under 3 s (P95) on a mid-range GPU.
2. Produce at least 4 simultaneous caption languages from one source language.
3. Let the broadcast engineer tune every timing and quality trade-off (video delay, caption timing, model size, filtering) through configuration, with defaults that work out of the box.
4. Run fully local: no per-minute fees, no cloud account, no audio leaves the machine.
5. Filter profanity and user-listed words before captions are emitted, in every output language. Support swapping in different AI models, and output the common TV and web caption formats.
6. Ship for Linux x86-64 and Linux ARM64, with Windows x86-64 as a stretch goal.
7. Source code free to use, modify and build; official prebuilt binaries sold with support.

**Non-goals (v1.0)**

- Muting or bleeping the audio itself (captions only; audio filtering is a later feature).
- Dubbing or text-to-speech in other languages.
- Human-in-the-loop caption correction UI.
- Recording, VOD processing or file-based captioning (live only, though file input is useful for testing).
- Replacing a full broadcast playout or encoder: MULTI sits inline and does one job.
- FCC/Ofcom caption-quality certification (we aim to support compliance, not certify it).

