# H-01: How the `latency` tool measures video delay and caption lag

> **Summary:** `latency tap` is a UDP MPEG-TS pass-through probe that logs each video frame's arrival time on CLOCK_MONOTONIC. `latency report` matches frames between two taps (content hash, then PTS) and reports delay, jitter and PTS offset. `latency captions` measures when each utterance's last words first appear in the captions. Self-test: tap overhead 0.02 ms, a known 250 ms delay reads 250.035 ms, a +2.0 s caption offset reads 2.000 s.

Code: `spikes/harness` (`cd spikes && cargo build --release -p harness` → `target/release/latency`). Evidence: [evidence/H](../evidence/H).

## Usage

```sh
latency tap --listen 127.0.0.1:9401 --forward 127.0.0.1:9402 --out in.csv   # before the SUT
latency tap --listen 127.0.0.1:9403 --forward 127.0.0.1:9404 --out out.csv  # after it
latency report in.csv out.csv --skip 10 [--json] [--pairs pairs.csv] [--pts-offset TICKS] [--match auto|hash|pts]
verify.sh udp://127.0.0.1:9404 --duration 120 --srt-out cap.srt            # SRT in absolute PTS
latency captions --srt cap.srt --segments $MULTI_MEDIA/long.segments.tsv --wav $MULTI_MEDIA/long.wav \
  --pts-origin 1.421333 --csv lag.csv [--words words.tsv] [--json]
latency delay --listen A --forward B --ms 250                                # calibration delay line
spikes/harness/latency-selftest.sh /tmp/out [--only relay,captions]           # reproduces the table below
```

A tap stops after `--duration S`, or when you kill it (the CSV is flushed after every frame). On stderr it prints stream error counts (bad sync, CRC, continuity counter, PAT/PMT changes, PTS wraps) and its own forwarding overhead.

## Method

- **Tap.** The receive thread only calls `recv`, reads the clock, calls `send`, then hands a copy to a parser thread over a queue that never blocks. If that queue fills, the tap still forwards the datagram and counts the drop. SO_RCVBUF is 8 MB. The parser follows PAT → PMT → the first video PID. It checks CRCs, follows table changes and rejoins packets split across datagrams. For each video PES it logs `wallclock_ns,pts_90k,frame_seq,tail_hash`:
  - `wallclock_ns` is when the datagram holding the PES start arrived;
  - `pts_90k` is unwrapped past 2^33;
  - `tail_hash` is an FNV-1a hash of the last 128 payload bytes.

  Bad input is counted and skipped; 3000 random, cut-off or corrupted datagrams caused no crash.
- **Report.** Frames are matched by `tail_hash` first. The slice data at the end of a frame survives SEI insertion and PTS rewriting. On duplicate hashes, the latest input frame that arrived before the output frame wins. The PTS offset is the most common `(out − in) mod 2^33` among the matched pairs. If fewer than half the output frames match by hash (for example, the SUT re-encodes), matching falls back to PTS minus the offset. Delay = out wallclock − in wallclock. The report gives:
  - nearest-rank percentiles p50/p95/p99/max;
  - jitter as the standard deviation and as the mean |successive difference|;
  - drift in ms/min;
  - unmatched frame counts within the window both taps saw.
- **Captions.** Stream time t = SRT time − `pts_origin`, and audio time = t mod loop. The loop length comes from `--loop-seconds`, else the WAV header, else the last segment end (485.645 s). Each utterance occurrence (`end + k·loop`) gets a target: the last 3 content words of the transcript up to that point. Stop-words are dropped, text is lowercased, punctuation is stripped, and short utterances borrow words from earlier ones. The appear time is the start of the first cue in [end − 1 s, end + 8 s] that meets all three rules:
  - it contains the target's **last** word;
  - it contains at least n−1 of the n target words, in order, within n+1 words;
  - each word is within edit distance 0 (≤2 chars), 1 (3–7) or 2 (8+).

  lag = appear − end. Only utterances fully inside the SRT span count. `--words` (`word⇥start_s⇥end_s`, audio time) measures from the end of the last word instead of the segment end, which includes trailing silence.

## Accuracy and limits

- **Clock.** CLOCK_MONOTONIC is shared by processes on one host only, so both taps must run on the same machine.
- **Overhead.** The tap's own `recv`→`send` takes 1.5 µs p50 and about 15 µs p99. A downstream tap sees 0.020 ms p50 and 0.156 ms max. Timestamps are taken in user space (not SO_TIMESTAMPNS), so the error is scheduler wake-up: tens of µs, with rare ms spikes.
- **What delay covers.** Delay runs from a frame's first packet in to its first packet out. Any SUT that demuxes adds at least 1 frame (33 ms), because it can only emit a frame once the next PES starts.
- **SRT legs.** The tap speaks UDP only. For an SRT SUT, bridge UDP↔SRT with `ffmpeg -c copy`. Measure the bridge pair alone and subtract it. SRT adds its configured `latency` (120 ms by default).
- **Hash collisions.** A static picture with identical frame tails can collide. If that happens, use `--match pts`.
- **Caption time base.** Checked: `verify.sh --srt-out` records with `-copyts` and keeps the source PTS exactly, and `movie=` keeps absolute times under `-copyts`. `pts_origin` 1.421333 s is source.sh's first video PTS (127920 = the 1.4 s mpegts offset + AAC priming). If the SUT shifts PTS, add the offset `report` finds. Not yet run against a real captioned stream.
- **Repeated endings.** An utterance whose final words repeat the previous one can match early: utt 42, "DIRECTION", read 0.58 s instead of 2.0 s.

## Self-test (40 s per stage, first 10 s skipped)

| Between the taps | p50 ms | p95 | p99 | max | σ | PTS offset |
|---|---|---|---|---|---|---|
| nothing (tap → tap) | 0.020 | 0.026 | 0.050 | 0.156 | 0.007 | 0 |
| `latency delay --ms 250` | **250.035** | 250.056 | 250.322 | 252.829 | 0.141 | 0 |
| `ffmpeg -c copy` relay | **133.3** | 200.2 | 200.6 | 237.0 | 47.0 | 0 |
| relay, `-max_interleave_delta 1` | 100.0 | 100.4 | 100.7 | 105.1 | 14.7 | 0 |
| relay, video only | 66.7 | 67.1 | 67.3 | 71.8 | 0.4 | −1920 (found) |
| relay, `-output_ts_offset 10` | 133.3 | 200.2 | 200.4 | 233.3 | 46.9 | +900000 (found) |

- **Relay sawtooth.** The relay's 67–200 ms sawtooth comes from waiting to interleave video with audio: `source.sh` sends audio in PES of about 180 ms. `-max_interleave_delta 0 -flush_packets 1` doesn't change it.
- **Relay startup.** The relay also holds frames for up to 4.6 s at startup while it probes the input, which `--skip` excludes.
- **Captions.** The synthetic SRT (each cue at utterance end + 2.0 s, 2 loops) gave:
  - 120/120 matched, p50 = p95 = max = 2.000 s;
  - with ASR-style damage (last long word misspelled, the word before it dropped): 112/120 (93.3%), all 2.000 s;
  - `--words` with the last words ending 0.3 s early: 2.300 s.
