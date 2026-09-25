//! `multi-asr`: the speech-recognition worker.
//!
//! Reads PCM frames (16 kHz mono i16) and `Shutdown` from stdin, writes
//! `Ready`, `Words` (times on the incoming PCM timeline) and `Heartbeat` to
//! stdout, logs to stderr. Protocol: `multi_core::ipc`.

mod clock;
mod engine;

use anyhow::{Context, Result};
use clap::{Parser, ValueEnum};
use clock::Clock;
use engine::{Engine, EngineCfg, RawWord};
use multi_core::Word;
use multi_core::ipc::{self, Frame, FrameError, Message};
use multi_core::worker::{self, Heartbeat};
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::{Duration, Instant};

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
enum Device {
    Cuda,
    Cpu,
    /// CUDA if it initialises, otherwise CPU.
    Auto,
}

#[derive(Debug, Parser)]
#[command(
    version,
    about = "MULTI speech-recognition worker (speaks the worker protocol on stdio)"
)]
struct Args {
    /// Nemotron 3.5 Streaming sherpa-onnx export (encoder/decoder/joiner + tokens.txt).
    #[arg(long)]
    model_dir: PathBuf,
    /// Silero VAD model; defaults to `silero_vad.onnx` next to the model dir.
    #[arg(long)]
    vad_model: Option<PathBuf>,
    #[arg(long, value_enum, default_value_t = Device::Auto)]
    device: Device,
    /// Chunk size the model was exported with.
    #[arg(long, default_value_t = 560, value_parser = parse_chunk)]
    chunk_ms: u32,
    #[arg(long, default_value_t = 0.5)]
    vad_threshold: f32,
    /// Language prompt for the model.
    #[arg(long, default_value = "en")]
    lang: String,
    /// CPU threads for ONNX Runtime.
    #[arg(long, default_value_t = 4)]
    threads: i32,
}

fn parse_chunk(s: &str) -> Result<u32, String> {
    match s.parse::<u32>() {
        Ok(v @ (80 | 160 | 320 | 560 | 1120)) => Ok(v),
        _ => Err("one of 80, 160, 320, 560, 1120 (must match the model export)".into()),
    }
}

/// A single decode longer than this means the worker is stuck.
const STALL_LIMIT: Duration = Duration::from_secs(10);

fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .init();
    let args = Args::parse();
    let (hb, hb_thread) = match Heartbeat::start(STALL_LIMIT) {
        Ok(x) => x,
        Err(e) => {
            tracing::error!("cannot start heartbeat thread: {e}");
            return ExitCode::FAILURE;
        }
    };
    let code = match run(&args, &hb) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            tracing::error!("{e:#}");
            let _ = worker::send(&Message::Error {
                message: format!("asr: {e:#}"),
            });
            ExitCode::FAILURE
        }
    };
    // The engine (and its CUDA state) is dropped inside run(), before the
    // heartbeat thread is stopped and the process exits.
    hb.stop();
    let _ = hb_thread.join();
    code
}

fn load(args: &Args) -> Result<Engine> {
    let vad_model = match &args.vad_model {
        Some(p) => p.clone(),
        None => args
            .model_dir
            .parent()
            .map(|p| p.join("silero_vad.onnx"))
            .context("--model-dir has no parent; pass --vad-model")?,
    };
    let cfg = |provider: &str| EngineCfg {
        model_dir: args.model_dir.clone(),
        vad_model: vad_model.clone(),
        provider: provider.into(),
        threads: args.threads,
        lang: args.lang.clone(),
        chunk_ms: args.chunk_ms,
        final_word_ms: 400,
        vad_threshold: args.vad_threshold,
    };
    let t = Instant::now();
    let mut engine = match args.device {
        Device::Cuda => Engine::new(cfg("cuda"))?,
        Device::Cpu => Engine::new(cfg("cpu"))?,
        Device::Auto => match Engine::new(cfg("cuda")) {
            Ok(e) => e,
            Err(e) => {
                tracing::warn!("CUDA init failed ({e:#}); falling back to CPU");
                Engine::new(cfg("cpu"))?
            }
        },
    };
    engine.warm_up();
    tracing::info!(
        model = %args.model_dir.display(),
        device = ?args.device,
        load_ms = t.elapsed().as_millis() as u64,
        "ASR ready"
    );
    Ok(engine)
}

fn run(args: &Args, hb: &Heartbeat) -> Result<()> {
    let mut engine = load(args)?;
    worker::send(&Message::Ready {
        worker: "multi-asr".into(),
        version: env!("CARGO_PKG_VERSION").into(),
    })?;
    let mut clock = Clock::default();
    let mut stdin = std::io::stdin().lock();
    loop {
        let frame = match ipc::read_frame(&mut stdin) {
            Ok(f) => f,
            Err(FrameError::Closed) => break,
            Err(e @ (FrameError::BadJson(_) | FrameError::BadKind(_) | FrameError::BadPcm)) => {
                tracing::warn!("ignoring bad input frame: {e}");
                worker::send(&Message::Error {
                    message: format!("asr: bad input frame: {e}"),
                })?;
                continue;
            }
            Err(e) => return Err(e).context("reading stdin"),
        };
        match frame {
            Frame::Pcm { start_ms, samples } => {
                clock.frame(start_ms, samples.len());
                let audio: Vec<f32> = samples.iter().map(|&s| f32::from(s) / 32768.0).collect();
                let words = {
                    let _busy = hb.busy();
                    engine.push(&audio)
                };
                emit(&clock, words)?;
            }
            Frame::Message(Message::Shutdown) => break,
            Frame::Message(other) => tracing::warn!("ignoring unexpected message: {other:?}"),
        }
    }
    let words = {
        let _busy = hb.busy();
        engine.finish()
    };
    emit(&clock, words)?;
    drop(engine);
    tracing::info!("ASR stopped");
    Ok(())
}

fn emit(clock: &Clock, words: Vec<RawWord>) -> Result<()> {
    if words.is_empty() {
        return Ok(());
    }
    let words = words
        .into_iter()
        .map(|w| Word {
            start_ms: clock.at(w.t0),
            end_ms: clock.at(w.t1),
            text: w.text,
        })
        .collect();
    worker::send(&Message::Words { words })?;
    Ok(())
}
