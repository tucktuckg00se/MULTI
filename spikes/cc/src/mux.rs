//! Per-frame caption multiplexer: CC1–CC4 (CEA-608) plus CEA-708 services 1–6
//! into one `cc_data` list per video frame.

use crate::cea608::{Cc608Encoder, NULL_PAIR};
use crate::cea708::Cc708Encoder;
use crate::{CcTriple, CcType, Channel};

/// Video frame rates with their A/53 / CEA-708 `cc_count`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FrameRate {
    Fps23_976,
    Fps24,
    Fps25,
    Fps29_97,
    Fps30,
    Fps50,
    Fps59_94,
    Fps60,
}

impl FrameRate {
    /// Nearest supported rate for a measured fps (e.g. 29.97 or 30000/1001).
    /// `None` outside 20–65 fps or for non-finite input.
    pub fn from_fps(fps: f64) -> Option<Self> {
        if !fps.is_finite() || !(20.0..=65.0).contains(&fps) {
            return None;
        }
        let all = [
            (23.976, Self::Fps23_976),
            (24.0, Self::Fps24),
            (25.0, Self::Fps25),
            (29.97, Self::Fps29_97),
            (30.0, Self::Fps30),
            (50.0, Self::Fps50),
            (59.94, Self::Fps59_94),
            (60.0, Self::Fps60),
        ];
        all.iter()
            .min_by(|a, b| (a.0 - fps).abs().total_cmp(&(b.0 - fps).abs()))
            .map(|x| x.1)
    }

    /// Triples per frame, fixed for the rate so the caption channel runs at
    /// 9600 bit/s (600 triples/s): 25 @24, 24 @25, 20 @30, 12 @50, 10 @60.
    pub fn cc_count(self) -> usize {
        match self {
            Self::Fps23_976 | Self::Fps24 => 25,
            Self::Fps25 => 24,
            Self::Fps29_97 | Self::Fps30 => 20,
            Self::Fps50 => 12,
            Self::Fps59_94 | Self::Fps60 => 10,
        }
    }

    /// 50/59.94/60 fps: each 608 field gets a pair on alternate frames only,
    /// keeping 608 at its fixed 2 bytes per field per 1/30 s.
    pub fn is_double_rate(self) -> bool {
        matches!(self, Self::Fps50 | Self::Fps59_94 | Self::Fps60)
    }
}

/// Per-frame caption multiplexer.
///
/// Holds up to four 608 encoders (one per channel) and up to six 708
/// encoders (one per service). Call [`next_frame`](Self::next_frame) once per
/// video frame **in display order** and put the result in that frame's SEI
/// with [`crate::h264_sei_nal`] / [`crate::hevc_sei_nal`].
///
/// Each frame's list is always exactly [`FrameRate::cc_count`] triples:
///
/// 1. one field-1 triple (CC1/CC2), then one field-2 triple (CC3/CC4) —
///    a null pair (0x80 0x80) when idle; at 50/60 fps the field not served
///    this frame is marked `valid = false`;
/// 2. `cc_count - 2` DTVCC triples: at most one DTVCC packet per frame (first
///    pair `DtvccStart`, rest `DtvccData`), then invalid padding.
///
/// Two channels on one field (CC1+CC2 or CC3+CC4) share its bandwidth: they
/// take turns a caption line at a time.
///
/// ```
/// use cc::{CcMux, FrameRate, Channel, Cc608Encoder, Cc708Encoder, h264_sei_nal};
/// let mut mux = CcMux::new(FrameRate::Fps30);
/// mux.add_608(Cc608Encoder::new(Channel::Cc1));
/// if let Some(enc) = Cc708Encoder::new(1) { mux.add_708(enc); }
/// mux.push_text_608(Channel::Cc1, "HELLO");
/// mux.push_text_708(1, "Hello");
/// let triples = mux.next_frame();
/// assert_eq!(triples.len(), 20);
/// let _sei = h264_sei_nal(&triples);
/// ```
#[derive(Clone, Debug)]
pub struct CcMux {
    rate: FrameRate,
    frame: u64,
    cc608: [Option<Cc608Encoder>; 4],
    /// Channel index (0..4) currently holding each field.
    field_owner: [usize; 2],
    cc708: Vec<Cc708Encoder>,
    next_service: usize,
    sequence: u8,
}

fn index(ch: Channel) -> usize {
    match ch {
        Channel::Cc1 => 0,
        Channel::Cc2 => 1,
        Channel::Cc3 => 2,
        Channel::Cc4 => 3,
    }
}

impl CcMux {
    pub fn new(rate: FrameRate) -> Self {
        Self {
            rate,
            frame: 0,
            cc608: [None, None, None, None],
            field_owner: [0, 2],
            cc708: Vec::new(),
            next_service: 0,
            sequence: 0,
        }
    }

    pub fn rate(&self) -> FrameRate {
        self.rate
    }

    /// Triples per frame returned by [`next_frame`](Self::next_frame).
    pub fn cc_count(&self) -> usize {
        self.rate.cc_count()
    }

    /// Frames produced so far.
    pub fn frame_count(&self) -> u64 {
        self.frame
    }

    /// Adds (or replaces) the encoder for its channel.
    pub fn add_608(&mut self, enc: Cc608Encoder) {
        let i = index(enc.channel);
        self.cc608[i] = Some(enc);
    }

    /// Adds (or replaces) the encoder for its service.
    pub fn add_708(&mut self, enc: Cc708Encoder) {
        self.cc708.retain(|e| e.service() != enc.service());
        self.cc708.push(enc);
        self.cc708.sort_by_key(|e| e.service());
    }

    pub fn cc608_mut(&mut self, ch: Channel) -> Option<&mut Cc608Encoder> {
        self.cc608[index(ch)].as_mut()
    }

    pub fn cc708_mut(&mut self, service: u8) -> Option<&mut Cc708Encoder> {
        self.cc708.iter_mut().find(|e| e.service() == service)
    }

    /// Queues text on a 608 channel. Returns false if that channel has no encoder.
    pub fn push_text_608(&mut self, ch: Channel, text: &str) -> bool {
        self.cc608_mut(ch).map(|e| e.push_text(text)).is_some()
    }

    /// Queues text on a 708 service. Returns false if that service has no encoder.
    pub fn push_text_708(&mut self, service: u8, text: &str) -> bool {
        self.cc708_mut(service).map(|e| e.push_text(text)).is_some()
    }

    /// True when nothing is queued anywhere.
    pub fn is_idle(&self) -> bool {
        self.cc608.iter().flatten().all(|e| e.pending() == 0)
            && self.cc708.iter().all(|e| e.is_idle())
    }

    /// This frame's `cc_data` triples (exactly [`cc_count`](Self::cc_count)).
    pub fn next_frame(&mut self) -> Vec<CcTriple> {
        let n = self.rate.cc_count();
        let mut out = Vec::with_capacity(n);
        let double = self.rate.is_double_rate();
        for (f, cc_type) in [(0, CcType::Field1), (1, CcType::Field2)] {
            let serve = !double || self.frame % 2 == f as u64;
            if serve {
                let pair = self.field_pair(f).unwrap_or(NULL_PAIR);
                out.push(CcTriple::new(cc_type, pair));
            } else {
                out.push(CcTriple {
                    valid: false,
                    cc_type,
                    data: NULL_PAIR,
                });
            }
        }
        self.dtvcc(&mut out, n);
        self.frame += 1;
        out
    }

    /// Next pair for field `f` (0 or 1), switching between its two channels
    /// only at line boundaries.
    fn field_pair(&mut self, f: usize) -> Option<[u8; 2]> {
        let chans = [2 * f, 2 * f + 1];
        let owner = self.field_owner[f];
        let other = if owner == chans[0] {
            chans[1]
        } else {
            chans[0]
        };
        let busy = |e: &Option<Cc608Encoder>| e.as_ref().is_some_and(|e| e.pending() > 0);
        let owner_mid_line = self.cc608[owner].as_ref().is_some_and(|e| !e.at_boundary());
        let pick = if owner_mid_line || (busy(&self.cc608[owner]) && !busy(&self.cc608[other])) {
            owner
        } else if busy(&self.cc608[other]) {
            other
        } else {
            return None;
        };
        self.field_owner[f] = pick;
        self.cc608[pick].as_mut().and_then(|e| e.next_pair())
    }

    /// Appends one DTVCC packet (if any service has data) and padding up to `n`.
    fn dtvcc(&mut self, out: &mut Vec<CcTriple>, n: usize) {
        let slots = n.saturating_sub(out.len());
        let max_len = (slots * 2).min(128);
        let mut packet = vec![0u8];
        let count = self.cc708.len();
        for k in 0..count {
            let i = (self.next_service + k) % count;
            let room = max_len.saturating_sub(packet.len() + 1).min(31);
            if room == 0 {
                break;
            }
            let Some(enc) = self.cc708.get_mut(i) else {
                break;
            };
            let block = enc.take_block(room);
            if !block.is_empty() {
                packet.push((enc.service() << 5) | block.len() as u8);
                packet.extend(block);
            }
        }
        if count > 0 {
            self.next_service = (self.next_service + 1) % count;
        }
        if packet.len() > 1 {
            if packet.len() % 2 == 1 {
                packet.push(0); // null service block header ends the packet
            }
            let size_code = (packet.len() / 2) as u8 & 0x3F; // 128 bytes -> 0
            packet[0] = (self.sequence << 6) | size_code;
            self.sequence = (self.sequence + 1) % 4;
            for (k, pair) in packet.chunks(2).enumerate() {
                let t = if k == 0 {
                    CcType::DtvccStart
                } else {
                    CcType::DtvccData
                };
                out.push(CcTriple::new(
                    t,
                    [pair[0], pair.get(1).copied().unwrap_or(0)],
                ));
            }
        }
        while out.len() < n {
            out.push(CcTriple {
                valid: false,
                cc_type: CcType::DtvccData,
                data: [0, 0],
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cea608::Mode608;

    #[test]
    fn cc_counts() {
        for (r, n) in [
            (FrameRate::Fps29_97, 20),
            (FrameRate::Fps30, 20),
            (FrameRate::Fps59_94, 10),
            (FrameRate::Fps60, 10),
            (FrameRate::Fps25, 24),
        ] {
            let mut m = CcMux::new(r);
            m.add_608(Cc608Encoder::new(Channel::Cc1));
            m.add_708(Cc708Encoder::new(1).unwrap_or_else(|| unreachable!()));
            m.push_text_608(Channel::Cc1, "HELLO");
            m.push_text_708(1, "Hello world this is a long caption line");
            for _ in 0..50 {
                assert_eq!(m.next_frame().len(), n);
            }
        }
        assert_eq!(
            FrameRate::from_fps(30000.0 / 1001.0),
            Some(FrameRate::Fps29_97)
        );
        assert_eq!(FrameRate::from_fps(59.9), Some(FrameRate::Fps59_94));
        assert_eq!(FrameRate::from_fps(f64::NAN), None);
        assert_eq!(FrameRate::from_fps(120.0), None);
    }

    #[test]
    fn idle_frame_is_null_and_padding() {
        let mut m = CcMux::new(FrameRate::Fps30);
        let t = m.next_frame();
        assert_eq!(t[0].to_bytes(), [0xFC, 0x80, 0x80]);
        assert_eq!(t[1].to_bytes(), [0xFD, 0x80, 0x80]);
        assert!(t[2..].iter().all(|x| x.to_bytes() == [0xFA, 0, 0]));
    }

    #[test]
    fn double_rate_alternates_fields() {
        let mut m = CcMux::new(FrameRate::Fps60);
        m.add_608(Cc608Encoder::new(Channel::Cc1));
        m.push_text_608(Channel::Cc1, "AB");
        let a = m.next_frame();
        let b = m.next_frame();
        assert!(a[0].valid && !a[1].valid);
        assert!(!b[0].valid && b[1].valid);
        assert_eq!(a[0].data, [0x94, 0x26]); // RU3 on field 1
        assert_eq!(b[1].data, NULL_PAIR);
    }

    #[test]
    fn same_field_channels_interleave_by_line() {
        let mut m = CcMux::new(FrameRate::Fps30);
        m.add_608(Cc608Encoder::with_mode(Channel::Cc1, Mode608::RollUp(2)));
        m.add_608(Cc608Encoder::with_mode(Channel::Cc2, Mode608::RollUp(2)));
        m.push_text_608(Channel::Cc1, "AAAA\nBBBB");
        m.push_text_608(Channel::Cc2, "CCCC");
        let f1: Vec<[u8; 2]> = (0..40)
            .map(|_| m.next_frame()[0].data)
            .map(|p| [p[0] & 0x7F, p[1] & 0x7F])
            .collect();
        // Both busy at a boundary: the field goes to the other channel (CC2,
        // control codes with the 0x08 bit), then back to CC1 for its two lines.
        let ru: Vec<u8> = f1.iter().filter(|p| p[1] == 0x25).map(|p| p[0]).collect();
        assert_eq!(ru, [0x1C, 0x1C, 0x14, 0x14, 0x14, 0x14]);
        assert!(m.is_idle());
    }

    #[test]
    fn dtvcc_packet_structure() {
        let mut m = CcMux::new(FrameRate::Fps30);
        m.add_708(Cc708Encoder::new(3).unwrap_or_else(|| unreachable!()));
        m.push_text_708(3, "Hi");
        let t = m.next_frame();
        assert_eq!(t[2].cc_type, CcType::DtvccStart);
        let bytes: Vec<u8> = t[2..]
            .iter()
            .filter(|x| x.valid)
            .flat_map(|x| x.data)
            .collect();
        let size = usize::from(bytes[0] & 0x3F) * 2;
        assert_eq!(bytes.len(), size);
        assert_eq!(bytes[1] >> 5, 3); // service 3
        let block = usize::from(bytes[1] & 0x1F);
        assert!(block <= 31 && 2 + block <= size);
        assert_eq!(bytes[2], 0x98); // DefineWindow 0 first
    }

    #[test]
    fn many_services_all_drain() {
        let mut m = CcMux::new(FrameRate::Fps59_94);
        for s in 1..=6 {
            m.add_708(Cc708Encoder::new(s).unwrap_or_else(|| unreachable!()));
            m.push_text_708(
                s,
                "Some text for every service, long enough to wrap onto two lines.",
            );
        }
        let mut seq = Vec::new();
        for _ in 0..500 {
            let t = m.next_frame();
            if let Some(s) = t.iter().find(|x| x.cc_type == CcType::DtvccStart) {
                seq.push(s.data[0] >> 6);
            }
        }
        assert!(m.is_idle());
        assert!(seq.windows(2).all(|w| w[1] == (w[0] + 1) % 4));
    }
}
