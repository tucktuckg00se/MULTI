//! HLS web output (M2-3): the API router in process with a real service,
//! real GStreamer and the fake workers.
//!
//! `source.sh` (UDP, H.264, 1 s GOP) -> pipeline -> `hls://t<pid>` (public)
//! and a UDP output. Checks: the master playlist (fetched as an anonymous
//! remote client) has SUBTITLES renditions for en, es and a WebVTT-only
//! language (ar); the video playlist has segments; VTT segments carry
//! `X-TIMESTAMP-MAP` and the fake words; ffprobe sees video and subtitles;
//! stopping and starting the HLS output live cleans and refills its
//! directory; the UDP output never stalls. Prints the caption latency from a
//! caption event (what `/api/events` sends) to its cue in a published VTT
//! segment.
//!
//! This binary is its own fake worker when its first argument is
//! `fake-worker`. Needs `ffmpeg`/`ffprobe`. Ports 9770–9779.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::BTreeMap;
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
use multi::service::{Event, Service, Workers};
use multi::web::{AppState, router};
use multi_core::Config;
use multi_core::config::Output;
use tower::ServiceExt;

const IN_PORT: u16 = 9770;
const UDP_OUT: u16 = 9771;
const WEB_PORT: u16 = 9772;
const FAKE_WORDS: [&str; 6] = multi_fake_worker::SCRIPT;

fn main() -> ExitCode {
    let args: Vec<std::ffi::OsString> = std::env::args_os().collect();
    if args.get(1).is_some_and(|a| a == "fake-worker") {
        let rest = std::iter::once(args[0].clone()).chain(args[2..].iter().cloned());
        return multi_fake_worker::main_from(rest);
    }
    let t = Instant::now();
    match std::panic::catch_unwind(hls_output) {
        Ok(()) => {
            println!("test hls_output ... ok ({} s)", t.elapsed().as_secs());
            println!("\ntest result: 1 passed; 0 failed");
            ExitCode::SUCCESS
        }
        Err(_) => {
            println!("test hls_output ... FAILED");
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

/// Arrival times of datagrams carrying a video PES start on a UDP port.
struct Receiver {
    times: Arc<Mutex<Vec<Instant>>>,
    stop: Arc<AtomicBool>,
}

impl Receiver {
    fn new(port: u16) -> Self {
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
                    let video = buf[..n].as_chunks::<188>().0.iter().any(|p| {
                        let off = if (p[3] >> 4) & 2 != 0 {
                            5 + p[4] as usize
                        } else {
                            4
                        };
                        p[0] == 0x47
                            && p[1] & 0x40 != 0
                            && p.len() > off + 3
                            && p[off..off + 3] == [0, 0, 1]
                            && (0xE0..=0xEF).contains(&p[off + 3])
                    });
                    if video {
                        t.lock().unwrap().push(Instant::now());
                    }
                }
            }
        });
        Self { times, stop }
    }

    fn max_gap(&self, from: Instant, to: Instant) -> Duration {
        let times = self.times.lock().unwrap();
        let w: Vec<&Instant> = times.iter().filter(|x| **x >= from && **x <= to).collect();
        w.windows(2)
            .map(|p| *p[1] - *p[0])
            .max()
            .unwrap_or(Duration::MAX)
    }
}

impl Drop for Receiver {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

/// A request from `peer`; `127.0.0.1` gets full access (no password set).
async fn get(app: &axum::Router, uri: &str, peer: &str) -> (StatusCode, String) {
    let peer: std::net::SocketAddr = format!("{peer}:50000").parse().unwrap();
    let req = Request::builder()
        .method("GET")
        .uri(uri)
        .extension(axum::extract::ConnectInfo(peer))
        .header("host", "localhost")
        .body(Body::empty())
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    let code = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    (code, String::from_utf8_lossy(&bytes).into_owned())
}

async fn post(app: &axum::Router, uri: &str) -> StatusCode {
    let peer: std::net::SocketAddr = "127.0.0.1:50000".parse().unwrap();
    let req = Request::builder()
        .method("POST")
        .uri(uri)
        .extension(axum::extract::ConnectInfo(peer))
        .header("host", "localhost")
        .header("x-multi", "1")
        .body(Body::empty())
        .unwrap();
    app.clone().oneshot(req).await.unwrap().status()
}

fn wait_for(limit: Duration, mut f: impl FnMut() -> bool) -> bool {
    let end = Instant::now() + limit;
    while Instant::now() < end {
        if f() {
            return true;
        }
        sleep(Duration::from_millis(100));
    }
    false
}

/// When each en VTT segment first appeared in `sub_en.m3u8`, and its text.
type Published = Arc<Mutex<BTreeMap<String, (Instant, String)>>>;

fn watch_vtt(dir: PathBuf, stop: Arc<AtomicBool>) -> Published {
    let seen: Published = Arc::default();
    let s = seen.clone();
    std::thread::spawn(move || {
        while !stop.load(Ordering::Relaxed) {
            if let Ok(pl) = fs::read_to_string(dir.join("sub_en.m3u8")) {
                for name in pl.lines().filter(|l| l.ends_with(".vtt")) {
                    let mut m = s.lock().unwrap();
                    if !m.contains_key(name)
                        && let Ok(text) = fs::read_to_string(dir.join(name))
                    {
                        m.insert(name.to_string(), (Instant::now(), text));
                    }
                }
            }
            sleep(Duration::from_millis(20));
        }
    });
    seen
}

fn hls_output() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    let dir = std::env::temp_dir().join(format!("multi-hls-test-{}", std::process::id()));
    fs::create_dir_all(&dir).unwrap();
    println!("work dir {}", dir.display());
    let log = fs::File::create(dir.join("multi.log")).unwrap();
    tracing_subscriber::fmt()
        .with_writer(Mutex::new(log))
        .with_ansi(false)
        .init();
    let wav = dir.join("tone.wav");
    let ok = Command::new("ffmpeg")
        .args(["-hide_banner", "-loglevel", "error", "-y", "-f", "lavfi"])
        .args([
            "-i",
            "sine=frequency=440:duration=120",
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

    let name = format!("t{}", std::process::id());
    let hls_dir = multi_media::hls::output_dir(&name);
    let mut cfg = Config::default();
    cfg.input.url = format!("udp://127.0.0.1:{IN_PORT}");
    cfg.outputs = vec![
        Output {
            public: true,
            ..Output::new(format!("hls://{name}?segment_s=2&window=6"))
        },
        Output::new(format!("udp://127.0.0.1:{UDP_OUT}")),
    ];
    // A language carried only as WebVTT (a script 608/708 cannot show).
    cfg.languages[2].code = "ar".into();
    cfg.languages[2].cea708_service = None;
    cfg.web.port = WEB_PORT;
    assert!(cfg.validate().is_empty(), "{:?}", cfg.validate());
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
    let udp = Receiver::new(UDP_OUT);
    let rt = tokio::runtime::Runtime::new().unwrap();

    // Caption events, as /api/events sends them.
    let events: Arc<Mutex<Vec<(Instant, String, String)>>> = Arc::default();
    let stop = Arc::new(AtomicBool::new(false));
    {
        let mut rx = service.events();
        let (ev, stop) = (events.clone(), stop.clone());
        std::thread::spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                match rx.blocking_recv() {
                    Ok(Event::Caption { lang, text, .. }) => {
                        ev.lock().unwrap().push((Instant::now(), lang, text))
                    }
                    Ok(_) | Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                    Err(_) => break,
                }
            }
        });
    }
    let published = watch_vtt(hls_dir.clone(), stop.clone());

    assert_eq!(rt.block_on(post(&app, "/api/start")), StatusCode::OK);
    let t_start = Instant::now();
    let _source = Proc(
        Command::new(root.join("spikes/harness/source.sh"))
            .arg("--audio")
            .arg(&wav)
            .arg(format!("udp://127.0.0.1:{IN_PORT}?pkt_size=1316"))
            .stdout(Stdio::null())
            .stderr(Stdio::from(
                fs::File::create(dir.join("source.log")).unwrap(),
            ))
            .spawn()
            .expect("source.sh"),
    );

    // The master playlist, fetched by an anonymous remote viewer (public).
    let remote = "203.0.113.7";
    let master_url = format!("/hls/{name}/master.m3u8");
    let mut master = String::new();
    assert!(
        wait_for(Duration::from_secs(30), || {
            let (code, body) = rt.block_on(get(&app, &master_url, remote));
            master = body;
            code == StatusCode::OK
        }),
        "no master playlist"
    );
    println!(
        "master playlist after {:.1} s:\n{master}",
        t_start.elapsed().as_secs_f64()
    );
    for l in ["en", "es", "ar"] {
        assert!(
            master.contains(&format!(
                "TYPE=SUBTITLES,GROUP-ID=\"subs\",LANGUAGE=\"{l}\""
            )),
            "{l} missing"
        );
    }
    assert!(master.contains("LANGUAGE=\"en\",NAME=\"English\",DEFAULT=YES"));
    assert!(master.contains("NAME=\"العربية\""));
    assert!(master.contains("SUBTITLES=\"subs\""));
    rt.block_on(async {
        let (code, page) = get(&app, &format!("/watch/{name}"), remote).await;
        assert_eq!(code, StatusCode::OK);
        assert!(page.contains("hls.min.js"));
        assert_eq!(get(&app, "/hls.min.js", remote).await.0, StatusCode::OK);
        // Public covers this output's page and stream only.
        assert_eq!(
            get(&app, "/api/status", remote).await.0,
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(
            get(&app, "/watch/other", "127.0.0.1").await.0,
            StatusCode::NOT_FOUND
        );
        let (code, video) = get(&app, &format!("/hls/{name}/video.m3u8"), remote).await;
        assert_eq!(code, StatusCode::OK);
        assert!(video.contains("#EXTINF"), "{video}");
    });

    // VTT segments with the fake words, in every language.
    let seg_with = |lang: &str, needle: &str| -> Option<String> {
        let pl = fs::read_to_string(hls_dir.join(format!("sub_{lang}.m3u8"))).ok()?;
        pl.lines()
            .filter(|l| l.ends_with(".vtt"))
            .filter_map(|l| fs::read_to_string(hls_dir.join(l)).ok())
            .find(|t| t.contains(needle))
    };
    let mut en = None;
    assert!(
        wait_for(Duration::from_secs(20), || {
            en = FAKE_WORDS.iter().find_map(|w| seg_with("en", w));
            en.is_some()
        }),
        "no en cue"
    );
    let en = en.unwrap();
    println!("an en VTT segment:\n{en}");
    assert!(en.starts_with("WEBVTT\nX-TIMESTAMP-MAP=MPEGTS:"), "{en}");
    assert!(en.contains(",LOCAL:00:00:00.000\n"));
    assert!(en.contains(" --> "));
    for l in ["es", "ar"] {
        assert!(
            wait_for(Duration::from_secs(15), || seg_with(l, &format!("[{l}]"))
                .is_some()),
            "no {l} cue"
        );
    }

    // ffprobe reads the master playlist: video and WebVTT subtitles.
    let probe = Command::new("timeout")
        .args(["30", "ffprobe", "-v", "error", "-show_entries"])
        .args([
            "stream=codec_type,codec_name:stream_tags=language",
            "-of",
            "csv=p=0",
        ])
        .arg(hls_dir.join("master.m3u8"))
        .output()
        .unwrap();
    let probe = String::from_utf8_lossy(&probe.stdout).into_owned();
    println!("ffprobe master.m3u8:\n{probe}");
    assert!(probe.contains("h264,video"), "{probe}");
    assert!(probe.contains("webvtt,subtitle"), "{probe}");

    // Latency: caption event -> first published en VTT segment with its text.
    // Let captions run 20 s past the warm-up (8 s); match events that have
    // had 5 s to reach a segment.
    let until = t_start + Duration::from_secs(33);
    sleep(until.saturating_duration_since(Instant::now()));
    let (from, to) = (
        t_start + Duration::from_secs(8),
        until - Duration::from_secs(5),
    );
    let mut lat: Vec<f64> = {
        let ev = events.lock().unwrap();
        let pubs = published.lock().unwrap();
        ev.iter()
            .filter(|(t, l, _)| l == "en" && *t > from && *t < to)
            .filter_map(|(t, _, text)| {
                pubs.values()
                    .filter(|(p, body)| p > t && body.contains(text.trim()))
                    .map(|(p, _)| (*p - *t).as_secs_f64())
                    .reduce(f64::min)
            })
            .collect()
    };
    println!(
        "{} caption events, {} en VTT segments seen",
        events.lock().unwrap().len(),
        published.lock().unwrap().len()
    );
    lat.sort_by(f64::total_cmp);
    assert!(
        lat.len() > 5,
        "too few caption events matched: {}",
        lat.len()
    );
    println!(
        "caption event -> cue in a published VTT segment: n={} median {:.2} s, p90 {:.2} s, max {:.2} s",
        lat.len(),
        lat[lat.len() / 2],
        lat[lat.len() * 9 / 10],
        lat[lat.len() - 1]
    );
    let video = fs::read_to_string(hls_dir.join("video.m3u8")).unwrap();
    let durs: Vec<f64> = video
        .lines()
        .filter_map(|l| l.strip_prefix("#EXTINF:"))
        .filter_map(|d| d.trim_end_matches(',').parse().ok())
        .collect();
    let tail: f64 = durs.iter().rev().take(3).sum();
    println!(
        "segments {:?}; a player 3 segments behind the live edge starts {tail:.1} s behind the newest segment's end",
        durs
    );

    // Stop and start the HLS output live: directory removed, then refilled.
    let t_stop = Instant::now();
    assert_eq!(
        rt.block_on(post(&app, "/api/outputs/0/stop")),
        StatusCode::OK
    );
    assert!(
        wait_for(Duration::from_secs(3), || !hls_dir.exists()),
        "HLS directory left after stop"
    );
    sleep(Duration::from_secs(2));
    assert_eq!(
        rt.block_on(post(&app, "/api/outputs/0/start")),
        StatusCode::OK
    );
    assert!(
        wait_for(Duration::from_secs(15), || hls_dir
            .join("master.m3u8")
            .exists()),
        "HLS did not come back"
    );
    let gap = udp.max_gap(t_start + Duration::from_secs(8), Instant::now());
    println!(
        "UDP output: longest gap between video datagrams {:.0} ms (HLS stopped and started at {:.0} s)",
        gap.as_secs_f64() * 1000.0,
        (t_stop - t_start).as_secs_f64()
    );
    assert!(
        gap < Duration::from_millis(500),
        "UDP output stalled: {gap:?}"
    );

    assert_eq!(rt.block_on(post(&app, "/api/stop")), StatusCode::OK);
    stop.store(true, Ordering::Relaxed);
    assert!(
        wait_for(Duration::from_secs(5), || !hls_dir.exists()),
        "HLS directory left after stopping the pipeline"
    );
    drop(udp);
    let _ = fs::remove_dir_all(&dir);
}
