//! A scripted worker that speaks the real worker protocol ([`multi_core::ipc`])
//! without any models, so the supervisor and the media pipeline can be tested
//! in CI.
//!
//! - `asr`: answers every PCM frame with one scripted word spanning the frame.
//! - `mt`: answers `Translate` with `[lang] text` for each language.
//!
//! Failure injection: `--crash-after N` exits after N input frames,
//! `--hang-after N` stops heartbeats and reading after N frames, and
//! `--garbage soft|hard` writes malformed output right after `Ready`.

use clap::{Parser, ValueEnum};
use multi_core::ipc::{self, Frame, FrameError, Message};
use multi_core::worker::{self, Heartbeat};
use multi_core::{Translation, Word};
use std::io::{self, Write};
use std::process::ExitCode;
use std::thread;
use std::time::Duration;

/// Words the `asr` mode emits, in order, one per PCM frame.
pub const SCRIPT: [&str; 6] = ["alpha", "bravo", "charlie", "delta", "echo", "foxtrot"];

#[derive(Clone, Copy, Debug, ValueEnum)]
pub enum Mode {
    Asr,
    Mt,
}

#[derive(Clone, Copy, Debug, ValueEnum)]
pub enum Garbage {
    /// Well-framed but invalid frames (bad JSON, unknown kind, wrong direction).
    Soft,
    /// Bytes that break the framing.
    Hard,
}

#[derive(Debug, Parser)]
#[command(
    name = "multi-fake-worker",
    about = "Scripted stand-in for MULTI workers"
)]
pub struct Args {
    #[arg(value_enum)]
    pub mode: Mode,
    /// Exit with status 3 after this many input frames.
    #[arg(long)]
    pub crash_after: Option<u64>,
    /// Stop heartbeats and stop reading after this many input frames.
    #[arg(long)]
    pub hang_after: Option<u64>,
    /// Write malformed output right after `Ready`.
    #[arg(long, value_enum)]
    pub garbage: Option<Garbage>,
    /// Pretend model loading takes this long.
    #[arg(long, default_value_t = 0)]
    pub load_ms: u64,
}

/// Parses `args` (including the program name) and runs the worker.
pub fn main_from<I, T>(args: I) -> ExitCode
where
    I: IntoIterator<Item = T>,
    T: Into<std::ffi::OsString> + Clone,
{
    match Args::try_parse_from(args) {
        Ok(args) => run(&args),
        Err(e) => {
            eprintln!("{e}");
            ExitCode::from(2)
        }
    }
}

pub fn run(args: &Args) -> ExitCode {
    let (hb, hb_thread) = match Heartbeat::start(Duration::from_secs(10)) {
        Ok(x) => x,
        Err(e) => {
            eprintln!("cannot start heartbeat thread: {e}");
            return ExitCode::FAILURE;
        }
    };
    let code = serve(args, &hb);
    hb.stop();
    let _ = hb_thread.join();
    code
}

fn serve(args: &Args, hb: &Heartbeat) -> ExitCode {
    thread::sleep(Duration::from_millis(args.load_ms));
    let name = match args.mode {
        Mode::Asr => "fake-asr",
        Mode::Mt => "fake-mt",
    };
    let ready = Message::Ready {
        worker: name.into(),
        version: env!("CARGO_PKG_VERSION").into(),
    };
    if worker::send(&ready).is_err() {
        return ExitCode::FAILURE;
    }
    eprintln!("{name} ready");
    if let Some(g) = args.garbage
        && write_garbage(g).is_err()
    {
        return ExitCode::FAILURE;
    }

    let mut stdin = io::stdin().lock();
    let mut frames = 0u64;
    let mut word = 0usize;
    loop {
        let frame = match ipc::read_frame(&mut stdin) {
            Ok(f) => f,
            Err(FrameError::Closed) => return ExitCode::SUCCESS,
            Err(e @ (FrameError::BadJson(_) | FrameError::BadKind(_) | FrameError::BadPcm)) => {
                let _ = worker::send(&Message::Error {
                    message: format!("bad input frame: {e}"),
                });
                continue;
            }
            Err(e) => {
                eprintln!("input broken: {e}");
                return ExitCode::FAILURE;
            }
        };
        frames += 1;
        if args.crash_after.is_some_and(|n| frames > n) {
            eprintln!("crashing on purpose after {} frames", frames - 1);
            return ExitCode::from(3);
        }
        if args.hang_after.is_some_and(|n| frames > n) {
            eprintln!("hanging on purpose after {} frames", frames - 1);
            hb.pause();
            loop {
                thread::sleep(Duration::from_secs(3600));
            }
        }
        let reply = match frame {
            Frame::Message(Message::Shutdown) => return ExitCode::SUCCESS,
            Frame::Pcm { start_ms, samples } if !samples.is_empty() => {
                let text = SCRIPT[word % SCRIPT.len()];
                word += 1;
                vec![Message::Words {
                    words: vec![Word {
                        text: text.into(),
                        start_ms,
                        end_ms: start_ms + samples.len() as u64 / 16,
                    }],
                }]
            }
            Frame::Message(Message::Translate { clause, langs }) => langs
                .into_iter()
                .map(|lang| Message::Translated {
                    translation: Translation {
                        clause_id: clause.id,
                        text: format!("[{lang}] {}", clause.text),
                        lang,
                        elapsed_ms: 0,
                    },
                })
                .collect(),
            _ => Vec::new(),
        };
        for msg in &reply {
            if worker::send(msg).is_err() {
                return ExitCode::FAILURE;
            }
        }
    }
}

fn write_garbage(kind: Garbage) -> io::Result<()> {
    let mut out = io::stdout().lock();
    match kind {
        Garbage::Soft => {
            raw_frame(&mut out, ipc::KIND_JSON, br#"{"type":"no_such_message"}"#)?;
            raw_frame(&mut out, ipc::KIND_JSON, b"not json at all")?;
            raw_frame(&mut out, 9, b"unknown kind")?;
            raw_frame(&mut out, ipc::KIND_PCM, &[1, 2, 3])?;
            // Valid, but only the main process may send it.
            let json = serde_translate_json();
            raw_frame(&mut out, ipc::KIND_JSON, json.as_bytes())?;
        }
        Garbage::Hard => {
            // "this" read as a little-endian length is ~1.9 GB.
            out.write_all(b"this is not a frame\n")?;
        }
    }
    out.flush()
}

fn serde_translate_json() -> String {
    r#"{"type":"translate","clause":{"id":1,"text":"x","start_ms":0,"end_ms":1},"langs":["es"]}"#
        .into()
}

fn raw_frame(w: &mut impl Write, kind: u8, payload: &[u8]) -> io::Result<()> {
    let len = u32::try_from(payload.len() + 1).map_err(io::Error::other)?;
    w.write_all(&len.to_le_bytes())?;
    w.write_all(&[kind])?;
    w.write_all(payload)
}
