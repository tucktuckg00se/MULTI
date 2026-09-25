//! Segmenter thread: forwards committed English words to the EN lane at once
//! (one caption push per ASR batch), and closes a clause on punctuation or
//! [`CLOSE_AFTER`] after the last word; closed clauses go to the translators.

use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
use std::time::{Duration, Instant};

use s2_gst_pipe::stamps::wall_ns;

use crate::asr::Word;
use crate::captioner::Line;

const CLOSE_AFTER: Duration = Duration::from_millis(800);
const MAX_WORDS: usize = 24;

/// A finished source clause.
#[derive(Clone, Debug)]
pub struct Clause {
    pub id: u64,
    pub text: String,
    pub closed_at: Instant,
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

struct Seg {
    id: u64,
    words: Vec<String>,
    /// The next EN text starts a new row (after a sentence end or a pause).
    new_row: bool,
    last_word: Option<Instant>,
    en: Option<Sender<Line>>,
    mt: Vec<Sender<Clause>>,
}

impl Seg {
    fn close(&mut self, sentence_end: bool) {
        if self.words.is_empty() {
            return;
        }
        let only_punct = self.words.iter().all(|w| is_punct(w));
        let mut text = String::new();
        for w in self.words.drain(..) {
            append(&mut text, &w);
        }
        let c = Clause { id: self.id, text, closed_at: Instant::now() };
        // A late punctuation suffix alone is not worth translating.
        if !only_punct {
            for tx in &self.mt {
                let _ = tx.send(c.clone());
            }
        }
        self.id += 1;
        self.new_row = sentence_end;
        self.last_word = None;
    }

    fn words(&mut self, ws: Vec<Word>) {
        let mut chunk = String::new();
        let mut chunk_row = self.new_row;
        let flush = |s: &mut Self, chunk: &mut String, row: bool| {
            if chunk.is_empty() {
                return;
            }
            if let Some(tx) = &s.en {
                // No leading space: in roll-up, tttocea708 already puts one
                // between consecutive text buffers.
                let _ = tx.send(Line { lane: 0, text: chunk.clone(), new_row: row, clause: s.id, ready_ns: wall_ns() });
            }
            chunk.clear();
        };
        for w in ws {
            let t = w.text.trim();
            if t.is_empty() {
                continue;
            }
            append(&mut chunk, t);
            self.words.push(t.to_string());
            let last = t.chars().last().unwrap_or(' ');
            if matches!(last, '.' | '!' | '?' | ',' | ';' | ':') || self.words.len() >= MAX_WORDS {
                flush(self, &mut chunk, chunk_row);
                self.close(matches!(last, '.' | '!' | '?'));
                chunk_row = self.new_row;
            }
        }
        if !chunk.is_empty() {
            flush(self, &mut chunk, chunk_row);
            self.new_row = false;
        }
        if !self.words.is_empty() {
            self.last_word = Some(Instant::now());
        }
    }
}

pub fn spawn(
    rx: Receiver<Vec<Word>>,
    en: Option<Sender<Line>>,
    mt: Vec<Sender<Clause>>,
) -> anyhow::Result<std::thread::JoinHandle<()>> {
    Ok(std::thread::Builder::new().name("segment".into()).spawn(move || {
        let mut s = Seg { id: 0, words: Vec::new(), new_row: true, last_word: None, en, mt };
        loop {
            let wait = s.last_word.map_or(Duration::from_secs(1), |t| CLOSE_AFTER.saturating_sub(t.elapsed()));
            match rx.recv_timeout(wait) {
                Ok(ws) => s.words(ws),
                Err(RecvTimeoutError::Timeout) => {
                    if s.last_word.is_some_and(|t| t.elapsed() >= CLOSE_AFTER) {
                        // A pause: the next text starts a new row.
                        s.close(true);
                    }
                }
                Err(RecvTimeoutError::Disconnected) => break,
            }
        }
    })?)
}
