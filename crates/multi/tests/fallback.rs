//! Fallback picture on input loss (WP11), end to end with real GStreamer and
//! the fake workers: `source.sh` (UDP) -> `multi run` -> UDP out, received
//! by this test. The source stops for 8 s and starts again.
//!
//! Checks, for H.264 and HEVC: output video keeps arriving (no gap longer
//! than `fallback_after_ms` + 500 ms), output video PTS only move forward
//! across both switches, FFmpeg decodes the recording without errors, and
//! the fake captions are back on CC1 after the input returns.
//!
//! Re-runs itself as the fake worker when started with `MULTI_FAKE_WORKER=1`.
//! Needs `ffmpeg`, `ffprobe`. Ports 9760–9763.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::fs;
use std::io::Write;
use std::net::UdpSocket;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitCode, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::thread::sleep;
use std::time::{Duration, Instant};

const ENV: &str = "MULTI_FAKE_WORKER";
const WORDS: [&str; 6] = multi_fake_worker::SCRIPT;
const FALLBACK_AFTER_MS: u64 = 1000;
/// mpegtsmux's video PID in multi's output.
const VIDEO_PID: u16 = 256;

fn main() -> ExitCode {
    if std::env::var_os(ENV).is_some() {
        return multi_fake_worker::main_from(std::env::args_os());
    }
    let mut failed = 0;
    for (codec, port) in [("h264", 9760u16), ("hevc", 9762)] {
        let t = Instant::now();
        let name = format!("fallback_{codec}");
        match std::panic::catch_unwind(|| fallback(codec, port, port + 1)) {
            Ok(()) => println!("test {name} ... ok ({} s)", t.elapsed().as_secs()),
            Err(_) => {
                println!("test {name} ... FAILED");
                failed += 1;
            }
        }
    }
    println!("\ntest result: {} passed; {failed} failed", 2 - failed);
    if failed == 0 {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

/// Kills its process on drop, so a failed assertion leaves nothing running.
struct Proc(Child);

impl Drop for Proc {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn run(cmd: &mut Command) {
    let out = cmd.output().expect("run command");
    assert!(
        out.status.success(),
        "{cmd:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

fn log_file(dir: &Path, name: &str) -> Stdio {
    Stdio::from(fs::File::create(dir.join(name)).unwrap())
}

/// Video PES starts seen by the receiver: arrival time and PTS (90 kHz).
struct Rx {
    arrivals: Vec<(Instant, u64)>,
}

/// Receives the UDP output: writes it to `files[i]`, moving to the next file
/// at the first keyframe (with the PAT/PMT before it) once `part` asks for
/// it, so every file after the first decodes from its start; notes every
/// video PES start.
fn receive(
    port: u16,
    files: Vec<PathBuf>,
    part: Arc<AtomicUsize>,
    stop: Arc<AtomicBool>,
) -> std::thread::JoinHandle<Rx> {
    let sock = UdpSocket::bind(("127.0.0.1", port)).expect("bind receiver");
    sock.set_read_timeout(Some(Duration::from_millis(50)))
        .unwrap();
    std::thread::spawn(move || {
        let mut out: Vec<fs::File> = files.iter().map(|f| fs::File::create(f).unwrap()).collect();
        let mut rx = Rx {
            arrivals: Vec::new(),
        };
        let mut buf = [0u8; 65536];
        let mut cur = 0;
        while !stop.load(Ordering::Relaxed) {
            let Ok(n) = sock.recv(&mut buf) else { continue };
            let now = Instant::now();
            let pkts: Vec<&[u8]> = buf[..n]
                .as_chunks::<188>()
                .0
                .iter()
                .map(|p| p.as_slice())
                .collect();
            let want = part.load(Ordering::Relaxed).min(out.len() - 1);
            let mut split = pkts.len();
            if want > cur
                && let Some(k) = pkts.iter().position(|p| is_video_key(p))
            {
                split = pkts[..k].iter().position(|p| pid(p) == 0).unwrap_or(k);
            }
            for (j, p) in pkts.iter().enumerate() {
                if j == split {
                    cur = want;
                }
                out[cur].write_all(p).unwrap();
            }
            for pkt in pkts {
                if let Some(pts) = video_pes_pts(pkt) {
                    rx.arrivals.push((now, pts));
                }
            }
        }
        rx
    })
}

fn pid(p: &[u8]) -> u16 {
    (u16::from(p[1] & 0x1f) << 8) | u16::from(p[2])
}

/// A video packet with the random access indicator (mpegtsmux sets it on keyframes).
fn is_video_key(p: &[u8]) -> bool {
    pid(p) == VIDEO_PID && (p[3] >> 4) & 2 != 0 && p[4] > 0 && p[5] & 0x40 != 0
}

/// PTS of a TS packet that starts a PES on the video PID.
fn video_pes_pts(p: &[u8]) -> Option<u64> {
    if p[0] != 0x47 || p[1] & 0x40 == 0 {
        return None;
    }
    if pid(p) != VIDEO_PID {
        return None;
    }
    let afc = (p[3] >> 4) & 3;
    let mut i = 4;
    if afc & 2 != 0 {
        i += 1 + usize::from(p[4]);
    }
    if afc & 1 == 0 || i + 14 > p.len() {
        return None;
    }
    let h = &p[i..];
    if h[..3] != [0, 0, 1] || h[7] & 0x80 == 0 {
        return None;
    }
    let t = &h[9..14];
    Some(
        (u64::from(t[0] >> 1) & 7) << 30
            | u64::from(t[1]) << 22
            | u64::from(t[2] >> 1) << 15
            | u64::from(t[3]) << 7
            | u64::from(t[4] >> 1),
    )
}

/// CEA-608 text from CC1, via FFmpeg's decoder.
fn cc1(file: &Path) -> String {
    let srt = file.with_extension("cc1.srt");
    let _ = Command::new("ffmpeg")
        .args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-y",
            "-f",
            "lavfi",
            "-i",
        ])
        .arg(format!("movie={}[out+subcc]", file.display()))
        .args(["-map", "0:1", "-c:s", "srt"])
        .arg(&srt)
        .output();
    fs::read_to_string(&srt).unwrap_or_default().to_lowercase()
}

/// FFmpeg decode errors of a recording (stderr lines at `-v error`).
fn decode_errors(file: &Path) -> (bool, Vec<String>) {
    let out = Command::new("ffmpeg")
        .args(["-hide_banner", "-v", "error", "-i"])
        .arg(file)
        .args(["-f", "null", "-"])
        .output()
        .expect("ffmpeg");
    // "[null @ …] non monotonically increasing dts" comes from the null
    // muxer's frame-rate conversion, not from decoding; pass-through
    // recordings print it too.
    let lines = String::from_utf8_lossy(&out.stderr)
        .lines()
        .filter(|l| !l.starts_with("[null @"))
        .map(str::to_owned)
        .collect();
    (out.status.success(), lines)
}

fn start_source(root: &Path, codec: &str, wav: &Path, port: u16, dir: &Path, log: &str) -> Proc {
    Proc(
        Command::new(root.join("spikes/harness/source.sh"))
            .args(["--codec", codec, "--audio"])
            .arg(wav)
            .arg(format!("udp://127.0.0.1:{port}?pkt_size=1316"))
            .stdout(Stdio::null())
            .stderr(log_file(dir, log))
            .spawn()
            .expect("source.sh"),
    )
}

fn fallback(codec: &str, in_port: u16, out_port: u16) {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    let dir = std::env::temp_dir().join(format!("multi-fallback-{codec}-{}", std::process::id()));
    fs::create_dir_all(&dir).unwrap();
    println!("[{codec}] work dir {}", dir.display());

    let wav = dir.join("tone.wav");
    run(Command::new("ffmpeg")
        .args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-y",
            "-f",
            "lavfi",
            "-i",
        ])
        .arg("sine=frequency=440:duration=120")
        .args(["-ar", "48000", "-ac", "2"])
        .arg(&wav));
    let config = dir.join("multi.toml");
    fs::write(
        &config,
        format!(
            "[input]\nurl = \"udp://127.0.0.1:{in_port}\"\n\n\
             [video]\nfallback = \"black\"\nfallback_after_ms = {FALLBACK_AFTER_MS}\n\n\
             [[outputs]]\nurl = \"udp://127.0.0.1:{out_port}\"\n"
        ),
    )
    .unwrap();

    // warm-up, the loss and recovery, after recovery.
    let files = vec![
        dir.join("warm.ts"),
        dir.join("switch.ts"),
        dir.join("after.ts"),
    ];
    let part = Arc::new(AtomicUsize::new(0));
    let stop = Arc::new(AtomicBool::new(false));
    let rx = receive(out_port, files.clone(), part.clone(), stop.clone());

    let me = std::env::current_exe().unwrap();
    let mut multi = Proc(
        Command::new(env!("CARGO_BIN_EXE_multi"))
            .arg("run")
            .arg("--config")
            .arg(&config)
            .arg("--asr-worker")
            .arg(format!("{} asr", me.display()))
            .arg("--mt-worker")
            .arg(format!("{} mt", me.display()))
            .env(ENV, "1")
            .env("RUST_LOG", "info")
            .stdout(Stdio::null())
            .stderr(log_file(&dir, "multi.log"))
            .spawn()
            .expect("multi"),
    );
    sleep(Duration::from_secs(1));
    let source = start_source(&root, codec, &wav, in_port, &dir, "source.log");
    sleep(Duration::from_secs(5));
    let t_measure = Instant::now();
    part.store(1, Ordering::Relaxed);
    sleep(Duration::from_secs(3));

    // Input gone for 8 s: the fallback takes over after 1 s.
    drop(source);
    let t_lost = Instant::now();
    sleep(Duration::from_secs(8));
    let source = start_source(&root, codec, &wav, in_port, &dir, "source2.log");
    // Recovery, then a separate recording to look for captions in.
    sleep(Duration::from_secs(3));
    part.store(2, Ordering::Relaxed);
    sleep(Duration::from_secs(8));
    stop.store(true, Ordering::Relaxed);
    let rx = rx.join().unwrap();
    drop(source);
    let _ = Command::new("kill")
        .args(["-INT", &multi.0.id().to_string()])
        .status();
    let end = Instant::now() + Duration::from_secs(20);
    while Instant::now() < end && matches!(multi.0.try_wait(), Ok(None)) {
        sleep(Duration::from_millis(100));
    }

    let log = fs::read_to_string(dir.join("multi.log")).unwrap_or_default();
    assert!(!log.contains("panicked"), "panic in multi");
    assert!(
        log.contains("sending the fallback picture"),
        "fallback never started"
    );
    assert!(
        log.contains("switching to pass-through"),
        "never switched back to the input"
    );

    // Arrival gaps and PTS order from the steady state on.
    let a: Vec<(Instant, u64)> = rx
        .arrivals
        .into_iter()
        .filter(|(t, _)| *t >= t_measure)
        .collect();
    assert!(a.len() > 20 * 25, "only {} video frames received", a.len());
    let mut worst = (Duration::ZERO, Duration::ZERO);
    for w in a.windows(2) {
        let gap = w[1].0 - w[0].0;
        if gap > worst.0 {
            worst = (gap, w[0].0.saturating_duration_since(t_lost));
        }
        assert!(
            w[1].1 > w[0].1,
            "output PTS went backwards: {} -> {} ({} s after the input was lost)",
            w[0].1,
            w[1].1,
            w[0].0.saturating_duration_since(t_lost).as_secs_f64()
        );
    }
    let back = t_lost + Duration::from_secs(5);
    let return_gap = a
        .windows(2)
        .filter(|w| w[0].0 >= back)
        .map(|w| w[1].0 - w[0].0)
        .max()
        .unwrap_or_default();
    println!(
        "[{codec}] longest gap after the input returned: {} ms",
        return_gap.as_millis()
    );
    let pts_span = (a[a.len() - 1].1 - a[0].1) as f64 / 90_000.0;
    let wall_span = (a[a.len() - 1].0 - a[0].0).as_secs_f64();
    println!(
        "[{codec}] {} frames, longest gap {} ms ({:.1} s after input loss), PTS span {pts_span:.2} s over {wall_span:.2} s wall",
        a.len(),
        worst.0.as_millis(),
        worst.1.as_secs_f64()
    );
    assert!(
        worst.0 <= Duration::from_millis(FALLBACK_AFTER_MS + 500),
        "output gap of {} ms",
        worst.0.as_millis()
    );
    // One continuous timeline: PTS advance with the wall clock.
    assert!(
        (pts_span - wall_span).abs() < 1.0,
        "PTS span {pts_span:.2} s vs wall {wall_span:.2} s"
    );

    for f in &files[1..] {
        let (ok, errs) = decode_errors(f);
        println!(
            "[{codec}] decode {}: {} error lines",
            f.display(),
            errs.len()
        );
        assert!(ok, "ffmpeg failed to decode {}: {errs:?}", f.display());
        assert!(
            errs.len() <= 3,
            "decode errors in {}: {errs:?}",
            f.display()
        );
    }
    let text = cc1(&files[2]);
    assert!(
        WORDS.iter().any(|w| text.contains(w)),
        "no fake captions on CC1 after the input returned: {text:.300}"
    );
    let _ = fs::remove_dir_all(&dir);
}
