//! Control layer: owns the pipeline's lifecycle for `multi run` and
//! `multi serve`.
//!
//! [`Service::start`] runs [`crate::run::run`] on its own thread;
//! [`Service::stop`] stops it (media first, then workers) and returns its
//! result. [`Service::apply`] says which config changes take effect live and
//! which need a restart; [`Service::status`] combines media stats, worker
//! health, uptime and recent errors. [`Service::events`] broadcasts stats
//! (once a second, after [`Service::spawn_ticker`]) and every caption line.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, Weak};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow, bail};
use multi_core::Config;
use multi_media::Stats;
use serde::Serialize;
use tokio::sync::broadcast;
use tracing::{info, warn};

use crate::run::{self, Observer, RunOptions, Snapshot};
use crate::supervisor::{State, Status};

const MAX_ERRORS: usize = 50;
const EVENT_CAPACITY: usize = 512;
const GPU_EVERY: Duration = Duration::from_secs(5);

/// Worker command-line overrides, as for `multi run`.
#[derive(Clone, Debug, Default)]
pub struct Workers {
    pub asr: Option<String>,
    pub mt: Option<String>,
    pub models_dir: Option<PathBuf>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum RunState {
    Stopped,
    /// Thread started, no snapshot yet (workers or media still starting).
    Starting,
    Running,
    Stopping,
    /// The pipeline ended with an error; see `errors`.
    Failed,
}

#[derive(Clone, Debug, Serialize)]
pub struct WorkerInfo {
    pub name: String,
    pub state: &'static str,
    pub restarts: u32,
    pub last_error: Option<String>,
    pub dropped_in: u64,
    pub pid: Option<u32>,
}

impl WorkerInfo {
    fn new(name: &str, s: &Status) -> Self {
        Self {
            name: name.into(),
            state: match s.state {
                State::Starting => "starting",
                State::Ready => "ready",
                State::Restarting => "restarting",
                State::Failed => "failed",
                State::Stopped => "stopped",
            },
            restarts: s.restarts,
            last_error: s.last_error.clone(),
            dropped_in: s.dropped_in,
            pid: s.pid,
        }
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct GpuInfo {
    pub index: u32,
    pub name: String,
    pub memory_used_mb: u64,
    pub memory_total_mb: u64,
    pub utilization_pct: u32,
}

#[derive(Clone, Debug, Serialize)]
pub struct ErrorEntry {
    /// Unix time in milliseconds.
    pub at_ms: u64,
    pub source: String,
    pub message: String,
}

/// Everything the status page shows.
#[derive(Clone, Debug, Serialize)]
pub struct ServiceStatus {
    pub state: RunState,
    pub uptime_s: Option<u64>,
    pub media: Option<Stats>,
    pub workers: Vec<WorkerInfo>,
    pub caption_lag_ms: Option<u64>,
    pub gpu: Option<Vec<GpuInfo>>,
    /// Newest first.
    pub errors: Vec<ErrorEntry>,
    /// Saved settings the running pipeline does not use yet (restart to apply).
    pub restart_pending: Vec<String>,
    /// Settings that need `multi serve` itself restarted (filled in by the web layer).
    pub server_restart_pending: Vec<String>,
}

/// Live events for the GUI.
#[derive(Clone, Debug, Serialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum Event {
    Stats(Box<ServiceStatus>),
    Caption {
        lang: String,
        text: String,
        new_row: bool,
    },
}

/// When a changed setting takes effect.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Effect {
    /// Right away, without touching the stream.
    Live,
    /// At the next pipeline start.
    Restart,
    /// When `multi serve` is restarted.
    Server,
}

/// Per-setting rules (dotted-path prefixes). Anything not listed restarts the
/// pipeline: when in doubt, restart.
const RULES: &[(&str, Effect)] = &[
    ("web.token", Effect::Live),
    ("web.autostart", Effect::Live),
    ("web.username", Effect::Live),
    ("web.password_hash", Effect::Live),
    ("web.bind", Effect::Server),
    ("web.port", Effect::Server),
    ("web.tls", Effect::Server),
    ("web.tls_cert", Effect::Server),
    ("web.tls_key", Effect::Server),
];

pub fn effect_of(path: &str) -> Effect {
    RULES
        .iter()
        .find(|(p, _)| path == *p || path.starts_with(&format!("{p}.")))
        .map_or(Effect::Restart, |(_, e)| *e)
}

/// What [`Service::apply`] did with a new configuration.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct ApplyReport {
    /// Changed settings already in effect (or used at the next start when stopped).
    pub live: Vec<String>,
    /// Changed settings that need a pipeline restart.
    pub restart: Vec<String>,
    /// Changed settings that need `multi serve` restarted.
    pub server_restart: Vec<String>,
}

/// Dotted paths of the settings that differ. Lists (outputs, languages,
/// word lists) count as one setting.
pub fn changed_paths(a: &Config, b: &Config) -> Vec<String> {
    fn walk(prefix: &str, a: &serde_json::Value, b: &serde_json::Value, out: &mut Vec<String>) {
        match (a, b) {
            (serde_json::Value::Object(x), serde_json::Value::Object(y)) => {
                let mut keys: Vec<&String> = x.keys().chain(y.keys()).collect();
                keys.sort();
                keys.dedup();
                for k in keys {
                    let p = if prefix.is_empty() {
                        k.clone()
                    } else {
                        format!("{prefix}.{k}")
                    };
                    let null = serde_json::Value::Null;
                    walk(
                        &p,
                        x.get(k).unwrap_or(&null),
                        y.get(k).unwrap_or(&null),
                        out,
                    );
                }
            }
            _ if a != b => out.push(prefix.to_string()),
            _ => {}
        }
    }
    let mut out = Vec::new();
    if let (Ok(x), Ok(y)) = (serde_json::to_value(a), serde_json::to_value(b)) {
        walk("", &x, &y, &mut out);
    }
    out
}

struct Running {
    stop: Arc<AtomicBool>,
    thread: JoinHandle<Result<()>>,
    started: Instant,
    config: Config,
    snapshot: Arc<Mutex<Option<Snapshot>>>,
}

struct St {
    config: Config,
    run: Option<Running>,
    stopping: bool,
    failed: bool,
}

struct Inner {
    workers: Workers,
    events: broadcast::Sender<Event>,
    st: Mutex<St>,
    errors: Arc<ErrorLog>,
    gpu: Mutex<Option<Vec<GpuInfo>>>,
}

#[derive(Default)]
struct ErrorLog(Mutex<VecDeque<ErrorEntry>>);

impl ErrorLog {
    fn push(&self, source: &str, message: String) {
        let at_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_millis() as u64);
        if let Ok(mut q) = self.0.lock() {
            q.push_front(ErrorEntry {
                at_ms,
                source: source.into(),
                message,
            });
            q.truncate(MAX_ERRORS);
        }
    }

    fn list(&self) -> Vec<ErrorEntry> {
        self.0
            .lock()
            .map(|q| q.iter().cloned().collect())
            .unwrap_or_default()
    }
}

/// The pipeline's observer: stores snapshots, turns counter changes into
/// error entries and forwards captions as events.
struct Obs {
    events: broadcast::Sender<Event>,
    snapshot: Arc<Mutex<Option<Snapshot>>>,
    errors: Arc<ErrorLog>,
}

impl Observer for Obs {
    fn caption(&self, lang: &str, text: &str, new_row: bool) {
        // No receivers is fine.
        let _ = self.events.send(Event::Caption {
            lang: lang.into(),
            text: text.into(),
            new_row,
        });
    }

    fn snapshot(&self, s: Snapshot) {
        let Ok(mut slot) = self.snapshot.lock() else {
            return;
        };
        if let Some(prev) = slot.as_ref() {
            self.diff_errors(prev, &s);
        }
        *slot = Some(s);
    }

    fn worker_error(&self, worker: &str, message: &str) {
        self.errors.push(worker, message.into());
    }
}

impl Obs {
    fn diff_errors(&self, prev: &Snapshot, s: &Snapshot) {
        let workers = [
            ("asr", Some(&prev.asr), Some(&s.asr)),
            ("mt", prev.mt.as_ref(), s.mt.as_ref()),
        ];
        for (name, a, b) in workers {
            if let (Some(a), Some(b)) = (a, b)
                && b.restarts > a.restarts
            {
                let why = b.last_error.clone().unwrap_or_else(|| "unknown".into());
                self.errors.push(name, format!("worker restarted: {why}"));
            }
        }
        if s.media.input_errors > prev.media.input_errors {
            self.errors
                .push("input", "input error; reconnecting".into());
        } else if s.media.input_restarts > prev.media.input_restarts {
            self.errors
                .push("input", "input restarted (no data or end of stream)".into());
        }
        for o in &s.media.outputs {
            let before = prev
                .media
                .outputs
                .iter()
                .find(|p| p.url == o.url)
                .map_or(0, |p| p.errors);
            if o.errors > before {
                self.errors
                    .push("output", format!("{}: error; restarting", o.url));
            }
        }
        if s.media.caption_errors > prev.media.caption_errors {
            self.errors
                .push("captions", "caption encoder error; rebuilt".into());
        }
    }
}

/// Handle to the control layer. Cheap to clone.
#[derive(Clone)]
pub struct Service {
    inner: Arc<Inner>,
}

fn lock(m: &Mutex<St>) -> MutexGuard<'_, St> {
    // A panic while holding the lock cannot leave `St` inconsistent enough
    // to matter; keep serving.
    m.lock().unwrap_or_else(|e| e.into_inner())
}

impl Service {
    /// Warnings for models the default workers need but that are missing
    /// from the models directory (none for an overridden worker).
    pub fn missing_models(&self, config: &Config) -> Vec<multi_core::config::Issue> {
        let w = &self.inner.workers;
        if w.asr.is_some() && w.mt.is_some() {
            return Vec::new();
        }
        let dir = crate::models::resolve_dir(w.models_dir.as_deref());
        let src = config.languages.iter().position(|l| l.source);
        let mut issues = crate::models::missing_models(config, &dir);
        issues.retain(|i| {
            let asr = i.path == "asr.model"
                || src.is_some_and(|s| i.path == format!("languages[{s}].code"));
            if asr { w.asr.is_none() } else { w.mt.is_none() }
        });
        issues
    }

    pub fn new(config: Config, workers: Workers) -> Self {
        let (events, _) = broadcast::channel(EVENT_CAPACITY);
        Self {
            inner: Arc::new(Inner {
                workers,
                events,
                st: Mutex::new(St {
                    config,
                    run: None,
                    stopping: false,
                    failed: false,
                }),
                errors: Arc::default(),
                gpu: Mutex::new(None),
            }),
        }
    }

    pub fn events(&self) -> broadcast::Receiver<Event> {
        self.inner.events.subscribe()
    }

    /// The configuration the next start uses.
    pub fn config(&self) -> Config {
        lock(&self.inner.st).config.clone()
    }

    /// Records an error for the status page.
    pub fn report_error(&self, source: &str, message: String) {
        self.inner.errors.push(source, message);
    }

    /// Joins a pipeline thread that ended by itself.
    fn reap(&self, st: &mut St) {
        if st.run.as_ref().is_some_and(|r| r.thread.is_finished())
            && let Some(r) = st.run.take()
        {
            match r.thread.join() {
                Ok(Ok(())) => {}
                Ok(Err(e)) => {
                    st.failed = true;
                    self.inner.errors.push("pipeline", format!("{e:#}"));
                }
                Err(_) => {
                    st.failed = true;
                    self.inner
                        .errors
                        .push("pipeline", "pipeline thread panicked".into());
                }
            }
        }
    }

    /// Starts the pipeline with `config` (which becomes the current config).
    pub fn start(&self, config: Config) -> Result<()> {
        let mut st = lock(&self.inner.st);
        self.reap(&mut st);
        if st.run.is_some() || st.stopping {
            bail!("the pipeline is already running");
        }
        st.config = config.clone();
        let w = &self.inner.workers;
        let opts = match RunOptions::new(
            config.clone(),
            w.asr.as_deref(),
            w.mt.as_deref(),
            w.models_dir.as_deref(),
        ) {
            Ok(o) => o,
            Err(e) => {
                self.inner.errors.push("pipeline", format!("{e:#}"));
                return Err(e);
            }
        };
        let stop = Arc::new(AtomicBool::new(false));
        let snapshot = Arc::new(Mutex::new(None));
        let obs = Obs {
            events: self.inner.events.clone(),
            snapshot: snapshot.clone(),
            errors: self.inner.errors.clone(),
        };
        let flag = stop.clone();
        let thread = std::thread::Builder::new()
            .name("pipeline".into())
            .spawn(move || run::run(opts, &flag, &obs))
            .context("cannot start the pipeline thread")?;
        info!("pipeline starting");
        st.failed = false;
        st.run = Some(Running {
            stop,
            thread,
            started: Instant::now(),
            config,
            snapshot,
        });
        Ok(())
    }

    /// True while the pipeline thread is alive.
    pub fn is_active(&self) -> bool {
        lock(&self.inner.st)
            .run
            .as_ref()
            .is_some_and(|r| !r.thread.is_finished())
    }

    /// Stops the pipeline (media first, then workers) and returns how it
    /// ended. Blocks until it has stopped. Does nothing when stopped.
    pub fn stop(&self) -> Result<()> {
        let run = {
            let mut st = lock(&self.inner.st);
            let Some(run) = st.run.take() else {
                return Ok(());
            };
            st.stopping = true;
            run
        };
        run.stop.store(true, Ordering::Release);
        let result = run
            .thread
            .join()
            .unwrap_or_else(|_| Err(anyhow!("pipeline thread panicked")));
        let mut st = lock(&self.inner.st);
        st.stopping = false;
        if let Err(e) = &result {
            st.failed = true;
            self.inner.errors.push("pipeline", format!("{e:#}"));
        }
        result
    }

    /// Makes `config` current. Settings marked live take effect now; the
    /// report lists the ones that wait for a pipeline or server restart.
    pub fn apply(&self, config: Config) -> ApplyReport {
        let mut st = lock(&self.inner.st);
        self.reap(&mut st);
        let mut report = ApplyReport::default();
        let running = st.run.as_ref().map(|r| r.config.clone());
        for path in changed_paths(&st.config, &config) {
            match effect_of(&path) {
                Effect::Server => report.server_restart.push(path),
                Effect::Restart if running.is_some() => report.restart.push(path),
                _ => report.live.push(path),
            }
        }
        st.config = config;
        report
    }

    pub fn status(&self) -> ServiceStatus {
        let mut st = lock(&self.inner.st);
        self.reap(&mut st);
        let snap = st
            .run
            .as_ref()
            .and_then(|r| r.snapshot.lock().ok().and_then(|s| s.clone()));
        let state = match (&st.run, st.stopping, st.failed) {
            (_, true, _) => RunState::Stopping,
            (Some(_), _, _) if snap.is_some() => RunState::Running,
            (Some(_), _, _) => RunState::Starting,
            (None, _, true) => RunState::Failed,
            (None, _, false) => RunState::Stopped,
        };
        let restart_pending = st
            .run
            .as_ref()
            .map(|r| {
                changed_paths(&r.config, &st.config)
                    .into_iter()
                    .filter(|p| effect_of(p) == Effect::Restart)
                    .collect()
            })
            .unwrap_or_default();
        let mut workers = Vec::new();
        if let Some(s) = &snap {
            workers.push(WorkerInfo::new("asr", &s.asr));
            if let Some(mt) = &s.mt {
                workers.push(WorkerInfo::new("mt", mt));
            }
        }
        ServiceStatus {
            state,
            uptime_s: st.run.as_ref().map(|r| r.started.elapsed().as_secs()),
            caption_lag_ms: snap.as_ref().and_then(|s| s.caption_lag_ms),
            media: snap.map(|s| s.media),
            workers,
            gpu: self.inner.gpu.lock().ok().and_then(|g| g.clone()),
            errors: self.inner.errors.list(),
            restart_pending,
            server_restart_pending: Vec::new(),
        }
    }

    /// Sends a stats event every second while anyone listens, and polls the
    /// GPU (`nvidia-smi`) every 5 s. Ends when the service is dropped.
    pub fn spawn_ticker(&self) -> std::io::Result<()> {
        let weak: Weak<Inner> = Arc::downgrade(&self.inner);
        std::thread::Builder::new()
            .name("service-ticker".into())
            .spawn(move || {
                let mut next_gpu = Instant::now();
                loop {
                    std::thread::sleep(Duration::from_secs(1));
                    let Some(inner) = weak.upgrade() else { return };
                    if Instant::now() >= next_gpu {
                        next_gpu = Instant::now() + GPU_EVERY;
                        let g = query_gpu();
                        if let Ok(mut slot) = inner.gpu.lock() {
                            *slot = g;
                        }
                    }
                    if inner.events.receiver_count() > 0 {
                        let svc = Service { inner };
                        let s = svc.status();
                        let _ = svc.inner.events.send(Event::Stats(Box::new(s)));
                    }
                }
            })
            .map(|_| ())
    }
}

/// GPU name and memory via `nvidia-smi`; `None` if it is missing or slow.
fn query_gpu() -> Option<Vec<GpuInfo>> {
    let mut child = Command::new("nvidia-smi")
        .args([
            "--query-gpu=index,name,memory.used,memory.total,utilization.gpu",
            "--format=csv,noheader,nounits",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let end = Instant::now() + Duration::from_secs(2);
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if Instant::now() < end => std::thread::sleep(Duration::from_millis(20)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                warn!("nvidia-smi did not answer in 2 s");
                return None;
            }
        }
    }
    let out = child.wait_with_output().ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    let gpus: Vec<GpuInfo> = text
        .lines()
        .filter_map(|l| {
            let f: Vec<&str> = l.split(',').map(str::trim).collect();
            Some(GpuInfo {
                index: f.first()?.parse().ok()?,
                name: f.get(1)?.to_string(),
                memory_used_mb: f.get(2)?.parse().ok()?,
                memory_total_mb: f.get(3)?.parse().ok()?,
                utilization_pct: f.get(4)?.parse().unwrap_or(0),
            })
        })
        .collect();
    (!gpus.is_empty()).then_some(gpus)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn changed_paths_are_dotted() {
        let a = Config::default();
        let mut b = a.clone();
        b.captions.rows = 2;
        b.web.port = 9000;
        b.outputs.clear();
        assert_eq!(
            changed_paths(&a, &b),
            vec!["captions.rows", "outputs", "web.port"]
        );
        assert!(changed_paths(&a, &a).is_empty());
    }

    #[test]
    fn rules() {
        assert_eq!(effect_of("web.token"), Effect::Live);
        assert_eq!(effect_of("web.port"), Effect::Server);
        assert_eq!(effect_of("captions.rows"), Effect::Restart);
        assert_eq!(effect_of("web.tokenx"), Effect::Restart);
    }

    #[test]
    fn apply_when_stopped_is_all_live() {
        let s = Service::new(Config::default(), Workers::default());
        let mut c = Config::default();
        c.captions.rows = 2;
        c.web.port = 1234;
        let r = s.apply(c.clone());
        assert_eq!(r.live, vec!["captions.rows"]);
        assert_eq!(r.server_restart, vec!["web.port"]);
        assert!(r.restart.is_empty());
        assert_eq!(s.config(), c);
        assert_eq!(s.status().state, RunState::Stopped);
    }
}
