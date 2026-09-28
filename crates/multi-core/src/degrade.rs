//! Degrade policy: when caption lag passes `degrade.max_lag_ms`, pause
//! translated lanes one at a time, lowest priority (highest `priority`
//! number) first; resume them, highest priority first, once lag has stayed
//! below half the limit for [`RECOVER_MS`].
//!
//! Only the `languages` and `source-only` steps of `degrade.order` are
//! built: the source lane is never paused. `model` and `pass-through` are
//! ignored until they exist. [`decide`] is pure: callers keep the returned
//! [`State`] and pass it back with the next lag sample.

use crate::config::{Degrade, DegradeStep, Language};

/// After a change, wait this long before pausing another lane, so the
/// change can take effect.
pub const HOLD_MS: u64 = 3_000;
/// Lag must stay below half the limit this long before a lane resumes.
pub const RECOVER_MS: u64 = 10_000;

/// Current lag of one lane.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LaneLag {
    pub lang: String,
    pub lag_ms: u64,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct State {
    /// Paused lanes, in the order they were paused.
    pub paused: Vec<String>,
    last_change_ms: Option<u64>,
    low_since_ms: Option<u64>,
}

impl State {
    pub fn is_paused(&self, lang: &str) -> bool {
        self.paused.iter().any(|p| p == lang)
    }
}

/// Translated lanes, lowest priority first (ties: later in the list first).
fn shed_order(langs: &[Language]) -> Vec<&Language> {
    let mut v: Vec<(usize, &Language)> = langs.iter().filter(|l| !l.source).enumerate().collect();
    v.sort_by(|a, b| b.1.priority.cmp(&a.1.priority).then(b.0.cmp(&a.0)));
    v.into_iter().map(|(_, l)| l).collect()
}

/// The next state given the lag of every lane at `now_ms`.
pub fn decide(
    lags: &[LaneLag],
    langs: &[Language],
    cfg: &Degrade,
    state: &State,
    now_ms: u64,
) -> State {
    let one_by_one = cfg.order.contains(&DegradeStep::Languages);
    let all_at_once = !one_by_one && cfg.order.contains(&DegradeStep::SourceOnly);
    if !one_by_one && !all_at_once {
        return State::default();
    }
    let order = shed_order(langs);
    let mut next = state.clone();
    // Forget lanes no longer configured.
    next.paused.retain(|p| order.iter().any(|l| &l.code == p));
    let max_lag = lags
        .iter()
        .filter(|l| !next.is_paused(&l.lang))
        .map(|l| l.lag_ms)
        .max()
        .unwrap_or(0);
    let high = u64::from(cfg.max_lag_ms);
    let low = high / 2;
    let since_change = state
        .last_change_ms
        .map_or(u64::MAX, |t| now_ms.saturating_sub(t));
    if max_lag > high {
        next.low_since_ms = None;
        if since_change >= HOLD_MS {
            let active: Vec<&Language> = order
                .iter()
                .copied()
                .filter(|l| !next.is_paused(&l.code))
                .collect();
            let take = if all_at_once { active.len() } else { 1 };
            if !active.is_empty() {
                for l in active.into_iter().take(take) {
                    next.paused.push(l.code.clone());
                }
                next.last_change_ms = Some(now_ms);
            }
        }
    } else if max_lag < low {
        let low_since = *next.low_since_ms.get_or_insert(now_ms);
        let calm = now_ms.saturating_sub(low_since) >= RECOVER_MS && since_change >= RECOVER_MS;
        if calm && !next.paused.is_empty() {
            if all_at_once {
                next.paused.clear();
            } else if let Some(best) = order.iter().rev().find(|l| next.is_paused(&l.code)) {
                let code = best.code.clone();
                next.paused.retain(|p| p != &code);
            }
            next.last_change_ms = Some(now_ms);
            next.low_since_ms = Some(now_ms);
        }
    } else {
        next.low_since_ms = None;
    }
    next
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;
    use crate::config::default_languages;

    fn lags(v: &[(&str, u64)]) -> Vec<LaneLag> {
        v.iter()
            .map(|(l, ms)| LaneLag {
                lang: l.to_string(),
                lag_ms: *ms,
            })
            .collect()
    }

    fn all(ms: u64) -> Vec<LaneLag> {
        lags(&[("en", 500), ("es", ms), ("fr", ms), ("de", ms)])
    }

    #[test]
    fn sheds_lowest_priority_first_with_hold() {
        let langs = default_languages();
        let cfg = Degrade::default();
        let s = decide(&all(1000), &langs, &cfg, &State::default(), 0);
        assert!(s.paused.is_empty());
        let s = decide(&all(6000), &langs, &cfg, &s, 1000);
        assert_eq!(s.paused, vec!["de"]);
        // Still lagging, but inside the hold time.
        let s = decide(&all(6000), &langs, &cfg, &s, 2000);
        assert_eq!(s.paused, vec!["de"]);
        let s = decide(&all(6000), &langs, &cfg, &s, 4000);
        assert_eq!(s.paused, vec!["de", "fr"]);
        let s = decide(&all(6000), &langs, &cfg, &s, 7000);
        let s = decide(&all(6000), &langs, &cfg, &s, 10_000);
        // The source lane is never paused.
        assert_eq!(s.paused, vec!["de", "fr", "es"]);
        // A paused lane's own lag does not count.
        let s2 = decide(&all(9000), &langs, &cfg, &s, 20_000);
        assert!(s2.low_since_ms.is_some());
    }

    #[test]
    fn recovers_with_hysteresis() {
        let langs = default_languages();
        let cfg = Degrade::default();
        let mut s = State::default();
        s = decide(&all(6000), &langs, &cfg, &s, 0);
        s = decide(&all(6000), &langs, &cfg, &s, 3000);
        assert_eq!(s.paused, vec!["de", "fr"]);
        // Between the marks: nothing changes, and the calm clock resets.
        s = decide(&all(3000), &langs, &cfg, &s, 4000);
        s = decide(&all(1000), &langs, &cfg, &s, 5000);
        s = decide(&all(3000), &langs, &cfg, &s, 14_000);
        s = decide(&all(1000), &langs, &cfg, &s, 15_000);
        s = decide(&all(1000), &langs, &cfg, &s, 24_000);
        assert_eq!(s.paused, vec!["de", "fr"]);
        s = decide(&all(1000), &langs, &cfg, &s, 25_000);
        assert_eq!(s.paused, vec!["de"], "highest priority resumes first");
        s = decide(&all(1000), &langs, &cfg, &s, 30_000);
        assert_eq!(s.paused, vec!["de"]);
        s = decide(&all(1000), &langs, &cfg, &s, 35_000);
        assert!(s.paused.is_empty());
    }

    #[test]
    fn source_lag_also_sheds_translations() {
        let langs = default_languages();
        let s = decide(
            &lags(&[("en", 7000)]),
            &langs,
            &Degrade::default(),
            &State::default(),
            0,
        );
        assert_eq!(s.paused, vec!["de"]);
    }

    #[test]
    fn order_controls_the_steps() {
        let langs = default_languages();
        let mut cfg = Degrade {
            order: vec![DegradeStep::SourceOnly],
            ..Degrade::default()
        };
        let s = decide(&all(6000), &langs, &cfg, &State::default(), 0);
        assert_eq!(s.paused.len(), 3);
        cfg.order = vec![DegradeStep::Model, DegradeStep::PassThrough];
        let s = decide(&all(6000), &langs, &cfg, &s, 5000);
        assert!(s.paused.is_empty());
    }

    #[test]
    fn priority_beats_list_order() {
        let mut langs = default_languages();
        langs[1].priority = 9; // es
        let s = decide(
            &all(6000),
            &langs,
            &Degrade::default(),
            &State::default(),
            0,
        );
        assert_eq!(s.paused, vec!["es"]);
    }
}
