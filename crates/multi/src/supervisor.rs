//! Runs one worker process (ASR or translation) and keeps it alive.
//!
//! The worker speaks [`multi_core::ipc`] over stdin/stdout and logs on stderr.
//! The caller gets a [`Supervisor`] handle and a channel of the worker's
//! output messages (`Words`, `Translated`, `Error`).
//!
//! Rules (see `docs/m1/README.md`, "Workers"):
//! - [`Supervisor::send`] never blocks: frames go into a bounded queue that
//!   drops its oldest frame when full. Video must never wait on a worker.
//! - A worker that exits, sends no frame for `heartbeat_timeout`, or breaks
//!   the framing is killed and restarted with exponential backoff
//!   (`backoff_initial` doubling to `backoff_max`; reset once a worker was
//!   ready for `healthy_reset`).
//! - Nothing is written to a new worker until it says `Ready`.
//! - [`Supervisor::shutdown`] sends `Shutdown`, waits `shutdown_grace`, then
//!   kills. Dropping the handle does the same.

use multi_core::ipc::{self, Frame, FrameError, Message};
use std::collections::VecDeque;
use std::ffi::OsString;
use std::io::{BufRead, BufReader, Read};
use std::path::PathBuf;
use std::process::{Child, ChildStderr, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, TrySendError};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

/// What to run.
#[derive(Clone, Debug)]
pub struct WorkerSpec {
    /// Short name for logs and status, e.g. `asr`.
    pub name: String,
    pub program: PathBuf,
    pub args: Vec<OsString>,
    /// Extra environment variables for the worker.
    pub env: Vec<(OsString, OsString)>,
}

impl WorkerSpec {
    pub fn new(name: impl Into<String>, program: impl Into<PathBuf>) -> Self {
        Self {
            name: name.into(),
            program: program.into(),
            args: Vec::new(),
            env: Vec::new(),
        }
    }

    pub fn arg(mut self, arg: impl Into<OsString>) -> Self {
        self.args.push(arg.into());
        self
    }

    pub fn args<I: IntoIterator<Item = S>, S: Into<OsString>>(mut self, args: I) -> Self {
        self.args.extend(args.into_iter().map(Into::into));
        self
    }

    pub fn env(mut self, key: impl Into<OsString>, value: impl Into<OsString>) -> Self {
        self.env.push((key.into(), value.into()));
        self
    }
}

/// Timing and queue limits.
#[derive(Clone, Debug)]
pub struct Policy {
    /// Frames waiting for the worker; the oldest is dropped beyond this.
    pub queue_capacity: usize,
    /// Output messages waiting for the caller; new ones are dropped beyond this.
    pub output_capacity: usize,
    /// A worker that sends no frame for this long is treated as hung.
    pub heartbeat_timeout: Duration,
    /// A worker that is not `Ready` this long after starting is restarted.
    pub ready_timeout: Duration,
    pub backoff_initial: Duration,
    pub backoff_max: Duration,
    /// A worker ready at least this long resets the backoff.
    pub healthy_reset: Duration,
    /// Time a worker gets to exit after `Shutdown` before it is killed.
    pub shutdown_grace: Duration,
}

impl Default for Policy {
    fn default() -> Self {
        Self {
            queue_capacity: 256,
            output_capacity: 1024,
            heartbeat_timeout: Duration::from_secs(3),
            ready_timeout: Duration::from_secs(120),
            backoff_initial: Duration::from_millis(500),
            backoff_max: Duration::from_secs(10),
            healthy_reset: Duration::from_secs(60),
            shutdown_grace: Duration::from_secs(2),
        }
    }
}

impl Policy {
    /// Delay before restart number `failures` (0-based) in a row.
    pub fn backoff(&self, failures: u32) -> Duration {
        let factor = 2u32.saturating_pow(failures.min(31));
        self.backoff_initial
            .saturating_mul(factor)
            .min(self.backoff_max)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    /// The worker process is running but has not said `Ready`.
    Starting,
    /// The worker is ready and receiving frames.
    Ready,
    /// The worker died or hung; waiting out the backoff before restarting.
    Restarting,
    /// The worker could not be started (e.g. missing binary); retried with backoff.
    Failed,
    /// Shut down on request.
    Stopped,
}

#[derive(Clone, Debug)]
pub struct Status {
    pub state: State,
    /// Restarts since the supervisor started.
    pub restarts: u32,
    pub last_error: Option<String>,
    /// Frames dropped from the input queue because it was full.
    pub dropped_in: u64,
    /// Output messages dropped because the caller did not keep up.
    pub dropped_out: u64,
    /// Process id of the current worker, if one is running.
    pub pid: Option<u32>,
}

struct Queue {
    frames: VecDeque<Frame>,
    /// Set once shutdown starts; no more frames are accepted.
    closed: bool,
}

struct Shared {
    name: String,
    policy: Policy,
    queue: Mutex<Queue>,
    queue_cv: Condvar,
    status: Mutex<Status>,
    stopping: AtomicBool,
    stop_cv: (Mutex<()>, Condvar),
    dropped_in: AtomicU64,
    dropped_out: AtomicU64,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

impl Shared {
    fn set_state(&self, state: State) {
        lock(&self.status).state = state;
    }

    fn stopping(&self) -> bool {
        self.stopping.load(Ordering::Acquire)
    }

    /// Sleeps up to `d`, waking early when shutdown starts.
    fn sleep(&self, d: Duration) {
        let (m, cv) = &self.stop_cv;
        let guard = lock(m);
        if !self.stopping() {
            let _ = cv.wait_timeout(guard, d);
        }
    }
}

/// Handle to a supervised worker. Cheap to share by reference across threads.
pub struct Supervisor {
    shared: Arc<Shared>,
    monitor: Mutex<Option<JoinHandle<()>>>,
}

impl Supervisor {
    /// Starts supervising `spec`. Returns the handle and the channel of the
    /// worker's output messages. Fails only if the monitor thread cannot start.
    pub fn start(spec: WorkerSpec, policy: Policy) -> std::io::Result<(Self, Receiver<Message>)> {
        let (tx, rx) = std::sync::mpsc::sync_channel(policy.output_capacity.max(1));
        let shared = Arc::new(Shared {
            name: spec.name.clone(),
            policy,
            queue: Mutex::new(Queue {
                frames: VecDeque::new(),
                closed: false,
            }),
            queue_cv: Condvar::new(),
            status: Mutex::new(Status {
                state: State::Starting,
                restarts: 0,
                last_error: None,
                dropped_in: 0,
                dropped_out: 0,
                pid: None,
            }),
            stopping: AtomicBool::new(false),
            stop_cv: (Mutex::new(()), Condvar::new()),
            dropped_in: AtomicU64::new(0),
            dropped_out: AtomicU64::new(0),
        });
        let s2 = Arc::clone(&shared);
        let monitor = thread::Builder::new()
            .name(format!("sup-{}", spec.name))
            .spawn(move || monitor(&s2, &spec, &tx))?;
        Ok((
            Self {
                shared,
                monitor: Mutex::new(Some(monitor)),
            },
            rx,
        ))
    }

    /// Queues a frame for the worker without blocking. Returns `false` if a
    /// frame was dropped to make room, or the supervisor is shutting down.
    pub fn send(&self, frame: Frame) -> bool {
        let cap = self.shared.policy.queue_capacity.max(1);
        let mut q = lock(&self.shared.queue);
        if q.closed || self.shared.stopping() {
            return false;
        }
        let mut clean = true;
        while q.frames.len() >= cap {
            q.frames.pop_front();
            self.shared.dropped_in.fetch_add(1, Ordering::Relaxed);
            clean = false;
        }
        q.frames.push_back(frame);
        drop(q);
        self.shared.queue_cv.notify_all();
        clean
    }

    pub fn send_message(&self, msg: Message) -> bool {
        self.send(Frame::Message(msg))
    }

    pub fn send_pcm(&self, start_ms: u64, samples: Vec<i16>) -> bool {
        self.send(Frame::Pcm { start_ms, samples })
    }

    pub fn status(&self) -> Status {
        let mut s = lock(&self.shared.status).clone();
        s.dropped_in = self.shared.dropped_in.load(Ordering::Relaxed);
        s.dropped_out = self.shared.dropped_out.load(Ordering::Relaxed);
        s
    }

    /// Frames waiting for the worker.
    pub fn queued(&self) -> usize {
        lock(&self.shared.queue).frames.len()
    }

    /// Stops the worker: `Shutdown`, up to `shutdown_grace` to exit, then kill.
    /// Returns once the worker process is gone. Safe to call more than once.
    pub fn shutdown(&self) {
        // Refuse new frames at once; the queue itself is closed by the
        // monitor after it queues `Shutdown`, so the writer delivers that.
        {
            let _q = lock(&self.shared.queue);
            self.shared.stopping.store(true, Ordering::Release);
        }
        {
            let (m, cv) = &self.shared.stop_cv;
            let _g = lock(m);
            cv.notify_all();
        }
        self.shared.queue_cv.notify_all();
        let handle = lock(&self.monitor).take();
        if let Some(h) = handle
            && h.join().is_err()
        {
            tracing::error!(worker = %self.shared.name, "supervisor thread panicked");
        }
    }
}

impl Drop for Supervisor {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// One running worker process and its I/O threads.
struct Instance {
    child: Child,
    started: Instant,
    io: Arc<InstanceIo>,
    threads: Vec<JoinHandle<()>>,
}

/// State the I/O threads share with the monitor.
struct InstanceIo {
    started: Instant,
    /// Milliseconds since `started` of the last valid frame.
    last_frame_ms: AtomicU64,
    ready_at: Mutex<Option<Instant>>,
    ready: AtomicBool,
    /// Set by the monitor when this instance is being torn down.
    done: AtomicBool,
    /// Why the instance stopped working (protocol error, closed pipe).
    fatal: Mutex<Option<String>>,
    /// Set by the writer once `Shutdown` has been written.
    shutdown_sent: AtomicBool,
}

impl InstanceIo {
    fn fail(&self, why: String) {
        let mut f = lock(&self.fatal);
        if f.is_none() {
            *f = Some(why);
        }
    }

    fn since_last_frame(&self) -> Duration {
        let last = Duration::from_millis(self.last_frame_ms.load(Ordering::Relaxed));
        self.started.elapsed().saturating_sub(last)
    }
}

enum Outcome {
    Stop,
    Died(String),
}

fn monitor(shared: &Arc<Shared>, spec: &WorkerSpec, out: &SyncSender<Message>) {
    let policy = shared.policy.clone();
    let mut failures = 0u32;
    while !shared.stopping() {
        shared.set_state(State::Starting);
        let failed_to_spawn = match spawn(shared, spec, out) {
            Err(e) => {
                let why = format!("cannot start {}: {e}", spec.program.display());
                tracing::error!(worker = %spec.name, "{why}");
                let mut s = lock(&shared.status);
                s.state = State::Failed;
                s.last_error = Some(why);
                s.pid = None;
                true
            }
            Ok(mut inst) => {
                lock(&shared.status).pid = Some(inst.child.id());
                match watch(shared, &mut inst) {
                    Outcome::Stop => {
                        stop_gracefully(shared, inst);
                        break;
                    }
                    Outcome::Died(why) => {
                        tracing::warn!(worker = %spec.name, "worker failed: {why}; restarting");
                        let healthy = lock(&inst.io.ready_at)
                            .is_some_and(|t| t.elapsed() >= policy.healthy_reset);
                        teardown(inst);
                        if healthy {
                            failures = 0;
                        }
                        let mut s = lock(&shared.status);
                        s.last_error = Some(why);
                        s.pid = None;
                        false
                    }
                }
            }
        };
        if shared.stopping() {
            break;
        }
        let delay = policy.backoff(failures);
        failures = failures.saturating_add(1);
        {
            let mut s = lock(&shared.status);
            if !failed_to_spawn {
                s.state = State::Restarting;
            }
            s.restarts = s.restarts.saturating_add(1);
        }
        tracing::info!(worker = %spec.name, delay_ms = delay.as_millis() as u64, "restarting worker");
        shared.sleep(delay);
    }
    let mut s = lock(&shared.status);
    s.state = State::Stopped;
    s.pid = None;
}

fn spawn(
    shared: &Arc<Shared>,
    spec: &WorkerSpec,
    out: &SyncSender<Message>,
) -> std::io::Result<Instance> {
    let mut cmd = Command::new(&spec.program);
    cmd.args(&spec.args)
        .envs(spec.env.iter().map(|(k, v)| (k, v)))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cmd.spawn()?;
    let started = Instant::now();
    let io = Arc::new(InstanceIo {
        started,
        last_frame_ms: AtomicU64::new(0),
        ready_at: Mutex::new(None),
        ready: AtomicBool::new(false),
        done: AtomicBool::new(false),
        fatal: Mutex::new(None),
        shutdown_sent: AtomicBool::new(false),
    });
    let (stdin, stdout, stderr) = (child.stdin.take(), child.stdout.take(), child.stderr.take());
    let (Some(stdin), Some(stdout), Some(stderr)) = (stdin, stdout, stderr) else {
        let _ = child.kill();
        let _ = child.wait();
        return Err(std::io::Error::other("worker stdio was not piped"));
    };
    let mut threads = Vec::with_capacity(3);
    let name = &spec.name;
    let spawn_thread = |label: &str, f: Box<dyn FnOnce() + Send>| {
        thread::Builder::new()
            .name(format!("{name}-{label}"))
            .spawn(f)
    };
    let started_threads = (|| -> std::io::Result<()> {
        let (s, i, o) = (Arc::clone(shared), Arc::clone(&io), out.clone());
        threads.push(spawn_thread(
            "out",
            Box::new(move || read_stdout(&s, &i, stdout, &o)),
        )?);
        let (s, i) = (Arc::clone(shared), Arc::clone(&io));
        threads.push(spawn_thread(
            "in",
            Box::new(move || write_stdin(&s, &i, stdin)),
        )?);
        let n = name.clone();
        threads.push(spawn_thread(
            "err",
            Box::new(move || read_stderr(&n, stderr)),
        )?);
        Ok(())
    })();
    let inst = Instance {
        child,
        started,
        io,
        threads,
    };
    if let Err(e) = started_threads {
        teardown(inst);
        return Err(e);
    }
    tracing::info!(worker = %spec.name, pid = inst.child.id(), "worker started");
    Ok(inst)
}

/// Polls the instance until it fails or shutdown starts.
fn watch(shared: &Shared, inst: &mut Instance) -> Outcome {
    let p = &shared.policy;
    loop {
        if shared.stopping() {
            return Outcome::Stop;
        }
        match inst.child.try_wait() {
            Ok(Some(status)) => return Outcome::Died(format!("worker exited ({status})")),
            Ok(None) => {}
            Err(e) => return Outcome::Died(format!("cannot poll worker: {e}")),
        }
        if let Some(why) = lock(&inst.io.fatal).clone() {
            return Outcome::Died(why);
        }
        let silent = inst.io.since_last_frame();
        if silent > p.heartbeat_timeout {
            return Outcome::Died(format!("no heartbeat for {} ms", silent.as_millis()));
        }
        if !inst.io.ready.load(Ordering::Acquire) && inst.started.elapsed() > p.ready_timeout {
            return Outcome::Died(format!("not ready after {} s", p.ready_timeout.as_secs()));
        }
        shared.sleep(Duration::from_millis(50));
    }
}

/// Kills the worker and waits for its I/O threads (briefly).
fn teardown(mut inst: Instance) {
    inst.io.done.store(true, Ordering::Release);
    let _ = inst.child.kill();
    let _ = inst.child.wait();
    join_briefly(inst.threads, Duration::from_secs(1));
}

/// Joins threads that finish within `limit`; the rest are left to end on
/// their own (a grandchild holding a pipe open must not hang us).
fn join_briefly(threads: Vec<JoinHandle<()>>, limit: Duration) {
    let deadline = Instant::now() + limit;
    for t in threads {
        while !t.is_finished() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(5));
        }
        if t.is_finished() {
            let _ = t.join();
        }
    }
}

fn stop_gracefully(shared: &Shared, mut inst: Instance) {
    let grace = shared.policy.shutdown_grace;
    {
        let mut q = lock(&shared.queue);
        q.closed = true;
        if inst.io.ready.load(Ordering::Acquire) {
            q.frames.push_back(Frame::Message(Message::Shutdown));
        }
    }
    shared.queue_cv.notify_all();
    if inst.io.ready.load(Ordering::Acquire) {
        let deadline = Instant::now() + grace;
        while Instant::now() < deadline {
            if matches!(inst.child.try_wait(), Ok(Some(_)) | Err(_)) {
                tracing::info!(worker = %shared.name, "worker exited cleanly");
                break;
            }
            thread::sleep(Duration::from_millis(10));
        }
        if !inst.io.shutdown_sent.load(Ordering::Acquire) {
            tracing::warn!(worker = %shared.name, "could not deliver Shutdown");
        }
    }
    if matches!(inst.child.try_wait(), Ok(None)) {
        tracing::warn!(worker = %shared.name, "worker did not exit in time; killing");
    }
    teardown(inst);
}

fn read_stdout(shared: &Shared, io: &InstanceIo, stdout: ChildStdout, out: &SyncSender<Message>) {
    // Soft protocol errors (a frame that is well-formed but not understood)
    // are logged, rate-limited; framing errors end the instance.
    let mut soft_errors = 0u64;
    let mut r = BufReader::new(stdout);
    loop {
        let frame = match ipc::read_frame(&mut r) {
            Ok(f) => f,
            Err(FrameError::Closed) => {
                io.fail("worker closed stdout".into());
                return;
            }
            Err(e @ (FrameError::BadJson(_) | FrameError::BadKind(_) | FrameError::BadPcm)) => {
                soft_errors += 1;
                if soft_errors <= 10 || soft_errors.is_power_of_two() {
                    tracing::warn!(worker = %shared.name, count = soft_errors, "ignoring bad frame: {e}");
                }
                continue;
            }
            Err(e) => {
                if !io.done.load(Ordering::Acquire) {
                    io.fail(format!("protocol error: {e}"));
                }
                return;
            }
        };
        let ms = u64::try_from(io.started.elapsed().as_millis()).unwrap_or(u64::MAX);
        io.last_frame_ms.store(ms, Ordering::Relaxed);
        let msg = match frame {
            Frame::Message(m) => m,
            Frame::Pcm { .. } => {
                tracing::warn!(worker = %shared.name, "ignoring PCM from worker");
                continue;
            }
        };
        match msg {
            Message::Heartbeat { .. } => {}
            Message::Ready { worker, version } => {
                if !io.ready.swap(true, Ordering::AcqRel) {
                    *lock(&io.ready_at) = Some(Instant::now());
                    tracing::info!(worker = %shared.name, %worker, %version,
                        load_ms = io.started.elapsed().as_millis() as u64, "worker ready");
                    if !io.done.load(Ordering::Acquire) && !shared.stopping() {
                        shared.set_state(State::Ready);
                    }
                    shared.queue_cv.notify_all();
                }
            }
            m @ (Message::Words { .. } | Message::Translated { .. } | Message::Error { .. }) => {
                match out.try_send(m) {
                    Ok(()) | Err(TrySendError::Disconnected(_)) => {}
                    Err(TrySendError::Full(_)) => {
                        shared.dropped_out.fetch_add(1, Ordering::Relaxed);
                    }
                }
            }
            m @ (Message::Translate { .. } | Message::Shutdown) => {
                soft_errors += 1;
                tracing::warn!(worker = %shared.name, "ignoring {m:?} from worker");
            }
        }
    }
}

fn write_stdin(shared: &Shared, io: &InstanceIo, stdin: ChildStdin) {
    let mut w = std::io::BufWriter::new(stdin);
    loop {
        let frame = {
            let mut q = lock(&shared.queue);
            loop {
                if io.done.load(Ordering::Acquire) {
                    return;
                }
                if io.ready.load(Ordering::Acquire)
                    && let Some(f) = q.frames.pop_front()
                {
                    break f;
                }
                if q.closed && q.frames.is_empty() {
                    // Dropping stdin gives the worker EOF.
                    return;
                }
                q = shared
                    .queue_cv
                    .wait_timeout(q, Duration::from_millis(100))
                    .map(|(g, _)| g)
                    .unwrap_or_else(|e| e.into_inner().0);
            }
        };
        let is_shutdown = matches!(frame, Frame::Message(Message::Shutdown));
        let res = match &frame {
            Frame::Message(m) => ipc::write_message(&mut w, m),
            Frame::Pcm { start_ms, samples } => ipc::write_pcm(&mut w, *start_ms, samples),
        };
        let res = res.and_then(|()| std::io::Write::flush(&mut w).map_err(FrameError::from));
        if let Err(e) = res {
            if !io.done.load(Ordering::Acquire) {
                io.fail(format!("cannot write to worker: {e}"));
            }
            return;
        }
        if is_shutdown {
            io.shutdown_sent.store(true, Ordering::Release);
            return;
        }
    }
}

fn read_stderr(name: &str, stderr: ChildStderr) {
    const MAX_LINE: u64 = 4096;
    let mut r = BufReader::new(stderr);
    let mut buf = Vec::new();
    loop {
        buf.clear();
        match r.by_ref().take(MAX_LINE).read_until(b'\n', &mut buf) {
            Ok(0) | Err(_) => return,
            Ok(_) => {
                let line = String::from_utf8_lossy(&buf);
                let line = line.trim_end();
                if !line.is_empty() {
                    tracing::info!(target: "multi::worker", worker = %name, "{line}");
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_doubles_to_cap() {
        let p = Policy::default();
        let ms: Vec<u128> = (0..8).map(|n| p.backoff(n).as_millis()).collect();
        assert_eq!(ms, [500, 1000, 2000, 4000, 8000, 10000, 10000, 10000]);
        assert_eq!(p.backoff(u32::MAX), p.backoff_max);
    }
}
