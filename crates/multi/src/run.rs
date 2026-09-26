//! `multi run`: media pipeline + ASR and translation workers.
//!
//! ```text
//! media audio tap -> PCM frames (100 ms) -> ASR worker -> Words -> segmenter -> source lane
//!                                                                      `-> clause -> MT worker -> translated lanes
//! ```
//!
//! Nothing here can block video: the tap callback only queues frames into
//! the supervisor (which drops the oldest when full), and caption pushes are
//! non-blocking. Shutdown order: media first, then the workers.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use multi_core::Config;
use multi_core::ipc::Message;
use multi_media::{AudioChunk, CaptionHandle, Media, MediaConfig};
use tracing::{debug, info, warn};

use crate::segment::{Event, Segmenter};
use crate::supervisor::{Policy, Supervisor, WorkerSpec};

/// PCM frame length sent to the ASR worker.
pub const PCM_FRAME_MS: u64 = 100;
const SAMPLES_PER_MS: u64 = 16;
const STATS_EVERY: Duration = Duration::from_secs(10);
/// Default Nemotron export (see docs/m1/README.md, "Workers").
const ASR_MODEL: &str = "sherpa/sherpa-onnx-nemotron-3.5-asr-streaming-0.6b-560ms-2026-06-11-fp32";

/// Splits a worker command line (`path arg arg ...`) on whitespace.
pub fn worker_command(name: &str, cmd: &str) -> Result<WorkerSpec> {
    let mut parts = cmd.split_whitespace();
    let Some(program) = parts.next() else {
        bail!("empty --{name}-worker command");
    };
    Ok(WorkerSpec::new(name, program).args(parts))
}

/// Where the default models live: `$MULTI_MODELS`, else `~/.cache/multi-models`.
pub fn default_models_dir() -> PathBuf {
    if let Some(d) = std::env::var_os("MULTI_MODELS") {
        return PathBuf::from(d);
    }
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_default()
        .join(".cache/multi-models")
}

fn source_lang(config: &Config) -> String {
    config
        .languages
        .iter()
        .find(|l| l.source)
        .map_or_else(|| "en".into(), |l| l.code.clone())
}

fn target_langs(config: &Config) -> Vec<String> {
    config
        .languages
        .iter()
        .filter(|l| !l.source)
        .map(|l| l.code.clone())
        .collect()
}

/// The real workers, next to the `multi` executable.
pub fn default_asr(config: &Config, bin_dir: &Path, models: &Path) -> WorkerSpec {
    WorkerSpec::new("asr", bin_dir.join("multi-asr"))
        .arg("--model-dir")
        .arg(models.join(ASR_MODEL))
        .args(["--device", "auto", "--lang"])
        .arg(source_lang(config))
        .arg("--vad-threshold")
        .arg(config.vad.threshold.to_string())
}

pub fn default_mt(config: &Config, bin_dir: &Path, models: &Path) -> WorkerSpec {
    WorkerSpec::new("mt", bin_dir.join("multi-mt"))
        .arg("--models")
        .arg(models.join("ct2"))
        .args(["--device", "auto", "--langs"])
        .arg(target_langs(config).join(","))
}

pub struct RunOptions {
    pub config: Config,
    pub asr: WorkerSpec,
    /// `None` when there is nothing to translate.
    pub mt: Option<WorkerSpec>,
}

impl RunOptions {
    /// Default worker specs, with command-line overrides.
    pub fn new(
        config: Config,
        asr_cmd: Option<&str>,
        mt_cmd: Option<&str>,
        models: Option<&Path>,
    ) -> Result<Self> {
        let exe = std::env::current_exe().context("cannot find the multi executable")?;
        let bin_dir = exe.parent().map(Path::to_path_buf).unwrap_or_default();
        let models = models.map_or_else(default_models_dir, Path::to_path_buf);
        let asr = match asr_cmd {
            Some(c) => worker_command("asr", c)?,
            None => default_asr(&config, &bin_dir, &models),
        };
        let mt = if target_langs(&config).is_empty() {
            None
        } else {
            Some(match mt_cmd {
                Some(c) => worker_command("mt", c)?,
                None => default_mt(&config, &bin_dir, &models),
            })
        };
        Ok(Self { config, asr, mt })
    }
}

/// Groups tap audio into fixed-length PCM frames on the audio timeline.
#[derive(Default)]
pub struct Framer {
    start_ms: u64,
    buf: Vec<i16>,
}

impl Framer {
    const LEN: usize = (PCM_FRAME_MS * SAMPLES_PER_MS) as usize;

    /// Adds a chunk; returns the frames it completed.
    pub fn push(&mut self, chunk: AudioChunk) -> Vec<(u64, Vec<i16>)> {
        let mut out = Vec::new();
        let expected = self.start_ms + self.buf.len() as u64 / SAMPLES_PER_MS;
        // A jump in the timeline (source restart, dropped audio) flushes.
        if !self.buf.is_empty() && chunk.start_ms.abs_diff(expected) > 20 {
            out.push((self.start_ms, std::mem::take(&mut self.buf)));
        }
        if self.buf.is_empty() {
            self.start_ms = chunk.start_ms;
        }
        self.buf.extend_from_slice(&chunk.samples);
        while self.buf.len() >= Self::LEN {
            let rest = self.buf.split_off(Self::LEN);
            out.push((self.start_ms, std::mem::replace(&mut self.buf, rest)));
            self.start_ms += PCM_FRAME_MS;
        }
        out
    }
}

/// Runs until `stop` is set, then shuts down media, then the workers.
pub fn run(opts: RunOptions, stop: &AtomicBool) -> Result<()> {
    let config = &opts.config;
    let (asr, asr_rx) =
        Supervisor::start(opts.asr, Policy::default()).context("cannot start the ASR worker")?;
    let asr = Arc::new(asr);
    let mt = match opts.mt {
        Some(spec) => Some(
            Supervisor::start(spec, Policy::default())
                .context("cannot start the translation worker")?,
        ),
        None => None,
    };

    let framer = Mutex::new(Framer::default());
    let asr_tap = asr.clone();
    let audio = Arc::new(move |chunk: AudioChunk| {
        let frames = match framer.lock() {
            Ok(mut f) => f.push(chunk),
            Err(_) => return,
        };
        for (start_ms, samples) in frames {
            asr_tap.send_pcm(start_ms, samples);
        }
    });
    let media = match Media::start(MediaConfig::from_config(config), audio) {
        Ok(m) => m,
        Err(e) => {
            asr.shutdown();
            if let Some((mt, _)) = &mt {
                mt.shutdown();
            }
            return Err(e.context("cannot start the media pipeline"));
        }
    };

    let text = TextPath {
        seg: Segmenter::new(Duration::from_millis(u64::from(
            config.translate.max_wait_ms,
        ))),
        captions: media.captions(),
        source: source_lang(config),
        targets: target_langs(config),
        rows: BTreeMap::new(),
    };
    text_loop(
        text,
        &asr_rx,
        mt.as_ref().map(|(s, rx)| (s, rx)),
        &media,
        &asr,
        stop,
    );

    info!("shutting down: media, then workers");
    media.stop();
    asr.shutdown();
    if let Some((mt, _)) = &mt {
        mt.shutdown();
    }
    info!("stopped");
    Ok(())
}

struct TextPath {
    seg: Segmenter,
    captions: CaptionHandle,
    source: String,
    targets: Vec<String>,
    /// Clause id -> starts a new row, for translations.
    rows: BTreeMap<u64, bool>,
}

impl TextPath {
    fn handle(&mut self, events: Vec<Event>, mt: Option<&Supervisor>) {
        for e in events {
            match e {
                Event::Source { text, new_row } => {
                    self.captions.push(&self.source, &text, new_row);
                }
                Event::Clause { clause, new_row } => {
                    let Some(mt) = mt else { continue };
                    self.rows.insert(clause.id, new_row);
                    while self.rows.len() > 256 {
                        self.rows.pop_first();
                    }
                    mt.send_message(Message::Translate {
                        clause,
                        langs: self.targets.clone(),
                    });
                }
            }
        }
    }
}

fn text_loop(
    mut t: TextPath,
    asr_rx: &Receiver<Message>,
    mt: Option<(&Supervisor, &Receiver<Message>)>,
    media: &Media,
    asr: &Supervisor,
    stop: &AtomicBool,
) {
    let mut next_stats = Instant::now() + STATS_EVERY;
    while !stop.load(Ordering::Acquire) {
        match asr_rx.recv_timeout(Duration::from_millis(20)) {
            Ok(Message::Words { words }) => {
                let ev = t.seg.words(&words, Instant::now());
                t.handle(ev, mt.map(|m| m.0));
            }
            Ok(Message::Error { message }) => warn!(worker = "asr", %message, "worker error"),
            Ok(other) => debug!(?other, "unexpected message from ASR worker"),
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => {
                warn!("ASR supervisor gone");
                std::thread::sleep(Duration::from_millis(20));
            }
        }
        let ev = t.seg.tick(Instant::now());
        t.handle(ev, mt.map(|m| m.0));
        if let Some((_, rx)) = mt {
            while let Ok(msg) = rx.try_recv() {
                match msg {
                    Message::Translated { translation: tr } => {
                        let row = t.rows.get(&tr.clause_id).copied().unwrap_or(true);
                        t.captions.push(&tr.lang, &tr.text, row);
                    }
                    Message::Error { message } => {
                        debug!(worker = "mt", %message, "translation error")
                    }
                    other => debug!(?other, "unexpected message from MT worker"),
                }
            }
        }
        if Instant::now() >= next_stats {
            next_stats += STATS_EVERY;
            log_stats(media, asr, mt.map(|m| m.0));
        }
    }
}

fn log_stats(media: &Media, asr: &Supervisor, mt: Option<&Supervisor>) {
    let s = media.stats();
    let outputs: Vec<String> = s
        .outputs
        .iter()
        .map(|o| {
            format!(
                "{}:{}:err={}",
                o.url,
                if o.running { "up" } else { "down" },
                o.errors
            )
        })
        .collect();
    let lanes: Vec<String> = s
        .lanes
        .iter()
        .map(|l| format!("{}:{}/{}/{}", l.lang, l.pushed, l.dropped, l.queued))
        .collect();
    let a = asr.status();
    let m = mt.map(|m| m.status());
    info!(
        frames_in = s.frames_in,
        frames_out = s.frames_out,
        input_live = s.input_live,
        input_restarts = s.input_restarts,
        sessions = s.sessions,
        caption_frames = s.caption_frames,
        audio_chunks = s.audio_chunks,
        audio_drops = s.audio_drops,
        outputs = %outputs.join(" "),
        lanes_pushed_dropped_queued = %lanes.join(" "),
        asr = ?a.state,
        asr_restarts = a.restarts,
        asr_dropped = a.dropped_in,
        mt = ?m.as_ref().map(|m| m.state),
        mt_restarts = m.as_ref().map_or(0, |m| m.restarts),
        "stats"
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsString;

    fn chunk(start_ms: u64, n: usize) -> AudioChunk {
        AudioChunk {
            start_ms,
            samples: vec![1; n],
        }
    }

    #[test]
    fn framer_makes_fixed_frames_on_the_timeline() {
        let mut f = Framer::default();
        assert!(f.push(chunk(1000, 1000)).is_empty());
        let out = f.push(chunk(1062, 1000));
        assert_eq!(out.len(), 1);
        assert_eq!((out[0].0, out[0].1.len()), (1000, 1600));
        let out = f.push(chunk(1125, 2800));
        assert_eq!(
            out.iter().map(|o| o.0).collect::<Vec<_>>(),
            vec![1100, 1200]
        );
    }

    #[test]
    fn framer_flushes_on_jump() {
        let mut f = Framer::default();
        f.push(chunk(5000, 800));
        let out = f.push(chunk(0, 800));
        assert_eq!(out, vec![(5000, vec![1; 800])]);
        let out = f.push(chunk(50, 800));
        assert_eq!(out, vec![(0, vec![1; 1600])]);
    }

    #[test]
    fn worker_command_splits() -> Result<()> {
        let s = worker_command("asr", "/bin/fake asr --crash-after 5")?;
        assert_eq!(s.program, PathBuf::from("/bin/fake"));
        assert_eq!(
            s.args,
            vec![OsString::from("asr"), "--crash-after".into(), "5".into()]
        );
        assert!(worker_command("mt", "  ").is_err());
        Ok(())
    }

    #[test]
    fn default_specs_follow_config() {
        let c = Config::default();
        let a = default_asr(&c, Path::new("/opt/multi"), Path::new("/m"));
        assert_eq!(a.program, PathBuf::from("/opt/multi/multi-asr"));
        let m = default_mt(&c, Path::new("/opt/multi"), Path::new("/m"));
        assert!(m.args.contains(&OsString::from("es,fr,de")));
    }
}
