//! Hallucination guard for Whisper output (M2-5).
//!
//! S4 measured Whisper making up "Thank you." thirteen times over music even
//! behind the VAD (Silero opens on music). The guard runs on every pass's
//! segments before LocalAgreement sees them:
//! - a segment with a high no-speech probability or a low average token
//!   log-probability is dropped;
//! - a segment made only of known filler lines ("Thank you.", "Thanks for
//!   watching", "Subtitles by …", in several languages) is dropped;
//! - an n-gram repeated in a loop ("the the the the", "I'm sorry. I'm sorry.
//!   I'm sorry.") is collapsed to one copy, within a pass and, through
//!   [`LoopFilter`], across the words committed over the last 30 s (music
//!   gives one "Oh." per VAD segment).
//!
//! The first rule is also the reason decoding happens only inside VAD speech.

use crate::engine::RawWord;

/// One decoder segment with its confidence.
#[derive(Clone, Debug, PartialEq)]
pub struct Segment {
    pub words: Vec<RawWord>,
    /// Probability of the no-speech token at the segment start.
    pub no_speech: f32,
    /// Mean log-probability of the segment's text tokens.
    pub avg_logprob: f32,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct GuardCfg {
    /// Drop a segment whose no-speech probability is above this.
    pub max_no_speech: f32,
    /// Drop a segment whose average log-probability is below this.
    pub min_avg_logprob: f32,
}

impl Default for GuardCfg {
    fn default() -> Self {
        Self {
            max_no_speech: 0.6,
            min_avg_logprob: -1.0,
        }
    }
}

/// Why a segment was dropped.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reject {
    NoSpeech,
    LowConfidence,
    Filler,
}

/// Whole lines Whisper produces from silence, music or noise (normalised:
/// lowercase, letters and digits only, single spaces).
const FILLER: &[&str] = &[
    "thank you",
    "thank you very much",
    "thank you so much",
    "thanks",
    "thanks for watching",
    "thank you for watching",
    "thanks for watching and see you next time",
    "please subscribe",
    "please like and subscribe",
    "like and subscribe",
    "subscribe to the channel",
    "see you next time",
    "see you in the next video",
    "you",
    "bye",
    "bye bye",
    "amen",
    "music",
    "applause",
    "silence",
    "blank audio",
    "gracias",
    "muchas gracias",
    "gracias por ver",
    "gracias por ver el video",
    "suscríbete",
    "danke",
    "vielen dank",
    "danke fürs zuschauen",
    "bis zum nächsten mal",
    "merci",
    "merci beaucoup",
    "merci d avoir regardé",
    "obrigado",
    "grazie",
    "продолжение следует",
    "спасибо",
];

/// Interjections: a sentence made only of these ("Oh, oh.") is filler.
/// Music gives a lone "Oh." every few VAD segments, too far apart to be a
/// loop; a caption loses little without them.
const INTERJECTION: &[&str] = &[
    "oh", "ah", "uh", "um", "umm", "hmm", "mm", "mhm", "huh", "ooh", "eh", "ha", "la", "na",
];

/// Credits lines ("Subtitles by the Amara.org community"): dropped when a
/// sentence starts with one of these.
const FILLER_PREFIX: &[&str] = &[
    "subtitles by",
    "subtitled by",
    "subtitling by",
    "captions by",
    "captioning by",
    "captioned by",
    "transcribed by",
    "transcription by",
    "translated by",
    "subtítulos por",
    "subtítulos realizados por",
    "subtitulado por",
    "untertitel",
    "untertitelung",
    "sous titres",
    "sous titrage",
    "sottotitoli",
    "legendas pela",
    "amara org",
    "www ",
    "http",
];

/// Lowercase, letters and digits only, single spaces.
pub fn normalise(text: &str) -> String {
    let mapped: String = text
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { ' ' })
        .flat_map(char::to_lowercase)
        .collect();
    mapped.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Every sentence of `words` is a known filler line (and there is one).
pub fn is_filler(words: &[RawWord]) -> bool {
    let mut sentences: Vec<String> = Vec::new();
    let mut cur = String::new();
    for w in words {
        cur.push_str(&w.text);
        cur.push(' ');
        if w.text.ends_with(['.', '!', '?', '。', '！', '？']) {
            sentences.push(std::mem::take(&mut cur));
        }
    }
    sentences.push(cur);
    let mut any = false;
    for s in sentences {
        let n = normalise(&s);
        if n.is_empty() {
            continue;
        }
        let filler = FILLER.contains(&n.as_str())
            || FILLER_PREFIX.iter().any(|p| n.starts_with(p))
            || n.split(' ').all(|w| INTERJECTION.contains(&w));
        if !filler {
            return false;
        }
        any = true;
    }
    any
}

/// The checks on one segment, in order.
pub fn check(seg: &Segment, cfg: &GuardCfg) -> Option<Reject> {
    if seg.no_speech > cfg.max_no_speech {
        Some(Reject::NoSpeech)
    } else if seg.avg_logprob < cfg.min_avg_logprob {
        Some(Reject::LowConfidence)
    } else if is_filler(&seg.words) {
        Some(Reject::Filler)
    } else {
        None
    }
}

/// Collapses an n-gram (n = 1..=8) repeated back to back into one copy: a
/// single word needs 4 copies in a row, a longer n-gram 3. Returns the kept
/// words and how many were removed.
pub fn collapse_loops(words: Vec<RawWord>) -> (Vec<RawWord>, usize) {
    let keys: Vec<String> = words.iter().map(|w| normalise(&w.text)).collect();
    let keep = loop_mask(&keys);
    let removed = keep.iter().filter(|k| !**k).count();
    let kept = words
        .into_iter()
        .zip(keep)
        .filter_map(|(w, k)| k.then_some(w))
        .collect();
    (kept, removed)
}

/// `false` for every word that repeats the n-gram before it in a loop.
fn loop_mask(keys: &[String]) -> Vec<bool> {
    const MAX_N: usize = 8;
    let mut keep = vec![true; keys.len()];
    let mut i = 0;
    while i < keys.len() {
        let mut skipped = false;
        for n in 1..=MAX_N {
            let min_reps = if n == 1 { 4 } else { 3 };
            if i + n * min_reps > keys.len() {
                break;
            }
            let same = |k: usize| (0..n).all(|j| keys[i + j] == keys[i + k * n + j]);
            let mut reps = 1;
            while i + (reps + 1) * n <= keys.len() && same(reps) {
                reps += 1;
            }
            if reps >= min_reps && keys[i..i + n].iter().any(|k| !k.is_empty()) {
                for k in keep.iter_mut().skip(i + n).take((reps - 1) * n) {
                    *k = false;
                }
                i += reps * n;
                skipped = true;
                break;
            }
        }
        if !skipped {
            i += 1;
        }
    }
    keep
}

/// Drops committed words that would continue a loop with the words
/// committed shortly before (across passes and VAD segments).
#[derive(Default)]
pub struct LoopFilter {
    recent: std::collections::VecDeque<(String, f64)>,
    pub dropped: u64,
}

impl LoopFilter {
    /// Words further back than this no longer count.
    const WINDOW_S: f64 = 30.0;

    /// The words of `words` that do not repeat, in a loop, what came
    /// before them (the recent committed words, then the batch itself).
    pub fn admit(&mut self, words: Vec<RawWord>) -> Vec<RawWord> {
        let Some(first) = words.first() else {
            return words;
        };
        let since = first.t0 - Self::WINDOW_S;
        while self.recent.front().is_some_and(|(_, t1)| *t1 < since) {
            self.recent.pop_front();
        }
        let old = self.recent.len();
        let keys: Vec<String> = self
            .recent
            .iter()
            .map(|(k, _)| k.clone())
            .chain(words.iter().map(|w| normalise(&w.text)))
            .collect();
        let keep = loop_mask(&keys);
        let mut out = Vec::new();
        for ((w, key), k) in words
            .into_iter()
            .zip(keys.into_iter().skip(old))
            .zip(keep.into_iter().skip(old))
        {
            if k {
                self.recent.push_back((key, w.t1));
                out.push(w);
            } else {
                self.dropped += 1;
            }
        }
        while self.recent.len() > 64 {
            self.recent.pop_front();
        }
        out
    }

    pub fn reset(&mut self) {
        self.recent.clear();
    }
}

/// Counts of what the guard removed, for the log.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct GuardStats {
    pub no_speech: u64,
    pub low_confidence: u64,
    pub filler: u64,
    pub loop_words: u64,
}

/// Runs the guard over one pass: the words of the segments that survive,
/// with loops collapsed.
pub fn apply(segments: Vec<Segment>, cfg: &GuardCfg, stats: &mut GuardStats) -> Vec<RawWord> {
    let mut words = Vec::new();
    for seg in segments {
        match check(&seg, cfg) {
            None => words.extend(seg.words),
            Some(Reject::NoSpeech) => stats.no_speech += 1,
            Some(Reject::LowConfidence) => stats.low_confidence += 1,
            Some(Reject::Filler) => stats.filler += 1,
        }
    }
    let (words, removed) = collapse_loops(words);
    stats.loop_words += removed as u64;
    words
}

#[cfg(test)]
mod tests {
    use super::*;

    fn words(s: &str) -> Vec<RawWord> {
        s.split_whitespace()
            .enumerate()
            .map(|(i, w)| RawWord {
                text: w.into(),
                t0: i as f64,
                t1: i as f64 + 0.5,
            })
            .collect()
    }

    fn seg(s: &str, no_speech: f32, avg_logprob: f32) -> Segment {
        Segment {
            words: words(s),
            no_speech,
            avg_logprob,
        }
    }

    fn text(w: &[RawWord]) -> String {
        w.iter()
            .map(|w| w.text.as_str())
            .collect::<Vec<_>>()
            .join(" ")
    }

    #[test]
    fn fillers_are_recognised() {
        for s in [
            "Thank you.",
            "Thank you. Thank you.",
            "Amen. Thank you.",
            "Thanks for watching!",
            "Subtitles by the Amara.org community",
            "Subtítulos realizados por la comunidad de Amara.org",
            "Untertitel im Auftrag des ZDF, 2021",
            "¡Gracias por ver el video!",
            "you",
            "Oh, oh, oh.",
            "Hmm. Thank you.",
        ] {
            assert!(is_filler(&words(s)), "{s}");
        }
        for s in [
            "Thank you for coming today.",
            "Thank you. Now the first item.",
            "Oh, that is new.",
            "It is a truth universally acknowledged",
            "",
        ] {
            assert!(!is_filler(&words(s)), "{s}");
        }
    }

    #[test]
    fn segments_are_checked_in_order() {
        let c = GuardCfg::default();
        assert_eq!(check(&seg("hello there", 0.1, -0.3), &c), None);
        assert_eq!(
            check(&seg("hello there", 0.9, -0.3), &c),
            Some(Reject::NoSpeech)
        );
        assert_eq!(
            check(&seg("hello there", 0.1, -1.6), &c),
            Some(Reject::LowConfidence)
        );
        assert_eq!(
            check(&seg("Thank you.", 0.1, -0.2), &c),
            Some(Reject::Filler)
        );
    }

    #[test]
    fn loops_collapse_to_one_copy() {
        let (w, n) = collapse_loops(words("so the the the the end"));
        assert_eq!((text(&w).as_str(), n), ("so the end", 3));
        let (w, n) = collapse_loops(words("I'm sorry. I'm sorry. I'm sorry. Go"));
        assert_eq!((text(&w).as_str(), n), ("I'm sorry. Go", 4));
        // Punctuation and case do not hide a loop.
        let (w, _) = collapse_loops(words("Yes, yes. YES yes! ok"));
        assert_eq!(text(&w), "Yes, ok");
        // Short repeats in real speech stay.
        for s in ["no no no", "very very good", "bye bye", "a b a b c"] {
            let (w, n) = collapse_loops(words(s));
            assert_eq!((text(&w).as_str(), n), (s, 0));
        }
        let (w, n) = collapse_loops(Vec::new());
        assert!(w.is_empty() && n == 0);
    }

    #[test]
    fn loops_across_commits_are_dropped() {
        let mut f = LoopFilter::default();
        let mut kept = Vec::new();
        // One "Oh." per commit, as music gives them.
        for i in 0..8 {
            let mut w = words("Oh.");
            w[0].t0 = f64::from(i) * 3.0;
            w[0].t1 = w[0].t0 + 0.5;
            kept.extend(f.admit(w));
        }
        assert_eq!(kept.len(), 3);
        assert_eq!(f.dropped, 5);
        // Two-word loops too, and ordinary text passes.
        let mut f = LoopFilter::default();
        let w = f.admit(words("thank you thank you thank you thank you and more"));
        assert_eq!(text(&w), "thank you and more");
        let w = f.admit(words("the the cat sat on the mat"));
        assert_eq!(text(&w), "the the cat sat on the mat");
        // Far apart in time: not a loop.
        let mut f = LoopFilter::default();
        let mut n = 0;
        for i in 0..6 {
            let mut w = words("Yes.");
            w[0].t0 = f64::from(i) * 40.0;
            w[0].t1 = w[0].t0 + 0.3;
            n += f.admit(w).len();
        }
        assert_eq!(n, 6);
    }

    #[test]
    fn apply_drops_and_counts() {
        let mut st = GuardStats::default();
        let segs = vec![
            seg("The court is in session.", 0.02, -0.2),
            seg("Thank you.", 0.3, -0.4),
            seg("go go go go go", 0.1, -0.5),
            seg("mumble", 0.8, -0.3),
            seg("garbled words", 0.1, -2.0),
        ];
        let w = apply(segs, &GuardCfg::default(), &mut st);
        assert_eq!(text(&w), "The court is in session. go");
        assert_eq!(
            st,
            GuardStats {
                no_speech: 1,
                low_confidence: 1,
                filler: 1,
                loop_words: 4,
            }
        );
    }
}
