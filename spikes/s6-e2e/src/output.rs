//! The persistent output side, as S2's `GstDirect` mode: appsrc (video, audio)
//! -> h26xparse -> [pad probe: Captioner attaches cc_data meta] ->
//! h26xccinserter -> mpegtsmux -> appsink -> one `OutSink` pipeline per output.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use gst::prelude::*;
use s2_gst_pipe::Codec;
use s2_gst_pipe::output::OutSink;
use s2_gst_pipe::stamps::Stamps;
use tracing::{info, warn};

use crate::captioner::Captioner;

pub struct Output {
    pub pipeline: gst::Pipeline,
    pub video: gst_app::AppSrc,
    pub audio: gst_app::AppSrc,
    pub sinks: Vec<Arc<OutSink>>,
    pub captioner: Arc<Mutex<Captioner>>,
}

fn make(factory: &str, name: &str) -> Result<gst::Element> {
    gst::ElementFactory::make(factory).name(name).build().with_context(|| format!("missing element {factory}"))
}

pub fn build(codec: Codec, outputs: &[String], cap: Captioner, stamps: Arc<Stamps>) -> Result<Output> {
    let p = gst::Pipeline::with_name("output");
    let vsrc = gst_app::AppSrc::builder()
        .name("vsrc")
        .is_live(true)
        .format(gst::Format::Time)
        .max_time(gst::ClockTime::from_seconds(2))
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
    let tsout = gst_app::AppSink::builder().name("tsout").sync(false).async_(false).build();
    p.add_many([vsrc.upcast_ref(), asrc.upcast_ref(), &vparse, &ins, &mux, &aq, tsout.upcast_ref()])?;
    let mux_v = mux.request_pad_simple("sink_256").context("mux video pad")?;
    let mux_a = mux.request_pad_simple("sink_257").context("mux audio pad")?;
    gst::Element::link_many([asrc.upcast_ref(), &aq])?;
    aq.static_pad("src").context("aq src")?.link(&mux_a)?;
    mux.link(&tsout)?;
    gst::Element::link_many([vsrc.upcast_ref(), &vparse, &ins])?;
    ins.static_pad("src").context("ins src")?.link(&mux_v)?;

    let cap = Arc::new(Mutex::new(cap));
    let c2 = cap.clone();
    let st = stamps.clone();
    let n = AtomicU64::new(0);
    vparse.static_pad("src").context("vparse src")?.add_probe(gst::PadProbeType::BUFFER, move |_, info| {
        let Some(pts) = info.buffer().and_then(|b| b.pts()) else { return gst::PadProbeReturn::Ok };
        let pts = pts.nseconds() as i64;
        st.on_cc_in(pts);
        let data = match c2.lock() {
            Ok(mut c) => {
                if n.fetch_add(1, Ordering::Relaxed) % 900 == 899 {
                    let (max, avg) = c.wait_us();
                    info!(max_wait_us = max, avg_wait_us = avg, pushed = ?c.pushed, dropped = ?c.dropped, "caption stage");
                    for m in c.bus_messages() {
                        warn!(msg = %m, "caption encoder bus");
                    }
                }
                c.meta_for(pts)
            }
            Err(_) => None,
        };
        if let (Some(data), Some(buf)) = (data, info.buffer_mut()) {
            let b = buf.make_mut();
            gst_video::VideoCaptionMeta::add(b, gst_video::VideoCaptionType::Cea708Raw, &data);
            st.counters.cc_frames.fetch_add(1, Ordering::Relaxed);
        }
        gst::PadProbeReturn::Ok
    });
    let st = stamps.clone();
    ins.static_pad("src").context("ins src")?.add_probe(gst::PadProbeType::BUFFER, move |_, info| {
        if let Some(pts) = info.buffer().and_then(|b| b.pts()) {
            st.on_cc_out(pts.nseconds() as i64);
        }
        gst::PadProbeReturn::Ok
    });
    let st = stamps;
    mux_v.add_probe(gst::PadProbeType::BUFFER, move |_, info| {
        if let Some(pts) = info.buffer().and_then(|b| b.pts()) {
            st.on_mux_in(pts.nseconds() as i64);
        }
        gst::PadProbeReturn::Ok
    });

    let sinks: Vec<Arc<OutSink>> = outputs.iter().map(|u| Arc::new(OutSink::new(u.clone()))).collect();
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
    Ok(Output { pipeline: p, video: vsrc, audio: asrc, sinks, captioner: cap })
}
