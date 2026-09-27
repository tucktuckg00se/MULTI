//! Caption quality glue for the run loop: cleans and filters text on its way
//! to the lanes, tracks clauses sent for translation, drops stale
//! translations, measures per-lane lag and applies the degrade policy.
//!
//! Pure bookkeeping with an explicit clock (`now_ms`), so the run loop keeps
//! only the calls.

use std::collections::BTreeMap;

use crate::clean::clean;
use crate::config::{Config, Degrade, Language};
use crate::degrade::{self, LaneLag};
use crate::filter::WordFilter;
use crate::segment::{CloseReason, SegStats};
use crate::{Clause, Translation};

/// Caption text older than this is dropped rather than shown (from the
/// clause closing, for translations; from arrival, for queued text).
pub const MAX_AGE_MS: u64 = 6_000;
/// Clauses remembered for their translations.
const MAX_CLAUSES: usize = 256;
/// A translation latency counts toward lane lag this long.
const LAG_MEMORY_MS: u64 = 10_000;

struct ClauseInfo {
    new_row: bool,
    closed_ms: u64,
    /// Languages still to arrive.
    pending: Vec<String>,
}

/// Counters for the stats log.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct QualityStats {
    /// Texts with at least one masked word.
    pub masked: u64,
    /// Translations dropped because their clause was older than [`MAX_AGE_MS`].
    pub stale: u64,
    /// Translations dropped because their lane is paused.
    pub paused_drops: u64,
    /// Degrade changes (pauses plus resumes).
    pub degrade_changes: u64,
}

/// A translation ready for its lane.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Ready {
    pub text: String,
    pub new_row: bool,
    /// How long ago its clause closed.
    pub age_ms: u64,
}

pub struct CaptionQuality {
    source: String,
    targets: Vec<String>,
    langs: Vec<Language>,
    degrade_cfg: Degrade,
    filters: BTreeMap<String, WordFilter>,
    clauses: BTreeMap<u64, ClauseInfo>,
    degrade: degrade::State,
    /// Last translation latency per language, and when it was measured.
    tr_lag: BTreeMap<String, (u64, u64)>,
    by_reason: BTreeMap<CloseReason, u64>,
    pub stats: QualityStats,
}

impl CaptionQuality {
    pub fn new(cfg: &Config) -> Self {
        let source = cfg
            .languages
            .iter()
            .find(|l| l.source)
            .map_or_else(|| "en".to_string(), |l| l.code.clone());
        // Each lane: its own list plus the source language's, since a
        // translation can copy a source word through unchanged.
        let filters = cfg
            .languages
            .iter()
            .map(|l| {
                let f = WordFilter::new(&cfg.filter, &[l.code.as_str(), source.as_str()]);
                (l.code.clone(), f)
            })
            .collect();
        Self {
            targets: cfg
                .languages
                .iter()
                .filter(|l| !l.source)
                .map(|l| l.code.clone())
                .collect(),
            source,
            langs: cfg.languages.clone(),
            degrade_cfg: cfg.degrade.clone(),
            filters,
            clauses: BTreeMap::new(),
            degrade: degrade::State::default(),
            tr_lag: BTreeMap::new(),
            by_reason: BTreeMap::new(),
            stats: QualityStats::default(),
        }
    }

    /// Cleans and filters text for lane `lang`; `None` if nothing is left.
    pub fn text_for(&mut self, lang: &str, text: &str) -> Option<String> {
        let cleaned = clean(text);
        let out = match self.filters.get(lang) {
            Some(f) => f.apply(&cleaned),
            // Unknown lane: filter with every list rather than not at all.
            None => {
                let mut t = cleaned.clone();
                for f in self.filters.values() {
                    t = f.apply(&t);
                }
                t
            }
        };
        if out != cleaned {
            self.stats.masked += 1;
        }
        (!out.trim().is_empty()).then_some(out)
    }

    /// Source-lane text from the segmenter.
    pub fn source_text(&mut self, text: &str) -> Option<String> {
        let lang = self.source.clone();
        self.text_for(&lang, text)
    }

    /// Records a closed clause; returns the languages to translate it into
    /// (paused lanes left out). The clause goes to MT unfiltered, so the
    /// model sees the real words; its output is filtered.
    pub fn clause(
        &mut self,
        clause: &Clause,
        new_row: bool,
        reason: CloseReason,
        now_ms: u64,
    ) -> Vec<String> {
        *self.by_reason.entry(reason).or_default() += 1;
        let langs: Vec<String> = self
            .targets
            .iter()
            .filter(|l| !self.degrade.is_paused(l))
            .cloned()
            .collect();
        self.clauses.insert(
            clause.id,
            ClauseInfo {
                new_row,
                closed_ms: now_ms,
                pending: langs.clone(),
            },
        );
        while self.clauses.len() > MAX_CLAUSES {
            self.clauses.pop_first();
        }
        langs
    }

    /// A translation from MT: `None` if it is stale, its lane is paused, or
    /// nothing is left after filtering.
    pub fn translation(&mut self, tr: &Translation, now_ms: u64) -> Option<Ready> {
        let (new_row, age_ms) = match self.clauses.get_mut(&tr.clause_id) {
            Some(c) => {
                c.pending.retain(|l| l != &tr.lang);
                (c.new_row, now_ms.saturating_sub(c.closed_ms))
            }
            // Forgotten long ago: certainly stale.
            None => (true, u64::MAX),
        };
        if age_ms != u64::MAX {
            self.tr_lag.insert(tr.lang.clone(), (age_ms, now_ms));
        }
        if age_ms > MAX_AGE_MS {
            self.stats.stale += 1;
            return None;
        }
        if self.degrade.is_paused(&tr.lang) {
            self.stats.paused_drops += 1;
            return None;
        }
        let text = self.text_for(&tr.lang.clone(), &tr.text)?;
        Some(Ready {
            text,
            new_row,
            age_ms,
        })
    }

    /// Per-lane lag: the oldest queued text in the lane (`queue_age_ms`,
    /// from the media stats), the latest translation latency, and the age of
    /// the oldest translation still outstanding.
    pub fn lags(&self, queue_age_ms: &[(String, u64)], now_ms: u64) -> Vec<LaneLag> {
        let mut out: Vec<LaneLag> = self
            .langs
            .iter()
            .map(|l| LaneLag {
                lang: l.code.clone(),
                lag_ms: 0,
            })
            .collect();
        for lag in &mut out {
            let q = queue_age_ms
                .iter()
                .find(|(l, _)| l == &lag.lang)
                .map_or(0, |q| q.1);
            let tr = self
                .tr_lag
                .get(&lag.lang)
                .filter(|(_, at)| now_ms.saturating_sub(*at) <= LAG_MEMORY_MS)
                .map_or(0, |(ms, _)| *ms);
            let pending = self
                .clauses
                .values()
                .filter(|c| c.pending.contains(&lag.lang))
                .map(|c| now_ms.saturating_sub(c.closed_ms))
                .filter(|&age| age <= MAX_AGE_MS + 1_000)
                .max()
                .unwrap_or(0);
            lag.lag_ms = q.max(tr).max(pending);
        }
        out
    }

    /// Runs the degrade policy. Returns the paused lanes when they change.
    pub fn update_degrade(
        &mut self,
        queue_age_ms: &[(String, u64)],
        now_ms: u64,
    ) -> Option<Vec<String>> {
        let lags = self.lags(queue_age_ms, now_ms);
        let next = degrade::decide(&lags, &self.langs, &self.degrade_cfg, &self.degrade, now_ms);
        let changed = next.paused != self.degrade.paused;
        self.degrade = next;
        if changed {
            self.stats.degrade_changes += 1;
            // Paused lanes are no longer expected.
            for c in self.clauses.values_mut() {
                c.pending.retain(|l| !self.degrade.is_paused(l));
            }
        }
        changed.then(|| self.degrade.paused.clone())
    }

    pub fn paused(&self) -> &[String] {
        &self.degrade.paused
    }

    /// One line for the stats log.
    pub fn summary(&self, seg: &SegStats) -> String {
        format!(
            "clauses={} punct={} pause={} timer={} timer_at_pause={} max_words={} masked={} stale_tr={} paused={:?}",
            seg.total(),
            seg.punctuation,
            seg.pause,
            seg.timer,
            seg.timer_at_pause,
            seg.max_words,
            self.stats.masked,
            self.stats.stale,
            self.degrade.paused,
        )
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;

    fn clause(id: u64, text: &str) -> Clause {
        Clause {
            id,
            text: text.into(),
            start_ms: 0,
            end_ms: 0,
        }
    }

    fn tr(id: u64, lang: &str, text: &str) -> Translation {
        Translation {
            clause_id: id,
            lang: lang.into(),
            text: text.into(),
            elapsed_ms: 0,
        }
    }

    fn config() -> Config {
        let mut c = Config::default();
        c.filter.blocklist = vec!["charlie".into()];
        c
    }

    #[test]
    fn source_and_translations_are_filtered() {
        let mut q = CaptionQuality::new(&config());
        assert_eq!(
            q.source_text("alpha Charlie!").as_deref(),
            Some("alpha C******!")
        );
        let langs = q.clause(&clause(1, "alpha charlie"), true, CloseReason::Pause, 0);
        assert_eq!(langs, vec!["es", "fr", "de"]);
        let r = q
            .translation(&tr(1, "es", "[es] alpha charlie"), 100)
            .unwrap();
        assert_eq!(r.text, "[es] alpha c******");
        assert!(r.new_row);
        // A source-language swear copied into a translation is caught too.
        let r = q.translation(&tr(1, "fr", "il a dit fuck…"), 100).unwrap();
        assert_eq!(r.text, "il a dit f***...");
        assert_eq!(q.stats.masked, 3);
        assert_eq!(q.source_text(" \u{1F600} "), None);
    }

    #[test]
    fn stale_translations_are_dropped() {
        let mut q = CaptionQuality::new(&config());
        q.clause(&clause(1, "a b c"), false, CloseReason::Timer, 1000);
        assert!(
            q.translation(&tr(1, "es", "x"), 1000 + MAX_AGE_MS + 1)
                .is_none()
        );
        assert!(q.translation(&tr(99, "es", "x"), 0).is_none());
        assert_eq!(q.stats.stale, 2);
        let r = q.translation(&tr(1, "fr", "y"), 1000 + MAX_AGE_MS).unwrap();
        assert_eq!((r.new_row, r.age_ms), (false, MAX_AGE_MS));
    }

    #[test]
    fn stuck_translations_pause_lanes() {
        let mut q = CaptionQuality::new(&Config::default());
        // MT never answers: the outstanding clause ages past max_lag_ms.
        q.clause(&clause(1, "a b c"), true, CloseReason::Pause, 0);
        assert_eq!(q.update_degrade(&[], 1000), None);
        assert_eq!(q.update_degrade(&[], 5500), Some(vec!["de".to_string()]));
        let langs = q.clause(&clause(2, "d e f"), true, CloseReason::Pause, 5600);
        assert_eq!(langs, vec!["es", "fr"]);
        assert!(q.translation(&tr(2, "de", "x"), 5700).is_none());
        assert_eq!(q.stats.paused_drops, 1);
        // Queue age from the media side counts as well.
        let lags = q.lags(&[("en".into(), 4200)], 5700);
        assert_eq!(lags[0].lag_ms, 4200);
    }
}
