Latency tool self-test (docs/m0/findings/H-latency-tool.md), 2026-09-22, Arch Linux x86-64, FFmpeg 9.0.1.
selftest.log                 full output of spikes/harness/latency-selftest.sh (40 s per stage, first 10 s skipped)
relay.pairs-every10th.csv    per-frame delay through the plain ffmpeg -c copy relay (every 10th matched frame)
captions-plus2*.csv          per-utterance caption lag for the synthetic +2.0 s SRT (clean and ASR-noised)
tap-garbage-input.log        tap stats after 3000 random/truncated/corrupted datagrams (no crash)
tap-csv-sample.txt           first rows of a tap CSV (source PTS origin 127920 = 1.421333 s)
