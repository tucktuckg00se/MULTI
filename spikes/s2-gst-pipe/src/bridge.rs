//! Input -> output bridge: re-stamps buffers so the output timeline is
//! continuous and monotonic across source restarts and PTS resets, and locked
//! to the output pipeline's running time.
//!
//! Why the lock matters: `mpegtsmux` (and `cccombiner`) are live aggregators.
//! A buffer whose running time is still in the future is held until every
//! other pad has data or the deadline passes. The source sends audio in
//! ~180 ms PES that arrive after the video they belong with, so video stamped
//! even slightly "early" waits for audio: a 67–200 ms sawtooth. Video stamped
//! at or just behind "now" goes straight through.
//!
//! - Each run of contiguous input timestamps is a *session* with one
//!   input->output offset shared by audio and video (A/V sync is the source's).
//!   A new session starts when the input is rebuilt, or when video PTS jumps
//!   back > 1 s or forward > 3 s (source restarted with PTS at 0, long gap).
//!   Only video opens a session; audio before it is dropped.
//! - For the first [`HOLD_NS`] of a session buffers are held while the offset
//!   is taken as the *earliest* arrival seen (min of running_time - PTS): the
//!   demuxer releases the first frames in a burst, so the first frame alone
//!   would stamp everything too early. The session never starts before the
//!   previous one's last output + 40 ms, so output PTS only move forward.
//!   The session also never starts before output PTS 40 ms, so the burst at
//!   startup is not pushed below zero; it starts late and catches up.
//! - After that, a video frame that would be early pulls the offset back, and
//!   one more than [`LATE_TOL_NS`] late pushes it forward, by at most
//!   [`SLEW_NS`] per frame (clock drift, startup bursts).

use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};

use gst::prelude::*;
use tracing::info;

use crate::stamps::Stamps;

const SEC: i64 = 1_000_000_000;
/// Startup hold per session.
pub const HOLD_NS: i64 = 500_000_000;
/// Max offset correction per video frame (1 ms/frame = 30 ms/s at 30 fps).
pub const SLEW_NS: i64 = 1_000_000;

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
}

#[derive(Debug, PartialEq, Eq)]
enum Verdict {
    Drop,
    /// Buffer belongs to the current session (possibly one just opened).
    Session { new: bool },
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
            tracing::debug!(in_pts, now_rt, arrival = now_rt - in_pts, "hold");
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

    /// Output PTS for a buffer of the current (non-holding) session.
    fn out_pts(&mut self, s: usize, in_pts: i64, now_rt: i64) -> i64 {
        let mut out = in_pts + self.offset;
        if s == VIDEO {
            let early = out - now_rt;
            if early > 0 {
                let d = early.min(SLEW_NS);
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
            // Strictly increasing video PTS whatever happens upstream.
            if out <= self.last_out[VIDEO] {
                out = self.last_out[VIDEO] + 1_000_000;
            }
        }
        self.last_out[s] = self.last_out[s].max(out);
        out
    }
}

struct Held {
    s: usize,
    buf: gst::Buffer,
    caps: Option<gst::Caps>,
    in_pts: i64,
}

struct State {
    r: Rebase,
    held: Vec<Held>,
}

pub struct Bridge {
    st: Mutex<State>,
    pub video: gst_app::AppSrc,
    pub audio: gst_app::AppSrc,
    out: gst::Pipeline,
    stamps: Arc<Stamps>,
}

impl Bridge {
    pub fn new(video: gst_app::AppSrc, audio: gst_app::AppSrc, out: gst::Pipeline, stamps: Arc<Stamps>) -> Arc<Self> {
        Arc::new(Self {
            st: Mutex::new(State { r: Rebase::default(), held: Vec::new() }),
            video,
            audio,
            out,
            stamps,
        })
    }

    /// Call when the input pipeline is rebuilt: the next video buffer starts a session.
    pub fn reset(&self) {
        if let Ok(mut st) = self.st.lock() {
            st.r.force_new = true;
            st.held.clear();
            st.r.hold_until = None;
        }
    }

    /// Total offset correction applied by the slew so far (ms).
    pub fn slewed_ms(&self) -> f64 {
        self.st.lock().map(|s| s.r.slewed_ns as f64 / 1e6).unwrap_or(0.0)
    }

    /// Re-stamps and forwards one sample. Never blocks (appsrc is leaky).
    pub fn push(&self, s: usize, sample: &gst::Sample) -> Result<gst::FlowSuccess, gst::FlowError> {
        let Some(buf) = sample.buffer_owned() else { return Ok(gst::FlowSuccess::Ok) };
        let Some(pts) = buf.pts() else {
            // Nothing to key on (tsdemux sets PTS on every PES start).
            self.stamps.counters.bridge_drops.fetch_add(1, Ordering::Relaxed);
            return Ok(gst::FlowSuccess::Ok);
        };
        let in_pts = pts.nseconds() as i64;
        let now_rt = self.out.current_running_time().map(|t| t.nseconds() as i64).unwrap_or(0);
        let Ok(mut st) = self.st.lock() else { return Ok(gst::FlowSuccess::Ok) };
        match st.r.classify(s, in_pts, now_rt) {
            Verdict::Drop => {
                self.stamps.counters.bridge_drops.fetch_add(1, Ordering::Relaxed);
                return Ok(gst::FlowSuccess::Ok);
            }
            Verdict::Session { new: true } => {
                // Buffers still held for a session that just ended are dropped.
                st.held.clear();
                self.stamps.counters.sessions.fetch_add(1, Ordering::Relaxed);
                info!(session = st.r.session, in_pts, now_rt, "new input session (holding)");
            }
            Verdict::Session { new: false } => {}
        }
        let caps = sample.caps().map(|c| c.to_owned());
        if let Some(until) = st.r.hold_until {
            st.held.push(Held { s, buf, caps, in_pts });
            if now_rt < until {
                return Ok(gst::FlowSuccess::Ok);
            }
            st.r.finish_hold();
            info!(session = st.r.session, offset_ms = st.r.offset / 1_000_000, held = st.held.len(), "session offset fixed");
            let held = std::mem::take(&mut st.held);
            let mut first = true;
            for h in held {
                let out = st.r.out_pts(h.s, h.in_pts, now_rt);
                self.forward(h.s, h.buf, h.caps.as_ref(), h.in_pts, out, now_rt, st.r.session, first);
                first = false;
            }
            return Ok(gst::FlowSuccess::Ok);
        }
        let out = st.r.out_pts(s, in_pts, now_rt);
        let session = st.r.session;
        drop(st);
        self.forward(s, buf, caps.as_ref(), in_pts, out, now_rt, session, false);
        Ok(gst::FlowSuccess::Ok)
    }

    #[allow(clippy::too_many_arguments)]
    fn forward(
        &self,
        s: usize,
        mut buf: gst::Buffer,
        caps: Option<&gst::Caps>,
        in_pts: i64,
        out_pts: i64,
        now_rt: i64,
        session: u64,
        first: bool,
    ) {
        if out_pts < 0 {
            self.stamps.counters.bridge_drops.fetch_add(1, Ordering::Relaxed);
            return;
        }
        let dts_shift = buf.dts().map(|d| d.nseconds() as i64 - in_pts);
        {
            let b = buf.make_mut();
            b.set_pts(gst::ClockTime::from_nseconds(out_pts as u64));
            b.set_dts(dts_shift.and_then(|d| u64::try_from(out_pts + d).ok()).map(gst::ClockTime::from_nseconds));
            if first {
                b.set_flags(gst::BufferFlags::DISCONT);
            }
        }
        let src = if s == VIDEO { &self.video } else { &self.audio };
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
        if s == VIDEO {
            self.stamps.on_bridge(session, in_pts, out_pts, now_rt);
        } else {
            self.stamps.counters.audio_in.fetch_add(1, Ordering::Relaxed);
        }
        // A flushing/stopped output must not take the input down with it.
        let _ = src.push_buffer(buf);
    }
}

#[cfg(test)]
mod tests {
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
        assert_eq!(r.classify(VIDEO, 0, 10 * SEC), Verdict::Session { new: true });
        assert_eq!(r.classify(AUDIO, -100 * MS, 10 * SEC), Verdict::Session { new: false });
    }

    #[test]
    fn burst_start_uses_earliest_arrival() {
        let mut r = Rebase::default();
        // First 3 frames released together 300 ms late, then on time.
        open(&mut r, &[(0, 1300 * MS), (33 * MS, 1300 * MS), (66 * MS, 1300 * MS), (100 * MS, 1100 * MS)]);
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
        assert_eq!(r.classify(VIDEO, 0, 17 * SEC), Verdict::Session { new: true });
        r.finish_hold();
        assert_eq!(r.out_pts(VIDEO, 0, 17 * SEC), 17 * SEC);
        // Audio of the new session adopts the same offset.
        assert_eq!(r.classify(AUDIO, 10 * MS, 17 * SEC), Verdict::Session { new: false });
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
