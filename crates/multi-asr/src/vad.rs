//! Silero VAD (sherpa-onnx, CPU) tracking whether we are inside speech.
//! Shared by both engines: neither decodes outside speech.

use crate::engine::SR;
use anyhow::{Result, anyhow};
use sherpa_onnx::{SileroVadModelConfig, VadModelConfig, VoiceActivityDetector};
use std::collections::VecDeque;
use std::path::Path;

pub struct VadGate {
    vad: VoiceActivityDetector,
    in_speech: bool,
    preroll: VecDeque<f32>,
    preroll_len: usize,
}

pub enum VadEvent {
    None,
    /// Speech started; carries the pre-roll including the current block.
    Start(Vec<f32>),
    /// Speech ended (after the VAD's minimum silence).
    End,
}

impl VadGate {
    pub fn new(model: &Path, threshold: f32) -> Result<Self> {
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

    pub fn feed(&mut self, block: &[f32]) -> VadEvent {
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

    pub fn reset(&mut self) {
        self.vad.reset();
        self.in_speech = false;
        self.preroll.clear();
    }
}
