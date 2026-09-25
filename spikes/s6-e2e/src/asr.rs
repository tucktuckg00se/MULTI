//! ASR thread: S4's recommended setup (Nemotron 3.5 Streaming 560 ms via
//! sherpa-onnx on CUDA, Silero VAD, one stream per VAD segment) fed from the
//! input's audio tap. Committed words get input-audio PTS: the streamer counts
//! samples, and each tap chunk anchors a sample index to its PTS.

use std::collections::VecDeque;
use std::io::Write;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, Sender};

use anyhow::Result;
use s2_gst_pipe::stamps::wall_ns;
use s4_asr::sherpa_asr::{OnlineCfg, OnlineStreamer};
use s4_asr::stream::{SR, Streamer, VadCfg, VadGate};
use tracing::{error, info};

use crate::input::AudioChunk;

pub struct AsrCfg {
    pub model: PathBuf,
    pub vad: String,
    pub lang: String,
    pub cpu: bool,
    pub words_log: Option<PathBuf>,
}

/// A committed word. `ts`/`te` are input audio PTS in seconds.
#[derive(Clone, Debug)]
pub struct Word {
    pub text: String,
    pub ts: f64,
    pub te: f64,
    pub wall_ns: u64,
}

#[derive(Default)]
pub struct AsrStats {
    pub words: AtomicU64,
    pub errors: AtomicU64,
    /// 1 once the model is loaded.
    pub ready: AtomicU64,
}

/// Sample index -> input PTS (ns) anchors, one per tap chunk.
struct Clock {
    anchors: VecDeque<(u64, i64)>,
}

impl Clock {
    fn at(&self, t: f64) -> f64 {
        let s = (t * SR as f64).max(0.0) as u64;
        let a = self.anchors.iter().rev().find(|(i, _)| *i <= s).or(self.anchors.front());
        match a {
            Some(&(i, pts)) => pts as f64 / 1e9 + (s as f64 - i as f64) / SR as f64,
            None => t,
        }
    }
}

pub fn spawn(
    cfg: AsrCfg,
    rx: Receiver<AudioChunk>,
    out: Sender<Vec<Word>>,
    stats: Arc<AsrStats>,
) -> Result<std::thread::JoinHandle<()>> {
    Ok(std::thread::Builder::new().name("asr".into()).spawn(move || {
        if let Err(e) = run(cfg, rx, out, &stats) {
            stats.errors.fetch_add(1, Ordering::Relaxed);
            error!(err = %e, "ASR thread stopped; video continues without captions");
        }
    })?)
}

fn run(cfg: AsrCfg, rx: Receiver<AudioChunk>, out: Sender<Vec<Word>>, stats: &AsrStats) -> Result<()> {
    let vad = VadGate::new(&VadCfg { model: cfg.vad.clone(), threshold: 0.5, min_silence_s: 0.5, min_speech_s: 0.25 })?;
    let ocfg = OnlineCfg { lang: cfg.lang.clone(), final_word_ms: 400, chunk_ms: 560 };
    let provider = if cfg.cpu { "cpu" } else { "cuda" };
    let t = std::time::Instant::now();
    let mut asr = OnlineStreamer::new(&cfg.model, provider, 4, ocfg, Some(vad))?;
    // Warm up (CUDA kernels, allocator) before real audio arrives.
    asr.push(&vec![0.0; SR])?;
    asr.reset()?;
    info!(ms = t.elapsed().as_millis() as u64, "ASR ready");
    stats.ready.store(1, Ordering::Relaxed);
    let mut log = match &cfg.words_log {
        Some(p) => {
            let mut f = std::io::BufWriter::new(std::fs::File::create(p)?);
            writeln!(f, "wall_ns\taudio_ts\taudio_te\ttext")?;
            Some(f)
        }
        None => None,
    };
    // Audio queued while the model loaded is stale; start from live audio.
    while rx.try_recv().is_ok() {}
    let mut clock = Clock { anchors: VecDeque::new() };
    let mut fed: u64 = 0;
    while let Ok(chunk) = rx.recv() {
        if let Some(pts) = chunk.pts_ns {
            clock.anchors.push_back((fed, pts));
            // ~60 s of anchors is plenty (words commit within seconds).
            while clock.anchors.len() > 3000 {
                clock.anchors.pop_front();
            }
        }
        fed += chunk.samples.len() as u64;
        let words = match asr.push(&chunk.samples) {
            Ok(w) => w,
            Err(e) => {
                stats.errors.fetch_add(1, Ordering::Relaxed);
                error!(err = %e, "ASR push failed");
                continue;
            }
        };
        if words.is_empty() {
            continue;
        }
        let now = wall_ns();
        let ws: Vec<Word> = words
            .into_iter()
            .map(|w| Word { text: w.text, ts: clock.at(w.t0), te: clock.at(w.t1), wall_ns: now })
            .collect();
        stats.words.fetch_add(ws.len() as u64, Ordering::Relaxed);
        if let Some(f) = log.as_mut() {
            for w in &ws {
                let _ = writeln!(f, "{}\t{:.3}\t{:.3}\t{}", w.wall_ns, w.ts, w.te, w.text);
            }
            let _ = f.flush();
        }
        if out.send(ws).is_err() {
            break;
        }
    }
    Ok(())
}
