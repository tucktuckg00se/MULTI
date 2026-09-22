//! Round-trip helpers: make a test elementary stream with the FFmpeg CLI,
//! insert caption SEI, remux to MPEG-TS and decode captions back.
#![allow(dead_code)]

use cc::annexb::{VideoCodec, insert_cc_sei};
use cc::{CcMux, FrameRate};
use std::path::{Path, PathBuf};
use std::process::Command;

pub fn ffmpeg_available() -> bool {
    Command::new("ffmpeg")
        .arg("-version")
        .output()
        .is_ok_and(|o| o.status.success())
}

pub fn work_dir(name: &str) -> PathBuf {
    let d = PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
        .join("roundtrip")
        .join(name);
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).expect("create work dir");
    d
}

pub fn fps_arg(rate: FrameRate) -> &'static str {
    match rate {
        FrameRate::Fps23_976 => "24000/1001",
        FrameRate::Fps24 => "24",
        FrameRate::Fps25 => "25",
        FrameRate::Fps29_97 => "30000/1001",
        FrameRate::Fps30 => "30",
        FrameRate::Fps50 => "50",
        FrameRate::Fps59_94 => "60000/1001",
        FrameRate::Fps60 => "60",
    }
}

pub fn run(cmd: &mut Command) -> std::process::Output {
    let out = cmd.output().expect("spawn");
    assert!(
        out.status.success(),
        "{:?} failed:\n{}",
        cmd,
        String::from_utf8_lossy(&out.stderr)
    );
    out
}

/// Encodes `secs` seconds of testsrc2 as an Annex-B elementary stream (no B-frames).
pub fn make_source(dir: &Path, codec: VideoCodec, rate: FrameRate, secs: u32) -> PathBuf {
    let (enc, ext, fmt) = match codec {
        VideoCodec::H264 => ("libx264", "h264", "h264"),
        VideoCodec::Hevc => ("libx265", "hevc", "hevc"),
    };
    let src = dir.join(format!("src.{ext}"));
    let mut c = Command::new("ffmpeg");
    c.args([
        "-hide_banner",
        "-loglevel",
        "error",
        "-y",
        "-f",
        "lavfi",
        "-i",
    ])
    .arg(format!("testsrc2=size=640x360:rate={}", fps_arg(rate)))
    .args([
        "-t",
        &secs.to_string(),
        "-c:v",
        enc,
        "-bf",
        "0",
        "-pix_fmt",
        "yuv420p",
    ]);
    if codec == VideoCodec::Hevc {
        c.args(["-x265-params", "log-level=error"]);
    }
    c.args(["-f", fmt]).arg(&src);
    run(&mut c);
    src
}

/// A scripted caption action at a frame.
pub type Action = Box<dyn Fn(&mut CcMux)>;

/// Inserts captions produced by `mux` + `script` into `src`, writes the ES and
/// an MPEG-TS remux. Returns the .ts path and number of frames.
pub fn caption_stream(
    dir: &Path,
    src: &Path,
    codec: VideoCodec,
    mut mux: CcMux,
    script: &[(u64, Action)],
) -> (PathBuf, u64) {
    let es = std::fs::read(src).expect("read src");
    let rate = mux.rate();
    let (out, frames) = insert_cc_sei(&es, codec, |frame| {
        for (at, act) in script {
            if *at == frame {
                act(&mut mux);
            }
        }
        mux.next_frame()
    });
    let ext = src.extension().and_then(|e| e.to_str()).unwrap_or("es");
    let es_out = dir.join(format!("captioned.{ext}"));
    std::fs::write(&es_out, out).expect("write es");
    let ts = dir.join("captioned.ts");
    run(Command::new("ffmpeg")
        .args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-y",
            "-framerate",
            fps_arg(rate),
            "-i",
        ])
        .arg(&es_out)
        .args(["-c", "copy", "-f", "mpegts"])
        .arg(&ts));
    (ts, frames)
}

/// Decodes 608 captions with FFmpeg's decoder (`field`: "first" = CC1/CC2,
/// "second" = CC3/CC4). Returns the SRT text.
pub fn ffmpeg_608(ts: &Path, field: &str) -> String {
    let srt = ts.with_file_name(format!("ffmpeg-{field}.srt"));
    let path = ts.to_str().expect("utf8 path");
    run(Command::new("ffmpeg")
        .args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-y",
            "-data_field",
            field,
            "-f",
            "lavfi",
            "-i",
        ])
        .arg(format!("movie={path}[out+subcc]"))
        .args(["-map", "0:1", "-c:s", "srt"])
        .arg(&srt));
    std::fs::read_to_string(&srt).unwrap_or_default()
}

/// Caption text of an SRT with numbers, timings and blank lines removed,
/// joined by spaces with whitespace collapsed.
pub fn srt_text(srt: &str) -> String {
    let body: Vec<&str> = srt
        .lines()
        .map(|l| l.trim_matches([' ', '\r']))
        .filter(|l| !l.is_empty() && !l.contains("-->") && l.parse::<u64>().is_err())
        .collect();
    // Split on spaces only: ccextractor's 708 output contains raw control bytes
    // (e.g. 0x0C for G2 `Œ`) that `split_whitespace` would eat.
    body.join(" ")
        .split(' ')
        .filter(|w| !w.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

/// Path to a ccextractor binary: $CCEXTRACTOR or ~/.cache/multi-tools/ccx-build/ccextractor.
pub fn ccextractor() -> Option<PathBuf> {
    let p = std::env::var_os("CCEXTRACTOR")
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME")
                .map(|h| PathBuf::from(h).join(".cache/multi-tools/ccx-build/ccextractor"))
        })?;
    p.exists().then_some(p)
}
