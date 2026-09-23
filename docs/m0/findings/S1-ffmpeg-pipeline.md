# S1: FFmpeg-library pass-through with caption SEI insertion

> **Summary:** It works. A Rust binary using `ffmpeg-next` 9 on FFmpeg 9.0.1 copies H.264/HEVC from SRT or UDP to several outputs without decoding, adding a CEA-608 SEI to every frame. Captions decode in FFmpeg, mpv and VLC. The pipe adds **0.05 ms p50 / 0.1 ms p99**; end to end it adds **33.4 ms p50 / 34 ms p99**, i.e. one frame spent in the TS demuxer. SRT adds only its configured latency. Reconnects and a 60-minute soak ran without errors.

## Design (`spikes/s1-ffmpeg-pipe`)

- **Input thread:** demuxes packets with `av_read_frame` and never decodes. After an EOF or read error it re-opens, backing off from 250 ms to 2 s.
- **Timestamp rebase:** output timestamps follow one continuous timeline. A new session, or an input PTS jump of < −1 s or > +10 s (the source restarted), starts a new epoch. The epoch continues from the last output packet plus the wall time the input was silent plus 0.5 s. PTS and PCR therefore keep real-time pace, and output DTS never goes backwards.
- **SEI insertion** (`nal.rs`, unit-tested): `00 00 00 01` + `cc::h264_sei_nal`/`hevc_sei_nal`, placed before the first VCL NAL. That puts it after AUD/VPS/SPS/PPS/existing SEI. If a packet has no start code or no slice, it passes through unchanged and is logged.
- **Display order:** with B-frames, captions are handed out in PTS order. The pipe holds `video_delay` frames to do this; FFmpeg reports that value, and it is 0 without B-frames.
- **Outputs:** one thread and one MPEG-TS muxer per output.
  - The input uses `try_send` into a bounded queue. If the queue is full, that output resyncs at the next keyframe, so the input never blocks.
  - An output starts writing at a fresh keyframe and reconnects after a write error.
  - The muxer survives input sessions, so PIDs, continuity counters and PCR stay continuous. A codec change re-opens it.
- **Instrumentation:** a CSV per output (`read_ns`, `write_ns`, PTS, frame index, …) and a status line every 10 s (RSS, counts, drops).

## Latency

Setup: 60 s runs, 720p30, 1 s GOP, no B-frames. Black-box numbers come from the harness `latency tap/report`; "pipe" is `write_ns − read_ns`.

| Codec | In | Pipe p50/p95/p99/max ms | Black-box → UDP out p50/p95/p99/max ms | → SRT out* p50/p99 |
|---|---|---|---|---|
| H.264 | UDP | 0.05/0.08/0.10/0.15 | 33.4/33.7/33.9/34.3 | 153.5/154.0 |
| H.264 | SRT* | 0.05/0.08/0.11/0.15 | 160.6/171.3/172.8/173.6 | 280.8/293.1 |
| HEVC | UDP | 0.05/0.08/0.10/0.15 | 33.5/36.2/69.1/72.1 | 153.6/189.2 |
| HEVC | SRT* | 0.06/0.14/0.41/1.52 | 160.3/174.0/203.0/287.9 | 280.4/323.1 |

\*SRT legs are bridged with `srt-live-transmit`, `latency=120` ms. The bridge alone adds 125 ms p50 / 130 ms p99. Subtracting it leaves about 33–35 ms for the pipe on every path. HEVC's p99 comes from x265 sending frames in bursts at the source.

**Where the time goes: one frame in the TS demuxer.** A video PES has length 0, so libavformat only finishes a frame when the next frame's first TS packet arrives.

- **FFmpeg's H.264/HEVC parser added another frame:** black-box delay was 66.8 ms p50 with the parser and 33.4 ms without.
- **Fix:** the pipe runs with `fflags=+noparse`. The demuxer then stops setting keyframe flags, so the pipe sets them itself from the NAL types (IDR/IRAP).
- **No interleave wait:** the pipe writes with `av_write_frame`, `flush_packets=1` and `pes_payload_size=0`. The harness saw a 133–200 ms sawtooth when it used `av_interleaved_write_frame`.
- **B-frames:** display-order captions hold 2 frames. The pipe then adds 100 ms p50 / 133 ms p95, but only for B-frame sources.

## Validation

- **`verify.sh`:** PASS on UDP and SRT outputs, H.264 and HEVC, in every run, including recordings made across reconnects.
- **ffprobe:** every video frame carries `ATSC A53 Part 4 Closed Captions` side data, and `ffmpeg -v error` reports 0 decode errors.
- **mpv** (headless, `--sub-create-cc-track=yes`, Lua `sub-text` dump) prints the roll-up lines exactly.
- **VLC** adds the CC1 track and loads its `cc` decoder. It ran headless, so rendered text was not checked.
- **B-frames** (x264 `-bf 2`, x265 `bframes=2`): captions stay in order and pass. With `--reorder 0` (decode order) they come out scrambled, e.g. `IOPTFIXTN E URNE TLIWO`. **Captions must follow display order.**
- **`cc` builders:** correct as written. No caption failures were seen.

## Reconnect behaviour

Test pattern: source on 20 s, off 5 s, on 20 s, off 1 s, on 12 s, with PTS restarting at 127920 each time. Downstream `ffmpeg -c copy` recorders stayed connected; their files decode with 0 errors and pass caption verification.

- **UDP in:**
  - The pipe sees no disconnect: `rw_timeout` does not fire for UDP (`udp://…?timeout=` would be needed).
  - Downstream receives **nothing** during the gap, and the demuxer holds the last old frame until new data arrives.
  - On restart the PTS jump starts a new epoch. Output PTS moves ahead by the gap plus about 1.3 s (the 0.5 s margin plus source start-up). The TS stays continuous.
- **SRT in (caller):**
  - A read error arrives about 1 s after the source stops.
  - The pipe resumes 0.3–2.6 s after the source returns, because probing must see a keyframe.
  - Output PTS gaps match the wall-clock gaps to within 0.5 s.
- **Codec change (H.264 → HEVC):**
  - The outputs re-open, and the new stream starts at a keyframe.
  - Over UDP the demuxer keeps the stale codec ID, so the pipe detects the change from the NAL headers after 2 packets and re-opens.
  - Downstream, the SRT output drops its caller.
  - The UDP output re-announces the stream with PMT version 0 again, so a running `-c copy` receiver keeps its H.264 decoder and outputs junk. **A product should bump the PMT version or change PIDs on a codec change.**

## Soak (60 min, H.264, UDP in → 2× UDP + 1× SRT)

- **Run 1:**
  - 108,164 frames, 0 warnings, 0 queue drops.
  - Pipe 0.04 ms p50, 0.11 ms p99, 1.65 ms max.
  - Black-box: UDP 33.4 / 34.0 ms (p50 / p99), SRT 153.5 / 154.1 ms.
  - **No drift:** the mean for each 10-minute block stayed between 0.046 and 0.054 ms.
  - `verify.sh` passed at 59 minutes.
- **RSS in run 1:** it rose from 38 to 237 MB in 7 minutes, then stayed flat. The cause was the input URL's `fifo_size=1000000`, a ring of that many 188-byte packets (188 MB). `fifo_size=50000` fixes it.
- **Run 2** (final code, `fifo_size=50000`): SOAK2.

## Binding crate: `ffmpeg-next` 9.0.0

- **FFmpeg 9 support:** works with FFmpeg 9 / libavformat 63 out of the box. Its version number tracks the FFmpeg major.
- **Build:** 9 s, against the system libraries via pkg-config, using bindgen with clang.
- **Alternative:** `rsmpeg` 0.18 stops at FFmpeg 8.
- **Ergonomics:** reading, remuxing, packets, dictionaries and the input interrupt callback are fine.
- **Unsafe needed (5 blocks):**
  - `output_as_with` passes no interrupt callback to `avio_open2`, so a waiting SRT listener could not be stopped. The pipe opens outputs by hand.
  - Clearing `codec_tag`.
  - Reading `codecpar.video_delay`.
  - An `unsafe impl Sync` for sharing `Parameters`.
- **Minor rough edges:** `Parameters::new()` does not check for NULL, and `Packet::copy` drops side data.

## Gotchas

- `analyzeduration` must cover one GOP; opening takes 0.7–1.1 s.
- Join the output threads before exiting. libsrt's atexit cleanup racing a thread still in `srt_*` caused a `double free` abort.
- When a session ends, B-frame-held frames are flushed up to 3.5 s late. A product should drop them instead.

## Recommendation

FFmpeg libraries with `ffmpeg-next` are good enough for MULTI's pass-through path: negligible processing time, one frame of demux delay that FFmpeg cannot avoid, and robust reconnects. Keep the design of one thread per output with keyframe resync. Getting below one frame would need our own TS demuxer that uses the PES length when the source sets it. That is only worth building if S2 does better.

## Running it

```
cd spikes && cargo build --release -p s1-ffmpeg-pipe
target/release/s1-ffmpeg-pipe run --input 'srt://127.0.0.1:9110?mode=caller' \
  --output 'udp://127.0.0.1:9103?pkt_size=1316' --output 'srt://127.0.0.1:9111?mode=listener' --csv-dir /tmp/s1
s1-ffmpeg-pipe/bench.sh h264 udp 60 /tmp/out         # latency + verify (ports 9101-9112)
s1-ffmpeg-pipe/reconnect.sh h264 srt /tmp/rc [hevc]  # drop/restart test
s1-ffmpeg-pipe/bframes.sh hevc /tmp/bf               # B-frame caption order
s1-ffmpeg-pipe/soak.sh 3600 /tmp/soak                # ports 9121-9126
```

`s1-ffmpeg-pipe stats out0.csv` prints pipe percentiles. Flags: `--no-captions`, `--reorder N`, `--queue N`, `--in-opt k=v` (replaces the defaults), `--out-opt k=v`.
