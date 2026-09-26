//! Input -> output bridge (ported from M0 spike S2): re-stamps buffers so
//! the output timeline is continuous and monotonic across source restarts and
//! PTS resets, and locked to the output pipeline's running time.
//!
//! Why the lock matters: `mpegtsmux` is a live aggregator. A buffer whose
//! running time is still in the future is held until every other pad has data
//! or the deadline passes. Sources send audio in ~180 ms PES that arrive after
//! the video they belong with, so video stamped even slightly "early" waits
//! for audio (a 67–200 ms sawtooth). Video stamped at or just behind "now"
//! goes straight through.
//!
//! - Each run of contiguous input timestamps is a *session* with one
//!   input->output offset shared by audio and video (A/V sync is the source's).
//!   A new session starts when the input is rebuilt, or when video PTS jumps
//!   back > 1 s or forward > 3 s (source restarted at PTS 0, long gap).
//!   Only video opens a session; audio before it is dropped.
//! - For the first [`HOLD_NS`] of a session buffers are held while the offset
//!   is taken as the *earliest* arrival seen (min of running_time - PTS): the
//!   demuxer releases the first frames in a burst. The session never starts
//!   before the previous one's last output + 40 ms, so output PTS only move
//!   forward, and never before output PTS 40 ms.
//! - After that, a video frame that would be early pulls the offset back, and
//!   one more than [`LATE_TOL_NS`] late pushes it forward, by at most
//!   [`SLEW_EARLY_NS`] / [`SLEW_NS`] per frame (clock drift, startup bursts).
//! - Monotonicity is enforced on video DTS, not PTS (B-frames, S2 gotcha 4).

use std::sync::{Mutex, MutexGuard};

use gst::prelude::*;
use tracing::{debug, info};

use crate::stats::{Counters, inc};
use std::sync::Arc;

const SEC: i64 = 1_000_000_000;
/// Startup hold per session.
pub const HOLD_NS: i64 = 500_000_000;
/// Max offset correction per video frame when frames are late (1 ms/frame =
/// 30 ms/s at 30 fps).
pub const SLEW_NS: i64 = 1_000_000;
/// Max correction per frame when frames are early. Earliness is paid for in
/// muxer wait (latency), so it is corrected faster: 5 ms/frame shortens the
/// PTS step to >= 28 ms, still monotonic.
pub const SLEW_EARLY_NS: i64 = 5_000_000;

/// Lateness tolerated before the slew pushes the offset forward.
pub const LATE_TOL_NS: i64 = 100_000_000;

pub const VIDEO: usize = 0;
pub const AUDIO: usize = 1;

#[derive(Default)]
struct Rebase {
    session: u64,
    offset: i64,
    ref_in: i64,
    last_in: [Option<i64>; 2],
    seen: [bool; 2],
    last_out: [i64; 2],
    force_new: bool,
    /// Some(until_rt) while the session's startup hold is running.
    hold_until: Option<i64>,
    min_off: i64,
    first_in: i64,
    slewed_ns: i64,
    last_dts: i64,
}

#[derive(Debug, PartialEq, Eq)]
enum Verdict {
    Drop,
    /// Buffer belongs to the current session (possibly one just opened).
    Session {
        new: bool,
    },
}

impl Rebase {
    fn classify(&mut self, s: usize, in_pts: i64, now_rt: i64) -> Verdict {
        let fits = !self.force_new
            && match (self.seen[s], self.last_in[s]) {
                (true, Some(l)) => in_pts >= l - SEC && in_pts <= l + 3 * SEC,
                _ => self.session > 0 && (in_pts - self.ref_in).abs() <= 3 * SEC,
            };
        let mut new = false;
        if !fits {
            if s == AUDIO {
                self.force_new = true;
                return Verdict::Drop;
            }
            self.session += 1;
            self.seen = [false; 2];
            self.last_in = [None; 2];
            self.force_new = false;
            self.hold_until = Some(now_rt + HOLD_NS);
            self.min_off = now_rt - in_pts;
            self.first_in = in_pts;
            new = true;
        }
        if s == VIDEO && self.hold_until.is_some() {
            debug!(in_pts, now_rt, arrival = now_rt - in_pts, "hold");
            self.min_off = self.min_off.min(now_rt - in_pts);
        }
        self.seen[s] = true;
        self.last_in[s] = Some(in_pts);
        self.ref_in = in_pts;
        Verdict::Session { new }
    }

    /// Ends the hold: fixes the session offset.
    fn finish_hold(&mut self) {
        let floor = self.last_out[VIDEO].max(self.last_out[AUDIO]) + SEC / 25 - self.first_in;
        self.offset = self.min_off.max(floor);
        self.hold_until = None;
    }

    /// Output (PTS, DTS) for a buffer of the current (non-holding) session.
    /// `dts_shift` = input DTS - input PTS (≤ 0 with B-frames).
    ///
    /// Lock and monotonicity work on the *decode* timestamp: that is what the
    /// muxer schedules on, and with B-frames PTS legitimately go backwards in
    /// decode order.
    fn out_ts(&mut self, s: usize, in_pts: i64, dts_shift: i64, now_rt: i64) -> (i64, i64) {
        let mut out = in_pts + self.offset;
        if s == VIDEO {
            let early = out + dts_shift - now_rt;
            if early > 0 {
                let d = early.min(SLEW_EARLY_NS);
                self.offset -= d;
                self.slewed_ns += d;
                out -= d;
            } else if early < -LATE_TOL_NS {
                // Behind "now" (e.g. the floor after a startup burst): catch up
                // slowly; lateness costs nothing in the muxer, earliness does.
                let d = (-early - LATE_TOL_NS).min(SLEW_NS);
                self.offset += d;
                self.slewed_ns += d;
                out += d;
            }
            // Strictly increasing video DTS whatever happens upstream: shift
            // the offset (not just this frame) so PTS spacing stays intact.
            let dts = out + dts_shift;
            if dts <= self.last_dts {
                let d = self.last_dts + 1_000_000 - dts;
                self.offset += d;
                out += d;
            }
            self.last_dts = out + dts_shift;
        }
        self.last_out[s] = self.last_out[s].max(out);
        (out, out + dts_shift)
    }

    #[cfg(test)]
    fn out_pts(&mut self, s: usize, in_pts: i64, now_rt: i64) -> i64 {
        self.out_ts(s, in_pts, 0, now_rt).0
    }
}

fn dts_shift(buf: &gst::Buffer, in_pts: i64) -> i64 {
    buf.dts().map(|d| d.nseconds() as i64 - in_pts).unwrap_or(0)
}

struct Held {
    s: usize,
    buf: gst::Buffer,
    caps: Option<gst::Caps>,
    in_pts: i64,
}

/// Where re-stamped buffers go: the current output pipeline's appsrcs.
#[derive(Clone)]
pub(crate) struct Targets {
    pub pipeline: gst::Pipeline,
    pub video: gst_app::AppSrc,
    pub audio: gst_app::AppSrc,
}

struct State {
    r: Rebase,
    held: Vec<Held>,
    targets: Option<Targets>,
}

pub(crate) struct Bridge {
    st: Mutex<State>,
    counters: Arc<Counters>,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

impl Bridge {
    pub fn new(counters: Arc<Counters>) -> Self {
        Self {
            st: Mutex::new(State {
                r: Rebase::default(),
                held: Vec::new(),
                targets: None,
            }),
            counters,
        }
    }

    /// Call when the input pipeline is rebuilt: the next video buffer starts a session.
    pub fn reset(&self) {
        let mut st = lock(&self.st);
        st.r.force_new = true;
        st.held.clear();
        st.r.hold_until = None;
    }

    /// Points the bridge at a (new) output pipeline. Its running time starts
    /// again at zero, so the timeline starts over too.
    pub fn set_targets(&self, t: Option<Targets>) {
        let mut st = lock(&self.st);
        st.r = Rebase {
            force_new: true,
            ..Rebase::default()
        };
        st.held.clear();
        st.targets = t;
    }

    fn drop_one(&self) {
        inc(&self.counters.bridge_drops);
    }

    /// Re-stamps and forwards one sample. Never blocks (the appsrcs are leaky).
    pub fn push(&self, s: usize, sample: &gst::Sample) {
        let Some(buf) = sample.buffer_owned() else {
            return;
        };
        let Some(pts) = buf.pts() else {
            // Nothing to key on (tsdemux sets PTS on every PES start).
            self.drop_one();
            return;
        };
        let in_pts = pts.nseconds() as i64;
        let mut st = lock(&self.st);
        let Some(targets) = st.targets.clone() else {
            drop(st);
            self.drop_one();
            return;
        };
        let now_rt = targets
            .pipeline
            .current_running_time()
            .map(|t| t.nseconds() as i64)
            .unwrap_or(0);
        match st.r.classify(s, in_pts, now_rt) {
            Verdict::Drop => {
                drop(st);
                self.drop_one();
                return;
            }
            Verdict::Session { new: true } => {
                // Buffers still held for a session that just ended are dropped.
                st.held.clear();
                inc(&self.counters.sessions);
                info!(
                    session = st.r.session,
                    in_pts, now_rt, "new input session (holding)"
                );
            }
            Verdict::Session { new: false } => {}
        }
        let caps = sample.caps().map(|c| c.to_owned());
        if let Some(until) = st.r.hold_until {
            st.held.push(Held {
                s,
                buf,
                caps,
                in_pts,
            });
            if now_rt < until {
                return;
            }
            st.r.finish_hold();
            info!(
                session = st.r.session,
                offset_ms = st.r.offset / 1_000_000,
                held = st.held.len(),
                "session offset fixed"
            );
            let held = std::mem::take(&mut st.held);
            let mut first = true;
            for h in held {
                let shift = dts_shift(&h.buf, h.in_pts);
                let out = st.r.out_ts(h.s, h.in_pts, shift, now_rt);
                self.forward(&targets, h.s, h.buf, h.caps.as_ref(), out, first);
                first = false;
            }
            return;
        }
        let out = st.r.out_ts(s, in_pts, dts_shift(&buf, in_pts), now_rt);
        drop(st);
        self.forward(&targets, s, buf, caps.as_ref(), out, false);
    }

    fn forward(
        &self,
        t: &Targets,
        s: usize,
        mut buf: gst::Buffer,
        caps: Option<&gst::Caps>,
        (out_pts, out_dts): (i64, i64),
        first: bool,
    ) {
        if out_pts < 0 || out_dts < 0 {
            self.drop_one();
            return;
        }
        let has_dts = buf.dts().is_some();
        {
            let b = buf.make_mut();
            b.set_pts(gst::ClockTime::from_nseconds(out_pts as u64));
            b.set_dts(has_dts.then(|| gst::ClockTime::from_nseconds(out_dts as u64)));
            if first {
                b.set_flags(gst::BufferFlags::DISCONT);
            }
        }
        let src = if s == VIDEO { &t.video } else { &t.audio };
        if let Some(caps) = caps {
            let mut caps = caps.clone();
            if s == VIDEO
                && let Some(st) = caps.make_mut().structure_mut(0)
                && !st.has_field("alignment")
            {
                // Raw tsdemux output: one PES = one access unit.
                st.set("alignment", "au");
                st.set("stream-format", "byte-stream");
            }
            if src.caps().as_ref() != Some(&caps) {
                info!(stream = s, %caps, "output caps");
                src.set_caps(Some(&caps));
            }
        }
        if s == AUDIO {
            inc(&self.counters.audio_in);
        }
        // A flushing or stopped output must not take the input down with it.
        let _ = src.push_buffer(buf);
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;
    const MS: i64 = 1_000_000;

    fn open(r: &mut Rebase, frames: &[(i64, i64)]) {
        for &(pts, rt) in frames {
            let _ = r.classify(VIDEO, pts, rt);
        }
        r.finish_hold();
    }

    #[test]
    fn audio_before_video_dropped() {
        let mut r = Rebase::default();
        assert_eq!(r.classify(AUDIO, 0, 10 * SEC), Verdict::Drop);
        assert_eq!(
            r.classify(VIDEO, 0, 10 * SEC),
            Verdict::Session { new: true }
        );
        assert_eq!(
            r.classify(AUDIO, -100 * MS, 10 * SEC),
            Verdict::Session { new: false }
        );
    }

    #[test]
    fn burst_start_uses_earliest_arrival() {
        let mut r = Rebase::default();
        // First 3 frames released together 300 ms late, then on time.
        open(
            &mut r,
            &[
                (0, 1300 * MS),
                (33 * MS, 1300 * MS),
                (66 * MS, 1300 * MS),
                (100 * MS, 1100 * MS),
            ],
        );
        // (floor is 40 ms, below the 1000 ms steady-state offset)
        assert_eq!(r.offset, 1000 * MS);
        // Steady state: frame at pts 200 ms arrives at rt 1200 ms -> exactly on time.
        assert_eq!(r.out_pts(VIDEO, 200 * MS, 1200 * MS), 1200 * MS);
    }

    #[test]
    fn restart_to_zero_moves_forward() {
        let mut r = Rebase::default();
        open(&mut r, &[(5 * SEC, 10 * SEC)]);
        let o = r.out_pts(VIDEO, 7 * SEC, 12 * SEC);
        assert_eq!(o, 12 * SEC);
        let _ = r.classify(VIDEO, 7 * SEC + 33 * MS, 12 * SEC + 33 * MS);
        // Source restarts with PTS back at 0 after a 5 s gap.
        assert_eq!(
            r.classify(VIDEO, 0, 17 * SEC),
            Verdict::Session { new: true }
        );
        r.finish_hold();
        assert_eq!(r.out_pts(VIDEO, 0, 17 * SEC), 17 * SEC);
        // Audio of the new session adopts the same offset.
        assert_eq!(
            r.classify(AUDIO, 10 * MS, 17 * SEC),
            Verdict::Session { new: false }
        );
        assert_eq!(r.out_pts(AUDIO, 10 * MS, 17 * SEC), 17 * SEC + 10 * MS);
    }

    #[test]
    fn restart_with_clock_behind_never_goes_back() {
        let mut r = Rebase::default();
        open(&mut r, &[(100 * SEC, 5 * SEC)]);
        let last = r.out_pts(VIDEO, 100 * SEC, 5 * SEC);
        r.force_new = true;
        let _ = r.classify(VIDEO, 0, 0);
        r.finish_hold();
        assert!(r.out_pts(VIDEO, 0, 0) > last);
    }

    #[test]
    fn burst_below_zero_starts_late_then_catches_up() {
        let mut r = Rebase::default();
        // Frames 2.0..2.5 s released at rt 0.08 s, then steady arrival 2.5 s behind PTS.
        let mut f: Vec<(i64, i64)> = (0..15).map(|k| (2 * SEC + k * 33 * MS, 80 * MS)).collect();
        f.push((2500 * MS, 10 * MS));
        open(&mut r, &f);
        // First frame at the 40 ms floor (+1 ms of catch-up slew).
        assert_eq!(r.out_pts(VIDEO, 2 * SEC, 500 * MS), 41 * MS);
        let mut late = 0;
        for k in 16..1000 {
            let pts = 2 * SEC + k * 33 * MS;
            let rt = pts - 2490 * MS;
            late = rt - r.out_pts(VIDEO, pts, rt);
        }
        assert!((0..=LATE_TOL_NS).contains(&late), "late {late}");
    }

    #[test]
    fn b_frames_keep_pts_order() {
        let mut r = Rebase::default();
        open(&mut r, &[(0, SEC)]);
        let f = 33_333_333;
        // Decode order I0 P3 B1 B2 with DTS = decode slot - 2 frames.
        let mut pts = Vec::new();
        for (k, p) in [0i64, 3, 1, 2, 6, 4, 5].into_iter().enumerate() {
            let in_pts = p * f;
            let dts = (k as i64 - 2) * f;
            let (o, _) = r.out_ts(VIDEO, in_pts, dts - in_pts, SEC + k as i64 * f - 2 * f);
            pts.push(o - SEC);
        }
        let want: Vec<i64> = [0i64, 3, 1, 2, 6, 4, 5].iter().map(|p| p * f).collect();
        assert_eq!(pts, want);
    }

    #[test]
    fn early_frames_slew_monotonic() {
        let mut r = Rebase::default();
        open(&mut r, &[(0, SEC)]);
        let mut last = 0;
        assert_eq!(r.offset, SEC);
        // Source clock 1% fast: each frame arrives 0.33 ms "earlier".
        for k in 1..300 {
            let pts = k * 33_333_333;
            let rt = SEC + (pts as f64 * 0.99) as i64;
            let o = r.out_pts(VIDEO, pts, rt);
            assert!(o > last);
            assert!(o - rt <= 0, "frame {k} early by {}", o - rt);
            last = o;
        }
    }
}
