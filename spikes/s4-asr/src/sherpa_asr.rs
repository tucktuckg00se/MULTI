//! sherpa-onnx engines (ONNX Runtime, CUDA EP):
//! - `ParakeetAsr`: offline NeMo transducer (Parakeet TDT v3), used under
//!   LocalAgreement like Whisper;
//! - `OnlineStreamer`: cache-aware streaming transducer (Nemotron 3.5 ASR),
//!   append-only output, committed word by word.

use std::path::Path;
use std::time::Instant;

use anyhow::{Result, anyhow};
use sherpa_onnx::{
    OfflineRecognizer, OfflineRecognizerConfig, OfflineTransducerModelConfig, OnlineRecognizer,
    OnlineRecognizerConfig, OnlineStream, OnlineTransducerModelConfig,
};

use crate::stream::{OfflineAsr, SR, StreamStats, Streamer, VadEvent, VadGate, Word};

fn pick(dir: &Path, stem: &str) -> Result<String> {
    for name in [format!("{stem}.int8.onnx"), format!("{stem}.onnx")] {
        let p = dir.join(&name);
        if p.exists() {
            return Ok(p.to_string_lossy().into_owned());
        }
    }
    Err(anyhow!("no {stem}[.int8].onnx in {}", dir.display()))
}

/// Group sentencepiece tokens into words: a token starting with a space or
/// U+2581 begins a new word. `ts` are token start times (s); `dur` optional.
fn group_words(tokens: &[String], ts: &[f32], dur: Option<&[f32]>) -> Vec<Word> {
    let mut words: Vec<Word> = Vec::new();
    // A bare U+2581 (or " ") token is a word boundary on its own.
    let mut boundary = true;
    for (i, tok) in tokens.iter().enumerate() {
        let t0 = ts.get(i).copied().unwrap_or(0.0) as f64;
        let t1 = t0 + dur.and_then(|d| d.get(i).copied()).map(f64::from).unwrap_or(0.08).max(0.04);
        let starts = tok.starts_with(' ') || tok.starts_with('\u{2581}');
        let clean = tok.trim_start_matches([' ', '\u{2581}']);
        if clean.starts_with('<') && clean.ends_with('>') {
            boundary = true;
            continue; // language tags and other specials
        }
        if clean.is_empty() {
            boundary |= starts;
            continue;
        }
        if starts || boundary || words.is_empty() {
            words.push(Word { text: clean.to_string(), t0, t1 });
        } else if let Some(w) = words.last_mut() {
            w.text.push_str(clean);
            w.t1 = t1;
        }
        boundary = false;
    }
    words
}

// ------------------------------------------------------------ Parakeet

pub struct ParakeetAsr {
    rec: OfflineRecognizer,
}

impl ParakeetAsr {
    pub fn new(dir: &Path, provider: &str, threads: i32) -> Result<Self> {
        let mut c = OfflineRecognizerConfig::default();
        c.model_config.transducer = OfflineTransducerModelConfig {
            encoder: Some(pick(dir, "encoder")?),
            decoder: Some(pick(dir, "decoder")?),
            joiner: Some(pick(dir, "joiner")?),
        };
        c.model_config.tokens = Some(dir.join("tokens.txt").to_string_lossy().into_owned());
        c.model_config.model_type = Some("nemo_transducer".into());
        c.model_config.provider = Some(provider.into());
        c.model_config.num_threads = threads;
        c.decoding_method = Some("greedy_search".into());
        let rec = OfflineRecognizer::create(&c).ok_or_else(|| anyhow!("failed to create Parakeet recognizer"))?;
        Ok(Self { rec })
    }
}

impl OfflineAsr for ParakeetAsr {
    fn transcribe(&mut self, audio: &[f32], _prompt: &str) -> Result<Vec<Word>> {
        let s = self.rec.create_stream();
        s.accept_waveform(SR as i32, audio);
        self.rec.decode(&s);
        let r = s.get_result().ok_or_else(|| anyhow!("no Parakeet result"))?;
        let ts = r.timestamps.unwrap_or_default();
        Ok(group_words(&r.tokens, &ts, r.durations.as_deref()))
    }
}

// ------------------------------------------------------------ Online

pub struct OnlineCfg {
    pub lang: String,
    /// Commit the last (possibly unfinished) word once no token has followed
    /// it for this long.
    pub final_word_ms: u32,
    /// The model's chunk size (decoded audio per decode step).
    pub chunk_ms: u32,
}

pub struct OnlineStreamer {
    rec: OnlineRecognizer,
    cfg: OnlineCfg,
    vad: Option<VadGate>,
    stream: Option<OnlineStream>,
    /// Audio time at which the current stream started.
    stream_t0: f64,
    now_samples: u64,
    /// Words of the current stream already committed.
    done: usize,
    last_partial: Vec<String>,
    /// Decode steps since the token sequence last grew.
    idle_steps: u32,
    /// Text of the last committed word of this stream, as committed.
    committed_last: Option<String>,
    stats: StreamStats,
}

impl OnlineStreamer {
    pub fn new(dir: &Path, provider: &str, threads: i32, cfg: OnlineCfg, vad: Option<VadGate>) -> Result<Self> {
        let mut c = OnlineRecognizerConfig::default();
        c.model_config.transducer = OnlineTransducerModelConfig {
            encoder: Some(pick(dir, "encoder")?),
            decoder: Some(pick(dir, "decoder")?),
            joiner: Some(pick(dir, "joiner")?),
        };
        c.model_config.tokens = Some(dir.join("tokens.txt").to_string_lossy().into_owned());
        c.model_config.provider = Some(provider.into());
        c.model_config.num_threads = threads;
        c.decoding_method = Some("greedy_search".into());
        // Endpointing is off: segmentation comes from the VAD (see step()).
        c.enable_endpoint = false;
        let rec = OnlineRecognizer::create(&c).ok_or_else(|| anyhow!("failed to create online recognizer"))?;
        let mut s = Self {
            rec,
            cfg,
            vad,
            stream: None,
            stream_t0: 0.0,
            now_samples: 0,
            done: 0,
            last_partial: Vec::new(),
            idle_steps: 0,
            committed_last: None,
            stats: StreamStats::default(),
        };
        if s.vad.is_none() {
            s.open_stream(0.0);
        }
        Ok(s)
    }

    fn now(&self) -> f64 {
        self.now_samples as f64 / SR as f64
    }

    fn open_stream(&mut self, t0: f64) {
        let st = self.rec.create_stream();
        st.set_option("language", &self.cfg.lang);
        self.stream = Some(st);
        self.stream_t0 = t0;
        self.done = 0;
        self.last_partial.clear();
        self.idle_steps = 0;
        self.committed_last = None;
    }

    fn feed(&mut self, audio: &[f32]) {
        if let Some(st) = &self.stream {
            st.accept_waveform(SR as i32, audio);
        }
    }

    /// Decode what is ready; commit finished words (or all, if `all`).
    fn step(&mut self, all: bool, out: &mut Vec<Word>) {
        let Some(st) = &self.stream else { return };
        let t = Instant::now();
        let mut n = 0;
        while self.rec.is_ready(st) {
            self.rec.decode(st);
            n += 1;
        }
        if n > 0 {
            self.stats.record(t.elapsed().as_secs_f64(), n);
        }
        let Some(r) = self.rec.get_result(st) else { return };
        let ts = r.timestamps.clone().unwrap_or_default();
        let mut words = group_words(&r.tokens, &ts, None);
        for w in &mut words {
            w.t0 += self.stream_t0;
            w.t1 += self.stream_t0;
        }
        // Stability check: transducer output should be append-only.
        let cur: Vec<String> = words.iter().map(|w| w.text.clone()).collect();
        if std::env::var_os("S4_DEBUG").is_some() && cur != self.last_partial {
            eprintln!("[{:.2}] done={} tokens={:?}", self.now(), self.done, r.tokens);
        }
        // A growing last word is not a revision; a changed one is.
        let changed = self.last_partial.iter().zip(&cur).skip(self.done).filter(|(a, b)| !b.starts_with(a.as_str())).count();
        self.stats.partial_revisions += changed as u64;
        if cur.len() != self.last_partial.len() || cur.last() != self.last_partial.last() {
            self.idle_steps = 0;
        } else {
            self.idle_steps += n as u32;
        }
        self.last_partial = cur;
        // The last word may still grow; commit it once enough decoded audio
        // (not fed audio: the encoder lags by a chunk) produced no token.
        let quiet = self.idle_steps * self.cfg.chunk_ms >= self.cfg.final_word_ms;
        let upto = if all || quiet { words.len() } else { words.len().saturating_sub(1) };
        // A word committed on the quiet rule can still gain a suffix: usually
        // punctuation ("panes" -> "panes,"), which we emit as its own token;
        // letters would mean we split a word (counted as a late suffix).
        if self.done > 0
            && let (Some(w), Some(prev)) = (words.get(self.done - 1), self.committed_last.as_ref())
            && w.text.len() > prev.len()
            && w.text.starts_with(prev.as_str())
        {
            let suffix = w.text[prev.len()..].to_string();
            if suffix.chars().any(char::is_alphanumeric) {
                self.stats.late_suffix += 1;
            }
            out.push(Word { text: suffix, t0: w.t1, t1: w.t1 });
            self.committed_last = Some(w.text.clone());
        }
        if upto > self.done {
            for w in &words[self.done..upto] {
                self.stats.committed += 1;
                out.push(w.clone());
            }
            self.done = upto;
            self.committed_last = words.get(upto - 1).map(|w| w.text.clone());
        }
        // No endpoint/reset: OnlineRecognizer::reset drops audio that was fed
        // but not yet decoded (up to a chunk plus look-ahead), which lost words
        // at every pause, and a stream left in the endpoint state commits
        // half-words. Without VAD we keep one stream; with VAD each speech
        // segment gets its own stream, flushed by input_finished.
    }
}

impl Streamer for OnlineStreamer {
    fn push(&mut self, audio: &[f32]) -> Result<Vec<Word>> {
        let mut out = Vec::new();
        if self.vad.is_none() {
            self.now_samples += audio.len() as u64;
            self.feed(audio);
            self.step(false, &mut out);
            return Ok(out);
        }
        for block in audio.chunks(320) {
            self.now_samples += block.len() as u64;
            let ev = self.vad.as_mut().map(|v| v.feed(block)).unwrap_or(VadEvent::None);
            match ev {
                VadEvent::Start(pre) => {
                    self.stats.vad_segments += 1;
                    let t0 = self.now() - pre.len() as f64 / SR as f64;
                    self.open_stream(t0);
                    self.feed(&pre);
                }
                VadEvent::None => {
                    if self.stream.is_some() {
                        self.feed(block);
                    }
                }
                VadEvent::End => {
                    self.feed(block);
                    if let Some(st) = &self.stream {
                        // Tail padding so the last chunk is complete.
                        st.accept_waveform(SR as i32, &vec![0.0; SR * 6 / 10]);
                        st.input_finished();
                    }
                    self.step(true, &mut out);
                    self.stream = None;
                }
            }
        }
        self.step(false, &mut out);
        Ok(out)
    }

    fn finish(&mut self) -> Result<Vec<Word>> {
        let mut out = Vec::new();
        if let Some(st) = &self.stream {
            st.accept_waveform(SR as i32, &vec![0.0; SR * 6 / 10]);
            st.input_finished();
        }
        self.step(true, &mut out);
        Ok(out)
    }

    fn reset(&mut self) -> Result<()> {
        self.now_samples = 0;
        if let Some(v) = self.vad.as_mut() {
            v.reset();
            self.stream = None;
        } else {
            self.open_stream(0.0);
        }
        Ok(())
    }

    fn stats(&self) -> StreamStats {
        self.stats.clone()
    }

    fn reset_stats(&mut self) {
        self.stats = StreamStats::default();
    }
}
