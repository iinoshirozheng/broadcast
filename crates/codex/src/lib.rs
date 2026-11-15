//! The Codex desktop app on this device: its executables, its processes and the Broadcast
//! injector that mounts the board inside it over the Chrome DevTools Protocol.

mod automation;
pub mod cdp;
mod host;
mod inject;
pub mod launcher;
mod policy;
mod process;
mod profile;
mod quota;
mod remote;
mod runtime;
#[cfg(windows)]
mod windows;

use std::path::{Path, PathBuf};

use serde_json::Value;
use tokio::process::Command;
use tokio::sync::oneshot;

pub use launcher::{Cdp, Event, Launcher, Options};

/// A request from the Broadcast server to a Codex host's App Server through the renderer bridge.
pub struct HostRequest {
    pub host_id: String,
    pub method: String,
    pub params: Value,
    pub reply: oneshot::Sender<anyhow::Result<Value>>,
}

/// An App Server notification a Codex host forwarded through the renderer bridge.
pub struct HostNotification {
    pub host_id: String,
    pub method: String,
    pub params: Value,
}

fn executable_file(candidate: PathBuf) -> Option<String> {
    let metadata = std::fs::metadata(&candidate).ok()?;
    #[cfg(unix)]
    let runnable = {
        use std::os::unix::fs::PermissionsExt;
        metadata.is_file() && metadata.permissions().mode() & 0o111 != 0
    };
    #[cfg(not(unix))]
    let runnable = metadata.is_file();
    runnable.then(|| candidate.to_string_lossy().into_owned())
}

/// `codexExecutableInApp`: the CLI the desktop app bundles.
fn executable_in_app(app: &Path) -> PathBuf {
    if cfg!(windows) {
        app.parent()
            .unwrap_or(app)
            .join("resources")
            .join("codex.exe")
    } else if cfg!(target_os = "linux") {
        PathBuf::from("/usr/lib/chatgpt/resources/codex")
    } else {
        app.join("Contents").join("Resources").join("codex")
    }
}

/// `resolveCodexExecutable`: the explicit path, the app's bundled CLI, the CLI on `PATH`, then
/// the macOS app bundles.
pub fn executable(explicit: Option<&str>, app: Option<&Path>) -> String {
    if let Some(explicit) = explicit.map(str::trim).filter(|value| !value.is_empty()) {
        return explicit.to_owned();
    }
    if let Some(found) = app.and_then(|app| executable_file(executable_in_app(app))) {
        return found;
    }
    let names: &[&str] = if cfg!(windows) {
        &["codex.exe", "node_modules/@openai/codex/bin/codex.js"]
    } else {
        &["codex"]
    };
    let on_path = std::env::var_os("PATH")
        .into_iter()
        .flat_map(|paths| std::env::split_paths(&paths).collect::<Vec<_>>())
        .filter(|directory| !directory.as_os_str().is_empty())
        .find_map(|directory| {
            names
                .iter()
                .find_map(|name| executable_file(directory.join(name)))
        });
    if let Some(found) = on_path {
        return found;
    }
    if cfg!(target_os = "macos") {
        let home = std::env::home_dir().unwrap_or_default();
        for directory in [PathBuf::from("/Applications"), home.join("Applications")] {
            for app in ["ChatGPT.app", "Codex.app"] {
                if let Some(found) = executable_file(executable_in_app(&directory.join(app))) {
                    return found;
                }
            }
        }
    }
    "codex".to_owned()
}

/// `resolveRunnableCodexExecutable`: a CLI inside a `WindowsApps` package runs from a copy in
/// `<data_dir>/codex-runtime`, refreshed when a file's size or modification time changes.
pub fn runnable_executable(executable: String, data_dir: &Path) -> std::io::Result<String> {
    if !cfg!(windows) || !executable.to_lowercase().contains("\\windowsapps\\") {
        return Ok(executable);
    }
    let source_directory = Path::new(&executable).parent().unwrap_or(Path::new(""));
    let cache = data_dir.join("codex-runtime");
    std::fs::create_dir_all(&cache)?;
    for name in [
        "codex.exe",
        "codex-code-mode-host.exe",
        "codex-command-runner.exe",
        "codex-windows-sandbox-setup.exe",
    ] {
        let source_path = source_directory.join(name);
        let cached_path = cache.join(name);
        let source = std::fs::metadata(&source_path)?;
        match std::fs::metadata(&cached_path) {
            Ok(cached)
                if cached.len() == source.len() && cached.modified()? == source.modified()? =>
            {
                continue;
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
        std::fs::copy(&source_path, &cached_path)?;
        let times = std::fs::FileTimes::new()
            .set_accessed(source.accessed()?)
            .set_modified(source.modified()?);
        std::fs::File::options()
            .write(true)
            .open(&cached_path)?
            .set_times(times)?;
    }
    Ok(cache.join("codex.exe").to_string_lossy().into_owned())
}

/// A command whose environment drops the launcher's `BROADCAST_*` variables.
pub fn command(program: impl AsRef<std::ffi::OsStr>) -> Command {
    let mut command = Command::new(program);
    command
        .env_clear()
        .envs(std::env::vars_os().filter(|(name, _)| {
            !name
                .to_str()
                .is_some_and(|name| name.starts_with("BROADCAST_"))
        }));
    command
}
