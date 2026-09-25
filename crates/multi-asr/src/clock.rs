//! Maps the engine's sample count back to the incoming PCM timeline.
//!
//! Each PCM frame carries its `start_ms`. The engine only sees a continuous
//! sample stream, so we keep an anchor (sample index → ms) wherever the
//! timeline jumps (dropped frames, restarts upstream) and map word times
//! through the latest anchor at or before them.

use std::collections::VecDeque;

const SAMPLES_PER_MS: u64 = 16;
/// Anchors kept; words commit within seconds, so old ones are never needed.
const MAX_ANCHORS: usize = 1024;

#[derive(Default)]
pub struct Clock {
    anchors: VecDeque<(u64, u64)>,
    /// Samples fed so far.
    fed: u64,
}

impl Clock {
    /// Records a PCM frame of `len` samples starting at `start_ms`.
    pub fn frame(&mut self, start_ms: u64, len: usize) {
        let contiguous = self.anchors.back().is_some_and(|&(idx, ms)| {
            ms.saturating_add((self.fed - idx) / SAMPLES_PER_MS) == start_ms
        });
        if !contiguous {
            self.anchors.push_back((self.fed, start_ms));
            while self.anchors.len() > MAX_ANCHORS {
                self.anchors.pop_front();
            }
        }
        self.fed += len as u64;
    }

    /// Timeline ms of the point `t` seconds after the first sample fed.
    pub fn at(&self, t: f64) -> u64 {
        let s = (t.max(0.0) * 16_000.0).round() as u64;
        let anchor = self
            .anchors
            .iter()
            .rev()
            .find(|(idx, _)| *idx <= s)
            .or(self.anchors.front());
        match anchor {
            Some(&(idx, ms)) if s >= idx => ms + (s - idx) / SAMPLES_PER_MS,
            Some(&(idx, ms)) => ms.saturating_sub((idx - s) / SAMPLES_PER_MS),
            None => s / SAMPLES_PER_MS,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn contiguous_frames_share_one_anchor() {
        let mut c = Clock::default();
        c.frame(5_000, 1600);
        c.frame(5_100, 1600);
        c.frame(5_200, 1600);
        assert_eq!(c.anchors.len(), 1);
        assert_eq!(c.at(0.0), 5_000);
        assert_eq!(c.at(0.25), 5_250);
    }

    #[test]
    fn gaps_are_followed() {
        let mut c = Clock::default();
        c.frame(0, 1600); // 0..100 ms
        c.frame(10_000, 1600); // jump: 10 s gap upstream
        assert_eq!(c.at(0.05), 50);
        assert_eq!(c.at(0.15), 10_050);
    }

    #[test]
    fn anchors_are_bounded() {
        let mut c = Clock::default();
        for i in 0..5_000u64 {
            c.frame(i * 1_000, 160); // 10 ms of audio per second: always a jump
        }
        assert_eq!(c.anchors.len(), MAX_ANCHORS);
        let first = 5_000 - MAX_ANCHORS as u64;
        assert_eq!(c.at(first as f64 * 0.01), first * 1_000);
    }
}
