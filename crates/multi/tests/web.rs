//! `multi serve` with the fake workers.
//!
//! 1. `start_stop`: the API router in process, `POST /api/start` and
//!    `/api/stop` against a real service with fake workers (no input needed).
//! 2. `serve_end_to_end`: the `multi serve` binary; PUT a config, start,
//!    feed `source.sh` over UDP, and wait for a caption on `/api/events`.
//!
//! This binary is its own fake worker when its first argument is
//! `fake-worker`. Needs `ffmpeg` and `gst-launch-1.0`. Ports 9740–9746.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::fs;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitCode, Stdio};
use std::thread::sleep;
use std::time::{Duration, Instant};

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use multi::service::{Service, Workers};
use multi::web::{AppState, router};
use multi_core::Config;
use multi_core::config::Output;
use tower::ServiceExt;

const WEB_PORT: u16 = 9743;
const IN_PORT: u16 = 9744;
const OUT_PORT: u16 = 9745;

fn main() -> ExitCode {
    let args: Vec<std::ffi::OsString> = std::env::args_os().collect();
    if args.get(1).is_some_and(|a| a == "fake-worker") {
        let rest = std::iter::once(args[0].clone()).chain(args[2..].iter().cloned());
        return multi_fake_worker::main_from(rest);
    }
    let tests: [(&str, fn()); 2] = [
        ("start_stop", start_stop),
        ("serve_end_to_end", serve_end_to_end),
    ];
    let mut failed = 0;
    for (name, f) in tests {
        let t = Instant::now();
        match std::panic::catch_unwind(f) {
            Ok(()) => println!("test {name} ... ok ({} s)", t.elapsed().as_secs()),
            Err(_) => {
                println!("test {name} ... FAILED");
                failed += 1;
            }
        }
    }
    println!("\ntest result: {} passed; {failed} failed", 2 - failed);
    if failed == 0 {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

fn me() -> String {
    std::env::current_exe().unwrap().display().to_string()
}

fn workers() -> Workers {
    Workers {
        asr: Some(format!("{} fake-worker asr", me())),
        mt: Some(format!("{} fake-worker mt", me())),
        models_dir: None,
    }
}

fn config(in_port: u16, out_port: u16) -> Config {
    let mut c = Config::default();
    c.input.url = format!("udp://127.0.0.1:{in_port}");
    c.outputs = vec![Output {
        url: format!("udp://127.0.0.1:{out_port}"),
    }];
    c.web.port = WEB_PORT;
    c
}

fn work_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("multi-web-{name}-{}", std::process::id()));
    fs::create_dir_all(&dir).unwrap();
    dir
}

// ---------------------------------------------------------------- in process

async fn call(app: &axum::Router, method: &str, uri: &str) -> (StatusCode, serde_json::Value) {
    let req = Request::builder()
        .method(method)
        .uri(uri)
        .header("host", "localhost")
        .header("x-multi", "1")
        .body(Body::empty())
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    let code = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    (code, serde_json::from_slice(&bytes).unwrap_or_default())
}

fn start_stop() {
    let dir = work_dir("api");
    let cfg = config(9740, 9741);
    let path = dir.join("multi.toml");
    fs::write(&path, cfg.to_toml().unwrap()).unwrap();
    let service = Service::new(cfg.clone(), workers());
    let (_tx, rx) = tokio::sync::watch::channel(false);
    let app = router(AppState::new(service.clone(), cfg, path, None, rx));
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        let (code, st) = call(&app, "POST", "/api/start").await;
        assert_eq!(code, StatusCode::OK, "{st}");
        let (code, _) = call(&app, "POST", "/api/start").await;
        assert_eq!(code, StatusCode::CONFLICT);
        let end = Instant::now() + Duration::from_secs(15);
        loop {
            let (_, st) = call(&app, "GET", "/api/status").await;
            let ready = st["state"] == "running"
                && st["workers"]
                    .as_array()
                    .is_some_and(|w| w.len() == 2 && w.iter().all(|w| w["state"] == "ready"));
            if ready {
                assert!(st["uptime_s"].is_number());
                break;
            }
            assert!(Instant::now() < end, "workers never ready: {st}");
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
        let (code, st) = call(&app, "POST", "/api/stop").await;
        assert_eq!(code, StatusCode::OK);
        assert_eq!(st["state"], "stopped", "{st}");
        assert!(!service.is_active());
    });
    let _ = fs::remove_dir_all(dir);
}

// ---------------------------------------------------------------- end to end

struct Proc(Child);

impl Drop for Proc {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn log_file(dir: &Path, name: &str) -> Stdio {
    Stdio::from(fs::File::create(dir.join(name)).unwrap())
}

/// One HTTP/1.1 request; returns (status code, body).
fn http(method: &str, path: &str, body: Option<&str>) -> (u16, String) {
    let mut s = TcpStream::connect(("127.0.0.1", WEB_PORT)).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(30))).unwrap();
    let body = body.unwrap_or("");
    write!(
        s,
        "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1:{WEB_PORT}\r\nX-Multi: 1\r\n\
         Content-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
    .unwrap();
    let mut out = String::new();
    s.read_to_string(&mut out).unwrap();
    let code = out
        .split_whitespace()
        .nth(1)
        .and_then(|c| c.parse().ok())
        .unwrap_or(0);
    let body = out
        .split_once("\r\n\r\n")
        .map_or("", |(_, b)| b)
        .to_string();
    (code, body)
}

fn serve_end_to_end() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    let dir = work_dir("e2e");
    println!("work dir {}", dir.display());
    let path = dir.join("multi.toml");
    // Only the port: everything else is set through the API.
    fs::write(&path, format!("[web]\nport = {WEB_PORT}\n")).unwrap();

    let mut multi = Proc(
        Command::new(env!("CARGO_BIN_EXE_multi"))
            .arg("serve")
            .arg("--config")
            .arg(&path)
            .arg("--asr-worker")
            .arg(format!("{} fake-worker asr", me()))
            .arg("--mt-worker")
            .arg(format!("{} fake-worker mt", me()))
            .env("RUST_LOG", "info")
            .env_remove("MULTI_WEB_TOKEN")
            .stdout(Stdio::null())
            .stderr(log_file(&dir, "multi.log"))
            .spawn()
            .expect("multi serve"),
    );
    let end = Instant::now() + Duration::from_secs(10);
    while TcpStream::connect(("127.0.0.1", WEB_PORT)).is_err() {
        assert!(Instant::now() < end, "multi serve did not listen");
        sleep(Duration::from_millis(100));
    }
    let (code, page) = http("GET", "/", None);
    assert_eq!(code, 200);
    assert!(page.contains("app.js"));

    let body = serde_json::to_string(&config(IN_PORT, OUT_PORT)).unwrap();
    let (code, resp) = http("PUT", "/api/config", Some(&body));
    assert_eq!(code, 200, "{resp}");
    assert!(
        fs::read_to_string(&path)
            .unwrap()
            .contains(&format!("udp://127.0.0.1:{IN_PORT}"))
    );

    // Listen for events before starting.
    let mut sse = TcpStream::connect(("127.0.0.1", WEB_PORT)).unwrap();
    sse.set_read_timeout(Some(Duration::from_secs(1))).unwrap();
    write!(
        sse,
        "GET /api/events HTTP/1.1\r\nHost: 127.0.0.1:{WEB_PORT}\r\nAccept: text/event-stream\r\n\r\n"
    )
    .unwrap();

    let (code, resp) = http("POST", "/api/start", None);
    assert_eq!(code, 200, "{resp}");

    let wav = dir.join("tone.wav");
    let ok = Command::new("ffmpeg")
        .args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-y",
            "-f",
            "lavfi",
            "-i",
        ])
        .arg("sine=frequency=440:duration=60")
        .args(["-ar", "48000", "-ac", "2"])
        .arg(&wav)
        .status()
        .unwrap();
    assert!(ok.success());
    let _source = Proc(
        Command::new(root.join("spikes/harness/source.sh"))
            .arg("--audio")
            .arg(&wav)
            .arg(format!("udp://127.0.0.1:{IN_PORT}?pkt_size=1316"))
            .stdout(Stdio::null())
            .stderr(log_file(&dir, "source.log"))
            .spawn()
            .expect("source.sh"),
    );

    let mut seen = String::new();
    let (mut caption, mut stats) = (false, false);
    let end = Instant::now() + Duration::from_secs(40);
    let mut buf = [0u8; 8192];
    while Instant::now() < end && !(caption && stats) {
        match sse.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => seen.push_str(&String::from_utf8_lossy(&buf[..n])),
            Err(_) => {}
        }
        caption |= seen.contains(r#""type":"caption""#) && seen.contains(r#""lang":"en""#);
        stats |= seen.contains(r#""type":"stats""#) && seen.contains(r#""state":"running""#);
    }
    assert!(stats, "no running stats event");
    assert!(caption, "no caption event in {} bytes", seen.len());
    let tail: String = seen
        .lines()
        .filter(|l| l.contains("caption"))
        .take(2)
        .collect();
    println!(
        "caption events, e.g. {}",
        tail.chars().take(200).collect::<String>()
    );

    let (code, st) = http("GET", "/api/status", None);
    assert_eq!(code, 200);
    assert!(st.contains(r#""frames_in""#), "{st}");

    Command::new("kill")
        .args(["-INT", &multi.0.id().to_string()])
        .status()
        .unwrap();
    let end = Instant::now() + Duration::from_secs(20);
    let status = loop {
        if let Some(s) = multi.0.try_wait().unwrap() {
            break s;
        }
        assert!(
            Instant::now() < end,
            "multi serve did not stop after SIGINT"
        );
        sleep(Duration::from_millis(100));
    };
    assert!(status.success(), "multi serve exit: {status}");
    let _ = fs::remove_dir_all(dir);
}
