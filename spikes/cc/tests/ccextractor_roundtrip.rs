//! Multi-track round trip through ccextractor: CC1–CC4 (608) and 708 services
//! 1–6 in one stream, each decoded separately.
//!
//! Ignored by default because it needs a ccextractor binary. Build it (see
//! docs/m0/findings/S3-caption-encoder.md) into
//! `~/.cache/multi-tools/ccx-build/ccextractor` or set `CCEXTRACTOR`, then:
//!
//! ```text
//! cargo test -p cc --test ccextractor_roundtrip -- --ignored --nocapture
//! ```

mod common;

use cc::annexb::VideoCodec;
use cc::{Cc608Encoder, Cc708Encoder, CcMux, Channel, FrameRate, Mode608};
use common::*;
use std::path::Path;
use std::process::Command;

const CC608: [(Channel, &str, [&str; 3]); 4] = [
    (
        Channel::Cc1,
        "cc1",
        [
            "English on CC1.",
            "Second English line.",
            "Third line, CC1.",
        ],
    ),
    (
        Channel::Cc2,
        "cc2",
        [
            "Español en CC2, ¿sí?",
            "Segunda línea: niño.",
            "Tercera: ¡adiós!",
        ],
    ),
    (
        Channel::Cc3,
        "cc3",
        [
            "Français sur CC3, très bien.",
            "Deuxième ligne : où ?",
            "Troisième, Noël.",
        ],
    ),
    (
        Channel::Cc4,
        "cc4",
        [
            "Deutsch auf CC4, Grüße.",
            "Zweite Zeile: Äpfel.",
            "Dritte Zeile, Straße.",
        ],
    ),
];

const CC708: [(u8, [&str; 3]); 6] = [
    (
        1,
        [
            "English on service 1.",
            "Second English line.",
            "Curly “quotes” and dash —",
        ],
    ),
    (
        2,
        [
            "Español en servicio 2.",
            "¿Qué tal? ¡Muy bien!",
            "Año, niño, corazón.",
        ],
    ),
    (
        3,
        [
            "Français, service 3.",
            "Œuvre, cœur, Noël.",
            "À bientôt, garçon.",
        ],
    ),
    (
        4,
        [
            "Deutsch, Dienst 4.",
            "Grüße aus München.",
            "Äpfel, Öl, Übung, Straße.",
        ],
    ),
    (
        5,
        [
            "Português, serviço 5.",
            "Informação pública.",
            "Não, São Paulo.",
        ],
    ),
    (
        6,
        [
            "Service six works too.",
            "Ellipsis… and ♪ music ♪",
            "Last line of service 6.",
        ],
    ),
];

fn build_mux(rate: FrameRate) -> (CcMux, Vec<(u64, Action)>) {
    let mut mux = CcMux::new(rate);
    for (ch, _, _) in CC608 {
        mux.add_608(Cc608Encoder::with_mode(ch, Mode608::RollUp(2)));
    }
    for (svc, _) in CC708 {
        if let Some(e) = Cc708Encoder::new(svc) {
            mux.add_708(e);
        }
    }
    let every = 60 * if rate.is_double_rate() { 2 } else { 1 };
    let mut script: Vec<(u64, Action)> = Vec::new();
    for i in 0..3 {
        script.push((
            1 + i as u64 * every,
            Box::new(move |m: &mut CcMux| {
                for (ch, _, lines) in CC608 {
                    m.push_text_608(ch, lines[i]);
                }
                for (svc, lines) in CC708 {
                    m.push_text_708(svc, lines[i]);
                }
            }),
        ));
    }
    script.push((
        1 + 3 * every,
        Box::new(|m: &mut CcMux| {
            for (ch, _, _) in CC608 {
                if let Some(e) = m.cc608_mut(ch) {
                    e.clear();
                }
            }
            for (svc, _) in CC708 {
                if let Some(e) = m.cc708_mut(svc) {
                    e.clear();
                }
            }
        }),
    ));
    (mux, script)
}

/// ccextractor writes 708 text as raw Latin-1 bytes, not UTF-8.
fn latin1(b: &[u8]) -> String {
    b.iter().map(|&c| char::from(c)).collect()
}

/// What ccextractor (0.96.5) writes for a line we encoded: G0/G1 as is; G2
/// characters as its internal byte (G2 code - 0x20, e.g. `…` -> 0x05); the
/// music note (G0 0x7F) as the two bytes of U+266C, `&l`; em dash -> our
/// fallback `-`. This checks that the exact G2 code arrived.
fn ccx_708_rendering(line: &str) -> String {
    line.chars()
        .map(|c| match c {
            '♪' => "&l".to_string(),
            '—' => "-".to_string(),
            '…' => "\u{05}".to_string(),
            '“' => "\u{13}".to_string(),
            '”' => "\u{14}".to_string(),
            'Œ' => "\u{0C}".to_string(),
            'œ' => "\u{1C}".to_string(),
            c => c.to_string(),
        })
        .collect()
}

fn ccx(bin: &Path, ts: &Path, out: &Path, extra: &[&str]) -> String {
    let o = Command::new(bin)
        .args(extra)
        .arg(ts)
        .arg("-o")
        .arg(out)
        .output()
        .expect("run ccextractor");
    let log = format!(
        "{}{}",
        String::from_utf8_lossy(&o.stdout),
        String::from_utf8_lossy(&o.stderr)
    );
    std::fs::write(out.with_extension("log"), &log).ok();
    std::fs::read_to_string(out).unwrap_or_default()
}

fn check(name: &str, codec: VideoCodec, rate: FrameRate) {
    let Some(bin) = ccextractor() else {
        panic!("ccextractor not found; set CCEXTRACTOR");
    };
    assert!(ffmpeg_available());
    let dir = work_dir(name);
    let src = make_source(&dir, codec, rate, 10);
    let (mux, script) = build_mux(rate);
    let (ts, _) = caption_stream(&dir, &src, codec, mux, &script);

    let mut failures = Vec::new();
    for (ch, tag, lines) in CC608 {
        let field = if ch.field() == cc::CcType::Field1 {
            "1"
        } else {
            "2"
        };
        let mut args = vec!["--output-field", field];
        if ch.is_second_in_field() {
            args.push("--cc2");
        }
        let srt = ccx(&bin, &ts, &dir.join(format!("{tag}.srt")), &args);
        let text = srt_text(&srt);
        println!("[{name}] {tag}: {text}");
        for l in lines {
            if !text.contains(l) {
                failures.push(format!("{tag}: missing {l:?}"));
            }
        }
    }
    // 708: one run, one output file per service.
    let base = dir.join("dtvcc.srt");
    ccx(&bin, &ts, &base, &["--service", "1,2,3,4,5,6"]);
    for (svc, lines) in CC708 {
        let path = dir.join(format!("dtvcc_{svc}.srt"));
        let srt = std::fs::read_dir(&dir)
            .ok()
            .into_iter()
            .flatten()
            .flatten()
            .find(|e| {
                let n = e.file_name().to_string_lossy().to_string();
                n.starts_with("dtvcc") && n.ends_with(&format!("svc0{svc}.srt"))
            })
            .and_then(|e| std::fs::read(e.path()).ok())
            .map(|b| latin1(&b))
            .unwrap_or_default();
        std::fs::write(&path, &srt).ok();
        let text = srt_text(&srt);
        println!("[{name}] svc{svc}: {text}");
        for l in lines {
            let want = ccx_708_rendering(l);
            if !text.contains(&want) {
                failures.push(format!("svc{svc}: missing {want:?} (from {l:?})"));
            }
        }
    }
    assert!(failures.is_empty(), "{failures:#?}");
}

#[test]
#[ignore = "needs ccextractor; see module docs"]
fn ccx_h264_all_tracks_30fps() {
    check("ccx-h264-30", VideoCodec::H264, FrameRate::Fps30);
}

#[test]
#[ignore = "needs ccextractor; see module docs"]
fn ccx_h264_all_tracks_59_94fps() {
    check("ccx-h264-5994", VideoCodec::H264, FrameRate::Fps59_94);
}

#[test]
#[ignore = "needs ccextractor; see module docs"]
fn ccx_hevc_all_tracks_29_97fps() {
    check("ccx-hevc-2997", VideoCodec::Hevc, FrameRate::Fps29_97);
}
