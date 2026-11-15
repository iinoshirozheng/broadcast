//! Runs the server with the Codex launcher attached, the way the desktop embeds them.
//!
//! `BROADCAST_INSTANCE_TOKEN`/`BROADCAST_INSTANCE_SECRET` put the server in launcher mode;
//! `BROADCAST_CODEX_APP` names the Codex bundle, `BROADCAST_CODEX_PROFILE` its user-data
//! directory and `BROADCAST_CODEX_CDP` either `pipe` or a DevTools port. `BROADCAST_CODEX_OPEN=0`
//! injects without opening the board.

use std::path::PathBuf;

use anyhow::Context;
use broadcast_codex::{Cdp, Launcher, Options};
use broadcast_server::{Config, Server};
use tokio::net::TcpListener;
use tokio::sync::mpsc;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let config = Config::from_env()?;
    let launcher = config.launcher.clone().context("BROADCAST_INSTANCE_TOKEN is required")?;
    let variable = |name: &str| std::env::var(name).with_context(|| format!("{name} is required"));
    let cdp = match variable("BROADCAST_CODEX_CDP")?.as_str() {
        "pipe" => Cdp::Pipe,
        port => Cdp::Port(port.parse().context("BROADCAST_CODEX_CDP must be pipe or a port")?),
    };
    let listener = TcpListener::bind((config.host, config.port)).await?;
    let port = listener.local_addr()?.port();
    let data_dir = config.data_dir.clone();
    let mut server = Server::open(config)?;
    let bridge = server.host_bridge().context("host bridge already taken")?;
    let (events, mut received) = mpsc::unbounded_channel();
    let options = Options {
        app_path: PathBuf::from(variable("BROADCAST_CODEX_APP")?),
        cdp,
        profile: PathBuf::from(variable("BROADCAST_CODEX_PROFILE")?),
        source_profile: None,
        data_dir,
        port,
        token: launcher.token,
        secret: launcher.secret,
        open: std::env::var("BROADCAST_CODEX_OPEN").map_or(true, |value| value != "0"),
        cli: std::env::current_exe()?.parent().and_then(std::path::Path::parent).map(|debug| debug.join("broadcast")).ok_or_else(|| anyhow::anyhow!("no target directory"))?,
        runtime_file: std::env::var_os("BROADCAST_RUNTIME_FILE").map(PathBuf::from),
    };
    let (launcher, task) = Launcher::start(options, bridge.requests, bridge.notifications, events);
    tokio::spawn(async move {
        while let Some(event) = received.recv().await {
            println!("launcher event: {event:?}");
        }
    });
    println!("Broadcast listening on http://127.0.0.1:{port}");
    let stop = launcher.clone();
    server
        .serve(listener, async move {
            let _ = tokio::signal::ctrl_c().await;
            stop.stop();
        })
        .await?;
    launcher.stop();
    task.await?
}
