# WP2 real-model check

> **Summary:** 180 s of `source.sh` (long.wav) through `multi run` with the real `multi-asr` + `multi-mt` (RTX 3090, CUDA). Video delay in -> UDP out p50 33.6 ms / p99 34.2 ms (one frame). EN caption lag p50 0.96 s / p95 1.08 s (10 utterances). ES on CC3 (103 cues). No drops, no restarts, clean Ctrl-C exit.

Files: `run.sh` (the harness run; ports 9730–9734), `summary.txt` (numbers), `multi.log`, `multi.toml`, `lag-en.csv` (per-utterance EN lag), `cc1.srt` (verify.sh), `cc3.srt` (FFmpeg `-data_field second`).

Run: `MULTI_MEDIA=… BIN_DIR=<dir with multi, multi-asr, multi-mt> LATENCY=spikes/target/release/latency flock /tmp/multi-gpu.lock docs/m1/evidence/WP2/run.sh OUT 180`.
