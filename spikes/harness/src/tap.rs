//! `latency tap`: UDP pass-through probe that logs video PES arrival times.
//!
//! The receive thread does only: recv -> read clock -> send (forward) -> hand the
//! datagram to a parser thread over a bounded channel (never blocks; if the
//! parser falls behind, datagrams are still forwarded but not parsed, and the
//! drop is counted). The parser thread parses TS and writes the CSV.

use crate::clock::now_ns;
use crate::stats::Summary;
use crate::ts::{Frame, Stats, TsParser};
use anyhow::{Context, Result};
use std::io::{BufWriter, Write};
use std::net::{SocketAddr, UdpSocket};
use std::sync::mpsc::{Receiver, TrySendError, sync_channel};
use std::time::Duration;

pub struct TapArgs {
    pub listen: SocketAddr,
    pub forward: Option<SocketAddr>,
    pub out: std::path::PathBuf,
    pub duration: Option<f64>,
    pub program: u16,
    pub quiet: bool,
}

struct Msg {
    wall_ns: i64,
    /// recv-return to send-return time for this datagram.
    fwd_ns: i64,
    data: Vec<u8>,
}

pub fn run(a: TapArgs) -> Result<()> {
    let sock = UdpSocket::bind(a.listen).with_context(|| format!("bind {}", a.listen))?;
    sock.set_read_timeout(Some(Duration::from_millis(200)))?;
    let rcvbuf = crate::sock::set_rcvbuf(&sock, 8 << 20);
    let fwd_sock = match a.forward {
        Some(dst) => {
            let bind: SocketAddr = if dst.is_ipv4() { "0.0.0.0:0" } else { "[::]:0" }
                .parse()
                .context("bind addr")?;
            let s = UdpSocket::bind(bind)?;
            s.connect(dst).with_context(|| format!("connect {dst}"))?;
            Some(s)
        }
        None => None,
    };
    let file = std::fs::File::create(&a.out).with_context(|| format!("create {}", a.out.display()))?;
    let (tx, rx) = sync_channel::<Msg>(20_000);
    let quiet = a.quiet;
    let program = a.program;
    let parser = std::thread::spawn(move || parse_loop(rx, file, program, quiet));

    let deadline = a.duration.map(|d| now_ns() + (d * 1e9) as i64);
    let mut buf = vec![0u8; 65536];
    let mut datagrams: u64 = 0;
    let mut send_errors: u64 = 0;
    let mut dropped: u64 = 0;
    eprintln!(
        "tap: listening on {} (SO_RCVBUF {} KiB), forwarding to {}, writing {}",
        a.listen,
        rcvbuf / 1024,
        a.forward.map_or("(nowhere)".to_string(), |f| f.to_string()),
        a.out.display()
    );
    loop {
        if deadline.is_some_and(|d| now_ns() >= d) {
            break;
        }
        let n = match sock.recv(&mut buf) {
            Ok(n) => n,
            Err(e) if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) => continue,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => {
                // e.g. ICMP port unreachable surfaced on some kernels; keep going.
                eprintln!("tap: recv error: {e}");
                continue;
            }
        };
        let t0 = now_ns();
        if let Some(s) = &fwd_sock
            && s.send(&buf[..n]).is_err()
        {
            // Downstream not listening yet (ECONNREFUSED): count and carry on.
            send_errors += 1;
        }
        let t1 = now_ns();
        datagrams += 1;
        match tx.try_send(Msg {
            wall_ns: t0,
            fwd_ns: t1 - t0,
            data: buf[..n].to_vec(),
        }) {
            Ok(()) => {}
            Err(TrySendError::Full(_)) => dropped += 1,
            Err(TrySendError::Disconnected(_)) => break,
        }
    }
    drop(tx);
    let res = parser.join();
    eprintln!("tap: datagrams={datagrams} send_errors={send_errors} parser_drops={dropped}");
    match res {
        Ok(r) => r,
        Err(_) => anyhow::bail!("parser thread panicked"),
    }
}

fn print_stats(s: &Stats, fwd: &Summary, pid: Option<u16>, st: u8) {
    eprintln!(
        "tap: video_pid={} stream_type=0x{st:02x} frames={} packets={} bad_sync={} bad_bytes={} tei={} bad_af={} crc_err={} bad_section={} pat_changes={} pmt_changes={} cc_err={} pes_bad_hdr={} pes_no_pts={} pts_wraps={}",
        pid.map_or("none".into(), |p| format!("0x{p:x}")),
        s.frames, s.packets, s.bad_sync, s.bad_bytes, s.tei, s.bad_af, s.crc_errors, s.bad_section,
        s.pat_changes, s.pmt_changes, s.cc_errors, s.pes_bad_header, s.pes_no_pts, s.pts_wraps
    );
    if fwd.n > 0 {
        eprintln!(
            "tap: forward overhead (recv->send return) us: p50={:.1} p99={:.1} max={:.1} mean={:.1} n={}",
            fwd.p50 / 1e3, fwd.p99 / 1e3, fwd.max / 1e3, fwd.mean / 1e3, fwd.n
        );
    }
}

fn write_rows(w: &mut impl Write, frames: &mut Vec<Frame>) -> std::io::Result<()> {
    for f in frames.drain(..) {
        match f.pts {
            Some(p) => writeln!(w, "{},{},{},{:016x}", f.wall_ns, p, f.seq, f.tail_hash)?,
            None => writeln!(w, "{},,{},{:016x}", f.wall_ns, f.seq, f.tail_hash)?,
        }
    }
    Ok(())
}

fn parse_loop(rx: Receiver<Msg>, file: std::fs::File, program: u16, quiet: bool) -> Result<()> {
    let mut w = BufWriter::new(file);
    writeln!(w, "wallclock_ns,pts_90k,frame_seq,tail_hash")?;
    w.flush()?;
    let mut p = TsParser::new(program);
    let mut frames = Vec::new();
    let mut fwd_ns: Vec<f64> = Vec::new();
    let mut last_print = now_ns();
    let mut write_err = false;
    for m in rx {
        if fwd_ns.len() < 5_000_000 {
            fwd_ns.push(m.fwd_ns as f64);
        }
        p.feed(&m.data, m.wall_ns, &mut frames);
        for e in p.events.drain(..) {
            eprintln!("tap: {e}");
        }
        if !frames.is_empty() {
            // Flush per frame so the CSV is complete even if the tap is killed.
            if write_rows(&mut w, &mut frames).and_then(|_| w.flush()).is_err() && !write_err {
                eprintln!("tap: CSV write failed; continuing to forward");
                write_err = true;
            }
        }
        if !quiet && m.wall_ns - last_print > 5_000_000_000 {
            last_print = m.wall_ns;
            print_stats(&p.stats, &Summary::of(&fwd_ns), p.video_pid(), p.video_stream_type);
        }
    }
    p.flush(&mut frames);
    let _ = write_rows(&mut w, &mut frames);
    let _ = w.flush();
    print_stats(&p.stats, &Summary::of(&fwd_ns), p.video_pid(), p.video_stream_type);
    Ok(())
}
