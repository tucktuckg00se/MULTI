//! `multi-mt`: the translation worker.
//!
//! Reads `Translate { clause, langs }` and `Shutdown` from stdin; writes one
//! `Translated` (or one `Error`) per requested language, plus `Ready` and
//! `Heartbeat`, to stdout; logs to stderr. Protocol: `multi_core::ipc`.
//!
//! Each language has its own thread, model and bounded queue, so a slow or
//! failing language never holds up the others. A request not finished within
//! the deadline (counted from arrival) is answered with an `Error`.

mod text;

use anyhow::{Context, Result, anyhow, bail};
use clap::{Parser, ValueEnum};
use ct2rs::tokenizers::sentencepiece::Tokenizer as SpTokenizer;
use ct2rs::{ComputeType, Config, GenerationStepResult, TranslationOptions, Translator};
use multi_core::ipc::{self, Frame, FrameError, Message};
use multi_core::worker::{self, Heartbeat};
use multi_core::{Clause, Translation};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::mpsc::{Receiver, SyncSender, TrySendError};
use std::thread::JoinHandle;
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
    about = "MULTI translation worker (speaks the worker protocol on stdio)"
)]
struct Args {
    /// Directory holding CTranslate2 opus-mt models (`opus-mt-en-<lang>`).
    #[arg(long)]
    models: PathBuf,
    #[arg(long, value_enum, default_value_t = Device::Auto)]
    device: Device,
    /// Target languages, comma-separated.
    #[arg(long, value_delimiter = ',', default_value = "es,fr,de")]
    langs: Vec<String>,
    /// Per-request deadline, counted from arrival.
    #[arg(long, default_value_t = 2000)]
    deadline_ms: u64,
    /// Requests waiting per language; more are refused with an `Error`.
    #[arg(long, default_value_t = 8)]
    queue: usize,
    /// CPU threads per language model.
    #[arg(long, default_value_t = 2)]
    threads: usize,
}

/// A translation running longer than this means the worker is stuck.
const STALL_LIMIT: Duration = Duration::from_secs(10);
/// Time allowed for all models to load.
const LOAD_TIMEOUT: Duration = Duration::from_secs(300);

struct Job {
    clause: Clause,
    arrived: Instant,
}

struct Lane {
    tx: SyncSender<Job>,
    thread: JoinHandle<()>,
}

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
                message: format!("mt: {e:#}"),
            });
            ExitCode::FAILURE
        }
    };
    hb.stop();
    let _ = hb_thread.join();
    code
}

fn error(clause_id: u64, lang: &str, why: impl std::fmt::Display) {
    tracing::warn!(clause = clause_id, %lang, "{why}");
    let _ = worker::send(&Message::Error {
        message: format!("mt: clause {clause_id} {lang}: {why}"),
    });
}

fn run(args: &Args, hb: &Arc<Heartbeat>) -> Result<()> {
    if args.langs.is_empty() {
        bail!("--langs is empty");
    }
    let deadline = Duration::from_millis(args.deadline_ms);
    let (ready_tx, ready_rx) = std::sync::mpsc::channel();
    let mut starting = BTreeMap::new();
    for lang in &args.langs {
        let (tx, rx) = std::sync::mpsc::sync_channel(args.queue.max(1));
        let dir = model_dir(&args.models, lang);
        let (device, threads) = (args.device, args.threads);
        let (lang2, ready, hb2) = (lang.clone(), ready_tx.clone(), Arc::clone(hb));
        let thread = std::thread::Builder::new()
            .name(format!("mt-{lang}"))
            .spawn(move || lane(&lang2, dir, device, threads, deadline, &ready, rx, &hb2))?;
        starting.insert(lang.clone(), Lane { tx, thread });
    }
    drop(ready_tx);

    // Wait for every lane to load (or fail); failed lanes answer with errors.
    let mut lanes = BTreeMap::new();
    let load_until = Instant::now() + LOAD_TIMEOUT;
    while !starting.is_empty() {
        let left = load_until.saturating_duration_since(Instant::now());
        let Ok((lang, res)) = ready_rx.recv_timeout(left) else {
            bail!("models did not load within {} s", LOAD_TIMEOUT.as_secs());
        };
        let Some(l) = starting.remove(&lang) else {
            continue;
        };
        match res {
            Ok(()) => {
                lanes.insert(lang, l);
            }
            Err(e) => {
                tracing::error!(%lang, "translator failed to load: {e:#}");
                worker::send(&Message::Error {
                    message: format!("mt: {lang} unavailable: {e:#}"),
                })?;
                let _ = l.thread.join();
            }
        }
    }
    if lanes.is_empty() {
        bail!("no translator loaded");
    }
    worker::send(&Message::Ready {
        worker: "multi-mt".into(),
        version: env!("CARGO_PKG_VERSION").into(),
    })?;

    let res = serve(&lanes);
    // Ordered shutdown: close every queue, then join, so each model is
    // dropped by its own thread before the process exits.
    let threads: Vec<_> = lanes.into_values().map(|l| l.thread).collect();
    for t in threads {
        let _ = t.join();
    }
    tracing::info!("MT stopped");
    res
}

fn serve(lanes: &BTreeMap<String, Lane>) -> Result<()> {
    let mut stdin = std::io::stdin().lock();
    loop {
        let frame = match ipc::read_frame(&mut stdin) {
            Ok(f) => f,
            Err(FrameError::Closed) => return Ok(()),
            Err(e @ (FrameError::BadJson(_) | FrameError::BadKind(_) | FrameError::BadPcm)) => {
                tracing::warn!("ignoring bad input frame: {e}");
                worker::send(&Message::Error {
                    message: format!("mt: bad input frame: {e}"),
                })?;
                continue;
            }
            Err(e) => return Err(e).context("reading stdin"),
        };
        match frame {
            Frame::Message(Message::Translate { clause, langs }) => {
                let arrived = Instant::now();
                for lang in langs {
                    if clause.text.len() > text::MAX_INPUT_BYTES {
                        error(clause.id, &lang, "clause too long");
                        continue;
                    }
                    let Some(l) = lanes.get(&lang) else {
                        error(clause.id, &lang, "no model for this language");
                        continue;
                    };
                    let job = Job {
                        clause: clause.clone(),
                        arrived,
                    };
                    match l.tx.try_send(job) {
                        Ok(()) => {}
                        Err(TrySendError::Full(_)) => error(clause.id, &lang, "queue full"),
                        Err(TrySendError::Disconnected(_)) => {
                            error(clause.id, &lang, "translator stopped")
                        }
                    }
                }
            }
            Frame::Message(Message::Shutdown) => return Ok(()),
            Frame::Message(other) => tracing::warn!("ignoring unexpected message: {other:?}"),
            Frame::Pcm { .. } => tracing::warn!("ignoring PCM frame"),
        }
    }
}

fn model_dir(root: &Path, lang: &str) -> PathBuf {
    for name in [
        format!("opus-mt-en-{lang}"),
        format!("opus-mt-tc-big-en-{lang}"),
    ] {
        let p = root.join(name);
        if p.is_dir() {
            return p;
        }
    }
    root.join(format!("opus-mt-en-{lang}"))
}

fn load_on(dir: &Path, device: ct2rs::Device, threads: usize) -> Result<Translator<SpTokenizer>> {
    let cfg = Config {
        device,
        compute_type: match device {
            ct2rs::Device::CUDA => ComputeType::FLOAT16,
            _ => ComputeType::INT8,
        },
        num_threads_per_replica: threads,
        ..Default::default()
    };
    let tok = SpTokenizer::new(dir).with_context(|| format!("tokenizer in {}", dir.display()))?;
    Translator::with_tokenizer(dir, tok, &cfg)
        .with_context(|| format!("model in {}", dir.display()))
}

fn load(lang: &str, dir: &Path, device: Device, threads: usize) -> Result<Translator<SpTokenizer>> {
    if !dir.is_dir() {
        bail!("no model directory {}", dir.display());
    }
    let tr = match device {
        Device::Cuda => load_on(dir, ct2rs::Device::CUDA, threads)?,
        Device::Cpu => load_on(dir, ct2rs::Device::CPU, threads)?,
        Device::Auto => match load_on(dir, ct2rs::Device::CUDA, threads) {
            Ok(t) => t,
            Err(e) => {
                tracing::warn!(%lang, "CUDA init failed ({e:#}); falling back to CPU");
                load_on(dir, ct2rs::Device::CPU, threads)?
            }
        },
    };
    Ok(tr)
}

fn translate(tr: &Translator<SpTokenizer>, text: &str, until: Instant) -> Result<String> {
    let parts = text::sentences(text);
    if parts.is_empty() {
        return Ok(String::new());
    }
    let opts = TranslationOptions {
        beam_size: 1,
        max_decoding_length: text::max_decoding_length(text.split_whitespace().count()),
        repetition_penalty: 1.0,
        ..Default::default()
    };
    let mut stop_at_deadline = |_: GenerationStepResult| -> Result<()> {
        if Instant::now() > until {
            Err(anyhow!("deadline passed"))
        } else {
            Ok(())
        }
    };
    let out = tr.translate_batch(&parts, &opts, Some(&mut stop_at_deadline))?;
    let parts: Vec<String> = out.into_iter().map(|(t, _)| t.trim().to_string()).collect();
    Ok(parts.join(" "))
}

#[allow(clippy::too_many_arguments)]
fn lane(
    lang: &str,
    dir: PathBuf,
    device: Device,
    threads: usize,
    deadline: Duration,
    ready: &std::sync::mpsc::Sender<(String, Result<()>)>,
    rx: Receiver<Job>,
    hb: &Heartbeat,
) {
    let t = Instant::now();
    let tr = {
        let loaded = load(lang, &dir, device, threads).and_then(|tr| {
            translate(&tr, "Hello, world.", Instant::now() + STALL_LIMIT)?; // warm-up
            Ok(tr)
        });
        match loaded {
            Ok(tr) => tr,
            Err(e) => {
                let _ = ready.send((lang.to_string(), Err(e)));
                return;
            }
        }
    };
    tracing::info!(%lang, model = %dir.display(), load_ms = t.elapsed().as_millis() as u64, "translator ready");
    let _ = ready.send((lang.to_string(), Ok(())));

    while let Ok(job) = rx.recv() {
        let id = job.clause.id;
        let until = job.arrived + deadline;
        if Instant::now() >= until {
            error(id, lang, "deadline passed while queued");
            continue;
        }
        let res = {
            let _busy = hb.busy();
            translate(&tr, &job.clause.text, until)
        };
        let elapsed = job.arrived.elapsed();
        match res {
            Ok(_) if elapsed > deadline => error(id, lang, "deadline passed"),
            Ok(text) if text.is_empty() => error(id, lang, "empty translation"),
            Ok(text) => {
                let msg = Message::Translated {
                    translation: Translation {
                        clause_id: id,
                        lang: lang.to_string(),
                        text,
                        elapsed_ms: u32::try_from(elapsed.as_millis()).unwrap_or(u32::MAX),
                    },
                };
                if worker::send(&msg).is_err() {
                    return;
                }
            }
            Err(e) => error(id, lang, format!("{e:#}")),
        }
    }
    drop(tr);
}
