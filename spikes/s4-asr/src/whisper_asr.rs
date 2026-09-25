//! Whisper via whisper-rs (whisper.cpp, CUDA): whole-buffer transcription with
//! token timestamps grouped into words.

use anyhow::{Context, Result, anyhow};
use whisper_rs::{FullParams, SamplingStrategy, WhisperContext, WhisperContextParameters, WhisperState};

use crate::stream::{OfflineAsr, Word};

pub struct WhisperAsr {
    // `state` borrows nothing from `ctx` at the type level but needs it alive.
    state: WhisperState,
    _ctx: WhisperContext,
    lang: String,
    threads: i32,
    beam: i32,
    eot: i32,
    pub use_prompt: bool,
}

impl WhisperAsr {
    pub fn new(model: &str, lang: &str, gpu: bool, threads: i32, beam: i32) -> Result<Self> {
        let mut p = WhisperContextParameters::default();
        p.use_gpu(gpu).flash_attn(gpu);
        let ctx = WhisperContext::new_with_params(model, p).with_context(|| format!("loading {model}"))?;
        let eot = ctx.token_eot();
        let state = ctx.create_state().map_err(|e| anyhow!("whisper state: {e:?}"))?;
        Ok(Self { state, _ctx: ctx, lang: lang.to_string(), threads, beam, eot, use_prompt: true })
    }
}

impl OfflineAsr for WhisperAsr {
    fn transcribe(&mut self, audio: &[f32], prompt: &str) -> Result<Vec<Word>> {
        let strategy = if self.beam > 1 {
            SamplingStrategy::BeamSearch { beam_size: self.beam, patience: -1.0 }
        } else {
            SamplingStrategy::Greedy { best_of: 1 }
        };
        let mut p = FullParams::new(strategy);
        p.set_n_threads(self.threads);
        p.set_language(Some(self.lang.as_str()));
        p.set_no_context(true);
        p.set_token_timestamps(true);
        p.set_suppress_nst(true);
        p.set_print_special(false);
        p.set_print_progress(false);
        p.set_print_realtime(false);
        p.set_print_timestamps(false);
        if self.use_prompt && !prompt.is_empty() {
            p.set_initial_prompt(prompt);
        }
        // whisper.cpp refuses < 1 s; pad with silence (does not move timestamps).
        let padded;
        let audio = if audio.len() < 16_000 + 1600 {
            let mut v = audio.to_vec();
            v.resize(16_000 + 1600, 0.0);
            padded = v;
            &padded[..]
        } else {
            audio
        };
        self.state.full(p, audio).map_err(|e| anyhow!("whisper full: {e:?}"))?;
        let mut words: Vec<Word> = Vec::new();
        for seg in self.state.as_iter() {
            for i in 0..seg.n_tokens() {
                let Some(tok) = seg.get_token(i) else { continue };
                if tok.token_id() >= self.eot {
                    continue; // special / timestamp tokens
                }
                let text = tok.to_str_lossy().map(|s| s.into_owned()).unwrap_or_default();
                if text.is_empty() {
                    continue;
                }
                let d = tok.token_data();
                let (t0, t1) = (d.t0 as f64 / 100.0, d.t1 as f64 / 100.0);
                if text.starts_with(' ') || words.is_empty() {
                    words.push(Word { text: text.trim().to_string(), t0, t1 });
                } else if let Some(w) = words.last_mut() {
                    w.text.push_str(&text);
                    w.t1 = t1.max(w.t1);
                }
            }
        }
        // Non-speech annotations such as "[BLANK_AUDIO]" or "(music)".
        words.retain(|w| !(w.text.starts_with('[') || w.text.starts_with('(') || w.text.starts_with('*')));
        Ok(words)
    }
}
