//! The persistent output side: appsrc (video, audio) -> h26xparse ->
//! [pad probe: caption lanes attach `cc_data` meta] -> h26xccinserter ->
//! mpegtsmux -> appsink -> one [`OutSink`] pipeline per output.
//!
//! The output pipeline is built when the first video stream appears (its codec
//! picks the parser and inserter) and rebuilt if the codec changes or the
//! pipeline fails. Outputs are separate pipelines instead of `tee` branches,
//! so one failing sink is restarted on its own and never stops the others.
//! SRT/UDP outputs get the muxed MPEG-TS; RTMP outputs get the elementary
//! streams as they enter the muxer (video already carrying the caption SEI)
//! and mux them to FLV.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use gst::prelude::*;
use tracing::{error, info, warn};

use crate::bridge::{AUDIO, Bridge, Targets, VIDEO};
use crate::captions::{Captioner, FALLBACK_FPS};
use crate::stats::{Counters, OutputStats, inc};
use crate::url::{OutputKind, output_kind, redact, srt_uri, udp_uri};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Codec {
    H264,
    Hevc,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

fn make(factory: &str, name: &str) -> Result<gst::Element> {
    gst::ElementFactory::make(factory)
        .name(name)
        .build()
        .with_context(|| format!("missing GStreamer element {factory}"))
}

struct Side {
    codec: Codec,
    pipeline: gst::Pipeline,
}

/// Owns the output pipeline and the output sinks.
pub(crate) struct OutputManager {
    side: Mutex<Option<Side>>,
    bridge: Arc<Bridge>,
    captioner: Arc<Mutex<Captioner>>,
    counters: Arc<Counters>,
    pub sinks: Arc<Vec<Arc<OutSink>>>,
}

impl OutputManager {
    pub fn new(
        bridge: Arc<Bridge>,
        captioner: Captioner,
        counters: Arc<Counters>,
        outputs: &[String],
        srt_latency_ms: u32,
    ) -> Self {
        let sinks = outputs
            .iter()
            .map(|u| Arc::new(OutSink::new(u.clone(), srt_latency_ms)))
            .collect();
        Self {
            side: Mutex::new(None),
            bridge,
            captioner: Arc::new(Mutex::new(captioner)),
            counters,
            sinks: Arc::new(sinks),
        }
    }

    /// Makes sure an output pipeline for `codec` is running.
    pub fn ensure(&self, codec: Codec) -> Result<()> {
        let mut side = lock(&self.side);
        if side.as_ref().is_some_and(|s| s.codec == codec) {
            return Ok(());
        }
        if let Some(old) = side.take() {
            warn!(from = ?old.codec, to = ?codec, "video codec changed; rebuilding the output pipeline");
            self.bridge.set_targets(None);
            let _ = old.pipeline.set_state(gst::State::Null);
        }
        let (pipeline, targets) = self.build(codec)?;
        pipeline
            .set_state(gst::State::Playing)
            .context("output pipeline failed to start")?;
        self.bridge.set_targets(Some(targets));
        info!(?codec, "output pipeline started");
        *side = Some(Side { codec, pipeline });
        Ok(())
    }

    /// Drains the output pipeline's bus; on an error the pipeline is rebuilt.
    pub fn poll(&self) {
        let failed = {
            let side = lock(&self.side);
            let Some(s) = side.as_ref() else { return };
            let Some(bus) = s.pipeline.bus() else { return };
            let mut failed = None;
            while let Some(msg) = bus.pop() {
                match msg.view() {
                    gst::MessageView::Error(e) => {
                        error!(src = ?msg.src().map(|s| s.path_string()), err = %e.error(), dbg = ?e.debug(), "output pipeline error");
                        inc(&self.counters.output_pipeline_errors);
                        failed = Some(s.codec);
                    }
                    gst::MessageView::Warning(w) => {
                        warn!(err = %w.error(), dbg = ?w.debug(), "output pipeline warning")
                    }
                    _ => {}
                }
            }
            failed
        };
        if let Some(codec) = failed {
            self.stop_pipeline();
            if let Err(e) = self.ensure(codec) {
                error!(err = %format!("{e:#}"), "output pipeline rebuild failed");
            }
        }
        if let Ok(mut c) = self.captioner.lock() {
            c.poll_bus();
            self.counters
                .caption_errors
                .store(c.errors, Ordering::Relaxed);
        }
    }

    fn stop_pipeline(&self) {
        self.bridge.set_targets(None);
        if let Some(s) = lock(&self.side).take() {
            let _ = s.pipeline.set_state(gst::State::Null);
        }
    }

    pub fn stop(&self) {
        self.stop_pipeline();
        for s in self.sinks.iter() {
            s.stop();
        }
    }

    fn build(&self, codec: Codec) -> Result<(gst::Pipeline, Targets)> {
        let p = gst::Pipeline::with_name("output");
        let vsrc = gst_app::AppSrc::builder()
            .name("vsrc")
            .is_live(true)
            .format(gst::Format::Time)
            .max_time(gst::ClockTime::from_seconds(2))
            // The default max-bytes (200 kB) is below one startup hold of
            // 720p video; with leaky-type=downstream it silently dropped
            // frames (S2 gotcha 2).
            .max_bytes(32 << 20)
            .build();
        let asrc = gst_app::AppSrc::builder()
            .name("asrc")
            .is_live(true)
            .format(gst::Format::Time)
            .max_time(gst::ClockTime::from_seconds(2))
            .max_bytes(4 << 20)
            .build();
        for s in [&vsrc, &asrc] {
            // Never block the input thread: drop the oldest buffer if the output stalls.
            s.set_property_from_str("leaky-type", "downstream");
            s.set_property("block", false);
        }
        let (parse_f, ins_f) = match codec {
            Codec::H264 => ("h264parse", "h264ccinserter"),
            Codec::Hevc => ("h265parse", "h265ccinserter"),
        };
        let vparse = make(parse_f, "vparse")?;
        vparse.set_property("config-interval", -1i32);
        let ins = make(ins_f, "ccinsert")?;
        ins.set_property("remove-caption-meta", true);
        let mux = make("mpegtsmux", "mux")?;
        mux.set_property("alignment", 7i32);
        let aq = make("queue", "aq")?;
        let tsout = gst_app::AppSink::builder()
            .name("tsout")
            .sync(false)
            .async_(false)
            .build();
        p.add_many([
            vsrc.upcast_ref(),
            asrc.upcast_ref(),
            &vparse,
            &ins,
            &mux,
            &aq,
            tsout.upcast_ref(),
        ])?;
        // Fixed PIDs, video first in the PMT (pad name = PID): with audio
        // first, ffmpeg's probe sometimes failed on the stream.
        let mux_v = mux
            .request_pad_simple("sink_256")
            .context("mux video pad")?;
        let mux_a = mux
            .request_pad_simple("sink_257")
            .context("mux audio pad")?;
        gst::Element::link_many([asrc.upcast_ref(), &aq])?;
        aq.static_pad("src").context("aq src")?.link(&mux_a)?;
        mux.link(&tsout)?;
        gst::Element::link_many([vsrc.upcast_ref(), &vparse, &ins])?;
        ins.static_pad("src").context("ins src")?.link(&mux_v)?;

        let cap = self.captioner.clone();
        let counters = self.counters.clone();
        let warned = AtomicBool::new(false);
        vparse.static_pad("src").context("vparse src")?.add_probe(
            gst::PadProbeType::BUFFER,
            move |pad, info| {
                let Some(pts) = info.buffer().and_then(|b| b.pts()) else {
                    return gst::PadProbeReturn::Ok;
                };
                let fps = pad
                    .current_caps()
                    .and_then(|c| {
                        c.structure(0)
                            .and_then(|s| s.get::<gst::Fraction>("framerate").ok())
                    })
                    .filter(|f| f.numer() > 0 && f.denom() > 0)
                    .map(|f| (f.numer(), f.denom()));
                let fps = fps.unwrap_or_else(|| {
                    if !warned.swap(true, Ordering::Relaxed) {
                        warn!(
                            "video caps carry no frame rate; captions assume {}/{} fps",
                            FALLBACK_FPS.0, FALLBACK_FPS.1
                        );
                    }
                    FALLBACK_FPS
                });
                let data = match cap.lock() {
                    Ok(mut c) => c.meta_for(pts.nseconds() as i64, fps),
                    Err(_) => None,
                };
                if let (Some(data), Some(buf)) = (data, info.buffer_mut()) {
                    let b = buf.make_mut();
                    gst_video::VideoCaptionMeta::add(
                        b,
                        gst_video::VideoCaptionType::Cea708Raw,
                        &data,
                    );
                    inc(&counters.caption_frames);
                }
                gst::PadProbeReturn::Ok
            },
        );
        // Frames out, and the elementary streams (video with caption SEI)
        // for RTMP outputs, taken where they enter the muxer.
        let counters = self.counters.clone();
        let fan = self.sinks.clone();
        mux_v.add_probe(gst::PadProbeType::BUFFER, move |pad, info| {
            inc(&counters.frames_out);
            if let Some(buf) = info.buffer() {
                for o in fan.iter() {
                    o.push_es(VIDEO, pad, buf);
                }
            }
            gst::PadProbeReturn::Ok
        });
        let fan = self.sinks.clone();
        mux_a.add_probe(gst::PadProbeType::BUFFER, move |pad, info| {
            if let Some(buf) = info.buffer() {
                for o in fan.iter() {
                    o.push_es(AUDIO, pad, buf);
                }
            }
            gst::PadProbeReturn::Ok
        });

        let fan = self.sinks.clone();
        tsout.set_callbacks(
            gst_app::AppSinkCallbacks::builder()
                .new_sample(move |s| {
                    let Ok(sample) = s.pull_sample() else {
                        return Err(gst::FlowError::Eos);
                    };
                    if let Some(buf) = sample.buffer_owned() {
                        for o in fan.iter() {
                            o.push_ts(&buf);
                        }
                    }
                    Ok(gst::FlowSuccess::Ok)
                })
                .build(),
        );
        Ok((
            p.clone(),
            Targets {
                pipeline: p,
                video: vsrc,
                audio: asrc,
            },
        ))
    }
}

const BACKOFF_INITIAL: Duration = Duration::from_millis(500);
const BACKOFF_MAX: Duration = Duration::from_secs(10);
/// An output running this long without errors resets its backoff.
const HEALTHY_RESET: Duration = Duration::from_secs(30);

enum Feed {
    /// MPEG-TS bytes.
    Ts(gst_app::AppSrc),
    /// Elementary streams for FLV: [video, audio].
    Es([gst_app::AppSrc; 2]),
}

struct Running {
    pipeline: gst::Pipeline,
    feed: Feed,
}

struct SinkState {
    running: Option<Running>,
    retry_at: Option<Instant>,
    backoff: Duration,
    started: Option<Instant>,
}

/// One output: `appsrc ! srtsink|udpsink` (MPEG-TS), or for RTMP
/// `appsrc ! h264parse ! flvmux ! rtmp2sink` plus `appsrc ! aacparse ! flvmux`.
/// Rebuilt on its own with backoff when it fails.
pub(crate) struct OutSink {
    url: String,
    kind: Option<OutputKind>,
    srt_latency_ms: u32,
    st: Mutex<SinkState>,
    errors: AtomicU64,
    starts: AtomicU64,
    /// Logged once: this output cannot carry the current video codec.
    codec_warned: AtomicBool,
}

fn leaky_src(format: gst::Format, live: bool, max_bytes: u64) -> gst_app::AppSrc {
    let src = gst_app::AppSrc::builder()
        .is_live(live)
        .format(format)
        .max_bytes(max_bytes)
        .build();
    // Never block the output pipeline: drop the oldest data instead.
    src.set_property_from_str("leaky-type", "downstream");
    src.set_property("block", false);
    src
}

impl OutSink {
    fn new(url: String, srt_latency_ms: u32) -> Self {
        Self {
            kind: output_kind(&url).ok(),
            url,
            srt_latency_ms,
            st: Mutex::new(SinkState {
                running: None,
                retry_at: Some(Instant::now()),
                backoff: BACKOFF_INITIAL,
                started: None,
            }),
            errors: AtomicU64::new(0),
            starts: AtomicU64::new(0),
            codec_warned: AtomicBool::new(false),
        }
    }

    pub fn stats(&self) -> OutputStats {
        OutputStats {
            url: redact(&self.url),
            running: lock(&self.st).running.is_some(),
            errors: self.errors.load(Ordering::Relaxed),
            starts: self.starts.load(Ordering::Relaxed),
        }
    }

    fn try_build(&self) -> Result<Running> {
        let p = gst::Pipeline::new();
        let kind = output_kind(&self.url)?;
        let feed = match kind {
            OutputKind::Srt | OutputKind::Udp => {
                let src = leaky_src(gst::Format::Bytes, true, 4 << 20);
                src.set_caps(Some(
                    &gst::Caps::builder("video/mpegts")
                        .field("systemstream", true)
                        .field("packetsize", 188i32)
                        .build(),
                ));
                let uri = if kind == OutputKind::Srt {
                    srt_uri(&self.url, self.srt_latency_ms)
                } else {
                    udp_uri(&self.url)
                };
                let sink = gst::Element::make_from_uri(gst::URIType::Sink, &uri, None)
                    .with_context(|| format!("no sink for {}", redact(&self.url)))?;
                for (prop, v) in [
                    ("sync", false),
                    ("async", false),
                    ("wait-for-connection", false),
                ] {
                    if sink.has_property(prop) {
                        sink.set_property(prop, v);
                    }
                }
                p.add_many([src.upcast_ref(), &sink])?;
                src.link(&sink)?;
                Feed::Ts(src)
            }
            OutputKind::Rtmp => Feed::Es(rtmp_chain(&p, &self.url)?),
        };
        p.set_state(gst::State::Playing)?;
        Ok(Running { pipeline: p, feed })
    }

    /// Muxed MPEG-TS for SRT/UDP outputs.
    pub fn push_ts(&self, buf: &gst::Buffer) {
        if let Some(Running {
            feed: Feed::Ts(src),
            ..
        }) = lock(&self.st).running.as_ref()
        {
            let _ = src.push_buffer(buf.clone());
        }
    }

    /// One elementary-stream buffer (as it enters `mpegtsmux` on `pad`) for
    /// RTMP outputs.
    pub fn push_es(&self, stream: usize, pad: &gst::Pad, buf: &gst::Buffer) {
        if self.kind != Some(OutputKind::Rtmp) {
            return;
        }
        let st = lock(&self.st);
        let Some(Running {
            feed: Feed::Es(srcs),
            ..
        }) = st.running.as_ref()
        else {
            return;
        };
        let Some(src) = srcs.get(stream) else { return };
        let caps = pad.current_caps();
        if src.caps() != caps
            && let Some(caps) = caps
        {
            let name = caps
                .structure(0)
                .map(|s| s.name().to_string())
                .unwrap_or_default();
            if !matches!(name.as_str(), "video/x-h264" | "audio/mpeg") {
                if !self.codec_warned.swap(true, Ordering::Relaxed) {
                    warn!(url = %redact(&self.url), stream = %name, "RTMP carries H.264 and AAC only; stream left out");
                }
                return;
            }
            src.set_caps(Some(&caps));
        }
        if src.caps().is_some() {
            let _ = src.push_buffer(buf.clone());
        }
    }

    /// Called periodically: drains the bus, restarts after errors with backoff.
    pub fn poll(&self) {
        let shown = redact(&self.url);
        let mut st = lock(&self.st);
        let mut failed = false;
        if let Some(r) = st.running.as_ref()
            && let Some(bus) = r.pipeline.bus()
        {
            while let Some(msg) = bus.pop() {
                match msg.view() {
                    gst::MessageView::Error(e) => {
                        warn!(url = %shown, err = %e.error(), dbg = ?e.debug(), "output error; restarting it");
                        self.errors.fetch_add(1, Ordering::Relaxed);
                        failed = true;
                    }
                    gst::MessageView::Warning(w) => {
                        warn!(url = %shown, warn = %w.error(), "output warning")
                    }
                    gst::MessageView::Eos(_) => failed = true,
                    _ => {}
                }
            }
        }
        if failed {
            if let Some(r) = st.running.take() {
                let _ = r.pipeline.set_state(gst::State::Null);
            }
            st.retry_at = Some(Instant::now() + st.backoff);
            st.backoff = (st.backoff * 2).min(BACKOFF_MAX);
        }
        if st.running.is_some() {
            // Running a while without errors resets the backoff.
            if st.started.is_some_and(|t| t.elapsed() >= HEALTHY_RESET) {
                st.backoff = BACKOFF_INITIAL;
            }
            return;
        }
        if st.retry_at.is_some_and(|t| Instant::now() >= t) {
            match self.try_build() {
                Ok(r) => {
                    info!(url = %shown, "output started");
                    self.starts.fetch_add(1, Ordering::Relaxed);
                    st.running = Some(r);
                    st.retry_at = None;
                    st.started = Some(Instant::now());
                }
                Err(e) => {
                    warn!(url = %shown, err = %format!("{e:#}"), retry_in_ms = st.backoff.as_millis() as u64, "output start failed");
                    self.errors.fetch_add(1, Ordering::Relaxed);
                    st.retry_at = Some(Instant::now() + st.backoff);
                    st.backoff = (st.backoff * 2).min(BACKOFF_MAX);
                }
            }
        }
    }

    pub fn stop(&self) {
        let mut st = lock(&self.st);
        if let Some(r) = st.running.take() {
            let _ = r.pipeline.set_state(gst::State::Null);
        }
        st.retry_at = None;
    }
}

/// `appsrc ! queue ! h264parse ! flvmux ! rtmp2sink` and
/// `appsrc ! queue ! aacparse ! flvmux`. FLV keeps the H.264 SEI, so the
/// captions survive. Not live: flvmux interleaves by timestamp as data
/// arrives, so the upstream running time does not matter. Needs both video
/// and audio (as RTMP services do).
fn rtmp_chain(p: &gst::Pipeline, url: &str) -> Result<[gst_app::AppSrc; 2]> {
    let flv = make("flvmux", "flv")?;
    flv.set_property("streamable", true);
    let sink = make("rtmp2sink", "rtmp")?;
    sink.set_property("location", url);
    sink.set_property("sync", false);
    sink.set_property("async", false);
    p.add_many([&flv, &sink])?;
    flv.link(&sink)?;
    let mut srcs = Vec::new();
    for (parse_f, pad, max_bytes) in [
        ("h264parse", "video", 32u64 << 20),
        ("aacparse", "audio", 4 << 20),
    ] {
        let src = leaky_src(gst::Format::Time, false, max_bytes);
        let q = make("queue", &format!("q_{pad}"))?;
        let parse = make(parse_f, &format!("parse_{pad}"))?;
        p.add_many([src.upcast_ref(), &q, &parse])?;
        gst::Element::link_many([src.upcast_ref(), &q, &parse])?;
        let fp = flv
            .request_pad_simple(pad)
            .with_context(|| format!("flvmux {pad} pad"))?;
        parse.static_pad("src").context("parser src")?.link(&fp)?;
        srcs.push(src);
    }
    srcs.try_into()
        .map_err(|_| anyhow::anyhow!("RTMP chain needs two sources"))
}
