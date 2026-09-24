//! S2b render tool: one caption scenario, two encoders, same video.
//!
//! ```text
//! s2b-gst-cc render scenarios/ru3-json.json --out /tmp/s2b/ru3-json   # or: run.sh ru3-json
//! ```
//! Writes `gst.ts` (GStreamer `tttocea608`/`tttocea708` driven per frame by
//! [`s2b_gst_cc::GstCc`]) and `ours.ts` (`cc::CcMux`), both with the same
//! testsrc2 H.264/HEVC and SEI inserted by `cc::annexb`, plus per-frame cc_data
//! dumps and stats. `decode.sh` then decodes both with FFmpeg, ccextractor and
//! libcaption.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};
use cc::annexb::{VideoCodec, insert_cc_sei};
use cc::{Cc608Encoder, Cc708Encoder, CcMux, CcTriple, Channel, FrameRate, Mode608};
use clap::{Parser, Subcommand};
use s2b_gst_cc::{GstCc, Input, TrackCfg, triples};
use serde::Deserialize;

#[derive(Parser)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Render a scenario with both encoders.
    Render {
        scenario: PathBuf,
        #[arg(long)]
        out: PathBuf,
    },
}

#[derive(Deserialize)]
struct Scenario {
    #[serde(default = "d_fps")]
    fps: (i32, i32),
    secs: u32,
    #[serde(default = "d_codec")]
    codec: String,
    tracks: Vec<Track>,
    events: Vec<Event>,
    /// Send GAPs every frame even when the encoder is ahead (default false).
    #[serde(default)]
    gap_always: bool,
}
fn d_fps() -> (i32, i32) {
    (30, 1)
}
fn d_codec() -> String {
    "h264".into()
}

#[derive(Deserialize)]
struct Track {
    name: String,
    gst: Option<TrackCfg>,
    ours: Option<Ours>,
}

#[derive(Deserialize)]
struct Ours {
    /// cc1..cc4
    cc: Option<String>,
    /// 1..6
    svc: Option<u8>,
    /// ru2, ru3, ru4, popon, painton (608 only)
    #[serde(default)]
    mode: Option<String>,
}

#[derive(Deserialize)]
struct Event {
    /// Seconds.
    t: f64,
    track: String,
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    json: Option<serde_json::Value>,
    /// Raw bytes as hex for GStreamer (invalid UTF-8 / invalid JSON tests).
    #[serde(default)]
    raw_hex: Option<String>,
    /// Text for our encoder if it differs (default: `text`, or the JSON lines joined by `\n`).
    #[serde(default)]
    ours_text: Option<String>,
    /// Buffer duration for GStreamer in seconds (pop-on display time). Default one frame.
    #[serde(default)]
    dur: Option<f64>,
    /// Send `rstranscribe/final-transcript` first (roll-up: new row).
    #[serde(default)]
    cr: bool,
    /// Repeat this event every `every` seconds `count` times (bursts).
    #[serde(default)]
    every: Option<f64>,
    #[serde(default)]
    count: Option<u32>,
}

fn rate_of(fps: (i32, i32)) -> Result<FrameRate> {
    FrameRate::from_fps(f64::from(fps.0) / f64::from(fps.1)).context("unsupported frame rate")
}

fn fps_arg(fps: (i32, i32)) -> String {
    format!("{}/{}", fps.0, fps.1)
}

fn run(cmd: &mut Command) -> Result<()> {
    let o = cmd.output().with_context(|| format!("spawn {cmd:?}"))?;
    if !o.status.success() {
        bail!("{cmd:?}: {}", String::from_utf8_lossy(&o.stderr));
    }
    Ok(())
}

fn json_text(v: &serde_json::Value) -> String {
    let mut lines = Vec::new();
    if let Some(ls) = v.get("lines").and_then(|l| l.as_array()) {
        for l in ls {
            let mut s = String::new();
            if let Some(cs) = l.get("chunks").and_then(|c| c.as_array()) {
                for c in cs {
                    s.push_str(c.get("text").and_then(|t| t.as_str()).unwrap_or(""));
                }
            }
            lines.push(s);
        }
    }
    lines.join("\n")
}

fn main() -> Result<()> {
    gst::init()?;
    match Cli::parse().cmd {
        Cmd::Render { scenario, out } => render(&scenario, &out),
    }
}

struct Ev<'a> {
    frame: u64,
    track: usize,
    e: &'a Event,
}

fn render(path: &Path, out: &Path) -> Result<()> {
    let sc: Scenario = serde_json::from_slice(&std::fs::read(path)?).context("scenario json")?;
    std::fs::create_dir_all(out)?;
    let rate = rate_of(sc.fps)?;
    let fps_f = f64::from(sc.fps.0) / f64::from(sc.fps.1);
    let codec = match sc.codec.as_str() {
        "hevc" => VideoCodec::Hevc,
        _ => VideoCodec::H264,
    };
    let src = make_source(out, codec, sc.fps, sc.secs)?;
    let es = std::fs::read(&src)?;
    let (_, frames) = insert_cc_sei(&es, codec, |_| Vec::new());

    // Expand events to frames.
    let mut evs: Vec<Ev> = Vec::new();
    for e in &sc.events {
        let track = sc.tracks.iter().position(|t| t.name == e.track).with_context(|| format!("no track {}", e.track))?;
        let n = e.count.unwrap_or(1);
        for k in 0..n {
            let t = e.t + e.every.unwrap_or(0.0) * f64::from(k);
            evs.push(Ev { frame: (t * fps_f).round() as u64, track, e });
        }
    }
    evs.sort_by_key(|e| e.frame);

    let mut log = String::new();

    // --- GStreamer pass.
    let gst_tracks: Vec<(usize, TrackCfg)> =
        sc.tracks.iter().enumerate().filter_map(|(i, t)| t.gst.clone().map(|g| (i, g))).collect();
    if !gst_tracks.is_empty() {
        let cfgs: Vec<TrackCfg> = gst_tracks.iter().map(|(_, g)| g.clone()).collect();
        let mut g = GstCc::new(&cfgs, sc.fps)?;
        g.gap_when_behind = !sc.gap_always;
        let mut per_frame: Vec<Vec<u8>> = Vec::with_capacity(frames as usize);
        let mut dump = String::new();
        for i in 0..frames {
            for ev in evs.iter().filter(|e| e.frame == i) {
                let Some(gi) = gst_tracks.iter().position(|(ti, _)| *ti == ev.track) else { continue };
                if ev.e.cr {
                    g.carriage_return(gi);
                }
                let input = match (&ev.e.json, &ev.e.text) {
                    _ if ev.e.raw_hex.is_some() => Input::Raw(unhex(ev.e.raw_hex.as_deref().unwrap_or(""))),
                    (Some(j), _) => Input::Json(j.to_string()),
                    (None, Some(t)) => Input::Text(t.clone()),
                    (None, None) => continue,
                };
                let fr = ev.e.dur.map_or(1, |d| (d * fps_f).round().max(1.0) as u64);
                if let Err(e) = g.push(gi, i, fr, &input) {
                    let _ = writeln!(log, "gst push error at frame {i}: {e}");
                }
            }
            let (data, lag) = g.frame(i);
            for m in g.bus_messages() {
                let _ = writeln!(log, "gst bus at frame {i}: {m}");
            }
            let hex: Vec<String> = data.chunks(3).filter(|t| t.len() == 3 && !(t[0] & 3 >= 2 && t[1] == 0 && t[2] == 0) && !(t[0] & 3 < 2 && t[1] == 0x80 && t[2] == 0x80)).map(|t| format!("{:02x}{:02x}{:02x}", t[0], t[1], t[2])).collect();
            if !hex.is_empty() {
                let _ = writeln!(dump, "{i} lag={lag} n={} {}", data.len() / 3, hex.join(" "));
            }
            per_frame.push(data);
        }
        for m in g.bus_messages() {
            let _ = writeln!(log, "gst bus: {m}");
        }
        let _ = writeln!(log, "gst stats: {:?} backlog_at_end={}", g.stats, g.backlog());
        std::fs::write(out.join("gst-cc.txt"), dump)?;
        let (es_out, _) = insert_cc_sei(&es, codec, |f| triples(per_frame.get(f as usize).map_or(&[][..], |v| v)));
        write_ts(out, "gst", &es_out, codec, sc.fps)?;
    }

    // --- Our encoder pass.
    let ours: Vec<(usize, &Ours)> = sc.tracks.iter().enumerate().filter_map(|(i, t)| t.ours.as_ref().map(|o| (i, o))).collect();
    if !ours.is_empty() {
        let mut mux = CcMux::new(rate);
        for (_, o) in &ours {
            if let Some(ch) = o.cc.as_deref().and_then(channel) {
                let mode = match o.mode.as_deref() {
                    Some("ru2") => Mode608::RollUp(2),
                    Some("ru4") => Mode608::RollUp(4),
                    Some("popon") => Mode608::PopOn,
                    Some("painton") => Mode608::PaintOn,
                    _ => Mode608::RollUp(3),
                };
                mux.add_608(Cc608Encoder::with_mode(ch, mode));
            }
            if let Some(e) = o.svc.and_then(Cc708Encoder::new) {
                mux.add_708(e);
            }
        }
        let mut per_frame: Vec<Vec<CcTriple>> = Vec::with_capacity(frames as usize);
        for i in 0..frames {
            for ev in evs.iter().filter(|e| e.frame == i) {
                let Some((_, o)) = ours.iter().find(|(ti, _)| *ti == ev.track) else { continue };
                let text = ev.e.ours_text.clone().or_else(|| ev.e.json.as_ref().map(json_text)).or_else(|| ev.e.text.clone());
                let Some(text) = text else { continue };
                if let Some(ch) = o.cc.as_deref().and_then(channel) {
                    mux.push_text_608(ch, &text);
                }
                if let Some(s) = o.svc {
                    mux.push_text_708(s, &text);
                }
            }
            per_frame.push(mux.next_frame());
        }
        let _ = writeln!(log, "ours: idle_at_end={}", mux.is_idle());
        let (es_out, _) = insert_cc_sei(&es, codec, |f| per_frame.get(f as usize).cloned().unwrap_or_default());
        write_ts(out, "ours", &es_out, codec, sc.fps)?;
    }
    let _ = writeln!(log, "frames={frames}");
    std::fs::write(out.join("render.log"), &log)?;
    print!("{log}");
    Ok(())
}

fn unhex(s: &str) -> Vec<u8> {
    (0..s.len() / 2).filter_map(|i| s.get(2 * i..2 * i + 2).and_then(|h| u8::from_str_radix(h, 16).ok())).collect()
}

fn channel(s: &str) -> Option<Channel> {
    match s {
        "cc1" => Some(Channel::Cc1),
        "cc2" => Some(Channel::Cc2),
        "cc3" => Some(Channel::Cc3),
        "cc4" => Some(Channel::Cc4),
        _ => None,
    }
}

fn make_source(dir: &Path, codec: VideoCodec, fps: (i32, i32), secs: u32) -> Result<PathBuf> {
    let (enc, ext) = match codec {
        VideoCodec::H264 => ("libx264", "h264"),
        VideoCodec::Hevc => ("libx265", "hevc"),
    };
    let src = dir.join(format!("src.{ext}"));
    let mut c = Command::new("ffmpeg");
    c.args(["-hide_banner", "-loglevel", "error", "-y", "-f", "lavfi", "-i"])
        .arg(format!("testsrc2=size=320x180:rate={}", fps_arg(fps)))
        .args(["-t", &secs.to_string(), "-c:v", enc, "-bf", "0", "-pix_fmt", "yuv420p"]);
    if codec == VideoCodec::Hevc {
        c.args(["-x265-params", "log-level=error"]);
    }
    c.args(["-f", ext]).arg(&src);
    run(&mut c)?;
    Ok(src)
}

fn write_ts(dir: &Path, name: &str, es: &[u8], codec: VideoCodec, fps: (i32, i32)) -> Result<()> {
    let ext = if codec == VideoCodec::Hevc { "hevc" } else { "h264" };
    let es_path = dir.join(format!("{name}.{ext}"));
    std::fs::write(&es_path, es)?;
    run(Command::new("ffmpeg")
        .args(["-hide_banner", "-loglevel", "error", "-y", "-framerate", &fps_arg(fps), "-i"])
        .arg(&es_path)
        .args(["-c", "copy", "-f", "mpegts"])
        .arg(dir.join(format!("{name}.ts"))))?;
    let _ = std::fs::remove_file(&es_path);
    Ok(())
}
