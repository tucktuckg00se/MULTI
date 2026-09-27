//! Wall-clock adapter over [`multi_core::segment`] (M1 WP4): the run loop
//! passes `Instant`s, the core segmenter takes milliseconds since start.

use multi_core::Word;
pub use multi_core::segment::{CloseReason, Event, MAX_WORDS, SegStats};
use std::time::{Duration, Instant};

pub struct Segmenter {
    inner: multi_core::segment::Segmenter,
    t0: Instant,
}

impl Segmenter {
    pub fn new(max_wait: Duration) -> Self {
        Self {
            inner: multi_core::segment::Segmenter::new(
                u64::try_from(max_wait.as_millis()).unwrap_or(u64::MAX),
            ),
            t0: Instant::now(),
        }
    }

    /// Milliseconds since this segmenter was made: the clock handed to
    /// [`multi_core::quality`] as well.
    pub fn ms(&self, now: Instant) -> u64 {
        u64::try_from(now.saturating_duration_since(self.t0).as_millis()).unwrap_or(u64::MAX)
    }

    /// Handles newly committed words.
    pub fn words(&mut self, ws: &[Word], now: Instant) -> Vec<Event> {
        let ms = self.ms(now);
        self.inner.words(ws, ms)
    }

    /// Closes the open clause after `max_wait` without words.
    pub fn tick(&mut self, now: Instant) -> Vec<Event> {
        let ms = self.ms(now);
        self.inner.tick(ms)
    }

    /// End of stream: closes whatever is open.
    pub fn finish(&mut self) -> Vec<Event> {
        self.inner.finish()
    }

    pub fn stats(&self) -> SegStats {
        self.inner.stats
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;

    #[test]
    fn wall_clock_timer() {
        let mut s = Segmenter::new(Duration::from_millis(800));
        let t0 = Instant::now();
        let w = |text: &str, t| Word {
            text: text.into(),
            start_ms: t,
            end_ms: t + 100,
        };
        s.words(&[w("a", 0), w("b", 150), w("c", 300)], t0);
        assert!(s.tick(t0 + Duration::from_millis(500)).is_empty());
        let ev = s.tick(t0 + Duration::from_millis(900));
        assert!(matches!(
            &ev[..],
            [Event::Clause {
                reason: CloseReason::Timer,
                ..
            }]
        ));
        assert_eq!(s.stats().timer, 1);
    }
}
