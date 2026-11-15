//! Tauri updater signatures: minisign, with the public key (tauri.conf.json
//! `plugins.updater.pubkey`, or a `.pub` from `tauri signer generate`) and the `.sig` both
//! stored as base64 of the minisign text.

use std::fs;
use std::path::Path;

use anyhow::{Context, Result};
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use minisign_verify::{PublicKey, Signature};

use crate::{read_json, root};

/// The updater public key the App ships with.
pub fn configured_pubkey() -> Result<String> {
    let tauri = read_json(&root().join("apps/desktop/tauri.conf.json"))?;
    tauri["plugins"]["updater"]["pubkey"]
        .as_str()
        .map(str::to_string)
        .context("tauri.conf.json has no plugins.updater.pubkey")
}

/// Verifies `<artifact>.sig` against `pubkey` and returns the signature text.
pub fn verify(artifact: &Path, pubkey: &str) -> Result<String> {
    let signature_path = artifact.with_file_name(format!(
        "{}.sig",
        artifact
            .file_name()
            .context("artifact has no name")?
            .to_string_lossy()
    ));
    let signature = fs::read_to_string(&signature_path)
        .with_context(|| format!("reading {}", signature_path.display()))?;
    let key = PublicKey::decode(&decode(pubkey)?)?;
    let parsed = Signature::decode(&decode(&signature)?)?;
    key.verify(&fs::read(artifact)?, &parsed, false)
        .with_context(|| {
            format!(
                "the updater signature of {} does not verify",
                artifact.display()
            )
        })?;
    Ok(signature)
}

fn decode(text: &str) -> Result<String> {
    Ok(String::from_utf8(STANDARD.decode(text.trim())?)?)
}
