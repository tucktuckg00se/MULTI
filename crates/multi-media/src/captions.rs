//! Caption lanes: one encoder track per configured language, driven once per
//! video frame from the output pipeline's pad probe (S6's captioner).
//!
//! Text for a lane waits in a per-lane queue and is handed to the lane's
//! encoder once the encoder is less than [`LEAD_FRAMES`] ahead of the current
//! frame. If queued text plus encoder lead exceed [`MAX_BACKLOG_S`], the
//! oldest queued text is dropped and counted. Callers push text through a
//! [`CaptionHandle`], which never blocks.

use std::collections::{BTreeMap, VecDeque};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::sync::mpsc::{Receiver, SyncSender, TrySendError};
use std::time::{Duration, Instant};

use anyhow::{Result, bail};
use multi_core::config::{CaptionMode, Captions, Cc608, Language};
use tracing::{info, warn};

use crate::gstcc::{GstCc, TrackCfg};
use crate::stats::{LaneCounters, LaneStats};

/// Keep this many frames of encoder output around for PTS that jump back.
const MAX_AHEAD: u64 = 32;
/// A PTS this many frames away from the expected one re-anchors the grid.
const REANCHOR: i64 = 90;
/// Hand text to an encoder only while it is at most this far ahead (frames).
const LEAD_FRAMES: u64 = 6;
/// Per-lane backlog cap (queued text plus encoder lead), seconds.
const MAX_BACKLOG_S: f64 = 6.0;
/// Rough caption bandwidth for the backlog estimate (608 is the bottleneck).
const CHARS_PER_S: f64 = 40.0;
/// Text waiting between callers and the caption stage.
const CHANNEL_CAP: usize = 1024;
/// Frames in a row without encoder output before the encoder is rebuilt.
const MAX_TIMEOUTS_IN_ROW: u64 = 150;
/// Wait between encoder rebuild attempts.
const REBUILD_DELAY: Duration = Duration::from_secs(5);
/// Frame rate used when the video caps carry none.
pub const FALLBACK_FPS: (i32, i32) = (30, 1);

/// One configured caption lane.
#[derive(Clone, Debug, PartialEq)]
pub struct LaneSpec {
    pub lang: String,
    pub track: TrackCfg,
}

/// Maps the configured languages to encoder tracks. A language with a 708
/// service gets a `tttocea708` track (plus 608 compatibility bytes on its
/// channel); a 608-only language gets a `tttocea608` track. CC2/CC4 are not
/// supported yet (M1 WP4).
pub fn lane_specs(langs: &[Language], c: &Captions) -> Result<Vec<LaneSpec>> {
    let rows = u32::from(c.rows.clamp(2, 4));
    let mut out = Vec::new();
    for l in langs {
        let ch = match l.cc608 {
            None => 0,
            Some(Cc608::Cc1) => 1,
            Some(Cc608::Cc3) => 3,
            Some(other) => bail!(
                "language {}: {other:?} is not supported yet (only CC1 and CC3)",
                l.code
            ),
        };
        let track = match l.cea708_service {
            Some(svc) => TrackCfg::Cea708 {
                service: u32::from(svc),
                cea608_channel: ch,
                mode: match c.mode {
                    CaptionMode::RollUp => "roll-up",
                    CaptionMode::PopOn => "pop-on",
                    CaptionMode::PaintOn => "paint-on",
                }
                .into(),
                roll_up_rows: rows,
            },
            None if ch > 0 => TrackCfg::Cea608 {
                field: u8::from(ch == 3),
                mode: match c.mode {
                    CaptionMode::RollUp => format!("roll-up{rows}"),
                    CaptionMode::PopOn => "pop-on".into(),
                    CaptionMode::PaintOn => "paint-on".into(),
                },
            },
            None => bail!("language {} has no 608 channel or 708 service", l.code),
        };
        out.push(LaneSpec {
            lang: l.code.clone(),
            track,
        });
    }
    if out.is_empty() {
        bail!("no caption languages");
    }
    Ok(out)
}

struct Line {
    lane: usize,
    text: String,
    new_row: bool,
    /// When the text may be shown (arrival + `captions.offset_ms`).
    due: Instant,
}

/// Pushes caption text into the lanes. Cheap to clone; never blocks.
#[derive(Clone)]
pub struct CaptionHandle {
    tx: SyncSender<Line>,
    langs: Arc<Vec<String>>,
    counters: Arc<Vec<LaneCounters>>,
    offset: Duration,
}

impl CaptionHandle {
    /// Queues `text` for language `lang`. `new_row` starts a new roll-up row
    /// first. Returns false if the language has no lane or the text was
    /// dropped because the caption stage is not keeping up.
    pub fn push(&self, lang: &str, text: &str, new_row: bool) -> bool {
        let Some(lane) = self.langs.iter().position(|l| l == lang) else {
            return false;
        };
        let text = text.trim();
        if text.is_empty() {
            return true;
        }
        let line = Line {
            lane,
            // No leading space: in roll-up, tttocea708 already puts one
            // between consecutive text buffers (S6 gotcha 3).
            text: text.to_string(),
            new_row,
            due: Instant::now() + self.offset,
        };
        match self.tx.try_send(line) {
            Ok(()) => true,
            Err(TrySendError::Full(_)) | Err(TrySendError::Disconnected(_)) => {
                if let Some(c) = self.counters.get(lane) {
                    c.dropped.fetch_add(1, Ordering::Relaxed);
                }
                false
            }
        }
    }

    /// The configured languages, in lane order.
    pub fn languages(&self) -> &[String] {
        &self.langs
    }
}

pub(crate) fn lane_stats(langs: &[String], counters: &[LaneCounters]) -> Vec<LaneStats> {
    langs
        .iter()
        .zip(counters)
        .map(|(l, c)| LaneStats {
            lang: l.clone(),
            pushed: c.pushed.load(Ordering::Relaxed),
            dropped: c.dropped.load(Ordering::Relaxed),
            queued: c.queued.load(Ordering::Relaxed),
        })
        .collect()
}

/// The caption stage, owned by the output pipeline's video probe.
pub(crate) struct Captioner {
    rx: Receiver<Line>,
    specs: Vec<LaneSpec>,
    counters: Arc<Vec<LaneCounters>>,
    /// Frames each text piece may occupy (pop-on/paint-on: display time).
    hold_ms: u32,
    roll_up: bool,
    enc: Option<GstCc>,
    retry_at: Option<Instant>,
    frame_ns: f64,
    last: Option<(i64, i64)>,
    next_idx: u64,
    pending: BTreeMap<u64, Vec<u8>>,
    queues: Vec<VecDeque<Line>>,
    /// Errors seen (rebuilds), for stats.
    pub errors: u64,
}

impl Captioner {
    pub fn new(specs: Vec<LaneSpec>, c: &Captions) -> (Self, CaptionHandle) {
        let (tx, rx) = std::sync::mpsc::sync_channel(CHANNEL_CAP);
        let counters: Arc<Vec<LaneCounters>> =
            Arc::new(specs.iter().map(|_| LaneCounters::default()).collect());
        if c.offset_ms < 0 {
            warn!(
                offset_ms = c.offset_ms,
                "negative captions.offset_ms needs video delay, which is not built yet; using 0"
            );
        }
        let handle = CaptionHandle {
            tx,
            langs: Arc::new(specs.iter().map(|s| s.lang.clone()).collect()),
            counters: counters.clone(),
            offset: Duration::from_millis(u64::try_from(c.offset_ms).unwrap_or(0)),
        };
        let n = specs.len();
        let cap = Self {
            rx,
            specs,
            counters,
            hold_ms: c.clear_after_ms,
            roll_up: c.mode == CaptionMode::RollUp,
            enc: None,
            retry_at: None,
            frame_ns: 1e9 / 30.0,
            last: None,
            next_idx: 0,
            pending: BTreeMap::new(),
            queues: (0..n).map(|_| VecDeque::new()).collect(),
            errors: 0,
        };
        (cap, handle)
    }

    pub fn counters(&self) -> Arc<Vec<LaneCounters>> {
        self.counters.clone()
    }

    /// Builds the encoder for `fps` if there is none or the rate changed.
    fn ensure_encoder(&mut self, fps: (i32, i32)) -> bool {
        if self.enc.as_ref().is_some_and(|e| e.fps() == fps) {
            return true;
        }
        if self.retry_at.is_some_and(|t| Instant::now() < t) {
            return false;
        }
        self.enc = None;
        let tracks: Vec<TrackCfg> = self.specs.iter().map(|s| s.track.clone()).collect();
        match GstCc::new(&tracks, fps) {
            Ok(e) => {
                info!(
                    fps = format!("{}/{}", fps.0, fps.1),
                    lanes = tracks.len(),
                    "caption encoder started"
                );
                self.frame_ns = 1e9 * f64::from(fps.1) / f64::from(fps.0);
                self.last = None;
                self.next_idx = 0;
                self.pending.clear();
                self.enc = Some(e);
                self.retry_at = None;
                true
            }
            Err(e) => {
                warn!(err = %format!("{e:#}"), "caption encoder failed to start; video continues without captions");
                self.errors += 1;
                self.retry_at = Some(Instant::now() + REBUILD_DELAY);
                false
            }
        }
    }

    /// Drops the encoder so it is rebuilt (after a delay) on a later frame.
    fn fail(&mut self, why: &str) {
        warn!(why, "caption encoder failed; rebuilding it");
        self.errors += 1;
        self.enc = None;
        self.retry_at = Some(Instant::now() + REBUILD_DELAY);
    }

    /// Checks the encoder's bus; logs warnings and rebuilds on errors.
    pub fn poll_bus(&mut self) {
        let Some(enc) = &self.enc else { return };
        let mut error = None;
        for m in enc.bus_messages() {
            match m {
                Ok(w) => warn!(msg = %w, "caption encoder warning"),
                Err(e) => error = Some(e),
            }
        }
        if let Some(e) = error {
            self.fail(&e);
        }
    }

    fn hold_frames(&self) -> u64 {
        if self.roll_up {
            1
        } else {
            (f64::from(self.hold_ms) * 1e6 / self.frame_ns)
                .round()
                .max(1.0) as u64
        }
    }

    /// Feeds queued text into the encoders for frame `slot`.
    fn feed(&mut self, slot: u64) {
        while let Ok(line) = self.rx.try_recv() {
            if let Some(q) = self.queues.get_mut(line.lane) {
                q.push_back(line);
            }
        }
        let fps = 1e9 / self.frame_ns;
        let hold = self.hold_frames();
        let now = Instant::now();
        for lane in 0..self.queues.len() {
            let Some(enc) = self.enc.as_mut() else { return };
            let lead = enc.lead(lane, slot);
            let (Some(q), Some(ctr)) = (self.queues.get_mut(lane), self.counters.get(lane)) else {
                continue;
            };
            // Backlog cap: drop the oldest queued text.
            loop {
                let chars: usize = q.iter().map(|l| l.text.chars().count()).sum();
                let backlog = lead as f64 / fps + chars as f64 / CHARS_PER_S;
                if backlog <= MAX_BACKLOG_S {
                    break;
                }
                let Some(l) = q.pop_front() else { break };
                ctr.dropped.fetch_add(1, Ordering::Relaxed);
                warn!(lane, backlog_s = format!("{backlog:.1}"), text = %l.text, "caption backlog cap: dropped oldest text");
            }
            if lead <= LEAD_FRAMES
                && q.front().is_some_and(|l| l.due <= now)
                && let Some(line) = q.pop_front()
            {
                if line.new_row && self.roll_up {
                    enc.carriage_return(lane);
                }
                match enc.push(lane, slot, hold, &line.text) {
                    Ok(()) => {
                        ctr.pushed.fetch_add(1, Ordering::Relaxed);
                    }
                    Err(e) => {
                        ctr.dropped.fetch_add(1, Ordering::Relaxed);
                        warn!(lane, err = %e, "caption push failed");
                    }
                }
            }
            ctr.queued.store(q.len() as u64, Ordering::Relaxed);
        }
    }

    /// `cc_data` for the frame at `pts` (ns). Never panics; `None` when there
    /// is nothing to attach.
    pub fn meta_for(&mut self, pts: i64, fps: (i32, i32)) -> Option<Vec<u8>> {
        if !self.ensure_encoder(fps) {
            // Keep the channel drained so callers see drops, not a stall.
            self.feed_without_encoder();
            return None;
        }
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
            self.feed(slot);
            let enc = self.enc.as_mut()?;
            let data = enc.frame(slot);
            let stuck = enc.timeouts_in_row > MAX_TIMEOUTS_IN_ROW;
            self.pending.insert(slot, data);
            self.next_idx += 1;
            if stuck {
                self.fail("no output from the caption encoder");
                return None;
            }
        }
        let data = self.pending.remove(&idx)?;
        let stale = idx.saturating_sub(MAX_AHEAD);
        self.pending = self.pending.split_off(&stale);
        (!data.is_empty()).then_some(data)
    }

    fn feed_without_encoder(&mut self) {
        while let Ok(line) = self.rx.try_recv() {
            if let Some(c) = self.counters.get(line.lane) {
                c.dropped.fetch_add(1, Ordering::Relaxed);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;
    use multi_core::config::default_languages;

    #[test]
    fn default_layout() {
        let specs = lane_specs(&default_languages(), &Captions::default()).unwrap();
        let tracks: Vec<_> = specs
            .iter()
            .map(|s| (s.lang.as_str(), s.track.clone()))
            .collect();
        let t708 = |service, ch| TrackCfg::Cea708 {
            service,
            cea608_channel: ch,
            mode: "roll-up".into(),
            roll_up_rows: 3,
        };
        assert_eq!(
            tracks,
            vec![
                ("en", t708(1, 1)),
                ("es", t708(2, 3)),
                ("fr", t708(3, 0)),
                ("de", t708(4, 0))
            ]
        );
    }

    #[test]
    fn pop_on_608_only_and_unsupported_channels() {
        let mut langs = default_languages();
        langs[1].cea708_service = None;
        let c = Captions {
            mode: CaptionMode::PopOn,
            ..Captions::default()
        };
        let specs = lane_specs(&langs, &c).unwrap();
        assert_eq!(
            specs[1].track,
            TrackCfg::Cea608 {
                field: 1,
                mode: "pop-on".into()
            }
        );
        langs[1].cc608 = Some(Cc608::Cc2);
        assert!(lane_specs(&langs, &c).is_err());
    }

    #[test]
    fn handle_routes_by_language_and_never_blocks() {
        let specs = lane_specs(&default_languages(), &Captions::default()).unwrap();
        let (cap, h) = Captioner::new(specs, &Captions::default());
        assert!(h.push("es", "hola", true));
        assert!(!h.push("xx", "nope", false));
        for _ in 0..CHANNEL_CAP + 10 {
            h.push("en", "word", false);
        }
        let st = lane_stats(h.languages(), &cap.counters);
        assert_eq!(st[0].dropped, 11);
        let line = cap.rx.try_recv().unwrap();
        assert_eq!(
            (line.lane, line.text.as_str(), line.new_row),
            (1, "hola", true)
        );
    }

    #[test]
    fn frames_carry_cc_data_and_text() {
        if gst::init().is_err() || gst::ElementFactory::find("tttocea708").is_none() {
            return;
        }
        let specs = lane_specs(&default_languages(), &Captions::default()).unwrap();
        let (mut cap, h) = Captioner::new(specs, &Captions::default());
        h.push("en", "HELLO", true);
        h.push("es", "HOLA", true);
        let mut with_data = 0;
        for i in 0..90i64 {
            if cap.meta_for(i * 33_333_333, (30, 1)).is_some() {
                with_data += 1;
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        assert!(with_data > 60, "{with_data} of 90 frames had cc_data");
        let st = lane_stats(h.languages(), &cap.counters);
        assert_eq!(st[0].pushed, 1);
        assert_eq!(st[1].pushed, 1);
    }
}
