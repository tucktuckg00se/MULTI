//! LocalAgreement-n streaming for a recogniser that transcribes a whole
//! buffer at once (Whisper), ported from the S4 spike (`spikes/s4-asr`,
//! `stream.rs`).
//!
//! While the VAD reports speech, the buffer is re-transcribed every
//! `chunk_ms`; the prefix the last `passes` hypotheses agree on is
//! committed. Words already committed are skipped by text alignment
//! (whisper.cpp timestamps are too coarse to skip them by time). The buffer
//! is trimmed at a committed sentence end once it passes `trim_s`, and
//! everything is committed at the end of speech or at `max_buf_s`.

use crate::engine::{Engine, RawWord, SR};
use crate::guard::LoopFilter;
use crate::vad::{VadEvent, VadGate};
use anyhow::Result;
use std::collections::VecDeque;

/// A recogniser that transcribes a whole buffer at once.
pub trait Transcriber {
    /// Words with times relative to the buffer start. `prompt` is the
    /// committed text just before the buffer.
    fn transcribe(&mut self, audio: &[f32], prompt: &str) -> Result<Vec<RawWord>>;
    /// Logs what was filtered so far (hallucination guard); `loop_words`:
    /// committed words dropped as loops.
    fn log_stats(&mut self, _loop_words: u64) {}
}

#[derive(Clone, Copy, Debug)]
pub struct LaCfg {
    /// Run a pass once this much new audio has arrived.
    pub chunk_ms: u32,
    /// LocalAgreement-n: commit the prefix shared by the last n hypotheses.
    pub passes: usize,
    /// Trim the buffer at a committed sentence end once it is longer.
    pub trim_s: f64,
    /// Hard limit: force-commit and clear (Whisper's window is 30 s).
    pub max_buf_s: f64,
}

impl Default for LaCfg {
    fn default() -> Self {
        Self {
            chunk_ms: 1000,
            passes: 2,
            trim_s: 12.0,
            max_buf_s: 25.0,
        }
    }
}

/// Lowercase, letters/digits/apostrophes only: used to compare hypotheses.
pub fn norm(w: &str) -> String {
    w.chars()
        .filter(|c| c.is_alphanumeric() || *c == '\'')
        .flat_map(char::to_lowercase)
        .collect()
}

/// Length of the prefix of `h` that best matches all of `c` (word-level edit
/// distance; ties go to the longer prefix).
pub fn skip_prefix(c: &[String], h: &[String]) -> usize {
    if c.is_empty() {
        return 0;
    }
    let m = h.len();
    let mut prev: Vec<usize> = (0..=m).collect();
    for (i, ci) in c.iter().enumerate() {
        let mut cur = vec![i + 1; m + 1];
        for j in 1..=m {
            let sub = prev[j - 1] + usize::from(*ci != h[j - 1]);
            cur[j] = sub.min(prev[j] + 1).min(cur[j - 1] + 1);
        }
        prev = cur;
    }
    let mut best = 0;
    for j in 0..=m {
        if prev[j] <= prev[best] {
            best = j;
        }
    }
    best
}

/// The LocalAgreement state for one audio stream, driven by VAD events.
pub struct LocalAgreement<T: Transcriber> {
    asr: T,
    cfg: LaCfg,
    buf: Vec<f32>,
    buf_t0: f64,
    since_pass: usize,
    history: VecDeque<RawWord>,
    hyps: VecDeque<Vec<RawWord>>,
    active: bool,
    /// Drops committed words that continue a loop (hallucination guard).
    loops: LoopFilter,
}

impl<T: Transcriber> LocalAgreement<T> {
    pub fn new(asr: T, cfg: LaCfg) -> Self {
        Self {
            asr,
            cfg,
            buf: Vec::new(),
            buf_t0: 0.0,
            since_pass: 0,
            history: VecDeque::new(),
            hyps: VecDeque::new(),
            active: false,
            loops: LoopFilter::default(),
        }
    }

    /// Words dropped as loops so far.
    pub fn loops_dropped(&self) -> u64 {
        self.loops.dropped
    }

    pub fn asr_mut(&mut self) -> &mut T {
        &mut self.asr
    }

    /// Speech started at `t0` (seconds); `pre` is the audio from `t0` on.
    pub fn start(&mut self, pre: Vec<f32>, t0: f64) {
        self.active = true;
        self.buf = pre;
        self.buf_t0 = t0;
        self.since_pass = self.buf.len();
        self.hyps.clear();
    }

    /// Audio inside speech (ignored outside).
    pub fn append(&mut self, block: &[f32]) {
        if self.active {
            self.buf.extend_from_slice(block);
            self.since_pass += block.len();
        }
    }

    /// Runs a pass if enough new audio arrived.
    pub fn maybe_pass(&mut self, out: &mut Vec<RawWord>) -> Result<()> {
        if self.active && self.since_pass >= self.cfg.chunk_ms as usize * SR / 1000 {
            self.pass(out)?;
        }
        Ok(())
    }

    /// Speech ended at `now`: commit the whole current hypothesis.
    pub fn end(&mut self, now: f64, out: &mut Vec<RawWord>) -> Result<()> {
        let r = self.final_pass(out);
        self.active = false;
        self.buf.clear();
        self.buf_t0 = now;
        r
    }

    pub fn reset(&mut self) {
        self.buf.clear();
        self.buf_t0 = 0.0;
        self.since_pass = 0;
        self.history.clear();
        self.hyps.clear();
        self.active = false;
        self.loops.reset();
    }

    /// Committed text that lies before the buffer: the model's prompt.
    fn prompt(&self) -> String {
        let mut words: Vec<&str> = self
            .history
            .iter()
            .filter(|w| w.t1 <= self.buf_t0 + 0.01)
            .map(|w| w.text.as_str())
            .collect();
        let n = words.len();
        if n > 40 {
            words.drain(..n - 40);
        }
        words.join(" ")
    }

    fn run_model(&mut self) -> Result<Vec<RawWord>> {
        let prompt = self.prompt();
        let mut words = self.asr.transcribe(&self.buf, &prompt)?;
        for w in &mut words {
            w.t0 += self.buf_t0;
            w.t1 += self.buf_t0;
        }
        // The buffer still holds audio of words already committed; the new
        // hypothesis should start with them. Skip them by text alignment.
        let done: Vec<String> = self
            .history
            .iter()
            .filter(|w| w.t1 > self.buf_t0 + 0.05)
            .map(|w| norm(&w.text))
            .collect();
        let h: Vec<String> = words.iter().map(|w| norm(&w.text)).collect();
        let k = skip_prefix(&done, &h);
        Ok(words
            .into_iter()
            .skip(k)
            .filter(|w| !norm(&w.text).is_empty())
            .collect())
    }

    /// Words dropped as loops still go into the history, so the next pass
    /// skips them by alignment instead of committing them again.
    fn commit(&mut self, words: Vec<RawWord>, out: &mut Vec<RawWord>) {
        for w in &words {
            self.history.push_back(w.clone());
            if self.history.len() > 200 {
                self.history.pop_front();
            }
        }
        out.extend(self.loops.admit(words));
    }

    /// One regular pass: LocalAgreement commit, then buffer trimming.
    fn pass(&mut self, out: &mut Vec<RawWord>) -> Result<()> {
        self.since_pass = 0;
        let hyp = self.run_model()?;
        self.hyps.push_back(hyp);
        while self.hyps.len() > self.cfg.passes {
            self.hyps.pop_front();
        }
        if self.hyps.len() >= self.cfg.passes
            && let Some(cur) = self.hyps.back()
        {
            let mut n = cur.len();
            for h in &self.hyps {
                let lcp = h
                    .iter()
                    .zip(cur)
                    .take_while(|(a, b)| norm(&a.text) == norm(&b.text))
                    .count();
                n = n.min(lcp);
            }
            if n > 0 {
                let committed: Vec<RawWord> = cur[..n].to_vec();
                for h in self.hyps.iter_mut() {
                    h.drain(..n.min(h.len()));
                }
                self.commit(committed, out);
            }
        }
        self.trim(out)
    }

    fn trim(&mut self, out: &mut Vec<RawWord>) -> Result<()> {
        let dur = self.buf.len() as f64 / SR as f64;
        if dur > self.cfg.max_buf_s {
            let now = self.buf_t0 + dur;
            let r = self.final_pass(out);
            self.buf.clear();
            self.buf_t0 = now;
            return r;
        }
        if dur <= self.cfg.trim_s {
            return Ok(());
        }
        // Cut after the latest committed sentence end inside the buffer; if
        // there is none, after the latest committed word once past 2/3 of max.
        let t0 = self.buf_t0;
        let in_buf = |w: &&RawWord| w.t1 > t0;
        let sentence = self
            .history
            .iter()
            .filter(in_buf)
            .filter(|w| w.text.ends_with(['.', '?', '!', '。', '？', '！']))
            .map(|w| w.t1)
            .next_back();
        let cut = sentence.or_else(|| {
            (dur > self.cfg.max_buf_s * 2.0 / 3.0)
                .then(|| self.history.iter().filter(in_buf).map(|w| w.t1).next_back())
                .flatten()
        });
        if let Some(cut) = cut {
            let n = (((cut - self.buf_t0) * SR as f64).max(0.0) as usize).min(self.buf.len());
            self.buf.drain(..n);
            self.buf_t0 += n as f64 / SR as f64;
        }
        Ok(())
    }

    /// Commits the whole current hypothesis (end of speech or hard limit).
    fn final_pass(&mut self, out: &mut Vec<RawWord>) -> Result<()> {
        self.since_pass = 0;
        let hyp = if self.buf.len() > SR / 10 {
            self.run_model()
        } else {
            Ok(Vec::new())
        };
        self.hyps.clear();
        self.commit(hyp?, out);
        Ok(())
    }
}

/// A LocalAgreement engine behind the VAD: nothing is decoded outside speech.
pub struct LaEngine<T: Transcriber> {
    la: LocalAgreement<T>,
    vad: VadGate,
    now_samples: u64,
}

impl<T: Transcriber> LaEngine<T> {
    pub fn new(asr: T, cfg: LaCfg, vad: VadGate) -> Self {
        Self {
            la: LocalAgreement::new(asr, cfg),
            vad,
            now_samples: 0,
        }
    }

    fn now(&self) -> f64 {
        self.now_samples as f64 / SR as f64
    }
}

impl<T: Transcriber> Engine for LaEngine<T> {
    fn warm_up(&mut self) -> Result<()> {
        self.la.asr_mut().transcribe(&vec![0.0; SR * 2], "")?;
        self.la.reset();
        self.vad.reset();
        self.now_samples = 0;
        Ok(())
    }

    fn push(&mut self, audio: &[f32]) -> Result<Vec<RawWord>> {
        let mut out = Vec::new();
        for block in audio.chunks(320) {
            self.now_samples += block.len() as u64;
            match self.vad.feed(block) {
                VadEvent::Start(pre) => {
                    let t0 = self.now() - pre.len() as f64 / SR as f64;
                    self.la.start(pre, t0);
                }
                VadEvent::None => self.la.append(block),
                VadEvent::End => {
                    self.la.append(block);
                    let now = self.now();
                    self.la.end(now, &mut out)?;
                    let loops = self.la.loops_dropped();
                    self.la.asr_mut().log_stats(loops);
                }
            }
        }
        self.la.maybe_pass(&mut out)?;
        Ok(out)
    }

    fn finish(&mut self) -> Result<Vec<RawWord>> {
        let mut out = Vec::new();
        let now = self.now();
        self.la.end(now, &mut out)?;
        let loops = self.la.loops_dropped();
        self.la.asr_mut().log_stats(loops);
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(s: &str) -> Vec<String> {
        s.split_whitespace().map(String::from).collect()
    }

    #[test]
    fn skip_prefix_cases() {
        assert_eq!(skip_prefix(&v(""), &v("a b")), 0);
        assert_eq!(skip_prefix(&v("a b"), &v("a b c d")), 2);
        assert_eq!(skip_prefix(&v("a b"), &v("a x c d")), 2);
        assert_eq!(skip_prefix(&v("a b c"), &v("b c d")), 2);
        assert_eq!(skip_prefix(&v("x a b"), &v("q a b c")), 3);
    }

    #[test]
    fn norm_strips_punct() {
        assert_eq!(norm("Brick."), "brick");
        assert_eq!(norm("don't,"), "don't");
    }

    /// Returns scripted hypotheses (one per call), each word 0.3 s long.
    struct Script(VecDeque<&'static str>);

    impl Transcriber for Script {
        fn transcribe(&mut self, _audio: &[f32], _prompt: &str) -> Result<Vec<RawWord>> {
            let s = self.0.pop_front().unwrap_or("");
            Ok(s.split_whitespace()
                .enumerate()
                .map(|(i, w)| RawWord {
                    text: w.into(),
                    t0: i as f64 * 0.3,
                    t1: i as f64 * 0.3 + 0.3,
                })
                .collect())
        }
    }

    fn texts(w: &[RawWord]) -> Vec<&str> {
        w.iter().map(|w| w.text.as_str()).collect()
    }

    #[test]
    fn commits_what_two_passes_agree_on() -> Result<()> {
        let script = Script(VecDeque::from([
            "the cat",
            "the cat sat on",
            "the cat sat on the mat",
            "the cat sat on the mat.",
        ]));
        let cfg = LaCfg {
            chunk_ms: 500,
            ..LaCfg::default()
        };
        let mut la = LocalAgreement::new(script, cfg);
        let mut out = Vec::new();
        la.start(Vec::new(), 10.0);
        let half = vec![0.0; SR / 2];
        la.append(&half);
        la.maybe_pass(&mut out)?; // "the cat": nothing to agree with yet
        assert!(out.is_empty());
        la.append(&half);
        la.maybe_pass(&mut out)?;
        assert_eq!(texts(&out), ["the", "cat"]);
        assert!((out[0].t0 - 10.0).abs() < 1e-9, "times are shifted by t0");
        la.append(&half);
        la.maybe_pass(&mut out)?;
        assert_eq!(texts(&out), ["the", "cat", "sat", "on"]);
        // End of speech: the rest of the last hypothesis is committed.
        la.end(11.5, &mut out)?;
        assert_eq!(texts(&out), ["the", "cat", "sat", "on", "the", "mat."]);
        // Nothing is decoded outside speech.
        la.append(&half);
        la.maybe_pass(&mut out)?;
        assert_eq!(out.len(), 6);
        Ok(())
    }

    #[test]
    fn too_little_new_audio_runs_no_pass() -> Result<()> {
        let mut la = LocalAgreement::new(Script(VecDeque::from(["x"])), LaCfg::default());
        let mut out = Vec::new();
        la.start(Vec::new(), 0.0);
        la.append(&[0.0; 320]);
        la.maybe_pass(&mut out)?;
        la.end(0.02, &mut out)?; // below 100 ms: no final pass either
        assert!(out.is_empty());
        Ok(())
    }
}
