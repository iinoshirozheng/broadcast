//! The Codex desktop app's processes: discovery from `ps`, the managed instance launched with
//! Broadcast's own profile, stopping it and bringing it to the front.

#[cfg(not(windows))]
use std::path::Path;
use std::path::PathBuf;
use std::sync::LazyLock;
use std::time::Duration;

#[cfg(not(windows))]
use anyhow::Context;
use anyhow::{Result, bail};
use regex::Regex;

use crate::command;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Record {
    pub pid: u32,
    pub command: String,
}

/// The app and the browser profile Broadcast launches it with.
#[derive(Clone)]
pub struct App {
    pub path: PathBuf,
    pub profile: PathBuf,
}

const POLL: Duration = Duration::from_millis(100);

async fn output(program: &str, args: &[&str]) -> Option<String> {
    let output = command(program).args(args).stdin(std::process::Stdio::null()).output().await.ok()?;
    output.status.success().then(|| String::from_utf8_lossy(&output.stdout).into_owned())
}

impl App {
    /// `codexExecutablePath`: the binary `ps` reports for the app.
    #[cfg(not(windows))]
    pub fn executable(&self) -> PathBuf {
        if cfg!(target_os = "linux") {
            return if self.path == Path::new("/usr/bin/chatgpt") { PathBuf::from("/usr/lib/chatgpt/ChatGPT") } else { self.path.clone() };
        }
        if !cfg!(target_os = "macos") {
            return self.path.clone();
        }
        let name = self.path.file_stem().unwrap_or_default();
        self.path.join("Contents").join("MacOS").join(name)
    }

    fn profile_argument(&self) -> String {
        format!("--user-data-dir={}", self.profile.display())
    }

    /// `codexAppBundleBuild`: `CFBundleVersion`, which changes when the app updates itself.
    pub async fn bundle_build(&self) -> Option<String> {
        if !cfg!(target_os = "macos") {
            return None;
        }
        let plist = self.path.join("Contents").join("Info.plist");
        let build = output("/usr/bin/plutil", &["-extract", "CFBundleVersion", "raw", "-o", "-", &plist.to_string_lossy()]).await?;
        Some(build.trim().to_owned()).filter(|build| !build.is_empty())
    }

    /// `codexAppProcesses`: every process running the app's binary.
    #[cfg(windows)]
    pub async fn processes(&self) -> Result<Vec<Record>> {
        Ok(crate::windows::processes(&self.path).await?.into_iter().map(|(record, _)| record).collect())
    }

    /// `codexAppProcesses`: every process running the app's binary.
    #[cfg(not(windows))]
    pub async fn processes(&self) -> Result<Vec<Record>> {
        let listing = output("/bin/ps", &["-ww", "-axo", "pid=,command="]).await.context("Unable to inspect the launched Codex process")?;
        let executable = self.executable().to_string_lossy().into_owned();
        let prefix = format!("{executable} ");
        Ok(listing
            .lines()
            .filter_map(|line| {
                let line = line.trim_start();
                let (pid, command) = line.split_once(|character: char| character.is_ascii_whitespace())?;
                let command = command.trim_start();
                let pid = pid.parse().ok()?;
                (command == executable || command.starts_with(&prefix)).then(|| Record { pid, command: command.to_owned() })
            })
            .collect())
    }

    /// `codexUpdateReplacementProcess`: after the managed instance exits because the app updated
    /// itself, the one ordinary instance the updater relaunched.
    pub async fn update_replacement(&self, exited: u32, previous_build: Option<&str>) -> Result<Option<(String, Record)>> {
        let Some(previous_build) = previous_build.filter(|_| cfg!(target_os = "macos")) else { return Ok(None) };
        let Some(build) = self.bundle_build().await.filter(|build| build != previous_build) else { return Ok(None) };
        let mut candidates: Vec<Record> = self.processes().await?.into_iter().filter(|record| record.pid != exited).collect();
        if candidates.len() != 1 {
            return Ok(None);
        }
        let candidate = candidates.remove(0);
        let profile = format!(" {}", self.profile_argument());
        if candidate.command.contains(&profile) || DEBUGGING_FLAG.is_match(&candidate.command) {
            return Ok(None);
        }
        Ok(Some((build, candidate)))
    }

    /// `managedCodexProcess`: the instance running Broadcast's profile.
    pub async fn managed(&self) -> Result<Option<Record>> {
        #[cfg(windows)]
        let mut managed = crate::windows::managed(&self.path, &self.profile).await?;
        #[cfg(not(windows))]
        let mut managed: Vec<Record> = {
            let profile = format!(" {} ", self.profile_argument());
            self.processes().await?.into_iter().filter(|record| record.command.contains(&profile)).collect()
        };
        if managed.len() > 1 {
            bail!("Multiple managed Codex processes are running");
        }
        Ok(managed.pop())
    }

    /// `launchCodexWithLaunchServices`: open the app with Broadcast's profile and debugging port.
    pub async fn launch(&self, http: &reqwest::Client, port: u16) -> Result<Record> {
        let existing = self.managed().await?;
        if let Some(existing) = existing {
            if uses_port(&existing, port) {
                return Ok(existing);
            }
            self.stop(&existing).await?;
        }
        if reachable(http, port).await {
            bail!("Codex CDP port {port} is already in use");
        }
        #[cfg(windows)]
        crate::windows::launch(&self.path, &self.profile, port).await?;
        #[cfg(not(windows))]
        self.open_with_launch_services(port).await?;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
        while tokio::time::Instant::now() < deadline {
            if let Some(launched) = self.managed().await? {
                if uses_port(&launched, port) {
                    return Ok(launched);
                }
                bail!("The platform launcher started Codex on an unexpected CDP port");
            }
            tokio::time::sleep(POLL).await;
        }
        bail!("The platform launcher did not start the managed Codex process")
    }

    #[cfg(not(windows))]
    async fn open_with_launch_services(&self, port: u16) -> Result<()> {
        let status = command("/usr/bin/open")
            .arg("-a")
            .arg(&self.path)
            .arg("--args")
            .arg(self.profile_argument())
            .arg("--remote-debugging-address=127.0.0.1")
            .arg(format!("--remote-debugging-port={port}"))
            .arg(format!("--remote-allow-origins=http://127.0.0.1:{port}"))
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .await?;
        if !status.success() {
            bail!("LaunchServices failed to start Codex ({})", exit_reason(status));
        }
        Ok(())
    }

    /// `stopManagedCodex`: end the process tree.
    #[cfg(windows)]
    pub async fn stop(&self, record: &Record) -> Result<()> {
        if !self.processes().await?.contains(record) {
            return Ok(());
        }
        crate::windows::stop(record.pid).await
    }

    /// `stopManagedCodex`: SIGTERM, then SIGKILL after 3 s.
    #[cfg(unix)]
    pub async fn stop(&self, record: &Record) -> Result<()> {
        use nix::sys::signal::Signal;

        if !is_running(record).await {
            return Ok(());
        }
        if !signal(record.pid, Signal::SIGTERM)? || exits_within(record, Duration::from_secs(3)).await {
            return Ok(());
        }
        if signal(record.pid, Signal::SIGKILL)? && !exits_within(record, Duration::from_secs(1)).await {
            bail!("Unable to stop the managed Codex process");
        }
        Ok(())
    }

    /// `launchCodexWithPipe`'s spawn: the binary with Broadcast's profile, debugging over fds 3/4.
    #[cfg(unix)]
    pub fn spawn_with_pipe(&self) -> Result<(tokio::process::Child, crate::cdp::Pipe)> {
        let executable = self.executable();
        let arguments = [self.profile_argument(), "--remote-debugging-pipe".to_owned()];
        crate::cdp::Pipe::spawn(&executable, &arguments).map_err(|error| {
            let diagnostic: Vec<&str> =
                arguments.iter().map(|argument| if argument.starts_with("--user-data-dir=") { "--user-data-dir=<broadcast-profile>" } else { argument }).collect();
            let errno = error.raw_os_error().map(|errno| format!("errno={errno}; ")).unwrap_or_default();
            anyhow::anyhow!(
                "Managed Codex spawn failed: executable={}; arguments={}; code={:?}; {errno}syscall=\"spawn\"; message={}",
                serde_json::Value::from(executable.to_string_lossy()),
                serde_json::json!(diagnostic),
                error.kind(),
                serde_json::Value::from(error.to_string()),
            )
            .context(SpawnFailure)
        })
    }
}

/// Marks a launch that could not start the Codex binary at all; the launcher then waits.
#[derive(Debug)]
pub struct SpawnFailure;

impl std::fmt::Display for SpawnFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("managed Codex spawn failure")
    }
}

#[cfg(not(windows))]
fn exit_reason(status: std::process::ExitStatus) -> String {
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        if let Some(signal) = status.signal() {
            return nix::sys::signal::Signal::try_from(signal).map_or_else(|_| signal.to_string(), |signal| signal.as_str().to_owned());
        }
    }
    status.code().map_or_else(|| "null".to_owned(), |code| code.to_string())
}

static DEBUGGING_FLAG: LazyLock<Regex> = LazyLock::new(|| Regex::new(r" --remote-debugging-port(?:=|\s|$)").expect("valid regex"));
static DEBUGGING_PORT: LazyLock<Regex> = LazyLock::new(|| Regex::new(r" --remote-debugging-port=(\d+)(?: |$)").expect("valid regex"));

/// `codexProcessDebuggingPort`.
pub fn debugging_port(record: &Record) -> Option<u16> {
    DEBUGGING_PORT.captures(&record.command)?[1].parse().ok().filter(|port| *port > 0)
}

/// `managedCodexUsesPort`.
pub fn uses_port(record: &Record, port: u16) -> bool {
    record.command.contains(&format!(" --remote-debugging-port={port} "))
}

/// `isManagedCodexRunning`: the pid still runs the same command line.
#[cfg(unix)]
async fn is_running(record: &Record) -> bool {
    output("/bin/ps", &["-ww", "-p", &record.pid.to_string(), "-o", "command="])
        .await
        .is_some_and(|command| command.trim_end() == record.command)
}

#[cfg(unix)]
pub(crate) fn signal(pid: u32, signal: nix::sys::signal::Signal) -> Result<bool> {
    let pid = nix::unistd::Pid::from_raw(i32::try_from(pid)?);
    match nix::sys::signal::kill(pid, signal) {
        Ok(()) => Ok(true),
        Err(nix::errno::Errno::ESRCH) => Ok(false),
        Err(error) => Err(error.into()),
    }
}

#[cfg(unix)]
async fn exits_within(record: &Record, timeout: Duration) -> bool {
    let deadline = tokio::time::Instant::now() + timeout;
    while tokio::time::Instant::now() < deadline {
        if !is_running(record).await {
            return true;
        }
        tokio::time::sleep(POLL).await;
    }
    !is_running(record).await
}

/// `activateCodexApp`: bring the window to the front; Windows may refuse, which only logs.
#[cfg(windows)]
pub async fn activate(pid: u32) -> Result<()> {
    if let Err(error) = crate::windows::activate_window(pid).await {
        eprintln!("Unable to activate the Codex window: {error:#}");
    }
    Ok(())
}

/// `activateCodexApp`: bring the app's window to the front.
#[cfg(not(windows))]
pub async fn activate(pid: u32) -> Result<()> {
    if !cfg!(target_os = "macos") {
        return Ok(());
    }
    let script = format!(
        "ObjC.import(\"AppKit\"); const app = $.NSRunningApplication.runningApplicationWithProcessIdentifier({pid}); if (!app || !app.activateWithOptions(1)) throw new Error(\"Unable to activate Codex\");"
    );
    let status = command("/usr/bin/osascript")
        .args(["-l", "JavaScript", "-e", &script])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .await?;
    if !status.success() {
        bail!("Unable to activate the Codex app");
    }
    Ok(())
}

/// `isReachable` for the debugging endpoint.
pub async fn reachable(http: &reqwest::Client, port: u16) -> bool {
    let request = http.get(format!("http://127.0.0.1:{port}/json/version")).timeout(Duration::from_millis(1_500));
    request.send().await.is_ok_and(|response| response.status().is_success())
}

/// `waitUntilReachable`, polling every 250 ms.
pub async fn wait_until_reachable(http: &reqwest::Client, port: u16, timeout: Duration) -> Result<()> {
    let deadline = tokio::time::Instant::now() + timeout;
    while tokio::time::Instant::now() < deadline {
        if reachable(http, port).await {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    bail!("Timed out waiting for http://127.0.0.1:{port}/json/version")
}

/// `isCodexTarget`: a Codex window, not the dictation or avatar overlays.
pub fn is_codex_target(target: &serde_json::Value) -> bool {
    let url = target["url"].as_str();
    target["type"] == "page"
        && !url.is_some_and(|url| url.contains("initialRoute=%2Fglobal-dictation") || url.contains("initialRoute=%2Favatar-overlay"))
        && (url.is_some_and(|url| url.starts_with("app://")) || target["title"] == "Codex")
}

/// `codexTargets(port).length > 0`: whether the debugging port serves a Codex window.
pub async fn has_codex_targets(http: &reqwest::Client, port: u16) -> bool {
    let Ok(response) = http.get(format!("http://127.0.0.1:{port}/json/list")).send().await else { return false };
    let Ok(targets) = response.json::<Vec<serde_json::Value>>().await else { return false };
    targets.iter().any(is_codex_target)
}
