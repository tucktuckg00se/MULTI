//! Resource sampler: RSS, CPU time and this process's VRAM (via nvidia-smi),
//! sampled once a second on a background thread.

use std::io::Write;
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

#[derive(Default, Clone, Debug, serde::Serialize)]
pub struct ResSummary {
    pub vram_peak_mb: u64,
    pub rss_peak_mb: f64,
    pub rss_start_mb: f64,
    pub rss_end_mb: f64,
    /// Mean CPU use while sampling, in % of one core.
    pub cpu_pct: f64,
    pub samples: usize,
}

pub fn rss_mb() -> f64 {
    let s = std::fs::read_to_string("/proc/self/statm").unwrap_or_default();
    let pages: f64 = s.split_whitespace().nth(1).and_then(|v| v.parse().ok()).unwrap_or(0.0);
    pages * 4096.0 / 1e6
}

/// utime + stime of this process in seconds.
pub fn cpu_s() -> f64 {
    let s = std::fs::read_to_string("/proc/self/stat").unwrap_or_default();
    // Fields after the ")" of comm: state is field 3; utime 14, stime 15.
    let rest = s.rsplit_once(')').map(|(_, r)| r).unwrap_or("");
    let f: Vec<&str> = rest.split_whitespace().collect();
    let tick = 100.0;
    let u: f64 = f.get(11).and_then(|v| v.parse().ok()).unwrap_or(0.0);
    let st: f64 = f.get(12).and_then(|v| v.parse().ok()).unwrap_or(0.0);
    (u + st) / tick
}

pub fn vram_mb() -> u64 {
    let pid = std::process::id().to_string();
    let Ok(o) = Command::new("nvidia-smi")
        .args(["--query-compute-apps=pid,used_memory", "--format=csv,noheader,nounits"])
        .output()
    else {
        return 0;
    };
    String::from_utf8_lossy(&o.stdout)
        .lines()
        .filter_map(|l| {
            let mut it = l.split(',').map(str::trim);
            let p = it.next()?;
            let m: u64 = it.next()?.parse().ok()?;
            (p == pid).then_some(m)
        })
        .sum()
}

pub struct Sampler {
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<ResSummary>>,
}

impl Sampler {
    /// `csv`: optional path for a per-second trace (t_s,rss_mb,vram_mb,cpu_pct).
    pub fn start(csv: Option<std::path::PathBuf>) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let s2 = stop.clone();
        let handle = std::thread::spawn(move || {
            let mut f = csv.and_then(|p| std::fs::File::create(p).ok());
            if let Some(f) = f.as_mut() {
                let _ = writeln!(f, "t_s,rss_mb,vram_mb,cpu_pct");
            }
            let t0 = Instant::now();
            let c0 = cpu_s();
            let mut sum = ResSummary { rss_start_mb: rss_mb(), ..Default::default() };
            let (mut last_t, mut last_c) = (0.0, c0);
            while !s2.load(Ordering::Relaxed) {
                let v = vram_mb();
                let r = rss_mb();
                let t = t0.elapsed().as_secs_f64();
                let c = cpu_s();
                let pct = if t > last_t { (c - last_c) / (t - last_t) * 100.0 } else { 0.0 };
                (last_t, last_c) = (t, c);
                sum.vram_peak_mb = sum.vram_peak_mb.max(v);
                sum.rss_peak_mb = sum.rss_peak_mb.max(r);
                sum.rss_end_mb = r;
                sum.samples += 1;
                if let Some(f) = f.as_mut() {
                    let _ = writeln!(f, "{t:.1},{r:.1},{v},{pct:.0}");
                }
                for _ in 0..10 {
                    if s2.load(Ordering::Relaxed) {
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(100));
                }
            }
            let t = t0.elapsed().as_secs_f64();
            sum.cpu_pct = if t > 0.0 { (cpu_s() - c0) / t * 100.0 } else { 0.0 };
            sum
        });
        Self { stop, handle: Some(handle) }
    }

    pub fn finish(mut self) -> ResSummary {
        self.stop.store(true, Ordering::Relaxed);
        self.handle.take().and_then(|h| h.join().ok()).unwrap_or_default()
    }
}
