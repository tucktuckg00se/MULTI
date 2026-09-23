//! Annex-B access-unit surgery: find the first VCL NAL and splice a SEI NAL in
//! front of it. Pure functions, no FFmpeg, so they are unit-tested.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VideoCodec {
    H264,
    Hevc,
}

impl VideoCodec {
    fn is_vcl(self, header: u8) -> bool {
        match self {
            // 1..=5: coded slices (non-IDR, partitions A-C, IDR).
            VideoCodec::H264 => (1..=5).contains(&(header & 0x1F)),
            // 0..=31: VCL NAL unit types (TRAIL..CRA, reserved VCL).
            VideoCodec::Hevc => (header >> 1) & 0x3F <= 31,
        }
    }
}

/// Byte offset of the start code (3- or 4-byte) that begins the first VCL NAL,
/// or `None` if the buffer is not Annex-B or has no VCL NAL.
pub fn first_vcl_offset(data: &[u8], codec: VideoCodec) -> Option<usize> {
    let mut i = 0;
    while i + 3 < data.len() {
        if data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1 {
            let header = data[i + 3];
            if codec.is_vcl(header) {
                let start = if i > 0 && data[i - 1] == 0 { i - 1 } else { i };
                return Some(start);
            }
            i += 3;
        } else {
            i += 1;
        }
    }
    None
}

/// True if the access unit contains an IDR (H.264 type 5) or IRAP (HEVC
/// types 16..=23) slice. Needed when FFmpeg's parser is disabled
/// (`fflags=+noparse`), because the TS demuxer then leaves keyframe flags unset.
pub fn is_keyframe(data: &[u8], codec: VideoCodec) -> bool {
    let mut i = 0;
    while i + 3 < data.len() {
        if data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1 {
            let h = data[i + 3];
            match codec {
                VideoCodec::H264 if h & 0x1F == 5 => return true,
                VideoCodec::Hevc if (16..=23).contains(&((h >> 1) & 0x3F)) => return true,
                _ => {}
            }
            if codec.is_vcl(h) {
                return false; // first slice decides
            }
            i += 3;
        } else {
            i += 1;
        }
    }
    false
}

/// Checks the first NAL header against `codec`: `Some(true)` = it is the
/// other codec, `Some(false)` = it is `codec`, `None` = can't tell (some
/// headers are valid in both). H.264 headers are 1 byte; HEVC 2 bytes with
/// layer id 0 and temporal_id_plus1 != 0.
pub fn looks_like_other_codec(data: &[u8], codec: VideoCodec) -> Option<bool> {
    let i = (0..data.len().saturating_sub(3)).find(|&i| data[i..i + 3] == [0, 0, 1])?;
    let (&b0, &b1) = (data.get(i + 3)?, data.get(i + 4)?);
    let hevc_ok = b0 & 0x81 == 0 && (b1 & 0xF8) == 0 && (b1 & 7) != 0;
    let hevc_ps = hevc_ok && matches!((b0 >> 1) & 0x3F, 32..=35 | 39);
    let h264_ok = b0 & 0x80 == 0 && matches!(b0 & 0x1F, 1..=12 | 14 | 15 | 20);
    match codec {
        VideoCodec::H264 if hevc_ps => Some(true),
        VideoCodec::H264 if h264_ok && !hevc_ok => Some(false),
        VideoCodec::Hevc if h264_ok && !hevc_ok => Some(true),
        VideoCodec::Hevc if hevc_ok => Some(false),
        _ => None,
    }
}

/// Why an access unit was passed through without a caption SEI.
#[derive(Debug, PartialEq, Eq)]
pub enum Skip {
    /// Buffer does not start with an Annex-B start code.
    NotAnnexB,
    /// No VCL NAL found (e.g. a parameter-set-only packet).
    NoVcl,
}

/// Returns a new access unit with `00 00 00 01 + sei_nal` spliced in directly
/// before the first VCL NAL, i.e. after AUD/SPS/PPS/(VPS)/existing SEI.
pub fn insert_sei(au: &[u8], sei_nal: &[u8], codec: VideoCodec) -> Result<Vec<u8>, Skip> {
    let annex_b = au.starts_with(&[0, 0, 1]) || au.starts_with(&[0, 0, 0, 1]);
    if !annex_b {
        return Err(Skip::NotAnnexB);
    }
    let at = first_vcl_offset(au, codec).ok_or(Skip::NoVcl)?;
    let mut out = Vec::with_capacity(au.len() + sei_nal.len() + 4);
    out.extend_from_slice(&au[..at]);
    out.extend_from_slice(&[0, 0, 0, 1]);
    out.extend_from_slice(sei_nal);
    out.extend_from_slice(&au[at..]);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn h264_after_aud_sps_pps_sei() {
        // AUD(9) SPS(7) PPS(8) SEI(6) IDR(5)
        let au = [
            0, 0, 0, 1, 0x09, 0xF0, 0, 0, 0, 1, 0x67, 0xAA, 0, 0, 1, 0x68, 0xBB, 0, 0, 1, 0x06, 0xCC, 0, 0, 0, 1,
            0x65, 0xDD,
        ];
        let out = insert_sei(&au, &[0x06, 0x04, 0x99], VideoCodec::H264).unwrap();
        let at = 22; // the 4-byte start code of the IDR slice
        assert_eq!(&out[..at], &au[..at]);
        assert_eq!(&out[at..at + 7], &[0, 0, 0, 1, 0x06, 0x04, 0x99]);
        assert_eq!(&out[at + 7..], &au[at..]);
    }

    #[test]
    fn hevc_slice_detected() {
        // AUD(35) VPS(32) SPS(33) PPS(34) IDR_W_RADL(19)
        let au = [
            0, 0, 0, 1, 0x46, 0x01, 0x50, 0, 0, 1, 0x40, 0x01, 0, 0, 1, 0x42, 0x01, 0, 0, 1, 0x44, 0x01, 0, 0, 1,
            0x26, 0x01, 0xAF,
        ];
        assert_eq!(first_vcl_offset(&au, VideoCodec::Hevc), Some(22));
        assert!(is_keyframe(&au, VideoCodec::Hevc));
        assert!(!is_keyframe(&[0, 0, 1, 0x02, 0x01], VideoCodec::Hevc));
        assert!(is_keyframe(&[0, 0, 1, 0x09, 0xF0, 0, 0, 1, 0x65], VideoCodec::H264));
        assert!(!is_keyframe(&[0, 0, 1, 0x41, 0x9A], VideoCodec::H264));
        // A 3-byte start code preceded by a data byte is not widened.
        assert_eq!(first_vcl_offset(&[0, 0, 1, 0x26, 0x01], VideoCodec::Hevc), Some(0));
    }

    #[test]
    fn codec_mismatch() {
        let hevc_aud = [0, 0, 0, 1, 0x46, 0x01, 0x50];
        let h264_aud = [0, 0, 0, 1, 0x09, 0xF0];
        assert_eq!(looks_like_other_codec(&hevc_aud, VideoCodec::H264), Some(true));
        assert_eq!(looks_like_other_codec(&hevc_aud, VideoCodec::Hevc), Some(false));
        assert_eq!(looks_like_other_codec(&h264_aud, VideoCodec::Hevc), Some(true));
        assert_eq!(looks_like_other_codec(&h264_aud, VideoCodec::H264), Some(false));
        assert_eq!(looks_like_other_codec(&[0, 0, 1, 0x41, 0x9A], VideoCodec::H264), Some(false));
        assert_eq!(looks_like_other_codec(&[0, 0, 1, 0x02, 0x01], VideoCodec::Hevc), Some(false));
        // HEVC TRAIL_R slice header is also a valid H.264 header: undecided.
        assert_eq!(looks_like_other_codec(&[0, 0, 1, 0x02, 0x01], VideoCodec::H264), None);
    }

    #[test]
    fn skips() {
        assert_eq!(insert_sei(&[0x65, 1, 2], &[6], VideoCodec::H264), Err(Skip::NotAnnexB));
        assert_eq!(insert_sei(&[0, 0, 1, 0x67, 1], &[6], VideoCodec::H264), Err(Skip::NoVcl));
        assert_eq!(insert_sei(&[], &[6], VideoCodec::H264), Err(Skip::NotAnnexB));
    }
}
