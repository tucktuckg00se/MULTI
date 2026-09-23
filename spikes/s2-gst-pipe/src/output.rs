//! The persistent output side: appsrc (video, audio) -> parser -> caption
//! stage -> h26xccinserter -> mpegtsmux -> appsink -> one small pipeline per
//! output (appsrc ! srtsink|udpsink).
//!
//! Outputs are separate pipelines instead of `tee` branches so that one
//! failing sink (e.g. an SRT caller whose peer went away) can be restarted on
//! its own; a `tee` would propagate that branch's flow error upstream and stop
//! the muxer for every output.

use std::sync::atomic::Ordering;
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use gst::prelude::*;
use tracing::{info, warn};

use crate::captions::{Line, OursCaptioner};
use crate::stamps::Stamps;
use crate::{CaptionMode, Codec};

pub struct OutputCfg {
    pub codec: Codec,
    pub captions: CaptionMode,
    pub gst608: bool,
    pub lanes: u8,
    pub fps: (i32, i32),
    pub cc_latency_ms: u64,
    pub outputs: Vec<String>,
}

pub struct Output {
    pub pipeline: gst::Pipeline,
    pub video: gst_app::AppSrc,
    pub audio: gst_app::AppSrc,
    /// Approach (a): timed text in.
    pub text: Option<gst_app::AppSrc>,
    /// Approach (b): lines to our encoder.
    pub ours_tx: Option<Sender<Line>>,
    pub sinks: Vec<Arc<OutSink>>,
}

fn make(factory: &str, name: &str) -> Result<gst::Element> {
    gst::ElementFactory::make(factory).name(name).build().with_context(|| format!("missing element {factory}"))
}

fn fps_f64(fps: (i32, i32)) -> f64 {
    if fps.1 == 0 { 0.0 } else { f64::from(fps.0) / f64::from(fps.1) }
}

pub fn build(cfg: &OutputCfg, stamps: Arc<Stamps>) -> Result<Output> {
    let p = gst::Pipeline::with_name("output");
    let vsrc = gst_app::AppSrc::builder()
        .name("vsrc")
        .is_live(true)
        .format(gst::Format::Time)
        .max_time(gst::ClockTime::from_seconds(1))
        .build();
    let asrc = gst_app::AppSrc::builder()
        .name("asrc")
        .is_live(true)
        .format(gst::Format::Time)
        .max_time(gst::ClockTime::from_seconds(1))
        .build();
    for s in [&vsrc, &asrc] {
        // Never block the input thread: drop the oldest buffer if the output stalls.
        s.set_property_from_str("leaky-type", "downstream");
        s.set_property("block", false);
    }
    let (parse_f, ins_f) = match cfg.codec {
        Codec::H264 => ("h264parse", "h264ccinserter"),
        Codec::Hevc => ("h265parse", "h265ccinserter"),
    };
    let vparse = make(parse_f, "vparse")?;
    vparse.set_property("config-interval", -1i32);
    let mux = make("mpegtsmux", "mux")?;
    mux.set_property("alignment", 7i32);
    let aq = make("queue", "aq")?;
    let tsout = gst_app::AppSink::builder().name("tsout").sync(false).async_(false).build();
    p.add_many([vsrc.upcast_ref(), asrc.upcast_ref(), &vparse, &mux, &aq, tsout.upcast_ref()])?;
    gst::Element::link_many([asrc.upcast_ref(), &aq, &mux])?;
    mux.link(&tsout)?;
    vsrc.link(&vparse)?;

    let mut text = None;
    let mut ours_tx = None;
    let fps = cfg.fps;
    let cc_caps = gst::Caps::builder("closedcaption/x-cea-708")
        .field("format", "cc_data")
        .field("framerate", gst::Fraction::new(fps.0, fps.1))
        .build();

    // The element whose src pad feeds mpegtsmux's video pad.
    let last: gst::Element = match cfg.captions {
        CaptionMode::None => vparse.clone(),
        CaptionMode::Ours => {
            let ins = make(ins_f, "ccinsert")?;
            ins.set_property("remove-caption-meta", true);
            p.add(&ins)?;
            vparse.link(&ins)?;
            let (tx, rx) = std::sync::mpsc::channel();
            ours_tx = Some(tx);
            let cap = Mutex::new(OursCaptioner::new(rx, cfg.lanes));
            let st = stamps.clone();
            let src = vparse.static_pad("src").context("vparse src")?;
            let fps_f = fps_f64(fps);
            src.add_probe(gst::PadProbeType::BUFFER, move |pad, info| {
                let Some(pts) = info.buffer().and_then(|b| b.pts()) else { return gst::PadProbeReturn::Ok };
                let pts = pts.nseconds() as i64;
                st.on_cc_in(pts);
                // Prefer the negotiated frame rate; fall back to --fps.
                let caps_fps = pad
                    .current_caps()
                    .and_then(|c| c.structure(0).and_then(|s| s.get::<gst::Fraction>("framerate").ok()))
                    .filter(|f| f.numer() > 0 && f.denom() > 0)
                    .map(|f| f64::from(f.numer()) / f64::from(f.denom()))
                    .unwrap_or(fps_f);
                let data = match cap.lock() {
                    Ok(mut c) => c.meta_for(pts, caps_fps),
                    Err(_) => None,
                };
                if let (Some(data), Some(buf)) = (data, info.buffer_mut()) {
                    let b = buf.make_mut();
                    gst_video::VideoCaptionMeta::add(b, gst_video::VideoCaptionType::Cea708Raw, &data);
                    st.counters.cc_frames.fetch_add(1, Ordering::Relaxed);
                }
                gst::PadProbeReturn::Ok
            });
            ins
        }
        CaptionMode::Gst => {
            let comb = make("cccombiner", "ccomb")?;
            comb.set_property("latency", cfg.cc_latency_ms * 1_000_000);
            let ins = make(ins_f, "ccinsert")?;
            ins.set_property("remove-caption-meta", true);
            let tsrc = gst_app::AppSrc::builder()
                .name("textsrc")
                .is_live(true)
                .format(gst::Format::Time)
                .do_timestamp(true)
                .caps(&gst::Caps::builder("text/x-raw").field("format", "utf8").build())
                .build();
            let ccf = gst::ElementFactory::make("capsfilter").name("cccaps").property("caps", &cc_caps).build()?;
            p.add_many([&comb, &ins, tsrc.upcast_ref(), &ccf])?;
            vparse.link_pads(Some("src"), &comb, Some("sink"))?;
            comb.link(&ins)?;
            if cfg.gst608 {
                if cfg.lanes > 1 {
                    bail!("tttocea608 only writes CC1 (field 1); use the 708 element for 2 lanes");
                }
                let t = make("tttocea608", "tt608")?;
                t.set_property_from_str("mode", "roll-up3");
                let conv = make("ccconverter", "ccconv")?;
                p.add_many([&t, &conv])?;
                gst::Element::link_many([tsrc.upcast_ref(), &t, &conv, &ccf])?;
            } else if cfg.lanes == 1 {
                let t = make("tttocea708", "tt708a")?;
                t.set_property("cea608-channel", 1u32);
                t.set_property("roll-up-rows", 3u32);
                p.add(&t)?;
                gst::Element::link_many([tsrc.upcast_ref(), &t, &ccf])?;
            } else {
                // One tttocea708 per lane (CC1+svc1, CC3+svc2), merged by cea708mux.
                let tee = make("tee", "texttee")?;
                let m = make("cea708mux", "c708mux")?;
                p.add_many([&tee, &m])?;
                tsrc.link(&tee)?;
                for (i, (ch, svc)) in [(1u32, 1u32), (3, 2)].into_iter().enumerate() {
                    let q = make("queue", &format!("tq{i}"))?;
                    let t = make("tttocea708", &format!("tt708{i}"))?;
                    t.set_property("cea608-channel", ch);
                    t.set_property("service-number", svc);
                    t.set_property("roll-up-rows", 3u32);
                    let f = gst::ElementFactory::make("capsfilter").property("caps", &cc_caps).build()?;
                    p.add_many([&q, &t, &f])?;
                    gst::Element::link_many([&tee, &q, &t, &f])?;
                    f.link(&m)?;
                }
                m.link(&ccf)?;
            }
            let cpad = comb.request_pad_simple("caption").context("cccombiner caption pad")?;
            let fsrc = ccf.static_pad("src").context("cccaps src")?;
            fsrc.link(&cpad)?;
            // Stage timing: into cccombiner and out of the inserter.
            if let Some(pad) = comb.static_pad("sink") {
                let st = stamps.clone();
                pad.add_probe(gst::PadProbeType::BUFFER, move |_, info| {
                    if let Some(pts) = info.buffer().and_then(|b| b.pts()) {
                        st.on_cc_in(pts.nseconds() as i64);
                    }
                    gst::PadProbeReturn::Ok
                });
            }
            text = Some(tsrc);
            ins
        }
    };
    if cfg.captions != CaptionMode::None {
        let st = stamps.clone();
        let src = last.static_pad("src").context("caption stage src")?;
        src.add_probe(gst::PadProbeType::BUFFER, move |_, info| {
            if let Some(pts) = info.buffer().and_then(|b| b.pts()) {
                st.on_cc_out(pts.nseconds() as i64);
            }
            gst::PadProbeReturn::Ok
        });
    }
    let mux_v = mux.request_pad_simple("sink_%d").context("mux video pad")?;
    last.static_pad("src").context("video src")?.link(&mux_v)?;
    let st = stamps.clone();
    mux_v.add_probe(gst::PadProbeType::BUFFER, move |_, info| {
        if let Some(pts) = info.buffer().and_then(|b| b.pts()) {
            st.on_mux_in(pts.nseconds() as i64);
        }
        gst::PadProbeReturn::Ok
    });

    let sinks: Vec<Arc<OutSink>> = cfg.outputs.iter().map(|u| Arc::new(OutSink::new(u.clone()))).collect();
    let fan = sinks.clone();
    tsout.set_callbacks(
        gst_app::AppSinkCallbacks::builder()
            .new_sample(move |s| {
                let Ok(sample) = s.pull_sample() else { return Err(gst::FlowError::Eos) };
                if let Some(buf) = sample.buffer_owned() {
                    for o in &fan {
                        o.push(buf.clone());
                    }
                }
                Ok(gst::FlowSuccess::Ok)
            })
            .build(),
    );
    Ok(Output { pipeline: p, video: vsrc, audio: asrc, text, ours_tx, sinks })
}

/// One output (`appsrc ! srtsink|udpsink`), rebuilt on its own when it fails.
pub struct OutSink {
    pub uri: String,
    inner: Mutex<Option<(gst::Pipeline, gst_app::AppSrc)>>,
    retry_at: Mutex<Option<Instant>>,
    backoff: Mutex<Duration>,
}

impl OutSink {
    fn new(uri: String) -> Self {
        Self {
            uri,
            inner: Mutex::new(None),
            retry_at: Mutex::new(Some(Instant::now())),
            backoff: Mutex::new(Duration::from_millis(500)),
        }
    }

    fn try_build(&self) -> Result<(gst::Pipeline, gst_app::AppSrc)> {
        let p = gst::Pipeline::with_name(&format!("out-{}", self.uri));
        let src = gst_app::AppSrc::builder()
            .is_live(true)
            .format(gst::Format::Bytes)
            .caps(&gst::Caps::builder("video/mpegts").field("systemstream", true).field("packetsize", 188i32).build())
            .max_bytes(4 << 20)
            .build();
        src.set_property_from_str("leaky-type", "downstream");
        src.set_property("block", false);
        let sink = gst::Element::make_from_uri(gst::URIType::Sink, &self.uri, None)
            .with_context(|| format!("no sink for {}", self.uri))?;
        if sink.has_property("sync") {
            sink.set_property("sync", false);
        }
        if sink.has_property("async") {
            sink.set_property("async", false);
        }
        if sink.has_property("wait-for-connection") {
            sink.set_property("wait-for-connection", false);
        }
        p.add_many([src.upcast_ref(), &sink])?;
        src.link(&sink)?;
        p.set_state(gst::State::Playing)?;
        Ok((p, src))
    }

    fn push(&self, buf: gst::Buffer) {
        if let Ok(g) = self.inner.lock()
            && let Some((_, src)) = g.as_ref()
        {
            let _ = src.push_buffer(buf);
        }
    }

    /// Called periodically from the supervisor: drains the bus, restarts on error.
    pub fn poll(&self, stamps: &Stamps) {
        let mut failed = false;
        if let Ok(g) = self.inner.lock()
            && let Some((p, _)) = g.as_ref()
            && let Some(bus) = p.bus()
        {
            while let Some(msg) = bus.pop() {
                match msg.view() {
                    gst::MessageView::Error(e) => {
                        warn!(uri = %self.uri, err = %e.error(), dbg = ?e.debug(), "output error; restarting it");
                        stamps.counters.output_errors.fetch_add(1, Ordering::Relaxed);
                        failed = true;
                    }
                    gst::MessageView::Warning(w) => warn!(uri = %self.uri, warn = %w.error(), "output warning"),
                    gst::MessageView::Eos(_) => failed = true,
                    _ => {}
                }
            }
        }
        if failed {
            if let Ok(mut g) = self.inner.lock()
                && let Some((p, _)) = g.take()
            {
                let _ = p.set_state(gst::State::Null);
            }
            if let (Ok(mut r), Ok(b)) = (self.retry_at.lock(), self.backoff.lock()) {
                *r = Some(Instant::now() + *b);
            }
        }
        let due = self.retry_at.lock().ok().and_then(|r| *r).is_some_and(|t| Instant::now() >= t);
        if due {
            match self.try_build() {
                Ok(x) => {
                    info!(uri = %self.uri, "output started");
                    if let Ok(mut g) = self.inner.lock() {
                        *g = Some(x);
                    }
                    if let Ok(mut r) = self.retry_at.lock() {
                        *r = None;
                    }
                    if let Ok(mut b) = self.backoff.lock() {
                        *b = Duration::from_millis(500);
                    }
                }
                Err(e) => {
                    warn!(uri = %self.uri, err = %e, "output start failed");
                    stamps.counters.output_errors.fetch_add(1, Ordering::Relaxed);
                    if let (Ok(mut r), Ok(mut b)) = (self.retry_at.lock(), self.backoff.lock()) {
                        *r = Some(Instant::now() + *b);
                        *b = (*b * 2).min(Duration::from_secs(5));
                    }
                }
            }
        }
    }

    pub fn stop(&self) {
        if let Ok(mut g) = self.inner.lock()
            && let Some((p, _)) = g.take()
        {
            let _ = p.set_state(gst::State::Null);
        }
    }
}
