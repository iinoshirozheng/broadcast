//! `cargo xtask <task>`: the build and release tasks around `tauri build`. Tauri bundles,
//! signs, notarizes the App and signs the updater; these tasks build the `broadcast-cli`
//! sidecar it bundles, name the release, and verify what it produced.

mod dev;
#[cfg(unix)]
mod linux;
#[cfg(unix)]
mod macos;
mod release;
mod sidecar;
#[cfg(unix)]
mod updater;

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use anyhow::{Context, Result, bail};

const USAGE: &str = "\
Usage: cargo xtask <task>

  sidecar                                  build broadcast-cli for TAURI_ENV_TARGET_TRIPLE
  dev                                      run the server and the web dev server
  release-metadata <tag>                   print the release names of <tag> as JSON
  check-release-order <tag> <releases.tsv> <commit>
  macos-latest-json <tag> <dir>            write <dir>/latest.json for the macOS updater
  verify-macos <App.app> <DMG> <updater.tar.gz> [--pubkey <file>] [--release <tag>]
  verify-packaged-cli <App.app>
  verify-linux-packages <deb> <AppImage>
  verify-linux-updater <deb> <AppImage> <latest.json> <tag>
";

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    match args.as_slice() {
        ["sidecar"] => sidecar::build(),
        ["dev"] => dev::run(),
        ["release-metadata", tag] => {
            println!("{}", release::Metadata::load(tag)?.to_json());
            Ok(())
        }
        ["check-release-order", tag, releases, commit] => {
            release::check_order(tag, Path::new(releases), commit)
        }
        ["macos-latest-json", tag, dir] => release::write_macos_latest(tag, Path::new(dir)),
        #[cfg(unix)]
        ["verify-macos", app, dmg, updater, options @ ..] => {
            let mut pubkey = None;
            let mut release = None;
            let mut options = options.iter();
            while let Some(option) = options.next() {
                match (*option, options.next()) {
                    ("--pubkey", Some(path)) => pubkey = Some(Path::new(*path)),
                    ("--release", Some(tag)) => release = Some(*tag),
                    _ => bail!("{USAGE}"),
                }
            }
            macos::verify(
                Path::new(app),
                Path::new(dmg),
                Path::new(updater),
                pubkey,
                release,
            )
        }
        #[cfg(unix)]
        ["verify-packaged-cli", app] => macos::verify_packaged_cli(Path::new(app)),
        #[cfg(unix)]
        ["verify-linux-packages", deb, appimage] => {
            linux::verify_packages(Path::new(deb), Path::new(appimage))
        }
        #[cfg(unix)]
        ["verify-linux-updater", deb, appimage, latest, tag] => {
            linux::verify_updater(Path::new(deb), Path::new(appimage), Path::new(latest), tag)
        }
        _ => bail!("{USAGE}"),
    }
}

/// The workspace root: xtask lives one level below it.
pub fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("xtask sits in the workspace")
        .to_path_buf()
}

pub fn read_json(path: &Path) -> Result<serde_json::Value> {
    let text =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display()))
}

/// Runs a command to completion and fails with its output when it exits non-zero.
pub fn output(command: &mut Command) -> Result<Output> {
    let output = command
        .output()
        .with_context(|| format!("running {command:?}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stdout = String::from_utf8_lossy(&output.stdout);
        bail!(
            "{command:?} exited with {}\n{}{}",
            output.status,
            stderr.trim(),
            stdout.trim()
        );
    }
    Ok(output)
}

/// Runs a command and returns its trimmed stdout.
pub fn run(command: &mut Command) -> Result<String> {
    Ok(String::from_utf8_lossy(&output(command)?.stdout)
        .trim()
        .to_string())
}

/// Runs a command with inherited stdio, for builds whose progress belongs on the terminal.
pub fn status(command: &mut Command) -> Result<()> {
    let status = command
        .status()
        .with_context(|| format!("running {command:?}"))?;
    if !status.success() {
        bail!("{command:?} exited with {status}");
    }
    Ok(())
}
