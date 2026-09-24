//! One thread per output. The input thread never blocks on an output: it
//! `try_send`s into a bounded queue and, if the queue is full, marks the
//! output as having lost packets so it resyncs on the next keyframe.

use std::ffi::{CString, c_int, c_void};
use std::fs::File;
use std::io::{BufWriter, Write};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::time::{Duration, Instant};

use ffmpeg_next as ff;
use ff::{Dictionary, Packet, Rational, format, media};
use tracing::{info, warn};

use crate::{SHUTDOWN, now_ns};

/// Stream layout of one input session. Outputs keep their muxer (and so a
/// continuous TS: same PIDs, continuity counters, PCR) across sessions whose
/// `sig` matches; a different `sig` (e.g. H.264 -> HEVC) reopens the output.
pub struct Layout {
    pub id: u64,
    pub sig: Vec<(media::Type, ff::codec::Id)>,
    /// Owned copies of the codec parameters, one per output stream.
    pub params: Vec<ff::codec::Parameters>,
    pub time_bases: Vec<Rational>,
}

// SAFETY: the parameters are owned copies (no owner back-reference), never
// mutated after construction, and only read (avcodec_parameters_copy source)
// from other threads.
unsafe impl Sync for Layout {}

#[derive(Clone, Copy)]
pub struct Meta {
    pub session: u32,
    pub frame: Option<u64>,
    pub in_pts: i64,
    pub read_ns: u64,
}

pub struct PktMsg {
    pub layout: Arc<Layout>,
    pub pkt: Packet,
    pub video: bool,
    pub meta: Meta,
}

/// Shared between the input thread and one output thread.
pub struct OutputShared {
    pub url: String,
    pub lost: AtomicBool,
    pub dropped_full: AtomicU64,
    pub written: AtomicU64,
    pub reconnects: AtomicU64,
}

struct Mux {
    ctx: format::context::Output,
    layout_id: u64,
    sig: Vec<(media::Type, ff::codec::Id)>,
    /// Header written (streams fixed).
    header: bool,
    /// Writing packets; false until a fresh keyframe after connect or overflow.
    synced: bool,
    last_dts: Vec<Option<i64>>,
}

pub fn run(shared: Arc<OutputShared>, rx: Receiver<PktMsg>, opts: Vec<(String, String)>, csv: Option<File>) {
    let mut csv = csv.map(BufWriter::new);
    if let Some(w) = csv.as_mut()
        && let Err(e) = writeln!(w, "session,frame,in_pts,out_pts,key,bytes,read_ns,write_ns,delay_us")
    {
        warn!(url = %shared.url, "csv header: {e}");
    }
    let mut mux: Option<Mux> = None;
    let mut retry_at = Instant::now();
    let mut rows = 0u64;
    let stale = Duration::from_millis(300);

    loop {
        if SHUTDOWN.load(Ordering::Relaxed) {
            break;
        }
        // Connect (blocks for an SRT listener until a caller arrives).
        if mux.is_none() {
            if Instant::now() < retry_at {
                // Drain while backing off so the queue does not hold stale video.
                match rx.recv_timeout(Duration::from_millis(50)) {
                    Ok(_) => continue,
                    Err(RecvTimeoutError::Timeout) => continue,
                    Err(RecvTimeoutError::Disconnected) => break,
                }
            }
            match open_output(&shared.url, &opts) {
                Ok(ctx) => {
                    info!(url = %shared.url, "output connected");
                    shared.reconnects.fetch_add(1, Ordering::Relaxed);
                    mux = Some(Mux { ctx, layout_id: 0, sig: Vec::new(), header: false, synced: false, last_dts: Vec::new() });
                }
                Err(e) => {
                    warn!(url = %shared.url, "output open failed: {e}; retrying in 1 s");
                    retry_at = Instant::now() + Duration::from_secs(1);
                    continue;
                }
            }
        }

        let msg = match rx.recv_timeout(Duration::from_millis(500)) {
            Ok(m) => m,
            Err(RecvTimeoutError::Timeout) => continue,
            Err(RecvTimeoutError::Disconnected) => break,
        };
        let Some(m) = mux.as_mut() else { continue };

        // Producer dropped packets for us: resync on the next video keyframe.
        if shared.lost.swap(false, Ordering::Relaxed) && m.synced {
            warn!(url = %shared.url, "queue overflowed; waiting for keyframe");
            m.synced = false;
        }

        // Layout change with different codecs: reopen with the new streams.
        if m.header && m.layout_id != msg.layout.id {
            if m.sig == msg.layout.sig {
                m.layout_id = msg.layout.id;
            } else {
                info!(url = %shared.url, "stream layout changed; reopening output");
                close(&mut mux, &shared.url);
                continue;
            }
        }

        if !m.synced {
            let fresh = now_ns().saturating_sub(msg.meta.read_ns) < stale.as_nanos() as u64;
            if !(msg.video && msg.pkt.is_key() && fresh) {
                continue;
            }
            if !m.header
                && let Err(e) = start(m, &msg.layout)
            {
                warn!(url = %shared.url, "write_header failed: {e}");
                close(&mut mux, &shared.url);
                retry_at = Instant::now() + Duration::from_secs(1);
                continue;
            }
            m.synced = true;
            info!(url = %shared.url, layout = msg.layout.id, "output synced at keyframe");
        }

        match write(m, msg, &mut csv, &shared) {
            Ok(()) => {
                rows += 1;
                if rows.is_multiple_of(30)
                    && let Some(w) = csv.as_mut()
                {
                    let _ = w.flush();
                }
            }
            Err(e) => {
                warn!(url = %shared.url, "write failed: {e}; reconnecting");
                close(&mut mux, &shared.url);
                retry_at = Instant::now() + Duration::from_millis(500);
            }
        }
    }
    close(&mut mux, &shared.url);
    if let Some(w) = csv.as_mut() {
        let _ = w.flush();
    }
}

unsafe extern "C" fn interrupted(_: *mut c_void) -> c_int {
    c_int::from(SHUTDOWN.load(Ordering::Relaxed))
}

/// Like `format::output_as_with(url, "mpegts", opts)` but with an interrupt
/// callback on both the open and the context, so a blocked SRT listen/write
/// returns at shutdown. ffmpeg-next's helper passes no callback to avio_open2.
fn open_output(url: &str, opts: &[(String, String)]) -> Result<format::context::Output, ff::Error> {
    let c_url = CString::new(url).map_err(|_| ff::Error::InvalidData)?;
    // SAFETY: plain FFmpeg calls; the context is handed to `Output::wrap`
    // right after allocation, which owns it (and pb) from then on.
    unsafe {
        let mut ps = std::ptr::null_mut();
        let r = ff::ffi::avformat_alloc_output_context2(&mut ps, std::ptr::null(), c"mpegts".as_ptr(), c_url.as_ptr());
        if r < 0 || ps.is_null() {
            return Err(ff::Error::from(r.min(-1)));
        }
        let cb = ff::ffi::AVIOInterruptCB { callback: Some(interrupted), opaque: std::ptr::null_mut() };
        (*ps).interrupt_callback = cb;
        let out = format::context::Output::wrap(ps);
        let mut d = dict(opts).disown();
        let r = ff::ffi::avio_open2(&mut (*ps).pb, c_url.as_ptr(), ff::ffi::AVIO_FLAG_WRITE as c_int, &cb, &mut d);
        let left = Dictionary::own(d);
        if left.iter().count() > 0 {
            warn!(url, unused = ?left.iter().map(|(k, _)| k.to_string()).collect::<Vec<_>>(), "unused output options");
        }
        if r < 0 {
            return Err(ff::Error::from(r));
        }
        Ok(out)
    }
}

fn dict(opts: &[(String, String)]) -> Dictionary<'static> {
    let mut d = Dictionary::new();
    for (k, v) in opts {
        d.set(k, v);
    }
    d
}

fn start(m: &mut Mux, layout: &Layout) -> Result<(), ff::Error> {
    for (params, tb) in layout.params.iter().zip(&layout.time_bases) {
        let mut st = m.ctx.add_stream(ff::codec::Id::None)?;
        st.set_parameters(params.clone());
        st.set_time_base(*tb);
        // SAFETY: codecpar belongs to the stream we just created; clearing the
        // tag lets the muxer choose its own (required when remuxing).
        unsafe {
            (*(*st.as_mut_ptr()).codecpar).codec_tag = 0;
        }
    }
    let mut hopts = Dictionary::new();
    hopts.set("flush_packets", "1"); // push every packet to the socket at once
    hopts.set("mpegts_flags", "+resend_headers");
    hopts.set("pes_payload_size", "0"); // don't hold audio to fill PES packets
    m.ctx.write_header_with(hopts)?;
    m.layout_id = layout.id;
    m.sig = layout.sig.clone();
    m.last_dts = vec![None; layout.params.len()];
    m.header = true;
    Ok(())
}

fn write(m: &mut Mux, msg: PktMsg, csv: &mut Option<BufWriter<File>>, shared: &OutputShared) -> Result<(), ff::Error> {
    let PktMsg { layout, mut pkt, video, meta } = msg;
    let idx = pkt.stream();
    let (Some(src_tb), Some(dst_tb)) = (layout.time_bases.get(idx), m.ctx.stream(idx).map(|s| s.time_base())) else {
        return Ok(()); // unknown stream: drop silently (cannot happen with a matching layout)
    };
    pkt.rescale_ts(*src_tb, dst_tb);
    let dts = pkt.dts().or(pkt.pts());
    if let (Some(d), Some(Some(last))) = (dts, m.last_dts.get(idx))
        && d <= *last
    {
        warn!(url = %shared.url, stream = idx, dts = d, last, "non-monotonic dts; dropping packet");
        return Ok(());
    }
    let bytes = pkt.size();
    let out_pts = pkt.pts().unwrap_or(-1);
    let key = pkt.is_key();
    pkt.write(&mut m.ctx)?;
    if let Some(slot) = m.last_dts.get_mut(idx) {
        *slot = dts;
    }
    shared.written.fetch_add(1, Ordering::Relaxed);
    if video && let Some(w) = csv.as_mut() {
        let write_ns = now_ns();
        let delay_us = write_ns.saturating_sub(meta.read_ns) / 1000;
        let frame = meta.frame.map_or(String::new(), |f| f.to_string());
        let _ = writeln!(
            w,
            "{},{},{},{},{},{},{},{},{}",
            meta.session, frame, meta.in_pts, out_pts, u8::from(key), bytes, meta.read_ns, write_ns, delay_us
        );
    }
    Ok(())
}

fn close(mux: &mut Option<Mux>, url: &str) {
    if let Some(mut m) = mux.take()
        && m.header
        && let Err(e) = m.ctx.write_trailer()
    {
        warn!(url, "write_trailer: {e}");
    }
}
