//! `installBroadcastHostBinding`: an isolated world in each Codex window carries requests from
//! the Broadcast frame to this process and App Server notifications from Codex to the server.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use anyhow::{Context as _, Result, anyhow, bail};
use hmac::{Hmac, Mac};
use regex::Regex;
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use std::sync::LazyLock;
use tokio::sync::{Notify, mpsc};

use crate::HostNotification;
use crate::cdp::{Event, Session};

pub const HOST_BINDING: &str = "__broadcastHostV1";
pub const HOST_REQUEST: &str = "__broadcastHostRequestV1";
pub const HOST_RESPONSE: &str = "__broadcastHostResponseV1";
pub const HOST_HEARTBEAT: &str = "__broadcastHostHeartbeatV1";
pub const NOTIFICATION_BINDING: &str = "__broadcastCodexNotificationV1";

const HOST_REQUEST_ERROR: &str = "自动认领配置暂时无法应用，请刷新后重试";
const AUTOMATION_SCHEMA_DIAGNOSTIC: &str = "AUTOMATION_SCHEMA_MISMATCH";
/// `taskConversationAppServerTimeoutMs`.
pub const APP_SERVER_TIMEOUT: Duration = Duration::from_secs(30);

/// What every Codex window's bridge shares: the Broadcast service it serves and the routing of
/// server requests to a window.
pub struct Context {
    pub http: reqwest::Client,
    pub data_dir: PathBuf,
    pub origin: String,
    pub base_url: String,
    pub page_url: String,
    pub secret: String,
    pub capability: String,
    pub startup_token: String,
    pub routes: Routes,
    pub notifications: mpsc::UnboundedSender<HostNotification>,
    /// The `broadcast` invocation automation prompts tell Codex to run.
    pub cli_command: String,
    /// The `codex` executable quota checks run.
    pub codex: String,
    pub policies: crate::policy::Policies,
    pub turns: crate::remote::Turns,
}

pub(crate) fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// `routableCodexConnections` and `remoteCodexConnections`: the windows whose App Server bridge is
/// ready, and the window each Codex host's requests stick to.
#[derive(Default)]
pub struct Routes {
    state: Mutex<RouteState>,
    ready: Notify,
}

#[derive(Default)]
struct RouteState {
    routable: Vec<Session>,
    hosts: HashMap<String, Session>,
    /// `quotaPolicyCdps`: installed windows, the most recently installed last.
    recent: Vec<Session>,
}

impl Routes {
    pub fn register(&self, session: &Session) {
        let mut state = lock(&self.state);
        if !state.routable.iter().any(|known| known.same(session)) {
            state.routable.push(session.clone());
        }
        state.recent.retain(|known| !known.same(session));
        state.recent.push(session.clone());
        drop(state);
        self.ready.notify_waiters();
    }

    pub fn unregister(&self, session: &Session) {
        let mut state = lock(&self.state);
        state.routable.retain(|known| !known.same(session));
        state.hosts.retain(|_, known| !known.same(session));
        state.recent.retain(|known| !known.same(session));
    }

    /// `currentQuotaPolicyCdp`: the most recently installed live window.
    pub fn latest(&self) -> Result<Session> {
        let mut state = lock(&self.state);
        state.recent.retain(|session| !session.is_closed());
        state
            .recent
            .last()
            .cloned()
            .ok_or_else(|| anyhow!("No live Codex renderer is available for quota automation"))
    }

    fn pick(&self, host_id: &str) -> Option<Session> {
        let mut state = lock(&self.state);
        state.routable.retain(|session| !session.is_closed());
        let RouteState {
            routable, hosts, ..
        } = &mut *state;
        if let Some(current) = hosts.get(host_id)
            && routable.iter().any(|session| session.same(current))
        {
            return Some(current.clone());
        }
        let session = routable.first()?.clone();
        hosts.insert(host_id.to_owned(), session.clone());
        Some(session)
    }

    /// `codexConnectionForHost`: waits until a window can carry the request.
    pub async fn session_for(&self, host_id: &str) -> Session {
        loop {
            let notified = self.ready.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if let Some(session) = self.pick(host_id) {
                return session;
            }
            notified.await;
        }
    }

    fn routes_to(&self, host_id: &str, session: &Session) -> bool {
        lock(&self.state)
            .hosts
            .get(host_id)
            .is_some_and(|current| current.same(session))
    }
}

/// One Codex window's bridge.
pub struct Bridge {
    pub session: Session,
    context: Arc<Context>,
    active: Mutex<Option<i64>>,
    installs: tokio::sync::Mutex<()>,
    installed: AtomicU64,
    /// Whether this window's install restored the saved quota policies.
    restored: Arc<AtomicBool>,
}

impl Bridge {
    pub fn new(session: Session, context: Arc<Context>) -> Arc<Self> {
        Arc::new(Self {
            session,
            context,
            active: Mutex::new(None),
            installs: tokio::sync::Mutex::new(()),
            installed: AtomicU64::new(0),
            restored: Arc::default(),
        })
    }

    pub fn context(&self) -> &Arc<Context> {
        &self.context
    }

    /// `install`: a fresh isolated world with both bindings and the message listener. Callers that
    /// arrive while an install runs share its result.
    pub async fn install(&self) -> Result<i64> {
        let seen = self.installed.load(Ordering::Acquire);
        let _guard = self.installs.lock().await;
        if self.installed.load(Ordering::Acquire) != seen
            && let Some(context) = *lock(&self.active)
        {
            return Ok(context);
        }
        let tree = self.session.send("Page.getFrameTree", json!({})).await?;
        let frame = tree["frameTree"]["frame"]["id"].clone();
        let world = self
            .session
            .send(
                "Page.createIsolatedWorld",
                json!({ "frameId": frame, "worldName": "broadcast-host" }),
            )
            .await?;
        let context = world["executionContextId"]
            .as_i64()
            .context("Codex returned no isolated world")?;
        *lock(&self.active) = Some(context);
        for name in [HOST_BINDING, NOTIFICATION_BINDING] {
            self.session
                .send(
                    "Runtime.addBinding",
                    json!({ "name": name, "executionContextId": context }),
                )
                .await?;
        }
        self.session.send("Runtime.evaluate", json!({ "contextId": context, "expression": self.listener(), "returnByValue": true })).await?;
        self.installed.fetch_add(1, Ordering::AcqRel);
        self.context.routes.register(&self.session);
        if !self.restored.swap(true, Ordering::AcqRel) {
            let context = Arc::clone(&self.context);
            let restored = Arc::clone(&self.restored);
            tokio::spawn(async move {
                if let Err(error) = crate::policy::restore(&context).await {
                    eprintln!("Broadcast quota policy restore failed: {error}");
                    restored.store(false, Ordering::Release);
                }
            });
        }
        Ok(context)
    }

    fn listener(&self) -> String {
        let capability = json!(self.context.capability);
        format!(
            r#"(() => {{
          const capability = {capability};
          if (globalThis.__broadcastIsolatedBridgeV1 === capability) return;
          globalThis.__broadcastIsolatedBridgeV1 = capability;
          window.addEventListener("message", (event) => {{
            const message = event.data;
            if (
              !message
              || typeof message !== "object"
            ) return;
            if (
              message.type === "mcp-notification"
              && typeof message.hostId === "string"
              && typeof message.method === "string"
            ) {{
              globalThis[{notification}](JSON.stringify({{
                hostId: message.hostId,
                method: message.method,
                params: message.params
              }}));
              return;
            }}
            if (
              event.source !== window
              || event.origin !== window.location.origin
              || message.type !== {request}
              || message.capability !== capability
            ) return;
            globalThis[{binding}](JSON.stringify(message.payload));
          }});
        }})()"#,
            notification = json!(NOTIFICATION_BINDING),
            request = json!(HOST_REQUEST),
            binding = json!(HOST_BINDING),
        )
    }

    /// `publishHeartbeat`: tells the userscript a live host serves it; a stuck window is closed.
    pub async fn heartbeat(&self) -> Result<()> {
        let publish = async {
            let context = self.install().await?;
            let expression = format!(
                "window.postMessage({{\n              type: {},\n              capability: {},\n              at: Date.now(),\n              startupToken: {}\n            }}, window.location.origin)",
                json!(HOST_HEARTBEAT),
                json!(self.context.capability),
                json!(self.context.startup_token),
            );
            self.session.send("Runtime.evaluate", json!({ "contextId": context, "expression": expression, "returnByValue": true })).await?;
            anyhow::Ok(())
        };
        match tokio::time::timeout(Duration::from_secs(3), publish).await {
            Ok(result) => result,
            Err(_) => {
                self.session.close();
                bail!("Timed out publishing the Broadcast host heartbeat")
            }
        }
    }

    /// The `Runtime.bindingCalled` handler: only the current isolated world is heard.
    pub fn binding(self: &Arc<Self>, event: &Event) {
        let context = event.params["executionContextId"].as_i64();
        if context.is_none() || context != *lock(&self.active) {
            return;
        }
        let payload = event.params["payload"].clone();
        match event.params["name"].as_str() {
            Some(NOTIFICATION_BINDING) => self.forward_notification(&payload),
            Some(HOST_BINDING) => {
                let bridge = Arc::clone(self);
                let context = context.unwrap_or_default();
                tokio::spawn(async move { bridge.handle(context, &payload).await });
            }
            _ => {}
        }
    }

    /// `forwardCodexAppServerNotification`.
    fn forward_notification(&self, payload: &Value) {
        let Some(notification) = payload
            .as_str()
            .and_then(|payload| serde_json::from_str::<Value>(payload).ok())
        else {
            return;
        };
        let (Some(host_id), Some(method)) = (
            notification["hostId"].as_str(),
            notification["method"].as_str(),
        ) else {
            return;
        };
        let params = notification.get("params").cloned().unwrap_or(Value::Null);
        self.context.turns.deliver(host_id, method, &params);
        if !self.context.routes.routes_to(host_id, &self.session) {
            return;
        }
        let _ = self.context.notifications.send(HostNotification {
            host_id: host_id.to_owned(),
            method: method.to_owned(),
            params,
        });
    }

    /// `handleHostBindingPayload`.
    async fn handle(&self, context: i64, payload: &Value) {
        let request = match parse(payload) {
            Ok(request) => request,
            Err(Rejected { id: None, .. }) => return,
            Err(Rejected {
                id: Some(id),
                diagnostic,
            }) => {
                let mut response = json!({ "id": id, "ok": false, "error": HOST_REQUEST_ERROR });
                if let Some(diagnostic) = diagnostic {
                    response["diagnosticCode"] = diagnostic.into();
                }
                self.respond(context, response).await;
                return;
            }
        };
        let id = request["id"].clone();
        let response = match self.perform(&request).await {
            Ok(result) => {
                let mut response =
                    Map::from_iter([("id".to_owned(), id), ("ok".to_owned(), true.into())]);
                response.extend(result);
                Value::Object(response)
            }
            Err(error) => json!({ "id": id, "ok": false, "error": error.to_string() }),
        };
        self.respond(context, response).await;
    }

    async fn perform(&self, request: &Value) -> Result<Map<String, Value>> {
        let result = match request["action"].as_str().unwrap_or_default() {
            "ensure" => json!({ "status": "ok", "restarted": false }),
            "read-current-user" => self.read_current_user().await?,
            "load-frame" => {
                self.load_frame(
                    str_field(request, "frameName"),
                    str_field(request, "frameCapability"),
                )
                .await?
            }
            "open-external" => {
                open(str_field(request, "url")).await?;
                json!({ "opened": true })
            }
            "open-attachment" => self.open_attachment(request).await?,
            "automation" => {
                let request =
                    crate::automation::parse(request).ok_or_else(|| anyhow!(HOST_REQUEST_ERROR))?;
                return crate::policy::run_automation(&self.context, &self.session, request).await;
            }
            _ => bail!(HOST_REQUEST_ERROR),
        };
        Ok(result.as_object().cloned().unwrap_or_default())
    }

    /// `sendHostResponse`.
    async fn respond(&self, context: i64, response: Value) {
        let expression = format!(
            "window.postMessage({{\n      type: {},\n      capability: {},\n      response: {response}\n    }}, window.location.origin)",
            json!(HOST_RESPONSE),
            json!(self.context.capability),
        );
        let _ = self
            .session
            .send(
                "Runtime.evaluate",
                json!({ "expression": expression, "contextId": context, "returnByValue": true }),
            )
            .await;
    }

    /// `readCurrentUser`: a stable id from the signed-in ChatGPT account's email.
    async fn read_current_user(&self) -> Result<Value> {
        let mut account = app_server_request(
            &self.session,
            "local",
            "account/read",
            json!({ "refreshToken": false }),
            APP_SERVER_TIMEOUT,
        )
        .await?;
        let email_missing = account["account"]["email"]
            .as_str()
            .is_none_or(|email| email.trim().is_empty());
        if account["account"]["type"] == "chatgpt" && email_missing {
            account = app_server_request(
                &self.session,
                "local",
                "account/read",
                json!({ "refreshToken": true }),
                APP_SERVER_TIMEOUT,
            )
            .await?;
        }
        Ok(json!({ "userId": stable_user_id(&account) }))
    }

    /// `loadBroadcastFrameViaCdp`: fill the sandboxed frame with the verified Broadcast document.
    async fn load_frame(&self, name: &str, capability: &str) -> Result<Value> {
        let html = self.verified_document(capability).await?;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while tokio::time::Instant::now() < deadline {
            let tree = self.session.send("Page.getFrameTree", json!({})).await?;
            if let Some(frame) = frame_named(&tree["frameTree"], name) {
                self.session
                    .send(
                        "Page.setDocumentContent",
                        json!({ "frameId": frame["id"], "html": html }),
                    )
                    .await?;
                return Ok(json!({ "loaded": true }));
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        bail!("Timed out waiting for the isolated Broadcast frame")
    }

    /// `verifiedBroadcastDocument`: the page, only from the service holding this instance's secret.
    async fn verified_document(&self, capability: &str) -> Result<String> {
        let challenge = format!(
            "{}{}",
            uuid::Uuid::new_v4().simple(),
            uuid::Uuid::new_v4().simple()
        );
        let response = self
            .context
            .http
            .get(&self.context.page_url)
            .header("origin", "app://-")
            .header("x-broadcast-challenge", &challenge)
            .send()
            .await?;
        if !response.status().is_success() {
            bail!("Broadcast HTTP {}", response.status().as_u16());
        }
        let proof = response
            .headers()
            .get("x-broadcast-proof")
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default()
            .to_owned();
        if proof != proof_of(&self.context.secret, &challenge) {
            bail!("Broadcast service identity check failed");
        }
        let html = response.text().await?;
        if !html.contains("<head>") {
            bail!("Broadcast document has no head element");
        }
        let head = format!(
            "<head><base href={}><script>globalThis.__BROADCAST_FRAME_CAPABILITY__={};</script>",
            json!(self.context.page_url),
            json!(capability),
        );
        Ok(html.replacen("<head>", &head, 1))
    }

    /// `openAttachment`: a device-local copy under the data directory, revealed or returned.
    async fn open_attachment(&self, request: &Value) -> Result<Value> {
        let attachment = str_field(request, "attachmentId");
        let directory = self
            .context
            .data_dir
            .join("opened-attachments")
            .join(attachment);
        let path = directory.join(str_field(request, "filename"));
        let local_path = path.to_string_lossy().into_owned();
        let operation = request["operation"].as_str();
        if let Some(operation) = operation {
            let local_copy = match tokio::fs::metadata(&path).await {
                Ok(metadata) => metadata.is_file(),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
                Err(error) => return Err(error.into()),
            };
            if local_copy {
                if operation == "reveal" {
                    reveal(&path, &directory).await?;
                }
                return Ok(json!({ "localPath": local_path, "opened": operation == "reveal" }));
            }
            if operation == "reveal" {
                bail!("No device-local attachment copy is available");
            }
            let session = self
                .context
                .http
                .get(format!("{}/api/local/cloud-session", self.context.base_url))
                .send()
                .await?;
            if !session.status().is_success() || session.json::<Value>().await?["mode"] != "local" {
                return Ok(json!({ "localPath": null }));
            }
        }
        let url = format!(
            "{}/api/attachments/{}/content",
            self.context.base_url,
            encode_component(attachment)
        );
        let response = self.context.http.get(url).send().await?;
        if !response.status().is_success() {
            bail!(
                "Attachment content returned HTTP {}",
                response.status().as_u16()
            );
        }
        let bytes = response.bytes().await?;
        write_private(&directory, &path, &bytes).await?;
        if operation == Some("local-path") {
            return Ok(json!({ "localPath": local_path }));
        }
        reveal(&path, &directory).await?;
        Ok(json!({ "opened": true, "localPath": local_path }))
    }
}

fn str_field<'a>(request: &'a Value, name: &str) -> &'a str {
    request[name].as_str().unwrap_or_default()
}

fn frame_named<'a>(tree: &'a Value, name: &str) -> Option<&'a Value> {
    if tree["frame"]["name"] == name {
        return Some(&tree["frame"]);
    }
    tree["childFrames"]
        .as_array()?
        .iter()
        .find_map(|child| frame_named(child, name))
}

/// The HMAC-SHA256 proof the server answers a challenge with.
pub fn proof_of(secret: &str, challenge: &str) -> String {
    let mut mac =
        Hmac::<Sha256>::new_from_slice(secret.as_bytes()).expect("HMAC accepts any key length");
    mac.update(challenge.as_bytes());
    mac.finalize()
        .into_bytes()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// `stableCodexUserId`.
fn stable_user_id(account: &Value) -> String {
    let email = if account["account"]["type"] == "chatgpt" {
        account["account"]["email"].as_str().unwrap_or_default()
    } else {
        ""
    };
    let email = email.trim().to_lowercase();
    if email.is_empty() {
        return String::new();
    }
    let digest = Sha256::new()
        .chain_update("broadcast-user\0")
        .chain_update(email)
        .finalize();
    format!(
        "codex-user-{}",
        digest
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>()
    )
}

/// `encodeURIComponent`.
pub fn encode_component(value: &str) -> String {
    const KEEP: &[u8] = b"-_.!~*'()";
    value
        .bytes()
        .map(|byte| {
            if byte.is_ascii_alphanumeric() || KEEP.contains(&byte) {
                (byte as char).to_string()
            } else {
                format!("%{byte:02X}")
            }
        })
        .collect()
}

#[cfg(unix)]
pub(crate) async fn write_private(
    directory: &std::path::Path,
    path: &std::path::Path,
    bytes: &[u8],
) -> Result<()> {
    use tokio::io::AsyncWriteExt;

    tokio::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(directory)
        .await?;
    let mut file = tokio::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)
        .await?;
    file.write_all(bytes).await?;
    Ok(())
}

#[cfg(not(unix))]
pub(crate) async fn write_private(
    directory: &std::path::Path,
    path: &std::path::Path,
    bytes: &[u8],
) -> Result<()> {
    tokio::fs::create_dir_all(directory).await?;
    tokio::fs::write(path, bytes).await?;
    Ok(())
}

/// `openWithDefaultApplication`: hand a URL or path to the desktop, without waiting.
pub async fn open(target: impl AsRef<std::ffi::OsStr>) -> Result<()> {
    let program = if cfg!(windows) {
        "explorer.exe"
    } else if cfg!(target_os = "linux") {
        "xdg-open"
    } else {
        "/usr/bin/open"
    };
    let mut child = crate::command(program)
        .arg(target)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()?;
    tokio::spawn(async move {
        let _ = child.wait().await;
    });
    Ok(())
}

/// `revealAttachmentInFinder`.
async fn reveal(path: &std::path::Path, directory: &std::path::Path) -> Result<()> {
    if cfg!(target_os = "linux") {
        return open(directory).await;
    }
    let revealed = crate::command("/usr/bin/open")
        .arg("-R")
        .arg(path)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .await
        .is_ok_and(|status| status.success());
    if revealed {
        Ok(())
    } else {
        open(directory).await
    }
}

static SEQUENCE: AtomicU64 = AtomicU64::new(0);

pub(crate) fn request_id(kind: &str) -> String {
    let sequence = SEQUENCE.fetch_add(1, Ordering::Relaxed) + 1;
    let now = chrono_millis();
    format!(
        "{kind}-{}-{}-{}",
        std::process::id(),
        radix36(now),
        radix36(sequence)
    )
}

pub(crate) fn chrono_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| {
            u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX)
        })
}

fn radix36(mut value: u64) -> String {
    const DIGITS: &[u8] = b"0123456789abcdefghijklmnopqrstuvwxyz";
    let mut digits = Vec::new();
    loop {
        digits.push(DIGITS[usize::try_from(value % 36).unwrap_or_default()]);
        value /= 36;
        if value == 0 {
            break;
        }
    }
    digits.reverse();
    String::from_utf8(digits).unwrap_or_default()
}

/// `requestCodexAppServerViaCdp`: an App Server request through the renderer's Electron bridge.
pub async fn app_server_request(
    session: &Session,
    host_id: &str,
    method: &str,
    params: Value,
    timeout: Duration,
) -> Result<Value> {
    let request_id = json!(request_id("broadcast-thread"));
    let host_id = json!(host_id);
    let timeout_ms = json!(u64::try_from(timeout.as_millis()).unwrap_or(u64::MAX));
    let expression = format!(
        r#"(() => new Promise((resolve) => {{
      const requestId = {request_id};
      const bridge = window.electronBridge;
      if (!bridge || typeof bridge.sendMessageFromView !== "function") {{
        resolve({{ ok: false, error: "Codex App Server bridge is unavailable" }});
        return;
      }}
      let settled = false;
      const finish = (result) => {{
        if (settled) return;
        settled = true;
        window.clearTimeout(timeout);
        window.removeEventListener("message", onMessage, true);
        resolve(result);
      }};
      const onMessage = (event) => {{
        const message = event.data;
        if (
          !message
          || typeof message !== "object"
          || message.type !== "mcp-response"
          || message.hostId !== {host_id}
          || message.message?.id !== requestId
        ) return;
        event.stopImmediatePropagation();
        if (message.message.error) {{
          finish({{
            ok: false,
            error: message.message.error.message || "Codex App Server request failed",
          }});
          return;
        }}
        finish({{ ok: true, result: message.message.result }});
      }};
      const timeout = window.setTimeout(
        () => finish({{ ok: false, error: "Codex App Server request timed out" }}),
        {timeout_ms},
      );
      window.addEventListener("message", onMessage, true);
      Promise.resolve(bridge.sendMessageFromView({{
        type: "mcp-request",
        hostId: {host_id},
        request: {{
          id: requestId,
          method: {method},
          params: {params},
        }},
        priority: "interactive",
        source: "broadcast_thread_create",
        timeoutMs: {timeout_ms},
        expiresAtMs: Date.now() + {timeout_ms},
      }})).catch((error) => {{
        finish({{
          ok: false,
          error: error instanceof Error ? error.message : String(error),
        }});
      }});
    }}))()"#,
        method = json!(method),
    );
    let evaluation = session
        .send_for(
            "Runtime.evaluate",
            json!({ "expression": expression, "awaitPromise": true, "returnByValue": true }),
            timeout + Duration::from_secs(5),
        )
        .await?;
    if let Some(description) = crate::cdp::exception(&evaluation) {
        bail!(if description.is_empty() {
            "Codex App Server request failed".to_owned()
        } else {
            description
        });
    }
    let response = &evaluation["result"]["value"];
    if response["ok"] != true {
        let error = response["error"]
            .as_str()
            .filter(|error| !error.is_empty())
            .unwrap_or("Codex App Server request failed");
        return Err(anyhow!(error.to_owned()));
    }
    Ok(response.get("result").cloned().unwrap_or(Value::Null))
}

struct Rejected {
    id: Option<String>,
    diagnostic: Option<&'static str>,
}

static REQUEST_ID: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)^[a-z0-9-]{1,80}$").expect("valid regex"));
static FRAME_NAME: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)^broadcast-[a-f0-9-]{36,80}$").expect("valid regex"));
static FRAME_CAPABILITY: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)^[a-f0-9-]{36,80}$").expect("valid regex"));
static ATTACHMENT_ID: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)^[a-f0-9-]{36}$").expect("valid regex"));

/// JavaScript's `string.length`.
fn js_length(value: &str) -> usize {
    value.encode_utf16().count()
}

fn has_control(value: &str) -> bool {
    value
        .chars()
        .any(|character| matches!(character, '\u{0}'..='\u{1f}' | '\u{7f}'))
}

/// Non-empty, at most `max` UTF-16 units and free of control characters.
pub(crate) fn bounded_text(value: &str, max: usize) -> bool {
    !value.is_empty() && js_length(value) <= max && !has_control(value)
}

fn bounded(value: &Value, max: usize) -> Option<&str> {
    value
        .as_str()
        .filter(|value| !value.is_empty() && js_length(value) <= max)
}

/// `parseHostRequest`: the frame's request, or why it gets no or an error response.
fn parse(payload: &Value) -> Result<Value, Rejected> {
    let rejected = |id: Option<&str>| Rejected {
        id: id.map(str::to_owned),
        diagnostic: None,
    };
    let payload = payload
        .as_str()
        .filter(|payload| js_length(payload) <= 4_194_304)
        .ok_or_else(|| rejected(None))?;
    let mut request: Value = serde_json::from_str(payload).map_err(|_| rejected(None))?;
    let id = request["id"]
        .as_str()
        .filter(|id| REQUEST_ID.is_match(id))
        .map(str::to_owned)
        .ok_or_else(|| rejected(None))?;
    let action = request["action"].as_str().unwrap_or_default();
    let accepted = match action {
        "ensure" | "read-current-user" => true,
        "load-frame" => {
            request["frameName"]
                .as_str()
                .is_some_and(|name| FRAME_NAME.is_match(name))
                && request["frameCapability"]
                    .as_str()
                    .is_some_and(|capability| FRAME_CAPABILITY.is_match(capability))
        }
        "open-external" => {
            let href = request["url"]
                .as_str()
                .and_then(|url| url::Url::parse(url).ok())
                .filter(|url| matches!(url.scheme(), "http" | "https"))
                .map(String::from)
                .filter(|href| js_length(href) <= 2_048);
            if let Some(href) = &href {
                request["url"] = href.as_str().into();
            }
            href.is_some()
        }
        "open-attachment" => {
            let operation = request.get("operation");
            let filename = bounded(&request["filename"], 240);
            operation.is_none_or(|operation| operation == "local-path" || operation == "reveal")
                && request["attachmentId"]
                    .as_str()
                    .is_some_and(|id| ATTACHMENT_ID.is_match(id))
                && filename.is_some_and(|name| {
                    name != "." && name != ".." && !has_control(name) && !name.contains(['/', '\\'])
                })
        }
        "automation" => {
            if crate::automation::parse(&request).is_none() {
                return Err(Rejected {
                    id: Some(id),
                    diagnostic: Some(AUTOMATION_SCHEMA_DIAGNOSTIC),
                });
            }
            true
        }
        _ => false,
    };
    if accepted {
        Ok(request)
    } else {
        Err(rejected(Some(&id)))
    }
}
