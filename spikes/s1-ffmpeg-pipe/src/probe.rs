//! Black-box timing helpers. `relay` forwards UDP datagrams unchanged and logs
//! when each video PES (by PTS) first arrives (no --forward: only logs); `measure`
//! reads any FFmpeg URL (e.g. SRT) and logs when each video packet is
//! returned by the demuxer. `stats` summarises the CSVs.

use std::collections::HashMap;
use std::fs::File;
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::net::UdpSocket;
use std::path::Path;
use std::time::{Duration, Instant};

use anyhow::Context;
use ffmpeg_next as ff;

use crate::now_ns;

/// PTS (90 kHz, 33 bit) of every video PES that starts in this TS datagram.
fn video_pes_pts(buf: &[u8], out: &mut Vec<i64>) {
    for ts in buf.as_chunks::<188>().0 {
        if ts[0] != 0x47 || ts[1] & 0x40 == 0 {
            continue; // not synced or no payload_unit_start
        }
        let afc = (ts[3] >> 4) & 3;
        let mut p = 4;
        if afc & 2 != 0 {
            p += 1 + ts[4] as usize;
        }
        if afc & 1 == 0 || p + 14 > 188 {
            continue;
        }
        let pes = &ts[p..];
        if pes[..3] != [0, 0, 1] || pes[3] & 0xF0 != 0xE0 || pes[7] & 0x80 == 0 {
            continue;
        }
        let b = &pes[9..14];
        let pts = ((i64::from(b[0]) >> 1) & 7) << 30
            | i64::from(b[1]) << 22
            | (i64::from(b[2]) >> 1) << 15
            | i64::from(b[3]) << 7
            | i64::from(b[4]) >> 1;
        out.push(pts);
    }
}

pub fn relay(listen: &str, forward: Option<&str>, log: &Path, secs: u64) -> anyhow::Result<()> {
    let sock = UdpSocket::bind(listen).with_context(|| format!("bind {listen}"))?;
    sock.set_read_timeout(Some(Duration::from_millis(500)))?;
    let fwd = UdpSocket::bind("127.0.0.1:0")?;
    let mut w = BufWriter::new(File::create(log)?);
    writeln!(w, "wall_ns,pts")?;
    let end = Instant::now() + Duration::from_secs(secs);
    let mut buf = [0u8; 65536];
    let mut pts = Vec::new();
    while Instant::now() < end {
        let n = match sock.recv(&mut buf) {
            Ok(n) => n,
            Err(e) if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) => continue,
            Err(e) => return Err(e.into()),
        };
        let t = now_ns();
        if let Some(to) = forward {
            let _ = fwd.send_to(&buf[..n], to);
        }
        pts.clear();
        video_pes_pts(&buf[..n], &mut pts);
        for p in &pts {
            writeln!(w, "{t},{p}")?;
        }
    }
    w.flush()?;
    Ok(())
}

pub fn measure(url: &str, log: &Path, secs: u64) -> anyhow::Result<()> {
    ff::init()?;
    let end = Instant::now() + Duration::from_secs(secs);
    let mut ictx = ff::format::input_with_interrupt(url, move || Instant::now() >= end)?;
    let video = ictx.streams().best(ff::media::Type::Video).map(|s| (s.index(), s.time_base()));
    let Some((vi, tb)) = video else { anyhow::bail!("no video stream") };
    let mut w = BufWriter::new(File::create(log)?);
    writeln!(w, "wall_ns,pts")?;
    let mut pkt = ff::Packet::empty();
    while Instant::now() < end {
        match pkt.read(&mut ictx) {
            Ok(()) => {}
            Err(ff::Error::Other { errno: 11 }) => continue,
            Err(_) => break,
        }
        if pkt.stream() == vi
            && let Some(p) = pkt.pts()
        {
            let p90 = ff::rescale::Rescale::rescale(&p, tb, ff::Rational(1, 90_000)) & ((1 << 33) - 1);
            writeln!(w, "{},{p90}", now_ns())?;
        }
    }
    w.flush()?;
    Ok(())
}

fn percentiles(mut v: Vec<f64>) -> String {
    if v.is_empty() {
        return "n=0".into();
    }
    v.sort_by(f64::total_cmp);
    let q = |p: f64| v[((v.len() - 1) as f64 * p).round() as usize];
    format!(
        "n={} p50={:.2} p95={:.2} p99={:.2} max={:.2} min={:.2}",
        v.len(),
        q(0.5),
        q(0.95),
        q(0.99),
        v[v.len() - 1],
        v[0]
    )
}

/// Pipe CSV: delay_us column -> ms percentiles, optionally skipping the first N seconds of rows.
pub fn stats_pipe(csv: &Path, skip_rows: usize) -> anyhow::Result<()> {
    let r = BufReader::new(File::open(csv)?);
    let mut d = Vec::new();
    for line in r.lines().skip(1 + skip_rows) {
        let line = line?;
        if let Some(us) = line.rsplit(',').next().and_then(|x| x.parse::<f64>().ok()) {
            d.push(us / 1000.0);
        }
    }
    println!("{}: pass-through ms {}", csv.display(), percentiles(d));
    Ok(())
}

fn load(path: &Path) -> anyhow::Result<HashMap<i64, u64>> {
    let mut m = HashMap::new();
    for line in BufReader::new(File::open(path)?).lines().skip(1) {
        let line = line?;
        let mut it = line.split(',');
        if let (Some(t), Some(p)) = (it.next(), it.next())
            && let (Ok(t), Ok(p)) = (t.parse::<u64>(), p.parse::<i64>())
        {
            m.entry(p).or_insert(t); // first arrival wins
        }
    }
    Ok(m)
}

/// Joins two (wall_ns, pts) logs on PTS and prints b - a percentiles in ms.
pub fn stats_join(a: &Path, b: &Path, skip_first: usize) -> anyhow::Result<()> {
    let (ma, mb) = (load(a)?, load(b)?);
    let mut pairs: Vec<(i64, f64)> = ma
        .iter()
        .filter_map(|(p, ta)| mb.get(p).map(|tb| (*p, (*tb as f64 - *ta as f64) / 1e6)))
        .collect();
    pairs.sort_by_key(|x| x.0);
    let d: Vec<f64> = pairs.into_iter().skip(skip_first).map(|x| x.1).collect();
    println!("{} -> {}: black-box ms {} (unmatched a={})", a.display(), b.display(), percentiles(d), ma.len());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_pes_pts() {
        let mut ts = [0xFFu8; 188];
        ts[..4].copy_from_slice(&[0x47, 0x41, 0x00, 0x10]);
        // PES: start code, stream 0xE0, len 0, flags 0x80 0x80, hdr len 5, PTS = 126000
        let pts: i64 = 126_000;
        let pes = [
            0,
            0,
            1,
            0xE0,
            0,
            0,
            0x80,
            0x80,
            5,
            0x21 | (((pts >> 30) & 7) << 1) as u8,
            (pts >> 22) as u8,
            (((pts >> 15) & 0x7F) << 1) as u8 | 1,
            (pts >> 7) as u8,
            ((pts & 0x7F) << 1) as u8 | 1,
        ];
        ts[4..4 + pes.len()].copy_from_slice(&pes);
        let mut out = Vec::new();
        video_pes_pts(&ts, &mut out);
        assert_eq!(out, [pts]);
    }
}
