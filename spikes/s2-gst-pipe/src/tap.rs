//! Black-box tap: receives MPEG-TS (UDP or SRT), logs the arrival wallclock of
//! every video PES keyed by a hash of its first VCL NAL unit, and optionally
//! forwards the bytes unchanged. The slice data is not touched by the
//! pipeline, so the same hash in the source-side and output-side logs is the
//! same picture regardless of re-stamped PTS. Also counts TS continuity errors
//! and pictures that carry an A/53 (GA94) caption SEI.

use std::collections::HashMap;
use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use clap::Parser;
use gst::prelude::*;
use tracing::{info, warn};

use crate::stamps::wall_ns;

#[derive(Parser)]
pub struct TapArgs {
    #[arg(long)]
    input: String,
    /// Forward the received bytes to this URI (udp:// or srt://).
    #[arg(long)]
    forward: Option<String>,
    #[arg(long)]
    csv: PathBuf,
    #[arg(long, default_value_t = 0)]
    duration_s: u64,
    #[arg(long)]
    hevc: bool,
}

#[derive(Default)]
struct Pes {
    data: Vec<u8>,
    wall: u64,
    video: bool,
}

struct Parser_ {
    hevc: bool,
    pes: HashMap<u16, Pes>,
    cc: HashMap<u16, u8>,
    cc_errors: u64,
    pictures: u64,
    with_captions: u64,
    last_wall: u64,
    gaps: Vec<(u64, u64)>,
    out: BufWriter<File>,
}

fn fnv(b: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for &x in b {
        h ^= u64::from(x);
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

/// Start offsets of NAL units (just past each 00 00 01).
fn nal_starts(b: &[u8]) -> Vec<usize> {
    let mut v = Vec::new();
    let mut i = 0;
    while i + 3 <= b.len() {
        if b[i] == 0 && b[i + 1] == 0 && b[i + 2] == 1 {
            v.push(i + 3);
            i += 3;
        } else {
            i += 1;
        }
    }
    v
}

impl Parser_ {
    fn finish(&mut self, pes: Pes) {
        let d = &pes.data;
        if d.len() < 14 || !pes.video {
            return;
        }
        let flags = d[7];
        let hdl = usize::from(d[8]);
        let pts = if flags & 0x80 != 0 {
            (u64::from(d[9] & 0x0E) << 29)
                | (u64::from(d[10]) << 22)
                | (u64::from(d[11] & 0xFE) << 14)
                | (u64::from(d[12]) << 7)
                | (u64::from(d[13]) >> 1)
        } else {
            0
        };
        let Some(es) = d.get(9 + hdl..) else { return };
        let starts = nal_starts(es);
        let mut hash = 0u64;
        let mut cc = false;
        for (k, &s) in starts.iter().enumerate() {
            let end = starts.get(k + 1).map(|&n| n.saturating_sub(3)).unwrap_or(es.len());
            let Some(&h) = es.get(s) else { continue };
            let (vcl, sei) = if self.hevc {
                let t = (h >> 1) & 0x3F;
                (t < 32, t == 39)
            } else {
                let t = h & 0x1F;
                ((1..=5).contains(&t), t == 6)
            };
            if sei && es.get(s..end).is_some_and(|n| n.windows(4).any(|w| w == b"GA94")) {
                cc = true;
            }
            if vcl && hash == 0 {
                let e = end.min(s + 4096);
                hash = fnv(es.get(s..e).unwrap_or(&[]));
            }
        }
        self.pictures += 1;
        if cc {
            self.with_captions += 1;
        }
        let _ = writeln!(self.out, "{},{},{},{:016x},{}", self.pictures, pts, pes.wall, hash, u8::from(cc));
    }

    fn packet(&mut self, p: &[u8], wall: u64) {
        if p.len() != 188 || p[0] != 0x47 {
            return;
        }
        let pusi = p[1] & 0x40 != 0;
        let pid = (u16::from(p[1] & 0x1F) << 8) | u16::from(p[2]);
        let afc = (p[3] >> 4) & 3;
        let cc = p[3] & 0x0F;
        if pid == 0x1FFF {
            return;
        }
        if afc & 1 != 0 {
            if let Some(&prev) = self.cc.get(&pid)
                && cc != (prev + 1) & 0x0F
                && cc != prev
            {
                self.cc_errors += 1;
            }
            self.cc.insert(pid, cc);
        }
        let mut off = 4;
        if afc & 2 != 0 {
            off += 1 + usize::from(p[4]);
        }
        if afc & 1 == 0 || off >= 188 {
            return;
        }
        let payload = &p[off..];
        if pusi {
            if let Some(prev) = self.pes.remove(&pid) {
                self.finish(prev);
            }
            let video = payload.len() > 4 && payload[..3] == [0, 0, 1] && (0xE0..=0xEF).contains(&payload[3]);
            self.pes.insert(pid, Pes { data: payload.to_vec(), wall, video });
        } else if let Some(e) = self.pes.get_mut(&pid)
            && e.video
        {
            e.data.extend_from_slice(payload);
        }
    }

    fn buffer(&mut self, b: &[u8]) {
        let wall = wall_ns();
        if self.last_wall > 0 && wall - self.last_wall > 500_000_000 {
            self.gaps.push((self.last_wall, wall));
            warn!(gap_ms = (wall - self.last_wall) / 1_000_000, "tap: input gap");
        }
        self.last_wall = wall;
        for p in b.chunks(188) {
            self.packet(p, wall);
        }
    }
}

pub fn run(a: TapArgs) -> Result<()> {
    let mut out = BufWriter::new(File::create(&a.csv)?);
    writeln!(out, "n,pts90k,wall_ns,vcl_hash,has_cc")?;
    let parser = Arc::new(Mutex::new(Parser_ {
        hevc: a.hevc,
        pes: HashMap::new(),
        cc: HashMap::new(),
        cc_errors: 0,
        pictures: 0,
        with_captions: 0,
        last_wall: 0,
        gaps: Vec::new(),
        out,
    }));
    let fwd = match &a.forward {
        Some(u) => {
            let p = gst::Pipeline::with_name("tapfwd");
            let src = gst_app::AppSrc::builder()
                .is_live(true)
                .format(gst::Format::Bytes)
                .caps(&gst::Caps::builder("video/mpegts").field("systemstream", true).build())
                .build();
            let sink = gst::Element::make_from_uri(gst::URIType::Sink, u, None).context("forward sink")?;
            sink.set_property("sync", false);
            if sink.has_property("wait-for-connection") {
                sink.set_property("wait-for-connection", false);
            }
            p.add_many([src.upcast_ref(), &sink])?;
            src.link(&sink)?;
            p.set_state(gst::State::Playing)?;
            Some((p, src))
        }
        None => None,
    };
    let p = gst::Pipeline::with_name("tap");
    let src = gst::Element::make_from_uri(gst::URIType::Src, &a.input, None).context("tap source")?;
    let sink = gst_app::AppSink::builder().sync(false).async_(false).build();
    p.add_many([&src, sink.upcast_ref()])?;
    src.link(&sink)?;
    let pr = parser.clone();
    let fsrc = fwd.as_ref().map(|f| f.1.clone());
    sink.set_callbacks(
        gst_app::AppSinkCallbacks::builder()
            .new_sample(move |s| {
                let Ok(sample) = s.pull_sample() else { return Err(gst::FlowError::Eos) };
                if let Some(buf) = sample.buffer_owned() {
                    if let Some(f) = &fsrc {
                        let _ = f.push_buffer(buf.clone());
                    }
                    if let (Ok(map), Ok(mut prs)) = (buf.map_readable(), pr.lock()) {
                        prs.buffer(map.as_slice());
                    }
                }
                Ok(gst::FlowSuccess::Ok)
            })
            .build(),
    );
    p.set_state(gst::State::Playing)?;
    let bus = p.bus().context("bus")?;
    let start = Instant::now();
    loop {
        if let Some(msg) = bus.timed_pop(gst::ClockTime::from_mseconds(200)) {
            match msg.view() {
                gst::MessageView::Error(e) => {
                    warn!(err = %e.error(), "tap input error; restarting");
                    let _ = p.set_state(gst::State::Null);
                    std::thread::sleep(Duration::from_millis(300));
                    let _ = p.set_state(gst::State::Playing);
                }
                gst::MessageView::Eos(_) => break,
                _ => {}
            }
        }
        if a.duration_s > 0 && start.elapsed() >= Duration::from_secs(a.duration_s) {
            break;
        }
    }
    let _ = p.set_state(gst::State::Null);
    if let Some((fp, _)) = fwd {
        let _ = fp.set_state(gst::State::Null);
    }
    if let Ok(mut prs) = parser.lock() {
        let _ = prs.out.flush();
        let gaps: Vec<String> = prs.gaps.iter().map(|(a, b)| format!("{}ms", (b - a) / 1_000_000)).collect();
        info!(pictures = prs.pictures, with_captions = prs.with_captions, cc_errors = prs.cc_errors, gaps = ?gaps, "tap done");
        println!(
            "tap: pictures={} with_captions={} cc_errors={} gaps={:?}",
            prs.pictures, prs.with_captions, prs.cc_errors, gaps
        );
    }
    Ok(())
}
