//! `latency`: measures video pass-through delay and caption lag for M0 spikes.
//! See docs/m0/findings/H-latency-tool.md for method and accuracy.

mod captions;
mod clock;
mod delay;
mod report;
mod sock;
mod stats;
mod tap;
mod ts;

use clap::{Parser, Subcommand};
use std::net::SocketAddr;
use std::path::PathBuf;

#[derive(Parser)]
#[command(name = "latency", about = "Video pass-through delay and caption lag measurement")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// UDP MPEG-TS pass-through probe: forwards datagrams unchanged and logs
    /// `wallclock_ns,pts_90k,frame_seq,tail_hash` per video PES (CLOCK_MONOTONIC).
    Tap {
        #[arg(long)]
        listen: SocketAddr,
        /// Where to forward datagrams (omit to only listen, e.g. as a sink).
        #[arg(long)]
        forward: Option<SocketAddr>,
        #[arg(long)]
        out: PathBuf,
        /// Stop after this many seconds (otherwise run until killed; the CSV is flushed per frame).
        #[arg(long)]
        duration: Option<f64>,
        /// Program number to follow (0 = first program in the PAT).
        #[arg(long, default_value_t = 0)]
        program: u16,
        /// No periodic stats on stderr.
        #[arg(long)]
        quiet: bool,
    },
    /// Match frames between two tap CSVs (before/after the system under test) and summarise delay.
    Report {
        input: PathBuf,
        output: PathBuf,
        #[arg(long = "match", value_enum, default_value_t = report::MatchMode::Auto)]
        mode: report::MatchMode,
        /// Force a PTS offset (90 kHz ticks, out - in) instead of auto-detecting it.
        #[arg(long, allow_hyphen_values = true)]
        pts_offset: Option<i64>,
        /// Ignore frames that entered in the first N seconds (warm-up).
        #[arg(long, default_value_t = 0.0)]
        skip: f64,
        /// Write per-frame pairs (in_seq,out_seq,in_pts,out_pts,in_wall_ns,delay_ms).
        #[arg(long)]
        pairs: Option<PathBuf>,
        #[arg(long)]
        json: bool,
    },
    /// Caption lag: when do each utterance's final words first appear in the captions?
    Captions {
        #[arg(long)]
        srt: PathBuf,
        #[arg(long)]
        segments: PathBuf,
        /// Word-level reference (`word<TAB>start_s<TAB>end_s`, audio time): lag is measured
        /// from the end of the utterance's last word instead of the segment end.
        #[arg(long)]
        words: Option<PathBuf>,
        /// Audio loop length in seconds (default: from --wav, else last segment end).
        #[arg(long)]
        loop_seconds: Option<f64>,
        /// Read the loop length from this WAV's header.
        #[arg(long)]
        wav: Option<PathBuf>,
        /// Stream-time origin in seconds, subtracted from SRT times. 0 if the SRT is already in
        /// stream time; 1.421333 for absolute PTS from source.sh (see doc).
        #[arg(long, default_value_t = 0.0, allow_hyphen_values = true)]
        pts_origin: f64,
        /// Number of final content words to look for.
        #[arg(long, default_value_t = 3)]
        n_words: usize,
        /// Search window after the utterance end, seconds.
        #[arg(long, default_value_t = 8.0)]
        max_lag: f64,
        /// Search window before the utterance end, seconds.
        #[arg(long, default_value_t = 1.0)]
        pre: f64,
        /// Per-utterance CSV output.
        #[arg(long)]
        csv: Option<PathBuf>,
        #[arg(long)]
        json: bool,
    },
    /// Calibration delay line: holds every UDP datagram for a fixed time, then forwards it.
    Delay {
        #[arg(long)]
        listen: SocketAddr,
        #[arg(long)]
        forward: SocketAddr,
        #[arg(long)]
        ms: f64,
        #[arg(long)]
        duration: Option<f64>,
    },
}

fn main() -> anyhow::Result<()> {
    match Cli::parse().cmd {
        Cmd::Tap {
            listen,
            forward,
            out,
            duration,
            program,
            quiet,
        } => tap::run(tap::TapArgs {
            listen,
            forward,
            out,
            duration,
            program,
            quiet,
        }),
        Cmd::Report {
            input,
            output,
            mode,
            pts_offset,
            skip,
            pairs,
            json,
        } => report::run(report::ReportArgs {
            input: &input,
            output: &output,
            mode,
            pts_offset,
            json,
            pairs: pairs.as_deref(),
            skip_seconds: skip,
        }),
        Cmd::Captions {
            srt,
            segments,
            words,
            loop_seconds,
            wav,
            pts_origin,
            n_words,
            max_lag,
            pre,
            csv,
            json,
        } => captions::run(captions::CaptionArgs {
            srt,
            segments,
            words,
            loop_seconds,
            wav,
            pts_origin,
            n_words: n_words.max(1),
            max_lag,
            pre,
            csv,
            json,
        }),
        Cmd::Delay {
            listen,
            forward,
            ms,
            duration,
        } => delay::run(listen, forward, ms, duration),
    }
}
