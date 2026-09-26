//! The restartable input pipeline: `srtsrc | udpsrc [! rtpmp2tdepay] !
//! tsdemux`, video (H.264/HEVC, never decoded) and the first audio track to
//! the bridge, and the audio tap for ASR:
//! `aacparse ! tee ! leaky queue ! avdec_aac ! audioconvert ! audioresample !
//! appsink` (16 kHz S16LE). The tap never blocks the demuxer: its queue leaks
//! and the appsink drops old buffers.

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use anyhow::{Context, Result};
use gst::prelude::*;
use tracing::{error, info, warn};

use crate::bridge::{AUDIO, VIDEO};
use crate::output::Codec;
use crate::stats::inc;
use crate::url::{InputKind, input_kind, redact, srt_uri, udp_uri};
use crate::{AudioChunk, Core, wall_ns};

pub(crate) struct Input {
    pub pipeline: gst::Pipeline,
}

fn media_sink(name: &str, core: Arc<Core>, stream: usize) -> gst_app::AppSink {
    // async=false: udpsrc is not live, so two prerolling sinks fed by one
    // tsdemux thread would deadlock (S2 gotcha 1).
    let s = gst_app::AppSink::builder()
        .name(name)
        .sync(false)
        .async_(false)
        .build();
    s.set_callbacks(
        gst_app::AppSinkCallbacks::builder()
            .new_sample(move |s| {
                let Ok(sample) = s.pull_sample() else {
                    return Err(gst::FlowError::Eos);
                };
                core.last_data.store(wall_ns(), Ordering::Relaxed);
                if stream == VIDEO {
                    inc(&core.counters.frames_in);
                }
                core.bridge.push(stream, &sample);
                Ok(gst::FlowSuccess::Ok)
            })
            .build(),
    );
    s
}

/// Converts one tap buffer (S16LE, 16 kHz, any channel count) to mono.
fn to_mono(bytes: &[u8], channels: usize, pick: Option<u32>) -> Vec<i16> {
    let channels = channels.max(1);
    let samples: Vec<i16> = bytes
        .as_chunks::<2>()
        .0
        .iter()
        .map(|b| i16::from_le_bytes(*b))
        .collect();
    if channels == 1 {
        return samples;
    }
    samples
        .chunks_exact(channels)
        .map(|frame| match pick {
            Some(c) => frame.get(c as usize).copied().unwrap_or(0),
            None => {
                let sum: i32 = frame.iter().map(|&s| i32::from(s)).sum();
                (sum / channels as i32) as i16
            }
        })
        .collect()
}

fn tap_sink(core: Arc<Core>) -> gst_app::AppSink {
    let caps = gst::Caps::builder("audio/x-raw")
        .field("format", "S16LE")
        .field("rate", 16_000i32)
        .field("layout", "interleaved")
        .build();
    let s = gst_app::AppSink::builder()
        .name("asr_tap")
        .caps(&caps)
        .sync(false)
        .async_(false)
        .max_buffers(50)
        .drop(true)
        .build();
    let mut next_ms: Option<u64> = None;
    let pick = core.cfg.audio_channel;
    s.set_callbacks(
        gst_app::AppSinkCallbacks::builder()
            .new_sample(move |s| {
                let Ok(sample) = s.pull_sample() else {
                    return Err(gst::FlowError::Eos);
                };
                let channels = sample
                    .caps()
                    .and_then(|c| c.structure(0).and_then(|s| s.get::<i32>("channels").ok()))
                    .unwrap_or(1);
                let Some(buf) = sample.buffer() else {
                    return Ok(gst::FlowSuccess::Ok);
                };
                let Ok(map) = buf.map_readable() else {
                    return Ok(gst::FlowSuccess::Ok);
                };
                let samples = to_mono(&map, usize::try_from(channels).unwrap_or(1), pick);
                if samples.is_empty() {
                    return Ok(gst::FlowSuccess::Ok);
                }
                let start_ms = buf.pts().map(|p| p.mseconds()).or(next_ms).unwrap_or(0);
                next_ms = Some(start_ms + samples.len() as u64 / 16);
                inc(&core.counters.audio_chunks);
                (core.audio)(AudioChunk { start_ms, samples });
                Ok(gst::FlowSuccess::Ok)
            })
            .build(),
    );
    s
}

fn make(f: &str) -> Result<gst::Element> {
    gst::ElementFactory::make(f)
        .build()
        .with_context(|| format!("missing GStreamer element {f}"))
}

fn source(url: &str, srt_latency_ms: u32) -> Result<Vec<gst::Element>> {
    let kind = input_kind(url)?;
    let uri = match kind {
        InputKind::Srt => srt_uri(url, srt_latency_ms),
        InputKind::Udp | InputKind::Rtp => udp_uri(url),
    };
    let src = gst::Element::make_from_uri(gst::URIType::Src, &uri, Some("insrc"))
        .with_context(|| format!("cannot open input {}", redact(url)))?;
    if kind != InputKind::Srt {
        // Default SO_RCVBUF drops datagrams at 3 Mbit/s bursts (I-frames).
        src.set_property("buffer-size", 8 << 20_i32);
    }
    Ok(match kind {
        InputKind::Srt => vec![src],
        InputKind::Udp => {
            src.set_property(
                "caps",
                gst::Caps::builder("video/mpegts")
                    .field("systemstream", true)
                    .build(),
            );
            vec![src]
        }
        InputKind::Rtp => {
            src.set_property(
                "caps",
                gst::Caps::builder("application/x-rtp")
                    .field("media", "video")
                    .field("clock-rate", 90_000i32)
                    .field("encoding-name", "MP2T")
                    .build(),
            );
            vec![src, make("rtpmp2tdepay")?]
        }
    })
}

pub(crate) fn build(core: &Arc<Core>) -> Result<Input> {
    let p = gst::Pipeline::with_name("input");
    let mut chain = source(&core.cfg.input_url, core.cfg.srt_latency_ms)?;
    let demux = gst::ElementFactory::make("tsdemux").name("demux").build()?;
    // Buffer PTS are the PES PTS exactly; the bridge does the clock locking.
    demux.set_property("ignore-pcr", true);
    chain.push(demux.clone());
    p.add_many(chain.iter())?;
    gst::Element::link_many(chain.iter())?;
    core.last_data.store(0, Ordering::Relaxed);

    let vsink = media_sink("in_vsink", core.clone(), VIDEO);
    let asink = media_sink("in_asink", core.clone(), AUDIO);
    p.add_many([vsink.upcast_ref::<gst::Element>(), asink.upcast_ref()])?;

    let pw = p.downgrade();
    let core2 = core.clone();
    let audio_seen = AtomicU32::new(0);
    let (vs, as_) = (
        vsink.upcast::<gst::Element>(),
        asink.upcast::<gst::Element>(),
    );
    demux.connect_pad_added(move |_, pad| {
        let caps = pad.current_caps().unwrap_or_else(|| pad.query_caps(None));
        let name = caps
            .structure(0)
            .map(|s| s.name().to_string())
            .unwrap_or_default();
        info!(pad = %pad.name(), %caps, "input stream");
        let codec = match name.as_str() {
            "video/x-h264" => Some(Codec::H264),
            "video/x-h265" => Some(Codec::Hevc),
            _ => None,
        };
        let target = if let Some(codec) = codec {
            match core2.output.ensure(codec) {
                Ok(()) => vs.static_pad("sink").filter(|s| !s.is_linked()),
                Err(e) => {
                    error!(err = %format!("{e:#}"), "cannot build the output pipeline");
                    None
                }
            }
        } else if name.starts_with("audio/") {
            let k = audio_seen.fetch_add(1, Ordering::Relaxed);
            audio_branch(
                &pw,
                &caps,
                &core2,
                (k == 0).then_some(&as_),
                k == core2.cfg.audio_track,
            )
        } else {
            None
        };
        let linked = target.as_ref().is_some_and(|t| pad.link(t).is_ok());
        if !linked {
            // Other streams (more audio, data) go to a fakesink so tsdemux
            // never sees not-linked.
            info!(%name, "input stream not used");
            if let Some(p) = pw.upgrade()
                && let Ok(fs) = gst::ElementFactory::make("fakesink")
                    .property("sync", false)
                    .property("async", false)
                    .build()
                && p.add(&fs).is_ok()
            {
                let _ = fs.sync_state_with_parent();
                if let Some(sp) = fs.static_pad("sink") {
                    let _ = pad.link(&sp);
                }
            }
        }
    });
    Ok(Input { pipeline: p })
}

/// Parser (no decoding) to the output, and/or the ASR tap. Returns the pad
/// the demuxer pad links to.
fn audio_branch(
    pw: &gst::glib::WeakRef<gst::Pipeline>,
    caps: &gst::Caps,
    core: &Arc<Core>,
    to_output: Option<&gst::Element>,
    to_tap: bool,
) -> Option<gst::Pad> {
    if to_output.is_none() && !to_tap {
        return None;
    }
    let s = caps.structure(0)?;
    let aac = s.name() == "audio/mpeg" && matches!(s.get::<i32>("mpegversion").ok(), Some(2 | 4));
    let parse_f = match (s.name().as_str(), aac) {
        (_, true) => "aacparse",
        ("audio/mpeg", false) => "mpegaudioparse",
        ("audio/x-ac3" | "audio/x-eac3", _) => "ac3parse",
        _ => {
            warn!(caps = %caps, "unknown audio format; passed through without a parser");
            return to_output.and_then(|e| e.static_pad("sink"));
        }
    };
    let to_tap = to_tap && {
        if !aac {
            warn!(caps = %caps, "audio tap needs AAC; no captions from this input");
        }
        aac
    };
    let p = pw.upgrade()?;
    let parse = gst::ElementFactory::make(parse_f).build().ok()?;
    p.add(&parse).ok()?;
    let mut tail = parse.clone();
    let mut added = vec![parse.clone()];
    if to_output.is_some() && to_tap {
        let tee = gst::ElementFactory::make("tee").build().ok()?;
        p.add(&tee).ok()?;
        parse.link(&tee).ok()?;
        tail = tee.clone();
        added.push(tee);
    }
    if let Some(out) = to_output {
        tail.link(out).ok()?;
    }
    if to_tap {
        let tap = (|| -> Result<Vec<gst::Element>> {
            let q = gst::ElementFactory::make("queue")
                .property("max-size-time", 2_000_000_000u64)
                .property("max-size-buffers", 0u32)
                .property("max-size-bytes", 0u32)
                .build()?;
            q.set_property_from_str("leaky", "downstream");
            let c = core.clone();
            q.connect("overrun", false, move |_| {
                inc(&c.counters.audio_drops);
                None
            });
            // avdec_aac (LGPL) only: never fdkaacdec or faad (licences).
            let v = vec![
                q,
                make("avdec_aac")?,
                make("audioconvert")?,
                make("audioresample")?,
                tap_sink(core.clone()).upcast(),
            ];
            p.add_many(v.iter())?;
            gst::Element::link_many(v.iter())?;
            tail.link(&v[0])?;
            Ok(v)
        })();
        match tap {
            Ok(v) => {
                for e in v.iter().rev() {
                    let _ = e.sync_state_with_parent();
                }
                info!("audio tap attached (avdec_aac -> 16 kHz)");
            }
            Err(e) => {
                error!(err = %format!("{e:#}"), "audio tap failed; no captions from this input")
            }
        }
    }
    for e in added.iter().rev() {
        e.sync_state_with_parent().ok()?;
    }
    parse.static_pad("sink")
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;

    fn bytes(s: &[i16]) -> Vec<u8> {
        s.iter().flat_map(|x| x.to_le_bytes()).collect()
    }

    #[test]
    fn mono_passthrough_downmix_and_pick() {
        assert_eq!(to_mono(&bytes(&[1, -2, 3]), 1, None), vec![1, -2, 3]);
        let st = bytes(&[100, 300, -50, 50]);
        assert_eq!(to_mono(&st, 2, None), vec![200, 0]);
        assert_eq!(to_mono(&st, 2, Some(1)), vec![300, 50]);
        assert_eq!(to_mono(&st, 2, Some(7)), vec![0, 0]);
    }
}
