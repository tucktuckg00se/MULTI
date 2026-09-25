//! Streaming recognition, ported from the S4 spike (`spikes/s4-asr`):
//! Nemotron 3.5 Streaming (cache-aware transducer) through sherpa-onnx, with
//! Silero VAD opening one recognizer stream per speech segment.
//!
//! Why one stream per segment (S4): `OnlineRecognizer::reset` drops audio fed
//! but not yet decoded, which lost words at every pause, and a stream left in
//! the endpoint state commits half-words. Endpointing is off; the VAD decides.
//!
//! Output is append-only: a word is committed once the next word has started,
//! or once `final_word_ms` of decoded audio produced no new token.

use anyhow::{Result, anyhow};
use sherpa_onnx::{
    OnlineRecognizer, OnlineRecognizerConfig, OnlineStream, OnlineTransducerModelConfig,
    SileroVadModelConfig, VadModelConfig, VoiceActivityDetector,
};
use std::collections::VecDeque;
use std::path::{Path, PathBuf};

pub const SR: usize = 16_000;

/// A committed word; times are seconds since the first sample pushed.
#[derive(Clone, Debug, PartialEq)]
pub struct RawWord {
    pub text: String,
    pub t0: f64,
    pub t1: f64,
}

pub struct EngineCfg {
    pub model_dir: PathBuf,
    pub vad_model: PathBuf,
    /// ONNX Runtime provider: `cuda` or `cpu`.
    pub provider: String,
    pub threads: i32,
    /// Language prompt, e.g. `en`.
    pub lang: String,
    /// Chunk size the model was exported with.
    pub chunk_ms: u32,
    pub final_word_ms: u32,
    pub vad_threshold: f32,
}

fn pick(dir: &Path, stem: &str) -> Result<String> {
    for name in [format!("{stem}.int8.onnx"), format!("{stem}.onnx")] {
        let p = dir.join(&name);
        if p.exists() {
            return Ok(p.to_string_lossy().into_owned());
        }
    }
    Err(anyhow!("no {stem}[.int8].onnx in {}", dir.display()))
}

/// Groups sentencepiece tokens into words: a token starting with a space or
/// U+2581 begins a new word. `ts` are token start times in seconds.
pub fn group_words(tokens: &[String], ts: &[f32]) -> Vec<RawWord> {
    const TOKEN_S: f64 = 0.08;
    let mut words: Vec<RawWord> = Vec::new();
    let mut boundary = true;
    for (i, tok) in tokens.iter().enumerate() {
        let t0 = f64::from(ts.get(i).copied().unwrap_or(0.0));
        let t1 = t0 + TOKEN_S;
        let starts = tok.starts_with(' ') || tok.starts_with('\u{2581}');
        let clean = tok.trim_start_matches([' ', '\u{2581}']);
        if clean.starts_with('<') && clean.ends_with('>') {
            boundary = true; // language tags and other specials
            continue;
        }
        if clean.is_empty() {
            boundary |= starts;
            continue;
        }
        if starts || boundary || words.is_empty() {
            words.push(RawWord {
                text: clean.to_string(),
                t0,
                t1,
            });
        } else if let Some(w) = words.last_mut() {
            w.text.push_str(clean);
            w.t1 = t1;
        }
        boundary = false;
    }
    words
}

/// Silero VAD (sherpa-onnx, CPU) tracking whether we are inside speech.
struct VadGate {
    vad: VoiceActivityDetector,
    in_speech: bool,
    preroll: VecDeque<f32>,
    preroll_len: usize,
}

enum VadEvent {
    None,
    /// Speech started; carries the pre-roll including the current block.
    Start(Vec<f32>),
    End,
}

impl VadGate {
    fn new(model: &Path, threshold: f32) -> Result<Self> {
        const MIN_SPEECH_S: f32 = 0.25;
        let c = VadModelConfig {
            silero_vad: SileroVadModelConfig {
                model: Some(model.to_string_lossy().into_owned()),
                threshold,
                min_silence_duration: 0.5,
                min_speech_duration: MIN_SPEECH_S,
                window_size: 512,
                max_speech_duration: 30.0,
            },
            sample_rate: SR as i32,
            num_threads: 1,
            provider: Some("cpu".into()),
            ..Default::default()
        };
        let vad = VoiceActivityDetector::create(&c, 60.0)
            .ok_or_else(|| anyhow!("cannot load Silero VAD from {}", model.display()))?;
        // Silero triggers after min_speech plus two windows; keep that much
        // audio (plus margin) so the first word is not clipped.
        let preroll_len = ((MIN_SPEECH_S + 0.35) * SR as f32) as usize;
        Ok(Self {
            vad,
            in_speech: false,
            preroll: VecDeque::new(),
            preroll_len,
        })
    }

    fn feed(&mut self, block: &[f32]) -> VadEvent {
        self.vad.accept_waveform(block);
        while !self.vad.is_empty() {
            self.vad.pop(); // only the live state is used
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
                VadEvent::Start(self.preroll.drain(..).collect())
            }
            (true, false) => {
                self.in_speech = false;
                VadEvent::End
            }
            _ => VadEvent::None,
        }
    }

    fn reset(&mut self) {
        self.vad.reset();
        self.in_speech = false;
        self.preroll.clear();
    }
}

pub struct Engine {
    rec: OnlineRecognizer,
    cfg: EngineCfg,
    vad: VadGate,
    stream: Option<OnlineStream>,
    /// Audio time at which the current stream started.
    stream_t0: f64,
    now_samples: u64,
    /// Words of the current stream already committed.
    done: usize,
    last_partial: Vec<String>,
    /// Decode steps since the token sequence last changed.
    idle_steps: u32,
    /// Text of the last committed word of this stream, as committed.
    committed_last: Option<String>,
}

impl Engine {
    pub fn new(cfg: EngineCfg) -> Result<Self> {
        let mut c = OnlineRecognizerConfig::default();
        c.model_config.transducer = OnlineTransducerModelConfig {
            encoder: Some(pick(&cfg.model_dir, "encoder")?),
            decoder: Some(pick(&cfg.model_dir, "decoder")?),
            joiner: Some(pick(&cfg.model_dir, "joiner")?),
        };
        let tokens = cfg.model_dir.join("tokens.txt");
        if !tokens.exists() {
            return Err(anyhow!("no tokens.txt in {}", cfg.model_dir.display()));
        }
        c.model_config.tokens = Some(tokens.to_string_lossy().into_owned());
        c.model_config.provider = Some(cfg.provider.clone());
        c.model_config.num_threads = cfg.threads;
        c.decoding_method = Some("greedy_search".into());
        c.enable_endpoint = false;
        let rec = OnlineRecognizer::create(&c)
            .ok_or_else(|| anyhow!("cannot create recognizer (provider {})", cfg.provider))?;
        let vad = VadGate::new(&cfg.vad_model, cfg.vad_threshold)?;
        Ok(Self {
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
        })
    }

    /// Runs a decode on synthetic input so the first real chunk is not slow
    /// (CUDA kernels, allocator), then resets.
    pub fn warm_up(&mut self) {
        let st = self.rec.create_stream();
        st.set_option("language", &self.cfg.lang);
        st.accept_waveform(SR as i32, &vec![0.0; SR * 2]);
        st.input_finished();
        while self.rec.is_ready(&st) {
            self.rec.decode(&st);
        }
        self.reset();
    }

    pub fn reset(&mut self) {
        self.now_samples = 0;
        self.vad.reset();
        self.stream = None;
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

    fn feed(&self, audio: &[f32]) {
        if let Some(st) = &self.stream {
            st.accept_waveform(SR as i32, audio);
        }
    }

    /// Ends the current stream: tail padding so the last chunk is complete.
    fn close_stream(&mut self, out: &mut Vec<RawWord>) {
        if let Some(st) = &self.stream {
            st.accept_waveform(SR as i32, &vec![0.0; SR * 6 / 10]);
            st.input_finished();
        }
        self.step(true, out);
        self.stream = None;
    }

    /// Decodes what is ready; commits finished words (or all, if `all`).
    fn step(&mut self, all: bool, out: &mut Vec<RawWord>) {
        let Some(st) = &self.stream else { return };
        let mut n = 0u32;
        while self.rec.is_ready(st) {
            self.rec.decode(st);
            n += 1;
        }
        let Some(r) = self.rec.get_result(st) else {
            return;
        };
        let ts = r.timestamps.unwrap_or_default();
        let mut words = group_words(&r.tokens, &ts);
        for w in &mut words {
            w.t0 += self.stream_t0;
            w.t1 += self.stream_t0;
        }
        let cur: Vec<String> = words.iter().map(|w| w.text.clone()).collect();
        if cur.len() != self.last_partial.len() || cur.last() != self.last_partial.last() {
            self.idle_steps = 0;
        } else {
            self.idle_steps = self.idle_steps.saturating_add(n);
        }
        self.last_partial = cur;
        // The last word may still grow; commit it once enough decoded audio
        // (not fed audio: the encoder lags by a chunk) produced no token.
        let quiet = self.idle_steps.saturating_mul(self.cfg.chunk_ms) >= self.cfg.final_word_ms;
        let upto = if all || quiet {
            words.len()
        } else {
            words.len().saturating_sub(1)
        };
        // A word committed on the quiet rule can still gain a suffix, usually
        // punctuation ("panes" -> "panes,"); emit the suffix as its own word.
        if self.done > 0
            && let (Some(w), Some(prev)) = (words.get(self.done - 1), self.committed_last.as_ref())
            && w.text.len() > prev.len()
            && let Some(suffix) = w.text.strip_prefix(prev.as_str())
        {
            out.push(RawWord {
                text: suffix.to_string(),
                t0: w.t1,
                t1: w.t1,
            });
            self.committed_last = Some(w.text.clone());
        }
        if upto > self.done {
            out.extend(words[self.done..upto].iter().cloned());
            self.done = upto;
            self.committed_last = words.get(upto - 1).map(|w| w.text.clone());
        }
    }

    /// Feeds audio (16 kHz mono, -1..1) and returns newly committed words.
    pub fn push(&mut self, audio: &[f32]) -> Vec<RawWord> {
        let mut out = Vec::new();
        for block in audio.chunks(320) {
            self.now_samples += block.len() as u64;
            match self.vad.feed(block) {
                VadEvent::Start(pre) => {
                    let t0 = self.now() - pre.len() as f64 / SR as f64;
                    self.open_stream(t0);
                    self.feed(&pre);
                }
                VadEvent::None => self.feed(block),
                VadEvent::End => {
                    self.feed(block);
                    self.close_stream(&mut out);
                }
            }
        }
        self.step(false, &mut out);
        out
    }

    /// Flushes the open segment, committing every word.
    pub fn finish(&mut self) -> Vec<RawWord> {
        let mut out = Vec::new();
        self.close_stream(&mut out);
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn toks(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn groups_pieces_into_words_and_skips_specials() {
        let t = toks(&["<en>", "\u{2581}hel", "lo", "\u{2581}", "wor", "ld", ","]);
        let w = group_words(&t, &[0.0, 0.1, 0.2, 0.3, 0.4, 0.5, 0.6]);
        let text: Vec<&str> = w.iter().map(|w| w.text.as_str()).collect();
        assert_eq!(text, ["hello", "world,"]);
        assert!((w[0].t0 - 0.1).abs() < 1e-6);
        assert!((w[1].t1 - 0.68).abs() < 1e-6);
    }

    #[test]
    fn missing_timestamps_do_not_panic() {
        let w = group_words(&toks(&["\u{2581}a", "\u{2581}b"]), &[]);
        assert_eq!(w.len(), 2);
    }
}
