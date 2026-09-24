//! S2b: drive GStreamer's own caption encoders (`tttocea608`, `tttocea708`)
//! one video frame at a time, without `cccombiner`.
//!
//! Each caption track is `appsrc ! tttocea608|tttocea708 ! ...`; several
//! tracks are merged by `cea708mux`; the result ends in an `appsink` as
//! CEA-708 `cc_data`. The caller calls [`GstCc::frame`] once per video frame
//! (display order): tracks that got no text this frame receive a GAP event for
//! it, so the encoders emit that frame's bytes, which the call then pulls from
//! the appsink. The pipeline is not live, so nothing waits on a clock.
//!
//! The encoders time-stamp their output by frame number. When text needs more
//! frames than the input buffer's duration, `tttocea608` clamps the extra pairs
//! to the last frame ("Too much text for bandwidth") and `tttocea708` stamps
//! them in the future. [`GstCc::frame`] hands out at most one output buffer per
//! frame, oldest first, and reports how far behind that buffer is (`lag`).

use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use gst::prelude::*;

/// One caption track.
#[derive(Clone, Debug, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum TrackCfg {
    /// `tttocea608` (always writes CC1 codes) → optional field rewrite to
    /// field 2 (CC3) → `ccconverter` → `cc_data`.
    #[serde(rename = "608")]
    Cea608 {
        /// 0 = CC1 (field 1), 1 = CC3 (field 2).
        #[serde(default)]
        field: u8,
        /// `pop-on`, `paint-on`, `roll-up2`, `roll-up3`, `roll-up4`.
        mode: String,
        /// Input caps `application/x-json,format=cea608` instead of text.
        #[serde(default)]
        json: bool,
        #[serde(default = "minus_one")]
        origin_row: i32,
    },
    /// `tttocea708` (text only), optional 608 compatibility bytes on CC1/CC3.
    #[serde(rename = "708")]
    Cea708 {
        service: u32,
        /// 0 = none, 1 = CC1, 3 = CC3.
        #[serde(default)]
        cea608_channel: u32,
        /// `pop-on`, `paint-on`, `roll-up`.
        mode: String,
        #[serde(default = "three")]
        roll_up_rows: u32,
        #[serde(default = "minus_one")]
        origin_row: i32,
        /// cea708mux pad `discarded-services` (negative = 608 channel).
        #[serde(default)]
        discard: Vec<i32>,
    },
}

fn minus_one() -> i32 {
    -1
}
fn three() -> u32 {
    3
}

/// What to feed a track.
#[derive(Clone, Debug)]
pub enum Input {
    Text(String),
    Json(String),
    /// Raw bytes (e.g. invalid UTF-8).
    Raw(Vec<u8>),
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
            Feed::Pad(p) => p.push(b).map(|_| ()).map_err(|e| anyhow::anyhow!("push: {e:?}")),
            Feed::App(a) => a.push_buffer(b).map(|_| ()).map_err(|e| anyhow::anyhow!("push: {e:?}")),
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
    /// Send a GAP only to tracks whose encoder is behind this frame. With
    /// `false`, every idle track gets a GAP every frame; `tttocea708` then
    /// adds one frame per GAP while it is ahead and never catches up.
    pub gap_when_behind: bool,
    pub wait: Duration,
    pub stats: Stats,
}

#[derive(Default, Debug, Clone)]
pub struct Stats {
    pub frames: u64,
    pub out_buffers: u64,
    /// Output buffers whose PTS maps to a frame already handed out
    /// (i.e. more than one buffer for a frame).
    pub late_buffers: u64,
    pub max_lag: u64,
    pub sum_lag: u64,
    pub timeouts: u64,
    pub empty_frames: u64,
    pub max_wait_us: u64,
    pub sum_wait_us: u64,
}

fn make(f: &str) -> Result<gst::Element> {
    gst::ElementFactory::make(f).build().with_context(|| format!("missing element {f}"))
}

impl GstCc {
    pub fn frame_ns(&self) -> u64 {
        1_000_000_000u64 * self.fps.1 as u64 / self.fps.0 as u64
    }

    /// PTS of frame `i` in ns.
    pub fn pts(&self, i: u64) -> u64 {
        gst::ClockTime::SECOND.nseconds().saturating_mul(i).saturating_mul(self.fps.1 as u64) / self.fps.0 as u64
    }

    fn idx(&self, pts: u64) -> u64 {
        // round(pts * fps)
        let num = pts as u128 * self.fps.0 as u128;
        let den = 1_000_000_000u128 * self.fps.1 as u128;
        ((num + den / 2) / den) as u64
    }

    pub fn new(tracks: &[TrackCfg], fps: (i32, i32)) -> Result<Self> {
        if tracks.is_empty() {
            bail!("no tracks");
        }
        let p = gst::Pipeline::new();
        let rate = gst::Fraction::new(fps.0, fps.1);
        let cc_caps = gst::Caps::builder("closedcaption/x-cea-708").field("format", "cc_data").field("framerate", rate).build();
        let sink = gst_app::AppSink::builder().sync(false).async_(false).max_buffers(0).build();
        p.add(&sink)?;
        let mux = if tracks.len() > 1 {
            let m = make("cea708mux")?;
            let f = gst::ElementFactory::make("capsfilter").property("caps", &cc_caps).build()?;
            p.add_many([&m, &f])?;
            gst::Element::link_many([&m, &f, sink.upcast_ref()])?;
            Some(m)
        } else {
            None
        };
        let mut srcs = Vec::new();
        let mut track_hi = Vec::new();
        let mut rows_fix: Vec<(gst::Element, u32)> = Vec::new();
        let (fn_, fd) = (fps.0 as u128, fps.1 as u128);
        for t in tracks {
            let src = gst_app::AppSrc::builder().format(gst::Format::Time).is_live(false).max_bytes(0).build();
            src.set_property("block", false);
            let sync = tracks.len() == 1;
            let mut chain: Vec<gst::Element> = if sync { vec![] } else { vec![src.clone().upcast()] };
            let in_caps: gst::Caps;
            let discard: Vec<i32> = match t {
                TrackCfg::Cea608 { field, mode, json, origin_row } => {
                    let caps = if *json {
                        gst::Caps::builder("application/x-json").field("format", "cea608").build()
                    } else {
                        gst::Caps::builder("text/x-raw").field("format", "utf8").build()
                    };
                    in_caps = caps;
                    let e = make("tttocea608")?;
                    e.set_property_from_str("mode", mode);
                    e.set_property("origin-row", *origin_row);
                    chain.push(e);
                    if *field == 1 {
                        // tttocea608 always writes CC1 codes on field 1. CC3 uses
                        // the same codes on field 2, so relabel the caps.
                        let cs = make("capssetter")?;
                        let c = gst::Caps::builder("closedcaption/x-cea-608").field("field", 1i32).build();
                        cs.set_property("caps", &c);
                        chain.push(cs);
                    }
                    chain.push(make("ccconverter")?);
                    vec![]
                }
                TrackCfg::Cea708 { service, cea608_channel, mode, roll_up_rows, origin_row, discard: d } => {
                    in_caps = gst::Caps::builder("text/x-raw").field("format", "utf8").build();
                    let e = make("tttocea708")?;
                    e.set_property_from_str("mode", mode);
                    e.set_property("service-number", *service);
                    e.set_property("cea608-channel", *cea608_channel);
                    e.set_property("roll-up-rows", *roll_up_rows);
                    e.set_property("origin-row", *origin_row);
                    rows_fix.push((e.clone(), *roll_up_rows));
                    chain.push(e);
                    d.clone()
                }
            };
            let f = gst::ElementFactory::make("capsfilter").property("caps", &cc_caps).build()?;
            chain.push(f);
            p.add_many(chain.iter())?;
            gst::Element::link_many(chain.iter())?;
            let last = chain.last().context("chain")?;
            let hi = Arc::new(AtomicU64::new(0));
            let h = hi.clone();
            last.static_pad("src").context("src")?.add_probe(gst::PadProbeType::BUFFER, move |_, info| {
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
                    let pad = m.request_pad_simple("sink_%u").context("cea708mux pad")?;
                    if !discard.is_empty() {
                        let arr = gst::Array::new(discard.iter().map(|v| v.to_send_value()));
                        pad.set_property("discarded-services", arr);
                    }
                    last.static_pad("src").context("src")?.link(&pad)?;
                }
                None => last.link(&sink)?,
            }
            if sync {
                let first = chain.first().context("chain")?;
                let pad = gst::Pad::builder(gst::PadDirection::Src).name("feed").build();
                pad.set_active(true)?;
                pad.link(&first.static_pad("sink").context("sink")?)?;
                srcs.push((Feed::Pad(pad), in_caps));
            } else {
                src.set_caps(Some(&in_caps));
                srcs.push((Feed::App(src), in_caps));
            }
        }
        p.set_state(gst::State::Playing)?;
        // Workaround: tttocea708's READY->PAUSED reset forgets roll-up-rows
        // (the 708 window is always 2 rows); setting it again once running
        // reaches the translator.
        if std::env::var_os("S2B_NO_ROWS_FIX").is_none() {
            for (e, r) in &rows_fix {
                e.set_property("roll-up-rows", *r);
            }
        }
        let sync = tracks.len() == 1;
        let srcs: Vec<Feed> = srcs
            .into_iter()
            .map(|(f, caps)| {
                if let Feed::Pad(pad) = &f {
                    pad.push_event(gst::event::StreamStart::new("s2b"));
                    pad.push_event(gst::event::Caps::new(&caps));
                    pad.push_event(gst::event::Segment::new(&gst::FormattedSegment::<gst::ClockTime>::new()));
                }
                f
            })
            .collect();
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
            gap_when_behind: true,
            wait: Duration::from_millis(500),
            stats: Stats::default(),
        })
    }

    /// Sends a GStreamer-transcriber "final transcript" event: the next text
    /// buffer on this track starts on a new row in roll-up (text input only).
    pub fn carriage_return(&self, track: usize) -> bool {
        let Some(src) = self.srcs.get(track) else { return false };
        let ev = gst::event::CustomDownstream::new(gst::Structure::new_empty("rstranscribe/final-transcript"));
        src.event(ev)
    }

    /// Queues `input` on `track` at frame `i`, lasting `frames` frames (pop-on
    /// display time; for roll-up, the frames it may use to transmit).
    pub fn push(&mut self, track: usize, i: u64, frames: u64, input: &Input) -> Result<()> {
        let Some(src) = self.srcs.get(track) else { bail!("no track {track}") };
        let bytes = match input {
            Input::Text(s) | Input::Json(s) => s.as_bytes().to_vec(),
            Input::Raw(b) => b.clone(),
        };
        let mut b = gst::Buffer::from_slice(bytes);
        let (pts, end) = (self.pts(i), self.pts(i + frames.max(1)));
        if let Some(bm) = b.get_mut() {
            bm.set_pts(gst::ClockTime::from_nseconds(pts));
            bm.set_duration(gst::ClockTime::from_nseconds(end - pts));
        }
        src.push(b)?;
        if let Some(f) = self.fed.get_mut(track) {
            *f = true;
        }
        Ok(())
    }

    /// Ends frame `i`: GAPs to idle tracks, then returns the `cc_data` for this
    /// frame (possibly empty) and its lag in frames.
    pub fn frame(&mut self, i: u64) -> (Vec<u8>, u64) {
        let (pts, dur) = (self.pts(i), self.pts(i + 1) - self.pts(i));
        for (k, src) in self.srcs.iter().enumerate() {
            let ahead = self.track_hi.get(k).is_some_and(|h| h.load(Ordering::Relaxed) > i);
            if !self.fed.get(k).copied().unwrap_or(false) && !(self.gap_when_behind && ahead) {
                let ev = gst::event::Gap::builder(gst::ClockTime::from_nseconds(pts))
                    .duration(gst::ClockTime::from_nseconds(dur))
                    .build();
                src.event(ev);
            }
        }
        self.fed.iter_mut().for_each(|f| *f = false);
        self.stats.frames += 1;
        // Wait until the output has reached this frame.
        let t0 = Instant::now();
        let deadline = t0 + self.wait;
        if self.sync {
            // Everything the push produced is already in the appsink.
            while let Some(s) = self.sink.try_pull_sample(gst::ClockTime::ZERO) {
                self.take(&s, i);
            }
        }
        while !self.sync && self.out_hi.is_none_or(|h| h < i) {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                self.stats.timeouts += 1;
                break;
            }
            let Some(s) = self.sink.try_pull_sample(gst::ClockTime::from_nseconds(left.as_nanos() as u64)) else {
                continue;
            };
            self.take(&s, i);
        }
        let w = t0.elapsed().as_micros() as u64;
        self.stats.sum_wait_us += w;
        self.stats.max_wait_us = self.stats.max_wait_us.max(w);
        match self.queue.front() {
            Some((idx, _)) if *idx <= i => {
                let (idx, data) = self.queue.pop_front().unwrap_or_default();
                let lag = i - idx;
                self.stats.max_lag = self.stats.max_lag.max(lag);
                self.stats.sum_lag += lag;
                (data, lag)
            }
            _ => {
                self.stats.empty_frames += 1;
                (Vec::new(), 0)
            }
        }
    }

    fn take(&mut self, s: &gst::Sample, i: u64) {
        let Some(buf) = s.buffer() else { return };
        let Ok(map) = buf.map_readable() else { return };
        let idx = buf.pts().map(|p| self.idx(p.nseconds())).unwrap_or(i);
        self.stats.out_buffers += 1;
        if idx < i {
            self.stats.late_buffers += 1;
        }
        self.out_hi = Some(self.out_hi.map_or(idx, |h| h.max(idx)));
        self.queue.push_back((idx, map.to_vec()));
    }

    /// Output buffers received but not handed out yet.
    pub fn backlog(&self) -> usize {
        self.queue.len()
    }

    /// Bus errors/warnings since the last call.
    pub fn bus_messages(&self) -> Vec<String> {
        let mut v = Vec::new();
        if let Some(bus) = self.pipeline.bus() {
            while let Some(m) = bus.pop() {
                match m.view() {
                    gst::MessageView::Error(e) => v.push(format!("ERROR {}: {} ({:?})", m.src().map(|s| s.name().to_string()).unwrap_or_default(), e.error(), e.debug())),
                    gst::MessageView::Warning(w) => v.push(format!("WARN {}: {} ({:?})", m.src().map(|s| s.name().to_string()).unwrap_or_default(), w.error(), w.debug())),
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

/// Splits `cc_data` bytes into triples.
pub fn triples(data: &[u8]) -> Vec<cc::CcTriple> {
    data.as_chunks::<3>()
        .0
        .iter()
        .map(|t| {
            let ty = match t[0] & 3 {
                0 => cc::CcType::Field1,
                1 => cc::CcType::Field2,
                2 => cc::CcType::DtvccData,
                _ => cc::CcType::DtvccStart,
            };
            cc::CcTriple { valid: t[0] & 4 != 0, cc_type: ty, data: [t[1], t[2]] }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lane(svc: u32, ch: u32) -> TrackCfg {
        TrackCfg::Cea708 { service: svc, cea608_channel: ch, mode: "roll-up".into(), roll_up_rows: 3, origin_row: -1, discard: vec![] }
    }

    #[test]
    fn one_buffer_per_frame_with_text() {
        if gst::init().is_err() || gst::ElementFactory::find("tttocea708").is_none() {
            return;
        }
        for tracks in [vec![lane(1, 1)], vec![lane(1, 1), lane(2, 3)]] {
            let Ok(mut g) = GstCc::new(&tracks, (30, 1)) else { panic!("pipeline") };
            let mut first = Vec::new();
            for i in 0..60 {
                if i == 5 {
                    g.carriage_return(0);
                    assert!(g.push(0, i, 1, &Input::Text("HI".into())).is_ok());
                }
                let (data, lag) = g.frame(i);
                assert_eq!(lag, 0, "frame {i}");
                assert_eq!(data.len(), 60, "frame {i}: 20 triples at 30 fps");
                if i == 5 {
                    first = data;
                }
            }
            // Frame 5 opens a DTVCC packet (cc_type 3) after the two 608 triples.
            assert_eq!(first[6] & 7, 7);
            assert_eq!(g.stats.timeouts, 0);
        }
    }
}
