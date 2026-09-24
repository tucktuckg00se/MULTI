//! S2 spike: live MPEG-TS in (SRT/UDP) -> captions inserted as SEI without
//! decoding video -> MPEG-TS out (SRT/UDP, several outputs), with GStreamer.
//!
//! ```text
//! s2-gst-pipe run --input 'srt://127.0.0.1:9200?mode=caller' \
//!     --output udp://127.0.0.1:9210 --output 'srt://:9211?mode=listener' \
//!     --codec h264 --captions ours --csv delay.csv
//! ```

mod bridge;
mod captions;
mod gstcc;
mod input;
mod output;
mod stamps;

use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use anyhow::{Result, bail};
use clap::{Parser, Subcommand, ValueEnum};
use gst::prelude::*;
use tracing::{error, info, warn};

use crate::stamps::{Stamps, rss_kb, wall_ns};

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum Codec {
    H264,
    Hevc,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum CaptionMode {
    /// Pass-through only (baseline).
    None,
    /// (a) appsrc text -> tttocea708/tttocea608 -> cccombiner.
    Gst,
    /// (b) spikes/cc CcMux -> GstVideoCaptionMeta per frame, keyed by PTS.
    Ours,
    /// (c, S2b) tttocea708 driven per frame from the video probe, no cccombiner.
    GstDirect,
}

#[derive(Parser)]
#[command(about = "S2: GStreamer caption pass-through spike")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    Run(RunArgs),
}

#[derive(Parser)]
struct RunArgs {
    /// srt://host:port?mode=caller|listener&latency=MS, or udp://host:port
    #[arg(long)]
    input: String,
    /// Repeatable. srt://... or udp://host:port
    #[arg(long, required = true)]
    output: Vec<String>,
    #[arg(long, value_enum, default_value = "h264")]
    codec: Codec,
    #[arg(long, value_enum, default_value = "ours")]
    captions: CaptionMode,
    /// (a) only: use tttocea608 + ccconverter instead of tttocea708.
    #[arg(long)]
    gst608: bool,
    /// 1 = CC1 + 708 service 1; 2 = also CC3 + service 2.
    #[arg(long, default_value_t = 1)]
    lanes: u8,
    /// Frame rate for the caption stage, as N or N/D.
    #[arg(long, default_value = "30")]
    fps: String,
    /// One fixture line every this many ms.
    #[arg(long, default_value_t = 2000)]
    line_ms: u64,
    /// cccombiner `latency` property (approach a).
    #[arg(long, default_value_t = 0)]
    cc_latency_ms: u64,
    /// Parse video in the input pipeline too (safer for PES that are not one
    /// access unit each; costs one frame of delay).
    #[arg(long)]
    in_parse: bool,
    /// Let tsdemux do PCR clock recovery instead of using PES PTS as is.
    #[arg(long)]
    pcr: bool,
    /// Restart the input after this long without data (once data has flowed).
    #[arg(long, default_value_t = 2000)]
    watchdog_ms: u64,
    /// Per-frame delay CSV.
    #[arg(long)]
    csv: Option<PathBuf>,
    /// Periodic stats CSV (RSS, counters, skew).
    #[arg(long)]
    stats: Option<PathBuf>,
    #[arg(long, default_value_t = 10)]
    stats_every_s: u64,
    /// Exit after this many seconds (0 = run forever).
    #[arg(long, default_value_t = 0)]
    duration_s: u64,
}

fn parse_fps(s: &str) -> Result<(i32, i32)> {
    let (n, d) = match s.split_once('/') {
        Some((n, d)) => (n.trim().parse()?, d.trim().parse()?),
        None => (s.trim().parse()?, 1),
    };
    if n <= 0 || d <= 0 {
        bail!("bad fps {s}");
    }
    Ok((n, d))
}

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .with_writer(std::io::stderr)
        .init();
    gst::init()?;
    match Cli::parse().cmd {
        Cmd::Run(a) => run(a),
    }
}

fn text_pump(out: &output::Output, line_ms: u64, lanes: u8) {
    let text = out.text.clone();
    let ours = out.ours_tx.clone();
    if text.is_none() && ours.is_none() {
        return;
    }
    let spawned = std::thread::Builder::new().name("text".into()).spawn(move || {
        let mut i = 0usize;
        loop {
            let line = cc::FIXTURE_LINES[i % cc::FIXTURE_LINES.len()];
            if let Some(t) = &text {
                let mut b = gst::Buffer::from_slice(line.as_bytes().to_vec());
                if let Some(bm) = b.get_mut() {
                    bm.set_duration(gst::ClockTime::from_mseconds(line_ms));
                }
                let _ = t.push_buffer(b);
            }
            if let Some(tx) = &ours {
                for lane in 0..lanes {
                    if tx.send(captions::Line { lane, text: line.to_string() }).is_err() {
                        return;
                    }
                }
            }
            info!(line, "caption line");
            i += 1;
            std::thread::sleep(Duration::from_millis(line_ms));
        }
    });
    if let Err(e) = spawned {
        warn!(err = %e, "text thread failed; running without captions");
    }
}

fn run(a: RunArgs) -> Result<()> {
    let fps = parse_fps(&a.fps)?;
    let stamps = Stamps::new(a.csv.as_deref())?;
    let cfg = output::OutputCfg {
        codec: a.codec,
        captions: a.captions,
        gst608: a.gst608,
        lanes: a.lanes.clamp(1, 2),
        fps,
        cc_latency_ms: a.cc_latency_ms,
        outputs: a.output.clone(),
    };
    let out = output::build(&cfg, stamps.clone())?;
    let bridge = bridge::Bridge::new(out.video.clone(), out.audio.clone(), out.pipeline.clone(), stamps.clone());
    out.pipeline.set_state(gst::State::Playing)?;
    for s in &out.sinks {
        s.poll(&stamps);
    }
    text_pump(&out, a.line_ms, cfg.lanes);

    let mut stats_w = match &a.stats {
        Some(p) => {
            let mut f = std::io::BufWriter::new(std::fs::File::create(p)?);
            use std::io::Write;
            writeln!(f, "wall_ns,uptime_s,rss_kb,video_in,video_out,audio_in,sessions,input_restarts,input_errors,output_errors,bridge_drops,rows_dropped,cc_frames,skew_us,slewed_ms")?;
            Some(f)
        }
        None => None,
    };
    let start = Instant::now();
    let mut next_stats = start;
    let out_bus = out.pipeline.bus();
    let mut backoff = Duration::from_millis(250);
    let c = &stamps.counters;

    'outer: loop {
        // (Re)build the input.
        bridge.reset();
        let inp = match input::build(&a.input, a.codec, a.in_parse, a.pcr, bridge.clone(), stamps.clone()) {
            Ok(i) => i,
            // First build failing is a config error (bad URI, missing plugin);
            // later failures are retried so the output keeps running.
            Err(e) if c.input_restarts.load(Ordering::Relaxed) == 0 => bail!(e),
            Err(e) => {
                error!(err = %e, "input rebuild failed; retrying");
                c.input_errors.fetch_add(1, Ordering::Relaxed);
                std::thread::sleep(backoff);
                backoff = (backoff * 2).min(Duration::from_secs(2));
                continue;
            }
        };
        let started = inp.pipeline.set_state(gst::State::Playing);
        if let Err(e) = started {
            warn!(err = %e, "input failed to start");
            c.input_errors.fetch_add(1, Ordering::Relaxed);
        }
        let in_bus = inp.pipeline.bus();
        let mut restart = started.is_err();
        while !restart {
            if let Some(bus) = &in_bus {
                while let Some(msg) = bus.timed_pop(gst::ClockTime::from_mseconds(100)) {
                    match msg.view() {
                        gst::MessageView::Error(e) => {
                            warn!(src = ?msg.src().map(|s| s.path_string()), err = %e.error(), dbg = ?e.debug(), "input error");
                            c.input_errors.fetch_add(1, Ordering::Relaxed);
                            restart = true;
                        }
                        gst::MessageView::Eos(_) => {
                            warn!("input EOS");
                            restart = true;
                        }
                        gst::MessageView::Warning(w) => warn!(err = %w.error(), "input warning"),
                        _ => {}
                    }
                    if restart {
                        break;
                    }
                }
            }
            // Watchdog: data flowed before but has stopped.
            let last = inp.last_data.load(Ordering::Relaxed);
            if !restart && last > 0 && wall_ns().saturating_sub(last) > a.watchdog_ms * 1_000_000 {
                warn!(silent_ms = (wall_ns() - last) / 1_000_000, "input silent; restarting input");
                restart = true;
            }
            if last > 0 {
                backoff = Duration::from_millis(250);
            }
            if let Some(bus) = &out_bus {
                while let Some(msg) = bus.pop() {
                    match msg.view() {
                        gst::MessageView::Error(e) => {
                            // Should not happen (appsrc-fed, no network I/O); log and keep going.
                            error!(src = ?msg.src().map(|s| s.path_string()), err = %e.error(), dbg = ?e.debug(), "output pipeline error");
                            c.output_errors.fetch_add(1, Ordering::Relaxed);
                        }
                        gst::MessageView::Warning(w) => warn!(err = %w.error(), dbg = ?w.debug(), "output warning"),
                        _ => {}
                    }
                }
            }
            for s in &out.sinks {
                s.poll(&stamps);
            }
            if Instant::now() >= next_stats {
                next_stats += Duration::from_secs(a.stats_every_s.max(1));
                let row = format!(
                    "{},{},{},{},{},{},{},{},{},{},{},{},{},{},{:.1}",
                    wall_ns(),
                    start.elapsed().as_secs(),
                    rss_kb(),
                    c.video_in.load(Ordering::Relaxed),
                    c.video_out.load(Ordering::Relaxed),
                    c.audio_in.load(Ordering::Relaxed),
                    c.sessions.load(Ordering::Relaxed),
                    c.input_restarts.load(Ordering::Relaxed),
                    c.input_errors.load(Ordering::Relaxed),
                    c.output_errors.load(Ordering::Relaxed),
                    c.bridge_drops.load(Ordering::Relaxed),
                    c.rows_dropped.load(Ordering::Relaxed),
                    c.cc_frames.load(Ordering::Relaxed),
                    c.last_skew_us.load(Ordering::Relaxed) as i64,
                    bridge.slewed_ms(),
                );
                info!(stats = %row, "stats");
                if let Some(w) = stats_w.as_mut() {
                    use std::io::Write;
                    let _ = writeln!(w, "{row}");
                    let _ = w.flush();
                }
            }
            if a.duration_s > 0 && start.elapsed() >= Duration::from_secs(a.duration_s) {
                let _ = inp.pipeline.set_state(gst::State::Null);
                break 'outer;
            }
        }
        let _ = inp.pipeline.set_state(gst::State::Null);
        c.input_restarts.fetch_add(1, Ordering::Relaxed);
        std::thread::sleep(backoff);
        backoff = (backoff * 2).min(Duration::from_secs(2));
    }
    let _ = out.pipeline.set_state(gst::State::Null);
    for s in &out.sinks {
        s.stop();
    }
    // Let the CSV thread flush.
    drop(stamps);
    std::thread::sleep(Duration::from_millis(200));
    Ok(())
}
