//! Synthetic ASR load: Whisper large-v3-turbo (CTranslate2, fp16, CUDA)
//! transcribing 30 s windows of real speech back to back, i.e. a GPU that is
//! never idle on the ASR side. This is harsher than streaming ASR, which
//! decodes a few hundred ms of work per second of audio.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::{Result, bail};
use ct2rs::{ComputeType, Config, Device, Whisper, WhisperOptions};

#[derive(clap::Args, Debug)]
pub struct AsrArgs {
    /// CTranslate2 Whisper model directory.
    #[arg(long)]
    pub model: PathBuf,
    /// 16 kHz mono s16le WAV.
    #[arg(long)]
    pub wav: PathBuf,
    /// Stop after this many minutes.
    #[arg(long, default_value_t = 60.0)]
    pub minutes: f64,
    /// Window length in seconds.
    #[arg(long, default_value_t = 30.0)]
    pub window_s: f64,
    /// Sleep between windows (0 = back to back).
    #[arg(long, default_value_t = 0)]
    pub gap_ms: u64,
    #[arg(long, default_value_t = 5)]
    pub beam: usize,
}

fn read_wav(p: &PathBuf) -> Result<Vec<f32>> {
    let b = std::fs::read(p)?;
    if b.len() < 44 || &b[0..4] != b"RIFF" {
        bail!("not a WAV file");
    }
    // Walk chunks to find "data".
    let mut i = 12;
    while i + 8 <= b.len() {
        let id = &b[i..i + 4];
        let len = u32::from_le_bytes([b[i + 4], b[i + 5], b[i + 6], b[i + 7]]) as usize;
        if id == b"data" {
            let end = (i + 8 + len).min(b.len());
            return Ok(b[i + 8..end]
                .chunks_exact(2)
                .map(|c| i16::from_le_bytes([c[0], c[1]]) as f32 / 32768.0)
                .collect());
        }
        i += 8 + len + (len & 1);
    }
    bail!("no data chunk")
}

pub fn run(a: AsrArgs) -> Result<()> {
    let audio = read_wav(&a.wav)?;
    let w = Whisper::new(
        &a.model,
        Config {
            device: Device::CUDA,
            compute_type: ComputeType::FLOAT16,
            ..Default::default()
        },
    )?;
    let opts = WhisperOptions {
        beam_size: a.beam,
        ..Default::default()
    };
    let win = (a.window_s * 16000.0) as usize;
    let start = Instant::now();
    let until = Duration::from_secs_f64(a.minutes * 60.0);
    let mut off = 0usize;
    let mut n = 0u64;
    let mut busy = Duration::ZERO;
    let mut last = Instant::now();
    while start.elapsed() < until {
        if off + win > audio.len() {
            off = 0;
        }
        let t = Instant::now();
        let text = w.generate(&audio[off..off + win], Some("en"), false, &opts)?;
        busy += t.elapsed();
        n += 1;
        off += win;
        if n == 1 {
            eprintln!("asrload first window: {:?}", text.first().map(|s| &s[..s.len().min(80)]));
        }
        if last.elapsed() >= Duration::from_secs(10) {
            let el = start.elapsed().as_secs_f64();
            eprintln!(
                "asrload t={el:.0}s windows={n} ms/window={:.0} audio_x_realtime={:.1}",
                busy.as_secs_f64() * 1e3 / n as f64,
                n as f64 * a.window_s / el
            );
            last = Instant::now();
        }
        if a.gap_ms > 0 {
            std::thread::sleep(Duration::from_millis(a.gap_ms));
        }
    }
    Ok(())
}
