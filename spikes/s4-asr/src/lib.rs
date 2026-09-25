//! Library side of the S4 spike (reused by S6): sherpa-onnx streaming ASR,
//! the VAD gate and resource sampling. Whisper stays in the binary.

pub mod res;
pub mod sherpa_asr;
pub mod stream;
