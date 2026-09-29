//! `multi-asr`: the speech-recognition worker.
//!
//! Reads PCM frames (16 kHz mono i16) and `Shutdown` from stdin, writes
//! `Ready`, `Words` (times on the incoming PCM timeline) and `Heartbeat` to
//! stdout, logs to stderr. Protocol: `multi_core::ipc`.

mod agreement;
mod clock;
mod engine;
mod guard;
mod sherpa;
mod vad;
#[cfg(feature = "whisper")]
mod whisper;

use anyhow::{Context, Result, anyhow, bail};
use clap::{Parser, ValueEnum};
use clock::Clock;
use engine::{Engine, EngineKind, RawWord};
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
    /// Recogniser: the catalogue entry's `engine`.
    #[arg(long, value_enum, default_value_t = EngineKind::SherpaStreaming)]
    engine: EngineKind,
    /// sherpa-streaming: the export's folder (encoder/decoder/joiner +
    /// tokens.txt). whisper: the ggml `.bin` file.
    #[arg(long, alias = "model-dir")]
    model: PathBuf,
    /// Silero VAD model; defaults to `silero_vad.onnx` next to the model.
    #[arg(long)]
    vad_model: Option<PathBuf>,
    #[arg(long, value_enum, default_value_t = Device::Auto)]
    device: Device,
    /// sherpa-streaming: the chunk the model was exported with (80, 160,
    /// 320, 560, 1120). whisper: the interval between passes (200-3000).
    #[arg(long, default_value_t = 560)]
    chunk_ms: u32,
    /// whisper: LocalAgreement-n, the passes that must agree before a word
    /// is committed (1-3).
    #[arg(long, default_value_t = 2, value_parser = clap::value_parser!(u8).range(1..=3))]
    passes: u8,
    #[arg(long, default_value_t = 0.5)]
    vad_threshold: f32,
    /// sherpa-streaming: keep the stream open this long after speech ends
    /// (bridges pauses; a new stream loses its first words).
    #[arg(long, default_value_t = 1500, value_parser = clap::value_parser!(u32).range(0..=10_000))]
    hangover_ms: u32,
    /// Spoken language, passed to the model.
    #[arg(long, default_value = "en")]
    lang: String,
    /// CPU threads (ONNX Runtime, whisper.cpp).
    #[arg(long, default_value_t = 4)]
    threads: i32,
}

/// What the arguments resolve to, checked before any model is loaded.
#[derive(Debug, PartialEq)]
struct Plan {
    engine: EngineKind,
    model: PathBuf,
    vad: PathBuf,
    chunk_ms: u32,
}

fn plan(args: &Args) -> Result<Plan> {
    if !args.engine.available() {
        bail!(
            "this multi-asr was built without the {:?} engine (cargo feature `whisper`)",
            args.engine
        );
    }
    args.engine
        .check_chunk(args.chunk_ms)
        .map_err(|e| anyhow!(e))?;
    match args.engine {
        EngineKind::SherpaStreaming if args.model.is_file() => bail!(
            "--model {}: a sherpa-streaming model is a folder, not a file",
            args.model.display()
        ),
        EngineKind::Whisper if args.model.is_dir() => bail!(
            "--model {}: a Whisper model is a ggml .bin file, not a folder",
            args.model.display()
        ),
        _ => {}
    }
    let vad = match &args.vad_model {
        Some(p) => p.clone(),
        None => args
            .model
            .parent()
            .map(|p| p.join("silero_vad.onnx"))
            .context("--model has no parent folder; pass --vad-model")?,
    };
    Ok(Plan {
        engine: args.engine,
        model: args.model.clone(),
        vad,
        chunk_ms: args.chunk_ms,
    })
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

fn build(args: &Args, plan: &Plan, cuda: bool) -> Result<Box<dyn Engine>> {
    let vad = vad::VadGate::new(&plan.vad, args.vad_threshold)?;
    match plan.engine {
        EngineKind::SherpaStreaming => Ok(Box::new(sherpa::SherpaStreaming::new(
            sherpa::SherpaCfg {
                model_dir: plan.model.clone(),
                provider: if cuda { "cuda" } else { "cpu" }.into(),
                threads: args.threads,
                lang: args.lang.clone(),
                chunk_ms: plan.chunk_ms,
                final_word_ms: 400,
                hangover_ms: args.hangover_ms,
            },
            vad,
        )?)),
        #[cfg(feature = "whisper")]
        EngineKind::Whisper => {
            let asr = whisper::WhisperAsr::new(&plan.model, &args.lang, cuda, args.threads)?;
            let cfg = agreement::LaCfg {
                chunk_ms: plan.chunk_ms,
                passes: usize::from(args.passes),
                ..agreement::LaCfg::default()
            };
            Ok(Box::new(agreement::LaEngine::new(asr, cfg, vad)))
        }
        #[cfg(not(feature = "whisper"))]
        EngineKind::Whisper => bail!("built without the whisper engine"),
    }
}

fn load(args: &Args) -> Result<Box<dyn Engine>> {
    let plan = plan(args)?;
    let t = Instant::now();
    let mut engine = match args.device {
        Device::Cuda => build(args, &plan, true)?,
        Device::Cpu => build(args, &plan, false)?,
        Device::Auto => match build(args, &plan, true) {
            Ok(e) => e,
            Err(e) => {
                tracing::warn!("CUDA init failed ({e:#}); falling back to CPU");
                build(args, &plan, false)?
            }
        },
    };
    engine.warm_up()?;
    tracing::info!(
        engine = ?plan.engine,
        model = %plan.model.display(),
        device = ?args.device,
        load_ms = t.elapsed().as_millis() as u64,
        "ASR ready"
    );
    Ok(engine)
}

/// Consecutive failed decodes after which the worker gives up (and the
/// supervisor restarts it).
const MAX_DECODE_ERRORS: u32 = 5;

fn run(args: &Args, hb: &Heartbeat) -> Result<()> {
    let mut engine = load(args)?;
    worker::send(&Message::Ready {
        worker: "multi-asr".into(),
        version: env!("CARGO_PKG_VERSION").into(),
    })?;
    let mut clock = Clock::default();
    let mut errors = 0u32;
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
                match words {
                    Ok(w) => {
                        errors = 0;
                        emit(&clock, w)?;
                    }
                    Err(e) => {
                        errors += 1;
                        if errors >= MAX_DECODE_ERRORS {
                            return Err(e.context("decoding keeps failing"));
                        }
                        tracing::warn!("decode failed: {e:#}");
                        worker::send(&Message::Error {
                            message: format!("asr: decode failed: {e:#}"),
                        })?;
                    }
                }
            }
            Frame::Message(Message::Shutdown) => break,
            Frame::Message(other) => tracing::warn!("ignoring unexpected message: {other:?}"),
        }
    }
    let words = {
        let _busy = hb.busy();
        engine.finish()
    };
    emit(&clock, words?)?;
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

#[cfg(test)]
mod tests {
    use super::*;

    fn args(v: &[&str]) -> Result<Args> {
        Ok(Args::try_parse_from(
            std::iter::once("multi-asr").chain(v.iter().copied()),
        )?)
    }

    #[test]
    fn defaults_to_sherpa_streaming() -> Result<()> {
        let a = args(&["--model-dir", "/m/sherpa/nemotron-560"])?;
        let p = plan(&a)?;
        assert_eq!(p.engine, EngineKind::SherpaStreaming);
        assert_eq!(p.model, PathBuf::from("/m/sherpa/nemotron-560"));
        assert_eq!(p.vad, PathBuf::from("/m/sherpa/silero_vad.onnx"));
        assert_eq!(p.chunk_ms, 560);
        assert_eq!((a.passes, a.hangover_ms, a.lang.as_str()), (2, 1500, "en"));
        Ok(())
    }

    #[test]
    fn engine_and_chunk_are_checked_together() -> Result<()> {
        let sherpa = |c: &str| args(&["--model", "/m/x", "--chunk-ms", c]);
        assert_eq!(plan(&sherpa("160")?)?.chunk_ms, 160);
        assert!(plan(&sherpa("1000")?).is_err(), "not an export chunk");
        let whisper = |c: &str| {
            args(&[
                "--engine",
                "whisper",
                "--model",
                "/m/whisper/ggml-small.bin",
                "--chunk-ms",
                c,
                "--vad-model",
                "/m/vad.onnx",
                "--lang",
                "de",
                "--passes",
                "3",
            ])
        };
        let a = whisper("1000")?;
        assert_eq!(
            (a.engine, a.passes, a.lang.as_str()),
            (EngineKind::Whisper, 3, "de")
        );
        if cfg!(feature = "whisper") {
            let p = plan(&a)?;
            assert_eq!(p.vad, PathBuf::from("/m/vad.onnx"));
            assert_eq!(p.chunk_ms, 1000);
            assert!(plan(&whisper("100")?).is_err(), "pass interval too short");
        } else {
            assert!(plan(&a).is_err(), "engine not built");
        }
        assert!(args(&["--engine", "vosk", "--model", "/m/x"]).is_err());
        assert!(args(&["--model", "/m/x", "--passes", "4"]).is_err());
        Ok(())
    }

    #[test]
    fn model_kind_must_match_the_engine() -> Result<()> {
        let dir = std::env::temp_dir();
        let a = args(&["--engine", "whisper", "--model", &dir.to_string_lossy()])?;
        assert!(plan(&a).is_err(), "a folder is not a ggml file");
        let exe = std::env::current_exe()?;
        let a = args(&["--model", &exe.to_string_lossy()])?;
        assert!(plan(&a).is_err(), "a file is not a sherpa export folder");
        Ok(())
    }
}
