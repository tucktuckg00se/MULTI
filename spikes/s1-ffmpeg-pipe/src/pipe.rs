//! Input side: open/reconnect, demux, rebase timestamps onto one continuous
//! output timeline, splice caption SEI into video access units, fan out.

use std::cmp::Reverse;
use std::collections::{BinaryHeap, VecDeque};
use std::fs::File;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{SyncSender, TrySendError, sync_channel};
use std::thread;
use std::time::{Duration, Instant};

use ffmpeg_next as ff;
use ff::{Dictionary, Packet, Rational, format, media};
use tracing::{info, warn};

use crate::nal::{self, VideoCodec};
use crate::output::{self, Layout, Meta, OutputShared, PktMsg};
use crate::{RunArgs, SHUTDOWN, now_ns, rss_kib};

const TB90K: Rational = Rational(1, 90_000);

struct Out {
    shared: Arc<OutputShared>,
    tx: SyncSender<PktMsg>,
    thread: thread::JoinHandle<()>,
}

/// Maps input timestamps onto a continuous output timeline (90 kHz).
///
/// Each run of input timestamps without a jump is an *epoch*. A new epoch
/// (reconnect, or the source restarting with PTS from 0) continues the output
/// timeline after the last packet sent, advanced by the wall time the input was
/// silent, so output PTS/PCR keep pace with real time across gaps.
struct Rebase {
    /// End (dts + duration) of the latest packet sent, on the output timeline.
    out_end: Option<i64>,
    /// Added to input dts (90 kHz) in the current epoch.
    delta: Option<i64>,
    last_in: Vec<Option<i64>>,
    /// min(wall - dts), 90 kHz: where the input timeline sits in wall time,
    /// measured by the packets that arrived earliest (not stale ones the
    /// demuxer flushed late). Kept over two ~2 s windows of input time so a
    /// start-up burst does not bias it for the whole epoch.
    anchor: Option<i64>,
    anchor_prev: Option<i64>,
    window_start: i64,
    /// Largest input dts seen in the epoch.
    in_max: i64,
    epochs: u64,
}

/// Forward margin added at an epoch change so streams whose first packet
/// starts slightly earlier than the one that opened the epoch stay monotonic.
const EPOCH_MARGIN_90K: i64 = 45_000;

/// Wall clock in 90 kHz ticks.
fn wall_90k(ns: u64) -> i64 {
    (ns / 100_000 * 9) as i64
}

impl Rebase {
    fn new() -> Self {
        Self {
            out_end: None,
            delta: None,
            last_in: Vec::new(),
            anchor: None,
            anchor_prev: None,
            window_start: 0,
            in_max: 0,
            epochs: 0,
        }
    }

    fn new_session(&mut self, streams: usize) {
        self.delta = None;
        self.last_in = vec![None; streams];
    }

    /// Returns the delta to apply to this packet's input dts (90 kHz).
    fn delta_for(&mut self, stream: usize, dts: i64, wall: i64) -> i64 {
        if let (Some(Some(prev)), Some(_)) = (self.last_in.get(stream), self.delta) {
            let jump = dts - prev;
            if !(-90_000..=10 * 90_000).contains(&jump) {
                warn!(stream, prev, dts, jump, "input timestamp discontinuity; starting new epoch");
                self.delta = None;
                self.last_in.iter_mut().for_each(|x| *x = None);
            }
        }
        let delta = match self.delta {
            Some(d) => d,
            None => {
                let anchor = match (self.anchor, self.anchor_prev) {
                    (Some(a), Some(b)) => Some(a.min(b)),
                    (a, b) => a.or(b),
                };
                let d = match (self.out_end, anchor) {
                    // First ever packet: keep input timestamps unchanged.
                    (None, _) | (_, None) => 0,
                    (Some(end), Some(anchor)) => {
                        // Wall time elapsed since the old epoch's last packet was due.
                        let silent = (wall - (self.in_max + anchor)).max(0);
                        end + silent + EPOCH_MARGIN_90K - dts
                    }
                };
                self.epochs += 1;
                info!(epoch = self.epochs, delta = d, "timestamp epoch");
                self.delta = Some(d);
                self.anchor = None;
                self.anchor_prev = None;
                self.window_start = dts;
                self.in_max = dts;
                d
            }
        };
        if let Some(slot) = self.last_in.get_mut(stream) {
            *slot = Some(dts);
        }
        self.in_max = self.in_max.max(dts);
        if dts - self.window_start > 180_000 {
            self.anchor_prev = self.anchor.take();
            self.window_start = dts;
        }
        let a = wall - dts;
        self.anchor = Some(self.anchor.map_or(a, |x| x.min(a)));
        delta
    }

    fn sent(&mut self, out_dts: i64, dur: i64) {
        let end = out_dts + dur.max(0);
        self.out_end = Some(self.out_end.map_or(end, |e| e.max(end)));
    }
}

pub fn run(args: RunArgs) -> anyhow::Result<()> {
    ff::init()?;
    ff::util::log::set_level(ff::util::log::Level::Warning);

    let mut outs = Vec::new();
    for (i, url) in args.output.iter().enumerate() {
        let shared = Arc::new(OutputShared {
            url: url.clone(),
            lost: AtomicBool::new(false),
            dropped_full: Default::default(),
            written: Default::default(),
            reconnects: Default::default(),
        });
        let (tx, rx) = sync_channel(args.queue);
        let csv = match &args.csv_dir {
            Some(dir) => {
                std::fs::create_dir_all(dir)?;
                Some(File::create(dir.join(format!("out{i}.csv")))?)
            }
            None => None,
        };
        let opts = parse_opts(&args.out_opt);
        let sh = shared.clone();
        let thread = thread::Builder::new().name(format!("out{i}")).spawn(move || output::run(sh, rx, opts, csv))?;
        outs.push(Out { shared, tx, thread });
    }

    let deadline = args.duration.map(|s| Instant::now() + Duration::from_secs(s));
    let expired = move || deadline.is_some_and(|d| Instant::now() >= d);
    let mut rebase = Rebase::new();
    let mut session = 0u32;
    let mut layout_id = 0u64;
    let mut frame: u64 = 0;
    let mut backoff = Duration::from_millis(250);
    let mut last_status = Instant::now();
    let mut counts = Counts::default();
    let mut mismatches = 0u32;

    while !expired() {
        let mut opts = Dictionary::new();
        for (k, v) in parse_opts(&args.in_opt) {
            opts.set(&k, &v);
        }
        let t_open = Instant::now();
        let dl = deadline;
        let ictx = format::input_with_interrupt_and_dictionary(
            &args.input,
            move || SHUTDOWN.load(Ordering::Relaxed) || dl.is_some_and(|d| Instant::now() >= d),
            opts,
        );
        let mut ictx = match ictx {
            Ok(c) => c,
            Err(e) => {
                warn!(input = %args.input, "open failed: {e}; retry in {backoff:?}");
                thread::sleep(backoff);
                backoff = (backoff * 2).min(Duration::from_secs(2));
                continue;
            }
        };
        backoff = Duration::from_millis(250);
        session += 1;
        layout_id += 1;

        // Map input streams -> output streams (video and audio only).
        let mut map: Vec<Option<usize>> = Vec::new();
        let mut layout = Layout { id: layout_id, sig: Vec::new(), params: Vec::new(), time_bases: Vec::new() };
        let mut video_in: Option<(usize, Option<VideoCodec>)> = None;
        let mut video_delay = 0usize;
        for st in ictx.streams() {
            let p = st.parameters();
            let medium = p.medium();
            if matches!(medium, media::Type::Video | media::Type::Audio) {
                if medium == media::Type::Video && video_in.is_none() {
                    let vc = match p.id() {
                        ff::codec::Id::H264 => Some(VideoCodec::H264),
                        ff::codec::Id::HEVC => Some(VideoCodec::Hevc),
                        other => {
                            warn!("video codec {other:?}: passing through without captions");
                            None
                        }
                    };
                    video_in = Some((st.index(), vc));
                    // SAFETY: reading a plain int field of valid codec parameters.
                    video_delay = unsafe { (*p.as_ptr()).video_delay }.max(0) as usize;
                }
                map.push(Some(layout.params.len()));
                layout.sig.push((medium, p.id()));
                layout.params.push(p.clone());
                layout.time_bases.push(st.time_base());
            } else {
                info!(index = st.index(), ?medium, "ignoring stream");
                map.push(None);
            }
        }
        info!(session, streams = ?layout.sig, open_ms = t_open.elapsed().as_millis() as u64, "input session started");
        let layout = Arc::new(layout);
        let in_tbs: Vec<Rational> = ictx.streams().map(|s| s.time_base()).collect();
        rebase.new_session(in_tbs.len());
        let depth = args.reorder.unwrap_or(video_delay);
        info!(depth, video_delay, "caption reorder depth (frames held for PTS-order captions)");
        let mut reorder = Reorder::new(depth);

        loop {
            if expired() {
                break;
            }
            let mut pkt = Packet::empty();
            match pkt.read(&mut ictx) {
                Ok(()) => {}
                Err(ff::Error::Eof) => {
                    warn!(session, "input EOF; reconnecting");
                    break;
                }
                Err(ff::Error::Other { errno }) if errno == libc_eagain() => continue,
                Err(e) => {
                    warn!(session, "input read error: {e}; reconnecting");
                    break;
                }
            }
            let read_ns = now_ns();
            counts.read += 1;
            let ist = pkt.stream();
            if ist >= map.len() {
                // A stream appeared mid-session (PMT change, e.g. the source
                // restarted with another codec). Re-open to re-probe the layout.
                warn!(stream = ist, "new input stream appeared; re-opening input");
                break;
            }
            let (Some(Some(ost)), Some(tb)) = (map.get(ist).copied(), in_tbs.get(ist).copied()) else {
                continue;
            };
            let Some(in_dts) = pkt.dts().or(pkt.pts()) else {
                counts.no_ts += 1;
                if counts.no_ts < 10 {
                    warn!(stream = ist, "packet without timestamps; dropped");
                }
                continue;
            };
            let in_pts = pkt.pts().unwrap_or(in_dts);
            if pkt.is_corrupt() {
                counts.corrupt += 1;
                warn!(stream = ist, dts = in_dts, "demuxer flagged packet corrupt (e.g. truncated PES); passing through");
            }
            let d90 = ff::rescale::Rescale::rescale(&in_dts, tb, TB90K);
            let delta90 = rebase.delta_for(ist, d90, wall_90k(read_ns));
            let delta = ff::rescale::Rescale::rescale(&delta90, TB90K, tb);
            let out_dts = in_dts + delta;

            let is_video = video_in.is_some_and(|(i, _)| i == ist);
            pkt.set_stream(ost);
            pkt.set_dts(Some(out_dts));
            pkt.set_pts(Some(in_pts + delta));
            let dur90 = ff::rescale::Rescale::rescale(&pkt.duration(), tb, TB90K);
            rebase.sent(d90 + delta90, dur90);
            let meta = Meta { session, frame: None, in_pts, read_ns };

            if is_video {
                let vc = video_in.and_then(|(_, vc)| vc);
                // The TS demuxer keeps the old codec id when a PMT changes a
                // PID's stream_type (source restarted as HEVC on the same UDP
                // port). Detect it from the NAL headers and re-probe.
                if let (Some(vc), Some(d)) = (vc, pkt.data()) {
                    match nal::looks_like_other_codec(d, vc) {
                        Some(true) => mismatches += 1,
                        Some(false) => mismatches = 0,
                        None => {}
                    }
                    if mismatches >= 2 {
                        warn!(expected = ?vc, "video NAL headers do not match the probed codec; re-opening input");
                        mismatches = 0;
                        break;
                    }
                }
                if let (Some(vc), Some(d)) = (vc, pkt.data())
                    && !pkt.is_key()
                    && nal::is_keyframe(d, vc)
                {
                    pkt.set_flags(pkt.flags() | ff::packet::Flags::KEY);
                }
                for (p, m) in reorder.push(pkt, in_pts, meta, &mut frame) {
                    let p = caption(p, &m, vc, args.no_captions, &mut counts);
                    fan_out(&outs, &layout, p, true, m);
                }
            } else {
                fan_out(&outs, &layout, pkt, false, meta);
            }

            if last_status.elapsed() >= Duration::from_secs(10) {
                last_status = Instant::now();
                status(&counts, frame, session, &outs);
            }
        }
        // Session over: release held video (captions assigned in PTS order).
        let vc = video_in.and_then(|(_, vc)| vc);
        for (p, m) in reorder.flush(&mut frame) {
            let p = caption(p, &m, vc, args.no_captions, &mut counts);
            fan_out(&outs, &layout, p, true, m);
        }
    }
    SHUTDOWN.store(true, Ordering::Relaxed);
    status(&counts, frame, session, &outs);
    // Dropping the senders ends the output threads; SHUTDOWN interrupts any
    // blocked open/write. Join them all so no thread is inside libsrt/libav
    // while the process exits (that raced with libsrt's atexit cleanup and
    // aborted with "double free" before this join existed).
    for o in outs {
        drop(o.tx);
        if o.thread.join().is_err() {
            warn!(url = %o.shared.url, "output thread panicked");
        }
    }
    Ok(())
}

/// Assigns caption frame indices in display (PTS) order while packets leave
/// in decode order. With B-frames, the k-th caption must go on the k-th
/// frame *displayed*; a frame's display rank is known once `depth` later
/// frames have arrived, so video is held for `depth` frames (0 = no delay).
struct Reorder {
    depth: usize,
    queue: VecDeque<Held>,
    unassigned: BinaryHeap<Reverse<(i64, u64)>>,
    next_seq: u64,
}

struct Held {
    seq: u64,
    pkt: Packet,
    meta: Meta,
}

impl Reorder {
    fn new(depth: usize) -> Self {
        Self { depth, queue: VecDeque::new(), unassigned: BinaryHeap::new(), next_seq: 0 }
    }

    fn push(&mut self, pkt: Packet, pts: i64, meta: Meta, next_caption: &mut u64) -> Vec<(Packet, Meta)> {
        let seq = self.next_seq;
        self.next_seq += 1;
        self.queue.push_back(Held { seq, pkt, meta });
        self.unassigned.push(Reverse((pts, seq)));
        while self.unassigned.len() > self.depth {
            self.assign_min(next_caption);
        }
        self.drain_ready()
    }

    fn flush(&mut self, next_caption: &mut u64) -> Vec<(Packet, Meta)> {
        while !self.unassigned.is_empty() {
            self.assign_min(next_caption);
        }
        self.drain_ready()
    }

    fn assign_min(&mut self, next_caption: &mut u64) {
        let Some(Reverse((_, seq))) = self.unassigned.pop() else { return };
        let front = self.queue.front().map_or(seq, |h| h.seq);
        if let Some(h) = seq.checked_sub(front).and_then(|i| self.queue.get_mut(i as usize)) {
            h.meta.frame = Some(*next_caption);
            *next_caption += 1;
        }
    }

    fn drain_ready(&mut self) -> Vec<(Packet, Meta)> {
        let mut out = Vec::new();
        while self.queue.front().is_some_and(|h| h.meta.frame.is_some()) {
            if let Some(h) = self.queue.pop_front() {
                out.push((h.pkt, h.meta));
            }
        }
        out
    }
}

/// Splices this frame's caption SEI into the access unit. On any problem the
/// packet passes through unchanged.
fn caption(pkt: Packet, meta: &Meta, vc: Option<VideoCodec>, off: bool, counts: &mut Counts) -> Packet {
    let (Some(vc), Some(frame), false) = (vc, meta.frame, off) else { return pkt };
    let triples = cc::fixture_triples(frame);
    let sei = match vc {
        VideoCodec::H264 => cc::h264_sei_nal(&triples),
        VideoCodec::Hevc => cc::hevc_sei_nal(&triples),
    };
    match pkt.data().map(|d| nal::insert_sei(d, &sei, vc)) {
        Some(Ok(au)) => {
            let mut np = Packet::copy(&au);
            np.set_flags(pkt.flags());
            np.set_duration(pkt.duration());
            np.set_position(pkt.position());
            np.set_stream(pkt.stream());
            np.set_pts(pkt.pts());
            np.set_dts(pkt.dts());
            counts.sei += 1;
            np
        }
        Some(Err(skip)) => {
            counts.sei_skip += 1;
            if counts.sei_skip <= 10 {
                warn!(frame, ?skip, "no SEI inserted; passing packet through");
            }
            pkt
        }
        None => {
            counts.sei_skip += 1;
            pkt
        }
    }
}

fn fan_out(outs: &[Out], layout: &Arc<Layout>, pkt: Packet, video: bool, meta: Meta) {
    for o in outs {
        let msg = PktMsg {
            layout: layout.clone(),
            pkt: pkt.clone(),
            video,
            meta,
        };
        match o.tx.try_send(msg) {
            Ok(()) => {}
            Err(TrySendError::Full(_)) => {
                o.shared.lost.store(true, Ordering::Relaxed);
                o.shared.dropped_full.fetch_add(1, Ordering::Relaxed);
            }
            Err(TrySendError::Disconnected(_)) => {
                warn!(url = %o.shared.url, "output thread gone");
            }
        }
    }
}

#[derive(Default)]
struct Counts {
    read: u64,
    sei: u64,
    sei_skip: u64,
    no_ts: u64,
    corrupt: u64,
}

fn status(c: &Counts, frame: u64, session: u32, outs: &[Out]) {
    let per_out: Vec<String> = outs
        .iter()
        .map(|o| {
            format!(
                "{}: written={} dropped_full={} connects={}",
                o.shared.url,
                o.shared.written.load(Ordering::Relaxed),
                o.shared.dropped_full.load(Ordering::Relaxed),
                o.shared.reconnects.load(Ordering::Relaxed)
            )
        })
        .collect();
    info!(
        session,
        read = c.read,
        frames = frame,
        sei = c.sei,
        sei_skip = c.sei_skip,
        corrupt = c.corrupt,
        rss_kib = rss_kib(),
        outputs = ?per_out,
        "status"
    );
}

fn libc_eagain() -> i32 {
    11 // EAGAIN on Linux
}

fn parse_opts(list: &[String]) -> Vec<(String, String)> {
    list.iter()
        .filter_map(|kv| {
            let (k, v) = kv.split_once('=')?;
            Some((k.to_string(), v.to_string()))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn captions_follow_display_order() {
        // Decode order I P B B P B B with PTS 0 3 1 2 6 4 5.
        let pts = [0, 3, 1, 2, 6, 4, 5];
        let mut r = Reorder::new(2);
        let mut next = 0;
        let mut got = Vec::new();
        for p in pts {
            let meta = Meta { session: 1, frame: None, in_pts: p, read_ns: 0 };
            got.extend(r.push(Packet::empty(), p, meta, &mut next).into_iter().map(|(_, m)| (m.in_pts, m.frame)));
        }
        got.extend(r.flush(&mut next).into_iter().map(|(_, m)| (m.in_pts, m.frame)));
        // Leaves in decode order; caption index == display rank == PTS here.
        let want: Vec<(i64, Option<u64>)> = pts.iter().map(|&p| (p, Some(p as u64))).collect();
        assert_eq!(got, want);
    }

    #[test]
    fn restart_continues_at_wall_pace() {
        let mut r = Rebase::new();
        r.new_session(1);
        let w0 = 1_000_000_000;
        // 10 s of video at 30 fps, arriving on time.
        for i in 0..300 {
            let dts = 126_000 + i * 3000;
            let d = r.delta_for(0, dts, w0 + i * 3000);
            assert_eq!(d, 0);
            r.sent(dts + d, 3000);
        }
        // A stale last packet flushed late must not shift the anchor.
        r.delta_for(0, 126_000 + 299 * 3000, w0 + 299 * 3000 + 450_000);
        // Source restarts 5 s after its last frame was due, PTS from 126000 again.
        let wall = w0 + 299 * 3000 + 450_000;
        let d = r.delta_for(0, 126_000, wall);
        let out = 126_000 + d;
        let end = 126_000 + 300 * 3000;
        assert_eq!(out, end + 450_000 + EPOCH_MARGIN_90K);
    }

    #[test]
    fn depth_zero_is_passthrough() {
        let mut r = Reorder::new(0);
        let mut next = 0;
        let meta = Meta { session: 1, frame: None, in_pts: 0, read_ns: 0 };
        assert_eq!(r.push(Packet::empty(), 0, meta, &mut next).len(), 1);
        assert_eq!(next, 1);
    }
}
