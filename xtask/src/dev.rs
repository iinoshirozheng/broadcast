//! `cargo xtask dev`: the Rust server and the Vite dev server side by side. When either exits
//! the other is stopped; Ctrl-C reaches both through the terminal's process group.

use std::process::{Child, Command};
use std::thread::sleep;
use std::time::Duration;

use anyhow::Result;

use crate::root;

pub fn run() -> Result<()> {
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
    let mut server = Command::new(cargo);
    server
        .current_dir(root())
        .args(["run", "-p", "broadcast-server"]);
    if std::env::var_os("BROADCAST_HOST").is_none() {
        server.env("BROADCAST_HOST", "127.0.0.1");
    }
    let pnpm = if cfg!(windows) { "pnpm.cmd" } else { "pnpm" };
    let mut children = [
        server.spawn()?,
        Command::new(pnpm)
            .current_dir(root())
            .arg("dev:web")
            .spawn()?,
    ];

    let code = loop {
        if let Some(code) = exited(&mut children)? {
            break code;
        }
        sleep(Duration::from_millis(200));
    };
    for child in &mut children {
        let _ = child.kill();
        let _ = child.wait();
    }
    std::process::exit(code);
}

fn exited(children: &mut [Child]) -> Result<Option<i32>> {
    for child in children {
        if let Some(status) = child.try_wait()? {
            return Ok(Some(status.code().unwrap_or(1)));
        }
    }
    Ok(None)
}
