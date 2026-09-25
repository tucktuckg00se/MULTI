//! The restartable input: S2's `srtsrc|udpsrc -> tsdemux -> appsink` (video
//! and audio to the bridge, video never decoded) plus an audio tap for ASR:
//! `parser -> tee -> leaky queue -> decoder -> 16 kHz mono F32 -> appsink`.
//! The tap never blocks the demuxer: its queue leaks and its appsink sends with
//! `try_send`, counting what it drops.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{SyncSender, TrySendError};

use anyhow::{Context, Result};
use gst::prelude::*;
use s2_gst_pipe::Codec;
use s2_gst_pipe::bridge::{AUDIO, Bridge, VIDEO};
use s2_gst_pipe::stamps::{Stamps, wall_ns};
use tracing::{info, warn};

/// 16 kHz mono PCM for the ASR thread, with the input PTS of its first sample.
pub struct AudioChunk {
    pub pts_ns: Option<i64>,
    pub samples: Vec<f32>,
}

pub struct Tap {
    pub tx: SyncSender<AudioChunk>,
    pub drops: Arc<AtomicU64>,
}

pub struct Input {
    pub pipeline: gst::Pipeline,
    pub last_data: Arc<AtomicU64>,
}

fn appsink(name: &str, bridge: Arc<Bridge>, stream: usize, last: Arc<AtomicU64>) -> gst_app::AppSink {
    // async=false: see S2 gotcha 1 (preroll deadlock behind tsdemux).
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

fn tap_sink(tap: &Tap) -> gst_app::AppSink {
    let caps = gst::Caps::builder("audio/x-raw")
        .field("format", "F32LE")
        .field("rate", 16_000i32)
        .field("channels", 1i32)
        .field("layout", "interleaved")
        .build();
    let s = gst_app::AppSink::builder()
        .name("asr_sink")
        .caps(&caps)
        .sync(false)
        .async_(false)
        .max_buffers(50)
        .drop(true)
        .build();
    let (tx, drops) = (tap.tx.clone(), tap.drops.clone());
    s.set_callbacks(
        gst_app::AppSinkCallbacks::builder()
            .new_sample(move |s| {
                let Ok(sample) = s.pull_sample() else { return Err(gst::FlowError::Eos) };
                let Some(buf) = sample.buffer() else { return Ok(gst::FlowSuccess::Ok) };
                let Ok(map) = buf.map_readable() else { return Ok(gst::FlowSuccess::Ok) };
                let samples: Vec<f32> =
                    map.as_chunks::<4>().0.iter().map(|b| f32::from_le_bytes(*b)).collect();
                let chunk = AudioChunk { pts_ns: buf.pts().map(|p| p.nseconds() as i64), samples };
                match tx.try_send(chunk) {
                    Ok(()) => {}
                    Err(TrySendError::Full(_)) => {
                        drops.fetch_add(1, Ordering::Relaxed);
                    }
                    // ASR thread gone: keep the video running.
                    Err(TrySendError::Disconnected(_)) => {}
                }
                Ok(gst::FlowSuccess::Ok)
            })
            .build(),
    );
    s
}

pub fn build(uri: &str, codec: Codec, bridge: Arc<Bridge>, stamps: Arc<Stamps>, tap: &Tap) -> Result<Input> {
    let p = gst::Pipeline::with_name("input");
    let src = gst::Element::make_from_uri(gst::URIType::Src, uri, Some("insrc"))
        .with_context(|| format!("no source for {uri}"))?;
    if uri.starts_with("udp") {
        src.set_property("caps", gst::Caps::builder("video/mpegts").field("systemstream", true).build());
        src.set_property("buffer-size", 8 << 20_i32);
    }
    let demux = gst::ElementFactory::make("tsdemux").name("demux").build()?;
    demux.set_property("ignore-pcr", true);
    let last = Arc::new(AtomicU64::new(0));
    let vsink = appsink("in_vsink", bridge.clone(), VIDEO, last.clone());
    let asink = appsink("in_asink", bridge, AUDIO, last.clone());
    let tsink = tap_sink(tap);
    p.add_many([&src, &demux, vsink.upcast_ref(), asink.upcast_ref()])?;
    src.link(&demux)?;

    let want_video = if codec == Codec::H264 { "video/x-h264" } else { "video/x-h265" };
    let pw = p.downgrade();
    let (vs, as_) = (vsink.upcast::<gst::Element>(), asink.upcast::<gst::Element>());
    demux.connect_pad_added(move |_, pad| {
        let caps = pad.current_caps().unwrap_or_else(|| pad.query_caps(None));
        let name = caps.structure(0).map(|s| s.name().to_string()).unwrap_or_default();
        info!(pad = %pad.name(), %caps, "demux pad");
        let target = if name == want_video {
            vs.static_pad("sink").filter(|s| !s.is_linked())
        } else if name.starts_with("audio/") && as_.static_pad("sink").is_some_and(|s| !s.is_linked()) {
            audio_branch(&pw, &caps, &as_, &tsink)
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

/// parser -> tee -> bridge appsink (bytes unchanged), and tee -> leaky queue ->
/// decoder -> audioconvert -> audioresample -> ASR appsink. If the decoder
/// is missing, audio still passes through without captions.
fn audio_branch(
    pw: &gst::glib::WeakRef<gst::Pipeline>,
    caps: &gst::Caps,
    sink: &gst::Element,
    tsink: &gst_app::AppSink,
) -> Option<gst::Pad> {
    let s = caps.structure(0)?;
    // Decoders in order of preference. avdec_* (gst-libav, FFmpeg's LGPL
    // decoders) is what should ship; fdkaacdec (FDK licence) and faad (GPL)
    // are dev-machine fallbacks only.
    let (parse_f, decs): (&str, &[&str]) = match (s.name().as_str(), s.get::<i32>("mpegversion").ok()) {
        ("audio/mpeg", Some(2 | 4)) => ("aacparse", &["avdec_aac", "fdkaacdec", "faad"]),
        ("audio/mpeg", _) => ("mpegaudioparse", &["avdec_mp3", "mpg123audiodec"]),
        ("audio/x-ac3" | "audio/x-eac3", _) => ("ac3parse", &["avdec_ac3", "a52dec"]),
        _ => return sink.static_pad("sink"),
    };
    let p = pw.upgrade()?;
    let parse = gst::ElementFactory::make(parse_f).build().ok()?;
    let tee = gst::ElementFactory::make("tee").build().ok()?;
    p.add_many([&parse, &tee]).ok()?;
    gst::Element::link_many([&parse, &tee, sink]).ok()?;
    let tap = (|| -> Result<Vec<gst::Element>> {
        let q = gst::ElementFactory::make("queue")
            .property("max-size-time", 2_000_000_000u64)
            .property("max-size-buffers", 0u32)
            .property("max-size-bytes", 0u32)
            .build()?;
        q.set_property_from_str("leaky", "downstream");
        let dec = decs
            .iter()
            .find_map(|f| gst::ElementFactory::make(f).build().ok())
            .with_context(|| format!("no decoder among {decs:?}"))?;
        info!(decoder = %dec.factory().map(|f| f.name().to_string()).unwrap_or_default(), "audio tap decoder");
        let conv = gst::ElementFactory::make("audioconvert").build()?;
        let res = gst::ElementFactory::make("audioresample").build()?;
        let v = vec![q, dec, conv, res, tsink.clone().upcast()];
        p.add_many(v.iter())?;
        gst::Element::link_many(v.iter())?;
        tee.link(&v[0])?;
        Ok(v)
    })();
    match tap {
        Ok(v) => {
            for e in v.iter().rev() {
                let _ = e.sync_state_with_parent();
            }
        }
        Err(e) => warn!(err = %e, "audio tap failed; no captions from this input"),
    }
    for e in [&tee, &parse] {
        e.sync_state_with_parent().ok()?;
    }
    parse.static_pad("sink")
}
