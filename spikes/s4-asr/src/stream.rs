//! Streaming layer shared by every engine: the `Word` type, the `Streamer`
//! trait, a Silero VAD gate, and the LocalAgreement-n commit policy that turns
//! any offline (whole-buffer) recogniser into a streaming one.

use std::collections::VecDeque;
use std::time::Instant;

use anyhow::Result;

pub const SR: usize = 16_000;

/// One committed word. Times are audio seconds from the stream start.
#[derive(Clone, Debug)]
pub struct Word {
    pub text: String,
    pub t0: f64,
    pub t1: f64,
}

/// Counters every streamer keeps; printed in the run summary.
#[derive(Default, Clone, Debug, serde::Serialize)]
pub struct StreamStats {
    /// Model invocations (whole-buffer passes, or decode calls for online models).
    pub passes: u64,
    /// Wall time spent inside the model.
    pub compute_s: f64,
    /// Longest single pass.
    pub max_pass_ms: f64,
    /// Words committed.
    pub committed: u64,
    /// Unstable-tail words that changed between successive passes (what a
    /// viewer would see flicker if partials were displayed).
    pub partial_revisions: u64,
    /// Partial words shown in total (denominator for the revision rate).
    pub partial_words: u64,
    /// VAD speech segments seen.
    pub vad_segments: u64,
    /// Forced commits because the buffer hit its hard limit.
    pub forced_commits: u64,
    /// Online model: a word committed early later grew letters (split word).
    pub late_suffix: u64,
    /// Per-pass model time in ms (summarised as percentiles in the output).
    #[serde(skip)]
    pub pass_ms: Vec<f32>,
}

impl StreamStats {
    pub fn record(&mut self, dt_s: f64, n: u64) {
        self.passes += n;
        self.compute_s += dt_s;
        let ms = dt_s * 1000.0 / n.max(1) as f64;
        self.max_pass_ms = self.max_pass_ms.max(ms);
        self.pass_ms.push(ms as f32);
    }

    /// Nearest-rank percentile of pass times, ms.
    pub fn pass_pct(&self, q: f64) -> f64 {
        let mut v = self.pass_ms.clone();
        if v.is_empty() {
            return 0.0;
        }
        v.sort_by(f32::total_cmp);
        let i = ((q / 100.0 * v.len() as f64).ceil() as usize).clamp(1, v.len()) - 1;
        v[i] as f64
    }
}

pub trait Streamer {
    /// Feed newly arrived 16 kHz mono audio; returns words committed now.
    fn push(&mut self, audio: &[f32]) -> Result<Vec<Word>>;
    /// End of input: commit whatever is left.
    fn finish(&mut self) -> Result<Vec<Word>>;
    /// Forget all state (between independent test utterances).
    fn reset(&mut self) -> Result<()>;
    fn stats(&self) -> StreamStats;
    fn reset_stats(&mut self);
}

/// A recogniser that transcribes a whole buffer at once (Whisper, Parakeet TDT).
pub trait OfflineAsr {
    /// Words with times relative to the buffer start.
    fn transcribe(&mut self, audio: &[f32], prompt: &str) -> Result<Vec<Word>>;
}

/// Lowercase, keep letters/digits/apostrophes only: used to compare hypotheses.
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
    let (n, m) = (c.len(), h.len());
    let mut d = vec![vec![0usize; m + 1]; n + 1];
    for (i, row) in d.iter_mut().enumerate() {
        row[0] = i;
    }
    for (j, v) in d[0].iter_mut().enumerate() {
        *v = j;
    }
    for i in 1..=n {
        for j in 1..=m {
            let sub = d[i - 1][j - 1] + usize::from(c[i - 1] != h[j - 1]);
            d[i][j] = sub.min(d[i - 1][j] + 1).min(d[i][j - 1] + 1);
        }
    }
    let mut best = 0;
    for j in 0..=m {
        if d[n][j] <= d[n][best] {
            best = j;
        }
    }
    best
}

// ---------------------------------------------------------------- VAD gate

pub struct VadCfg {
    pub model: String,
    pub threshold: f32,
    pub min_silence_s: f32,
    pub min_speech_s: f32,
}

/// Silero VAD (via sherpa-onnx, CPU) tracking whether we are inside speech.
pub struct VadGate {
    vad: sherpa_onnx::VoiceActivityDetector,
    pub in_speech: bool,
    preroll: VecDeque<f32>,
    preroll_len: usize,
}

pub enum VadEvent {
    None,
    Start(Vec<f32>),
    End,
}

impl VadGate {
    pub fn new(cfg: &VadCfg) -> Result<Self> {
        let c = sherpa_onnx::VadModelConfig {
            silero_vad: sherpa_onnx::SileroVadModelConfig {
                model: Some(cfg.model.clone()),
                threshold: cfg.threshold,
                min_silence_duration: cfg.min_silence_s,
                min_speech_duration: cfg.min_speech_s,
                window_size: 512,
                max_speech_duration: 30.0,
            },
            sample_rate: SR as i32,
            num_threads: 1,
            provider: Some("cpu".into()),
            ..Default::default()
        };
        let vad = sherpa_onnx::VoiceActivityDetector::create(&c, 60.0)
            .ok_or_else(|| anyhow::anyhow!("failed to create Silero VAD from {}", cfg.model))?;
        // Silero only triggers after min_speech plus two windows; keep that much
        // audio (plus margin) so the first word is not clipped.
        let preroll_len = ((cfg.min_speech_s + 0.35) * SR as f32) as usize;
        Ok(Self { vad, in_speech: false, preroll: VecDeque::new(), preroll_len })
    }

    /// Feed one small block (e.g. 20 ms). Returns a state transition, if any.
    /// On `Start`, the returned vector is the pre-roll *including* this block.
    pub fn feed(&mut self, block: &[f32]) -> VadEvent {
        self.vad.accept_waveform(block);
        while !self.vad.is_empty() {
            self.vad.pop(); // we only use the live state; drop stored segments
        }
        let speech = self.vad.detected();
        if !self.in_speech {
            self.preroll.extend(block.iter().copied());
            while self.preroll.len() > self.preroll_len {
                self.preroll.pop_front();
            }
        }
        match (self.in_speech, speech) {
            (false, true) => {
                self.in_speech = true;
                let pre: Vec<f32> = self.preroll.drain(..).collect();
                VadEvent::Start(pre)
            }
            (true, false) => {
                self.in_speech = false;
                VadEvent::End
            }
            _ => VadEvent::None,
        }
    }

    pub fn reset(&mut self) {
        self.vad.reset();
        self.in_speech = false;
        self.preroll.clear();
    }
}

// ------------------------------------------------ LocalAgreement streamer

pub struct LaCfg {
    /// Run a pass once this much new audio has arrived.
    pub chunk_ms: u32,
    /// LocalAgreement-n: commit the prefix shared by the last n hypotheses.
    pub passes: usize,
    /// Trim the buffer at a committed sentence end once it is longer than this.
    pub trim_s: f64,
    /// Hard limit: force-commit and clear (Whisper's window is 30 s).
    pub max_buf_s: f64,
}

pub struct LaStreamer<A: OfflineAsr> {
    asr: A,
    cfg: LaCfg,
    vad: Option<VadGate>,
    buf: Vec<f32>,
    buf_t0: f64,
    /// Audio time at the end of everything pushed so far.
    now_samples: u64,
    since_pass: usize,
    history: VecDeque<Word>,
    last_t1: f64,
    hyps: VecDeque<Vec<Word>>,
    active: bool,
    stats: StreamStats,
}

impl<A: OfflineAsr> LaStreamer<A> {
    pub fn new(asr: A, cfg: LaCfg, vad: Option<VadGate>) -> Self {
        let active = vad.is_none();
        Self {
            asr,
            cfg,
            vad,
            buf: Vec::new(),
            buf_t0: 0.0,
            now_samples: 0,
            since_pass: 0,
            history: VecDeque::new(),
            last_t1: 0.0,
            hyps: VecDeque::new(),
            active,
            stats: StreamStats::default(),
        }
    }

    fn now(&self) -> f64 {
        self.now_samples as f64 / SR as f64
    }

    /// Committed text that lies before the buffer: used as Whisper's prompt.
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

    fn run_model(&mut self) -> Result<Vec<Word>> {
        let prompt = self.prompt();
        let t = Instant::now();
        let mut words = self.asr.transcribe(&self.buf, &prompt)?;
        self.stats.record(t.elapsed().as_secs_f64(), 1);
        for w in &mut words {
            w.t0 += self.buf_t0;
            w.t1 += self.buf_t0;
        }
        // The buffer still holds audio of words already committed; the new
        // hypothesis should start with them. Skip them by text alignment, not
        // by time (whisper.cpp token times are too coarse for that).
        let done: Vec<String> = self
            .history
            .iter()
            .filter(|w| w.t1 > self.buf_t0 + 0.05)
            .map(|w| norm(&w.text))
            .collect();
        let h: Vec<String> = words.iter().map(|w| norm(&w.text)).collect();
        let k = skip_prefix(&done, &h);
        let hyp: Vec<Word> = words.into_iter().skip(k).filter(|w| !norm(&w.text).is_empty()).collect();
        Ok(hyp)
    }

    fn commit(&mut self, words: Vec<Word>, out: &mut Vec<Word>) {
        for w in words {
            self.last_t1 = self.last_t1.max(w.t1);
            self.history.push_back(w.clone());
            if self.history.len() > 200 {
                self.history.pop_front();
            }
            self.stats.committed += 1;
            out.push(w);
        }
    }

    /// One regular pass: LocalAgreement commit, then buffer trimming.
    fn pass(&mut self, out: &mut Vec<Word>) -> Result<()> {
        self.since_pass = 0;
        let hyp = self.run_model()?;
        if let Some(prev) = self.hyps.back() {
            let changed = prev
                .iter()
                .zip(&hyp)
                .filter(|(a, b)| norm(&a.text) != norm(&b.text))
                .count();
            self.stats.partial_revisions += changed as u64;
        }
        self.stats.partial_words += hyp.len() as u64;
        self.hyps.push_back(hyp);
        while self.hyps.len() > self.cfg.passes {
            self.hyps.pop_front();
        }
        if self.hyps.len() >= self.cfg.passes {
            let cur = &self.hyps[self.hyps.len() - 1];
            let mut n = cur.len();
            for h in self.hyps.iter() {
                let lcp = h.iter().zip(cur).take_while(|(a, b)| norm(&a.text) == norm(&b.text)).count();
                n = n.min(lcp);
            }
            if n > 0 {
                let committed: Vec<Word> = cur[..n].to_vec();
                for h in self.hyps.iter_mut() {
                    h.drain(..n.min(h.len()));
                }
                self.commit(committed, out);
            }
        }
        self.trim(out)
    }

    fn trim(&mut self, out: &mut Vec<Word>) -> Result<()> {
        let dur = self.buf.len() as f64 / SR as f64;
        if dur > self.cfg.max_buf_s {
            self.stats.forced_commits += 1;
            return self.final_pass(out);
        }
        if dur <= self.cfg.trim_s {
            return Ok(());
        }
        // Cut after the latest committed sentence end inside the buffer; if
        // there is none, after the latest committed word once past 2/3 of max.
        let in_buf = |w: &&Word| w.t1 > self.buf_t0;
        let sentence = self
            .history
            .iter()
            .filter(in_buf)
            .filter(|w| w.text.ends_with(['.', '?', '!']))
            .map(|w| w.t1)
            .next_back();
        let cut = sentence.or_else(|| {
            (dur > self.cfg.max_buf_s * 2.0 / 3.0)
                .then(|| self.history.iter().filter(in_buf).map(|w| w.t1).next_back())
                .flatten()
        });
        if let Some(cut) = cut {
            let n = (((cut - self.buf_t0) * SR as f64) as usize).min(self.buf.len());
            self.buf.drain(..n);
            self.buf_t0 += n as f64 / SR as f64;
        }
        Ok(())
    }

    /// Commit the whole current hypothesis (end of speech or hard limit).
    fn final_pass(&mut self, out: &mut Vec<Word>) -> Result<()> {
        if self.buf.len() > SR / 10 {
            let hyp = self.run_model()?;
            self.commit(hyp, out);
        }
        self.hyps.clear();
        self.buf.clear();
        self.buf_t0 = self.now();
        self.since_pass = 0;
        Ok(())
    }
}

impl<A: OfflineAsr> Streamer for LaStreamer<A> {
    fn push(&mut self, audio: &[f32]) -> Result<Vec<Word>> {
        let mut out = Vec::new();
        for block in audio.chunks(320) {
            self.now_samples += block.len() as u64;
            match self.vad.as_mut().map(|v| v.feed(block)) {
                None => {
                    self.buf.extend_from_slice(block);
                    self.since_pass += block.len();
                }
                Some(VadEvent::Start(pre)) => {
                    self.stats.vad_segments += 1;
                    self.active = true;
                    self.buf = pre;
                    self.buf_t0 = self.now() - self.buf.len() as f64 / SR as f64;
                    self.since_pass = self.buf.len();
                }
                Some(VadEvent::None) if self.active => {
                    self.buf.extend_from_slice(block);
                    self.since_pass += block.len();
                }
                Some(VadEvent::None) => {}
                Some(VadEvent::End) => {
                    self.buf.extend_from_slice(block);
                    self.active = false;
                    self.final_pass(&mut out)?;
                }
            }
        }
        if self.active && self.since_pass >= self.cfg.chunk_ms as usize * SR / 1000 {
            self.pass(&mut out)?;
        }
        Ok(out)
    }

    fn finish(&mut self) -> Result<Vec<Word>> {
        let mut out = Vec::new();
        self.final_pass(&mut out)?;
        Ok(out)
    }

    fn reset(&mut self) -> Result<()> {
        self.buf.clear();
        self.buf_t0 = 0.0;
        self.now_samples = 0;
        self.since_pass = 0;
        self.history.clear();
        self.last_t1 = 0.0;
        self.hyps.clear();
        if let Some(v) = self.vad.as_mut() {
            v.reset();
        }
        self.active = self.vad.is_none();
        Ok(())
    }

    fn stats(&self) -> StreamStats {
        self.stats.clone()
    }

    fn reset_stats(&mut self) {
        self.stats = StreamStats::default();
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
}
