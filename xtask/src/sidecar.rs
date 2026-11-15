//! Builds the `broadcast-cli` sidecar (`bundle.externalBin`). Tauri runs this as its
//! `beforeBuildCommand`/`beforeDevCommand` with `TAURI_ENV_TARGET_TRIPLE` set to the `--target`
//! it builds, or to the host triple.

use std::fs;
use std::path::PathBuf;
use std::process::Command;

use anyhow::{Context, Result};

use crate::{root, status};

const UNIVERSAL: &str = "universal-apple-darwin";
const MAC_TRIPLES: [&str; 2] = ["aarch64-apple-darwin", "x86_64-apple-darwin"];

pub fn build() -> Result<()> {
    let triple = std::env::var("TAURI_ENV_TARGET_TRIPLE")
        .context("TAURI_ENV_TARGET_TRIPLE is unset: run this through `tauri build`/`tauri dev`")?;
    let binaries = root().join("apps/desktop/binaries");
    if binaries.exists() {
        fs::remove_dir_all(&binaries)?;
    }
    fs::create_dir_all(&binaries)?;

    if triple == UNIVERSAL {
        // The bundle takes the universal binary; each per-architecture desktop build checks
        // for its own triple's sidecar.
        let universal = binaries.join(format!("broadcast-cli-{UNIVERSAL}"));
        let mut lipo = Command::new("/usr/bin/lipo");
        lipo.arg("-create");
        for triple in MAC_TRIPLES {
            lipo.arg(build_cli(triple)?);
        }
        status(lipo.arg("-output").arg(&universal))?;
        for triple in MAC_TRIPLES {
            fs::hard_link(&universal, binaries.join(format!("broadcast-cli-{triple}")))?;
        }
        return Ok(());
    }

    let suffix = exe_suffix(&triple);
    fs::copy(
        build_cli(&triple)?,
        binaries.join(format!("broadcast-cli-{triple}{suffix}")),
    )?;
    Ok(())
}

fn build_cli(triple: &str) -> Result<PathBuf> {
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
    status(Command::new(cargo).current_dir(root()).args([
        "build",
        "--release",
        "--locked",
        "-p",
        "broadcast-cli",
        "--target",
        triple,
    ]))?;
    Ok(root()
        .join("target")
        .join(triple)
        .join("release")
        .join(format!("broadcast{}", exe_suffix(triple))))
}

fn exe_suffix(triple: &str) -> &'static str {
    if triple.contains("windows") {
        ".exe"
    } else {
        ""
    }
}
