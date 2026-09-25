//! Translator threads, S5's setup: CTranslate2 + opus-mt (one model per pair),
//! fp16 on CUDA, greedy, sentence split, `max_decoding_length = 4×words + 16`.
//! One thread per target language, so the pairs run in parallel. A clause
//! older than [`DEADLINE`] (queued or finished late) is skipped for that
//! language; errors skip the clause too. The EN lane never waits on this.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, Sender};
use std::time::Duration;

use ct2rs::tokenizers::sentencepiece::Tokenizer as SpTokenizer;
use ct2rs::{ComputeType, Config, Device, Translator};
use s2_gst_pipe::stamps::wall_ns;
use s5_translate::mt::{opus_dir, options, sentences};
use tracing::{error, info, warn};

use crate::captioner::Line;
use crate::segment::Clause;

pub const DEADLINE: Duration = Duration::from_secs(2);

#[derive(Default)]
pub struct MtStats {
    pub ok: AtomicU64,
    pub late: AtomicU64,
    pub errors: AtomicU64,
    pub sum_ms: AtomicU64,
    pub max_ms: AtomicU64,
}

fn load(root: &Path, lang: &str, cpu: bool) -> anyhow::Result<Translator<SpTokenizer>> {
    let d = opus_dir(root, lang)?;
    let cfg = Config {
        device: if cpu { Device::CPU } else { Device::CUDA },
        compute_type: if cpu { ComputeType::INT8 } else { ComputeType::FLOAT16 },
        num_threads_per_replica: 1,
        ..Default::default()
    };
    Translator::with_tokenizer(&d, SpTokenizer::new(&d)?, &cfg)
}

fn translate(tr: &Translator<SpTokenizer>, text: &str) -> anyhow::Result<String> {
    let parts = sentences(text);
    let opts = options(1, text.split_whitespace().count());
    let r = tr.translate_batch(&parts, &opts, None)?;
    let v: Vec<String> = r.into_iter().map(|x| x.0.trim().to_string()).collect();
    Ok(v.join(" "))
}

pub fn spawn(
    root: PathBuf,
    lang: String,
    lane: usize,
    cpu: bool,
    rx: Receiver<Clause>,
    out: Sender<Line>,
    stats: Arc<MtStats>,
) -> anyhow::Result<std::thread::JoinHandle<()>> {
    Ok(std::thread::Builder::new().name(format!("mt-{lang}")).spawn(move || {
        let tr = match load(&root, &lang, cpu) {
            Ok(t) => t,
            Err(e) => {
                stats.errors.fetch_add(1, Ordering::Relaxed);
                error!(%lang, err = %e, "translator failed to load; lane stays empty");
                return;
            }
        };
        // Warm-up (first CUDA call is slow).
        let _ = translate(&tr, "Hello, world.");
        info!(%lang, "translator ready");
        while let Ok(c) = rx.recv() {
            if c.closed_at.elapsed() > DEADLINE {
                stats.late.fetch_add(1, Ordering::Relaxed);
                warn!(%lang, id = c.id, "clause skipped: queued past deadline");
                continue;
            }
            let t0 = std::time::Instant::now();
            match translate(&tr, &c.text) {
                Ok(text) if c.closed_at.elapsed() <= DEADLINE && !text.is_empty() => {
                    let ms = t0.elapsed().as_millis() as u64;
                    stats.ok.fetch_add(1, Ordering::Relaxed);
                    stats.sum_ms.fetch_add(ms, Ordering::Relaxed);
                    stats.max_ms.fetch_max(ms, Ordering::Relaxed);
                    let line = Line { lane, text, new_row: true, clause: c.id, ready_ns: wall_ns() };
                    if out.send(line).is_err() {
                        break;
                    }
                }
                Ok(_) => {
                    stats.late.fetch_add(1, Ordering::Relaxed);
                    warn!(%lang, id = c.id, "clause skipped: translation past deadline or empty");
                }
                Err(e) => {
                    stats.errors.fetch_add(1, Ordering::Relaxed);
                    warn!(%lang, id = c.id, err = %e, "translation failed; clause skipped");
                }
            }
        }
    })?)
}
