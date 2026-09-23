//! Per-frame wallclock instrumentation, keyed by PTS.
//!
//! Stages (all wallclock ns since the Unix epoch, so the black-box `tap`
//! process can be compared against them):
//! - `entry`: video buffer leaves `tsdemux` in the input pipeline (keyed by input PTS)
//! - `bridge`: pushed into the output pipeline's `appsrc` (input PTS -> output PTS)
//! - `cc_in`: enters the caption stage (cccombiner sink / probe point)
//! - `cc_out`: leaves the caption stage (h26xccinserter src)
//! - `mux_in`: reaches `mpegtsmux`'s video sink pad -> the row is written
//!
//! Rows go through a bounded channel to a writer thread; if it falls behind,
//! rows are dropped rather than blocking a streaming thread.

use std::collections::HashMap;
use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{SyncSender, TrySendError, sync_channel};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

pub fn wall_ns() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_nanos() as u64).unwrap_or(0)
}

#[derive(Clone, Copy, Default)]
struct Row {
    session: u64,
    in_pts: i64,
    entry: u64,
    bridge: u64,
    bridge_rt: i64,
    cc_in: u64,
    cc_out: u64,
}

/// Map size cap: anything older than this many in-flight frames is stale.
const MAX_INFLIGHT: usize = 600;

#[derive(Default)]
pub struct Counters {
    pub video_in: AtomicU64,
    pub audio_in: AtomicU64,
    pub video_out: AtomicU64,
    pub sessions: AtomicU64,
    pub input_restarts: AtomicU64,
    pub input_errors: AtomicU64,
    pub output_errors: AtomicU64,
    pub bridge_drops: AtomicU64,
    pub rows_dropped: AtomicU64,
    pub cc_frames: AtomicU64,
    /// Last (out_pts - running_time) at the bridge in µs, for drift.
    pub last_skew_us: AtomicU64,
}

pub struct Stamps {
    entry: Mutex<HashMap<i64, u64>>,
    rows: Mutex<HashMap<i64, Row>>,
    tx: Option<SyncSender<String>>,
    pub counters: Counters,
}

impl Stamps {
    pub fn new(csv: Option<&Path>) -> anyhow::Result<Arc<Self>> {
        let tx = match csv {
            Some(p) => {
                let mut w = BufWriter::new(File::create(p)?);
                writeln!(w, "session,in_pts_ns,out_pts_ns,entry_ns,bridge_ns,bridge_rt_ns,cc_in_ns,cc_out_ns,mux_in_ns,delay_us,cc_stage_us")?;
                let (tx, rx) = sync_channel::<String>(8192);
                std::thread::Builder::new().name("csv".into()).spawn(move || {
                    let mut n = 0u64;
                    while let Ok(line) = rx.recv() {
                        if writeln!(w, "{line}").is_err() {
                            break;
                        }
                        n += 1;
                        if n.is_multiple_of(30) {
                            let _ = w.flush();
                        }
                    }
                    let _ = w.flush();
                })?;
                Some(tx)
            }
            None => None,
        };
        Ok(Arc::new(Self {
            entry: Mutex::new(HashMap::new()),
            rows: Mutex::new(HashMap::new()),
            tx,
            counters: Counters::default(),
        }))
    }

    pub fn on_entry(&self, in_pts: i64) {
        self.counters.video_in.fetch_add(1, Ordering::Relaxed);
        if let Ok(mut m) = self.entry.lock() {
            if m.len() > MAX_INFLIGHT {
                m.clear();
            }
            m.insert(in_pts, wall_ns());
        }
    }

    pub fn on_bridge(&self, session: u64, in_pts: i64, out_pts: i64, rt: i64) {
        let entry = self.entry.lock().ok().and_then(|mut m| m.remove(&in_pts)).unwrap_or(0);
        let skew_us = (out_pts - rt) / 1000;
        self.counters.last_skew_us.store(skew_us as u64, Ordering::Relaxed);
        if let Ok(mut m) = self.rows.lock() {
            if m.len() > MAX_INFLIGHT {
                m.clear();
            }
            m.insert(out_pts, Row { session, in_pts, entry, bridge: wall_ns(), bridge_rt: rt, ..Row::default() });
        }
    }

    pub fn on_cc_in(&self, out_pts: i64) {
        if let Ok(mut m) = self.rows.lock()
            && let Some(r) = m.get_mut(&out_pts)
        {
            r.cc_in = wall_ns();
        }
    }

    pub fn on_cc_out(&self, out_pts: i64) {
        if let Ok(mut m) = self.rows.lock()
            && let Some(r) = m.get_mut(&out_pts)
        {
            r.cc_out = wall_ns();
        }
    }

    pub fn on_mux_in(&self, out_pts: i64) {
        self.counters.video_out.fetch_add(1, Ordering::Relaxed);
        let now = wall_ns();
        let Some(r) = self.rows.lock().ok().and_then(|mut m| m.remove(&out_pts)) else {
            return;
        };
        let Some(tx) = &self.tx else { return };
        let delay_us = if r.entry > 0 { (now as i64 - r.entry as i64) / 1000 } else { -1 };
        let cc_us = if r.cc_in > 0 && r.cc_out > 0 { (r.cc_out as i64 - r.cc_in as i64) / 1000 } else { -1 };
        let line = format!(
            "{},{},{},{},{},{},{},{},{},{},{}",
            r.session, r.in_pts, out_pts, r.entry, r.bridge, r.bridge_rt, r.cc_in, r.cc_out, now, delay_us, cc_us
        );
        if let Err(TrySendError::Full(_)) = tx.try_send(line) {
            self.counters.rows_dropped.fetch_add(1, Ordering::Relaxed);
        }
    }
}

/// Resident set size in kB from /proc (Linux only; 0 elsewhere).
pub fn rss_kb() -> u64 {
    std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|s| {
            s.lines()
                .find(|l| l.starts_with("VmRSS:"))
                .and_then(|l| l.split_whitespace().nth(1).and_then(|v| v.parse().ok()))
        })
        .unwrap_or(0)
}
