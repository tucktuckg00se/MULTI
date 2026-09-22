//! Prints the bytes the encoders produce, for eyeballing and for comparison
//! with other encoders (libcaption).
//!
//! ```text
//! cargo run -p cc --example cc_dump -- "Ça va? Grüße ÄÄ èè"
//! ```

use cc::{
    Cc608Encoder, Cc708Encoder, CcMux, Channel, FrameRate, Mode608, h264_sei_nal, hevc_sei_nal,
};

fn hex(b: &[u8]) -> String {
    b.iter()
        .map(|x| format!("{x:02x}"))
        .collect::<Vec<_>>()
        .join(" ")
}

fn main() {
    let text = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "HELLO".to_string());
    println!("text: {text:?}");
    for (name, mode) in [
        ("pop-on", Mode608::PopOn),
        ("roll-up 2", Mode608::RollUp(2)),
        ("paint-on", Mode608::PaintOn),
    ] {
        let mut e = Cc608Encoder::with_mode(Channel::Cc1, mode);
        e.push_text(&text);
        let pairs: Vec<String> = std::iter::from_fn(|| e.next_pair())
            .map(|p| format!("{:02x}{:02x}", p[0], p[1]))
            .collect();
        println!(
            "608 CC1 {name} ({} pairs = {} frames at 1 pair/frame): {}",
            pairs.len(),
            pairs.len(),
            pairs.join(" ")
        );
    }

    let mut mux = CcMux::new(FrameRate::Fps29_97);
    mux.add_608(Cc608Encoder::with_mode(Channel::Cc1, Mode608::RollUp(2)));
    if let Some(e) = Cc708Encoder::new(1) {
        mux.add_708(e);
    }
    mux.push_text_608(Channel::Cc1, &text);
    mux.push_text_708(1, &text);
    for f in 0..3 {
        let t = mux.next_frame();
        let cc: Vec<u8> = t.iter().flat_map(|x| x.to_bytes()).collect();
        println!("\nframe {f} cc_data ({} triples): {}", t.len(), hex(&cc));
        println!(
            "frame {f} H.264 SEI NAL: 00 00 00 01 {}",
            hex(&h264_sei_nal(&t))
        );
        println!(
            "frame {f} HEVC SEI NAL:  00 00 00 01 {}",
            hex(&hevc_sei_nal(&t))
        );
    }
}
