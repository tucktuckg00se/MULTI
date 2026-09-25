//! Helpers for the worker side of [`crate::ipc`]: framed writes to stdout that
//! are safe from several threads, and the heartbeat thread with a stall
//! watchdog.
//!
//! Workers send [`Message::Heartbeat`] every [`HEARTBEAT_INTERVAL`] from their
//! own thread, so a long model load or decode does not look like a hang. If a
//! piece of work marked with [`Heartbeat::busy`] runs longer than the stall
//! limit, heartbeats stop, and the supervisor restarts the worker.

use crate::ipc::{self, FrameError, Message};
use std::collections::BTreeMap;
use std::io::{self, Write};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

pub const HEARTBEAT_INTERVAL: Duration = Duration::from_millis(500);

/// Writes one message to stdout as a whole frame and flushes it. The stdout
/// lock is held for the frame, so threads never interleave partial frames.
pub fn send(msg: &Message) -> Result<(), FrameError> {
    let mut out = io::stdout().lock();
    ipc::write_message(&mut out, msg)?;
    out.flush()?;
    Ok(())
}

/// Heartbeat state shared between the worker's threads.
#[derive(Default)]
pub struct Heartbeat {
    stop: AtomicBool,
    paused: AtomicBool,
    next_id: AtomicU64,
    busy: Mutex<BTreeMap<u64, Instant>>,
}

/// Marks work in progress; dropping it ends the mark.
pub struct Busy<'a> {
    hb: &'a Heartbeat,
    id: u64,
}

impl Drop for Busy<'_> {
    fn drop(&mut self) {
        self.hb.lock_busy().remove(&self.id);
    }
}

impl Heartbeat {
    /// Starts the heartbeat thread. Heartbeats stop while any [`Busy`] mark is
    /// older than `stall_limit`.
    pub fn start(stall_limit: Duration) -> io::Result<(Arc<Self>, JoinHandle<()>)> {
        let hb = Arc::new(Self::default());
        let hb2 = Arc::clone(&hb);
        let handle = thread::Builder::new()
            .name("heartbeat".into())
            .spawn(move || hb2.run(stall_limit))?;
        Ok((hb, handle))
    }

    /// Marks the start of work that must finish within the stall limit.
    pub fn busy(&self) -> Busy<'_> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        self.lock_busy().insert(id, Instant::now());
        Busy { hb: self, id }
    }

    /// Stops heartbeats without ending the thread (used to simulate a hang).
    pub fn pause(&self) {
        self.paused.store(true, Ordering::Relaxed);
    }

    /// Ends the heartbeat thread within one tick.
    pub fn stop(&self) {
        self.stop.store(true, Ordering::Relaxed);
    }

    fn lock_busy(&self) -> std::sync::MutexGuard<'_, BTreeMap<u64, Instant>> {
        self.busy.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn stalled(&self, limit: Duration) -> bool {
        self.lock_busy().values().any(|t| t.elapsed() > limit)
    }

    fn run(&self, stall_limit: Duration) {
        const TICK: Duration = Duration::from_millis(50);
        let mut seq = 0u64;
        let mut next = Instant::now();
        let mut warned = false;
        while !self.stop.load(Ordering::Relaxed) {
            if Instant::now() >= next {
                next += HEARTBEAT_INTERVAL;
                if self.paused.load(Ordering::Relaxed) {
                    continue;
                }
                if self.stalled(stall_limit) {
                    if !warned {
                        eprintln!(
                            "worker stalled for over {stall_limit:?}; withholding heartbeats"
                        );
                        warned = true;
                    }
                    continue;
                }
                warned = false;
                if send(&Message::Heartbeat { seq }).is_err() {
                    // Parent gone; the main thread sees EOF on stdin and exits.
                    return;
                }
                seq += 1;
            }
            thread::sleep(TICK);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn busy_marks_stall_only_when_old() {
        let hb = Heartbeat::default();
        assert!(!hb.stalled(Duration::ZERO));
        let guard = hb.busy();
        thread::sleep(Duration::from_millis(5));
        assert!(hb.stalled(Duration::from_millis(1)));
        assert!(!hb.stalled(Duration::from_secs(10)));
        drop(guard);
        assert!(!hb.stalled(Duration::ZERO));
    }
}
