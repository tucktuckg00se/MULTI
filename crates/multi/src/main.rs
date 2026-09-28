//! `multi`: live multilingual captions for video streams.

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use multi::models::{self, Check};
use multi::service::{Service, Workers};
use multi_core::Config;
use multi_core::models::Registry;
use std::io::IsTerminal;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

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
        /// Models directory ($MULTI_MODELS, else $XDG_DATA_HOME/multi/models,
        /// else ~/.local/share/multi/models).
        #[arg(long)]
        models_dir: Option<PathBuf>,
    },
    /// Run the web GUI (settings, status, live captions) and the pipeline.
    Serve {
        /// Configuration file (TOML); created with defaults if missing.
        #[arg(long, short)]
        config: PathBuf,
        /// ASR worker command line (`path [args]`), instead of `multi-asr`.
        #[arg(long, value_name = "CMD")]
        asr_worker: Option<String>,
        /// Translation worker command line (`path [args]`), instead of `multi-mt`.
        #[arg(long, value_name = "CMD")]
        mt_worker: Option<String>,
        /// Models directory ($MULTI_MODELS, else $XDG_DATA_HOME/multi/models,
        /// else ~/.local/share/multi/models).
        #[arg(long)]
        models_dir: Option<PathBuf>,
    },
    /// Set the web GUI sign-in password (stored as an argon2id hash).
    Passwd {
        /// Configuration file (TOML); created with defaults if missing.
        #[arg(long, short)]
        config: PathBuf,
        /// Also change the user name (default `admin`).
        #[arg(long)]
        username: Option<String>,
        /// Read the password from the first line of standard input instead of
        /// asking twice on the terminal.
        #[arg(long)]
        password_stdin: bool,
    },
    /// Work with configuration files.
    #[command(subcommand)]
    Config(ConfigCmd),
    /// Download, check and remove models.
    Models(ModelsArgs),
}

#[derive(clap::Args)]
struct ModelsArgs {
    /// Models directory ($MULTI_MODELS, else $XDG_DATA_HOME/multi/models,
    /// else ~/.local/share/multi/models).
    #[arg(long, global = true)]
    models_dir: Option<PathBuf>,
    #[command(subcommand)]
    cmd: ModelsCmd,
}

#[derive(Subcommand)]
enum ModelsCmd {
    /// List the registry with installed/missing status and sizes.
    List,
    /// Download (or convert) models; the default set when no ids are given.
    Pull { ids: Vec<String> },
    /// Check installed models against the registry (SHA-256) or their manifest.
    Verify { ids: Vec<String> },
    /// Delete an installed model.
    Remove { id: String },
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
            let service = Service::new(
                config.clone(),
                Workers {
                    asr: asr_worker,
                    mt: mt_worker,
                    models_dir,
                },
            );
            let stop = Arc::new(AtomicBool::new(false));
            let flag = stop.clone();
            ctrlc::set_handler(move || {
                tracing::info!("stop requested");
                flag.store(true, Ordering::Release);
            })
            .context("cannot install the Ctrl-C handler")?;
            service.start(config)?;
            while !stop.load(Ordering::Acquire) && service.is_active() {
                std::thread::sleep(Duration::from_millis(50));
            }
            service.stop()
        }
        Cmd::Models(args) => models_cmd(args),
        Cmd::Passwd {
            config,
            username,
            password_stdin,
        } => passwd(&config, username.as_deref(), password_stdin),
        Cmd::Serve {
            config,
            asr_worker,
            mt_worker,
            models_dir,
        } => multi::web::serve(
            &config,
            Workers {
                asr: asr_worker,
                mt: mt_worker,
                models_dir,
            },
        ),
    }
}

fn models_cmd(args: ModelsArgs) -> Result<()> {
    let reg = Registry::builtin().map_err(anyhow::Error::msg)?;
    let root = models::resolve_dir(args.models_dir.as_deref());
    match args.cmd {
        ModelsCmd::List => {
            models::list(&reg, &root);
            Ok(())
        }
        ModelsCmd::Pull { ids } => models::pull(&reg, &ids, &root),
        ModelsCmd::Remove { id } => models::remove(&reg, &id, &root),
        ModelsCmd::Verify { ids } => {
            println!("models directory: {}", root.display());
            let results = models::verify(&reg, &ids, &root)?;
            let mut failed = 0;
            for (id, check) in &results {
                match check {
                    Check::Ok => println!("{id}: OK"),
                    Check::Missing => {
                        failed += 1;
                        println!("{id}: not installed");
                    }
                    Check::Unverified(why) => println!("{id}: present, unverified: {why}"),
                    Check::Failed(problems) => {
                        failed += 1;
                        println!("{id}: FAILED");
                        for p in problems {
                            println!("  {p}");
                        }
                    }
                }
            }
            if results.is_empty() {
                println!("no models installed");
            }
            if failed > 0 {
                bail!("{failed} model(s) failed verification");
            }
            Ok(())
        }
    }
}

fn passwd(path: &std::path::Path, username: Option<&str>, stdin: bool) -> Result<()> {
    let password = if stdin {
        let mut line = String::new();
        std::io::stdin()
            .read_line(&mut line)
            .context("cannot read the password from standard input")?;
        line.trim_end_matches(['\r', '\n']).to_string()
    } else {
        let first =
            rpassword::prompt_password("New password: ").context("cannot read the password")?;
        multi::auth::check_new_password(&first)?;
        let again =
            rpassword::prompt_password("Repeat it: ").context("cannot read the password")?;
        if first != again {
            bail!("the passwords do not match");
        }
        first
    };
    multi::auth::set_password(path, username, &password)?;
    println!(
        "Password set in {}. Restart `multi serve` if it is running.",
        path.display()
    );
    Ok(())
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
