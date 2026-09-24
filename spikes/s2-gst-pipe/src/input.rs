//! The restartable input side: srtsrc|udpsrc -> tsdemux -> parser -> appsink
//! (video) and tsdemux -> appsink (audio, untouched). Video is never decoded.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Context, Result};
use gst::prelude::*;
use tracing::{info, warn};

use crate::Codec;
use crate::bridge::{AUDIO, Bridge, VIDEO};
use crate::stamps::{Stamps, wall_ns};

pub struct Input {
    pub pipeline: gst::Pipeline,
    /// Wallclock ns of the last buffer from the demuxer (0 = none yet).
    pub last_data: Arc<AtomicU64>,
}

fn appsink(name: &str, bridge: Arc<Bridge>, stream: usize, last: Arc<AtomicU64>) -> gst_app::AppSink {
    // async=false: udpsrc is not a live source, so two prerolling sinks fed by one
    // tsdemux thread would deadlock (the first sink blocks in preroll).
    let s = gst_app::AppSink::builder().name(name).sync(false).async_(false).build();
    s.set_callbacks(
        gst_app::AppSinkCallbacks::builder()
            .new_sample(move |s| {
                let Ok(sample) = s.pull_sample() else { return Err(gst::FlowError::Eos) };
                last.store(wall_ns(), Ordering::Relaxed);
                bridge.push(stream, &sample)
            })
            .build(),
    );
    s
}

/// `in_parse`: run the video through h26xparse (alignment=au) before the
/// bridge. Without it the demuxer's PES is forwarded as one access unit and
/// the output parser is told so; this saves one frame of delay, because a
/// parser fed an unaligned byte-stream can only close an AU when the next one
/// starts.
pub fn build(uri: &str, codec: Codec, in_parse: bool, pcr: bool, bridge: Arc<Bridge>, stamps: Arc<Stamps>) -> Result<Input> {
    let p = gst::Pipeline::with_name("input");
    let src = gst::Element::make_from_uri(gst::URIType::Src, uri, Some("insrc"))
        .with_context(|| format!("no source for {uri}"))?;
    if uri.starts_with("udp") {
        src.set_property("caps", gst::Caps::builder("video/mpegts").field("systemstream", true).build());
        // Default SO_RCVBUF drops datagrams at 3 Mbit/s bursts (I-frames).
        src.set_property("buffer-size", 8 << 20_i32);
    }
    let demux = gst::ElementFactory::make("tsdemux").name("demux").build()?;
    // Default: ignore PCR, so buffer PTS are the PES PTS exactly and the
    // bridge does the clock locking. With PCR clock recovery (`pcr`), tsdemux
    // skew-corrects PTS by a few 90 kHz ticks per frame and, for ~1.5 s after
    // every start, emits PTS that step backwards (non-monotonic DTS out).
    demux.set_property("ignore-pcr", !pcr);
    let parse_f = match codec {
        Codec::H264 => "h264parse",
        Codec::Hevc => "h265parse",
    };
    let vparse = gst::ElementFactory::make(parse_f).name("in_vparse").build()?;
    let vcaps = gst::ElementFactory::make("capsfilter")
        .property(
            "caps",
            gst::Caps::builder(if codec == Codec::H264 { "video/x-h264" } else { "video/x-h265" })
                .field("stream-format", "byte-stream")
                .field("alignment", "au")
                .build(),
        )
        .build()?;
    let last = Arc::new(AtomicU64::new(0));
    let vsink = appsink("in_vsink", bridge.clone(), VIDEO, last.clone());
    let asink = appsink("in_asink", bridge.clone(), AUDIO, last.clone());
    p.add_many([&src, &demux, vsink.upcast_ref(), asink.upcast_ref()])?;
    src.link(&demux)?;
    let vin = if in_parse {
        p.add_many([&vparse, &vcaps])?;
        gst::Element::link_many([&vparse, &vcaps, vsink.upcast_ref()])?;
        vparse.clone()
    } else {
        vsink.clone().upcast::<gst::Element>()
    };

    let want_video = if codec == Codec::H264 { "video/x-h264" } else { "video/x-h265" };
    let pw = p.downgrade();
    let (vp, asink_e) = (vin, asink.clone().upcast::<gst::Element>());
    demux.connect_pad_added(move |_, pad| {
        let caps = pad.current_caps().unwrap_or_else(|| pad.query_caps(None));
        let name = caps.structure(0).map(|s| s.name().to_string()).unwrap_or_default();
        info!(pad = %pad.name(), %caps, "demux pad");
        let target = if name == want_video {
            vp.static_pad("sink").filter(|s| !s.is_linked())
        } else if name.starts_with("audio/") && asink_e.static_pad("sink").is_some_and(|s| !s.is_linked()) {
            // A parser (no decoding) so mpegtsmux gets framed caps; the
            // elementary stream bytes are unchanged.
            audio_parser(&pw, &caps, &asink_e)
        } else {
            None
        };
        let linked = target.as_ref().is_some_and(|t| pad.link(t).is_ok());
        if linked && name == want_video {
            let st = stamps.clone();
            pad.add_probe(gst::PadProbeType::BUFFER, move |_, info| {
                if let Some(pts) = info.buffer().and_then(|b| b.pts()) {
                    st.on_entry(pts.nseconds() as i64);
                }
                gst::PadProbeReturn::Ok
            });
        }
        if !linked {
            // Other streams (second audio, data, wrong codec) go to a fakesink so
            // tsdemux never sees not-linked.
            warn!(%name, "stream not forwarded");
            if let Some(p) = pw.upgrade()
                && let Ok(fs) = gst::ElementFactory::make("fakesink").property("sync", false).build()
                && p.add(&fs).is_ok()
            {
                let _ = fs.sync_state_with_parent();
                if let Some(sp) = fs.static_pad("sink") {
                    let _ = pad.link(&sp);
                }
            }
        }
    });
    Ok(Input { pipeline: p, last_data: last })
}

fn audio_parser(pw: &gst::glib::WeakRef<gst::Pipeline>, caps: &gst::Caps, sink: &gst::Element) -> Option<gst::Pad> {
    let s = caps.structure(0)?;
    let factory = match (s.name().as_str(), s.get::<i32>("mpegversion").ok()) {
        ("audio/mpeg", Some(2 | 4)) => "aacparse",
        ("audio/mpeg", _) => "mpegaudioparse",
        ("audio/x-ac3" | "audio/x-eac3", _) => "ac3parse",
        _ => return sink.static_pad("sink"),
    };
    let p = pw.upgrade()?;
    let e = gst::ElementFactory::make(factory).build().ok()?;
    p.add(&e).ok()?;
    e.link(sink).ok()?;
    e.sync_state_with_parent().ok()?;
    e.static_pad("sink")
}
