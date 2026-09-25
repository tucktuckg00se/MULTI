# S6: end to end, live speech to captions in four languages

> **Summary:** It works in one process. Speech goes through Nemotron ASR, opus-mt and `tttocea708` into CC1 (EN), CC3 (ES) and 708 services 1–4 (EN/ES/FR/DE), and video still passes undecoded with a one-frame delay (33.7 ms p50). In a 10-minute harness run, EN caption lag was P95 1.13 s. Translations came ≤0.83 s after their EN clause (P95 ≈ 2.0 s in total). Memory stayed flat, and there were no errors or drops. The clause timer, not the MT model, sets the translated lag.

Code: `spikes/s6-e2e` (`run.sh` = harness run). Evidence: [evidence/S6](../evidence/S6).

## Run

```sh
cd spikes
export SHERPA_ONNX_LIB_DIR=~/.cache/multi-tools/sherpa/sherpa-onnx-v1.13.8-cuda-13.x-cudnn-9.x-onnxruntime1.28.2-linux-x64-gpu/lib
cargo build --release -p s6-e2e
target/release/s6-e2e --input 'srt://127.0.0.1:9600?mode=listener' \
  --output 'srt://127.0.0.1:9601?mode=listener' --output udp://127.0.0.1:9602 \
  --langs en,es,fr,de --words-log words.tsv --emit-log emit.tsv   # [--codec hevc]
MULTI_MEDIA=… s6-e2e/run.sh OUT 600    # harness: taps, verify, ccextractor, lag
```

OBS: Custom service, server `srt://127.0.0.1:9600?mode=caller`, keyframe interval 1 s, B-frames 0, AAC 48 kHz.

- **Threads:** the input's AAC goes through `tee → leaky queue → decoder → 16 kHz mono → appsink`, then `try_send` to the ASR thread (S4: 560 ms fp32 CUDA, Silero VAD, one stream per segment). A segmenter sends each ASR batch to EN at once. It closes a clause on `. , ; : ! ?`, after 800 ms without words, or at 24 words. Three MT threads (S5: opus-mt fp16, sentence split, length cap) skip any clause older than 2 s.
- **Captioner:** S2's per-frame `GstCc` driver with 4 lanes. Lane i gets service i+1; lanes 1–2 also carry CC1/CC3. A lane's text goes to its encoder only while the encoder is ≤6 frames ahead. Beyond 6 s of backlog (queue plus lead), the oldest text is dropped and logged. S2b's two workarounds are kept.
- **Reuse:** `lib.rs` in s2-gst-pipe (enums moved there), s4-asr and s5-translate. A few items made `pub`.

## Results (H.264 harness, `long.wav` looped, RTX 3090)

| Metric | Result | Target |
|---|---|---|
| Video delay, in → UDP out (10 min, `--skip 10`) | p50 33.66, p95 34.01, p99 34.22, max 35.2 ms | ≈33 ms |
| EN caption lag, CC1, 65/68 utterances | p50 0.95, **p95 1.13**, max 1.71 s | ≲1.5 s |
| EN clause → its translation (226/227 clauses, per lane) | p50 0.80, p95 0.83 s | — |
| Translated lag (EN p95 + delta p95) | **≈1.97 s** | ≲2.5 s |
| MT per clause | mean 11 ms, max 38 ms. 0 late or failed | 2 s |
| Word timestamps vs reference start | p50 +0.16 s, p95 +0.38 s (n 1540) | — |
| RSS / VRAM, 20 s → 610 s | 2008 → 2011 MB / 4240 → 4240 MB | flat |
| Errors (10 min) | 0 errors or drops. 1 benign warning (udpsrc buffer-size) | 0 |
| Decoded text | CC1 EN (verify.sh), CC3 ES, svc 1–4 EN/ES/FR/DE (ccextractor) | all |
| HEVC (60 s) | all lanes OK. Delay p50 33.6 ms (tail untrusted: hash matching) | — |

## Live OBS test (2026-09-25)

The user streamed from OBS with their microphone and watched the output in VLC 3.0.23. English (CC1) and Spanish (CC3) captions appeared live. French and German did not: VLC 3 only offers 608 channels CC1–CC4, and FR/DE are 708-only (services 3–4). MULTI's counters show FR/DE translated and pushed for every clause (102 each, 0 late, 0 errors), and ccextractor decoded services 3–4 in the harness run, so this is a player limitation. Evidence: `docs/m0/evidence/S6/live-obs.txt`.

## Gotchas

1. **The verify.sh SRT under-reads streamed roll-up.** FFmpeg's default `ccaption_dec` gives a row's cue the time of its first word, which put EN lag at p50 0.20 s with negative values. `-real_time 1` emits a cue per change, and `rtfix.py` repairs its bogus end times. `run.sh` does both.
2. **tsdemux (`ignore-pcr`) shifts all PTS by a per-run offset** (2.1–2.6 s). Sync is unaffected. `--words-log` times carry the shift, which `tsacc.py` removes.
3. **`tttocea708` roll-up adds a space between text buffers.** Pushing " word" gave double spaces.
4. **Nemotron punctuates read speech sparsely** (70 of 1626 words). About 70% of clauses close on the 800 ms timer, which *is* the translated lag. Mid-sentence cuts translate badly.
5. **gst-libav is not installed here.** The tap uses `fdkaacdec` (FDK licence), with `faad` (GPL) as a further fallback. Ship only `avdec_aac` (LGPL).
6. **CUDA teardown abort at exit** ("driver shutting down" from a destructor) while MT/ASR threads still held models: 10-minute run, after the measurement. Fixed with an ordered shutdown (close the tap, then join ASR → segmenter → MT). Clean exits checked since.
7. **`verify.sh udp://… --srt-out` left an empty capture**, so `run.sh` records the file first. ccextractor writes 708 SRT in Latin-1.

## Open

- Process isolation (S4/S5 child-process design) is not done. A crash in ASR, MT or ONNX/CUDA takes down the video.
- The per-lane backlog cap was not exercised (0 drops). There is no lane restart after an encoder flow error (S2b).
- Segmenter: use VAD pauses and a minimum length instead of a flat 800 ms. Check translation quality on real speech.
- Not yet tested: OBS live, 708 services 3–4 on real players, non-English speech.
