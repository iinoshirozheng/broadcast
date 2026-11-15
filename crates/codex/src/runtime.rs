//! `launcher-runtime.json`: where the `broadcast` CLI finds the running board.

use std::path::Path;

use anyhow::Result;
use serde_json::{Value, json};
use tokio::io::AsyncWriteExt;

/// `publishBroadcastRuntime`: written atomically, readable only by the user.
pub async fn publish(file: &Path, url: &str) -> Result<()> {
    if let Some(parent) = file.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    let name = file.file_name().unwrap_or_default().to_string_lossy();
    let temporary = file.with_file_name(format!("{name}.{}.tmp", std::process::id()));
    let mut options = tokio::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    options.mode(0o600);
    let mut handle = options.open(&temporary).await?;
    handle
        .write_all(
            format!(
                "{}\n",
                json!({ "version": 1, "pid": std::process::id(), "url": url })
            )
            .as_bytes(),
        )
        .await?;
    handle.flush().await?;
    drop(handle);
    tokio::fs::rename(&temporary, file).await?;
    Ok(())
}

/// `removeBroadcastRuntime`: only the descriptor this process published.
pub async fn remove(file: &Path, url: &str) -> Result<()> {
    let content = match tokio::fs::read_to_string(file).await {
        Ok(content) => content,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    let descriptor: Value = serde_json::from_str(&content)?;
    if descriptor["pid"] == std::process::id() && descriptor["url"] == url {
        tokio::fs::remove_file(file).await?;
    }
    Ok(())
}
