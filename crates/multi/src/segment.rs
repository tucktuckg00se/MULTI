//! Placeholder segmenter (M1 WP2; WP4 replaces it): committed words go to
//! the source-language lane as they arrive, and a clause for translation
//! closes on punctuation, after a pause of `translate.max_wait_ms`, or at
//! [`MAX_WORDS`] words.

use multi_core::{Clause, Word};
use std::time::{Duration, Instant};

/// A clause never grows beyond this many words.
pub const MAX_WORDS: usize = 24;

#[derive(Clone, Debug, PartialEq)]
pub enum Event {
    /// Text for the source-language lane.
    Source { text: String, new_row: bool },
    /// A closed clause to translate. `new_row`: it starts a new caption row.
    Clause { clause: Clause, new_row: bool },
}

fn is_punct(w: &str) -> bool {
    !w.is_empty() && w.chars().all(|c| !c.is_alphanumeric())
}

/// Appends `w` with a space unless it is bare punctuation.
fn append(s: &mut String, w: &str) {
    if !s.is_empty() && !is_punct(w) {
        s.push(' ');
    }
    s.push_str(w);
}

pub struct Segmenter {
    max_wait: Duration,
    next_id: u64,
    words: Vec<Word>,
    /// The next source text starts a new row (after a sentence end or pause).
    new_row: bool,
    /// Whether the open clause started on a new row.
    clause_row: bool,
    last_word: Option<Instant>,
}

impl Segmenter {
    pub fn new(max_wait: Duration) -> Self {
        Self {
            max_wait,
            next_id: 0,
            words: Vec::new(),
            new_row: true,
            clause_row: true,
            last_word: None,
        }
    }

    fn close(&mut self, sentence_end: bool, out: &mut Vec<Event>) {
        if self.words.is_empty() {
            return;
        }
        let only_punct = self.words.iter().all(|w| is_punct(&w.text));
        let mut text = String::new();
        for w in &self.words {
            append(&mut text, &w.text);
        }
        let (start_ms, end_ms) = (
            self.words.first().map_or(0, |w| w.start_ms),
            self.words.last().map_or(0, |w| w.end_ms),
        );
        self.words.clear();
        // A late punctuation suffix alone is not worth translating.
        if !only_punct {
            out.push(Event::Clause {
                clause: Clause {
                    id: self.next_id,
                    text,
                    start_ms,
                    end_ms,
                },
                new_row: self.clause_row,
            });
            self.next_id += 1;
        }
        self.new_row = sentence_end;
        self.clause_row = sentence_end;
        self.last_word = None;
    }

    /// Handles newly committed words.
    pub fn words(&mut self, ws: &[Word], now: Instant) -> Vec<Event> {
        let mut out = Vec::new();
        let mut chunk = String::new();
        let mut chunk_row = self.new_row;
        for w in ws {
            let t = w.text.trim();
            if t.is_empty() {
                continue;
            }
            if self.words.is_empty() {
                self.clause_row = self.new_row;
            }
            append(&mut chunk, t);
            self.words.push(Word {
                text: t.to_string(),
                ..w.clone()
            });
            let last = t.chars().last().unwrap_or(' ');
            if matches!(last, '.' | '!' | '?' | ',' | ';' | ':') || self.words.len() >= MAX_WORDS {
                out.push(Event::Source {
                    text: std::mem::take(&mut chunk),
                    new_row: chunk_row,
                });
                self.close(matches!(last, '.' | '!' | '?'), &mut out);
                chunk_row = self.new_row;
            }
        }
        if !chunk.is_empty() {
            out.push(Event::Source {
                text: chunk,
                new_row: chunk_row,
            });
            self.new_row = false;
        }
        if !self.words.is_empty() {
            self.last_word = Some(now);
        }
        out
    }

    /// Closes the open clause after a pause of `max_wait`.
    pub fn tick(&mut self, now: Instant) -> Vec<Event> {
        let mut out = Vec::new();
        if self
            .last_word
            .is_some_and(|t| now.saturating_duration_since(t) >= self.max_wait)
        {
            // A pause: the next text starts a new row.
            self.close(true, &mut out);
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn w(text: &str, t: u64) -> Word {
        Word {
            text: text.into(),
            start_ms: t,
            end_ms: t + 100,
        }
    }

    fn clauses(ev: &[Event]) -> Vec<(String, bool)> {
        ev.iter()
            .filter_map(|e| match e {
                Event::Clause { clause, new_row } => Some((clause.text.clone(), *new_row)),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn punctuation_closes_and_source_streams() {
        let mut s = Segmenter::new(Duration::from_millis(800));
        let now = Instant::now();
        let ev = s.words(&[w("Hello", 0), w("there.", 100), w("How", 200)], now);
        assert_eq!(
            ev[0],
            Event::Source {
                text: "Hello there.".into(),
                new_row: true
            }
        );
        assert_eq!(clauses(&ev), vec![("Hello there.".into(), true)]);
        assert_eq!(
            ev.last(),
            Some(&Event::Source {
                text: "How".into(),
                new_row: true
            })
        );
        // Mid-sentence continuation stays on the row.
        let ev = s.words(&[w("are", 300)], now);
        assert_eq!(
            ev,
            vec![Event::Source {
                text: "are".into(),
                new_row: false
            }]
        );
    }

    #[test]
    fn pause_closes_clause_with_times() {
        let mut s = Segmenter::new(Duration::from_millis(800));
        let t0 = Instant::now();
        s.words(&[w("alpha", 1000), w("bravo", 1200)], t0);
        assert!(s.tick(t0 + Duration::from_millis(500)).is_empty());
        let ev = s.tick(t0 + Duration::from_millis(800));
        match &ev[..] {
            [Event::Clause { clause, new_row }] => {
                assert_eq!(clause.text, "alpha bravo");
                assert_eq!((clause.start_ms, clause.end_ms, clause.id), (1000, 1300, 0));
                assert!(*new_row);
            }
            other => panic!("unexpected {other:?}"),
        }
        assert!(s.tick(t0 + Duration::from_secs(5)).is_empty());
    }

    #[test]
    fn long_run_closes_at_max_words() {
        let mut s = Segmenter::new(Duration::from_millis(800));
        let ws: Vec<Word> = (0..MAX_WORDS as u64 + 3).map(|i| w("x", i * 100)).collect();
        let ev = s.words(&ws, Instant::now());
        assert_eq!(clauses(&ev).len(), 1);
    }

    #[test]
    fn bare_punctuation_is_not_translated() {
        let mut s = Segmenter::new(Duration::from_millis(800));
        let ev = s.words(&[w(".", 0)], Instant::now());
        assert!(clauses(&ev).is_empty());
    }
}
