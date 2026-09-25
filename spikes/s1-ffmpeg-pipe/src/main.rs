//! S1 spike: FFmpeg-library pass-through with caption SEI insertion.
//!
//! `run`: SRT/UDP MPEG-TS in -> demux -> copy packets (no decode) -> splice
//! CEA-608 SEI into each video access unit -> remux MPEG-TS -> N outputs.
//! `relay`/`measure`/`stats`: black-box latency helpers.

mod nal;
mod output;
mod pipe;
mod probe;

use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use clap::{Args, Parser, Subcommand};

#[derive(Parser)]
#[command(about = "S1: FFmpeg pass-through with caption SEI insertion")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Run the pipeline.
    Run(RunArgs),
    /// Receive UDP datagrams, optionally forward them, and log video PES arrival (wall_ns,pts).
    Relay {
        #[arg(long)]
        listen: String,
        #[arg(long)]
        forward: Option<String>,
        #[arg(long)]
        log: PathBuf,
        #[arg(long, default_value_t = 60)]
        duration: u64,
    },
    /// Read a URL through libavformat and log video packet arrival (wall_ns,pts).
    Measure {
        url: String,
        #[arg(long)]
        log: PathBuf,
        #[arg(long, default_value_t = 60)]
        duration: u64,
    },
    /// Summarise a pipe CSV, or join two (wall_ns,pts) logs on PTS.
    Stats {
        a: PathBuf,
        b: Option<PathBuf>,
        /// Skip this many rows/frames at the start (warm-up).
        #[arg(long, default_value_t = 0)]
        skip: usize,
    },
}

#[derive(Args)]
pub struct RunArgs {
    /// Input URL, e.g. 'srt://127.0.0.1:9110?mode=caller' or 'udp://127.0.0.1:9102'.
    #[arg(long)]
    pub input: String,
    /// Output URL (repeatable), e.g. 'udp://127.0.0.1:9103?pkt_size=1316'.
    #[arg(long, required = true)]
    pub output: Vec<String>,
    /// Input option key=value (repeatable), passed to avformat_open_input. Replaces the defaults.
    #[arg(long = "in-opt", default_values_t = default_in_opts())]
    pub in_opt: Vec<String>,
    /// Output protocol option key=value (repeatable), passed to avio_open2.
    #[arg(long = "out-opt")]
    pub out_opt: Vec<String>,
    /// Directory for per-output CSVs (out0.csv, ...).
    #[arg(long)]
    pub csv_dir: Option<PathBuf>,
    /// Stop after this many seconds.
    #[arg(long)]
    pub duration: Option<u64>,
    /// Pass through without inserting captions (baseline).
    #[arg(long)]
    pub no_captions: bool,
    /// Frames of video held to assign captions in PTS order (default: the
    /// stream's reorder depth from FFmpeg, 0 without B-frames).
    #[arg(long)]
    pub reorder: Option<usize>,
    /// Packets queued per output before dropping to the next keyframe.
    #[arg(long, default_value_t = 300)]
    pub queue: usize,
}

fn default_in_opts() -> Vec<String> {
    vec![
        "rw_timeout=2000000".into(), // 2 s without data = input lost
        "probesize=500000".into(),
        "analyzeduration=1500000".into(), // must cover one 1 s GOP to find SPS/PPS
        // No AVParser: saves one frame of delay (the H.264/HEVC parser waits for
        // the next AU start). Keyframes are then flagged from NAL types instead.
        "fflags=+noparse".into(),
    ]
}

/// Set once at shutdown; checked by FFmpeg interrupt callbacks and threads.
pub static SHUTDOWN: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

pub fn now_ns() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_nanos() as u64)
}

/// Resident set size in KiB from /proc/self/statm (0 if unavailable).
pub fn rss_kib() -> u64 {
    std::fs::read_to_string("/proc/self/statm")
        .ok()
        .and_then(|s| s.split_whitespace().nth(1).and_then(|p| p.parse::<u64>().ok()))
        .map_or(0, |pages| pages * 4)
}

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_target(false)
        .with_ansi(false)
        .init();
    match Cli::parse().cmd {
        Cmd::Run(a) => pipe::run(a),
        Cmd::Relay { listen, forward, log, duration } => probe::relay(&listen, forward.as_deref(), &log, duration),
        Cmd::Measure { url, log, duration } => probe::measure(&url, &log, duration),
        Cmd::Stats { a, b: None, skip } => probe::stats_pipe(&a, skip),
        Cmd::Stats { a, b: Some(b), skip } => probe::stats_join(&a, &b, skip),
    }
}
