//! Counters updated from streaming threads, and the snapshot handed to
//! callers ([`Stats`]), e.g. for the periodic stats log and the web GUI.

use serde::Serialize;
use std::sync::atomic::{AtomicU64, Ordering};

#[derive(Default)]
pub(crate) struct Counters {
    pub frames_in: AtomicU64,
    pub frames_out: AtomicU64,
    pub audio_in: AtomicU64,
    pub sessions: AtomicU64,
    pub input_restarts: AtomicU64,
    pub input_errors: AtomicU64,
    pub output_pipeline_errors: AtomicU64,
    pub bridge_drops: AtomicU64,
    pub caption_frames: AtomicU64,
    pub caption_errors: AtomicU64,
    pub audio_chunks: AtomicU64,
    pub audio_drops: AtomicU64,
}

pub(crate) fn get(a: &AtomicU64) -> u64 {
    a.load(Ordering::Relaxed)
}

pub(crate) fn inc(a: &AtomicU64) {
    a.fetch_add(1, Ordering::Relaxed);
}

/// Per-lane caption counters, shared between the caption stage and callers.
#[derive(Default)]
pub(crate) struct LaneCounters {
    pub pushed: AtomicU64,
    pub dropped: AtomicU64,
    pub queued: AtomicU64,
}

/// A point-in-time view of the media pipeline.
#[derive(Clone, Debug, Default, Serialize)]
pub struct Stats {
    /// Video frames from the input demuxer.
    pub frames_in: u64,
    /// Video frames into the output muxer.
    pub frames_out: u64,
    /// Audio buffers forwarded to the output.
    pub audio_in: u64,
    /// Input timelines seen (first start, source restarts, PTS jumps).
    pub sessions: u64,
    /// Times the input pipeline was rebuilt (error, EOS, silence).
    pub input_restarts: u64,
    pub input_errors: u64,
    /// Errors in the shared output pipeline (it is rebuilt after each).
    pub output_pipeline_errors: u64,
    /// Buffers the bridge dropped (no timestamp, audio before video, no output yet).
    pub bridge_drops: u64,
    /// Video frames that carried caption data.
    pub caption_frames: u64,
    /// Caption encoder failures (the encoder is rebuilt after each).
    pub caption_errors: u64,
    /// PCM chunks handed to the audio callback.
    pub audio_chunks: u64,
    /// Audio dropped before the tap decoder because it fell behind.
    pub audio_drops: u64,
    /// Data arrived from the input within the watchdog window.
    pub input_live: bool,
    pub outputs: Vec<OutputStats>,
    pub lanes: Vec<LaneStats>,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct OutputStats {
    /// The output URL with secrets removed.
    pub url: String,
    pub running: bool,
    /// Errors so far (start failures and runtime errors).
    pub errors: u64,
    /// Successful (re)starts.
    pub starts: u64,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct LaneStats {
    pub lang: String,
    /// Text pieces handed to the caption encoder.
    pub pushed: u64,
    /// Text pieces dropped (backlog cap, full queue).
    pub dropped: u64,
    /// Text pieces waiting for the encoder.
    pub queued: u64,
}
