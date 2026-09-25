//! S4 spike: streaming speech recognition candidates, measured.
//!
//! `live`  feeds a WAV at 1x wall clock (like a live stream) and records when
//!         each committed word is emitted -> words TSV, SRT of emission times,
//!         JSON summary, per-second resource trace.
//! `batch` runs the same streaming path as fast as possible over a list of
//!         WAVs (for WER) and writes one hypothesis line per file.

mod res;
mod sherpa_asr;
mod stream;
mod whisper_asr;

use std::fmt::Write as _;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use clap::{Args, Parser, Subcommand, ValueEnum};

use stream::{LaCfg, LaStreamer, SR, Streamer, VadCfg, VadGate, Word};

#[derive(Parser)]
#[command(about = "S4 streaming ASR spike")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Real-time (1x) run over one WAV: caption lag, resources, stability.
    Live {
        #[command(flatten)]
        eng: EngineArgs,
        #[arg(long)]
        wav: PathBuf,
        /// Output path prefix (.words.tsv, .srt, .json, .res.csv, .txt).
        #[arg(long)]
        out: PathBuf,
        /// Play the WAV this many times back to back (soak).
        #[arg(long, default_value_t = 1)]
        loops: u32,
        /// Feed as fast as possible instead of 1x (emission times meaningless).
        #[arg(long)]
        fast: bool,
    },
    /// Fast streaming run over many WAVs: `id<TAB>wav` per line.
    Batch {
        #[command(flatten)]
        eng: EngineArgs,
        #[arg(long)]
        list: PathBuf,
        /// Writes `id<TAB>hypothesis` lines; summary to <out>.json.
        #[arg(long)]
        out: PathBuf,
    },
}

#[derive(Clone, Copy, ValueEnum, Debug, serde::Serialize)]
enum Engine {
    Whisper,
    Parakeet,
    Online,
}

#[derive(Args, Clone, Debug, serde::Serialize)]
struct EngineArgs {
    #[arg(long, value_enum)]
    engine: Engine,
    /// GGML file (whisper) or model directory (sherpa).
    #[arg(long)]
    model: PathBuf,
    #[arg(long, default_value = "en")]
    lang: String,
    /// Run on CPU only.
    #[arg(long)]
    cpu: bool,
    #[arg(long, default_value_t = 4)]
    threads: i32,
    /// LocalAgreement: new audio per pass.
    #[arg(long, default_value_t = 1000)]
    chunk_ms: u32,
    /// LocalAgreement-n.
    #[arg(long, default_value_t = 2)]
    passes: usize,
    #[arg(long, default_value_t = 12.0)]
    trim_s: f64,
    #[arg(long, default_value_t = 25.0)]
    max_buf_s: f64,
    /// Whisper beam size (1 = greedy).
    #[arg(long, default_value_t = 1)]
    beam: i32,
    /// Don't pass committed text as the Whisper prompt.
    #[arg(long)]
    no_prompt: bool,
    /// Silero VAD model; enables the VAD gate.
    #[arg(long)]
    vad: Option<String>,
    #[arg(long, default_value_t = 500)]
    vad_silence_ms: u32,
    #[arg(long, default_value_t = 0.5)]
    vad_threshold: f32,
    /// Online model: commit a trailing word after this much quiet.
    #[arg(long, default_value_t = 400)]
    final_word_ms: u32,
}

fn build(e: &EngineArgs) -> Result<Box<dyn Streamer>> {
    let vad = match &e.vad {
        Some(m) => Some(VadGate::new(&VadCfg {
            model: m.clone(),
            threshold: e.vad_threshold,
            min_silence_s: e.vad_silence_ms as f32 / 1000.0,
            min_speech_s: 0.25,
        })?),
        None => None,
    };
    let la = LaCfg { chunk_ms: e.chunk_ms, passes: e.passes.max(1), trim_s: e.trim_s, max_buf_s: e.max_buf_s };
    let provider = if e.cpu { "cpu" } else { "cuda" };
    let model = e.model.to_string_lossy().into_owned();
    Ok(match e.engine {
        Engine::Whisper => {
            let mut asr = whisper_asr::WhisperAsr::new(&model, &e.lang, !e.cpu, e.threads, e.beam)?;
            asr.use_prompt = !e.no_prompt;
            Box::new(LaStreamer::new(asr, la, vad))
        }
        Engine::Parakeet => {
            let asr = sherpa_asr::ParakeetAsr::new(&e.model, provider, e.threads)?;
            Box::new(LaStreamer::new(asr, la, vad))
        }
        Engine::Online => {
            let cfg = sherpa_asr::OnlineCfg {
                lang: e.lang.clone(),
                final_word_ms: e.final_word_ms,
                chunk_ms: e.chunk_ms,
            };
            Box::new(sherpa_asr::OnlineStreamer::new(&e.model, provider, e.threads, cfg, vad)?)
        }
    })
}

fn read_wav(p: &Path) -> Result<Vec<f32>> {
    let mut r = hound::WavReader::open(p).with_context(|| format!("open {}", p.display()))?;
    let spec = r.spec();
    if spec.sample_rate as usize != SR || spec.channels != 1 {
        bail!("{}: need 16 kHz mono, got {} Hz x{}", p.display(), spec.sample_rate, spec.channels);
    }
    Ok(match spec.sample_format {
        hound::SampleFormat::Int => r.samples::<i16>().map(|s| s.map(|v| v as f32 / 32768.0)).collect::<Result<_, _>>()?,
        hound::SampleFormat::Float => r.samples::<f32>().collect::<Result<_, _>>()?,
    })
}

/// Load + warm up (first CUDA kernels, cuDNN autotune) on the first seconds of `audio`.
fn load(e: &EngineArgs, audio: &[f32]) -> Result<(Box<dyn Streamer>, f64, f64)> {
    let t = Instant::now();
    let mut s = build(e)?;
    let load_s = t.elapsed().as_secs_f64();
    let t = Instant::now();
    let n = audio.len().min(SR * 4);
    for b in audio[..n].chunks(320) {
        s.push(b)?;
    }
    s.finish()?;
    s.reset()?;
    s.reset_stats();
    Ok((s, load_s, t.elapsed().as_secs_f64()))
}

fn srt_ts(t: f64) -> String {
    let ms = (t.max(0.0) * 1000.0).round() as u64;
    format!("{:02}:{:02}:{:02},{:03}", ms / 3_600_000, ms / 60_000 % 60, ms / 1000 % 60, ms % 1000)
}

/// Join words for display: punctuation-only tokens attach to the previous word.
fn join_words<'a>(ws: impl IntoIterator<Item = &'a str>) -> String {
    let mut s = String::new();
    for w in ws {
        if !s.is_empty() && w.chars().any(char::is_alphanumeric) {
            s.push(' ');
        }
        s.push_str(w);
    }
    s
}

/// Roll-up style cues: each emission shows the last 16 committed words, from
/// its emission time until the next emission.
fn build_srt(events: &[(f64, Vec<Word>)]) -> String {
    let mut all: Vec<&str> = Vec::new();
    let mut s = String::new();
    for (i, (t, ws)) in events.iter().enumerate() {
        all.extend(ws.iter().map(|w| w.text.as_str()));
        let end = events.get(i + 1).map(|e| e.0).unwrap_or(t + 4.0).max(t + 0.001);
        let tail = &all[all.len().saturating_sub(16)..];
        let _ = writeln!(s, "{}\n{} --> {}\n{}\n", i + 1, srt_ts(*t), srt_ts(end), join_words(tail.iter().copied()));
    }
    s
}

#[derive(serde::Serialize)]
struct LiveSummary {
    args: EngineArgs,
    wav: String,
    loops: u32,
    realtime: bool,
    audio_s: f64,
    wall_s: f64,
    load_s: f64,
    warmup_s: f64,
    rtf: f64,
    /// Worst lag of the recogniser behind the incoming audio after a push.
    max_backlog_ms: f64,
    pass_p50_ms: f64,
    pass_p95_ms: f64,
    pass_p99_ms: f64,
    words: usize,
    stats: stream::StreamStats,
    res: res::ResSummary,
    error: Option<String>,
}

fn live(e: EngineArgs, wav: PathBuf, out: PathBuf, loops: u32, fast: bool) -> Result<()> {
    let audio = read_wav(&wav)?;
    let sampler = res::Sampler::start(Some(out.with_extension("res.csv")));
    let (mut s, load_s, warmup_s) = load(&e, &audio)?;
    eprintln!("loaded in {load_s:.2}s, warm-up {warmup_s:.2}s");

    let total: Vec<f32> = (0..loops).flat_map(|_| audio.iter().copied()).collect();
    let audio_s = total.len() as f64 / SR as f64;
    let (tx, rx) = mpsc::channel::<Vec<f32>>();
    let t0 = Instant::now();
    let feeder = {
        let total = total.clone();
        std::thread::spawn(move || {
            for (k, b) in total.chunks(320).enumerate() {
                if !fast {
                    let due = Duration::from_micros((k as u64 + 1) * 20_000);
                    let el = t0.elapsed();
                    if due > el {
                        std::thread::sleep(due - el);
                    }
                }
                if tx.send(b.to_vec()).is_err() {
                    break;
                }
            }
        })
    };
    let mut events: Vec<(f64, Vec<Word>)> = Vec::new();
    let mut fed = 0usize;
    let mut max_backlog = 0.0f64;
    let mut error = None;
    let mut last_report = 0.0;
    while let Ok(first) = rx.recv() {
        let mut chunk = first;
        // Real time: take everything that arrived while the last pass ran.
        // Fast mode: one 20 ms block per push, as if the model kept up.
        while !fast && let Ok(more) = rx.try_recv() {
            chunk.extend(more);
        }
        fed += chunk.len();
        match s.push(&chunk) {
            Ok(ws) => {
                let now = t0.elapsed().as_secs_f64();
                max_backlog = max_backlog.max(now - fed as f64 / SR as f64);
                if !ws.is_empty() {
                    events.push((now, ws));
                }
                if now - last_report > 60.0 {
                    last_report = now;
                    let n: usize = events.iter().map(|e| e.1.len()).sum();
                    eprintln!("t={now:.0}s words={n} rss={:.0}MB", res::rss_mb());
                }
            }
            Err(err) => {
                error = Some(format!("{err:#}"));
                break;
            }
        }
    }
    let _ = feeder.join();
    match s.finish() {
        Ok(ws) if !ws.is_empty() => events.push((t0.elapsed().as_secs_f64(), ws)),
        Ok(_) => {}
        Err(err) => error = Some(format!("{err:#}")),
    }
    let wall_s = t0.elapsed().as_secs_f64();
    let r = sampler.finish();

    let mut f = std::fs::File::create(out.with_extension("words.tsv"))?;
    writeln!(f, "word\tt0_s\tt1_s\temit_s")?;
    let mut text = Vec::new();
    for (t, ws) in &events {
        for w in ws {
            writeln!(f, "{}\t{:.3}\t{:.3}\t{:.3}", w.text, w.t0, w.t1, t)?;
            text.push(w.text.clone());
        }
    }
    std::fs::write(out.with_extension("srt"), build_srt(&events))?;
    std::fs::write(out.with_extension("txt"), join_words(text.iter().map(String::as_str)) + "\n")?;
    let st = s.stats();
    let sum = LiveSummary {
        args: e,
        wav: wav.display().to_string(),
        loops,
        realtime: !fast,
        audio_s,
        wall_s,
        load_s,
        warmup_s,
        rtf: st.compute_s / audio_s,
        max_backlog_ms: max_backlog * 1000.0,
        pass_p50_ms: st.pass_pct(50.0),
        pass_p95_ms: st.pass_pct(95.0),
        pass_p99_ms: st.pass_pct(99.0),
        words: text.len(),
        stats: st,
        res: r,
        error,
    };
    let j = serde_json::to_string_pretty(&sum)?;
    std::fs::write(out.with_extension("json"), &j)?;
    println!("{j}");
    Ok(())
}

#[derive(serde::Serialize)]
struct BatchSummary {
    args: EngineArgs,
    files: usize,
    audio_s: f64,
    wall_s: f64,
    load_s: f64,
    warmup_s: f64,
    rtf: f64,
    pass_p50_ms: f64,
    pass_p95_ms: f64,
    stats: stream::StreamStats,
    res: res::ResSummary,
    errors: usize,
}

fn batch(e: EngineArgs, list: PathBuf, out: PathBuf) -> Result<()> {
    let items: Vec<(String, PathBuf)> = std::fs::read_to_string(&list)?
        .lines()
        .filter_map(|l| l.split_once('\t').map(|(a, b)| (a.to_string(), PathBuf::from(b.trim()))))
        .collect();
    let first = read_wav(&items.first().context("empty list")?.1)?;
    let sampler = res::Sampler::start(None);
    let (mut s, load_s, warmup_s) = load(&e, &first)?;
    let mut f = std::fs::File::create(&out)?;
    let t0 = Instant::now();
    let mut audio_s = 0.0;
    let mut errors = 0;
    for (id, p) in &items {
        let a = read_wav(p)?;
        audio_s += a.len() as f64 / SR as f64;
        s.reset()?;
        let mut words = Vec::new();
        let mut run = || -> Result<()> {
            for b in a.chunks(320) {
                words.extend(s.push(b)?);
            }
            words.extend(s.finish()?);
            Ok(())
        };
        if let Err(err) = run() {
            eprintln!("{id}: {err:#}");
            errors += 1;
        }
        let hyp: Vec<&str> = words.iter().map(|w| w.text.as_str()).collect();
        writeln!(f, "{id}\t{}", join_words(hyp))?;
    }
    let wall_s = t0.elapsed().as_secs_f64();
    let r = sampler.finish();
    let st = s.stats();
    let sum = BatchSummary {
        args: e,
        files: items.len(),
        audio_s,
        wall_s,
        load_s,
        warmup_s,
        rtf: st.compute_s / audio_s,
        pass_p50_ms: st.pass_pct(50.0),
        pass_p95_ms: st.pass_pct(95.0),
        stats: st,
        res: r,
        errors,
    };
    let j = serde_json::to_string_pretty(&sum)?;
    std::fs::write(out.with_extension("json"), &j)?;
    println!("{j}");
    Ok(())
}

fn main() -> Result<()> {
    match Cli::parse().cmd {
        Cmd::Live { eng, wav, out, loops, fast } => live(eng, wav, out, loops, fast),
        Cmd::Batch { eng, list, out } => batch(eng, list, out),
    }
}
