//! Counters updated from streaming threads, and the snapshot handed to
//! callers ([`Stats`]), e.g. for the periodic stats log and the web GUI.

use serde::Serialize;
use std::sync::Mutex;
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

/// Level below which input audio counts as silent.
pub const SILENCE_DBFS: f32 = -60.0;

/// Samples per level window: 250 ms at 16 kHz.
const LEVEL_WINDOW: u64 = 4_000;

/// Floor reported for digital silence, so levels stay finite.
const FLOOR_DBFS: f32 = -100.0;

/// Converts an amplitude relative to full scale (0.0–1.0) to dBFS.
pub fn to_dbfs(ratio: f64) -> f32 {
    if ratio <= 0.0 {
        return FLOOR_DBFS;
    }
    ((20.0 * ratio.log10()) as f32).max(FLOOR_DBFS)
}

/// RMS and peak of a block of samples, in dBFS.
pub fn level_dbfs(samples: &[i16]) -> (f32, f32) {
    if samples.is_empty() {
        return (FLOOR_DBFS, FLOOR_DBFS);
    }
    let sum_sq: f64 = samples.iter().map(|&s| f64::from(s) * f64::from(s)).sum();
    let peak = samples
        .iter()
        .map(|&s| i32::from(s).unsigned_abs())
        .max()
        .unwrap_or(0);
    let rms = (sum_sq / samples.len() as f64).sqrt() / 32768.0;
    (to_dbfs(rms), to_dbfs(f64::from(peak) / 32768.0))
}

/// Input audio level over 250 ms windows, and when it was last above
/// [`SILENCE_DBFS`]. Updated from the audio tap thread.
#[derive(Default)]
pub(crate) struct LevelMeter {
    inner: Mutex<LevelState>,
}

#[derive(Default)]
struct LevelState {
    sum_sq: f64,
    peak: u32,
    n: u64,
    rms_dbfs: Option<f32>,
    peak_dbfs: Option<f32>,
    /// Wall-clock ns when audio was first seen or last above the silence level.
    last_sound_ns: Option<u64>,
}

impl LevelMeter {
    pub fn add(&self, samples: &[i16], now_ns: u64) {
        let Ok(mut st) = self.inner.lock() else {
            return;
        };
        st.last_sound_ns.get_or_insert(now_ns);
        for &s in samples {
            st.sum_sq += f64::from(s) * f64::from(s);
            st.peak = st.peak.max(i32::from(s).unsigned_abs());
            st.n += 1;
            if st.n >= LEVEL_WINDOW {
                let rms = to_dbfs((st.sum_sq / st.n as f64).sqrt() / 32768.0);
                st.rms_dbfs = Some(rms);
                st.peak_dbfs = Some(to_dbfs(f64::from(st.peak) / 32768.0));
                if rms > SILENCE_DBFS {
                    st.last_sound_ns = Some(now_ns);
                }
                st.sum_sq = 0.0;
                st.peak = 0;
                st.n = 0;
            }
        }
    }

    /// (RMS dBFS, peak dBFS, seconds since audio was last above the silence level).
    pub fn snapshot(&self, now_ns: u64) -> (Option<f32>, Option<f32>, Option<f64>) {
        let Ok(st) = self.inner.lock() else {
            return (None, None, None);
        };
        let silent_s = st
            .last_sound_ns
            .map(|t| now_ns.saturating_sub(t) as f64 / 1e9);
        (st.rms_dbfs, st.peak_dbfs, silent_s)
    }
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
    pub stale: AtomicU64,
    pub oldest_ms: AtomicU64,
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
    /// Input audio level over the last 250 ms, in dBFS (`None` before any audio).
    pub audio_rms_dbfs: Option<f32>,
    pub audio_peak_dbfs: Option<f32>,
    /// Seconds since the input audio was last above [`SILENCE_DBFS`].
    pub audio_silent_s: Option<f64>,
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
    /// Of `dropped`: text older than the age cap (6 s).
    pub stale: u64,
    /// Age of the oldest waiting text, ms (0 when none).
    pub oldest_ms: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn full_scale_and_silence() {
        let (rms, peak) = level_dbfs(&[i16::MAX, i16::MIN, i16::MAX, i16::MIN]);
        assert!(rms > -0.1 && peak > -0.1, "{rms} {peak}");
        assert_eq!(level_dbfs(&[0; 100]), (FLOOR_DBFS, FLOOR_DBFS));
        assert_eq!(level_dbfs(&[]), (FLOOR_DBFS, FLOOR_DBFS));
    }

    #[test]
    fn half_scale_is_about_minus_six() {
        let (rms, _) = level_dbfs(&[16384, -16384, 16384, -16384]);
        assert!((rms + 6.02).abs() < 0.05, "{rms}");
    }

    #[test]
    fn meter_tracks_silence_duration() {
        let m = LevelMeter::default();
        let s = 1_000_000_000u64;
        m.add(&vec![8000; 4000], s); // loud window at t = 1 s
        m.add(&vec![0; 4000], 5 * s); // silent window at t = 5 s
        let (rms, _, silent) = m.snapshot(12 * s);
        assert_eq!(rms, Some(FLOOR_DBFS));
        assert_eq!(silent, Some(11.0));
    }

    #[test]
    fn silence_from_the_start_counts_from_first_audio() {
        let m = LevelMeter::default();
        let s = 1_000_000_000u64;
        m.add(&vec![0; 4000], 2 * s);
        assert_eq!(m.snapshot(14 * s).2, Some(12.0));
    }
}
