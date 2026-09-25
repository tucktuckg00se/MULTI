//! S6 spike: end to end. Live MPEG-TS in (SRT/UDP) -> S2 pass-through bridge
//! (video never decoded) -> captions in up to four languages as CEA-608/708
//! SEI -> MPEG-TS out (SRT/UDP, several outputs).
//!
//! ```text
//! input -> tsdemux -> bridge -> h26xparse -> [Captioner probe] -> h26xccinserter -> mpegtsmux -> outputs
//!             `-> audio tap (AAC -> 16 kHz mono) -> ASR thread -> segmenter -> EN lane
//!                                                                     `-> MT threads (es, fr, de) -> lanes
//! ```
//!
//! One process, threads only (process isolation is out of scope for S6).

mod asr;
mod captioner;
mod input;
mod output;
mod segment;
mod translate;

use std::io::Write;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Result, bail};
use clap::Parser;
use gst::prelude::*;
use s2_gst_pipe::Codec;
use s2_gst_pipe::bridge::Bridge;
use s2_gst_pipe::stamps::{Stamps, wall_ns};
use tracing::{error, info, warn};

fn models() -> PathBuf {
    std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default().join(".cache/multi-models")
}

#[derive(Parser)]
#[command(about = "S6: live captions in four languages, end to end")]
struct Cli {
    /// srt://host:port?mode=caller|listener&latency=MS, or udp://host:port
    #[arg(long)]
    input: String,
    /// Repeatable. srt://... or udp://host:port
    #[arg(long, required = true)]
    output: Vec<String>,
    /// Caption languages, in lane order (lane 1 = CC1+svc1, 2 = CC3+svc2, 3.. = svc only).
    /// The first must be the spoken language (en).
    #[arg(long, default_value = "en,es,fr,de", value_delimiter = ',')]
    langs: Vec<String>,
    #[arg(long, value_enum, default_value = "h264")]
    codec: Codec,
    /// Frame rate for the caption stage, as N or N/D.
    #[arg(long, default_value = "30")]
    fps: String,
    /// Committed words: wall_ns, audio PTS (s), text.
    #[arg(long)]
    words_log: Option<PathBuf>,
    /// Caption pushes per lane: wall_ns, lane, clause, frame PTS, wait, text.
    #[arg(long)]
    emit_log: Option<PathBuf>,
    /// Nemotron 3.5 streaming (560 ms) sherpa-onnx model directory.
    #[arg(long)]
    asr_model: Option<PathBuf>,
    /// Silero VAD model.
    #[arg(long)]
    vad: Option<PathBuf>,
    /// Directory with opus-mt-en-<lang> CTranslate2 models.
    #[arg(long)]
    mt_dir: Option<PathBuf>,
    /// Run ASR and MT on CPU.
    #[arg(long)]
    cpu: bool,
    #[arg(long, default_value_t = 2000)]
    watchdog_ms: u64,
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
        .with_ansi(false)
        .init();
    gst::init()?;
    run(Cli::parse())
}

fn line_logger(path: &PathBuf, header: &str) -> Result<std::sync::mpsc::Sender<String>> {
    let mut w = std::io::BufWriter::new(std::fs::File::create(path)?);
    writeln!(w, "{header}")?;
    let (tx, rx) = std::sync::mpsc::channel::<String>();
    std::thread::Builder::new().name("emitlog".into()).spawn(move || {
        while let Ok(l) = rx.recv() {
            let _ = writeln!(w, "{l}");
            let _ = w.flush();
        }
    })?;
    Ok(tx)
}

fn run(a: Cli) -> Result<()> {
    let fps = parse_fps(&a.fps)?;
    if a.langs.is_empty() || a.langs.len() > 4 {
        bail!("--langs takes 1 to 4 languages");
    }
    if a.langs[0] != "en" {
        bail!("the first language is the spoken one and must be en (S6 translates from English)");
    }
    let m = models();
    let asr_model = a
        .asr_model
        .clone()
        .unwrap_or_else(|| m.join("sherpa/sherpa-onnx-nemotron-3.5-asr-streaming-0.6b-560ms-2026-06-11-fp32"));
    let vad = a.vad.clone().unwrap_or_else(|| m.join("sherpa/silero_vad.onnx"));
    let mt_dir = a.mt_dir.clone().unwrap_or_else(|| m.join("ct2"));
    let stamps = Stamps::new(None)?;

    // Text side (threads; none of them can block the video path).
    let emit = a.emit_log.as_ref().map(|p| line_logger(p, "wall_ns\tlane\tclause\tpts_ns\twait_ms\ttext")).transpose()?;
    let (line_tx, line_rx) = std::sync::mpsc::channel();
    let cap = captioner::Captioner::new(line_rx, a.langs.len(), fps, emit)?;
    let (audio_tx, audio_rx) = std::sync::mpsc::sync_channel(500);
    let tap = input::Tap { tx: audio_tx, drops: Arc::new(AtomicU64::new(0)) };
    let (words_tx, words_rx) = std::sync::mpsc::channel();
    let asr_stats = Arc::new(asr::AsrStats::default());
    let mut threads = vec![asr::spawn(
        asr::AsrCfg {
            model: asr_model,
            vad: vad.to_string_lossy().into_owned(),
            lang: "en".into(),
            cpu: a.cpu,
            words_log: a.words_log.clone(),
        },
        audio_rx,
        words_tx,
        asr_stats.clone(),
    )?];
    let mut mt_txs = Vec::new();
    let mut mt_stats = Vec::new();
    for (lane, lang) in a.langs.iter().enumerate().skip(1) {
        let (tx, rx) = std::sync::mpsc::channel();
        let st = Arc::new(translate::MtStats::default());
        threads.push(translate::spawn(mt_dir.clone(), lang.clone(), lane, a.cpu, rx, line_tx.clone(), st.clone())?);
        mt_txs.push(tx);
        mt_stats.push((lang.clone(), st));
    }
    threads.push(segment::spawn(words_rx, Some(line_tx), mt_txs)?);

    // Media side (S2).
    let out = output::build(a.codec, &a.output, cap, stamps.clone())?;
    let bridge = Bridge::new(out.video.clone(), out.audio.clone(), out.pipeline.clone(), stamps.clone());
    out.pipeline.set_state(gst::State::Playing)?;
    for s in &out.sinks {
        s.poll(&stamps);
    }

    let start = Instant::now();
    let mut next_stats = start;
    let out_bus = out.pipeline.bus();
    let mut backoff = Duration::from_millis(250);
    let c = &stamps.counters;
    'outer: loop {
        bridge.reset();
        let inp = match input::build(&a.input, a.codec, bridge.clone(), stamps.clone(), &tap) {
            Ok(i) => i,
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
                let (pushed, dropped) =
                    out.captioner.lock().map(|c| (c.pushed.clone(), c.dropped.clone())).unwrap_or_default();
                let mt: Vec<String> = mt_stats
                    .iter()
                    .map(|(l, s)| {
                        let ok = s.ok.load(Ordering::Relaxed);
                        format!(
                            "{l}:ok={ok},late={},err={},avg_ms={},max_ms={}",
                            s.late.load(Ordering::Relaxed),
                            s.errors.load(Ordering::Relaxed),
                            s.sum_ms.load(Ordering::Relaxed) / ok.max(1),
                            s.max_ms.load(Ordering::Relaxed)
                        )
                    })
                    .collect();
                info!(
                    uptime_s = start.elapsed().as_secs(),
                    rss_mb = format!("{:.0}", s4_asr::res::rss_mb()),
                    vram_mb = s4_asr::res::vram_mb(),
                    video_in = c.video_in.load(Ordering::Relaxed),
                    video_out = c.video_out.load(Ordering::Relaxed),
                    cc_frames = c.cc_frames.load(Ordering::Relaxed),
                    input_restarts = c.input_restarts.load(Ordering::Relaxed),
                    input_errors = c.input_errors.load(Ordering::Relaxed),
                    output_errors = c.output_errors.load(Ordering::Relaxed),
                    bridge_drops = c.bridge_drops.load(Ordering::Relaxed),
                    asr_ready = asr_stats.ready.load(Ordering::Relaxed),
                    asr_words = asr_stats.words.load(Ordering::Relaxed),
                    asr_errors = asr_stats.errors.load(Ordering::Relaxed),
                    audio_drops = tap.drops.load(Ordering::Relaxed),
                    ?pushed,
                    ?dropped,
                    mt = %mt.join(" "),
                    "stats"
                );
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
    // Ordered shutdown: closing the audio tap ends the ASR thread, which ends
    // the segmenter, which ends the translators. Their CUDA objects must be
    // freed before main returns, or CUDA's teardown aborts the process
    // ("driver shutting down" thrown from a destructor).
    drop(tap);
    let deadline = Instant::now() + Duration::from_secs(5);
    while threads.iter().any(|t| !t.is_finished()) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(50));
    }
    let stuck = threads.iter().filter(|t| !t.is_finished()).count();
    if stuck > 0 {
        warn!(stuck, "text threads did not stop in time");
    }
    for t in threads.into_iter().filter(|t| t.is_finished()) {
        let _ = t.join();
    }
    Ok(())
}
