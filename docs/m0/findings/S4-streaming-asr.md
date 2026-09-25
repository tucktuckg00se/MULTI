# S4: Streaming speech recognition

> **Summary:** Default to **Nemotron 3.5 ASR Streaming 0.6B** (NVIDIA, OpenMDW-1.1, 32 usable locales) through sherpa-onnx, CUDA 13, 560 ms chunk. On `long.wav` at 1×: lag after the last word p50 0.84 s, p95 1.21 s. Committed text never changed, and silence or music produced no words. RTF 0.03, 3.6 GB VRAM, and real time on CPU (RTF 0.22). Whisper large-v3-turbo with LocalAgreement-2 is more accurate (WER 3.3 vs 5.4) but lags more (p95 2.85 s) and makes up "Thank you." over music even behind the VAD. Keep it as the accuracy option.

Code: `spikes/s4-asr` (`s4-asr live|batch`, scripts in `scripts/`). Evidence: [evidence/S4](../evidence/S4), with model sources and revisions in `models.txt`.

## Candidates

| Model | Runtime | Licence | Languages |
|---|---|---|---|
| Whisper large-v3-turbo, small | whisper-rs 0.16 (whisper.cpp 1.8.3), built against CUDA 13.4 | MIT | 99 |
| distil-large-v3.5 | whisper-rs | MIT | English only |
| Parakeet TDT 0.6B v3 (offline) | sherpa-onnx 1.13.8, ORT 1.28.2 CUDA-13 build | CC-BY-4.0 | 25 European |
| **Nemotron 3.5 ASR Streaming 0.6B** (cache-aware RNNT, 2026-06) | sherpa-onnx | OpenMDW-1.1 | 19 ready + 13 broad; language set per stream or `auto`; punctuation and capitals |

- Silero VAD v5 (MIT) runs in front of every model.
- CUDA 13 caused no problems: sherpa-onnx publishes a `cuda-13.x` prebuilt, and whisper.cpp builds with `CMAKE_CUDA_ARCHITECTURES=86`.
- The int8 ONNX exports run their int8 ops on the CPU even with the CUDA EP (~900% CPU). Use fp32 on the GPU and int8 on the CPU.

## Results

Test file: `long.wav`, 485.6 s, fed at 1× wall clock with the VAD on. Lag was measured by `latency captions` on an SRT of emission times, two ways:
- from the segment end;
- from the forced-aligned last word (`--words long.words.tsv`, wav2vec2 CTC alignment).

Other columns:
- WER is normalised with Whisper's English normaliser.
- The music column is Rachmaninoff mixed at 5 dB SNR.
- "Pass p95 / max" is model time per pass.

| Model, settings | Lag seg p50/p95 | Lag word p50/p95/max | Matched | WER clean | WER music 5 dB | RTF | VRAM | Pass p95 / max | Tail revised |
|---|---|---|---|---|---|---|---|---|---|
| **Nemotron 560 ms fp32** | 0.33 / 0.81 | **0.84 / 1.21 / 1.33** | 95% | 5.4 | 8.5 | 0.027 | 3.6 GB | 17 / 23 ms | **0** |
| Nemotron 160 ms | 0.11 / 0.57 | 0.70 / 1.01 / 1.12 | 97% | 5.3 | – | 0.084 | 3.6 GB | 15 / 21 ms | 0 |
| Nemotron 1120 ms | 0.69 / 1.78 | 1.11 / 2.17 / 2.55 | 97% | 5.4 | – | 0.016 | 3.6 GB | 20 / 26 ms | 0 |
| **Whisper turbo, 1000 ms, LA-2** | 0.59 / 2.34 | 1.13 / 2.85 / 2.88 | 98% | **3.3** | **4.4** | 0.11 | 2.1 GB | 147 / 975 ms | 8.9% |
| Whisper turbo, 500 ms, LA-2 | 0.13 / 0.57 | 0.61 / 1.12 / 3.67 | 98% | 3.6 | – | 0.21 | 2.1 GB | 149 / 1016 ms | 9.6% |
| Whisper turbo, 1000 ms, LA-1 | −0.35 / 0.17 | 0.14 / 0.63 / 0.69 | 85% | 12.2 | – | 0.11 | 2.1 GB | – | – |
| Parakeet v3 fp32, 500 ms, LA-2 | −0.01 / 0.41 | 0.55 / 0.89 / 1.52 | 98% | 4.6 | – | 0.12 | 4.6 GB | 74 / 83 ms | 14.7% |
| Whisper small / distil-v3.5, 1000 ms | – / – | 1.15 / 1.93; 1.19 / 2.94 | 97% | 5.8; 4.4 | – | 0.08; 0.10 | 1.0; 2.0 GB | – | – |
| Nemotron 560 int8, **CPU only**, 8 threads | – | – | – | 5.9 | – | **0.22** | 0 | 187 ms | 0 |

More settings are in `live-runs.csv`. RSS stayed flat after warm-up in every 8-minute run (Whisper 788 MB, Nemotron 1361 MB). Model load plus warm-up took under 2 s.

## Streaming and commit strategy

- **Nemotron (native streaming):** output is append-only.
  - A word is committed once the next word starts, or after 400 ms of decoded audio with no new token.
  - Each VAD speech segment gets its own stream. A 0.6 s pad and `input_finished` flush it.
  - Don't use the recogniser's endpoint/`reset`: it drops audio that was fed but not yet decoded, and lost words at every pause (WER 21%).
- **Whisper and Parakeet (LocalAgreement-n):**
  - Re-transcribe the buffer every `chunk_ms`.
  - Commit the prefix shared by the last n hypotheses.
  - Skip already-committed words by text alignment; whisper.cpp timestamps are too coarse to skip them by time.
  - Trim the buffer at a committed sentence end once it passes 12 s. Commit everything at VAD end-of-speech.
  - LA-1 is fast but wrong (12% WER). LA-3 adds ~1 s of lag with no accuracy gain.
- **Stalls:** Whisper passes stall at up to 1 s when the buffer is long. Nemotron's cost per step is constant.

**PRD §5 defaults:** `asr.model` = nemotron-3.5-streaming-560ms, `asr.chunk_ms` = 560 (a model export per chunk: 160/560/1120), `asr.stability_passes` = 2. `stability_passes` applies only to LocalAgreement backends; Nemotron needs none.

## Hallucination and robustness

Test input: 60 s silence, 60 s pink noise, then 280 s of orchestral and piano music, with no speech.

| Model | Words emitted |
|---|---|
| Nemotron + VAD | **0** |
| Whisper turbo + VAD | 27 ("Amen. Thank you. ×13", all during the music) |
| Whisper turbo, no VAD | 104 |

- Silero opens on music, so the VAD does not protect Whisper.
- Under music, Nemotron loses more words than Whisper: 46 deletions, WER 8.5 vs 4.4.
- Without the VAD, Nemotron splits words and loses accuracy. Keep the VAD in front of it.

## CPU fallback (OP-1)

- Nemotron int8 on 8 CPU threads runs at RTF 0.22 with WER 5.9 (vs 5.4 on GPU). That meets OP-1.
- Whisper turbo is not viable on CPU. Whisper small on CPU was not measured.

## Process isolation and interface

- Run ASR as a supervised child process (`multi-asr`), one per stream and model.
- The parent sends 20 ms PCM frames (16 kHz mono f32) over a Unix socket or pipe, with a sequence number, and never blocks on it (bounded queue, drop oldest, count the drops).
- The child replies with newline-delimited JSON events: `{"type":"words","seq":n,"lang":"en","words":[{"text":"Brick.","t0":12.34,"t1":12.61}],"emit_mono_ns":…}`, plus `{"type":"heartbeat"}` every second.
- Times are audio-stream seconds, so captions line up with PTS. Words are final, since committed text is never revised. A separate optional `partial` event can feed a UI preview.
- On a crash, missed heartbeats (2 s) or `backlog > 3 s`, the supervisor restarts the child (load plus warm-up < 2 s). The video path never waits on it.

## Open items (scope cut)

- Not run: the LibriSpeech 200+ utterance WER, the other noise levels (media prepared: pink/music at 10/5/0 dB), the 30-minute soak (8-minute runs showed flat RSS), and CPU lag at 1× (only CPU RTF was measured).
- Not scored: the multilingual check (es/fr/de MLS clips are downloaded) and the real-world Supreme Court clip. Nemotron's non-English accuracy is still unverified by us.
- Hallucination filter for Whisper if it is offered: a music classifier, or dropping low-probability "Thank you." segments.
- Test Nemotron's `auto` language ID and mid-stream language switching.
