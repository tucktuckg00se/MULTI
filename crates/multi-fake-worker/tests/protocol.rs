//! Drives the fake worker binary directly over its stdio.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use multi_core::ipc::{self, Frame, Message};
use multi_core::{Clause, Translation};
use std::process::{Command, Stdio};

fn next_non_heartbeat(r: &mut impl std::io::Read) -> Frame {
    loop {
        match ipc::read_frame(r).unwrap() {
            Frame::Message(Message::Heartbeat { .. }) => continue,
            f => return f,
        }
    }
}

#[test]
fn mt_mode_translates_and_shuts_down() {
    let mut child = Command::new(env!("CARGO_BIN_EXE_multi-fake-worker"))
        .arg("mt")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut out = child.stdout.take().unwrap();
    let mut inp = child.stdin.take().unwrap();
    assert!(matches!(
        next_non_heartbeat(&mut out),
        Frame::Message(Message::Ready { .. })
    ));
    let clause = Clause {
        id: 5,
        text: "hello".into(),
        start_ms: 0,
        end_ms: 500,
    };
    ipc::write_message(
        &mut inp,
        &Message::Translate {
            clause,
            langs: vec!["es".into(), "de".into()],
        },
    )
    .unwrap();
    for lang in ["es", "de"] {
        assert_eq!(
            next_non_heartbeat(&mut out),
            Frame::Message(Message::Translated {
                translation: Translation {
                    clause_id: 5,
                    lang: lang.into(),
                    text: format!("[{lang}] hello"),
                    elapsed_ms: 0,
                }
            })
        );
    }
    ipc::write_message(&mut inp, &Message::Shutdown).unwrap();
    assert!(child.wait().unwrap().success());
}
