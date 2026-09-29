//! What every speech engine offers the worker loop, and how one is chosen.
//!
//! Two engines (M2-5):
//! - [`EngineKind::SherpaStreaming`]: Nemotron 3.5 Streaming through
//!   sherpa-onnx, native streaming, append-only output (`sherpa.rs`).
//! - [`EngineKind::Whisper`]: Whisper through whisper.cpp, re-transcribing a
//!   buffer and committing with LocalAgreement (`agreement.rs`,
//!   `whisper.rs`), behind a hallucination guard (`guard.rs`).
//!
//! Both sit behind Silero VAD (`vad.rs`) and report words with times in
//! seconds since the first sample pushed.

use anyhow::Result;
use clap::ValueEnum;

pub const SR: usize = 16_000;

/// A committed word; times are seconds since the first sample pushed.
#[derive(Clone, Debug, PartialEq)]
pub struct RawWord {
    pub text: String,
    pub t0: f64,
    pub t1: f64,
}

/// A speech recogniser fed 16 kHz mono audio (-1..1) as it arrives.
pub trait Engine {
    /// One decode on synthetic input so the first real one is not slow
    /// (CUDA kernels, allocator); leaves the engine reset.
    fn warm_up(&mut self) -> Result<()>;
    /// Feeds audio and returns newly committed words. Committed words are
    /// final: they are never revised.
    fn push(&mut self, audio: &[f32]) -> Result<Vec<RawWord>>;
    /// Flushes the open segment, committing every word.
    fn finish(&mut self) -> Result<Vec<RawWord>>;
}

/// The `--engine` choice; matches the catalogue's `engine` field.
#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum EngineKind {
    /// sherpa-onnx online transducer (Nemotron 3.5 Streaming): `--model` is
    /// the export's folder, `--chunk-ms` the chunk it was exported with.
    SherpaStreaming,
    /// whisper.cpp (ggml file) with LocalAgreement: `--model` is the `.bin`
    /// file, `--chunk-ms` the interval between passes.
    Whisper,
}

/// Chunk sizes the Nemotron streaming exports exist for.
pub const SHERPA_CHUNKS: [u32; 5] = [80, 160, 320, 560, 1120];

impl EngineKind {
    /// Checks `--chunk-ms` for this engine.
    pub fn check_chunk(self, chunk_ms: u32) -> Result<(), String> {
        match self {
            Self::SherpaStreaming if !SHERPA_CHUNKS.contains(&chunk_ms) => Err(format!(
                "--chunk-ms {chunk_ms}: a sherpa-streaming model needs the chunk it was exported with (80, 160, 320, 560 or 1120)"
            )),
            Self::Whisper if !(200..=3000).contains(&chunk_ms) => Err(format!(
                "--chunk-ms {chunk_ms}: the Whisper pass interval must be 200-3000 ms"
            )),
            _ => Ok(()),
        }
    }

    /// Whether this build can run the engine.
    pub fn available(self) -> bool {
        match self {
            Self::SherpaStreaming => true,
            Self::Whisper => cfg!(feature = "whisper"),
        }
    }
}
