use std::path::PathBuf;
use std::time::Duration;

use broadcast_agents::claude;
use clap::{Parser, Subcommand};
use url::Url;

#[derive(Parser)]
#[command(name = "broadcast", version)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run an agent that works issues on the board.
    #[command(subcommand)]
    Runner(Runner),
}

#[derive(Subcommand)]
enum Runner {
    /// Work issues delegated to Claude Agent or mentioning @claude with `claude -p`.
    Claude {
        #[arg(long, env = "BROADCAST_URL", default_value = "http://127.0.0.1:47823")]
        url: Url,
        /// Working directory for `claude` (default: the current directory).
        #[arg(long)]
        cwd: Option<PathBuf>,
        #[arg(long, default_value = "acceptEdits")]
        permission_mode: String,
        /// Seconds between board polls.
        #[arg(long, default_value = "5", value_parser = seconds)]
        interval: Duration,
    },
}

fn seconds(value: &str) -> Result<Duration, String> {
    let seconds: f64 = value.parse().map_err(|_| format!("'{value}' is not a number"))?;
    Duration::try_from_secs_f64(seconds).map_err(|error| error.to_string())
}

fn main() -> anyhow::Result<()> {
    let Command::Runner(Runner::Claude { url, cwd, permission_mode, interval }) = Cli::parse().command;
    let cwd = match cwd {
        Some(cwd) => cwd,
        None => std::env::current_dir()?,
    };
    let options = claude::Options { url, cwd, permission_mode, interval };
    // The runner future (and with it a running `claude`) is dropped once the select ends.
    tokio::runtime::Runtime::new()?.block_on(async {
        tokio::select! {
            result = claude::run(options) => result,
            () = shutdown_signal() => Ok(()),
        }
    })?;
    std::process::exit(130)
}

#[cfg(unix)]
async fn shutdown_signal() {
    use tokio::signal::unix::{SignalKind, signal};
    match signal(SignalKind::terminate()) {
        Ok(mut terminate) => tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = terminate.recv() => {}
        },
        Err(_) => {
            let _ = tokio::signal::ctrl_c().await;
        }
    }
}

#[cfg(not(unix))]
async fn shutdown_signal() {
    let _ = tokio::signal::ctrl_c().await;
}
