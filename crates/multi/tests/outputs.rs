//! Per-output control, live (WP10): the API router in process with a real
//! service, real GStreamer and the fake workers.
//!
//! `source.sh` (UDP, H.264, 30 fps) -> pipeline -> UDP outputs A and B.
//! Stop B through `POST /api/outputs/1/stop` (saved as `enabled = false`),
//! start it again, then add a third output C with `PUT /api/config`. A must
//! never have a gap longer than 2 frames around any of these changes, and no
//! restart may be needed.
//!
//! This binary is its own fake worker when its first argument is
//! `fake-worker`. Needs `ffmpeg`. Ports 9750–9754.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::fs;
use std::net::UdpSocket;
use std::path::PathBuf;
use std::process::{Child, Command, ExitCode, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
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

const IN_PORT: u16 = 9750;
const OUT: [u16; 3] = [9751, 9752, 9753];
const WEB_PORT: u16 = 9754;
/// `source.sh` sends here; the test taps it and relays to `IN_PORT`.
const TAP_PORT: u16 = 9755;
/// Extra frames in flight allowed on A while another output changes.
const MAX_EXTRA_FRAMES: i64 = 2;

fn main() -> ExitCode {
    let args: Vec<std::ffi::OsString> = std::env::args_os().collect();
    if args.get(1).is_some_and(|a| a == "fake-worker") {
        let rest = std::iter::once(args[0].clone()).chain(args[2..].iter().cloned());
        return multi_fake_worker::main_from(rest);
    }
    let t = Instant::now();
    match std::panic::catch_unwind(live_outputs) {
        Ok(()) => {
            println!("test live_outputs ... ok ({} s)", t.elapsed().as_secs());
            println!("\ntest result: 1 passed; 0 failed");
            ExitCode::SUCCESS
        }
        Err(_) => {
            println!("test live_outputs ... FAILED");
            println!("\ntest result: 0 passed; 1 failed");
            ExitCode::FAILURE
        }
    }
}

struct Proc(Child);

impl Drop for Proc {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Number of video PES starts (one per frame) in MPEG-TS datagram `d`.
fn frame_starts(d: &[u8]) -> usize {
    d.as_chunks::<188>()
        .0
        .iter()
        .filter(|p| {
            if p[0] != 0x47 || p[1] & 0x40 == 0 {
                return false;
            }
            let afc = (p[3] >> 4) & 3;
            let off = if afc & 2 != 0 { 5 + p[4] as usize } else { 4 };
            afc & 1 != 0
                && p.len() > off + 3
                && p[off..off + 3] == [0, 0, 1]
                && (0xE0..=0xEF).contains(&p[off + 3])
        })
        .count()
}

/// Arrival time of every video frame on a UDP port; optionally relays the
/// datagrams on (a tap on the pipeline's input).
struct Receiver {
    times: Arc<Mutex<Vec<Instant>>>,
    stop: Arc<AtomicBool>,
}

impl Receiver {
    fn new(port: u16, relay_to: Option<u16>) -> Self {
        let sock = UdpSocket::bind(("127.0.0.1", port)).unwrap();
        sock.set_read_timeout(Some(Duration::from_millis(50)))
            .unwrap();
        let times = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let (t, s) = (times.clone(), stop.clone());
        std::thread::spawn(move || {
            let mut buf = [0u8; 65536];
            while !s.load(Ordering::Relaxed) {
                if let Ok(n) = sock.recv(&mut buf) {
                    let now = Instant::now();
                    if let Some(to) = relay_to {
                        let _ = sock.send_to(&buf[..n], ("127.0.0.1", to));
                    }
                    let k = frame_starts(&buf[..n]);
                    t.lock().unwrap().extend(std::iter::repeat_n(now, k));
                }
            }
        });
        Self { times, stop }
    }

    fn count_after(&self, t: Instant) -> usize {
        self.times
            .lock()
            .unwrap()
            .iter()
            .filter(|x| **x > t)
            .count()
    }

    fn wait_for_frames(&self, after: Instant, limit: Duration) -> bool {
        let end = Instant::now() + limit;
        while Instant::now() < end {
            if self.count_after(after) > 15 {
                return true;
            }
            sleep(Duration::from_millis(50));
        }
        false
    }

    /// Longest gap between frames in `[from, to]` (includes input jitter).
    fn max_gap(&self, from: Instant, to: Instant) -> Duration {
        let times = self.times.lock().unwrap();
        let w: Vec<&Instant> = times.iter().filter(|x| **x >= from && **x <= to).collect();
        w.windows(2)
            .map(|p| *p[1] - *p[0])
            .max()
            .unwrap_or(Duration::MAX)
    }
}

/// Frames in flight between the input tap and output `out` (input frames
/// seen minus output frames seen, plus a constant) at each frame event in
/// `[from, to]`, sorted. A stall or dropped frames on the output raises it;
/// input jitter does not, since the output follows the input.
fn in_flight(input: &Receiver, out: &Receiver, from: Instant, to: Instant) -> Vec<i64> {
    let a = input.times.lock().unwrap().clone();
    let b = out.times.lock().unwrap().clone();
    let count = |v: &[Instant], t: Instant| v.partition_point(|x| *x <= t) as i64;
    let events = a.iter().chain(&b).filter(|t| **t >= from && **t <= to);
    let mut lags: Vec<i64> = events.map(|t| count(&a, *t) - count(&b, *t)).collect();
    assert!(lags.len() > 20, "too few frames in the window");
    lags.sort_unstable();
    lags
}

impl Drop for Receiver {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

async fn call(
    app: &axum::Router,
    method: &str,
    uri: &str,
    body: Option<&serde_json::Value>,
) -> (StatusCode, serde_json::Value) {
    let req = Request::builder()
        .method(method)
        .uri(uri)
        .header("host", "localhost")
        .header("x-multi", "1")
        .header("content-type", "application/json")
        .body(body.map_or_else(Body::empty, |b| Body::from(b.to_string())))
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    let code = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    (code, serde_json::from_slice(&bytes).unwrap_or_default())
}

fn live_outputs() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    let dir = std::env::temp_dir().join(format!("multi-outputs-{}", std::process::id()));
    fs::create_dir_all(&dir).unwrap();
    println!("work dir {}", dir.display());
    let wav = dir.join("tone.wav");
    let ok = Command::new("ffmpeg")
        .args(["-hide_banner", "-loglevel", "error", "-y", "-f", "lavfi"])
        .args([
            "-i",
            "sine=frequency=440:duration=90",
            "-ar",
            "48000",
            "-ac",
            "2",
        ])
        .arg(&wav)
        .status()
        .unwrap()
        .success();
    assert!(ok, "ffmpeg tone");

    let mut cfg = Config::default();
    cfg.input.url = format!("udp://127.0.0.1:{IN_PORT}");
    cfg.outputs = vec![
        Output::new(format!("udp://127.0.0.1:{}", OUT[0])),
        Output {
            name: Some("B".into()),
            ..Output::new(format!("udp://127.0.0.1:{}", OUT[1]))
        },
    ];
    cfg.web.port = WEB_PORT;
    let path = dir.join("multi.toml");
    fs::write(&path, cfg.to_toml().unwrap()).unwrap();
    let me = std::env::current_exe().unwrap().display().to_string();
    let workers = Workers {
        asr: Some(format!("{me} fake-worker asr")),
        mt: Some(format!("{me} fake-worker mt")),
        models_dir: None,
    };
    let service = Service::new(cfg.clone(), workers);
    let (_tx, rx) = tokio::sync::watch::channel(false);
    let app = router(AppState::new(service.clone(), cfg, path.clone(), None, rx));
    let recv: Vec<Receiver> = OUT.iter().map(|p| Receiver::new(*p, None)).collect();
    let tap = Receiver::new(TAP_PORT, Some(IN_PORT));
    let rt = tokio::runtime::Runtime::new().unwrap();

    rt.block_on(async {
        let (code, st) = call(&app, "POST", "/api/start", None).await;
        assert_eq!(code, StatusCode::OK, "{st}");
    });
    let _source = Proc(
        Command::new(root.join("spikes/harness/source.sh"))
            .arg("--audio")
            .arg(&wav)
            .arg(format!("udp://127.0.0.1:{TAP_PORT}?pkt_size=1316"))
            .stdout(Stdio::null())
            .stderr(Stdio::from(
                fs::File::create(dir.join("source.log")).unwrap(),
            ))
            .spawn()
            .expect("source.sh"),
    );
    let t0 = Instant::now();
    assert!(
        recv[0].wait_for_frames(t0, Duration::from_secs(20)),
        "A never received"
    );
    assert!(
        recv[1].wait_for_frames(t0, Duration::from_secs(5)),
        "B never received"
    );
    sleep(Duration::from_secs(5));
    // Steady state: frames in flight input -> A (median is the baseline),
    // and A's longest gap between frames (input jitter included).
    let now = Instant::now();
    let steady = in_flight(&tap, &recv[0], now - Duration::from_secs(3), now);
    let base = steady[steady.len() / 2];
    println!(
        "A steady state: frames in flight min {} / median {base} / max {}, longest gap between frames {:.1} ms",
        steady[0],
        steady[steady.len() - 1],
        ms(recv[0].max_gap(now - Duration::from_secs(3), now))
    );
    // Extra frames in flight on A (over the median) in a window around a
    // change, and A's longest gap there.
    let extra = |from: Instant, what: &str| {
        let now = Instant::now();
        let w = in_flight(&tap, &recv[0], from, now);
        let e = w[w.len() - 1] - base;
        println!(
            "A around {what}: at most {e} frame(s) over the median in flight, longest gap between frames {:.1} ms",
            ms(recv[0].max_gap(from, now))
        );
        e
    };

    // Stop B: saved, B goes quiet, A carries on.
    let t_stop = Instant::now();
    rt.block_on(async {
        let (code, r) = call(&app, "POST", "/api/outputs/1/stop", None).await;
        assert_eq!(code, StatusCode::OK, "{r}");
        assert!(r["report"]["restart"].as_array().unwrap().is_empty(), "{r}");
        assert_eq!(r["config"]["outputs"][1]["enabled"], false);
        let (code, _) = call(&app, "POST", "/api/outputs/7/stop", None).await;
        assert_eq!(code, StatusCode::NOT_FOUND);
    });
    let saved = Config::load(&path).unwrap();
    assert!(
        !saved.outputs[1].enabled && saved.outputs[0].enabled,
        "enabled not saved"
    );
    assert_eq!(saved.outputs[1].name.as_deref(), Some("B"));
    sleep(Duration::from_secs(3));
    let quiet = t_stop + Duration::from_millis(500);
    assert_eq!(recv[1].count_after(quiet), 0, "B still sending after stop");
    let gap_stop = extra(t_stop - Duration::from_millis(200), "stopping B");
    rt.block_on(async {
        let (_, st) = call(&app, "GET", "/api/status", None).await;
        let outs = &st["media"]["outputs"];
        assert_eq!(outs[1]["enabled"], false, "{st}");
        assert_eq!(outs[1]["running"], false, "{st}");
        assert_eq!(outs[1]["name"], "B", "{st}");
        assert_eq!(outs[0]["running"], true, "{st}");
    });

    // Start B again.
    let t_start = Instant::now();
    rt.block_on(async {
        let (code, r) = call(&app, "POST", "/api/outputs/1/start", None).await;
        assert_eq!(code, StatusCode::OK, "{r}");
    });
    assert!(Config::load(&path).unwrap().outputs[1].enabled);
    assert!(
        recv[1].wait_for_frames(t_start, Duration::from_secs(3)),
        "B did not resume"
    );
    sleep(Duration::from_secs(1));
    let gap_start = extra(t_start - Duration::from_millis(200), "starting B");

    // Add C with a config save: live, no restart.
    let t_add = Instant::now();
    rt.block_on(async {
        let (_, mut c) = call(&app, "GET", "/api/config", None).await;
        c["outputs"]
            .as_array_mut()
            .unwrap()
            .push(serde_json::json!({ "url": format!("udp://127.0.0.1:{}", OUT[2]), "name": "C" }));
        let (code, r) = call(&app, "PUT", "/api/config", Some(&c)).await;
        assert_eq!(code, StatusCode::OK, "{r}");
        assert!(r["report"]["restart"].as_array().unwrap().is_empty(), "{r}");
    });
    assert!(
        recv[2].wait_for_frames(t_add, Duration::from_secs(3)),
        "C did not start"
    );
    sleep(Duration::from_secs(1));
    let gap_add = extra(t_add - Duration::from_millis(200), "adding C");
    rt.block_on(async {
        let (_, st) = call(&app, "GET", "/api/status", None).await;
        let outs = st["media"]["outputs"].as_array().unwrap().clone();
        assert_eq!(outs.len(), 3, "{st}");
        assert!(outs.iter().all(|o| o["running"] == true), "{st}");
        assert_eq!(outs[2]["id"], 2, "{st}");
        assert!(st["restart_pending"].as_array().unwrap().is_empty(), "{st}");
        let (code, _) = call(&app, "POST", "/api/stop", None).await;
        assert_eq!(code, StatusCode::OK);
    });

    for (what, e) in [("stop", gap_stop), ("start", gap_start), ("add", gap_add)] {
        assert!(
            e <= MAX_EXTRA_FRAMES,
            "A fell {e} frames behind when another output was changed ({what})"
        );
    }
    drop(tap);
    drop(recv);
    let _ = fs::remove_dir_all(&dir);
}

fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1000.0
}
