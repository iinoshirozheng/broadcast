//! `broadcast runner claude`: work board issues with `claude -p` until interrupted.

use std::time::Duration;

use broadcast_agents::claude;
use url::Url;

use crate::args::Parsed;
use crate::error::{CliError, Result, usage};
use crate::target::DEFAULT_API_URL;

/// Returns once a shutdown signal stops the runner.
pub async fn claude(parsed: &Parsed) -> Result<()> {
    parsed.expect_operands(0)?;
    let url = match parsed.option("url") {
        Some(url) => url.to_owned(),
        None => std::env::var("BROADCAST_URL").unwrap_or_else(|_| DEFAULT_API_URL.to_owned()),
    };
    let url = Url::parse(&url).map_err(|_| usage("Runner URL must be a valid URL"))?;
    let cwd = match parsed.option("cwd") {
        Some(cwd) => cwd.into(),
        None => std::env::current_dir().map_err(|error| CliError::new(error.to_string(), "INTERNAL_ERROR", 1))?,
    };
    let permission_mode = parsed.option("permission-mode").unwrap_or("acceptEdits").to_owned();
    let interval = match parsed.option("interval") {
        Some(raw) => raw.parse().ok().and_then(|seconds| Duration::try_from_secs_f64(seconds).ok()),
        None => Some(Duration::from_secs(5)),
    };
    let interval = interval.ok_or_else(|| usage("--interval must be a non-negative number of seconds"))?;
    let options = claude::Options { url, cwd, permission_mode, interval };
    // The runner future (and with it a running `claude`) is dropped once the select ends.
    tokio::select! {
        result = claude::run(options) => result.map_err(|error| CliError::new(format!("{error:#}"), "INTERNAL_ERROR", 1)),
        () = shutdown_signal() => Ok(()),
    }
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
