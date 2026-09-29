//! `multi run` with a Spanish speaker (M2-2), real GStreamer and the fake
//! workers (no GPU): `source.sh` -> `multi run` -> UDP out.
//!
//! Config: ES is the source on CC1, EN a translation on CC3, FR on 708
//! service 2. Checks: the fake ASR words land on the Spanish lane (CC1)
//! untranslated, the fake MT answers `[en] …` on CC3, the source language
//! is never sent to MT (no `[es]` anywhere), and video keeps flowing.
//!
//! Re-runs itself as the fake worker (`MULTI_FAKE_WORKER=1`). Needs
//! `ffmpeg`, `ffprobe` and `gst-launch-1.0`. Ports 9780–9781.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitCode, Stdio};
use std::thread::sleep;
use std::time::{Duration, Instant};

const ENV: &str = "MULTI_FAKE_WORKER";
const IN_PORT: u16 = 9780;
const UDP_OUT_PORT: u16 = 9781;
const WORDS: [&str; 6] = multi_fake_worker::SCRIPT;

fn main() -> ExitCode {
    if std::env::var_os(ENV).is_some() {
        return multi_fake_worker::main_from(std::env::args_os());
    }
    let t = Instant::now();
    match std::panic::catch_unwind(spanish_speaker) {
        Ok(()) => {
            println!("test spanish_speaker ... ok ({} s)", t.elapsed().as_secs());
            println!("\ntest result: 1 passed; 0 failed");
            ExitCode::SUCCESS
        }
        Err(_) => {
            println!("test spanish_speaker ... FAILED");
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

fn spanish_speaker() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    let dir = std::env::temp_dir().join(format!("multi-source-lang-{}", std::process::id()));
    fs::create_dir_all(&dir).unwrap();
    println!("work dir {}", dir.display());

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
        .arg("sine=frequency=440:duration=60")
        .args(["-ar", "48000", "-ac", "2"])
        .arg(&wav));
    let config = dir.join("multi.toml");
    fs::write(
        &config,
        format!(
            "[input]\nurl = \"udp://127.0.0.1:{IN_PORT}\"\n\n\
             [[outputs]]\nurl = \"udp://127.0.0.1:{UDP_OUT_PORT}\"\n\n\
             [[languages]]\ncode = \"es\"\nsource = true\ncc608 = \"cc1\"\ncea708_service = 1\n\n\
             [[languages]]\ncode = \"en\"\ncc608 = \"cc3\"\ncea708_service = 3\npriority = 1\n\n\
             [[languages]]\ncode = \"fr\"\ncea708_service = 2\npriority = 2\n"
        ),
    )
    .unwrap();
    let cfg = multi_core::Config::load(&config).unwrap();
    assert!(cfg.validate().is_empty(), "{:?}", cfg.validate());

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
    let _source = start_source(&root, &wav, &dir, "source.log");
    sleep(Duration::from_secs(4));
    let a = record_udp(&dir, "a.ts", 12);
    let frames = video_packets(&a);
    assert!(
        frames >= 12 * 30 * 8 / 10,
        "only {frames} video frames in 12 s"
    );
    let cc1 = cc608(&a, false);
    let cc3 = cc608(&a, true);
    assert!(
        has_script_word(&cc1),
        "no Spanish (source) words on CC1: {cc1:.300}"
    );
    assert!(
        !cc1.contains('['),
        "source lane carries a translation: {cc1:.300}"
    );
    assert!(
        cc3.contains("[en]"),
        "no English translation on CC3: {cc3:.300}"
    );
    assert!(
        !cc3.contains("[es]"),
        "the source language went to MT: {cc3:.300}"
    );
    let log = fs::read_to_string(dir.join("multi.log")).unwrap_or_default();
    assert!(
        log.lines()
            .any(|l| l.contains("lanes_pushed_dropped_queued") && l.contains("fr:")),
        "no FR lane in the stats"
    );
    println!("ES source on CC1, [en] on CC3, no [es] translation; {frames} video frames");

    signal(multi.0.id(), "-INT");
    wait_exit(&mut multi, Duration::from_secs(20)).expect("multi did not stop after SIGINT");
    let _ = fs::remove_dir_all(&dir);
}
