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
//!
//! An [`Observer`] sees every caption line and a [`Snapshot`] each second;
//! the control layer (`service`) uses it for status and live events.

use crate::models::{self, AsrPaths};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use multi_core::Config;
use multi_core::ipc::Message;
use multi_core::quality::CaptionQuality;
use multi_media::{AudioChunk, CaptionHandle, Media, MediaConfig, OutputControl, Stats};
use tracing::{debug, info, warn};

use crate::segment::{Event, Segmenter};
use crate::supervisor::{Policy, Status, Supervisor, WorkerSpec};

/// PCM frame length sent to the ASR worker.
pub const PCM_FRAME_MS: u64 = 100;
const SAMPLES_PER_MS: u64 = 16;
const STATS_EVERY: Duration = Duration::from_secs(10);
const SNAPSHOT_EVERY: Duration = Duration::from_secs(1);

/// What the running pipeline looks like right now.
#[derive(Clone, Debug)]
pub struct Snapshot {
    pub media: Stats,
    pub asr: Status,
    /// `None` when there is nothing to translate.
    pub mt: Option<Status>,
    /// Audio timeline position minus the end of the latest ASR word: how far
    /// the source captions trail the audio (ASR plus IPC, before offset).
    pub caption_lag_ms: Option<u64>,
}

/// Watches a running pipeline. Called from the text thread; must not block.
pub trait Observer: Send + Sync {
    /// A caption line was queued for `lang`'s lane.
    fn caption(&self, _lang: &str, _text: &str, _new_row: bool) {}
    /// Called once a second.
    fn snapshot(&self, _s: Snapshot) {}
    /// A worker reported an error.
    fn worker_error(&self, _worker: &str, _message: &str) {}
    /// The media pipeline is up; `outputs` controls its outputs live (ids
    /// `0..n` in config order).
    fn media_started(&self, _outputs: OutputControl) {}
}

/// No observer.
impl Observer for () {}
/// Splits a worker command line (`path arg arg ...`) on whitespace.
pub fn worker_command(name: &str, cmd: &str) -> Result<WorkerSpec> {
    let mut parts = cmd.split_whitespace();
    let Some(program) = parts.next() else {
        bail!("empty --{name}-worker command");
    };
    Ok(WorkerSpec::new(name, program).args(parts))
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

/// The real workers, next to the `multi` executable, with model folders
/// resolved from the registry (`crate::models`).
pub fn default_asr(config: &Config, bin_dir: &Path, paths: &AsrPaths) -> WorkerSpec {
    WorkerSpec::new("asr", bin_dir.join("multi-asr"))
        .arg("--model-dir")
        .arg(&paths.model_dir)
        .arg("--vad-model")
        .arg(&paths.vad)
        .args(["--device", "auto", "--lang"])
        .arg(source_lang(config))
        .arg("--vad-threshold")
        .arg(config.vad.threshold.to_string())
}

/// `models`: `(language, folder)` per target language.
pub fn default_mt(
    config: &Config,
    bin_dir: &Path,
    root: &Path,
    models: &[(String, PathBuf)],
) -> WorkerSpec {
    let mut spec = WorkerSpec::new("mt", bin_dir.join("multi-mt"))
        .arg("--models")
        .arg(root.join("ct2"))
        .args(["--device", "auto", "--langs"])
        .arg(target_langs(config).join(","));
    for (lang, dir) in models {
        let mut pair = std::ffi::OsString::from(format!("{lang}="));
        pair.push(dir);
        spec = spec.arg("--model").arg(pair);
    }
    spec
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
        let models = models::resolve_dir(models);
        let asr = match asr_cmd {
            Some(c) => worker_command("asr", c)?,
            None => default_asr(&config, &bin_dir, &models::asr_paths(&config, &models)?),
        };
        let mt = if target_langs(&config).is_empty() {
            None
        } else {
            Some(match mt_cmd {
                Some(c) => worker_command("mt", c)?,
                None => default_mt(
                    &config,
                    &bin_dir,
                    &models,
                    &models::mt_paths(&config, &models)?,
                ),
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
pub fn run(opts: RunOptions, stop: &AtomicBool, obs: &dyn Observer) -> Result<()> {
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
    let audio_ms = Arc::new(AtomicU64::new(0));
    let audio_pos = audio_ms.clone();
    let audio = Arc::new(move |chunk: AudioChunk| {
        let frames = match framer.lock() {
            Ok(mut f) => f.push(chunk),
            Err(_) => return,
        };
        for (start_ms, samples) in frames {
            audio_pos.store(start_ms + PCM_FRAME_MS, Ordering::Relaxed);
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
    obs.media_started(media.outputs());

    let text = TextPath {
        seg: Segmenter::new(Duration::from_millis(u64::from(
            config.translate.max_wait_ms,
        ))),
        captions: media.captions(),
        source: source_lang(config),
        q: CaptionQuality::new(config),
        obs,
        audio_ms,
        lag_ms: None,
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

struct TextPath<'a> {
    seg: Segmenter,
    captions: CaptionHandle,
    source: String,
    /// Cleaning, word filter, stale drops and degrade policy (WP4).
    q: CaptionQuality,
    obs: &'a dyn Observer,
    /// End of the latest PCM frame sent to ASR, on the audio timeline.
    audio_ms: Arc<AtomicU64>,
    lag_ms: Option<u64>,
}

impl TextPath<'_> {
    /// `age`: how old the text already is (a translation: since its clause
    /// closed); text past the lanes' age cap is dropped.
    fn push(&self, lang: &str, text: &str, new_row: bool, age: Duration) {
        if self.captions.push_aged(lang, text, new_row, age) {
            self.obs.caption(lang, text.trim(), new_row);
        }
    }

    fn words_arrived(&mut self, words: &[multi_core::Word]) {
        if let Some(w) = words.last() {
            let pos = self.audio_ms.load(Ordering::Relaxed);
            self.lag_ms = Some(pos.saturating_sub(w.end_ms));
        }
    }

    fn handle(&mut self, events: Vec<Event>, mt: Option<&Supervisor>) {
        for e in events {
            match e {
                Event::Source { text, new_row } => {
                    if let Some(text) = self.q.source_text(&text) {
                        self.push(&self.source, &text, new_row, Duration::ZERO);
                    }
                }
                Event::Clause {
                    clause,
                    new_row,
                    reason,
                } => {
                    let Some(mt) = mt else { continue };
                    let now = self.seg.ms(Instant::now());
                    let langs = self.q.clause(&clause, new_row, reason, now);
                    if !langs.is_empty() {
                        mt.send_message(Message::Translate { clause, langs });
                    }
                }
            }
        }
    }

    /// Degrade policy tick: per-lane queue age from the media stats.
    fn degrade(&mut self, media: &Media) {
        let ages: Vec<(String, u64)> = media
            .stats()
            .lanes
            .into_iter()
            .map(|l| (l.lang, l.oldest_ms))
            .collect();
        let now = self.seg.ms(Instant::now());
        if let Some(paused) = self.q.update_degrade(&ages, now) {
            warn!(?paused, "caption lag: paused lanes changed");
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
    let mut next_degrade = Instant::now() + Duration::from_secs(1);
    let mut next_snapshot = Instant::now() + SNAPSHOT_EVERY;
    while !stop.load(Ordering::Acquire) {
        match asr_rx.recv_timeout(Duration::from_millis(20)) {
            Ok(Message::Words { words }) => {
                t.words_arrived(&words);
                let ev = t.seg.words(&words, Instant::now());
                t.handle(ev, mt.map(|m| m.0));
            }
            Ok(Message::Error { message }) => {
                warn!(worker = "asr", %message, "worker error");
                t.obs.worker_error("asr", &message);
            }
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
                        let now = t.seg.ms(Instant::now());
                        if let Some(r) = t.q.translation(&tr, now) {
                            let age = Duration::from_millis(r.age_ms);
                            t.push(&tr.lang, &r.text, r.new_row, age);
                        }
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
            info!(quality = %t.q.summary(&t.seg.stats()), "caption quality");
        }
        if Instant::now() >= next_degrade {
            next_degrade += Duration::from_secs(1);
            t.degrade(media);
        }
        if Instant::now() >= next_snapshot {
            next_snapshot = Instant::now() + SNAPSHOT_EVERY;
            t.obs.snapshot(Snapshot {
                media: media.stats(),
                asr: asr.status(),
                mt: mt.map(|m| m.0.status()),
                caption_lag_ms: t.lag_ms,
            });
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
                match (o.enabled, o.running) {
                    (false, _) => "off",
                    (true, true) => "up",
                    (true, false) => "down",
                },
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
        fallback_active = s.fallback_active,
        fallback_activations = s.fallback_activations,
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
        let paths = AsrPaths {
            id: "x".into(),
            model_dir: "/m/asr".into(),
            vad: "/m/vad.onnx".into(),
        };
        let a = default_asr(&c, Path::new("/opt/multi"), &paths);
        assert_eq!(a.program, PathBuf::from("/opt/multi/multi-asr"));
        assert!(a.args.contains(&OsString::from("/m/vad.onnx")));
        let mt = [("es".to_string(), PathBuf::from("/m/ct2/opus-mt-en-es"))];
        let m = default_mt(&c, Path::new("/opt/multi"), Path::new("/m"), &mt);
        assert!(m.args.contains(&OsString::from("es,fr,de")));
        assert!(m.args.contains(&OsString::from("es=/m/ct2/opus-mt-en-es")));
    }
}
