//! Clause segmenter: turns committed ASR words into source-lane text (as the
//! words arrive) and closed clauses for translation.
//!
//! A clause closes
//! - on sentence punctuation (`.`, `!`, `?`);
//! - on a speech pause: a gap of at least [`PAUSE_MS`] between one word's
//!   end and the next word's start (word timestamps stand in for VAD);
//! - on the timer: `translate.max_wait_ms` of wall time since the last word;
//! - at [`MAX_WORDS`], cut after the last comma if there is one;
//! - at the end of the stream ([`Segmenter::finish`]).
//!
//! A clause has at least [`MIN_WORDS`] words, unless the pause is long
//! ([`LONG_PAUSE_MS`], on the word timeline or the wall clock) or the stream
//! ends. The segmenter has no clock of its own: callers pass `now_ms`
//! (milliseconds on any monotonic clock), so it is tested without real time.

use crate::clean::clean;
use crate::{Clause, Word};

/// A clause never grows beyond this many words.
pub const MAX_WORDS: usize = 24;
/// Fewer words than this only close on a long pause or at the end.
pub const MIN_WORDS: usize = 3;
/// A gap between words at least this long is a pause.
pub const PAUSE_MS: u64 = 350;
/// A pause at least this long closes even a short clause.
pub const LONG_PAUSE_MS: u64 = 1000;

/// Why a clause closed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum CloseReason {
    Punctuation,
    Pause,
    Timer,
    MaxWords,
    End,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Event {
    /// Text for the source-language lane.
    Source { text: String, new_row: bool },
    /// A closed clause to translate. `new_row`: it starts a new caption row.
    Clause {
        clause: Clause,
        new_row: bool,
        reason: CloseReason,
    },
}

/// Clauses closed so far, by reason.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SegStats {
    pub punctuation: u64,
    pub pause: u64,
    pub timer: u64,
    /// Timer closes where the next word came after a pause anyway (the
    /// timer only beat the pause detector), as opposed to mid-speech cuts.
    pub timer_at_pause: u64,
    pub max_words: u64,
    pub end: u64,
}

impl SegStats {
    pub fn total(&self) -> u64 {
        self.punctuation + self.pause + self.timer + self.max_words + self.end
    }
}

fn is_punct(w: &str) -> bool {
    !w.is_empty() && w.chars().all(|c| !c.is_alphanumeric())
}

/// Marks that open a phrase and attach to the next word (`¿Qué`, `«Oui`).
fn opening(c: char) -> bool {
    matches!(
        c,
        '¿' | '¡' | '«' | '(' | '[' | '“' | '„' | '「' | '『' | '（' | '《'
    )
}

/// Marks that may follow a sentence end (`fin.»`, `好。」`).
fn closing(c: char) -> bool {
    matches!(
        c,
        '»' | ')' | ']' | '"' | '\'' | '”' | '’' | '」' | '』' | '）' | '》'
    )
}

/// Ends a sentence: `. ! ?`, the full-width `。！？`, or `…`, before any
/// closing quotes or brackets.
fn sentence_end(w: &str) -> bool {
    matches!(
        w.trim_end_matches(closing).chars().last(),
        Some('.' | '!' | '?' | '。' | '！' | '？' | '…')
    )
}

/// Ends a clause: a comma (Latin, full-width `，` or the ideographic `、`).
fn clause_end(w: &str) -> bool {
    matches!(
        w.trim_end_matches(closing).chars().last(),
        Some(',' | '，' | '、')
    )
}

/// Chinese and Japanese characters, written without spaces between words.
fn cjk(c: char) -> bool {
    matches!(c,
        '\u{3000}'..='\u{30FF}'
        | '\u{3400}'..='\u{4DBF}'
        | '\u{4E00}'..='\u{9FFF}'
        | '\u{F900}'..='\u{FAFF}'
        | '\u{FF00}'..='\u{FFEF}')
}

/// Appends `w` with a space, except: before bare closing punctuation, after
/// an opening mark (`¿`, `«`), and between CJK characters.
fn append(s: &mut String, w: &str) {
    let glue = s.is_empty()
        || (is_punct(w) && !w.chars().all(opening))
        || s.ends_with(opening)
        || (s.chars().last().is_some_and(cjk) && w.chars().next().is_some_and(cjk));
    if !glue {
        s.push(' ');
    }
    s.push_str(w);
}

pub struct Segmenter {
    max_wait_ms: u64,
    next_id: u64,
    words: Vec<Word>,
    /// The next source text starts a new row (after a close other than a
    /// word-limit cut).
    new_row: bool,
    /// Whether the open clause started on a new row.
    clause_row: bool,
    /// Wall time the last word arrived, while a clause is open.
    last_arrival: Option<u64>,
    /// End of the last word on the word timeline.
    prev_end_ms: Option<u64>,
    /// The last clause closed on the timer (for [`SegStats::timer_at_pause`]).
    timer_pending: bool,
    pub stats: SegStats,
}

impl Segmenter {
    pub fn new(max_wait_ms: u64) -> Self {
        Self {
            max_wait_ms,
            next_id: 0,
            words: Vec::new(),
            new_row: true,
            clause_row: true,
            last_arrival: None,
            prev_end_ms: None,
            timer_pending: false,
            stats: SegStats::default(),
        }
    }

    /// Words in the open clause, not counting bare punctuation.
    fn count(&self) -> usize {
        self.words.iter().filter(|w| !is_punct(&w.text)).count()
    }

    /// Closes the first `n` words of the open clause.
    fn close(&mut self, n: usize, reason: CloseReason, out: &mut Vec<Event>) {
        let n = n.min(self.words.len());
        if n == 0 {
            return;
        }
        let rest = self.words.split_off(n);
        let words = std::mem::replace(&mut self.words, rest);
        let mut text = String::new();
        for w in &words {
            append(&mut text, &w.text);
        }
        let start_ms = words.first().map_or(0, |w| w.start_ms);
        let end_ms = words.last().map_or(0, |w| w.end_ms);
        // A late punctuation suffix alone is not worth translating.
        if !words.iter().all(|w| is_punct(&w.text)) {
            out.push(Event::Clause {
                clause: Clause {
                    id: self.next_id,
                    text,
                    start_ms,
                    end_ms,
                },
                new_row: self.clause_row,
                reason,
            });
            self.next_id += 1;
            let s = &mut self.stats;
            match reason {
                CloseReason::Punctuation => s.punctuation += 1,
                CloseReason::Pause => s.pause += 1,
                CloseReason::Timer => s.timer += 1,
                CloseReason::MaxWords => s.max_words += 1,
                CloseReason::End => s.end += 1,
            }
        }
        self.timer_pending = reason == CloseReason::Timer;
        let row = reason != CloseReason::MaxWords;
        self.new_row = row;
        self.clause_row = row;
        if self.words.is_empty() {
            self.last_arrival = None;
        }
    }

    /// Where to cut a full clause: after the last comma that leaves at
    /// least [`MIN_WORDS`] words, else everything.
    fn cut_point(&self) -> usize {
        self.words
            .iter()
            .enumerate()
            .rev()
            .skip(1)
            .find(|(i, w)| *i + 1 >= MIN_WORDS && clause_end(&w.text))
            .map_or(self.words.len(), |(i, _)| i + 1)
    }

    /// Handles newly committed words that arrived at `now_ms`.
    pub fn words(&mut self, ws: &[Word], now_ms: u64) -> Vec<Event> {
        let mut out = Vec::new();
        let mut chunk = String::new();
        let mut chunk_row = self.new_row;
        let flush = |chunk: &mut String, row: bool, out: &mut Vec<Event>| {
            if !chunk.is_empty() {
                out.push(Event::Source {
                    text: std::mem::take(chunk),
                    new_row: row,
                });
            }
        };
        for w in ws {
            let t = clean(&w.text);
            if t.is_empty() {
                continue;
            }
            let gap = self
                .prev_end_ms
                .map(|p| w.start_ms.saturating_sub(p))
                .unwrap_or(0);
            if gap >= PAUSE_MS && self.timer_pending {
                self.stats.timer_at_pause += 1;
            }
            self.timer_pending = false;
            self.prev_end_ms = Some(w.end_ms.max(w.start_ms));
            if gap >= PAUSE_MS
                && !self.words.is_empty()
                && (self.count() >= MIN_WORDS || gap >= LONG_PAUSE_MS)
            {
                flush(&mut chunk, chunk_row, &mut out);
                let n = self.words.len();
                self.close(n, CloseReason::Pause, &mut out);
                chunk_row = self.new_row;
            }
            if self.words.is_empty() {
                self.clause_row = self.new_row;
            }
            append(&mut chunk, &t);
            let end = sentence_end(&t);
            self.words.push(Word {
                text: t,
                ..w.clone()
            });
            if end && self.count() >= MIN_WORDS {
                flush(&mut chunk, chunk_row, &mut out);
                let n = self.words.len();
                self.close(n, CloseReason::Punctuation, &mut out);
                chunk_row = self.new_row;
            } else if self.count() >= MAX_WORDS {
                flush(&mut chunk, chunk_row, &mut out);
                let n = self.cut_point();
                self.close(n, CloseReason::MaxWords, &mut out);
                chunk_row = self.new_row;
            }
        }
        if !chunk.is_empty() {
            flush(&mut chunk, chunk_row, &mut out);
            self.new_row = false;
        }
        if !self.words.is_empty() && !ws.is_empty() {
            self.last_arrival = Some(now_ms);
        }
        out
    }

    /// Closes the open clause once `max_wait_ms` has passed since the last
    /// word (a short clause waits for a long pause).
    pub fn tick(&mut self, now_ms: u64) -> Vec<Event> {
        let mut out = Vec::new();
        let Some(t) = self.last_arrival else {
            return out;
        };
        let idle = now_ms.saturating_sub(t);
        let short = self.count() < MIN_WORDS;
        if idle >= self.max_wait_ms && (!short || idle >= self.max_wait_ms.max(LONG_PAUSE_MS)) {
            let n = self.words.len();
            self.close(n, CloseReason::Timer, &mut out);
        }
        out
    }

    /// End of stream: closes whatever is open.
    pub fn finish(&mut self) -> Vec<Event> {
        let mut out = Vec::new();
        let n = self.words.len();
        self.close(n, CloseReason::End, &mut out);
        out
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;

    fn w(text: &str, t: u64) -> Word {
        Word {
            text: text.into(),
            start_ms: t,
            end_ms: t + 100,
        }
    }

    /// Words 150 ms apart (no pause).
    fn run(texts: &[&str], t0: u64) -> Vec<Word> {
        texts
            .iter()
            .enumerate()
            .map(|(i, s)| w(s, t0 + i as u64 * 150))
            .collect()
    }

    fn clauses(ev: &[Event]) -> Vec<(String, bool, CloseReason)> {
        ev.iter()
            .filter_map(|e| match e {
                Event::Clause {
                    clause,
                    new_row,
                    reason,
                } => Some((clause.text.clone(), *new_row, *reason)),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn punctuation_closes_and_source_streams() {
        let mut s = Segmenter::new(800);
        let ev = s.words(&run(&["Hello", "out", "there.", "How"], 0), 0);
        assert_eq!(
            ev[0],
            Event::Source {
                text: "Hello out there.".into(),
                new_row: true
            }
        );
        assert_eq!(
            clauses(&ev),
            vec![("Hello out there.".into(), true, CloseReason::Punctuation)]
        );
        assert_eq!(
            ev.last(),
            Some(&Event::Source {
                text: "How".into(),
                new_row: true
            })
        );
        let ev = s.words(&[w("are", 600)], 10);
        assert_eq!(
            ev,
            vec![Event::Source {
                text: "are".into(),
                new_row: false
            }]
        );
    }

    #[test]
    fn short_sentence_waits_for_more_words() {
        let mut s = Segmenter::new(800);
        let ev = s.words(&run(&["Yes.", "I", "think", "so."], 0), 0);
        assert_eq!(
            clauses(&ev),
            vec![("Yes. I think so.".into(), true, CloseReason::Punctuation)]
        );
    }

    #[test]
    fn pause_between_words_closes() {
        let mut s = Segmenter::new(800);
        let mut ws = run(&["one", "two", "three"], 0);
        ws.push(w("four", 300 + 100 + PAUSE_MS));
        let ev = s.words(&ws, 0);
        assert_eq!(
            clauses(&ev),
            vec![("one two three".into(), true, CloseReason::Pause)]
        );
        // The source lane gets a new row at the pause.
        assert_eq!(
            ev.last(),
            Some(&Event::Source {
                text: "four".into(),
                new_row: true
            })
        );
    }

    #[test]
    fn short_clause_needs_a_long_pause() {
        let mut s = Segmenter::new(800);
        let ev = s.words(&[w("hi", 0), w("there", 600)], 0);
        assert!(clauses(&ev).is_empty());
        let ev = s.words(&[w("friend", 700 + LONG_PAUSE_MS)], 100);
        assert_eq!(
            clauses(&ev),
            vec![("hi there".into(), true, CloseReason::Pause)]
        );
    }

    #[test]
    fn timer_closes_with_times_and_stats() {
        let mut s = Segmenter::new(800);
        s.words(&run(&["alpha", "bravo", "charlie"], 1000), 5000);
        assert!(s.tick(5500).is_empty());
        let ev = s.tick(5800);
        match &ev[..] {
            [
                Event::Clause {
                    clause,
                    new_row,
                    reason,
                },
            ] => {
                assert_eq!(clause.text, "alpha bravo charlie");
                assert_eq!((clause.start_ms, clause.end_ms, clause.id), (1000, 1400, 0));
                assert!(*new_row);
                assert_eq!(*reason, CloseReason::Timer);
            }
            other => panic!("unexpected {other:?}"),
        }
        assert!(s.tick(9000).is_empty());
        // Next word after a real pause: the timer only beat the detector.
        s.words(&[w("delta", 3000)], 9000);
        assert_eq!(s.stats.timer, 1);
        assert_eq!(s.stats.timer_at_pause, 1);
    }

    #[test]
    fn short_clause_timer_waits_for_long_pause() {
        let mut s = Segmenter::new(800);
        s.words(&[w("so", 0)], 0);
        assert!(s.tick(900).is_empty());
        assert_eq!(clauses(&s.tick(LONG_PAUSE_MS)).len(), 1);
    }

    #[test]
    fn max_words_cuts_at_last_comma() {
        let mut s = Segmenter::new(800);
        let mut texts = vec!["x"; MAX_WORDS];
        texts[9] = "x,";
        texts[15] = "y,";
        let ev = s.words(&run(&texts, 0), 0);
        let c = clauses(&ev);
        assert_eq!(c.len(), 1);
        assert_eq!(c[0].0.split(' ').count(), 16);
        assert!(c[0].0.ends_with("y,"));
        assert_eq!(c[0].2, CloseReason::MaxWords);
        // The rest stays open, on the same row.
        let ev = s.finish();
        assert_eq!(
            clauses(&ev),
            vec![(["x"; 8].join(" "), false, CloseReason::End)]
        );
    }

    #[test]
    fn max_words_without_comma_cuts_all() {
        let mut s = Segmenter::new(800);
        let ws = run(&vec!["x"; MAX_WORDS + 3], 0);
        let ev = s.words(&ws, 0);
        assert_eq!(clauses(&ev)[0].0.split(' ').count(), MAX_WORDS);
    }

    #[test]
    fn end_of_stream_flushes_short_clause() {
        let mut s = Segmenter::new(800);
        s.words(&[w("bye", 0)], 0);
        assert_eq!(
            clauses(&s.finish()),
            vec![("bye".into(), true, CloseReason::End)]
        );
        assert!(s.finish().is_empty());
    }

    #[test]
    fn bare_punctuation_is_not_translated_and_text_is_cleaned() {
        let mut s = Segmenter::new(800);
        let ev = s.words(&[w(".", 0)], 0);
        assert!(clauses(&ev).is_empty());
        let mut s = Segmenter::new(800);
        let ev = s.words(&run(&["Café\u{7}", "costs", "5€…", "\u{1F600}"], 0), 0);
        assert_eq!(
            clauses(&ev),
            vec![("Café costs 5EUR...".into(), true, CloseReason::Punctuation)]
        );
    }

    #[test]
    fn spanish_and_french_marks() {
        let mut s = Segmenter::new(800);
        let ev = s.words(&run(&["¿Dónde", "está", "la", "estación?", "¡Allí"], 0), 0);
        assert_eq!(
            clauses(&ev),
            vec![(
                "¿Dónde está la estación?".into(),
                true,
                CloseReason::Punctuation
            )]
        );
        // Opening marks as separate words attach to the next word; a
        // sentence end inside closing quotes still closes.
        let mut s = Segmenter::new(800);
        let ev = s.words(
            &run(&["Il", "a", "dit", "«", "c'est", "fini.»", "Puis"], 0),
            0,
        );
        assert_eq!(
            clauses(&ev),
            vec![(
                "Il a dit «c'est fini.»".into(),
                true,
                CloseReason::Punctuation
            )]
        );
        let mut s = Segmenter::new(800);
        let ev = s.words(&run(&["Pues", "sí", "¡", "claro!"], 0), 0);
        assert_eq!(
            clauses(&ev),
            vec![("Pues sí ¡claro!".into(), true, CloseReason::Punctuation)]
        );
    }

    #[test]
    fn chinese_full_stop_and_no_spaces() {
        let mut s = Segmenter::new(800);
        let ev = s.words(&run(&["我们", "今天", "开会。", "然后"], 0), 0);
        assert_eq!(
            clauses(&ev),
            vec![("我们今天开会。".into(), true, CloseReason::Punctuation)]
        );
        assert!(sentence_end("好。」") && sentence_end("吗？") && !sentence_end("好，"));
        assert!(clause_end("然后，") && clause_end("一、") && clause_end("sí,"));
    }

    #[test]
    fn long_clause_cuts_at_a_full_width_comma() {
        let mut s = Segmenter::new(800);
        let han = |i: usize| char::from_u32(0x4E00 + i as u32).unwrap_or('字');
        let mut words: Vec<String> = (0..MAX_WORDS + 2)
            .map(|i| format!("{}字", han(i)))
            .collect();
        words[5] = "丙字，".into();
        let texts: Vec<&str> = words.iter().map(String::as_str).collect();
        let c = clauses(&s.words(&run(&texts, 0), 0));
        assert!(c[0].0.ends_with("丙字，"), "{c:?}");
        assert!(!c[0].0.contains(' '), "{c:?}");
    }

    /// Replays S4's real word timings (`long.words.tsv`, 8 min of read
    /// speech, no punctuation) with Nemotron's 560 ms chunking: each word
    /// arrives when the chunk holding its end has been decoded.
    fn replay(max_wait_ms: u64) -> (SegStats, Vec<usize>) {
        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../docs/m0/evidence/S4/long.words.tsv"
        );
        let tsv = std::fs::read_to_string(path).unwrap();
        let words: Vec<Word> = tsv
            .lines()
            .skip(1)
            .filter_map(|l| {
                let mut f = l.split('\t');
                let text = f.next()?.to_string();
                let s: f64 = f.next()?.parse().ok()?;
                let e: f64 = f.next()?.parse().ok()?;
                Some(Word {
                    text,
                    start_ms: (s * 1000.0) as u64,
                    end_ms: (e * 1000.0) as u64,
                })
            })
            .collect();
        assert!(words.len() > 1300);
        const CHUNK: u64 = 560;
        const DECODE: u64 = 60;
        let arrival = |w: &Word| (w.end_ms / CHUNK + 1) * CHUNK + DECODE;
        let mut seg = Segmenter::new(max_wait_ms);
        let mut sizes = Vec::new();
        let collect = |ev: Vec<Event>, sizes: &mut Vec<usize>| {
            for e in ev {
                if let Event::Clause { clause, .. } = e {
                    sizes.push(clause.text.split(' ').count());
                }
            }
        };
        let mut i = 0;
        let mut now = 0;
        while i < words.len() {
            let at = arrival(&words[i]);
            while now + 20 < at {
                now += 20;
                collect(seg.tick(now), &mut sizes);
            }
            now = at;
            let j = words[i..]
                .iter()
                .position(|w| arrival(w) != at)
                .map_or(words.len(), |k| i + k);
            collect(seg.words(&words[i..j], now), &mut sizes);
            i = j;
        }
        collect(seg.finish(), &mut sizes);
        (seg.stats, sizes)
    }

    #[test]
    fn real_timings_close_mostly_on_pauses() {
        let (st, sizes) = replay(800);
        let total = st.total();
        let natural = st.pause + st.punctuation + st.timer_at_pause;
        let pct = |n: u64| n as f64 * 100.0 / total as f64;
        println!(
            "S4 long.words.tsv: {total} clauses; pause {:.0}%, punctuation {:.0}%, timer {:.0}% \
             (at a pause {:.0}%, mid-speech {:.0}%), max-words {:.0}%, end {}; mean {:.1} words",
            pct(st.pause),
            pct(st.punctuation),
            pct(st.timer),
            pct(st.timer_at_pause),
            pct(st.timer - st.timer_at_pause),
            pct(st.max_words),
            st.end,
            sizes.iter().sum::<usize>() as f64 / sizes.len() as f64,
        );
        assert!(sizes.iter().all(|&n| n <= MAX_WORDS));
        // Short clauses only at long pauses or the end: rare.
        let short = sizes.iter().filter(|&&n| n < MIN_WORDS).count();
        assert!(short * 10 < sizes.len(), "{short} short of {}", sizes.len());
        // S6 had ~70% on the timer, mostly mid-speech.
        assert!(pct(natural) >= 70.0, "{st:?}");
        assert!(pct(st.timer - st.timer_at_pause) < 15.0, "{st:?}");
    }
}
