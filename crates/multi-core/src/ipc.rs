//! Framing between the main process and its workers (ASR, translation),
//! carried over the worker's stdin and stdout.
//!
//! Each frame is `u32 length (little-endian) | u8 kind | payload`, where
//! `length` counts the kind byte and payload. Kinds:
//! - [`KIND_JSON`]: one [`Message`] as JSON.
//! - [`KIND_PCM`]: audio for ASR, `u64 start_ms (LE)` then 16 kHz mono `i16` LE samples.
//!
//! Readers never trust the peer: oversized, empty or malformed frames are
//! errors, not panics, so a misbehaving worker can be restarted cleanly.

use crate::types::{Clause, Translation, Word};
use serde::{Deserialize, Serialize};
use std::io::{self, Read, Write};

pub const KIND_JSON: u8 = 1;
pub const KIND_PCM: u8 = 2;

/// Largest frame accepted (1 MiB): ~32 s of 16 kHz audio, far above any real message.
pub const MAX_FRAME: usize = 1 << 20;

/// Control and data messages, in both directions.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Message {
    /// First message a worker sends once its model is loaded.
    Ready { worker: String, version: String },
    /// Sent by workers at least once a second; silence means the worker is stuck.
    Heartbeat { seq: u64 },
    /// ASR → main: newly committed words.
    Words { words: Vec<Word> },
    /// Main → MT: translate a clause into these languages.
    Translate { clause: Clause, langs: Vec<String> },
    /// MT → main: one translation.
    Translated { translation: Translation },
    /// Main → worker: finish current work and exit.
    Shutdown,
    /// Worker → main: a recoverable problem worth logging.
    Error { message: String },
}

/// A decoded frame.
#[derive(Clone, Debug, PartialEq)]
pub enum Frame {
    Message(Message),
    Pcm { start_ms: u64, samples: Vec<i16> },
}

#[derive(Debug, thiserror::Error)]
pub enum FrameError {
    #[error("peer closed the stream")]
    Closed,
    #[error("frame length {0} outside 1..={MAX_FRAME}")]
    BadLength(usize),
    #[error("unknown frame kind {0}")]
    BadKind(u8),
    #[error("malformed PCM frame")]
    BadPcm,
    #[error("malformed JSON message: {0}")]
    BadJson(#[from] serde_json::Error),
    #[error(transparent)]
    Io(#[from] io::Error),
}

pub fn write_message(w: &mut impl Write, msg: &Message) -> Result<(), FrameError> {
    let json = serde_json::to_vec(msg)?;
    write_frame(w, KIND_JSON, &json)
}

pub fn write_pcm(w: &mut impl Write, start_ms: u64, samples: &[i16]) -> Result<(), FrameError> {
    let mut payload = Vec::with_capacity(8 + samples.len() * 2);
    payload.extend_from_slice(&start_ms.to_le_bytes());
    for s in samples {
        payload.extend_from_slice(&s.to_le_bytes());
    }
    write_frame(w, KIND_PCM, &payload)
}

fn write_frame(w: &mut impl Write, kind: u8, payload: &[u8]) -> Result<(), FrameError> {
    let len = payload.len() + 1;
    if len > MAX_FRAME {
        return Err(FrameError::BadLength(len));
    }
    let len32 = u32::try_from(len).map_err(|_| FrameError::BadLength(len))?;
    w.write_all(&len32.to_le_bytes())?;
    w.write_all(&[kind])?;
    w.write_all(payload)?;
    w.flush()?;
    Ok(())
}

/// Reads one frame. Returns [`FrameError::Closed`] on a clean end of stream.
pub fn read_frame(r: &mut impl Read) -> Result<Frame, FrameError> {
    let mut len_buf = [0u8; 4];
    match r.read_exact(&mut len_buf) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Err(FrameError::Closed),
        Err(e) => return Err(e.into()),
    }
    let len = u32::from_le_bytes(len_buf) as usize;
    if len == 0 || len > MAX_FRAME {
        return Err(FrameError::BadLength(len));
    }
    let mut buf = vec![0u8; len];
    r.read_exact(&mut buf)?;
    let (kind, payload) = buf.split_first().ok_or(FrameError::BadLength(len))?;
    match *kind {
        KIND_JSON => Ok(Frame::Message(serde_json::from_slice(payload)?)),
        KIND_PCM => decode_pcm(payload),
        other => Err(FrameError::BadKind(other)),
    }
}

fn decode_pcm(payload: &[u8]) -> Result<Frame, FrameError> {
    let (head, body) = payload.split_at_checked(8).ok_or(FrameError::BadPcm)?;
    if body.len() % 2 != 0 {
        return Err(FrameError::BadPcm);
    }
    let start_ms = u64::from_le_bytes(head.try_into().map_err(|_| FrameError::BadPcm)?);
    let samples = body
        .as_chunks::<2>()
        .0
        .iter()
        .map(|b| i16::from_le_bytes(*b))
        .collect();
    Ok(Frame::Pcm { start_ms, samples })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn round_trip(msg: Message) -> Result<(), FrameError> {
        let mut buf = Vec::new();
        write_message(&mut buf, &msg)?;
        assert_eq!(read_frame(&mut Cursor::new(buf))?, Frame::Message(msg));
        Ok(())
    }

    #[test]
    fn messages_round_trip() -> Result<(), FrameError> {
        round_trip(Message::Ready {
            worker: "asr".into(),
            version: "0.1.0".into(),
        })?;
        round_trip(Message::Heartbeat { seq: 7 })?;
        round_trip(Message::Words {
            words: vec![Word {
                text: "hello".into(),
                start_ms: 10,
                end_ms: 400,
            }],
        })?;
        round_trip(Message::Translate {
            clause: Clause {
                id: 3,
                text: "hello there".into(),
                start_ms: 0,
                end_ms: 900,
            },
            langs: vec!["es".into(), "fr".into()],
        })?;
        round_trip(Message::Translated {
            translation: Translation {
                clause_id: 3,
                lang: "es".into(),
                text: "hola".into(),
                elapsed_ms: 12,
            },
        })?;
        round_trip(Message::Shutdown)?;
        round_trip(Message::Error {
            message: "x".into(),
        })
    }

    #[test]
    fn pcm_round_trip() -> Result<(), FrameError> {
        let mut buf = Vec::new();
        write_pcm(&mut buf, 1234, &[0, -1, i16::MAX, i16::MIN])?;
        let frame = read_frame(&mut Cursor::new(buf))?;
        assert_eq!(
            frame,
            Frame::Pcm {
                start_ms: 1234,
                samples: vec![0, -1, i16::MAX, i16::MIN]
            }
        );
        Ok(())
    }

    #[test]
    fn several_frames_in_sequence() -> Result<(), FrameError> {
        let mut buf = Vec::new();
        write_message(&mut buf, &Message::Heartbeat { seq: 1 })?;
        write_pcm(&mut buf, 0, &[5])?;
        let mut r = Cursor::new(buf);
        assert!(matches!(
            read_frame(&mut r)?,
            Frame::Message(Message::Heartbeat { seq: 1 })
        ));
        assert!(matches!(read_frame(&mut r)?, Frame::Pcm { .. }));
        assert!(matches!(read_frame(&mut r), Err(FrameError::Closed)));
        Ok(())
    }

    #[test]
    fn hostile_input_is_rejected_without_panicking() {
        let cases: Vec<Vec<u8>> = vec![
            vec![0, 0, 0, 0],                                       // zero length
            vec![0xFF, 0xFF, 0xFF, 0x7F, 1],                        // huge length
            vec![2, 0, 0, 0, 9, 0],                                 // unknown kind
            vec![3, 0, 0, 0, KIND_JSON, b'{', b'x'],                // bad JSON
            vec![4, 0, 0, 0, KIND_PCM, 1, 2, 3],                    // PCM too short
            vec![11, 0, 0, 0, KIND_PCM, 0, 0, 0, 0, 0, 0, 0, 0, 1], // odd sample bytes
            vec![9, 0, 0, 0, KIND_JSON],                            // truncated
        ];
        for bytes in cases {
            assert!(read_frame(&mut Cursor::new(bytes)).is_err());
        }
    }

    #[test]
    fn oversized_write_is_refused() {
        let samples = vec![0i16; MAX_FRAME];
        assert!(matches!(
            write_pcm(&mut Vec::new(), 0, &samples),
            Err(FrameError::BadLength(_))
        ));
    }
}
