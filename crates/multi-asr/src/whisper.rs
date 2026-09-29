//! Whisper through whisper.cpp (`whisper-rs`), ported from the S4 spike:
//! whole-buffer transcription with token timestamps grouped into words,
//! the language set explicitly, and every segment passed through the
//! hallucination guard (`guard.rs`) before LocalAgreement sees it.

use crate::agreement::Transcriber;
use crate::engine::{RawWord, SR};
use crate::guard::{self, GuardCfg, GuardStats, Segment};
use anyhow::{Context, Result, anyhow};
use std::path::Path;
use whisper_rs::{
    FullParams, SamplingStrategy, WhisperContext, WhisperContextParameters, WhisperState,
};

pub struct WhisperAsr {
    // `state` needs the context alive; keep both.
    state: WhisperState,
    _ctx: WhisperContext,
    lang: String,
    threads: i32,
    eot: i32,
    guard: GuardCfg,
    stats: GuardStats,
    logged: GuardStats,
}

impl WhisperAsr {
    pub fn new(model: &Path, lang: &str, gpu: bool, threads: i32) -> Result<Self> {
        whisper_rs::install_logging_hooks();
        let mut p = WhisperContextParameters::default();
        p.use_gpu(gpu).flash_attn(gpu);
        let ctx = WhisperContext::new_with_params(model, p)
            .with_context(|| format!("loading Whisper model {}", model.display()))?;
        let eot = ctx.token_eot();
        let state = ctx
            .create_state()
            .map_err(|e| anyhow!("whisper state: {e:?}"))?;
        Ok(Self {
            state,
            _ctx: ctx,
            lang: lang.to_string(),
            threads,
            eot,
            guard: GuardCfg::default(),
            stats: GuardStats::default(),
            logged: GuardStats::default(),
        })
    }

    /// The decoder's segments with words and confidence.
    fn segments(&self) -> Vec<Segment> {
        let mut out = Vec::new();
        for seg in self.state.as_iter() {
            let mut words: Vec<RawWord> = Vec::new();
            let (mut lp_sum, mut lp_n) = (0.0f32, 0u32);
            for i in 0..seg.n_tokens() {
                let Some(tok) = seg.get_token(i) else {
                    continue;
                };
                if tok.token_id() >= self.eot {
                    continue; // special and timestamp tokens
                }
                let d = tok.token_data();
                lp_sum += d.plog;
                lp_n += 1;
                let text = tok
                    .to_str_lossy()
                    .map(|s| s.into_owned())
                    .unwrap_or_default();
                if text.is_empty() {
                    continue;
                }
                let (t0, t1) = (d.t0 as f64 / 100.0, d.t1 as f64 / 100.0);
                if text.starts_with(' ') || words.is_empty() {
                    words.push(RawWord {
                        text: text.trim().to_string(),
                        t0,
                        t1,
                    });
                } else if let Some(w) = words.last_mut() {
                    w.text.push_str(&text);
                    w.t1 = t1.max(w.t1);
                }
            }
            // Non-speech annotations such as "[BLANK_AUDIO]" or "(music)".
            words.retain(|w| {
                !(w.text.is_empty()
                    || w.text.starts_with('[')
                    || w.text.starts_with('(')
                    || w.text.starts_with('*')
                    || w.text.starts_with('♪'))
            });
            let avg_logprob = if lp_n > 0 { lp_sum / lp_n as f32 } else { 0.0 };
            out.push(Segment {
                words,
                no_speech: seg.no_speech_probability(),
                avg_logprob,
            });
        }
        out
    }
}

impl Transcriber for WhisperAsr {
    fn transcribe(&mut self, audio: &[f32], prompt: &str) -> Result<Vec<RawWord>> {
        let mut p = FullParams::new(SamplingStrategy::Greedy { best_of: 1 });
        p.set_n_threads(self.threads);
        p.set_language(Some(self.lang.as_str()));
        p.set_no_context(true);
        p.set_token_timestamps(true);
        p.set_suppress_nst(true);
        p.set_print_special(false);
        p.set_print_progress(false);
        p.set_print_realtime(false);
        p.set_print_timestamps(false);
        if !prompt.is_empty() {
            p.set_initial_prompt(prompt);
        }
        // whisper.cpp refuses < 1 s; pad with silence (does not move times).
        let min = SR + SR / 10;
        let padded;
        let audio = if audio.len() < min {
            let mut v = audio.to_vec();
            v.resize(min, 0.0);
            padded = v;
            &padded[..]
        } else {
            audio
        };
        self.state
            .full(p, audio)
            .map_err(|e| anyhow!("whisper decode: {e:?}"))?;
        let segments = self.segments();
        for s in &segments {
            tracing::trace!(
                no_speech = s.no_speech,
                avg_logprob = s.avg_logprob,
                words = s.words.len(),
                "whisper segment"
            );
        }
        Ok(guard::apply(segments, &self.guard, &mut self.stats))
    }

    fn log_stats(&mut self, committed_loops: u64) {
        if self.stats != self.logged {
            self.logged = self.stats;
            let s = self.stats;
            tracing::debug!(
                no_speech = s.no_speech,
                low_confidence = s.low_confidence,
                filler = s.filler,
                loop_words = s.loop_words,
                committed_loops,
                "hallucination guard (totals)"
            );
        }
    }
}
