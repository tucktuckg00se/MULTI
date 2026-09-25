//! Minimal Annex-B elementary-stream helpers: split into NAL units and insert
//! a caption SEI into every access unit before its first VCL NAL.
//!
//! Test tooling for S3; S1 does the same thing on demuxed packets.

use crate::{CcTriple, h264_sei_nal, hevc_sei_nal};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VideoCodec {
    H264,
    Hevc,
}

/// NAL units (without start codes) of an Annex-B byte stream. Trailing zero
/// bytes before a start code are dropped.
pub fn split_nals(data: &[u8]) -> Vec<&[u8]> {
    let mut starts = Vec::new(); // index of first NAL byte
    let mut i = 0;
    while i + 3 <= data.len() {
        if data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1 {
            starts.push(i + 3);
            i += 3;
        } else {
            i += 1;
        }
    }
    let mut nals = Vec::with_capacity(starts.len());
    for (k, &s) in starts.iter().enumerate() {
        let mut end = starts.get(k + 1).map_or(data.len(), |&n| n - 3);
        while end > s && data[end - 1] == 0 {
            end -= 1;
        }
        if end > s {
            nals.push(&data[s..end]);
        }
    }
    nals
}

/// True if `nal` is the first slice of a new picture (H.264 `first_mb_in_slice
/// == 0`, HEVC `first_slice_segment_in_pic_flag`).
pub fn starts_picture(codec: VideoCodec, nal: &[u8]) -> bool {
    match codec {
        VideoCodec::H264 => {
            let t = nal.first().map_or(0, |b| b & 0x1F);
            (1..=5).contains(&t) && nal.get(1).is_some_and(|b| b & 0x80 != 0)
        }
        VideoCodec::Hevc => {
            let t = nal.first().map_or(64, |b| (b >> 1) & 0x3F);
            t < 32 && nal.get(2).is_some_and(|b| b & 0x80 != 0)
        }
    }
}

/// Copies `stream`, inserting a caption SEI (from `triples_for`, called with
/// the frame index in decode order) right before the first VCL NAL of every
/// picture. Returns the new stream and the number of pictures.
///
/// Decode order equals display order only without B-frames; with B-frames the
/// caller must map decode order to display order itself.
pub fn insert_cc_sei(
    stream: &[u8],
    codec: VideoCodec,
    mut triples_for: impl FnMut(u64) -> Vec<CcTriple>,
) -> (Vec<u8>, u64) {
    let mut out = Vec::with_capacity(stream.len() + stream.len() / 8);
    let mut frames = 0;
    for nal in split_nals(stream) {
        if starts_picture(codec, nal) {
            let triples = triples_for(frames);
            let sei = match codec {
                VideoCodec::H264 => h264_sei_nal(&triples),
                VideoCodec::Hevc => hevc_sei_nal(&triples),
            };
            out.extend_from_slice(&[0, 0, 0, 1]);
            out.extend(sei);
            frames += 1;
        }
        out.extend_from_slice(&[0, 0, 0, 1]);
        out.extend_from_slice(nal);
    }
    (out, frames)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_handles_3_and_4_byte_start_codes() {
        let s = [
            0, 0, 0, 1, 0x67, 1, 2, 0, 0, 1, 0x68, 3, 0, 0, 0, 0, 1, 0x65, 0x88,
        ];
        assert_eq!(
            split_nals(&s),
            [&[0x67, 1, 2][..], &[0x68, 3], &[0x65, 0x88]]
        );
        assert!(split_nals(&[]).is_empty());
        assert!(split_nals(&[0, 0, 1]).is_empty());
    }

    #[test]
    fn picture_starts() {
        assert!(starts_picture(VideoCodec::H264, &[0x65, 0x88]));
        assert!(!starts_picture(VideoCodec::H264, &[0x65, 0x08]));
        assert!(!starts_picture(VideoCodec::H264, &[0x67, 0x88]));
        assert!(starts_picture(VideoCodec::Hevc, &[0x26, 0x01, 0xAF]));
        assert!(!starts_picture(VideoCodec::Hevc, &[0x40, 0x01, 0x80]));
        assert!(!starts_picture(VideoCodec::Hevc, &[0x26]));
    }
}
