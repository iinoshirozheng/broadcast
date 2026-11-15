//! Codex app-server JSON-RPC over stdio: a lazily started local `codex app-server --stdio`
//! child, and the per-host client for remote Codex projects, whose requests travel through the
//! desktop host bridge.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, bail};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStderr};
use tokio::sync::{mpsc, oneshot, watch};

use super::exec::{self, Line};

const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_STDOUT_LINE: usize = 4 * 1024 * 1024;
const STDERR_LIMIT: usize = 16 * 1024;
const CLOSE_GRACE: Duration = Duration::from_secs(1);

/// Which app-server process a notification came from: a local child generation or a remote host.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Origin {
    Local(u64),
    Host(String),
}

pub struct Notification {
    pub origin: Origin,
    pub method: String,
    pub params: Value,
}

pub type Notifications = mpsc::UnboundedSender<Notification>;

type Reply = oneshot::Sender<std::result::Result<Value, String>>;

/// One running child: its stdin writer, the requests awaiting replies and its exit state.
struct Connection {
    generation: u64,
    pid: Option<u32>,
    writer: Mutex<Option<mpsc::UnboundedSender<String>>>,
    pending: Mutex<HashMap<u64, (String, Reply)>>,
    terminated: AtomicBool,
    exited: watch::Receiver<bool>,
    stderr: Arc<Mutex<Vec<u8>>>,
    notifications: Notifications,
}

impl Connection {
    fn write(&self, line: String) {
        if let Some(writer) = self.writer.lock().unwrap_or_else(|poison| poison.into_inner()).as_ref() {
            let _ = writer.send(line);
        }
    }

    /// `#handleExit`: fails pending requests, stops the child and announces the termination once.
    fn terminate(&self, message: &str) {
        if self.terminated.swap(true, Ordering::SeqCst) {
            return;
        }
        let pending: Vec<_> = self.pending.lock().unwrap_or_else(|poison| poison.into_inner()).drain().collect();
        for (_, (_, reply)) in pending {
            let _ = reply.send(Err(message.to_owned()));
        }
        if !*self.exited.borrow() {
            signal(self.pid, false);
        }
        let _ = self.notifications.send(Notification {
            origin: Origin::Local(self.generation),
            method: "app-server/terminated".into(),
            params: json!({ "message": message }),
        });
    }

    fn handle(&self, message: Value) {
        let Some(object) = message.as_object() else { return };
        let method = object.get("method");
        if object.contains_key("id") && !method.is_some_and(broadcast_domain::js::truthy) {
            let Some(id) = object["id"].as_u64() else { return };
            let Some((method, reply)) = self.pending.lock().unwrap_or_else(|poison| poison.into_inner()).remove(&id)
            else {
                return;
            };
            let result = match object.get("error").filter(|error| broadcast_domain::js::truthy(error)) {
                Some(error) => {
                    let detail = match &error["message"] {
                        Value::Null => "unknown error".to_owned(),
                        Value::String(text) => text.clone(),
                        other => other.to_string(),
                    };
                    Err(format!("Codex app-server rejected '{method}': {detail}"))
                }
                None => Ok(object.get("result").cloned().unwrap_or(Value::Null)),
            };
            let _ = reply.send(result);
            return;
        }
        let Some(method) = method.and_then(Value::as_str) else { return };
        if let Some(id) = object.get("id") {
            let error = json!({ "code": -32601, "message": format!("Unsupported server request '{method}'") });
            self.write(format!("{}\n", json!({ "id": id, "error": error })));
            return;
        }
        let _ = self.notifications.send(Notification {
            origin: Origin::Local(self.generation),
            method: method.to_owned(),
            params: object.get("params").cloned().unwrap_or(Value::Null),
        });
    }

    async fn request(&self, method: &str, params: Value) -> anyhow::Result<Value> {
        if self.terminated.load(Ordering::SeqCst) {
            bail!("Codex app-server is not running");
        }
        static NEXT_ID: AtomicU64 = AtomicU64::new(1);
        let id = NEXT_ID.fetch_add(1, Ordering::SeqCst);
        let (reply, response) = oneshot::channel();
        self.pending.lock().unwrap_or_else(|poison| poison.into_inner()).insert(id, (method.to_owned(), reply));
        self.write(format!("{}\n", json!({ "id": id, "method": method, "params": params })));
        match tokio::time::timeout(REQUEST_TIMEOUT, response).await {
            Ok(Ok(result)) => result.map_err(anyhow::Error::msg),
            Ok(Err(_)) => bail!("Codex app-server is not running"),
            Err(_) => {
                self.pending.lock().unwrap_or_else(|poison| poison.into_inner()).remove(&id);
                bail!("Codex app-server request '{method}' timed out")
            }
        }
    }
}

pub(super) fn signal(pid: Option<u32>, kill: bool) {
    #[cfg(unix)]
    if let Some(pid) = pid.and_then(|pid| i32::try_from(pid).ok()) {
        use nix::sys::signal::Signal;
        let signal = if kill { Signal::SIGKILL } else { Signal::SIGTERM };
        let _ = nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid), signal);
    }
    #[cfg(not(unix))]
    if let Some(pid) = pid {
        let _ = kill;
        let _ = std::process::Command::new("taskkill").args(["/PID", &pid.to_string(), "/F"]).status();
    }
}

/// `CodexAppServer`: the local app-server, started on first use and restarted after it exits.
pub struct LocalAppServer {
    executable: String,
    current: tokio::sync::Mutex<Option<Arc<Connection>>>,
    generations: AtomicU64,
    closing: AtomicBool,
    notifications: Notifications,
}

impl LocalAppServer {
    pub fn new(executable: String, notifications: Notifications) -> Self {
        Self {
            executable,
            current: tokio::sync::Mutex::new(None),
            generations: AtomicU64::new(0),
            closing: AtomicBool::new(false),
            notifications,
        }
    }

    /// The generation of the running child, if any (`appServer.child`).
    pub async fn generation(&self) -> Option<u64> {
        self.current.lock().await.as_ref().filter(|c| !c.terminated.load(Ordering::SeqCst)).map(|c| c.generation)
    }

    pub async fn request(&self, method: &str, params: Value) -> anyhow::Result<Value> {
        let connection = self.ensure_started().await?;
        connection.request(method, params).await
    }

    async fn ensure_started(&self) -> anyhow::Result<Arc<Connection>> {
        if self.closing.load(Ordering::SeqCst) {
            bail!("Codex app-server is closing");
        }
        let mut current = self.current.lock().await;
        if let Some(connection) = current.as_ref().filter(|c| !c.terminated.load(Ordering::SeqCst)) {
            return Ok(Arc::clone(connection));
        }
        let mut command = broadcast_codex::command(&self.executable);
        command
            .args(["app-server", "--stdio"])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);
        let mut child = command.spawn()?;
        let generation = self.generations.fetch_add(1, Ordering::SeqCst) + 1;
        let (writer, lines) = mpsc::unbounded_channel();
        let (exit_sender, exited) = watch::channel(false);
        let connection = Arc::new(Connection {
            generation,
            pid: child.id(),
            writer: Mutex::new(Some(writer)),
            pending: Mutex::new(HashMap::new()),
            terminated: AtomicBool::new(false),
            exited,
            stderr: Arc::new(Mutex::new(Vec::new())),
            notifications: self.notifications.clone(),
        });
        start_io(&connection, &mut child, lines)?;
        tokio::spawn(wait_exit(Arc::clone(&connection), child, exit_sender));
        *current = Some(Arc::clone(&connection));
        let initialize = json!({
            "clientInfo": { "name": "broadcast", "title": "Broadcast", "version": "1.0.1" },
            "capabilities": { "experimentalApi": true, "requestAttestation": false },
        });
        connection.request("initialize", initialize).await?;
        connection.write(format!("{}\n", json!({ "method": "initialized" })));
        Ok(connection)
    }

    pub async fn close(&self) {
        self.closing.store(true, Ordering::SeqCst);
        let Some(connection) = self.current.lock().await.take() else { return };
        connection.terminate("Codex app-server closed");
        connection.writer.lock().unwrap_or_else(|poison| poison.into_inner()).take();
        let mut exited = connection.exited.clone();
        if tokio::time::timeout(CLOSE_GRACE, exited.wait_for(|exited| *exited)).await.is_err() {
            signal(connection.pid, true);
        }
    }
}

fn start_io(connection: &Arc<Connection>, child: &mut Child, mut lines: mpsc::UnboundedReceiver<String>) -> anyhow::Result<()> {
    let mut stdin = child.stdin.take().context("app-server stdin is unavailable")?;
    let stdout = child.stdout.take().context("app-server stdout is unavailable")?;
    let stderr = child.stderr.take().context("app-server stderr is unavailable")?;
    let writer = Arc::clone(connection);
    tokio::spawn(async move {
        while let Some(line) = lines.recv().await {
            if let Err(error) = stdin.write_all(line.as_bytes()).await {
                writer.terminate(&error.to_string());
                return;
            }
        }
    });
    let reader = Arc::clone(connection);
    tokio::spawn(async move {
        let mut stdout = BufReader::new(stdout);
        let mut line = Vec::new();
        loop {
            match exec::read_line(&mut stdout, &mut line, MAX_STDOUT_LINE).await {
                Ok(Line::Read) => {}
                Ok(Line::Eof) => return,
                Ok(Line::TooLong) => return reader.terminate("Codex app-server output exceeded its limit"),
                Err(error) => return reader.terminate(&error.to_string()),
            }
            let text = String::from_utf8_lossy(&line);
            let text = text.trim();
            if text.is_empty() {
                continue;
            }
            match serde_json::from_str(text) {
                Ok(message) => reader.handle(message),
                Err(error) => eprintln!("Codex app-server returned invalid JSON {error}"),
            }
        }
    });
    tokio::spawn(keep_stderr(stderr, Arc::clone(&connection.stderr)));
    Ok(())
}

async fn keep_stderr(mut stderr: ChildStderr, kept: Arc<Mutex<Vec<u8>>>) {
    let mut chunk = [0u8; 4096];
    while let Ok(read) = stderr.read(&mut chunk).await {
        if read == 0 {
            return;
        }
        let mut kept = kept.lock().unwrap_or_else(|poison| poison.into_inner());
        kept.extend_from_slice(&chunk[..read]);
        let excess = kept.len().saturating_sub(STDERR_LIMIT);
        kept.drain(..excess);
    }
}

async fn wait_exit(connection: Arc<Connection>, mut child: Child, exited: watch::Sender<bool>) {
    let status = child.wait().await;
    let _ = exited.send(true);
    let message = match status {
        Ok(status) => {
            let stderr = String::from_utf8_lossy(&connection.stderr.lock().unwrap_or_else(|p| p.into_inner())).into_owned();
            let stderr = stderr.trim();
            let suffix = if stderr.is_empty() { String::new() } else { format!(": {stderr}") };
            format!("Codex app-server exited ({}){suffix}", exit_label(&status))
        }
        Err(error) => error.to_string(),
    };
    connection.terminate(&message);
}

/// `signal || code`: the terminating signal's name, else the exit code.
pub fn exit_label(status: &std::process::ExitStatus) -> String {
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        if let Some(signal) = status.signal() {
            return nix::sys::signal::Signal::try_from(signal).map_or_else(|_| signal.to_string(), |s| s.as_str().to_owned());
        }
    }
    status.code().map_or_else(|| "null".to_owned(), |code| code.to_string())
}

/// `CodexHostAppServer`: a remote host's app-server, reached through the desktop launcher's host
/// bridge. A server started without a launcher reports the bridge unavailable.
pub struct HostAppServer {
    pub host_id: String,
    pub requests: mpsc::UnboundedSender<broadcast_codex::HostRequest>,
}

impl HostAppServer {
    pub async fn request(&self, method: &str, params: Value) -> anyhow::Result<Value> {
        let (reply, response) = oneshot::channel();
        let request = broadcast_codex::HostRequest { host_id: self.host_id.clone(), method: method.to_owned(), params, reply };
        if self.requests.send(request).is_err() {
            bail!("Codex host bridge is unavailable");
        }
        match tokio::time::timeout(REQUEST_TIMEOUT, response).await {
            Err(_) => bail!("Codex host request '{method}' timed out"),
            Ok(Err(_)) => bail!("Codex host bridge closed"),
            Ok(Ok(result)) => result.map_err(|error| anyhow::anyhow!("Codex host rejected '{method}': {error:#}")),
        }
    }
}

/// The app-server a thread runs on.
#[derive(Clone)]
pub enum AppServer {
    Local(Arc<LocalAppServer>),
    Host(Arc<HostAppServer>),
}

impl AppServer {
    pub async fn request(&self, method: &str, params: Value) -> anyhow::Result<Value> {
        match self {
            Self::Local(server) => server.request(method, params).await,
            Self::Host(server) => server.request(method, params).await,
        }
    }

    /// The origin its current process's notifications carry (`active.appServerChild`).
    pub async fn origin(&self) -> Option<Origin> {
        match self {
            Self::Local(server) => server.generation().await.map(Origin::Local),
            Self::Host(server) => Some(Origin::Host(server.host_id.clone())),
        }
    }

    /// `listSkills`: `skills/list` for one workspace.
    pub async fn list_skills(&self, workspace: &str, force_reload: bool) -> anyhow::Result<Vec<Value>> {
        let result = self.request("skills/list", json!({ "cwds": [workspace], "forceReload": force_reload })).await?;
        Ok(result["data"].as_array().cloned().unwrap_or_default())
    }
}
