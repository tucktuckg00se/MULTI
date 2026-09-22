//! Minimal, defensive MPEG-TS parser: PAT -> PMT -> first video PID -> PES starts.
//!
//! Never panics on bad input: malformed packets and sections are counted in
//! [`Stats`] and skipped. For each video PES it reports the wallclock of the
//! datagram that carried its first packet, its PTS (33-bit, unwrapped to a
//! monotonic i64), a sequence number, and a hash of the PES payload tail (used
//! to match frames whose PTS was rewritten; see `report`).

use std::collections::HashMap;

pub const TS_PACKET: usize = 188;
pub const PTS_WRAP: i64 = 1 << 33;
/// Bytes of PES payload tail kept for the content hash.
const TAIL_KEEP: usize = 256;
/// Bytes (after trimming trailing zeros) that go into the hash.
const TAIL_HASH: usize = 128;

#[derive(Debug, Default, Clone)]
pub struct Stats {
    pub packets: u64,
    pub bad_sync: u64,
    pub bad_bytes: u64,
    pub tei: u64,
    pub bad_af: u64,
    pub bad_section: u64,
    pub crc_errors: u64,
    pub pat_changes: u64,
    pub pmt_changes: u64,
    pub cc_errors: u64,
    pub pes_bad_header: u64,
    pub pes_no_pts: u64,
    pub pts_wraps: u64,
    pub frames: u64,
}

/// One video PES (normally one frame / access unit).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    pub wall_ns: i64,
    /// Unwrapped PTS in 90 kHz ticks; `None` if the PES header had no PTS.
    pub pts: Option<i64>,
    pub seq: u64,
    /// FNV-1a of the last payload bytes (trailing zeros trimmed); 0 if empty.
    pub tail_hash: u64,
}

struct PesState {
    wall_ns: i64,
    pts: Option<i64>,
    seq: u64,
    tail: Vec<u8>,
}

#[derive(Default)]
struct SectionAsm {
    buf: Vec<u8>,
    active: bool,
}

#[derive(Default)]
struct PtsUnwrap {
    last_raw: Option<i64>,
    wraps: i64,
}

impl PtsUnwrap {
    /// Returns the unwrapped value and whether a forward wrap happened.
    fn unwrap(&mut self, raw: i64) -> (i64, bool) {
        let half = PTS_WRAP / 2;
        let Some(last) = self.last_raw else {
            self.last_raw = Some(raw);
            return (raw + self.wraps * PTS_WRAP, false);
        };
        if last - raw > half {
            // Forward across the 2^33 boundary.
            self.wraps += 1;
            self.last_raw = Some(raw);
            (raw + self.wraps * PTS_WRAP, true)
        } else if raw - last > half {
            // A late value from before the wrap: don't move the state.
            (raw + (self.wraps - 1) * PTS_WRAP, false)
        } else {
            self.last_raw = Some(raw);
            (raw + self.wraps * PTS_WRAP, false)
        }
    }
}

pub struct TsParser {
    pub stats: Stats,
    /// Only follow this program number (0 = first program in the PAT).
    program: u16,
    pmt_pid: Option<u16>,
    video_pid: Option<u16>,
    pub video_stream_type: u8,
    pat_crc: Option<u32>,
    pmt_crc: Option<u32>,
    sections: HashMap<u16, SectionAsm>,
    last_cc: Option<u8>,
    pes: Option<PesState>,
    unwrap: PtsUnwrap,
    seq: u64,
    /// Partial packet left at the end of the previous chunk (datagrams that are not
    /// multiples of 188 bytes).
    carry: Vec<u8>,
    /// Human-readable events (PMT change etc.) for the caller to log.
    pub events: Vec<String>,
}

fn is_video(stream_type: u8) -> bool {
    // MPEG-1/2 video, MPEG-4 part 2, H.264, HEVC, VVC, AVS2/3.
    matches!(stream_type, 0x01 | 0x02 | 0x10 | 0x1B | 0x24 | 0x33 | 0xD2 | 0xD4)
}

pub fn crc32_mpeg2(data: &[u8]) -> u32 {
    let mut crc: u32 = 0xFFFF_FFFF;
    for &b in data {
        crc ^= (b as u32) << 24;
        for _ in 0..8 {
            crc = if crc & 0x8000_0000 != 0 {
                (crc << 1) ^ 0x04C1_1DB7
            } else {
                crc << 1
            };
        }
    }
    crc
}

fn fnv1a(data: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in data {
        h ^= b as u64;
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    h
}

fn parse_pts(b: &[u8]) -> Option<i64> {
    if b.len() < 5 {
        return None;
    }
    let v = (((b[0] as i64) >> 1) & 0x07) << 30
        | (b[1] as i64) << 22
        | ((b[2] as i64) >> 1) << 15
        | (b[3] as i64) << 7
        | (b[4] as i64) >> 1;
    Some(v)
}

impl Default for TsParser {
    fn default() -> Self {
        Self::new(0)
    }
}

impl TsParser {
    pub fn new(program: u16) -> Self {
        Self {
            stats: Stats::default(),
            program,
            pmt_pid: None,
            video_pid: None,
            video_stream_type: 0,
            pat_crc: None,
            pmt_crc: None,
            sections: HashMap::new(),
            last_cc: None,
            pes: None,
            unwrap: PtsUnwrap::default(),
            seq: 0,
            carry: Vec::new(),
            events: Vec::new(),
        }
    }

    pub fn video_pid(&self) -> Option<u16> {
        self.video_pid
    }

    /// Feeds one datagram (or any byte chunk that starts on a packet boundary,
    /// ideally). Completed frames are appended to `out`.
    pub fn feed(&mut self, data: &[u8], wall_ns: i64, out: &mut Vec<Frame>) {
        if self.carry.is_empty() {
            self.feed_aligned(data, wall_ns, out);
        } else {
            let mut joined = std::mem::take(&mut self.carry);
            joined.extend_from_slice(data);
            self.feed_aligned(&joined, wall_ns, out);
        }
    }

    fn feed_aligned(&mut self, data: &[u8], wall_ns: i64, out: &mut Vec<Frame>) {
        let mut i = 0;
        while i < data.len() {
            if data[i] != 0x47 {
                // Resync: next 0x47 that is followed by another 0x47 (or ends the buffer).
                let start = i;
                i += 1;
                while i < data.len() {
                    if data[i] == 0x47
                        && (i + TS_PACKET >= data.len() || data[i + TS_PACKET] == 0x47)
                    {
                        break;
                    }
                    i += 1;
                }
                self.stats.bad_sync += 1;
                self.stats.bad_bytes += (i - start) as u64;
                continue;
            }
            if i + TS_PACKET > data.len() {
                // Keep the partial packet for the next chunk.
                self.carry.extend_from_slice(&data[i..]);
                break;
            }
            let pkt = &data[i..i + TS_PACKET];
            self.packet(pkt, wall_ns, out);
            i += TS_PACKET;
        }
    }

    /// Emits the PES in progress (call at end of stream).
    pub fn flush(&mut self, out: &mut Vec<Frame>) {
        self.finish_pes(out);
    }

    fn packet(&mut self, p: &[u8], wall_ns: i64, out: &mut Vec<Frame>) {
        self.stats.packets += 1;
        if p[1] & 0x80 != 0 {
            self.stats.tei += 1;
            return;
        }
        let pusi = p[1] & 0x40 != 0;
        let pid = ((p[1] as u16 & 0x1F) << 8) | p[2] as u16;
        let afc = (p[3] >> 4) & 0x3;
        let cc = p[3] & 0x0F;
        if afc == 0 {
            self.stats.bad_af += 1;
            return;
        }
        let mut off = 4;
        let mut discontinuity = false;
        if afc & 0x2 != 0 {
            let af_len = p[4] as usize;
            let max = if afc == 2 { 183 } else { 182 };
            if af_len > max {
                self.stats.bad_af += 1;
                return;
            }
            if af_len > 0 {
                discontinuity = p[5] & 0x80 != 0;
            }
            off = 5 + af_len;
        }
        let has_payload = afc & 0x1 != 0;
        let payload: &[u8] = if has_payload { &p[off..] } else { &[] };

        if pid == 0 || Some(pid) == self.pmt_pid {
            self.psi(pid, pusi, payload);
        } else if Some(pid) == self.video_pid {
            if has_payload {
                if let Some(last) = self.last_cc {
                    let expected = (last + 1) & 0x0F;
                    if cc != expected && cc != last && !discontinuity {
                        self.stats.cc_errors += 1;
                    }
                }
                self.last_cc = Some(cc);
            }
            self.video(pusi, payload, wall_ns, out);
        }
    }

    fn psi(&mut self, pid: u16, pusi: bool, payload: &[u8]) {
        if payload.is_empty() {
            return;
        }
        let mut complete: Vec<Vec<u8>> = Vec::new();
        {
            let asm = self.sections.entry(pid).or_default();
            let mut data = payload;
            if pusi {
                let ptr = data[0] as usize;
                if 1 + ptr > data.len() {
                    self.stats.bad_section += 1;
                    asm.active = false;
                    asm.buf.clear();
                    return;
                }
                if asm.active {
                    asm.buf.extend_from_slice(&data[1..1 + ptr]);
                    if let Some(s) = take_section(asm) {
                        complete.push(s);
                    }
                }
                data = &data[1 + ptr..];
                asm.buf.clear();
                asm.active = true;
            } else if !asm.active {
                return;
            }
            asm.buf.extend_from_slice(data);
            // Pull out every complete section in the buffer.
            loop {
                if asm.buf.first().is_none_or(|&b| b == 0xFF) {
                    asm.buf.clear();
                    asm.active = false;
                    break;
                }
                match take_section(asm) {
                    Some(s) => complete.push(s),
                    None => break,
                }
            }
            if asm.buf.len() > 4096 {
                asm.buf.clear();
                asm.active = false;
                self.stats.bad_section += 1;
            }
        }
        for s in complete {
            self.section(pid, &s);
        }
    }

    fn section(&mut self, pid: u16, s: &[u8]) {
        if s.len() < 12 || crc32_mpeg2(s) != 0 {
            self.stats.crc_errors += 1;
            return;
        }
        // Skip sections with current_next_indicator == 0.
        if s[5] & 0x01 == 0 {
            return;
        }
        let crc = u32::from_be_bytes([s[s.len() - 4], s[s.len() - 3], s[s.len() - 2], s[s.len() - 1]]);
        let body = &s[8..s.len() - 4];
        if pid == 0 && s[0] == 0x00 {
            if self.pat_crc == Some(crc) {
                return;
            }
            self.pat_crc = Some(crc);
            let mut new_pmt = None;
            for e in body.as_chunks::<4>().0 {
                let prog = u16::from_be_bytes([e[0], e[1]]);
                let ppid = ((e[2] as u16 & 0x1F) << 8) | e[3] as u16;
                if prog == 0 {
                    continue; // network PID
                }
                if self.program == 0 || prog == self.program {
                    new_pmt = Some(ppid);
                    break;
                }
            }
            if new_pmt != self.pmt_pid {
                if self.pmt_pid.is_some() {
                    self.stats.pat_changes += 1;
                    self.events.push(format!("PAT change: PMT PID {:?} -> {:?}", self.pmt_pid, new_pmt));
                }
                if let Some(old) = self.pmt_pid {
                    self.sections.remove(&old);
                }
                self.pmt_pid = new_pmt;
                self.pmt_crc = None;
            }
        } else if Some(pid) == self.pmt_pid && s[0] == 0x02 {
            if self.pmt_crc == Some(crc) {
                return;
            }
            self.pmt_crc = Some(crc);
            if body.len() < 4 {
                self.stats.bad_section += 1;
                return;
            }
            let pil = ((body[2] as usize & 0x0F) << 8) | body[3] as usize;
            let mut i = 4 + pil;
            let mut found = None;
            while i + 5 <= body.len() {
                let st = body[i];
                let epid = ((body[i + 1] as u16 & 0x1F) << 8) | body[i + 2] as u16;
                let esl = ((body[i + 3] as usize & 0x0F) << 8) | body[i + 4] as usize;
                if found.is_none() && is_video(st) {
                    found = Some((epid, st));
                }
                i += 5 + esl;
            }
            let new_pid = found.map(|f| f.0);
            if new_pid != self.video_pid {
                if self.video_pid.is_some() {
                    self.stats.pmt_changes += 1;
                    self.events.push(format!("PMT change: video PID {:?} -> {:?}", self.video_pid, new_pid));
                }
                self.pes = None; // drop a partial PES from the old PID
                self.last_cc = None;
                self.video_pid = new_pid;
            }
            if let Some((_, st)) = found {
                self.video_stream_type = st;
            }
        } else {
            self.stats.bad_section += 1;
        }
    }

    fn video(&mut self, pusi: bool, payload: &[u8], wall_ns: i64, out: &mut Vec<Frame>) {
        if pusi {
            self.finish_pes(out);
            // PES header: 00 00 01 sid len(2) flags1 flags2 hdr_len ...
            if payload.len() < 9 || payload[0..3] != [0, 0, 1] {
                self.stats.pes_bad_header += 1;
                return;
            }
            let flags2 = payload[7];
            let hdr_len = payload[8] as usize;
            let pts = if flags2 & 0x80 != 0 {
                match payload.get(9..14).and_then(parse_pts) {
                    Some(raw) => {
                        let (v, wrapped) = self.unwrap.unwrap(raw);
                        if wrapped {
                            self.stats.pts_wraps += 1;
                        }
                        Some(v)
                    }
                    None => {
                        self.stats.pes_bad_header += 1;
                        None
                    }
                }
            } else {
                None
            };
            if pts.is_none() {
                self.stats.pes_no_pts += 1;
            }
            self.seq += 1;
            let mut st = PesState {
                wall_ns,
                pts,
                seq: self.seq,
                tail: Vec::with_capacity(TAIL_KEEP * 2),
            };
            if let Some(es) = payload.get(9 + hdr_len..) {
                push_tail(&mut st.tail, es);
            }
            self.pes = Some(st);
        } else if let Some(st) = self.pes.as_mut() {
            push_tail(&mut st.tail, payload);
        }
    }

    fn finish_pes(&mut self, out: &mut Vec<Frame>) {
        if let Some(st) = self.pes.take() {
            let mut t: &[u8] = &st.tail;
            while let Some((&0, rest)) = t.split_last() {
                t = rest;
            }
            let h = if t.is_empty() {
                0
            } else {
                fnv1a(&t[t.len().saturating_sub(TAIL_HASH)..])
            };
            self.stats.frames += 1;
            out.push(Frame {
                wall_ns: st.wall_ns,
                pts: st.pts,
                seq: st.seq,
                tail_hash: h,
            });
        }
    }
}

fn push_tail(tail: &mut Vec<u8>, data: &[u8]) {
    tail.extend_from_slice(data);
    if tail.len() > TAIL_KEEP * 2 {
        let cut = tail.len() - TAIL_KEEP;
        tail.drain(..cut);
    }
}

/// Removes one complete section from the front of `asm.buf`, if present.
fn take_section(asm: &mut SectionAsm) -> Option<Vec<u8>> {
    if asm.buf.len() < 3 {
        return None;
    }
    let len = 3 + (((asm.buf[1] as usize) & 0x0F) << 8 | asm.buf[2] as usize);
    if asm.buf.len() < len {
        return None;
    }
    Some(asm.buf.drain(..len).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pkt(pid: u16, pusi: bool, cc: u8, payload: &[u8]) -> Vec<u8> {
        let mut p = vec![0x47, ((pusi as u8) << 6) | (pid >> 8) as u8, pid as u8];
        let room = 184;
        if payload.len() >= room {
            p.push(0x10 | cc);
            p.extend_from_slice(&payload[..room]);
        } else {
            // Adaptation field stuffing.
            let af_len = room - payload.len() - 1;
            p.push(0x30 | cc);
            p.push(af_len as u8);
            if af_len > 0 {
                p.push(0);
                p.extend(std::iter::repeat_n(0xFF, af_len - 1));
            }
            p.extend_from_slice(payload);
        }
        assert_eq!(p.len(), 188);
        p
    }

    fn section(table_id: u8, ext: u16, version: u8, body: &[u8]) -> Vec<u8> {
        let len = 5 + body.len() + 4;
        let mut s = vec![
            table_id,
            0xB0 | (len >> 8) as u8,
            len as u8,
            (ext >> 8) as u8,
            ext as u8,
            0xC1 | (version << 1),
            0,
            0,
        ];
        s.extend_from_slice(body);
        let crc = crc32_mpeg2(&s);
        s.extend_from_slice(&crc.to_be_bytes());
        s
    }

    fn pat(pmt_pid: u16, version: u8) -> Vec<u8> {
        let body = [0, 1, 0xE0 | (pmt_pid >> 8) as u8, pmt_pid as u8];
        let mut p = vec![0];
        p.extend(section(0, 1, version, &body));
        pkt(0, true, 0, &p)
    }

    fn pmt(pmt_pid: u16, video_pid: u16, version: u8) -> Vec<u8> {
        let body = [
            0xE0 | (video_pid >> 8) as u8,
            video_pid as u8,
            0xF0,
            0,
            0x1B,
            0xE0 | (video_pid >> 8) as u8,
            video_pid as u8,
            0xF0,
            0,
        ];
        let mut p = vec![0];
        p.extend(section(2, 1, version, &body));
        pkt(pmt_pid, true, 0, &p)
    }

    fn pes(pid: u16, pts: i64, cc: u8, fill: u8) -> Vec<u8> {
        let p = pts & (PTS_WRAP - 1);
        let mut h = vec![0, 0, 1, 0xE0, 0, 0, 0x80, 0x80, 5];
        h.push(0x21 | (((p >> 30) & 7) as u8) << 1);
        h.push((p >> 22) as u8);
        h.push(0x01 | ((p >> 15) as u8) << 1);
        h.push((p >> 7) as u8);
        h.push(0x01 | (p as u8) << 1);
        h.extend(std::iter::repeat_n(fill, 300));
        let mut v = pkt(pid, true, cc, &h[..184]);
        v.extend(pkt(pid, false, (cc + 1) & 15, &h[184..]));
        v
    }

    #[test]
    fn crc_known_value() {
        // CRC-32/MPEG-2 check value for "123456789".
        assert_eq!(crc32_mpeg2(b"123456789"), 0x0376_E6E7);
    }

    #[test]
    fn synthetic_stream_and_pmt_change() {
        let mut t = TsParser::default();
        let mut out = Vec::new();
        let mut d = pat(0x1000, 0);
        d.extend(pmt(0x1000, 0x100, 0));
        t.feed(&d, 1, &mut out);
        assert_eq!(t.video_pid(), Some(0x100));
        t.feed(&pes(0x100, 1000, 0, 1), 2, &mut out);
        t.feed(&pes(0x100, 4000, 2, 2), 3, &mut out);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].pts, Some(1000));
        assert_eq!(out[0].wall_ns, 2);
        // New PMT moves video to PID 0x200; the partial PES is dropped.
        t.feed(&pmt(0x1000, 0x200, 1), 4, &mut out);
        assert_eq!(t.video_pid(), Some(0x200));
        assert_eq!(t.stats.pmt_changes, 1);
        t.feed(&pes(0x200, 7000, 0, 3), 5, &mut out);
        t.flush(&mut out);
        assert_eq!(out.len(), 2);
        assert_eq!(out[1].pts, Some(7000));
        assert_eq!(t.stats.cc_errors, 0);
        // PAT change to a new PMT PID.
        t.feed(&pat(0x1100, 1), 6, &mut out);
        assert_eq!(t.stats.pat_changes, 1);
        t.feed(&pmt(0x1100, 0x300, 0), 7, &mut out);
        assert_eq!(t.video_pid(), Some(0x300));
    }

    #[test]
    fn pts_wrap_is_unwrapped() {
        let mut t = TsParser::default();
        let mut out = Vec::new();
        let mut d = pat(0x1000, 0);
        d.extend(pmt(0x1000, 0x100, 0));
        t.feed(&d, 0, &mut out);
        let near = PTS_WRAP - 3000;
        t.feed(&pes(0x100, near, 0, 1), 1, &mut out);
        t.feed(&pes(0x100, near + 3000, 2, 2), 2, &mut out);
        t.feed(&pes(0x100, near + 6000, 4, 3), 3, &mut out);
        t.flush(&mut out);
        let pts: Vec<_> = out.iter().map(|f| f.pts.unwrap_or(-1)).collect();
        assert_eq!(pts, vec![near, near + 3000, near + 6000]);
        assert_eq!(t.stats.pts_wraps, 1);
    }

    #[test]
    fn garbage_never_panics() {
        let mut t = TsParser::default();
        let mut out = Vec::new();
        // Pseudo-random bytes, with and without sync bytes sprinkled in.
        let mut x: u32 = 12345;
        for round in 0..200 {
            let mut buf = Vec::new();
            for j in 0..(round * 37 % 1500) {
                x = x.wrapping_mul(1_103_515_245).wrapping_add(12345);
                let b = (x >> 16) as u8;
                buf.push(if j % 188 == 0 && round % 2 == 0 { 0x47 } else { b });
            }
            t.feed(&buf, round as i64, &mut out);
        }
        // Truncated / corrupt real packets.
        let mut d = pat(0x1000, 0);
        d.extend(pmt(0x1000, 0x100, 0));
        d[5] ^= 0xFF; // break PAT CRC
        t.feed(&d, 0, &mut out);
        t.feed(&d[..100], 0, &mut out);
        t.flush(&mut out);
        assert!(t.stats.bad_sync > 0 || t.stats.crc_errors > 0);
    }

    #[test]
    fn packets_split_across_datagrams() {
        let mut d = pat(0x1000, 0);
        d.extend(pmt(0x1000, 0x100, 0));
        d.extend(pes(0x100, 1000, 0, 1));
        d.extend(pes(0x100, 4000, 2, 2));
        let mut t = TsParser::default();
        let mut out = Vec::new();
        for (k, c) in d.chunks(100).enumerate() {
            t.feed(c, k as i64, &mut out);
        }
        t.flush(&mut out);
        assert_eq!(out.len(), 2);
        assert_eq!(t.stats.bad_sync, 0);
        assert_eq!(out[1].pts, Some(4000));
    }

    #[test]
    fn content_hash_ignores_trailing_zeros_and_prefix() {
        let mut a = Vec::new();
        push_tail(&mut a, &[9; 1000]);
        push_tail(&mut a, &[1, 2, 3]);
        assert!(a.len() <= TAIL_KEEP * 2);
        assert_eq!(&a[a.len() - 3..], &[1, 2, 3]);
    }

    /// Parses a real FFmpeg-made MPEG-TS (skipped if ffmpeg is missing).
    #[test]
    fn ffmpeg_generated_ts() {
        let dir = std::env::temp_dir().join(format!("latency-test-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("t.ts");
        let status = std::process::Command::new("ffmpeg")
            .args(["-v", "error", "-y", "-f", "lavfi", "-i", "testsrc2=size=320x240:rate=30", "-t", "2"])
            .args(["-c:v", "libx264", "-bf", "0", "-g", "30", "-f", "mpegts"])
            .arg(&path)
            .status();
        let Ok(status) = status else {
            eprintln!("ffmpeg not found; skipping");
            return;
        };
        assert!(status.success());
        let data = std::fs::read(&path).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
        let mut t = TsParser::default();
        let mut out = Vec::new();
        // Feed in 7-packet "datagrams" like a UDP sender.
        for chunk in data.chunks(TS_PACKET * 7) {
            t.feed(chunk, 0, &mut out);
        }
        t.flush(&mut out);
        assert_eq!(t.video_stream_type, 0x1B);
        assert_eq!(out.len(), 60, "stats: {:?}", t.stats);
        let pts: Vec<i64> = out.iter().map(|f| f.pts.unwrap()).collect();
        for w in pts.windows(2) {
            assert_eq!(w[1] - w[0], 3000);
        }
        assert_eq!(t.stats.cc_errors, 0);
        assert_eq!(t.stats.crc_errors, 0);
        // Distinct frames give distinct content hashes.
        let mut hashes: Vec<u64> = out.iter().map(|f| f.tail_hash).collect();
        hashes.sort_unstable();
        hashes.dedup();
        assert_eq!(hashes.len(), 60);
    }
}
