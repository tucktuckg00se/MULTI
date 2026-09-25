//! Text moving through the caption path: words from ASR, clauses for
//! translation, and translations back.
//!
//! Times are milliseconds on the audio stream's timeline, so captions can be
//! mapped back to video presentation time.

use serde::{Deserialize, Serialize};

/// One committed word from speech recognition. Committed words never change.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Word {
    pub text: String,
    pub start_ms: u64,
    pub end_ms: u64,
}

/// A run of words closed by the segmenter and sent for translation.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Clause {
    pub id: u64,
    pub text: String,
    pub start_ms: u64,
    pub end_ms: u64,
}

/// A clause translated into one target language.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Translation {
    pub clause_id: u64,
    /// ISO 639-1 language code, e.g. `es`.
    pub lang: String,
    pub text: String,
    /// Time the translation took, for monitoring.
    pub elapsed_ms: u32,
}
