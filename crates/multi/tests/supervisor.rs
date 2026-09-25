//! Supervisor tests against the fake worker.
//!
//! This test binary has its own `main` (`harness = false`): when started with
//! `MULTI_FAKE_WORKER=1` it *is* the fake worker, so the tests need no other
//! binary built first. Tests run one after another because they measure time.
//! Pass a substring to run only matching tests.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use multi::supervisor::{Policy, State, Status, Supervisor, WorkerSpec};
use multi_core::Clause;
use multi_core::ipc::Message;
use std::process::ExitCode;
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::time::{Duration, Instant};

const ENV: &str = "MULTI_FAKE_WORKER";

fn main() -> ExitCode {
    if std::env::var_os(ENV).is_some() {
        return multi_fake_worker::main_from(std::env::args_os());
    }
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "warn".into()),
        )
        .with_writer(std::io::stderr)
        .init();
    let tests: &[(&str, fn())] = &[
        ("words_flow", words_flow),
        ("translations_flow", translations_flow),
        ("restart_after_crash", restart_after_crash),
        ("restart_after_hang", restart_after_hang),
        ("send_never_blocks_while_down", send_never_blocks_while_down),
        ("send_never_blocks_while_hung", send_never_blocks_while_hung),
        ("missing_binary_is_failed", missing_binary_is_failed),
        ("clean_shutdown", clean_shutdown),
        ("shutdown_kills_hung_worker", shutdown_kills_hung_worker),
        ("soft_garbage_is_ignored", soft_garbage_is_ignored),
        ("hard_garbage_restarts", hard_garbage_restarts),
    ];
    let filter: Option<String> = std::env::args().skip(1).find(|a| !a.starts_with('-'));
    let (mut passed, mut failed) = (0, 0);
    for (name, test) in tests {
        if filter.as_deref().is_some_and(|f| !name.contains(f)) {
            continue;
        }
        let t = Instant::now();
        match std::panic::catch_unwind(test) {
            Ok(()) => {
                passed += 1;
                println!("test {name} ... ok ({} ms)", t.elapsed().as_millis());
            }
            Err(_) => {
                failed += 1;
                println!("test {name} ... FAILED");
            }
        }
    }
    println!("\ntest result: {passed} passed; {failed} failed");
    if failed == 0 {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

fn fake(mode: &str, extra: &[&str]) -> WorkerSpec {
    WorkerSpec::new(format!("fake-{mode}"), std::env::current_exe().unwrap())
        .arg(mode)
        .args(extra.iter().copied())
        .env(ENV, "1")
}

fn start(spec: WorkerSpec) -> (Supervisor, Receiver<Message>) {
    Supervisor::start(spec, Policy::default()).unwrap()
}

fn wait_for(sup: &Supervisor, what: &str, limit: Duration, f: impl Fn(&Status) -> bool) -> Status {
    let t = Instant::now();
    loop {
        let s = sup.status();
        if f(&s) {
            return s;
        }
        assert!(t.elapsed() < limit, "timed out waiting for {what}: {s:?}");
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn wait_ready(sup: &Supervisor) {
    wait_for(sup, "ready", Duration::from_secs(5), |s| {
        s.state == State::Ready
    });
}

fn pcm(sup: &Supervisor, start_ms: u64) -> bool {
    sup.send_pcm(start_ms, vec![0; 1600])
}

fn next_words(rx: &Receiver<Message>, limit: Duration) -> Vec<String> {
    match rx.recv_timeout(limit) {
        Ok(Message::Words { words }) => words.into_iter().map(|w| w.text).collect(),
        other => panic!("expected words, got {other:?}"),
    }
}

fn words_flow() {
    let (sup, rx) = start(fake("asr", &[]));
    wait_ready(&sup);
    for i in 0..3 {
        assert!(pcm(&sup, i * 100));
    }
    let mut got = Vec::new();
    for _ in 0..3 {
        got.extend(next_words(&rx, Duration::from_secs(2)));
    }
    assert_eq!(got, ["alpha", "bravo", "charlie"]);
    let s = sup.status();
    assert_eq!((s.restarts, s.dropped_in), (0, 0));
    sup.shutdown();
}

fn translations_flow() {
    let (sup, rx) = start(fake("mt", &[]));
    let clause = Clause {
        id: 9,
        text: "good morning".into(),
        start_ms: 0,
        end_ms: 800,
    };
    // Sent before Ready: held until the worker is ready.
    sup.send_message(Message::Translate {
        clause,
        langs: vec!["es".into(), "fr".into()],
    });
    let mut got = Vec::new();
    for _ in 0..2 {
        match rx.recv_timeout(Duration::from_secs(5)) {
            Ok(Message::Translated { translation }) => got.push(translation.text),
            other => panic!("expected a translation, got {other:?}"),
        }
    }
    assert_eq!(got, ["[es] good morning", "[fr] good morning"]);
    sup.shutdown();
}

/// Feeds 100 ms of PCM every 50 ms and returns the longest gap between
/// consecutive `Words`, measured across the first restart.
fn longest_gap_across_restart(sup: &Supervisor, rx: &Receiver<Message>) -> Duration {
    wait_ready(sup);
    let t0 = Instant::now();
    let mut last_words = Instant::now();
    let mut gap = Duration::ZERO;
    let mut restarted_at = None;
    let mut ms = 0;
    while t0.elapsed() < Duration::from_secs(15) {
        pcm(sup, ms);
        ms += 100;
        loop {
            match rx.recv_timeout(Duration::from_millis(50)) {
                Ok(Message::Words { .. }) => {
                    gap = gap.max(last_words.elapsed());
                    last_words = Instant::now();
                    if restarted_at.is_some() {
                        return gap;
                    }
                }
                Ok(_) => {}
                Err(RecvTimeoutError::Timeout) => break,
                Err(RecvTimeoutError::Disconnected) => panic!("channel closed"),
            }
        }
        if restarted_at.is_none() && sup.status().restarts > 0 {
            restarted_at = Some(Instant::now());
        }
    }
    panic!("captions did not resume: {:?}", sup.status());
}

fn restart_after_crash() {
    let (sup, rx) = start(fake("asr", &["--crash-after", "5"]));
    let gap = longest_gap_across_restart(&sup, &rx);
    let s = sup.status();
    assert!(gap < Duration::from_secs(5), "captions gap {gap:?}");
    assert!(s.last_error.unwrap_or_default().contains("exited"));
    println!("  crash: captions gap {} ms", gap.as_millis());
    sup.shutdown();
}

fn restart_after_hang() {
    let (sup, rx) = start(fake("asr", &["--hang-after", "5"]));
    let gap = longest_gap_across_restart(&sup, &rx);
    let s = sup.status();
    assert!(gap < Duration::from_secs(5), "captions gap {gap:?}");
    assert!(s.last_error.unwrap_or_default().contains("heartbeat"));
    println!("  hang: captions gap {} ms", gap.as_millis());
    sup.shutdown();
}

/// Sends `n` frames and returns the slowest single `send`.
fn slowest_send(sup: &Supervisor, n: u64) -> Duration {
    let mut worst = Duration::ZERO;
    for i in 0..n {
        let t = Instant::now();
        pcm(sup, i * 100);
        worst = worst.max(t.elapsed());
    }
    worst
}

fn send_never_blocks_while_down() {
    // Crashes on its first frame: the worker is down or restarting throughout.
    let (sup, _rx) = start(fake("asr", &["--crash-after", "0"]));
    wait_ready(&sup);
    let t = Instant::now();
    while t.elapsed() < Duration::from_secs(2) {
        let worst = slowest_send(&sup, 1000);
        assert!(worst < Duration::from_millis(20), "send took {worst:?}");
    }
    let s = sup.status();
    assert!(s.restarts >= 1, "{s:?}");
    assert!(s.dropped_in > 0, "{s:?}");
    assert!(sup.queued() <= Policy::default().queue_capacity);
    sup.shutdown();
}

fn send_never_blocks_while_hung() {
    // Hangs on its first frame without reading: the pipe fills and the
    // writer thread blocks, but `send` must not.
    let (sup, _rx) = start(fake("asr", &["--hang-after", "0"]));
    wait_ready(&sup);
    let worst = slowest_send(&sup, 20_000);
    assert!(worst < Duration::from_millis(20), "send took {worst:?}");
    assert!(sup.status().dropped_in > 0);
    sup.shutdown();
}

fn missing_binary_is_failed() {
    let spec = WorkerSpec::new("ghost", "/nonexistent/multi-worker");
    let (sup, _rx) = start(spec);
    let s = wait_for(&sup, "failed", Duration::from_secs(2), |s| {
        s.state == State::Failed
    });
    assert!(s.last_error.unwrap_or_default().contains("cannot start"));
    assert!(pcm(&sup, 0));
    let t = Instant::now();
    sup.shutdown();
    assert!(
        t.elapsed() < Duration::from_millis(500),
        "shutdown waited out the backoff"
    );
    assert_eq!(sup.status().state, State::Stopped);
    assert!(!pcm(&sup, 0), "send after shutdown must be refused");
}

fn pid_alive(pid: u32) -> bool {
    std::path::Path::new(&format!("/proc/{pid}")).exists()
        && !std::fs::read_to_string(format!("/proc/{pid}/stat"))
            .unwrap_or_default()
            .contains(") Z ")
}

fn clean_shutdown() {
    let (sup, rx) = start(fake("asr", &[]));
    wait_ready(&sup);
    let pid = sup.status().pid.unwrap();
    pcm(&sup, 0);
    next_words(&rx, Duration::from_secs(2));
    let t = Instant::now();
    sup.shutdown();
    // A clean exit is quick; a kill would come only after the 2 s grace.
    assert!(
        t.elapsed() < Duration::from_secs(1),
        "took {:?}",
        t.elapsed()
    );
    assert!(!pid_alive(pid));
    let s = sup.status();
    assert_eq!((s.state, s.pid), (State::Stopped, None));
    sup.shutdown(); // idempotent
}

fn shutdown_kills_hung_worker() {
    let (sup, _rx) = start(fake("asr", &["--hang-after", "0"]));
    wait_ready(&sup);
    let pid = sup.status().pid.unwrap();
    pcm(&sup, 0);
    std::thread::sleep(Duration::from_millis(100));
    let t = Instant::now();
    sup.shutdown();
    let took = t.elapsed();
    assert!(took < Duration::from_secs(3), "took {took:?}");
    assert!(!pid_alive(pid));
}

fn soft_garbage_is_ignored() {
    let (sup, rx) = start(fake("asr", &["--garbage", "soft"]));
    wait_ready(&sup);
    pcm(&sup, 0);
    assert_eq!(next_words(&rx, Duration::from_secs(2)), ["alpha"]);
    assert_eq!(sup.status().restarts, 0);
    sup.shutdown();
}

fn hard_garbage_restarts() {
    let (sup, _rx) = start(fake("asr", &["--garbage", "hard"]));
    let s = wait_for(&sup, "restart", Duration::from_secs(5), |s| s.restarts >= 1);
    assert!(
        s.last_error
            .as_deref()
            .unwrap_or_default()
            .contains("protocol"),
        "{s:?}"
    );
    assert!(slowest_send(&sup, 1000) < Duration::from_millis(20));
    sup.shutdown();
    assert_eq!(sup.status().state, State::Stopped);
}
