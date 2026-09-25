//! Approach (c), S2b: GStreamer's `tttocea708` driven per frame from the
//! video probe (no `cccombiner`), via `s2b_gst_cc::GstCc`.
//!
//! Same slot logic as [`crate::captions::OursCaptioner`]: each frame's display
//! slot is the previous slot plus the PTS step in whole frames, and
//! `GstCc::frame` is called once per slot in increasing order. Lane 0 is one
//! `tttocea708` (service 1 + CC1). With 2 lanes a second one (service 2 + CC3)
//! joins it through `cea708mux`. Each line starts a new row (the
//! `rstranscribe/final-transcript` event).

use std::collections::BTreeMap;
use std::sync::mpsc::Receiver;

use s2b_gst_cc::{GstCc, Input, TrackCfg};
use tracing::warn;

use crate::captions::Line;

const MAX_AHEAD: u64 = 32;
const REANCHOR: i64 = 90;

pub struct GstCaptioner {
    rx: Receiver<Line>,
    enc: Option<GstCc>,
    frame_ns: f64,
    last: Option<(i64, i64)>,
    next_idx: u64,
    pending: BTreeMap<u64, Vec<u8>>,
}

impl GstCaptioner {
    pub fn new(rx: Receiver<Line>, lanes: u8, fps: (i32, i32)) -> anyhow::Result<Self> {
        let lane = |svc: u32, ch: u32| TrackCfg::Cea708 {
            service: svc,
            cea608_channel: ch,
            mode: "roll-up".into(),
            roll_up_rows: 3,
            origin_row: -1,
            discard: vec![],
        };
        let mut tracks = vec![lane(1, 1)];
        if lanes > 1 {
            tracks.push(lane(2, 3));
        }
        let mut enc = GstCc::new(&tracks, fps)?;
        // Never block the video thread for long: a late caption buffer is
        // handed out on a later frame instead.
        enc.wait = std::time::Duration::from_millis(5);
        Ok(Self {
            rx,
            enc: Some(enc),
            frame_ns: 1e9 * f64::from(fps.1) / f64::from(fps.0),
            last: None,
            next_idx: 0,
            pending: BTreeMap::new(),
        })
    }

    /// `cc_data` for the frame at `pts` (ns). Never panics.
    pub fn meta_for(&mut self, pts: i64) -> Option<Vec<u8>> {
        let enc = self.enc.as_mut()?;
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
            while let Ok(line) = self.rx.try_recv() {
                let track = usize::from(line.lane.min(1));
                enc.carriage_return(track);
                if let Err(e) = enc.push(track, slot, 1, &Input::Text(line.text)) {
                    warn!(err = %e, "caption push failed");
                }
            }
            let (data, _lag) = enc.frame(slot);
            self.pending.insert(slot, data);
            self.next_idx += 1;
        }
        let data = self.pending.remove(&idx)?;
        let stale = idx.saturating_sub(MAX_AHEAD);
        self.pending = self.pending.split_off(&stale);
        (!data.is_empty()).then_some(data)
    }

    pub fn wait_us(&self) -> (u64, u64) {
        self.enc.as_ref().map_or((0, 0), |e| (e.stats.max_wait_us, e.stats.sum_wait_us / e.stats.frames.max(1)))
    }
}
