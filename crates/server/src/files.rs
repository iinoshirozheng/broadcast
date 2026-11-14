//! Owner-only JSON files under the data directory.

use std::io::ErrorKind;
use std::path::Path;

use serde::Serialize;
use tokio::fs;

/// Parses a JSON file; a missing file is `None`.
pub async fn read_json(path: &Path) -> std::io::Result<Option<serde_json::Value>> {
    match fs::read(path).await {
        Ok(bytes) => Ok(Some(serde_json::from_slice(&bytes)?)),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

/// Writes `value` plus a trailing newline through a temporary file renamed into place, mode 0600.
pub async fn write_private<T: Serialize>(path: &Path, value: &T, pretty: bool) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).await?;
    }
    let mut bytes = if pretty { serde_json::to_vec_pretty(value)? } else { serde_json::to_vec(value)? };
    bytes.push(b'\n');
    let temporary = path.with_extension(format!("{}.{}.tmp", std::process::id(), uuid::Uuid::new_v4().simple()));
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    options.mode(0o600);
    let mut file = options.open(&temporary).await?;
    tokio::io::AsyncWriteExt::write_all(&mut file, &bytes).await?;
    file.sync_all().await?;
    drop(file);
    fs::rename(&temporary, path).await
}
