//! `multi`: live multilingual captions for video streams.

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use multi::run::RunOptions;
use multi_core::Config;
use std::io::IsTerminal;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

#[derive(Parser)]
#[command(version, about = "Live multilingual captions for video streams")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Run the captioning pipeline.
    Run {
        /// Configuration file (TOML). Missing settings use defaults.
        #[arg(long, short)]
        config: Option<PathBuf>,
        /// ASR worker command line (`path [args]`), instead of `multi-asr`
        /// next to this executable.
        #[arg(long, value_name = "CMD")]
        asr_worker: Option<String>,
        /// Translation worker command line (`path [args]`), instead of `multi-mt`.
        #[arg(long, value_name = "CMD")]
        mt_worker: Option<String>,
        /// Model directory for the default workers ($MULTI_MODELS, else ~/.cache/multi-models).
        #[arg(long)]
        models_dir: Option<PathBuf>,
    },
    /// Work with configuration files.
    #[command(subcommand)]
    Config(ConfigCmd),
}

#[derive(Subcommand)]
enum ConfigCmd {
    /// Print a complete configuration with every default filled in.
    Default,
    /// Check a configuration file and report every problem.
    Check { path: PathBuf },
}

fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .with_writer(std::io::stderr)
        .with_ansi(std::io::stderr().is_terminal())
        .init();

    match run(Cli::parse()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e:#}");
            ExitCode::FAILURE
        }
    }
}

fn run(cli: Cli) -> Result<()> {
    match cli.cmd {
        Cmd::Config(ConfigCmd::Default) => {
            print!("{}", Config::default().to_toml()?);
            Ok(())
        }
        Cmd::Config(ConfigCmd::Check { path }) => {
            let config = Config::load(&path)?;
            check(&config)?;
            println!("{}: OK", path.display());
            Ok(())
        }
        Cmd::Run {
            config,
            asr_worker,
            mt_worker,
            models_dir,
        } => {
            let config = match config {
                Some(path) => Config::load(&path).with_context(|| "loading configuration")?,
                None => Config::default(),
            };
            check(&config)?;
            tracing::info!(
                input = %multi_media::url::redact(&config.input.url),
                outputs = config.outputs.len(),
                "configuration OK"
            );
            let opts = RunOptions::new(
                config,
                asr_worker.as_deref(),
                mt_worker.as_deref(),
                models_dir.as_deref(),
            )?;
            let stop = Arc::new(AtomicBool::new(false));
            let flag = stop.clone();
            ctrlc::set_handler(move || {
                tracing::info!("stop requested");
                flag.store(true, Ordering::Release);
            })
            .context("cannot install the Ctrl-C handler")?;
            multi::run::run(opts, &stop)
        }
    }
}

fn check(config: &Config) -> Result<()> {
    let issues = config.validate();
    if issues.is_empty() {
        return Ok(());
    }
    for issue in &issues {
        eprintln!("  {}: {}", issue.path, issue.message);
    }
    bail!("{} problem(s) in the configuration", issues.len())
}
