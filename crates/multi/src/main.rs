//! `multi`: live multilingual captions for video streams.

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use multi_core::Config;
use std::path::PathBuf;
use std::process::ExitCode;

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
        Cmd::Run { config } => {
            let config = match config {
                Some(path) => Config::load(&path).with_context(|| "loading configuration")?,
                None => Config::default(),
            };
            check(&config)?;
            tracing::info!(input = %config.input.url, outputs = config.outputs.len(), "configuration OK");
            bail!("the media pipeline is not built yet (M1 work package 2)")
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
