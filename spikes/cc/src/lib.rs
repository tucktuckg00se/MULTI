//! Caption bytes for MULTI spikes: CEA-608/708 `cc_data` triples, ATSC A/53
//! packing, and H.264/HEVC SEI NAL units.
//!
//! Phase 0 provides the shared interface plus a fixed caption fixture so the
//! pipeline spikes (S1, S2) can inject captions before the real encoder (S3)
//! exists. S3 replaces [`Cc608Encoder`]'s stub and adds CEA-708.

/// Which caption stream a `cc_data` triple belongs to (A/53 `cc_type`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CcType {
    /// CEA-608 field 1 (CC1, CC2).
    Field1 = 0,
    /// CEA-608 field 2 (CC3, CC4).
    Field2 = 1,
    /// CEA-708 DTVCC packet data.
    DtvccData = 2,
    /// CEA-708 DTVCC packet start.
    DtvccStart = 3,
}

/// One `cc_data` triple: validity, type and two data bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CcTriple {
    pub valid: bool,
    pub cc_type: CcType,
    pub data: [u8; 2],
}

impl CcTriple {
    pub fn new(cc_type: CcType, data: [u8; 2]) -> Self {
        Self { valid: true, cc_type, data }
    }

    /// Filler that decoders ignore: 608 null pair (0x80 0x80) with parity already set.
    pub fn null608(cc_type: CcType) -> Self {
        Self::new(cc_type, [0x80, 0x80])
    }

    /// The three bytes as they appear in `cc_data`: marker bits `11111`, `cc_valid`, `cc_type`.
    pub fn to_bytes(self) -> [u8; 3] {
        [0xF8 | (u8::from(self.valid) << 2) | self.cc_type as u8, self.data[0], self.data[1]]
    }
}

/// CEA-608 caption channel.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Channel {
    Cc1,
    Cc2,
    Cc3,
    Cc4,
}

/// Sets bit 7 so the byte has odd parity, as CEA-608 requires.
pub fn odd_parity(b: u8) -> u8 {
    let b = b & 0x7F;
    if b.count_ones().is_multiple_of(2) { b | 0x80 } else { b }
}

/// CEA-608 encoder for one channel. **Stub: S3 implements this.**
///
/// Contract: text is queued with [`push_text`](Self::push_text); every video frame
/// the caller asks for that frame's byte pair with [`next_pair`](Self::next_pair),
/// which returns `None` when idle. Pairs already carry parity.
pub struct Cc608Encoder {
    pub channel: Channel,
}

impl Cc608Encoder {
    pub fn new(channel: Channel) -> Self {
        Self { channel }
    }

    pub fn push_text(&mut self, _text: &str) {}

    pub fn next_pair(&mut self) -> Option<[u8; 2]> {
        None
    }
}

/// ATSC A/53 `user_data_registered_itu_t_t35` payload carrying `cc_data`,
/// laid out as FFmpeg writes it (country 0xB5, provider 0x0031, "GA94", type 3).
pub fn a53_payload(triples: &[CcTriple]) -> Vec<u8> {
    let count = triples.len().min(31);
    let mut p = Vec::with_capacity(11 + count * 3);
    p.extend_from_slice(&[0xB5, 0x00, 0x31, b'G', b'A', b'9', b'4', 0x03]);
    p.push(0x40 | count as u8); // process_cc_data_flag + cc_count
    p.push(0xFF); // em_data
    for t in &triples[..count] {
        p.extend_from_slice(&t.to_bytes());
    }
    p.push(0xFF); // marker_bits
    p
}

/// H.264 SEI NAL unit (type 6) carrying the triples. No start code; emulation
/// prevention applied.
pub fn h264_sei_nal(triples: &[CcTriple]) -> Vec<u8> {
    let mut nal = vec![0x06];
    nal.extend(escape(&sei_rbsp(triples)));
    nal
}

/// HEVC prefix SEI NAL unit (type 39, layer 0, temporal id 0) carrying the
/// triples. No start code; emulation prevention applied.
pub fn hevc_sei_nal(triples: &[CcTriple]) -> Vec<u8> {
    let mut nal = vec![39 << 1, 0x01];
    nal.extend(escape(&sei_rbsp(triples)));
    nal
}

fn sei_rbsp(triples: &[CcTriple]) -> Vec<u8> {
    let payload = a53_payload(triples);
    let mut rbsp = vec![4]; // payload_type: user_data_registered_itu_t_t35
    let mut size = payload.len();
    while size >= 255 {
        rbsp.push(0xFF);
        size -= 255;
    }
    rbsp.push(size as u8);
    rbsp.extend(payload);
    rbsp.push(0x80); // rbsp_trailing_bits
    rbsp
}

/// Inserts emulation-prevention bytes (0x03) so the payload never contains a start code.
fn escape(rbsp: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(rbsp.len() + rbsp.len() / 64);
    let mut zeros = 0;
    for &b in rbsp {
        if zeros >= 2 && b <= 3 {
            out.push(0x03);
            zeros = 0;
        }
        out.push(b);
        zeros = if b == 0 { zeros + 1 } else { 0 };
    }
    out
}

/// Fixed CC1 roll-up captions for testing insertion before S3 lands: one line
/// every 2 seconds at 30 fps, cycling through [`FIXTURE_LINES`]. Returns this
/// frame's triples (a field-1 pair plus field-2 filler).
pub fn fixture_triples(frame: u64) -> Vec<CcTriple> {
    const FRAMES_PER_LINE: u64 = 60;
    let line = (frame / FRAMES_PER_LINE) as usize % FIXTURE_LINES.len();
    let pairs = fixture_line_pairs(FIXTURE_LINES[line]);
    let at = (frame % FRAMES_PER_LINE) as usize;
    let f1 = pairs.get(at).copied().unwrap_or([0x80, 0x80]);
    vec![CcTriple::new(CcType::Field1, f1), CcTriple::null608(CcType::Field2)]
}

/// The lines [`fixture_triples`] sends, in order.
pub const FIXTURE_LINES: [&str; 3] = ["HELLO FROM MULTI", "CAPTION FIXTURE LINE TWO", "ROLL UP TEST 123"];

fn fixture_line_pairs(text: &str) -> Vec<[u8; 2]> {
    // CC1 control codes, each sent twice as 608 recommends: RU2 = 14 25, CR = 14 2D.
    let ctrl = |a: u8, b: u8| [odd_parity(a), odd_parity(b)];
    let mut pairs = vec![ctrl(0x14, 0x25), ctrl(0x14, 0x25), ctrl(0x14, 0x2D), ctrl(0x14, 0x2D)];
    let bytes: Vec<u8> = text.bytes().filter(|b| (0x20..0x7F).contains(b)).collect();
    for chunk in bytes.chunks(2) {
        let second = chunk.get(1).copied().unwrap_or(0);
        pairs.push([odd_parity(chunk[0]), odd_parity(second)]);
    }
    pairs
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parity_is_odd() {
        for b in 0..=0x7F_u8 {
            assert_eq!(odd_parity(b).count_ones() % 2, 1);
        }
        assert_eq!(odd_parity(0x00), 0x80);
        assert_eq!(odd_parity(0x14), 0x94);
    }

    #[test]
    fn triple_header_byte() {
        assert_eq!(CcTriple::new(CcType::Field1, [0x94, 0x25]).to_bytes(), [0xFC, 0x94, 0x25]);
        assert_eq!(CcTriple::null608(CcType::Field2).to_bytes(), [0xFD, 0x80, 0x80]);
    }

    #[test]
    fn a53_layout_matches_ffmpeg() {
        let p = a53_payload(&[CcTriple::new(CcType::Field1, [0x94, 0x25])]);
        assert_eq!(p, [0xB5, 0x00, 0x31, b'G', b'A', b'9', b'4', 0x03, 0x41, 0xFF, 0xFC, 0x94, 0x25, 0xFF]);
    }

    #[test]
    fn emulation_prevention() {
        assert_eq!(escape(&[0, 0, 1, 0, 0, 0, 0, 0, 4]), [0, 0, 3, 1, 0, 0, 3, 0, 0, 3, 0, 4]);
    }

    #[test]
    fn sei_headers() {
        let t = fixture_triples(0);
        assert_eq!(h264_sei_nal(&t)[..3], [0x06, 0x04, 17]);
        assert_eq!(hevc_sei_nal(&t)[..4], [0x4E, 0x01, 0x04, 17]);
    }

    #[test]
    fn fixture_idles_after_line() {
        assert_eq!(fixture_triples(59)[0].data, [0x80, 0x80]);
        assert_eq!(fixture_triples(0)[0].data, [0x94, 0x25]);
    }
}
