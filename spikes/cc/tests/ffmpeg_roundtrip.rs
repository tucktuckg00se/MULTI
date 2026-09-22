//! CEA-608 round trip through FFmpeg: insert SEI into x264/x265 output, remux
//! to TS, decode with FFmpeg's `ccaption` decoder (lavfi `movie=...[out+subcc]`).
//!
//! Needs the `ffmpeg` CLI with libx264/libx265; tests skip (pass) without it.

mod common;

use cc::annexb::VideoCodec;
use cc::{Cc608Encoder, CcMux, Channel, FrameRate, Mode608};
use common::*;

/// Pushes each line on `ch` every `every` frames, then clears (EDM) so the
/// decoder flushes the last caption.
fn script(ch: Channel, lines: &[&str], every: u64) -> Vec<(u64, Action)> {
    let mut s: Vec<(u64, Action)> = Vec::new();
    for (i, l) in lines.iter().enumerate() {
        let l = l.to_string();
        s.push((
            i as u64 * every + 1,
            Box::new(move |m: &mut CcMux| {
                m.push_text_608(ch, &l);
            }),
        ));
    }
    s.push((
        lines.len() as u64 * every + 1,
        Box::new(move |m: &mut CcMux| {
            if let Some(e) = m.cc608_mut(ch) {
                e.clear();
            }
        }),
    ));
    s
}

fn roundtrip(
    name: &str,
    codec: VideoCodec,
    rate: FrameRate,
    ch: Channel,
    mode: Mode608,
    lines: &[&str],
) -> String {
    let dir = work_dir(name);
    let secs = (lines.len() as u32 * 2 + 2).max(4);
    let src = make_source(&dir, codec, rate, secs);
    let mut mux = CcMux::new(rate);
    mux.add_608(Cc608Encoder::with_mode(ch, mode));
    let every = match rate {
        FrameRate::Fps50 | FrameRate::Fps59_94 | FrameRate::Fps60 => 120,
        _ => 60,
    };
    let (ts, frames) = caption_stream(&dir, &src, codec, mux, &script(ch, lines, every));
    assert!(frames >= u64::from(secs) * 20, "only {frames} frames");
    let field = match ch {
        Channel::Cc1 | Channel::Cc2 => "first",
        _ => "second",
    };
    let srt = ffmpeg_608(&ts, field);
    std::fs::write(dir.join("decoded.srt"), &srt).ok();
    srt_text(&srt)
}

fn assert_all(text: &str, lines: &[&str]) {
    for l in lines {
        assert!(text.contains(l), "missing {l:?} in decoded {text:?}");
    }
}

const EN: [&str; 4] = [
    "HELLO FROM MULTI",
    "Live captions, no re-encode.",
    "This line is long enough that it must wrap onto a second row.",
    "Numbers 0123456789 and (brackets) [ok]",
];

#[test]
fn h264_rollup_cc1_30fps() {
    if !ffmpeg_available() {
        return;
    }
    let text = roundtrip(
        "h264-ru3-cc1",
        VideoCodec::H264,
        FrameRate::Fps30,
        Channel::Cc1,
        Mode608::RollUp(3),
        &EN,
    );
    assert_all(
        &text,
        &[
            "HELLO FROM MULTI",
            "Live captions, no re-encode.",
            "Numbers 0123456789 and (brackets) [ok]",
        ],
    );
    assert!(
        text.contains("This line is long enough that it")
            && text.contains("must wrap onto a second row."),
        "{text}"
    );
}

#[test]
fn hevc_rollup_cc1_30fps() {
    if !ffmpeg_available() {
        return;
    }
    let text = roundtrip(
        "hevc-ru2-cc1",
        VideoCodec::Hevc,
        FrameRate::Fps30,
        Channel::Cc1,
        Mode608::RollUp(2),
        &EN[..2],
    );
    assert_all(&text, &EN[..2]);
}

#[test]
fn h264_popon_29_97() {
    if !ffmpeg_available() {
        return;
    }
    let lines = ["POP-ON CAPTION ONE", "Pop-on caption two"];
    let text = roundtrip(
        "h264-popon",
        VideoCodec::H264,
        FrameRate::Fps29_97,
        Channel::Cc1,
        Mode608::PopOn,
        &lines,
    );
    assert_all(&text, &lines);
}

#[test]
fn h264_painton_25() {
    if !ffmpeg_available() {
        return;
    }
    let lines = ["PAINT-ON CAPTION", "Second paint-on"];
    let text = roundtrip(
        "h264-painton",
        VideoCodec::H264,
        FrameRate::Fps25,
        Channel::Cc1,
        Mode608::PaintOn,
        &lines,
    );
    assert_all(&text, &lines);
}

#[test]
fn h264_rollup_59_94_alternating_fields() {
    if !ffmpeg_available() {
        return;
    }
    let lines = ["SIXTY FPS LINE ONE", "Sixty fps line two"];
    let text = roundtrip(
        "h264-5994",
        VideoCodec::H264,
        FrameRate::Fps59_94,
        Channel::Cc1,
        Mode608::RollUp(2),
        &lines,
    );
    assert_all(&text, &lines);
}

#[test]
fn h264_cc3_field2() {
    if !ffmpeg_available() {
        return;
    }
    let lines = ["FIELD TWO CC3 TEXT", "Second line on CC3"];
    let text = roundtrip(
        "h264-cc3",
        VideoCodec::H264,
        FrameRate::Fps30,
        Channel::Cc3,
        Mode608::RollUp(2),
        &lines,
    );
    assert_all(&text, &lines);
}

#[test]
fn h264_accented_languages() {
    if !ffmpeg_available() {
        return;
    }
    let lines = [
        "¿Qué tal? Señor Muñoz, ¡olé!",
        "Ça va très bien, à bientôt.",
        "Grüße aus München: Äpfel, Öl.",
        "Não, a informação é pública.",
        "Élève, Noël, naïve, où, Übung",
    ];
    let text = roundtrip(
        "h264-accents",
        VideoCodec::H264,
        FrameRate::Fps30,
        Channel::Cc1,
        Mode608::RollUp(3),
        &lines,
    );
    assert_all(&text, &lines);
}

#[test]
fn h264_repeated_special_and_ascii_remaps() {
    if !ffmpeg_available() {
        return;
    }
    // "èè"/"àà" are back-to-back identical special-char codes (separated by a
    // null pair); * _ ~ | { } \ ^ are remapped through the extended sets.
    let lines = ["Très èè àà ôô ÄÄ", "a*b_c~d|e{f}g\\h^i"];
    let text = roundtrip(
        "h264-repeats",
        VideoCodec::H264,
        FrameRate::Fps30,
        Channel::Cc1,
        Mode608::RollUp(2),
        &lines,
    );
    assert_all(&text, &lines);
}
