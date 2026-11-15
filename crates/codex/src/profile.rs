//! `importCodexBrowserProfile`: copy the Codex in-app browser's sign-ins into Broadcast's
//! profile once, so the managed instance starts signed in.

use std::path::Path;

use anyhow::Result;

const MARKER: &str = ".broadcast-browser-profile-imported-v1";
const DATABASES: [&str; 3] = [
    "Default/Partitions/codex-browser-app/Cookies",
    "Default/Partitions/codex-browser-app/Login Data",
    "Default/Partitions/codex-browser-app/Login Data For Account",
];

pub async fn import(source: Option<&Path>, profile: &Path) -> Result<()> {
    let Some(source) = source.filter(|source| *source != profile) else {
        return Ok(());
    };
    let (source, profile) = (source.to_owned(), profile.to_owned());
    tokio::task::spawn_blocking(move || copy(&source, &profile)).await?
}

fn copy(source: &Path, profile: &Path) -> Result<()> {
    let marker = profile.join(MARKER);
    if marker.try_exists()? {
        return Ok(());
    }
    let mut sources = Vec::new();
    for relative in DATABASES {
        let path = source.join(relative);
        if path.try_exists()? {
            sources.push((relative, path));
        }
    }
    if sources.is_empty() {
        return Ok(());
    }
    for (relative, path) in &sources {
        let destination = profile.join(relative);
        if let Some(parent) = destination.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let database = rusqlite::Connection::open_with_flags(
            path,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )?;
        database.backup(
            rusqlite::MAIN_DB,
            &destination,
            None::<fn(rusqlite::backup::Progress)>,
        )?;
    }
    if sources.len() == DATABASES.len() {
        std::fs::write(marker, "1\n")?;
    }
    Ok(())
}
