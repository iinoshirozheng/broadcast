//! A Chrome DevTools Protocol client. One browser connection — the loopback WebSocket endpoint or
//! the `--remote-debugging-pipe` descriptors — carries every target as a flattened session.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::sync::{mpsc, oneshot, watch};
use tokio_tungstenite::tungstenite::Message;
use url::Url;

/// `CdpPipeBrowser` waits this long for each command.
pub const COMMAND_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug)]
pub struct Event {
    pub method: String,
    pub params: Value,
}

#[derive(Clone)]
pub struct Browser(Arc<Connection>);

struct Connection {
    outgoing: mpsc::UnboundedSender<String>,
    next_id: AtomicU64,
    state: Mutex<State>,
    closed: watch::Sender<bool>,
}

#[derive(Default)]
struct State {
    closed: bool,
    pending: HashMap<u64, Pending>,
    sessions: HashMap<String, mpsc::UnboundedSender<Event>>,
}

struct Pending {
    session: Option<String>,
    reply: oneshot::Sender<Result<Value, String>>,
}

impl Connection {
    fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn receive(&self, text: &str) {
        let Ok(message) = serde_json::from_str::<Value>(text) else { return };
        if let Some(id) = message["id"].as_u64() {
            let Some(pending) = self.state().pending.remove(&id) else { return };
            let reply = match message.get("error") {
                Some(error) => Err(error["message"].as_str().unwrap_or("CDP command failed").to_owned()),
                None => Ok(message.get("result").cloned().unwrap_or_else(|| json!({}))),
            };
            let _ = pending.reply.send(reply);
            return;
        }
        let method = message["method"].as_str().unwrap_or_default().to_owned();
        let params = message.get("params").cloned().unwrap_or(Value::Null);
        if let Some(session) = message["sessionId"].as_str() {
            if let Some(events) = self.state().sessions.get(session) {
                let _ = events.send(Event { method, params });
            }
        } else if method == "Target.detachedFromTarget"
            && let Some(session) = params["sessionId"].as_str()
        {
            self.close_session(session);
        }
    }

    fn close_session(&self, session: &str) {
        let mut state = self.state();
        state.sessions.remove(session);
        let detached: Vec<u64> =
            state.pending.iter().filter(|(_, pending)| pending.session.as_deref() == Some(session)).map(|(id, _)| *id).collect();
        for id in detached {
            if let Some(pending) = state.pending.remove(&id) {
                let _ = pending.reply.send(Err("CDP session closed".to_owned()));
            }
        }
    }

    fn fail(&self, reason: &str) {
        let mut state = self.state();
        if state.closed {
            return;
        }
        state.closed = true;
        state.sessions.clear();
        for (_, pending) in state.pending.drain() {
            let _ = pending.reply.send(Err(reason.to_owned()));
        }
        drop(state);
        self.closed.send_replace(true);
    }
}

impl Browser {
    fn start() -> (Self, mpsc::UnboundedReceiver<String>, watch::Receiver<bool>) {
        let (outgoing, requests) = mpsc::unbounded_channel();
        let (closed, stopped) = watch::channel(false);
        let connection = Connection { outgoing, next_id: AtomicU64::new(0), state: Mutex::default(), closed };
        (Self(Arc::new(connection)), requests, stopped)
    }

    /// The browser endpoint `/json/version` names, which must stay on this loopback port.
    pub async fn connect(http: &reqwest::Client, port: u16) -> Result<Self> {
        let version: Value = http
            .get(format!("http://127.0.0.1:{port}/json/version"))
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        let endpoint = loopback_endpoint(version["webSocketDebuggerUrl"].as_str(), port)?;
        let (socket, _) = tokio_tungstenite::connect_async(endpoint.as_str()).await.context("CDP WebSocket connection failed")?;
        let (mut sink, mut stream) = socket.split();
        let (browser, mut requests, mut stopped) = Self::start();
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    request = requests.recv() => {
                        let Some(text) = request else { break };
                        if sink.send(Message::text(text)).await.is_err() {
                            break;
                        }
                    }
                    () = async { let _ = stopped.wait_for(|closed| *closed).await; } => break,
                }
            }
            let _ = sink.close().await;
        });
        let connection = Arc::clone(&browser.0);
        tokio::spawn(async move {
            while let Some(Ok(message)) = stream.next().await {
                if let Message::Text(text) = message {
                    connection.receive(&text);
                }
            }
            connection.fail("CDP WebSocket closed");
        });
        browser.open().await?;
        Ok(browser)
    }

    /// `CdpPipeBrowser`: NUL-delimited JSON written to the child's fd 3 and read from its fd 4.
    #[cfg(unix)]
    pub async fn pipe(pipe: Pipe) -> Result<Self> {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

        let Pipe { input, output } = pipe;
        let (browser, mut requests, mut stopped) = Self::start();
        let writer = Arc::clone(&browser.0);
        tokio::spawn(async move {
            let mut input = input;
            loop {
                tokio::select! {
                    request = requests.recv() => {
                        let Some(mut text) = request else { break };
                        text.push('\0');
                        if input.write_all(text.as_bytes()).await.is_err() {
                            writer.fail("CDP pipe closed");
                            break;
                        }
                    }
                    () = async { let _ = stopped.wait_for(|closed| *closed).await; } => break,
                }
            }
        });
        let reader = Arc::clone(&browser.0);
        let mut stopped = reader.closed.subscribe();
        tokio::spawn(async move {
            let mut output = BufReader::new(output);
            let mut frame = Vec::new();
            loop {
                frame.clear();
                tokio::select! {
                    read = output.read_until(0, &mut frame) => match read {
                        Ok(0) | Err(_) => break,
                        Ok(_) => {
                            if frame.last() == Some(&0) {
                                frame.pop();
                            }
                            if !frame.is_empty() {
                                reader.receive(&String::from_utf8_lossy(&frame));
                            }
                        }
                    },
                    () = async { let _ = stopped.wait_for(|closed| *closed).await; } => break,
                }
            }
            reader.fail("CDP pipe closed");
        });
        browser.open().await?;
        Ok(browser)
    }

    async fn open(&self) -> Result<()> {
        self.send("Browser.getVersion", json!({})).await?;
        self.send("Target.setDiscoverTargets", json!({ "discover": true })).await?;
        Ok(())
    }

    async fn command(&self, session: Option<&str>, method: &str, params: Value, timeout: Duration) -> Result<Value> {
        let id = self.0.next_id.fetch_add(1, Ordering::Relaxed) + 1;
        let (reply, response) = oneshot::channel();
        {
            let mut state = self.0.state();
            if state.closed {
                bail!("CDP connection closed");
            }
            if let Some(session) = session
                && !state.sessions.contains_key(session)
            {
                bail!("CDP session closed");
            }
            state.pending.insert(id, Pending { session: session.map(str::to_owned), reply });
        }
        let mut message = json!({ "id": id, "method": method, "params": params });
        if let Some(session) = session {
            message["sessionId"] = session.into();
        }
        let _ = self.0.outgoing.send(message.to_string());
        match tokio::time::timeout(timeout, response).await {
            Ok(Ok(reply)) => reply.map_err(|error| anyhow!(error)),
            Ok(Err(_)) => bail!("CDP connection closed"),
            Err(_) => {
                self.0.state().pending.remove(&id);
                bail!("Timed out waiting for CDP command {method}")
            }
        }
    }

    pub async fn send(&self, method: &str, params: Value) -> Result<Value> {
        self.command(None, method, params, COMMAND_TIMEOUT).await
    }

    /// `Target.getTargets`.
    pub async fn targets(&self) -> Result<Vec<Value>> {
        let result = self.send("Target.getTargets", json!({})).await?;
        Ok(result["targetInfos"].as_array().cloned().unwrap_or_default())
    }

    /// A flattened session on the target and the events it receives until it detaches.
    pub async fn attach(&self, target_id: &str) -> Result<(Session, mpsc::UnboundedReceiver<Event>)> {
        let result = self.send("Target.attachToTarget", json!({ "targetId": target_id, "flatten": true })).await?;
        let id = result["sessionId"].as_str().context("CDP attach returned no session")?.to_owned();
        let (events, receiver) = mpsc::unbounded_channel();
        self.0.state().sessions.insert(id.clone(), events);
        Ok((Session { browser: self.clone(), id }, receiver))
    }

    pub fn is_closed(&self) -> bool {
        self.0.state().closed
    }

    pub fn close(&self) {
        self.0.fail("CDP connection closed");
    }
}

/// The parent's ends of a `--remote-debugging-pipe` child's fds 3 (its input) and 4 (its output).
#[cfg(unix)]
pub struct Pipe {
    input: tokio::net::unix::pipe::Sender,
    output: tokio::net::unix::pipe::Receiver,
}

#[cfg(unix)]
impl Pipe {
    pub fn spawn(program: &std::path::Path, arguments: &[String]) -> std::io::Result<(tokio::process::Child, Self)> {
        use command_fds::{CommandFdExt, FdMapping};
        use std::os::fd::OwnedFd;

        let (child_input, input) = std::io::pipe()?;
        let (output, child_output) = std::io::pipe()?;
        let mut command = crate::command(program);
        command
            .args(arguments)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .fd_mappings(vec![
                FdMapping { parent_fd: OwnedFd::from(child_input), child_fd: 3 },
                FdMapping { parent_fd: OwnedFd::from(child_output), child_fd: 4 },
            ])
            .map_err(std::io::Error::other)?;
        let child = command.spawn()?;
        drop(command);
        let input = tokio::net::unix::pipe::Sender::from_owned_fd(OwnedFd::from(input))?;
        let output = tokio::net::unix::pipe::Receiver::from_owned_fd(OwnedFd::from(output))?;
        Ok((child, Self { input, output }))
    }
}

/// `validatedLoopbackCdpWebSocketUrl`, for the browser endpoint.
fn loopback_endpoint(value: Option<&str>, port: u16) -> Result<Url> {
    let value = value.context("Rejected Codex CDP target without a WebSocket URL")?;
    let endpoint = Url::parse(value)?;
    if endpoint.scheme() != "ws"
        || endpoint.host_str() != Some("127.0.0.1")
        || endpoint.port() != Some(port)
        || !endpoint.path().starts_with("/devtools/browser/")
        || !endpoint.username().is_empty()
        || endpoint.password().is_some()
    {
        bail!("Rejected unexpected Codex CDP target URL: {}", endpoint.origin().ascii_serialization());
    }
    Ok(endpoint)
}

#[derive(Clone)]
pub struct Session {
    browser: Browser,
    id: String,
}

impl Session {
    pub async fn send(&self, method: &str, params: Value) -> Result<Value> {
        self.send_for(method, params, COMMAND_TIMEOUT).await
    }

    pub async fn send_for(&self, method: &str, params: Value, timeout: Duration) -> Result<Value> {
        if method == "Target.getTargets" {
            return self.browser.command(None, method, params, timeout).await;
        }
        self.browser.command(Some(&self.id), method, params, timeout).await
    }

    pub fn is_closed(&self) -> bool {
        let state = self.browser.0.state();
        state.closed || !state.sessions.contains_key(&self.id)
    }

    /// Detaches; the session's event stream ends.
    pub fn close(&self) {
        if self.is_closed() {
            return;
        }
        self.browser.0.close_session(&self.id);
        let browser = self.browser.clone();
        let id = self.id.clone();
        tokio::spawn(async move {
            let _ = browser.send("Target.detachFromTarget", json!({ "sessionId": id })).await;
        });
    }

    pub fn same(&self, other: &Self) -> bool {
        self.id == other.id && Arc::ptr_eq(&self.browser.0, &other.browser.0)
    }
}

/// The description of a failed `Runtime.evaluate`.
pub fn exception(evaluation: &Value) -> Option<String> {
    let details = evaluation.get("exceptionDetails")?;
    Some(details["exception"]["description"].as_str().unwrap_or_default().to_owned())
}
