//! Caption stage: S2b's `GstCc` (one `tttocea708` per language lane, joined by
//! `cea708mux`), driven once per video frame from the output pad probe, as in
//! `s2_gst_pipe::gstcc` but with N lanes, per-lane pacing and a backlog cap.
//!
//! Lane i gets 708 service i+1; lane 0 also CC1 and lane 1 also CC3 (608 has
//! no room for more). Text is handed to a lane's encoder as soon as the encoder
//! is less than [`LEAD_FRAMES`] ahead of the current frame; until then it waits
//! in a per-lane queue. If queue plus encoder lead exceed [`MAX_BACKLOG_S`], the
//! oldest queued text is dropped and logged.
//!
//! Kept S2b workarounds: `GstCc::new` re-sets `roll-up-rows` once PLAYING
//! (bug 1), and `gap_when_behind` (default true) only sends a GAP to a lane
//! whose encoder is behind (bug 2).

use std::collections::{BTreeMap, VecDeque};
use std::sync::atomic::Ordering;
use std::sync::mpsc::{Receiver, Sender};

use s2_gst_pipe::stamps::wall_ns;
use s2b_gst_cc::{GstCc, Input, TrackCfg};
use tracing::warn;

const MAX_AHEAD: u64 = 32;
const REANCHOR: i64 = 90;
/// Hand text to an encoder only while it is at most this far ahead (frames).
const LEAD_FRAMES: u64 = 6;
/// Per-lane backlog cap (queued text plus encoder lead), seconds.
const MAX_BACKLOG_S: f64 = 6.0;
/// Rough caption bandwidth for the backlog estimate (608 is the bottleneck:
/// 2 bytes/frame minus control codes).
const CHARS_PER_S: f64 = 40.0;

/// A piece of caption text for one lane.
pub struct Line {
    pub lane: usize,
    pub text: String,
    /// Start a new roll-up row first.
    pub new_row: bool,
    /// Clause id (for the emit log).
    pub clause: u64,
    /// Wall ns when the text became ready.
    pub ready_ns: u64,
}

pub struct Captioner {
    rx: Receiver<Line>,
    enc: GstCc,
    lanes: usize,
    frame_ns: f64,
    fps: f64,
    last: Option<(i64, i64)>,
    next_idx: u64,
    pending: BTreeMap<u64, Vec<u8>>,
    queues: Vec<VecDeque<Line>>,
    /// Emit log lines (wall_ns, lane, clause, pts, wait, text).
    log: Option<Sender<String>>,
    pub dropped: Vec<u64>,
    pub pushed: Vec<u64>,
}

pub fn lane_track(i: usize) -> TrackCfg {
    TrackCfg::Cea708 {
        service: i as u32 + 1,
        cea608_channel: match i {
            0 => 1,
            1 => 3,
            _ => 0,
        },
        mode: "roll-up".into(),
        roll_up_rows: 3,
        origin_row: -1,
        discard: vec![],
    }
}

impl Captioner {
    pub fn new(rx: Receiver<Line>, lanes: usize, fps: (i32, i32), log: Option<Sender<String>>) -> anyhow::Result<Self> {
        let tracks: Vec<TrackCfg> = (0..lanes).map(lane_track).collect();
        let mut enc = GstCc::new(&tracks, fps)?;
        // Never block the video thread for long: a late caption buffer is
        // handed out on a later frame instead.
        enc.wait = std::time::Duration::from_millis(5);
        let fps_f = f64::from(fps.0) / f64::from(fps.1);
        Ok(Self {
            rx,
            enc,
            lanes,
            frame_ns: 1e9 / fps_f,
            fps: fps_f,
            last: None,
            next_idx: 0,
            pending: BTreeMap::new(),
            queues: (0..lanes).map(|_| VecDeque::new()).collect(),
            log,
            dropped: vec![0; lanes],
            pushed: vec![0; lanes],
        })
    }

    fn lead(&self, lane: usize, slot: u64) -> u64 {
        self.enc.track_hi.get(lane).map_or(0, |h| h.load(Ordering::Relaxed).saturating_sub(slot))
    }

    /// Feeds queued text into the encoders for `slot`.
    fn feed(&mut self, slot: u64, pts: i64) {
        while let Ok(line) = self.rx.try_recv() {
            if let Some(q) = self.queues.get_mut(line.lane) {
                q.push_back(line);
            }
        }
        for lane in 0..self.lanes {
            let lead = self.lead(lane, slot);
            // Backlog cap: drop the oldest queued text.
            loop {
                let q = &self.queues[lane];
                let chars: usize = q.iter().map(|l| l.text.chars().count()).sum();
                let backlog = lead as f64 / self.fps + chars as f64 / CHARS_PER_S;
                if backlog <= MAX_BACKLOG_S || q.is_empty() {
                    break;
                }
                if let Some(l) = self.queues[lane].pop_front() {
                    self.dropped[lane] += 1;
                    warn!(lane, backlog_s = format!("{backlog:.1}"), text = %l.text, "caption backlog cap: dropped oldest text");
                }
            }
            if lead > LEAD_FRAMES {
                continue;
            }
            let Some(line) = self.queues[lane].pop_front() else { continue };
            if line.new_row {
                self.enc.carriage_return(lane);
            }
            if let Err(e) = self.enc.push(lane, slot, 1, &Input::Text(line.text.clone())) {
                warn!(lane, err = %e, "caption push failed");
                continue;
            }
            self.pushed[lane] += 1;
            if let Some(tx) = &self.log {
                let now = wall_ns();
                let wait_ms = now.saturating_sub(line.ready_ns) as f64 / 1e6;
                let _ = tx.send(format!("{now}\t{lane}\t{}\t{pts}\t{wait_ms:.1}\t{}", line.clause, line.text));
            }
        }
    }

    /// `cc_data` for the frame at `pts` (ns). Never panics.
    pub fn meta_for(&mut self, pts: i64) -> Option<Vec<u8>> {
        let next = self.next_idx as i64;
        let mut idx = match self.last {
            Some((lp, li)) => li + ((pts - lp) as f64 / self.frame_ns).round() as i64,
            None => next,
        };
        if idx >= next + REANCHOR || idx < next - REANCHOR {
            self.pending.clear();
            idx = next;
        }
        self.last = Some((pts, idx));
        if idx < 0 {
            return None;
        }
        let idx = idx as u64;
        while self.next_idx <= idx {
            let slot = self.next_idx;
            self.feed(slot, pts);
            let (data, _lag) = self.enc.frame(slot);
            self.pending.insert(slot, data);
            self.next_idx += 1;
        }
        let data = self.pending.remove(&idx)?;
        let stale = idx.saturating_sub(MAX_AHEAD);
        self.pending = self.pending.split_off(&stale);
        (!data.is_empty()).then_some(data)
    }

    pub fn wait_us(&self) -> (u64, u64) {
        let s = &self.enc.stats;
        (s.max_wait_us, s.sum_wait_us / s.frames.max(1))
    }

    pub fn bus_messages(&self) -> Vec<String> {
        self.enc.bus_messages()
    }
}
