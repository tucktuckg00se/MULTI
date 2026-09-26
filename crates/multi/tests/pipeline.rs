//! `multi run` end to end with real GStreamer and the fake workers (no GPU):
//! `source.sh` (UDP, H.264) -> `multi run` -> UDP and RTMP out.
//!
//! Checks: fake ASR words on CC1, fake translations (`[es] …`) on CC3,
//! captions in the RTMP (FLV) output, video keeps flowing (and captions come
//! back) after the ASR worker is killed, and Ctrl-C exits cleanly.
//!
//! Like `supervisor.rs`, this binary is its own fake worker when started with
//! `MULTI_FAKE_WORKER=1`. Needs `ffmpeg`, `ffprobe` and `gst-launch-1.0`.
//! Ports 9720–9722.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitCode, Stdio};
use std::thread::sleep;
use std::time::{Duration, Instant};

const ENV: &str = "MULTI_FAKE_WORKER";
const IN_PORT: u16 = 9720;
const UDP_OUT_PORT: u16 = 9721;
const RTMP_PORT: u16 = 9722;
const WORDS: [&str; 6] = multi_fake_worker::SCRIPT;

fn main() -> ExitCode {
    if std::env::var_os(ENV).is_some() {
        return multi_fake_worker::main_from(std::env::args_os());
    }
    let t = Instant::now();
    let result = std::panic::catch_unwind(end_to_end);
    match result {
        Ok(()) => {
            println!("test end_to_end ... ok ({} s)", t.elapsed().as_secs());
            println!("\ntest result: 1 passed; 0 failed");
            ExitCode::SUCCESS
        }
        Err(_) => {
            println!("test end_to_end ... FAILED");
            println!("\ntest result: 0 passed; 1 failed");
            ExitCode::FAILURE
        }
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

fn signal(pid: u32, sig: &str) {
    let _ = Command::new("kill").args([sig, &pid.to_string()]).status();
}

fn wait_exit(p: &mut Proc, limit: Duration) -> Option<std::process::ExitStatus> {
    let end = Instant::now() + limit;
    while Instant::now() < end {
        if let Ok(Some(s)) = p.0.try_wait() {
            return Some(s);
        }
        sleep(Duration::from_millis(100));
    }
    None
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

/// Records the UDP output byte-exact for `secs`.
fn record_udp(dir: &Path, name: &str, secs: u64) -> PathBuf {
    let path = dir.join(name);
    let child = Command::new("gst-launch-1.0")
        .args([
            "-eq",
            "udpsrc",
            &format!("port={UDP_OUT_PORT}"),
            "buffer-size=8388608",
            "!",
            "filesink",
            &format!("location={}", path.display()),
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("gst-launch-1.0");
    let mut p = Proc(child);
    sleep(Duration::from_secs(secs));
    signal(p.0.id(), "-INT");
    assert!(
        wait_exit(&mut p, Duration::from_secs(10)).is_some(),
        "recorder did not stop"
    );
    path
}

fn video_packets(file: &Path) -> u64 {
    let out = Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-select_streams",
            "v:0",
            "-count_packets",
            "-show_entries",
            "stream=nb_read_packets",
            "-of",
            "csv=p=0",
        ])
        .arg(file)
        .output()
        .expect("ffprobe");
    // One line per stream, then per program; the first is the stream.
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .find_map(|l| l.trim().trim_end_matches(',').parse().ok())
        .unwrap_or(0)
}

/// CEA-608 text from field 1 (CC1) or field 2 (CC3), via FFmpeg's decoder.
fn cc608(file: &Path, second_field: bool) -> String {
    let srt = file.with_extension(if second_field { "cc3.srt" } else { "cc1.srt" });
    let mut cmd = Command::new("ffmpeg");
    cmd.args(["-hide_banner", "-loglevel", "error", "-y"]);
    if second_field {
        cmd.args(["-data_field", "second"]);
    }
    cmd.args(["-f", "lavfi", "-i"])
        .arg(format!("movie={}[out+subcc]", file.display()))
        .args(["-map", "0:1", "-c:s", "srt"])
        .arg(&srt);
    let _ = cmd.output();
    fs::read_to_string(&srt).unwrap_or_default().to_lowercase()
}

fn has_script_word(text: &str) -> bool {
    WORDS.iter().any(|w| text.contains(w))
}

/// Pids of `parent`'s child processes whose command line contains `arg`.
fn children_with_arg(parent: u32, arg: &str) -> Vec<u32> {
    let mut out = Vec::new();
    let Ok(tasks) = fs::read_dir(format!("/proc/{parent}/task")) else {
        return out;
    };
    for t in tasks.flatten() {
        let kids = fs::read_to_string(t.path().join("children")).unwrap_or_default();
        for pid in kids
            .split_whitespace()
            .filter_map(|p| p.parse::<u32>().ok())
        {
            let cmdline = fs::read(format!("/proc/{pid}/cmdline")).unwrap_or_default();
            if cmdline.split(|b| *b == 0).any(|a| a == arg.as_bytes()) {
                out.push(pid);
            }
        }
    }
    out
}

/// `source.sh` (H.264, 30 fps, 1 s GOP) to the UDP input. Each start
/// begins again at the same PTS, like a restarted encoder.
fn start_source(root: &Path, wav: &Path, dir: &Path, log: &str) -> Proc {
    Proc(
        Command::new(root.join("spikes/harness/source.sh"))
            .arg("--audio")
            .arg(wav)
            .arg(format!("udp://127.0.0.1:{IN_PORT}?pkt_size=1316"))
            .stdout(Stdio::null())
            .stderr(log_file(dir, log))
            .spawn()
            .expect("source.sh"),
    )
}

fn end_to_end() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    let dir = std::env::temp_dir().join(format!("multi-pipeline-{}", std::process::id()));
    fs::create_dir_all(&dir).unwrap();
    println!("work dir {}", dir.display());

    // Audio content does not matter to the fake ASR; a tone keeps CI free of media files.
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
            "[input]\nurl = \"udp://127.0.0.1:{IN_PORT}\"\n\n\
             [[outputs]]\nurl = \"udp://127.0.0.1:{UDP_OUT_PORT}\"\n\n\
             [[outputs]]\nurl = \"rtmp://127.0.0.1:{RTMP_PORT}/live/test\"\n"
        ),
    )
    .unwrap();

    // RTMP receiver: FFmpeg as a one-shot RTMP server, recording FLV.
    let flv = dir.join("rtmp.flv");
    let mut rtmp = Proc(
        Command::new("ffmpeg")
            .args([
                "-hide_banner",
                "-loglevel",
                "error",
                "-y",
                "-listen",
                "1",
                "-i",
            ])
            .arg(format!("rtmp://127.0.0.1:{RTMP_PORT}/live/test"))
            .args(["-t", "12", "-c", "copy"])
            .arg(&flv)
            .stdout(Stdio::null())
            .stderr(log_file(&dir, "rtmp.log"))
            .spawn()
            .expect("ffmpeg rtmp listener"),
    );

    sleep(Duration::from_millis(500));
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
    let source = start_source(&root, &wav, &dir, "source.log");

    // Let the pipeline settle, then record 15 s.
    sleep(Duration::from_secs(4));
    let a = record_udp(&dir, "a.ts", 15);
    let frames = video_packets(&a);
    println!("recording A: {frames} video frames");
    assert!(
        frames >= 15 * 30 * 8 / 10,
        "only {frames} video frames in 15 s"
    );
    let cc1 = cc608(&a, false);
    assert!(
        has_script_word(&cc1),
        "no fake ASR words on CC1: {cc1:.300}"
    );
    let cc3 = cc608(&a, true);
    assert!(
        cc3.contains("[es]"),
        "no fake translation on CC3: {cc3:.300}"
    );
    println!("CC1 and CC3 carry the fake ASR and MT text");

    // RTMP: the listener stops by itself after 12 s of input.
    assert!(
        wait_exit(&mut rtmp, Duration::from_secs(20)).is_some(),
        "RTMP recording did not finish"
    );
    let cc_rtmp = cc608(&flv, false);
    let rtmp_frames = video_packets(&flv);
    println!("RTMP: {rtmp_frames} video frames");
    assert!(rtmp_frames >= 8 * 30, "only {rtmp_frames} frames over RTMP");
    assert!(
        has_script_word(&cc_rtmp),
        "no captions in the RTMP output: {cc_rtmp:.300}"
    );

    // Kill the ASR worker: video must keep flowing, captions must come back.
    let asr = children_with_arg(multi.0.id(), "asr");
    assert_eq!(asr.len(), 1, "expected one fake ASR worker, found {asr:?}");
    signal(asr[0], "-KILL");
    let b = record_udp(&dir, "b.ts", 10);
    let frames = video_packets(&b);
    println!("recording B (ASR killed at its start): {frames} video frames");
    assert!(
        frames >= 10 * 30 * 8 / 10,
        "video stalled after the ASR kill: {frames} frames in 10 s"
    );
    let cc1 = cc608(&b, false);
    assert!(
        has_script_word(&cc1),
        "captions did not come back after the ASR kill: {cc1:.300}"
    );
    let restarted = children_with_arg(multi.0.id(), "asr");
    assert!(
        restarted.len() == 1 && restarted[0] != asr[0],
        "ASR worker not restarted"
    );

    // Source restart (PTS starts over): the input watchdog rebuilds the
    // input and the bridge opens a new session; the output carries on.
    drop(source);
    sleep(Duration::from_secs(3));
    let source = start_source(&root, &wav, &dir, "source2.log");
    sleep(Duration::from_secs(2));
    let c = record_udp(&dir, "c.ts", 8);
    let frames = video_packets(&c);
    println!("recording C (after a source restart): {frames} video frames");
    assert!(
        frames >= 8 * 30 * 7 / 10,
        "video did not resume after a source restart: {frames} frames in 8 s"
    );
    assert!(
        has_script_word(&cc608(&c, false)),
        "no captions after a source restart"
    );

    // Ctrl-C: clean exit.
    signal(multi.0.id(), "-INT");
    let status =
        wait_exit(&mut multi, Duration::from_secs(20)).expect("multi did not stop after SIGINT");
    assert!(status.success(), "multi exited with {status}");
    let log = fs::read_to_string(dir.join("multi.log")).unwrap_or_default();
    assert!(log.contains("stats"), "no stats line in the log");
    assert!(!log.contains("panicked"), "panic in multi");
    assert!(log.contains("input silent"), "input watchdog did not fire");
    assert!(
        log.contains("session=2"),
        "no second input session after the restart"
    );
    drop(source);
    let _ = fs::remove_dir_all(&dir);
}
