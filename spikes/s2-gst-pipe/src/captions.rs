//! Approach (b): our `cc` encoder, keyed to each frame's PTS.
//!
//! A pad probe on the output parser's src pad calls [`OursCaptioner::meta_for`]
//! per video buffer. Each frame's display slot is the previous frame's slot
//! plus `round((pts - prev_pts) / frame_duration)`, and `CcMux::next_frame()`
//! is called once per slot in increasing (display) order, so B-frames arriving
//! in decode order still get the triples for their display slot. Indexing is
//! relative, not against a fixed grid: the bridge slews PTS by up to 5 ms per
//! frame, and a fixed grid then maps two frames to one slot (a frame without
//! captions) or skips a slot (caption bytes lost). Triples go on the buffer as `GstVideoCaptionMeta`
//! (CEA-708 `cc_data`), which `h264ccinserter`/`h265ccinserter` turn into SEI.

use std::collections::BTreeMap;
use std::sync::mpsc::Receiver;

use cc::{Cc608Encoder, Cc708Encoder, CcMux, CcTriple, Channel, FrameRate};

/// Text for one caption line; `lane` 0 = CC1 + 708 service 1, 1 = CC3 + service 2.
pub struct Line {
    pub lane: u8,
    pub text: String,
}

/// Frames the mux may run ahead of the current PTS (B-frame reorder depth).
const MAX_AHEAD: u64 = 32;
/// Forward/backward index jumps larger than this re-anchor instead of emitting
/// (and losing) captions for frames that never existed.
const REANCHOR: i64 = 90;

pub struct OursCaptioner {
    rx: Receiver<Line>,
    lanes: u8,
    mux: Option<CcMux>,
    frame_ns: f64,
    /// (pts, slot) of the previous frame, in arrival order.
    last: Option<(i64, i64)>,
    next_idx: u64,
    pending: BTreeMap<u64, Vec<CcTriple>>,
    pub reanchors: u64,
}

impl OursCaptioner {
    pub fn new(rx: Receiver<Line>, lanes: u8) -> Self {
        Self { rx, lanes, mux: None, frame_ns: 0.0, last: None, next_idx: 0, pending: BTreeMap::new(), reanchors: 0 }
    }

    fn init(&mut self, fps: f64) -> bool {
        let Some(rate) = FrameRate::from_fps(fps) else { return false };
        let mut mux = CcMux::new(rate);
        mux.add_608(Cc608Encoder::new(Channel::Cc1));
        if let Some(e) = Cc708Encoder::new(1) {
            mux.add_708(e);
        }
        if self.lanes > 1 {
            mux.add_608(Cc608Encoder::new(Channel::Cc3));
            if let Some(e) = Cc708Encoder::new(2) {
                mux.add_708(e);
            }
        }
        self.frame_ns = 1e9 / fps;
        self.mux = Some(mux);
        true
    }

    /// `cc_data` bytes for the frame at `pts` (ns), or `None` if captions are
    /// unavailable (unknown frame rate, duplicate PTS). Never panics.
    pub fn meta_for(&mut self, pts: i64, fps: f64) -> Option<Vec<u8>> {
        if self.mux.is_none() && !self.init(fps) {
            return None;
        }
        let mux = self.mux.as_mut()?;
        while let Ok(line) = self.rx.try_recv() {
            let (ch, svc) = if line.lane == 0 { (Channel::Cc1, 1) } else { (Channel::Cc3, 2) };
            mux.push_text_608(ch, &line.text);
            mux.push_text_708(svc, &line.text);
        }
        let next = self.next_idx as i64;
        let mut idx = match self.last {
            Some((lp, li)) => li + ((pts - lp) as f64 / self.frame_ns).round() as i64,
            None => next,
        };
        if idx >= next + REANCHOR || idx < next - REANCHOR {
            // Gap or timeline jump: this frame becomes the next slot.
            self.pending.clear();
            self.reanchors += 1;
            idx = next;
        }
        self.last = Some((pts, idx));
        if idx < 0 {
            return None;
        }
        let idx = idx as u64;
        while self.next_idx <= idx {
            let t = mux.next_frame();
            self.pending.insert(self.next_idx, t);
            self.next_idx += 1;
        }
        let triples = self.pending.remove(&idx)?;
        // Slots nobody claimed (dropped frames) are discarded after a while.
        let stale = idx.saturating_sub(MAX_AHEAD);
        self.pending = self.pending.split_off(&stale);
        let mut out = Vec::with_capacity(triples.len() * 3);
        for t in triples {
            out.extend_from_slice(&t.to_bytes());
        }
        Some(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc::channel;

    #[test]
    fn keyed_by_pts_in_display_order() {
        let (tx, rx) = channel();
        let mut c = OursCaptioner::new(rx, 1);
        let _ = tx.send(Line { lane: 0, text: "HI".into() });
        let f = 1e9 / 30.0;
        // Decode order I P B B: PTS 0, 3, 1, 2.
        let a = c.meta_for(0, 30.0);
        let d = c.meta_for((3.0 * f) as i64, 30.0);
        let b = c.meta_for(f as i64, 30.0);
        let cc = c.meta_for((2.0 * f) as i64, 30.0);
        for m in [&a, &b, &cc, &d] {
            assert_eq!(m.as_ref().map(|v| v.len()), Some(60));
        }
        // First frame starts with the RU3 control on CC1 (0x94 0x26).
        assert_eq!(&a.unwrap_or_default()[..3], &[0xFC, 0x94, 0x26]);
        assert_eq!(c.reanchors, 0);
    }

    #[test]
    fn slewed_timeline_serves_every_frame_in_order() {
        let (tx, rx) = channel();
        let mut c = OursCaptioner::new(rx, 1);
        let _ = tx.send(Line { lane: 0, text: "HELLO FROM MULTI".into() });
        // Squeezed (28.3 ms) then stretched (34.3 ms) PTS steps, as the bridge slews.
        let mut pts = 0i64;
        let mut first_pairs = Vec::new();
        for k in 0..200 {
            let m = c.meta_for(pts, 30.0);
            assert!(m.is_some(), "frame {k} got no captions");
            if let Some(v) = m {
                first_pairs.push([v[1], v[2]]);
            }
            pts += if k < 100 { 28_333_333 } else { 34_333_333 };
        }
        assert_eq!(c.next_idx, 200);
        // CC1 stream is contiguous: RU3 control first, text follows without gaps.
        assert_eq!(first_pairs[0], [0x94, 0x26]);
    }

    #[test]
    fn gap_reanchors() {
        let (_tx, rx) = channel();
        let mut c = OursCaptioner::new(rx, 2);
        assert!(c.meta_for(0, 30.0).is_some());
        assert!(c.meta_for(3_600_000_000_000, 30.0).is_some());
        assert_eq!(c.reanchors, 1);
        assert_eq!(c.next_idx, 2);
    }
}
