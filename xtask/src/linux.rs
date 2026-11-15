//! Checks of the Linux x64 deb and AppImage `tauri build` produced, and of their updater entries.

use std::fs;
use std::io::Read;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result, ensure};

use crate::release::Metadata;
use crate::{read_json, run, updater};

pub fn verify_packages(deb: &Path, appimage: &Path) -> Result<()> {
    ensure!(
        cfg!(target_os = "linux"),
        "Linux packages must be verified on Linux"
    );
    let beta = std::env::var("BROADCAST_RELEASE_VERSION").is_ok_and(|v| v.contains("-beta."));
    let product = if beta { "Broadcast Beta" } else { "Broadcast" };
    let scratch = tempfile::tempdir()?;

    let architecture = run(Command::new("dpkg-deb")
        .arg("--field")
        .arg(deb)
        .arg("Architecture"))?;
    ensure!(
        architecture == "amd64",
        "the deb architecture is {architecture}, not amd64"
    );
    let deb_root = scratch.path().join("deb");
    run(Command::new("dpkg-deb")
        .arg("--extract")
        .arg(deb)
        .arg(&deb_root))?;
    verify_root(&deb_root, product)?;

    let appimage = fs::canonicalize(appimage)?;
    executable_elf(&appimage)?;
    let extracted = scratch.path().join("appimage");
    fs::create_dir(&extracted)?;
    run(Command::new(&appimage)
        .arg("--appimage-extract")
        .current_dir(&extracted))?;
    verify_root(&extracted.join("squashfs-root"), product)?;

    println!("Verified the Linux x64 deb and AppImage contents");
    Ok(())
}

fn verify_root(root: &Path, product: &str) -> Result<()> {
    executable_elf(&root.join("usr/bin/broadcast-launcher"))?;
    executable_elf(&root.join("usr/bin/broadcast-cli"))?;
    let resources = root.join("usr/lib").join(product);
    let wrapper = resources.join("bin/broadcast");
    executable(&wrapper)?;
    for resource in ["web/index.html", "skills/manage-broadcast/SKILL.md"] {
        ensure!(
            resources.join(resource).is_file(),
            "{} lacks {resource}",
            root.display()
        );
    }
    // The wrapper finds the sidecar relative to itself, so it runs inside the extracted tree.
    let help = run(Command::new(&wrapper).arg("--help"))?;
    ensure!(
        help.starts_with("Usage: broadcast "),
        "{} does not run the packaged broadcast-cli",
        wrapper.display()
    );
    Ok(())
}

fn executable(path: &Path) -> Result<()> {
    let details = fs::metadata(path).with_context(|| format!("{} is missing", path.display()))?;
    ensure!(
        details.is_file() && details.permissions().mode() & 0o111 != 0,
        "{} is not an executable file",
        path.display()
    );
    Ok(())
}

fn executable_elf(path: &Path) -> Result<()> {
    executable(path)?;
    let mut header = [0; 20];
    fs::File::open(path)?.read_exact(&mut header)?;
    ensure!(
        header[..4] == [0x7f, b'E', b'L', b'F']
            && header[4] == 2
            && header[5] == 1
            && u16::from_le_bytes([header[18], header[19]]) == 0x3e,
        "{} is not a Linux x86_64 ELF",
        path.display()
    );
    Ok(())
}

/// `latest.json` has a verified deb and AppImage entry, and no installer-agnostic one.
pub fn verify_updater(deb: &Path, appimage: &Path, latest: &Path, tag: &str) -> Result<()> {
    let metadata = Metadata::load(tag)?;
    let latest = read_json(latest)?;
    ensure!(
        latest["version"] == metadata.release_version.as_str(),
        "latest.json version is not {}",
        metadata.release_version
    );
    ensure!(
        latest["platforms"].get("linux-x86_64").is_none(),
        "latest.json must use installer-specific Linux updater entries"
    );
    let pubkey = updater::configured_pubkey()?;
    for (platform, artifact) in [
        ("linux-x86_64-deb", deb),
        ("linux-x86_64-appimage", appimage),
    ] {
        let signature = updater::verify(artifact, &pubkey)?;
        let name = artifact.file_name().context("artifact has no name")?;
        let entry = &latest["platforms"][platform];
        ensure!(
            entry["url"] == metadata.download_url(&name.to_string_lossy()).as_str()
                && entry["signature"] == signature.as_str(),
            "latest.json {platform} entry is incorrect"
        );
    }
    println!("Verified the Linux x86_64 deb and AppImage updaters of {tag}");
    Ok(())
}
