//! `multi-mt`: the translation worker.
//!
//! Reads `Translate { clause, langs }` and `Shutdown` from stdin; writes one
//! `Translated` (or one `Error`) per requested language, plus `Ready` and
//! `Heartbeat`, to stdout; logs to stderr. Protocol: `multi_core::ipc`.
//!
//! Each language has its own thread, model and bounded queue, so a slow or
//! failing language never holds up the others. A request not finished within
//! the deadline (counted from arrival) is answered with an `Error`.
//!
//! Pivot (M2-2): a `--pivot` language has no direct `source->X` model. Its
//! clause is translated to English on the `en` lane (`source->en`, added
//! when English is not itself a target), which hands the English text to
//! the target's `en->X` lane. One English translation serves every pivot
//! target of a clause; the deadline still counts from arrival.

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
    /// Directory holding CTranslate2 opus-mt models (`opus-mt-<src>-<lang>`).
    #[arg(long)]
    models: PathBuf,
    /// The spoken (source) language.
    #[arg(long, default_value = "en")]
    source: String,
    /// Target languages translated through English (comma-separated): their
    /// model is `en-><lang>`, fed by the `en` lane (`source->en`).
    #[arg(long, value_delimiter = ',')]
    pivot: Vec<String>,
    /// Model folder for one language (`LANG=DIR`), instead of looking in
    /// `--models`; repeatable. `multi run` passes these from the registry.
    #[arg(long = "model", value_name = "LANG=DIR")]
    model: Vec<String>,
    /// Token put before each sentence for one language (`LANG=TOKEN`, e.g.
    /// `pt=>>por<<`), for multi-target models; repeatable.
    #[arg(long = "prefix", value_name = "LANG=TOKEN")]
    prefix: Vec<String>,
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

/// The language translations pivot through.
const PIVOT: &str = "en";

struct Job {
    clause: Clause,
    arrived: Instant,
    /// Send the result as a `Translated` for this lane's language.
    emit: bool,
    /// Pivot targets to hand the result to (only on the `en` lane).
    then: Vec<String>,
}

/// Splits a request's languages into direct lanes and the pivot targets
/// that go through the `en` lane. Returns `(direct, via_en, emit_en)`:
/// with pivot targets, a requested `en` is answered by that same English
/// translation, so it moves out of `direct`.
fn split(langs: Vec<String>, pivot: &[String]) -> (Vec<String>, Vec<String>, bool) {
    let (via, mut direct): (Vec<String>, Vec<String>) =
        langs.into_iter().partition(|l| pivot.contains(l));
    let mut emit_en = false;
    if !via.is_empty()
        && let Some(i) = direct.iter().position(|l| l == PIVOT)
    {
        direct.remove(i);
        emit_en = true;
    }
    (direct, via, emit_en)
}

/// Lanes to start: every target, plus `en` when something pivots.
fn lane_langs(args: &Args) -> Result<Vec<String>> {
    let mut langs = args.langs.clone();
    if args.pivot.is_empty() {
        return Ok(langs);
    }
    if args.source == PIVOT {
        bail!("--pivot needs a non-English --source");
    }
    if let Some(l) = args
        .pivot
        .iter()
        .find(|l| !args.langs.contains(l) || *l == PIVOT)
    {
        bail!("--pivot {l}: must be one of --langs, and not {PIVOT}");
    }
    if !langs.iter().any(|l| l == PIVOT) {
        langs.push(PIVOT.into());
    }
    Ok(langs)
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
    let langs = lane_langs(args)?;
    let mut queues: BTreeMap<String, _> = langs
        .iter()
        .map(|l| (l.clone(), std::sync::mpsc::sync_channel(args.queue.max(1))))
        .collect();
    // The `en` lane hands English text to the pivot targets' queues.
    let forward: BTreeMap<String, SyncSender<Job>> = queues
        .iter()
        .filter(|(l, _)| args.pivot.contains(l))
        .map(|(l, (tx, _))| (l.clone(), tx.clone()))
        .collect();
    let mut starting = BTreeMap::new();
    for lang in &langs {
        let Some((tx, rx)) = queues.remove(lang) else {
            continue;
        };
        let from = if args.pivot.contains(lang) {
            PIVOT
        } else {
            args.source.as_str()
        };
        let dir =
            explicit_dir(&args.model, lang).unwrap_or_else(|| model_dir(&args.models, from, lang));
        let prefix = lookup(&args.prefix, lang);
        let fwd = if lang == PIVOT {
            forward.clone()
        } else {
            BTreeMap::new()
        };
        let (device, threads) = (args.device, args.threads);
        let (lang2, ready, hb2) = (lang.clone(), ready_tx.clone(), Arc::clone(hb));
        let thread = std::thread::Builder::new()
            .name(format!("mt-{lang}"))
            .spawn(move || {
                lane(
                    &lang2,
                    dir,
                    prefix.as_deref(),
                    device,
                    threads,
                    deadline,
                    &ready,
                    rx,
                    &fwd,
                    &hb2,
                )
            })?;
        starting.insert(lang.clone(), Lane { tx, thread });
    }
    drop(forward);
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

    let res = serve(&lanes, &args.pivot);
    // Ordered shutdown: close every queue, then join, so each model is
    // dropped by its own thread before the process exits.
    let threads: Vec<_> = lanes.into_values().map(|l| l.thread).collect();
    for t in threads {
        let _ = t.join();
    }
    tracing::info!("MT stopped");
    res
}

fn serve(lanes: &BTreeMap<String, Lane>, pivot: &[String]) -> Result<()> {
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
                if clause.text.len() > text::MAX_INPUT_BYTES {
                    for lang in &langs {
                        error(clause.id, lang, "clause too long");
                    }
                    continue;
                }
                let (direct, via, emit_en) = split(langs, pivot);
                for lang in direct {
                    let job = Job {
                        clause: clause.clone(),
                        arrived,
                        emit: true,
                        then: Vec::new(),
                    };
                    submit(lanes, &lang, job);
                }
                if !via.is_empty() {
                    let job = Job {
                        clause: clause.clone(),
                        arrived,
                        emit: emit_en,
                        then: via,
                    };
                    submit(lanes, PIVOT, job);
                }
            }
            Frame::Message(Message::Shutdown) => return Ok(()),
            Frame::Message(other) => tracing::warn!("ignoring unexpected message: {other:?}"),
            Frame::Pcm { .. } => tracing::warn!("ignoring PCM frame"),
        }
    }
}

/// Queues `job` on `lang`'s lane; on failure, answers every language the
/// job stood for with an `Error`.
fn submit(lanes: &BTreeMap<String, Lane>, lang: &str, job: Job) {
    let Some(l) = lanes.get(lang) else {
        return fail(&job, lang, "no model for this language");
    };
    match l.tx.try_send(job) {
        Ok(()) => {}
        Err(TrySendError::Full(j)) => fail(&j, lang, "queue full"),
        Err(TrySendError::Disconnected(j)) => fail(&j, lang, "translator stopped"),
    }
}

/// Answers every language `job` stood for on lane `lang` with an `Error`.
fn fail(job: &Job, lang: &str, why: &str) {
    if job.emit {
        error(job.clause.id, lang, why);
    }
    for t in &job.then {
        error(job.clause.id, t, format!("via {lang}: {why}"));
    }
}

fn explicit_dir(pairs: &[String], lang: &str) -> Option<PathBuf> {
    lookup(pairs, lang).map(PathBuf::from)
}

/// The value of `LANG=VALUE` for `lang`.
fn lookup(pairs: &[String], lang: &str) -> Option<String> {
    pairs.iter().find_map(|p| {
        let (l, v) = p.split_once('=')?;
        (l == lang).then(|| v.to_string())
    })
}

fn model_dir(root: &Path, src: &str, lang: &str) -> PathBuf {
    for name in [
        format!("opus-mt-{src}-{lang}"),
        format!("opus-mt-tc-big-{src}-{lang}"),
    ] {
        let p = root.join(name);
        if p.is_dir() {
            return p;
        }
    }
    root.join(format!("opus-mt-{src}-{lang}"))
}

/// SentencePiece plus the multi-target `>>id<<` token. The token must reach
/// the model as one vocabulary entry: put into the text, SentencePiece
/// splits it into pieces (`>>por<<` came out as "Por ⁇ …", M2-2).
struct Tok {
    sp: SpTokenizer,
    prefix: Option<String>,
}

impl ct2rs::Tokenizer for Tok {
    fn encode(&self, input: &str) -> Result<Vec<String>> {
        let mut out: Vec<String> = self.prefix.iter().cloned().collect();
        out.extend(self.sp.encode(input)?);
        Ok(out)
    }

    fn decode(&self, tokens: Vec<String>) -> Result<String> {
        self.sp.decode(tokens)
    }
}

fn load_on(
    dir: &Path,
    prefix: Option<&str>,
    device: ct2rs::Device,
    threads: usize,
) -> Result<Translator<Tok>> {
    let cfg = Config {
        device,
        compute_type: match device {
            ct2rs::Device::CUDA => ComputeType::FLOAT16,
            _ => ComputeType::INT8,
        },
        num_threads_per_replica: threads,
        ..Default::default()
    };
    let sp = SpTokenizer::new(dir).with_context(|| format!("tokenizer in {}", dir.display()))?;
    let tok = Tok {
        sp,
        prefix: prefix.map(str::to_string),
    };
    Translator::with_tokenizer(dir, tok, &cfg)
        .with_context(|| format!("model in {}", dir.display()))
}

fn load(
    lang: &str,
    dir: &Path,
    prefix: Option<&str>,
    device: Device,
    threads: usize,
) -> Result<Translator<Tok>> {
    if !dir.is_dir() {
        bail!("no model directory {}", dir.display());
    }
    let tr = match device {
        Device::Cuda => load_on(dir, prefix, ct2rs::Device::CUDA, threads)?,
        Device::Cpu => load_on(dir, prefix, ct2rs::Device::CPU, threads)?,
        Device::Auto => match load_on(dir, prefix, ct2rs::Device::CUDA, threads) {
            Ok(t) => t,
            Err(e) => {
                tracing::warn!(%lang, "CUDA init failed ({e:#}); falling back to CPU");
                load_on(dir, prefix, ct2rs::Device::CPU, threads)?
            }
        },
    };
    Ok(tr)
}

fn translate(tr: &Translator<Tok>, text: &str, until: Instant) -> Result<String> {
    let parts: Vec<String> = text::sentences(text)
        .into_iter()
        .map(str::to_string)
        .collect();
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
    prefix: Option<&str>,
    device: Device,
    threads: usize,
    deadline: Duration,
    ready: &std::sync::mpsc::Sender<(String, Result<()>)>,
    rx: Receiver<Job>,
    forward: &BTreeMap<String, SyncSender<Job>>,
    hb: &Heartbeat,
) {
    let t = Instant::now();
    let tr = {
        let loaded = load(lang, &dir, prefix, device, threads).and_then(|tr| {
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
            fail(&job, lang, "deadline passed while queued");
            continue;
        }
        let res = {
            let _busy = hb.busy();
            translate(&tr, &job.clause.text, until)
        };
        let elapsed = job.arrived.elapsed();
        let text = match res {
            Ok(_) if elapsed > deadline => {
                fail(&job, lang, "deadline passed");
                continue;
            }
            Ok(text) if text.is_empty() => {
                fail(&job, lang, "empty translation");
                continue;
            }
            Ok(text) => text,
            Err(e) => {
                fail(&job, lang, &format!("{e:#}"));
                continue;
            }
        };
        // Pivot: hand the English text on to each target's `en->X` lane.
        for t in &job.then {
            let next = Job {
                clause: Clause {
                    text: text.clone(),
                    ..job.clause.clone()
                },
                arrived: job.arrived,
                emit: true,
                then: Vec::new(),
            };
            match forward.get(t).map(|tx| tx.try_send(next)) {
                Some(Ok(())) => {}
                Some(Err(TrySendError::Full(_))) => error(id, t, "queue full"),
                Some(Err(TrySendError::Disconnected(_))) => error(id, t, "translator stopped"),
                None => error(id, t, "no pivot lane for this language"),
            }
        }
        if job.emit {
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
    }
    drop(tr);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(s: &[&str]) -> Vec<String> {
        s.iter().map(|x| x.to_string()).collect()
    }

    #[test]
    fn split_sends_pivot_targets_through_english() {
        // No pivot: everything direct.
        assert_eq!(
            split(v(&["en", "fr"]), &[]),
            (v(&["en", "fr"]), v(&[]), false)
        );
        // FR and DE pivot; a requested EN is answered by the pivot hop.
        assert_eq!(
            split(v(&["en", "fr", "de", "it"]), &v(&["fr", "de"])),
            (v(&["it"]), v(&["fr", "de"]), true)
        );
        // English not requested (paused, or not a target): no EN output.
        assert_eq!(split(v(&["fr"]), &v(&["fr"])), (v(&[]), v(&["fr"]), false));
        // Only direct targets requested this time: EN stays direct.
        assert_eq!(
            split(v(&["en", "it"]), &v(&["fr"])),
            (v(&["en", "it"]), v(&[]), false)
        );
    }

    #[test]
    fn pivot_adds_an_english_lane() -> Result<()> {
        let parse = |a: &[&str]| Args::try_parse_from(a.iter().copied());
        let a = parse(&[
            "multi-mt", "--models", "/m", "--source", "pt", "--langs", "fr,de", "--pivot", "fr,de",
        ])?;
        assert_eq!(lane_langs(&a)?, v(&["fr", "de", "en"]));
        let a = parse(&[
            "multi-mt", "--models", "/m", "--source", "es", "--langs", "en,fr", "--pivot", "fr",
        ])?;
        assert_eq!(lane_langs(&a)?, v(&["en", "fr"]));
        let a = parse(&[
            "multi-mt", "--models", "/m", "--langs", "fr", "--pivot", "fr",
        ])?;
        assert!(lane_langs(&a).is_err(), "English source cannot pivot");
        let a = parse(&[
            "multi-mt", "--models", "/m", "--source", "es", "--langs", "fr", "--pivot", "de",
        ])?;
        assert!(lane_langs(&a).is_err(), "pivot must be a target");
        assert_eq!(
            model_dir(Path::new("/nonexistent"), "es", "fr"),
            PathBuf::from("/nonexistent/opus-mt-es-fr")
        );
        Ok(())
    }
}
