//! Shared pieces: input clauses, per-translation output records, the run loop
//! (single pass or timed soak), and process stats (RSS, VRAM).

use std::collections::HashMap;
use std::fs::File;
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde::Serialize;

#[derive(clap::Args, Debug, Clone)]
pub struct CommonArgs {
    /// TSV with header `id doc kind text` (see data/clauses.tsv).
    #[arg(long)]
    pub input: PathBuf,
    /// JSONL output, one line per (clause, language).
    #[arg(long)]
    pub out: PathBuf,
    /// Target languages, ISO 639-1, comma separated.
    #[arg(long, default_value = "es,fr,de,pt", value_delimiter = ',')]
    pub langs: Vec<String>,
    /// Give the previous N clauses of the same document as context.
    #[arg(long, default_value_t = 0)]
    pub context: usize,
    /// Only the first N clauses.
    #[arg(long)]
    pub limit: Option<usize>,
    /// Soak: loop over the input for this many minutes (0 = one pass).
    #[arg(long, default_value_t = 0.0)]
    pub minutes: f64,
    /// Soak: print a stats line this often.
    #[arg(long, default_value_t = 60)]
    pub stats_every_s: u64,
    /// Label written into every record (model/quant/mode).
    #[arg(long, default_value = "")]
    pub label: String,
    /// Clauses translated before timing starts (not recorded).
    #[arg(long, default_value_t = 3)]
    pub warmup: usize,
    /// Hard wall-clock cap per request; generation stops and the output is flagged.
    #[arg(long, default_value_t = 2000)]
    pub max_ms: u64,
}

#[derive(Debug, Clone)]
pub struct Clause {
    pub id: String,
    pub doc: String,
    pub kind: String,
    pub text: String,
}

pub fn read_clauses(path: &PathBuf) -> Result<Vec<Clause>> {
    let f = File::open(path).with_context(|| format!("open {}", path.display()))?;
    let mut out = Vec::new();
    for (i, line) in BufReader::new(f).lines().enumerate() {
        let line = line?;
        if i == 0 || line.trim().is_empty() {
            continue;
        }
        let cols: Vec<&str> = line.splitn(4, '\t').collect();
        if cols.len() != 4 {
            bail!("{}:{}: expected 4 tab-separated columns", path.display(), i + 1);
        }
        out.push(Clause {
            id: cols[0].into(),
            doc: cols[1].into(),
            kind: cols[2].into(),
            text: cols[3].into(),
        });
    }
    Ok(out)
}

/// One translation request: a clause plus optional preceding clauses.
pub struct Job<'a> {
    pub text: &'a str,
    pub prev: Vec<&'a str>,
    pub langs: &'a [String],
    pub max_ms: u64,
}

/// One language's result for a job, as the engine reports it.
#[derive(Debug, Clone, Default)]
pub struct Out {
    pub lang: String,
    pub text: String,
    /// Time from request start until this language's text was complete.
    pub ms: f64,
    /// Time from request start until the first output token (None if unknown).
    pub ttft_ms: Option<f64>,
    pub n_out: usize,
    /// Why generation stopped early: "max_tokens", "deadline", or empty.
    pub cap: String,
}

#[derive(Serialize)]
struct Record<'a> {
    label: &'a str,
    pass: usize,
    id: &'a str,
    kind: &'a str,
    lang: &'a str,
    ctx: usize,
    src: &'a str,
    out: &'a str,
    ms: f64,
    /// Whole request (all languages) wall time.
    req_ms: f64,
    ttft_ms: Option<f64>,
    n_out: usize,
    cap: &'a str,
}

#[derive(Serialize)]
pub struct Summary {
    pub label: String,
    pub load_ms: f64,
    pub first_req_ms: f64,
    pub n_requests: usize,
    pub vram_mib: Option<u64>,
    pub rss_mib: f64,
    pub wall_s: f64,
    pub out_tokens: usize,
    pub caps: usize,
    pub per_lang: HashMap<String, [f64; 3]>,
    pub req: [f64; 3],
}

pub fn pct(v: &mut [f64], p: f64) -> f64 {
    if v.is_empty() {
        return f64::NAN;
    }
    v.sort_by(|a, b| a.total_cmp(b));
    let idx = ((p / 100.0) * (v.len() - 1) as f64).round() as usize;
    v[idx.min(v.len() - 1)]
}

fn p50_95_max(v: &mut [f64]) -> [f64; 3] {
    [pct(v, 50.0), pct(v, 95.0), pct(v, 100.0)]
}

pub fn rss_mib() -> f64 {
    std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|s| {
            s.lines()
                .find(|l| l.starts_with("VmRSS:"))
                .and_then(|l| l.split_whitespace().nth(1))
                .and_then(|kb| kb.parse::<f64>().ok())
        })
        .map_or(f64::NAN, |kb| kb / 1024.0)
}

/// VRAM used by this process, per nvidia-smi.
pub fn vram_mib() -> Option<u64> {
    let out = std::process::Command::new("nvidia-smi")
        .args([
            "--query-compute-apps=pid,used_memory",
            "--format=csv,noheader,nounits",
        ])
        .output()
        .ok()?;
    let me = std::process::id().to_string();
    String::from_utf8_lossy(&out.stdout).lines().find_map(|l| {
        let mut it = l.split(',').map(str::trim);
        (it.next()? == me).then(|| it.next()?.parse().ok())?
    })
}

/// Drives an engine over the input: warmup, one timed pass (or a soak loop),
/// writes JSONL records and returns a summary.
pub fn run<F>(args: &CommonArgs, load_ms: f64, mut translate: F) -> Result<Summary>
where
    F: FnMut(&Job) -> Result<Vec<Out>>,
{
    let mut clauses = read_clauses(&args.input)?;
    if let Some(n) = args.limit {
        clauses.truncate(n);
    }
    if clauses.is_empty() {
        bail!("no clauses");
    }
    let mut w = BufWriter::new(File::create(&args.out)?);
    let job_for = |i: usize| -> Job<'_> {
        let c = &clauses[i];
        let mut prev = Vec::new();
        let mut j = i;
        while prev.len() < args.context && j > 0 {
            j -= 1;
            if clauses[j].doc != c.doc {
                break;
            }
            prev.insert(0, clauses[j].text.as_str());
        }
        Job {
            text: &c.text,
            prev,
            langs: &args.langs,
            max_ms: args.max_ms,
        }
    };

    // Cold first request, then warmup.
    let t = Instant::now();
    translate(&job_for(0))?;
    let first_req_ms = t.elapsed().as_secs_f64() * 1e3;
    for i in 0..args.warmup.min(clauses.len()) {
        translate(&job_for(i))?;
    }

    let mut per_lang: HashMap<String, Vec<f64>> = HashMap::new();
    let mut req = Vec::new();
    let mut out_tokens = 0;
    let mut caps = 0;
    let mut n_requests = 0;
    let start = Instant::now();
    let soak = args.minutes > 0.0;
    let until = Duration::from_secs_f64(args.minutes * 60.0);
    let mut next_stats = Duration::from_secs(args.stats_every_s);
    let mut window: Vec<f64> = Vec::new();
    let mut window_caps = 0;
    let mut pass = 0;
    'outer: loop {
        for i in 0..clauses.len() {
            let job = job_for(i);
            let t = Instant::now();
            let outs = translate(&job)?;
            let req_ms = t.elapsed().as_secs_f64() * 1e3;
            n_requests += 1;
            req.push(req_ms);
            for o in &outs {
                out_tokens += o.n_out;
                if !o.cap.is_empty() {
                    caps += 1;
                    window_caps += 1;
                }
                window.push(o.ms);
                per_lang.entry(o.lang.clone()).or_default().push(o.ms);
                let c = &clauses[i];
                serde_json::to_writer(
                    &mut w,
                    &Record {
                        label: &args.label,
                        pass,
                        id: &c.id,
                        kind: &c.kind,
                        lang: &o.lang,
                        ctx: job.prev.len(),
                        src: &c.text,
                        out: &o.text,
                        ms: o.ms,
                        req_ms,
                        ttft_ms: o.ttft_ms,
                        n_out: o.n_out,
                        cap: &o.cap,
                    },
                )?;
                w.write_all(b"\n")?;
            }
            if soak {
                let el = start.elapsed();
                if el >= next_stats {
                    let [p50, p95, max] = p50_95_max(&mut window);
                    eprintln!(
                        "soak t={:.0}s reqs={} p50={:.1} p95={:.1} max={:.1} caps={} rss_mib={:.1} vram_mib={}",
                        el.as_secs_f64(),
                        n_requests,
                        p50,
                        p95,
                        max,
                        window_caps,
                        rss_mib(),
                        vram_mib().map_or("?".into(), |v| v.to_string())
                    );
                    window.clear();
                    window_caps = 0;
                    next_stats += Duration::from_secs(args.stats_every_s);
                    w.flush()?;
                }
                if el >= until {
                    break 'outer;
                }
            }
        }
        pass += 1;
        if !soak {
            break;
        }
    }
    w.flush()?;
    let wall_s = start.elapsed().as_secs_f64();
    let summary = Summary {
        label: args.label.clone(),
        load_ms,
        first_req_ms,
        n_requests,
        vram_mib: vram_mib(),
        rss_mib: rss_mib(),
        wall_s,
        out_tokens,
        caps,
        per_lang: per_lang
            .into_iter()
            .map(|(k, mut v)| (k, p50_95_max(&mut v)))
            .collect(),
        req: p50_95_max(&mut req),
    };
    Ok(summary)
}

pub fn lang_name(code: &str) -> &'static str {
    match code {
        "es" => "Spanish",
        "fr" => "French",
        "de" => "German",
        "pt" => "Portuguese",
        "zh" => "Chinese",
        "ar" => "Arabic",
        "it" => "Italian",
        "ja" => "Japanese",
        _ => "English",
    }
}
