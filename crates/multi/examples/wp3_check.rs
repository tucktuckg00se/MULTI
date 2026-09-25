//! Local real-model check for M1 WP3 (not run in CI; needs models and the
//! release builds of `multi-asr` and `multi-mt`).
//!
//! 1. Streams a 16 kHz mono WAV through the supervisor into `multi-asr` at
//!    1x real time; reports words, WER against a reference, and word lag.
//! 2. Kills `multi-asr` with SIGKILL mid-stream and times the recovery.
//! 3. Sends 20 clauses through `multi-mt` (es, fr, de); reports P50/P95.
//! 4. Kills `multi-mt` with SIGKILL mid-run and times the recovery.
//!
//! ```sh
//! cargo run --release -p multi --example wp3_check -- --help
//! ```

use anyhow::{Context, Result, bail};
use clap::Parser;
use multi::supervisor::{Policy, State, Supervisor, WorkerSpec};
use multi_core::Clause;
use multi_core::ipc::Message;
use std::path::{Path, PathBuf};
use std::sync::mpsc::Receiver;
use std::time::{Duration, Instant};

#[derive(Parser)]
struct Args {
    #[arg(long, default_value = "target/release/multi-asr")]
    asr_bin: PathBuf,
    #[arg(long, default_value = "target/release/multi-mt")]
    mt_bin: PathBuf,
    #[arg(long)]
    asr_model_dir: PathBuf,
    #[arg(long)]
    mt_models: PathBuf,
    #[arg(long)]
    wav: PathBuf,
    /// Reference transcript for WER (plain words).
    #[arg(long)]
    reference: Option<PathBuf>,
    #[arg(long, default_value = "auto")]
    device: String,
    /// Skip the full-length ASR run (only the kill tests and MT).
    #[arg(long)]
    skip_full: bool,
}

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "warn".into()),
        )
        .with_writer(std::io::stderr)
        .init();
    let a = Args::parse();
    let audio = read_wav(&a.wav)?;
    println!(
        "input: {} ({:.1} s)",
        a.wav.display(),
        audio.len() as f64 / 16_000.0
    );
    let asr = WorkerSpec::new("asr", &a.asr_bin).args([
        "--model-dir".into(),
        a.asr_model_dir.clone().into_os_string(),
        "--device".into(),
        a.device.clone().into(),
    ]);
    if !a.skip_full {
        asr_full(&asr, &audio, a.reference.as_deref())?;
    }
    asr_kill(&asr, &audio)?;
    let mt = WorkerSpec::new("mt", &a.mt_bin).args([
        "--models".into(),
        a.mt_models.clone().into_os_string(),
        "--device".into(),
        a.device.clone().into(),
        "--langs".into(),
        "es,fr,de".into(),
    ]);
    let clauses = clauses(a.reference.as_deref(), 20)?;
    mt_latency(&mt, &clauses)?;
    mt_kill(&mt, &clauses)?;
    Ok(())
}

fn read_wav(path: &Path) -> Result<Vec<i16>> {
    let bytes = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    if bytes.get(0..4) != Some(b"RIFF") || bytes.get(8..12) != Some(b"WAVE") {
        bail!("not a WAV file");
    }
    let mut pos = 12;
    let mut fmt_ok = false;
    while pos + 8 <= bytes.len() {
        let id = &bytes[pos..pos + 4];
        let len = u32::from_le_bytes(bytes[pos + 4..pos + 8].try_into()?) as usize;
        let body = bytes
            .get(pos + 8..pos + 8 + len)
            .unwrap_or(&bytes[pos + 8..]);
        if id == b"fmt " && body.len() >= 16 {
            let ch = u16::from_le_bytes(body[2..4].try_into()?);
            let sr = u32::from_le_bytes(body[4..8].try_into()?);
            let bits = u16::from_le_bytes(body[14..16].try_into()?);
            if (ch, sr, bits) != (1, 16_000, 16) {
                bail!("need 16 kHz mono 16-bit PCM, got {ch} ch {sr} Hz {bits} bit");
            }
            fmt_ok = true;
        } else if id == b"data" && fmt_ok {
            return Ok(body
                .as_chunks::<2>()
                .0
                .iter()
                .map(|b| i16::from_le_bytes(*b))
                .collect());
        }
        pos += 8 + len + (len & 1);
    }
    bail!("no fmt/data chunk")
}

fn wait_ready(sup: &Supervisor, limit: Duration) -> Result<Duration> {
    let t = Instant::now();
    while sup.status().state != State::Ready {
        if t.elapsed() > limit {
            bail!("worker not ready: {:?}", sup.status());
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    Ok(t.elapsed())
}

fn kill9(sup: &Supervisor) -> Result<Instant> {
    let pid = sup.status().pid.context("no worker pid")?;
    let st = std::process::Command::new("kill")
        .args(["-9", &pid.to_string()])
        .status()?;
    if !st.success() {
        bail!("kill -9 {pid} failed");
    }
    Ok(Instant::now())
}

fn pct(v: &mut [f64], p: f64) -> f64 {
    if v.is_empty() {
        return f64::NAN;
    }
    v.sort_by(f64::total_cmp);
    let i = ((v.len() - 1) as f64 * p).round() as usize;
    v[i.min(v.len() - 1)]
}

/// 100 ms frames paced at real time. `on_frame(i)` runs before frame `i`.
fn stream(
    sup: &Supervisor,
    audio: &[i16],
    mut on_frame: impl FnMut(usize) -> Result<()>,
) -> Result<Instant> {
    let t0 = Instant::now();
    for (i, chunk) in audio.chunks(1600).enumerate() {
        on_frame(i)?;
        let due = t0 + Duration::from_millis(i as u64 * 100);
        if let Some(d) = due.checked_duration_since(Instant::now()) {
            std::thread::sleep(d);
        }
        sup.send_pcm(i as u64 * 100, chunk.to_vec());
    }
    Ok(t0)
}

struct Got {
    at: Instant,
    words: Vec<(String, u64)>,
}

fn collect(rx: Receiver<Message>) -> std::thread::JoinHandle<Vec<Got>> {
    std::thread::spawn(move || {
        let mut v = Vec::new();
        while let Ok(m) = rx.recv() {
            if let Message::Words { words } = m {
                let at = Instant::now();
                v.push(Got {
                    at,
                    words: words.into_iter().map(|w| (w.text, w.end_ms)).collect(),
                });
            }
        }
        v
    })
}

fn norm(text: &str) -> Vec<String> {
    text.split_whitespace()
        .map(|w| {
            w.chars()
                .filter(|c| c.is_alphanumeric() || *c == '\'')
                .flat_map(char::to_uppercase)
                .collect::<String>()
        })
        .filter(|w| !w.is_empty())
        .collect()
}

fn wer(reference: &[String], hyp: &[String]) -> f64 {
    let mut prev: Vec<usize> = (0..=hyp.len()).collect();
    for (i, r) in reference.iter().enumerate() {
        let mut cur = vec![i + 1; hyp.len() + 1];
        for (j, h) in hyp.iter().enumerate() {
            let sub = prev[j] + usize::from(r != h);
            cur[j + 1] = sub.min(prev[j + 1] + 1).min(cur[j] + 1);
        }
        prev = cur;
    }
    prev[hyp.len()] as f64 / reference.len().max(1) as f64
}

fn asr_full(spec: &WorkerSpec, audio: &[i16], reference: Option<&Path>) -> Result<()> {
    println!("\n== ASR, full file at 1x ==");
    let (sup, rx) = Supervisor::start(spec.clone(), Policy::default())?;
    let load = wait_ready(&sup, Duration::from_secs(120))?;
    println!("ready after {} ms", load.as_millis());
    let words = collect(rx);
    let t0 = stream(&sup, audio, |_| Ok(()))?;
    std::thread::sleep(Duration::from_secs(3));
    sup.shutdown();
    let got = words
        .join()
        .map_err(|_| anyhow::anyhow!("collector panicked"))?;
    let hyp: Vec<String> = got
        .iter()
        .flat_map(|g| g.words.iter().map(|w| w.0.clone()))
        .collect();
    let mut lag: Vec<f64> = got
        .iter()
        .flat_map(|g| {
            g.words.iter().map(move |w| {
                let spoken = t0 + Duration::from_millis(w.1);
                g.at.saturating_duration_since(spoken).as_secs_f64()
            })
        })
        .collect();
    let s = sup.status();
    println!("words: {} (messages: {})", hyp.len(), got.len());
    println!(
        "word lag after word end (s): P50 {:.2}  P95 {:.2}",
        pct(&mut lag, 0.5),
        pct(&mut lag, 0.95)
    );
    println!(
        "restarts {}  dropped_in {}  dropped_out {}",
        s.restarts, s.dropped_in, s.dropped_out
    );
    if let Some(r) = reference {
        let r = norm(&std::fs::read_to_string(r)?);
        let h = norm(&hyp.join(" "));
        println!(
            "WER vs reference ({} words, simple normaliser): {:.1}%",
            r.len(),
            100.0 * wer(&r, &h)
        );
    }
    Ok(())
}

fn asr_kill(spec: &WorkerSpec, audio: &[i16]) -> Result<()> {
    println!("\n== ASR, kill -9 at 20 s of a 60 s run ==");
    let (sup, rx) = Supervisor::start(spec.clone(), Policy::default())?;
    wait_ready(&sup, Duration::from_secs(120))?;
    let words = collect(rx);
    let mut killed = None;
    let mut ready_after = None;
    let part = &audio[..audio.len().min(60 * 16_000)];
    stream(&sup, part, |i| {
        if i == 200 {
            killed = Some(kill9(&sup)?);
        }
        if let Some(k) = killed
            && ready_after.is_none()
            && sup.status().restarts > 0
            && sup.status().state == State::Ready
        {
            ready_after = Some(k.elapsed());
        }
        Ok(())
    })?;
    std::thread::sleep(Duration::from_secs(3));
    let s = sup.status();
    sup.shutdown();
    let got = words
        .join()
        .map_err(|_| anyhow::anyhow!("collector panicked"))?;
    let k = killed.context("never killed")?;
    let first = got.iter().find(|g| g.at > k).map(|g| g.at - k);
    println!(
        "worker ready again after {} ms; first words after kill: {}",
        ready_after.map_or("n/a".into(), |d| d.as_millis().to_string()),
        first.map_or("none".into(), |d| format!("{} ms", d.as_millis()))
    );
    println!(
        "restarts {}  last_error {:?}  dropped_in {}",
        s.restarts, s.last_error, s.dropped_in
    );
    Ok(())
}

fn clauses(reference: Option<&Path>, n: usize) -> Result<Vec<String>> {
    let text = match reference {
        Some(p) => std::fs::read_to_string(p)?,
        None => "the quick brown fox jumps over the lazy dog ".repeat(40),
    };
    let words: Vec<String> = text.split_whitespace().map(str::to_lowercase).collect();
    let mut out = Vec::new();
    for (i, c) in words.chunks(10).take(n).enumerate() {
        let mut s = c.join(" ");
        if let Some(f) = s.get(0..1) {
            s = f.to_uppercase() + s.get(1..).unwrap_or_default();
        }
        if i % 2 == 1 {
            s.push('.');
        }
        out.push(s);
    }
    Ok(out)
}

/// Sends one clause and waits for all three answers. Returns per-language
/// latency (ms) and the number of errors.
fn translate_one(
    sup: &Supervisor,
    rx: &Receiver<Message>,
    id: u64,
    text: &str,
) -> (Vec<(String, f64, String)>, usize) {
    let langs = ["es", "fr", "de"];
    let t = Instant::now();
    sup.send_message(Message::Translate {
        clause: Clause {
            id,
            text: text.into(),
            start_ms: 0,
            end_ms: 0,
        },
        langs: langs.iter().map(|s| s.to_string()).collect(),
    });
    let (mut ok, mut errors) = (Vec::new(), 0);
    let until = t + Duration::from_secs(5);
    while ok.len() + errors < langs.len() {
        let left = until.saturating_duration_since(Instant::now());
        match rx.recv_timeout(left) {
            Ok(Message::Translated { translation }) if translation.clause_id == id => ok.push((
                translation.lang,
                t.elapsed().as_secs_f64() * 1000.0,
                translation.text,
            )),
            Ok(Message::Error { message }) if message.contains(&format!("clause {id} ")) => {
                errors += 1
            }
            Ok(_) => {}
            Err(_) => break,
        }
    }
    (ok, errors)
}

fn mt_latency(spec: &WorkerSpec, clauses: &[String]) -> Result<()> {
    println!(
        "\n== MT, {} clauses x es/fr/de, one at a time ==",
        clauses.len()
    );
    let (sup, rx) = Supervisor::start(spec.clone(), Policy::default())?;
    let load = wait_ready(&sup, Duration::from_secs(300))?;
    println!("ready after {} ms", load.as_millis());
    let (mut lat, mut errors) = (Vec::new(), 0);
    for (i, c) in clauses.iter().enumerate() {
        let (ok, e) = translate_one(&sup, &rx, i as u64, c);
        errors += e;
        if i < 2 {
            println!("  en: {c}");
            for (lang, _, text) in &ok {
                println!("  {lang}: {text}");
            }
        }
        lat.extend(ok.into_iter().map(|x| x.1));
        std::thread::sleep(Duration::from_millis(200));
    }
    println!(
        "results {}  errors {}  latency (ms, send to receipt): P50 {:.0}  P95 {:.0}  max {:.0}",
        lat.len(),
        errors,
        pct(&mut lat, 0.5),
        pct(&mut lat, 0.95),
        pct(&mut lat, 1.0)
    );
    sup.shutdown();
    Ok(())
}

fn mt_kill(spec: &WorkerSpec, clauses: &[String]) -> Result<()> {
    println!("\n== MT, kill -9 while sending a clause every 200 ms ==");
    let (sup, rx) = Supervisor::start(spec.clone(), Policy::default())?;
    wait_ready(&sup, Duration::from_secs(300))?;
    const KILL_ID: u64 = 1010;
    let (mut killed, mut first_ok) = (None, None);
    let t = Instant::now();
    let mut id = 1000u64;
    let mut next_send = Instant::now();
    while t.elapsed() < Duration::from_secs(60) && first_ok.is_none() {
        if Instant::now() >= next_send {
            if id == KILL_ID {
                killed = Some(kill9(&sup)?);
            }
            let text = &clauses[(id as usize) % clauses.len()];
            sup.send_message(Message::Translate {
                clause: Clause {
                    id,
                    text: text.clone(),
                    start_ms: 0,
                    end_ms: 0,
                },
                langs: vec!["es".into(), "fr".into(), "de".into()],
            });
            id += 1;
            next_send += Duration::from_millis(200);
        }
        if let Ok(Message::Translated { translation }) = rx.recv_timeout(Duration::from_millis(10))
            && translation.clause_id >= KILL_ID
            && let Some(k) = killed
        {
            first_ok = Some((k.elapsed(), translation.clause_id));
        }
    }
    let s = sup.status();
    println!(
        "first translation after kill: {}",
        first_ok.map_or("none within 60 s".into(), |(d, id)| format!(
            "{} ms (clause {id}; clauses sent from {KILL_ID})",
            d.as_millis()
        ))
    );
    println!("restarts {}  last_error {:?}", s.restarts, s.last_error);
    sup.shutdown();
    Ok(())
}
