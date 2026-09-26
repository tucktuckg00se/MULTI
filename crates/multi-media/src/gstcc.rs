//! GStreamer's caption encoders (`tttocea608`, `tttocea708`) driven one video
//! frame at a time, without `cccombiner` (ported from M0 spike S2b).
//!
//! Each caption track is `appsrc ! tttocea608|tttocea708 ! ...`; several
//! tracks are merged by `cea708mux`; the result ends in an `appsink` as
//! CEA-708 `cc_data`. The caller calls [`GstCc::frame`] once per video frame
//! (display order): tracks that got no text this frame receive a GAP event
//! for it, so the encoders emit that frame's bytes, which the call then pulls
//! from the appsink. The pipeline is not live, so nothing waits on a clock.
//!
//! Two upstream bugs are worked around (S2b):
//! 1. `tttocea708` forgets `roll-up-rows` in its READY->PAUSED reset (the 708
//!    window is always 2 rows); [`GstCc::new`] sets it again once PLAYING.
//! 2. A GAP to a `tttocea708` that is already ahead of the frame makes it add
//!    one more frame, so it never catches up; GAPs go only to tracks whose
//!    encoder is behind the current frame.

use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use gst::prelude::*;

/// One caption track.
#[derive(Clone, Debug, PartialEq)]
pub enum TrackCfg {
    /// `tttocea608` (always writes CC1 codes) -> field relabel for CC3 ->
    /// `ccconverter` -> `cc_data`.
    Cea608 {
        /// 0 = CC1 (field 1), 1 = CC3 (field 2).
        field: u8,
        /// `pop-on`, `paint-on`, `roll-up2`, `roll-up3`, `roll-up4`.
        mode: String,
    },
    /// `tttocea708`, with optional 608 compatibility bytes on CC1/CC3.
    Cea708 {
        service: u32,
        /// 0 = none, 1 = CC1, 3 = CC3.
        cea608_channel: u32,
        /// `pop-on`, `paint-on`, `roll-up`.
        mode: String,
        roll_up_rows: u32,
    },
}

/// How a track is fed: our own src pad pushed from the caller's thread (one
/// track, fully synchronous), or an appsrc with its own streaming thread
/// (several tracks into the `cea708mux` aggregator, which would otherwise
/// deadlock a single pushing thread).
enum Feed {
    Pad(gst::Pad),
    App(gst_app::AppSrc),
}

impl Feed {
    fn push(&self, b: gst::Buffer) -> Result<()> {
        match self {
            Feed::Pad(p) => p
                .push(b)
                .map(|_| ())
                .map_err(|e| anyhow::anyhow!("push: {e:?}")),
            Feed::App(a) => a
                .push_buffer(b)
                .map(|_| ())
                .map_err(|e| anyhow::anyhow!("push: {e:?}")),
        }
    }

    fn event(&self, e: gst::Event) -> bool {
        match self {
            Feed::Pad(p) => p.push_event(e),
            Feed::App(a) => a.send_event(e),
        }
    }
}

pub struct GstCc {
    pipeline: gst::Pipeline,
    srcs: Vec<Feed>,
    sync: bool,
    sink: gst_app::AppSink,
    fps: (i32, i32),
    /// Tracks that already got a buffer covering the current frame.
    fed: Vec<bool>,
    /// Output buffers not handed out yet: (frame index from PTS, cc_data).
    queue: VecDeque<(u64, Vec<u8>)>,
    /// Highest frame index seen on the output.
    out_hi: Option<u64>,
    /// Per track: 1 + highest frame index its encoder has output (0 = none).
    track_hi: Vec<Arc<AtomicU64>>,
    /// Longest wait for the encoders per frame; a late buffer is handed out
    /// on a later frame instead.
    pub wait: Duration,
    /// Frames whose output did not arrive within `wait`, in a row.
    pub timeouts_in_row: u64,
}

fn make(f: &str) -> Result<gst::Element> {
    gst::ElementFactory::make(f)
        .build()
        .with_context(|| format!("missing GStreamer element {f}"))
}

impl GstCc {
    /// PTS of frame `i` in ns.
    pub fn pts(&self, i: u64) -> u64 {
        gst::ClockTime::SECOND
            .nseconds()
            .saturating_mul(i)
            .saturating_mul(self.fps.1 as u64)
            / self.fps.0 as u64
    }

    fn idx(&self, pts: u64) -> u64 {
        let num = pts as u128 * self.fps.0 as u128;
        let den = 1_000_000_000u128 * self.fps.1 as u128;
        ((num + den / 2) / den) as u64
    }

    pub fn fps(&self) -> (i32, i32) {
        self.fps
    }

    pub fn new(tracks: &[TrackCfg], fps: (i32, i32)) -> Result<Self> {
        if tracks.is_empty() {
            bail!("no caption tracks");
        }
        if fps.0 <= 0 || fps.1 <= 0 {
            bail!("bad frame rate {}/{}", fps.0, fps.1);
        }
        let p = gst::Pipeline::with_name("captions");
        let rate = gst::Fraction::new(fps.0, fps.1);
        let cc_caps = gst::Caps::builder("closedcaption/x-cea-708")
            .field("format", "cc_data")
            .field("framerate", rate)
            .build();
        let sink = gst_app::AppSink::builder()
            .sync(false)
            .async_(false)
            .max_buffers(0)
            .build();
        p.add(&sink)?;
        let mux = if tracks.len() > 1 {
            let m = make("cea708mux")?;
            let f = gst::ElementFactory::make("capsfilter")
                .property("caps", &cc_caps)
                .build()?;
            p.add_many([&m, &f])?;
            gst::Element::link_many([&m, &f, sink.upcast_ref()])?;
            Some(m)
        } else {
            None
        };
        let sync = tracks.len() == 1;
        let mut srcs = Vec::new();
        let mut track_hi = Vec::new();
        let mut rows_fix: Vec<(gst::Element, u32)> = Vec::new();
        let (fn_, fd) = (fps.0 as u128, fps.1 as u128);
        let text_caps = gst::Caps::builder("text/x-raw")
            .field("format", "utf8")
            .build();
        for t in tracks {
            let src = gst_app::AppSrc::builder()
                .format(gst::Format::Time)
                .is_live(false)
                .max_bytes(0)
                .build();
            src.set_property("block", false);
            let mut chain: Vec<gst::Element> = if sync {
                vec![]
            } else {
                vec![src.clone().upcast()]
            };
            match t {
                TrackCfg::Cea608 { field, mode } => {
                    let e = make("tttocea608")?;
                    e.set_property_from_str("mode", mode);
                    chain.push(e);
                    if *field == 1 {
                        // tttocea608 always writes CC1 codes on field 1. CC3 uses
                        // the same codes on field 2, so relabel the caps.
                        let cs = make("capssetter")?;
                        let c = gst::Caps::builder("closedcaption/x-cea-608")
                            .field("field", 1i32)
                            .build();
                        cs.set_property("caps", &c);
                        chain.push(cs);
                    }
                    chain.push(make("ccconverter")?);
                }
                TrackCfg::Cea708 {
                    service,
                    cea608_channel,
                    mode,
                    roll_up_rows,
                } => {
                    let e = make("tttocea708")?;
                    e.set_property_from_str("mode", mode);
                    e.set_property("service-number", *service);
                    e.set_property("cea608-channel", *cea608_channel);
                    e.set_property("roll-up-rows", *roll_up_rows);
                    rows_fix.push((e.clone(), *roll_up_rows));
                    chain.push(e);
                }
            }
            let f = gst::ElementFactory::make("capsfilter")
                .property("caps", &cc_caps)
                .build()?;
            chain.push(f);
            p.add_many(chain.iter())?;
            gst::Element::link_many(chain.iter())?;
            let last = chain.last().context("empty caption chain")?;
            let hi = Arc::new(AtomicU64::new(0));
            let h = hi.clone();
            last.static_pad("src")
                .context("caption chain has no src pad")?
                .add_probe(gst::PadProbeType::BUFFER, move |_, info| {
                    if let Some(p) = info.buffer().and_then(|b| b.pts()) {
                        let den = 1_000_000_000u128 * fd;
                        let idx = ((p.nseconds() as u128 * fn_ + den / 2) / den) as u64;
                        h.fetch_max(idx + 1, Ordering::Relaxed);
                    }
                    gst::PadProbeReturn::Ok
                });
            track_hi.push(hi);
            match &mux {
                Some(m) => {
                    let pad = m
                        .request_pad_simple("sink_%u")
                        .context("cea708mux has no free pad")?;
                    last.static_pad("src")
                        .context("caption chain has no src pad")?
                        .link(&pad)?;
                }
                None => last.link(&sink)?,
            }
            if sync {
                let first = chain.first().context("empty caption chain")?;
                let pad = gst::Pad::builder(gst::PadDirection::Src)
                    .name("feed")
                    .build();
                pad.set_active(true)?;
                pad.link(&first.static_pad("sink").context("no sink pad")?)?;
                srcs.push(Feed::Pad(pad));
            } else {
                src.set_caps(Some(&text_caps));
                srcs.push(Feed::App(src));
            }
        }
        p.set_state(gst::State::Playing)?;
        // Workaround 1: tttocea708's READY->PAUSED reset forgets roll-up-rows;
        // setting it again once running reaches the translator.
        for (e, r) in &rows_fix {
            e.set_property("roll-up-rows", *r);
        }
        for f in &srcs {
            if let Feed::Pad(pad) = f {
                pad.push_event(gst::event::StreamStart::new("multi-captions"));
                pad.push_event(gst::event::Caps::new(&text_caps));
                pad.push_event(gst::event::Segment::new(&gst::FormattedSegment::<
                    gst::ClockTime,
                >::new()));
            }
        }
        let n = srcs.len();
        Ok(Self {
            pipeline: p,
            srcs,
            sync,
            sink,
            fps,
            fed: vec![false; n],
            queue: VecDeque::new(),
            out_hi: None,
            track_hi,
            wait: Duration::from_millis(5),
            timeouts_in_row: 0,
        })
    }

    /// How many frames `track`'s encoder has output beyond frame `slot`.
    pub fn lead(&self, track: usize, slot: u64) -> u64 {
        self.track_hi
            .get(track)
            .map_or(0, |h| h.load(Ordering::Relaxed).saturating_sub(slot))
    }

    /// Sends a "final transcript" event: in roll-up, the next text buffer on
    /// this track starts on a new row.
    pub fn carriage_return(&self, track: usize) -> bool {
        let Some(src) = self.srcs.get(track) else {
            return false;
        };
        let ev = gst::event::CustomDownstream::new(gst::Structure::new_empty(
            "rstranscribe/final-transcript",
        ));
        src.event(ev)
    }

    /// Queues `text` on `track` at frame `i`, lasting `frames` frames (pop-on
    /// display time; for roll-up, the frames it may use to transmit).
    pub fn push(&mut self, track: usize, i: u64, frames: u64, text: &str) -> Result<()> {
        let Some(src) = self.srcs.get(track) else {
            bail!("no caption track {track}")
        };
        let mut b = gst::Buffer::from_slice(text.as_bytes().to_vec());
        let (pts, end) = (self.pts(i), self.pts(i + frames.max(1)));
        if let Some(bm) = b.get_mut() {
            bm.set_pts(gst::ClockTime::from_nseconds(pts));
            bm.set_duration(gst::ClockTime::from_nseconds(end.saturating_sub(pts)));
        }
        src.push(b)?;
        if let Some(f) = self.fed.get_mut(track) {
            *f = true;
        }
        Ok(())
    }

    /// Ends frame `i`: GAPs to idle tracks that are behind, then returns the
    /// `cc_data` for this frame (possibly empty).
    pub fn frame(&mut self, i: u64) -> Vec<u8> {
        let (pts, dur) = (self.pts(i), self.pts(i + 1).saturating_sub(self.pts(i)));
        for (k, src) in self.srcs.iter().enumerate() {
            // Workaround 2: never GAP an encoder that is already ahead.
            let ahead = self
                .track_hi
                .get(k)
                .is_some_and(|h| h.load(Ordering::Relaxed) > i);
            if !self.fed.get(k).copied().unwrap_or(false) && !ahead {
                let ev = gst::event::Gap::builder(gst::ClockTime::from_nseconds(pts))
                    .duration(gst::ClockTime::from_nseconds(dur))
                    .build();
                src.event(ev);
            }
        }
        self.fed.iter_mut().for_each(|f| *f = false);
        let deadline = Instant::now() + self.wait;
        if self.sync {
            // Everything the push produced is already in the appsink.
            while let Some(s) = self.sink.try_pull_sample(gst::ClockTime::ZERO) {
                self.take(&s, i);
            }
        }
        let mut timed_out = false;
        while !self.sync && self.out_hi.is_none_or(|h| h < i) {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                timed_out = true;
                break;
            }
            let Some(s) = self
                .sink
                .try_pull_sample(gst::ClockTime::from_nseconds(left.as_nanos() as u64))
            else {
                continue;
            };
            self.take(&s, i);
        }
        self.timeouts_in_row = if timed_out {
            self.timeouts_in_row + 1
        } else {
            0
        };
        match self.queue.front() {
            Some((idx, _)) if *idx <= i => {
                self.queue.pop_front().map(|(_, d)| d).unwrap_or_default()
            }
            _ => Vec::new(),
        }
    }

    fn take(&mut self, s: &gst::Sample, i: u64) {
        let Some(buf) = s.buffer() else { return };
        let Ok(map) = buf.map_readable() else { return };
        let idx = buf.pts().map(|p| self.idx(p.nseconds())).unwrap_or(i);
        self.out_hi = Some(self.out_hi.map_or(idx, |h| h.max(idx)));
        // Bound the queue: a stuck consumer must not grow memory.
        if self.queue.len() >= 256 {
            self.queue.pop_front();
        }
        self.queue.push_back((idx, map.to_vec()));
    }

    /// Bus errors (`Err`) and warnings (`Ok`) since the last call.
    pub fn bus_messages(&self) -> Vec<Result<String, String>> {
        let mut v = Vec::new();
        if let Some(bus) = self.pipeline.bus() {
            while let Some(m) = bus.pop() {
                let src = m.src().map(|s| s.name().to_string()).unwrap_or_default();
                match m.view() {
                    gst::MessageView::Error(e) => {
                        v.push(Err(format!("{src}: {} ({:?})", e.error(), e.debug())))
                    }
                    gst::MessageView::Warning(w) => {
                        v.push(Ok(format!("{src}: {} ({:?})", w.error(), w.debug())))
                    }
                    _ => {}
                }
            }
        }
        v
    }
}

impl Drop for GstCc {
    fn drop(&mut self) {
        let _ = self.pipeline.set_state(gst::State::Null);
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;

    fn lane(svc: u32, ch: u32) -> TrackCfg {
        TrackCfg::Cea708 {
            service: svc,
            cea608_channel: ch,
            mode: "roll-up".into(),
            roll_up_rows: 3,
        }
    }

    #[test]
    fn one_buffer_per_frame_with_text() {
        if gst::init().is_err() || gst::ElementFactory::find("tttocea708").is_none() {
            return;
        }
        for tracks in [vec![lane(1, 1)], vec![lane(1, 1), lane(2, 3)]] {
            let mut g = GstCc::new(&tracks, (30, 1)).unwrap();
            g.wait = Duration::from_millis(500);
            let mut first = Vec::new();
            for i in 0..60 {
                if i == 5 {
                    g.carriage_return(0);
                    g.push(0, i, 1, "HI").unwrap();
                }
                let data = g.frame(i);
                assert_eq!(data.len(), 60, "frame {i}: 20 triples at 30 fps");
                if i == 5 {
                    first = data;
                }
            }
            // Frame 5 opens a DTVCC packet (cc_type 3) after the two 608 triples.
            assert_eq!(first[6] & 7, 7);
            assert_eq!(g.timeouts_in_row, 0);
        }
    }
}
