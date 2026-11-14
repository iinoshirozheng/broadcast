//! Local AI chat (`server/ai-chat.mjs`): conversations with Codex on this device or on a remote
//! Codex host, the turns they run and the events those turns stream to the board.
//!
//! A turn on this device runs `codex exec` under a turn owner; a composer turn, or any turn on a
//! remote host, runs through a Codex app-server. Either way the run is recorded in the store and
//! its events are fanned out to the thread's event streams.

pub mod agents;
pub mod app_server;
pub mod catalog;
pub mod composer;
pub mod context;
pub mod exec;
pub mod summary;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::ExitStatus;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use base64::Engine as _;
use broadcast_domain::records::{AiChatOrigin, AiChatRun, AiChatThread};
use broadcast_domain::{ApiError, Error, Result, js};
use broadcast_store::{AiChatRunOutcome, AiChatThreadChanges, NewAiChatEvent, NewAiChatThread};
use serde_json::{Map, Value, json};
use tokio::io::AsyncWriteExt;
use tokio::sync::{broadcast, mpsc, watch};

use crate::App;
use agents::Agent;
use app_server::{AppServer, HostAppServer, LocalAppServer, Notification, Origin};
use catalog::{Catalog, Model};
use composer::{ComposerCatalog, DocumentNode, RebindNode, Reference};
use context::{RemoteTarget, Resolved};
use exec::{CodexEvent, EventRecord, Line, SkillLink, TurnProcess, capped_str};

/// How long a signalled turn gets to finish before it is killed or abandoned.
const KILL_GRACE: Duration = Duration::from_secs(1);
const INTERRUPT_WAIT: Duration = Duration::from_millis(1_025);
const AGENT_DISPATCH_PROTOCOL: &str = "broadcast.agent.v1";
const IMAGE_TYPES: [&str; 4] = ["image/gif", "image/jpeg", "image/png", "image/webp"];

/// A file sent with a turn, already decoded.
pub struct Attachment {
    pub filename: String,
    pub content_type: String,
    pub data: Vec<u8>,
}

impl Attachment {
    fn summary(&self) -> Value {
        json!({ "filename": self.filename, "contentType": self.content_type, "size": self.data.len() })
    }

    fn is_image(&self) -> bool {
        IMAGE_TYPES.contains(&self.content_type.as_str())
    }

    /// `remoteAttachmentInput`: a remote host cannot read this device's files, so it gets a data URL.
    fn remote_input(&self) -> Value {
        let encoded = base64::engine::general_purpose::STANDARD.encode(&self.data);
        let url = format!("data:{};base64,{encoded}", self.content_type);
        if self.is_image() {
            json!({ "type": "image", "url": url })
        } else if self.content_type.starts_with("audio/") {
            json!({ "type": "audio", "url": url })
        } else {
            json!({ "type": "text", "text": format!("\n\nAttached file {}: {url}", self.filename) })
        }
    }
}

pub struct NewThread {
    pub project_id: String,
    pub issue_id: Option<String>,
    pub title: Option<String>,
    pub model: Option<String>,
    pub reasoning_effort: Option<String>,
    pub sandbox: Option<String>,
    pub remote: Option<RemoteTarget>,
}

pub struct ThreadPatch {
    pub title: Option<String>,
    pub model: Option<String>,
    pub reasoning_effort: Option<String>,
    pub sandbox: Option<String>,
}

pub enum TurnBody {
    /// A plain message whose Skill markers name `skill_ids` in order.
    Message { message: String, skill_ids: Vec<String> },
    /// A composer document resolved against the catalog `revision`.
    Composer { revision: String, nodes: Vec<DocumentNode> },
}

pub struct Turn {
    pub confirmed: bool,
    pub attachments: Vec<Attachment>,
    pub body: TurnBody,
}

pub struct CandidateQuery {
    pub project_id: Option<String>,
    pub thread_id: Option<String>,
    pub trigger: String,
    pub query: String,
    pub remote: Option<RemoteTarget>,
}

/// An app-server and the composer catalog built on it.
#[derive(Clone)]
struct Runtime {
    server: AppServer,
    composer: Arc<ComposerCatalog>,
}

enum Kind {
    Exec { pid: Option<u32> },
    AppServer { server: AppServer, origin: Option<Origin>, codex_thread_id: String, turn_id: Mutex<Option<String>> },
}

/// A run in flight. `done` flips once the run is recorded as finished.
struct Active {
    run: AiChatRun,
    thread_id: String,
    interrupted: AtomicBool,
    temporary: Option<PathBuf>,
    done: watch::Sender<bool>,
    kind: Kind,
}

impl Active {
    fn new(run: &AiChatRun, temporary: Option<PathBuf>, kind: Kind) -> Arc<Self> {
        Arc::new(Self {
            run: run.clone(),
            thread_id: run.thread_id.clone(),
            interrupted: AtomicBool::new(false),
            temporary,
            done: watch::channel(false).0,
            kind,
        })
    }

    fn interrupted(&self) -> bool {
        self.interrupted.load(Ordering::SeqCst)
    }

    async fn wait(&self) {
        let mut done = self.done.subscribe();
        let _ = done.wait_for(|done| *done).await;
    }

    async fn settled(&self, within: Duration) {
        let _ = tokio::time::timeout(within, self.wait()).await;
    }
}

/// What an exec turn reported before its owner exited.
#[derive(Default)]
struct ExecState {
    started: Option<String>,
    terminal: Option<&'static str>,
    terminal_error: String,
    pending_error: String,
}

/// Files written for a local turn.
#[derive(Default)]
struct Written {
    directory: Option<PathBuf>,
    paths: Vec<String>,
    images: Vec<String>,
}

/// One thread's stream message: `None` ends every stream.
pub type StreamMessage = Option<(String, Value)>;

pub struct AiChat {
    executable: String,
    local: Runtime,
    local_server: Arc<LocalAppServer>,
    remote: Mutex<HashMap<String, Runtime>>,
    active: Mutex<HashMap<String, Arc<Active>>>,
    listeners: broadcast::Sender<StreamMessage>,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

fn internal(message: &str) -> Error {
    Error::Internal(anyhow::anyhow!(message.to_owned()))
}

fn busy(thread_id: &str) -> Error {
    ApiError::new(409, "THREAD_BUSY", format!("AI chat thread '{thread_id}' has a running turn")).into()
}

fn workspace_changed() -> Error {
    ApiError::new(409, "PROJECT_WORKSPACE_CHANGED", "The project's device workspace no longer matches this conversation").into()
}

fn invalid_query(message: &str) -> Error {
    ApiError::new(400, "INVALID_COMPOSER_QUERY", message.to_owned()).into()
}

fn confirm_danger(thread: &AiChatThread, confirmed: bool) -> Result<()> {
    if thread.sandbox == "danger-full-access" && !confirmed {
        return Err(ApiError::new(400, "DANGER_CONFIRMATION_REQUIRED", "danger-full-access must be confirmed for every turn").into());
    }
    Ok(())
}

/// `codexTargetFromOrigin`: the remote Codex project a thread was started on.
fn target_of(origin: &AiChatOrigin) -> Option<RemoteTarget> {
    let present = |value: &Option<String>| value.clone().filter(|value| !value.is_empty());
    if !origin.remote() || origin.workspace_path.is_empty() {
        return None;
    }
    Some(RemoteTarget {
        codex_project_id: present(&origin.codex_project_id)?,
        codex_host_id: present(&origin.codex_host_id)?,
        workspace_path: origin.workspace_path.clone(),
    })
}

fn resolve_model<'a>(catalog: &'a Catalog, requested: Option<&str>) -> Result<&'a Model> {
    let model = match requested {
        None => catalog.models.first(),
        Some(slug) => catalog.models.iter().find(|model| model.slug == slug),
    };
    model.ok_or_else(|| {
        let message = match requested {
            None => "Codex did not provide an available model".to_owned(),
            Some(slug) => format!("Unknown model '{slug}'"),
        };
        ApiError::new(400, "INVALID_MODEL", message).into()
    })
}

fn validate_effort(model: &Model, effort: &str) -> Result<()> {
    if !model.supported_reasoning_efforts.iter().any(|supported| supported == effort) {
        let message = format!("Reasoning effort '{effort}' is not supported by model '{}'", model.slug);
        return Err(ApiError::new(400, "INVALID_REASONING_EFFORT", message).into());
    }
    Ok(())
}

/// `String(value ?? "")` for the JSON values Codex sends.
fn js_string(value: Option<&Value>) -> String {
    match value {
        None | Some(Value::Null) => String::new(),
        Some(Value::String(text)) => text.clone(),
        Some(other) => other.to_string(),
    }
}

/// `cappedError` with the generic fallback for an empty message.
fn failure(message: &str, fallback: &str) -> String {
    let capped = capped_str(message);
    if capped.is_empty() { fallback.to_owned() } else { capped }
}

fn agent_dispatch_text(agent: &Agent) -> String {
    format!(
        "Broadcast private agent dispatch ({AGENT_DISPATCH_PROTOCOL}):\nUse the configured Broadcast agent {} (id {}) for this request.\nThis is Broadcast product-private routing context, not a Codex App Server UserInput type.",
        json!(agent.name),
        json!(agent.stable_id),
    )
}

fn document_node(node: &DocumentNode) -> Value {
    match node {
        DocumentNode::Text(text) => json!({ "type": "text", "text": text }),
        DocumentNode::Reference { kind, candidate_ref, label } => json!({ "type": kind, "candidateRef": candidate_ref, "label": label }),
        DocumentNode::Other(kind) => json!({ "type": kind }),
    }
}

/// `normalizedAppServerItem`: a completed app-server item as an event, or nothing to record.
fn app_server_item(item: Option<&Value>) -> Option<EventRecord> {
    let item = item?.as_object()?;
    let text = |key: &str| capped_str(&js_string(item.get(key)));
    let mut data = Map::new();
    data.insert("status".into(), item.get("status").filter(|status| !status.is_null()).cloned().unwrap_or(json!("completed")));
    if let Some(id) = item.get("id").and_then(Value::as_str).map(capped_str).filter(|id| !id.is_empty()) {
        data.insert("itemId".into(), json!(id));
    }
    let record = |kind: &str, role: &'static str, content: String, data: Map<String, Value>| EventRecord {
        kind: kind.to_owned(),
        role,
        content,
        data: Value::Object(data),
    };
    match item.get("type").and_then(Value::as_str)? {
        "agentMessage" => Some(record("agent_message", "assistant", text("text"), data)),
        "commandExecution" => {
            data.insert("command".into(), json!(text("command")));
            if let Some(output) = item.get("aggregatedOutput").and_then(Value::as_str) {
                data.insert("output".into(), json!(capped_str(output)));
            }
            if let Some(code) = item.get("exitCode").filter(|code| code.as_f64().is_some_and(|code| code.fract() == 0.0)) {
                data.insert("exitCode".into(), code.clone());
            }
            Some(record("command_execution", "activity", text("command"), data))
        }
        "fileChange" => {
            let changes = item.get("changes").and_then(Value::as_array).map(Vec::as_slice).unwrap_or_default();
            let files: Vec<String> = changes.iter().filter_map(|change| change.get("path")?.as_str().map(capped_str)).collect();
            let content = capped_str(&files.join("\n"));
            data.insert("files".into(), json!(files));
            Some(record("file_change", "activity", content, data))
        }
        "mcpToolCall" => {
            let joined = format!("{}.{}", js_string(item.get("server")), js_string(item.get("tool")));
            let content = joined.strip_prefix('.').unwrap_or(&joined);
            let content = content.strip_suffix('.').unwrap_or(content).to_owned();
            for key in ["server", "tool"] {
                if let Some(value) = item.get(key).filter(|value| value.is_string()) {
                    data.insert(key.into(), value.clone());
                }
            }
            let role = if item.get("error").is_some_and(js::truthy) { "error" } else { "activity" };
            Some(record("mcp_tool_call", role, content, data))
        }
        _ => None,
    }
}

async fn remove_dir(path: &Path) {
    let _ = tokio::fs::remove_dir_all(path).await;
}

async fn write_file(path: &Path, data: &[u8]) -> std::io::Result<()> {
    let mut options = tokio::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    options.mode(0o600);
    let mut file = options.open(path).await?;
    file.write_all(data).await?;
    file.flush().await
}

/// `writeTurnAttachments`: each file in a private temporary directory Codex can read.
async fn write_attachments(attachments: &[Attachment]) -> Result<Written> {
    if attachments.is_empty() {
        return Ok(Written::default());
    }
    let directory = tempfile::Builder::new().prefix("broadcast-ai-turn-").tempdir()?.keep();
    let mut written = Written { directory: Some(directory.clone()), ..Written::default() };
    for (index, attachment) in attachments.iter().enumerate() {
        let path = directory.join(format!("attachment-{}-{}", index + 1, attachment.filename));
        if let Err(error) = write_file(&path, &attachment.data).await {
            remove_dir(&directory).await;
            return Err(error.into());
        }
        let path = path.to_string_lossy().into_owned();
        if attachment.is_image() {
            written.images.push(path.clone());
        }
        written.paths.push(path);
    }
    Ok(written)
}

async fn get_thread(app: &Arc<App>, id: &str) -> Result<AiChatThread> {
    let key = id.to_owned();
    app.store(move |store| store.get_ai_chat_thread(&key))
        .await?
        .ok_or_else(|| ApiError::new(404, "AI_CHAT_THREAD_NOT_FOUND", format!("AI chat thread '{id}' does not exist")).into())
}

async fn get_run(app: &Arc<App>, id: &str) -> Result<AiChatRun> {
    let key = id.to_owned();
    app.store(move |store| store.get_ai_chat_run(&key))
        .await?
        .ok_or_else(|| ApiError::new(404, "AI_CHAT_RUN_NOT_FOUND", format!("AI chat run '{id}' does not exist")).into())
}

fn exit_failure(status: &ExitStatus) -> Option<String> {
    match status.code() {
        Some(0) => None,
        Some(code) => Some(format!("Codex exited with code {code}")),
        None => Some(format!("Codex exited due to signal {}", app_server::exit_label(status))),
    }
}

impl AiChat {
    /// The service and the receiver for its app-server notifications, which [`AiChat::consume`] drains.
    pub fn new(executable: String, codex_home: PathBuf) -> (Self, mpsc::UnboundedReceiver<Notification>) {
        let (notifications, receiver) = mpsc::unbounded_channel();
        let local_server = Arc::new(LocalAppServer::new(executable.clone(), notifications));
        let server = AppServer::Local(Arc::clone(&local_server));
        let local = Runtime { composer: Arc::new(ComposerCatalog::new(server.clone(), Some(codex_home))), server };
        let (listeners, _) = broadcast::channel(1024);
        let chat = Self { executable, local, local_server, remote: Mutex::default(), active: Mutex::default(), listeners };
        (chat, receiver)
    }

    pub fn subscribe(&self) -> broadcast::Receiver<StreamMessage> {
        self.listeners.subscribe()
    }

    /// Ends every open thread event stream (server shutdown).
    pub fn end_streams(&self) {
        let _ = self.listeners.send(None);
    }

    fn emit(&self, thread_id: &str, value: Value) {
        let _ = self.listeners.send(Some((thread_id.to_owned(), value)));
    }

    /// `runtimeForTarget`: this device's app-server, or one per remote host (without local agents).
    fn runtime(&self, remote: Option<&RemoteTarget>) -> Runtime {
        let Some(target) = remote else { return self.local.clone() };
        lock(&self.remote)
            .entry(target.codex_host_id.clone())
            .or_insert_with(|| {
                let server = AppServer::Host(Arc::new(HostAppServer { host_id: target.codex_host_id.clone() }));
                Runtime { composer: Arc::new(ComposerCatalog::new(server.clone(), None)), server }
            })
            .clone()
    }

    fn composer_for(&self, origin: &Origin) -> Option<Arc<ComposerCatalog>> {
        match origin {
            Origin::Local(_) => Some(Arc::clone(&self.local.composer)),
            Origin::Host(host_id) => lock(&self.remote).get(host_id).map(|runtime| Arc::clone(&runtime.composer)),
        }
    }

    fn is_active(&self, thread: &AiChatThread) -> bool {
        thread.current_run.is_some() || lock(&self.active).values().any(|active| active.thread_id == thread.id)
    }

    async fn record(&self, app: &Arc<App>, thread_id: &str, run_id: &str, event: EventRecord) -> Result<()> {
        let (thread, run) = (thread_id.to_owned(), run_id.to_owned());
        let stored = app
            .store(move |store| {
                store.insert_ai_chat_event(NewAiChatEvent {
                    thread_id: &thread,
                    run_id: Some(&run),
                    kind: &event.kind,
                    role: event.role,
                    content: &event.content,
                    data: Some(&event.data).filter(|data| !data.is_null()),
                })
            })
            .await?;
        self.emit(thread_id, json!({ "type": "ai.event", "event": stored }));
        Ok(())
    }

    async fn create_run(&self, app: &Arc<App>, thread_id: &str) -> Result<AiChatRun> {
        let id = thread_id.to_owned();
        let run = app.store(move |store| store.create_ai_chat_run(&id)).await?;
        self.emit(thread_id, json!({ "type": "ai.run", "run": run }));
        Ok(run)
    }

    async fn catalog_for(&self, resolved: &Resolved) -> Result<Catalog> {
        match &resolved.remote {
            Some(target) => catalog::discover_app_server(&self.runtime(Some(target)).server, &resolved.workspace_path).await,
            None => catalog::discover(&self.executable, &resolved.workspace_path).await,
        }
        .map_err(Error::Internal)
    }

    /// `getCatalog`: the models, Skills and commands available where the project runs.
    pub async fn catalog(&self, app: &Arc<App>, project_id: &str, remote: Option<&RemoteTarget>) -> Result<Catalog> {
        let resolved = context::resolve(app, project_id, None, remote).await?;
        self.catalog_for(&resolved).await
    }

    /// `getComposerCandidates`: `/` and `@` candidates for a project, optionally within a thread.
    pub async fn candidates(&self, app: &Arc<App>, query: CandidateQuery) -> Result<Value> {
        let mut project_id = query.project_id;
        let mut remote = query.remote;
        let mut thread = None;
        if let Some(thread_id) = &query.thread_id {
            let found = match get_thread(app, thread_id).await {
                Err(Error::Api(error)) if error.code == "AI_CHAT_THREAD_NOT_FOUND" => {
                    return Err(invalid_query("Composer thread does not exist"));
                }
                other => other?,
            };
            if project_id.as_ref().is_some_and(|id| *id != found.origin.project_id) {
                return Err(invalid_query("Composer thread does not belong to the selected project"));
            }
            project_id = Some(found.origin.project_id.clone());
            remote = target_of(&found.origin);
            thread = Some(found);
        }
        let without_compact = |mut response: Value| {
            if let Some(candidates) = response.get_mut("candidates").and_then(Value::as_array_mut) {
                candidates.retain(|candidate| {
                    candidate.pointer("/dispatch/handlerId").and_then(Value::as_str) != Some(composer::COMPACT_HANDLER)
                });
            }
            response
        };
        let Some(project_id) = project_id else {
            let response = self.local.composer.candidates(None, &query.trigger, &query.query).await?;
            return Ok(without_compact(response));
        };
        let issue_id = thread.as_ref().and_then(|thread| thread.origin.issue_id.clone());
        let resolved = match context::resolve(app, &project_id, issue_id.as_deref(), remote.as_ref()).await {
            Err(Error::Api(error)) if error.code == "PROJECT_NOT_FOUND" || error.code == "AI_CHAT_ISSUE_NOT_FOUND" => {
                return Err(invalid_query("Composer project is invalid"));
            }
            other => other?,
        };
        if thread.as_ref().is_some_and(|thread| thread.origin.workspace_path != resolved.workspace_path) {
            return Err(invalid_query("Composer thread workspace no longer matches the selected project"));
        }
        let composer = self.runtime(resolved.remote.as_ref()).composer;
        let response = composer.candidates(Some(&resolved.workspace_path), &query.trigger, &query.query).await?;
        let compactable = thread.is_some_and(|thread| {
            thread.codex_thread_id.is_some_and(|id| !id.is_empty()) && thread.status != "running"
        });
        Ok(if compactable { response } else { without_compact(response) })
    }

    /// `resolveComposerRebindWorkspace` + `rebind`: re-resolves persisted references for a document.
    pub async fn rebind(&self, app: &Arc<App>, project_id: &str, thread_id: Option<&str>, nodes: Vec<RebindNode>) -> Result<Value> {
        let (workspace, composer) = match thread_id {
            Some(thread_id) => {
                let thread = match get_thread(app, thread_id).await {
                    Err(Error::Api(error)) if error.code == "AI_CHAT_THREAD_NOT_FOUND" => {
                        return Err(invalid_query("Composer thread does not exist"));
                    }
                    other => other?,
                };
                if thread.origin.project_id != project_id {
                    return Err(invalid_query("Composer thread does not belong to the selected project"));
                }
                if !thread.origin.remote() {
                    let directory = tokio::fs::metadata(&thread.origin.workspace_path).await.is_ok_and(|meta| meta.is_dir());
                    if !directory {
                        let message = "The conversation workspace is not available on this device";
                        return Err(ApiError::new(409, "PROJECT_WORKSPACE_UNAVAILABLE", message).into());
                    }
                }
                let composer = self.runtime(target_of(&thread.origin).as_ref()).composer;
                (thread.origin.workspace_path, composer)
            }
            None => {
                let resolved = match context::resolve(app, project_id, None, None).await {
                    Err(Error::Api(error)) if error.code == "PROJECT_NOT_FOUND" || error.code == "AI_CHAT_ISSUE_NOT_FOUND" => {
                        return Err(invalid_query("Composer project is invalid"));
                    }
                    other => other?,
                };
                (resolved.workspace_path, Arc::clone(&self.local.composer))
            }
        };
        composer.rebind(&workspace, nodes).await
    }

    pub async fn threads(&self, app: &Arc<App>) -> Result<Vec<AiChatThread>> {
        app.store(|store| store.list_ai_chat_threads()).await
    }

    pub async fn thread(&self, app: &Arc<App>, id: &str) -> Result<AiChatThread> {
        get_thread(app, id).await
    }

    /// `getThreadSummary`: the thread and its runs.
    pub async fn summary(&self, app: &Arc<App>, id: &str) -> Result<Value> {
        let thread = get_thread(app, id).await?;
        let key = id.to_owned();
        let runs = app.store(move |store| store.list_ai_chat_runs(&key)).await?;
        Ok(json!({ "thread": thread, "runs": runs }))
    }

    /// `getThreadSnapshot`: the thread, its events and its runs.
    pub async fn snapshot(&self, app: &Arc<App>, id: &str) -> Result<Value> {
        let thread = get_thread(app, id).await?;
        let key = id.to_owned();
        let (events, runs) = app
            .store(move |store| Ok((store.list_ai_chat_events(&key)?, store.list_ai_chat_runs(&key)?)))
            .await?;
        Ok(json!({ "thread": thread, "events": events, "runs": runs }))
    }

    /// `createThread`: a conversation pinned to the project's workspace and a supported model.
    pub async fn create(&self, app: &Arc<App>, input: NewThread) -> Result<AiChatThread> {
        let resolved = context::resolve(app, &input.project_id, input.issue_id.as_deref(), input.remote.as_ref()).await?;
        let catalog = self.catalog_for(&resolved).await?;
        let model = resolve_model(&catalog, input.model.as_deref())?;
        let reasoning_effort = input.reasoning_effort.unwrap_or_else(|| model.default_reasoning_effort.clone());
        validate_effort(model, &reasoning_effort)?;
        let remote = resolved.remote.as_ref();
        let issue = resolved.issue.as_ref();
        let origin = AiChatOrigin {
            project_id: resolved.project_id.clone(),
            project_name: resolved.project_name.clone(),
            workspace_path: resolved.workspace_path.clone(),
            codex_project_id: remote.map(|target| target.codex_project_id.clone()),
            codex_project_kind: remote.map(|_| "remote".to_owned()),
            codex_host_id: remote.map(|target| target.codex_host_id.clone()),
            issue_id: issue.map(|issue| issue.id.clone()),
            issue_identifier: issue.map(|issue| issue.identifier.clone()),
        };
        let thread = NewAiChatThread {
            title: input.title.or_else(|| issue.map(|issue| issue.identifier.clone())).unwrap_or_else(|| "New conversation".to_owned()),
            origin,
            model: model.slug.clone(),
            reasoning_effort,
            sandbox: input.sandbox.unwrap_or_else(|| "workspace-write".to_owned()),
        };
        app.store(move |store| store.create_ai_chat_thread(thread)).await
    }

    /// `updateThread`: settings cannot change while a turn runs.
    pub async fn update(&self, app: &Arc<App>, id: &str, patch: ThreadPatch) -> Result<AiChatThread> {
        let mut thread = get_thread(app, id).await?;
        let settings = patch.model.is_some() || patch.reasoning_effort.is_some() || patch.sandbox.is_some();
        let was_active = settings && self.is_active(&thread);
        if patch.model.is_some() || patch.reasoning_effort.is_some() {
            let catalog = self.catalog(app, &thread.origin.project_id, target_of(&thread.origin).as_ref()).await?;
            thread = get_thread(app, id).await?;
            let model = resolve_model(&catalog, Some(patch.model.as_deref().unwrap_or(&thread.model)))?;
            validate_effort(model, patch.reasoning_effort.as_deref().unwrap_or(&thread.reasoning_effort))?;
        }
        if was_active || (settings && self.is_active(&thread)) {
            return Err(busy(id));
        }
        let changes = AiChatThreadChanges {
            title: patch.title,
            codex_thread_id: None,
            model: patch.model,
            reasoning_effort: patch.reasoning_effort,
            sandbox: patch.sandbox,
        };
        let key = id.to_owned();
        app.store(move |store| store.update_ai_chat_thread(&key, changes)).await
    }

    pub async fn delete(&self, app: &Arc<App>, id: &str) -> Result<()> {
        let thread = get_thread(app, id).await?;
        if self.is_active(&thread) {
            return Err(busy(id));
        }
        let key = id.to_owned();
        app.store(move |store| store.delete_ai_chat_thread(&key)).await?;
        Ok(())
    }

    /// `compactThread`: asks Codex to compact a started, idle conversation.
    pub async fn compact(&self, app: &Arc<App>, id: &str) -> Result<AiChatThread> {
        let thread = get_thread(app, id).await?;
        if thread.status == "running" {
            return Err(ApiError::new(409, "AI_CHAT_THREAD_RUNNING", "Cannot compact a running conversation").into());
        }
        let Some(codex_thread_id) = thread.codex_thread_id.clone().filter(|id| !id.is_empty()) else {
            return Err(ApiError::new(409, "AI_CHAT_THREAD_NOT_STARTED", "Conversation has not started").into());
        };
        let server = self.runtime(target_of(&thread.origin).as_ref()).server;
        server.request("thread/compact/start", json!({ "threadId": codex_thread_id })).await.map_err(Error::Internal)?;
        get_thread(app, id).await
    }

    /// `startTurn`: starts a turn and returns its run; the turn's events arrive on the thread stream.
    pub async fn start_turn(&self, app: &Arc<App>, thread_id: &str, turn: Turn) -> Result<AiChatRun> {
        let thread = get_thread(app, thread_id).await?;
        if self.is_active(&thread) {
            return Err(busy(thread_id));
        }
        let Turn { confirmed, attachments, body } = turn;
        match body {
            TurnBody::Composer { revision, nodes } => self.start_composer_turn(app, thread, confirmed, attachments, revision, nodes).await,
            TurnBody::Message { message, skill_ids } => {
                self.start_message_turn(app, thread, confirmed, attachments, message, skill_ids).await
            }
        }
    }

    async fn start_message_turn(
        &self,
        app: &Arc<App>,
        thread: AiChatThread,
        confirmed: bool,
        attachments: Vec<Attachment>,
        message: String,
        skill_ids: Vec<String>,
    ) -> Result<AiChatRun> {
        if js::trim(&message).is_empty() && attachments.is_empty() {
            return Err(ApiError::new(400, "INVALID_MESSAGE", "A message or at least one attachment is required").into());
        }
        confirm_danger(&thread, confirmed)?;
        let remote = target_of(&thread.origin);
        let resolved = context::resolve(app, &thread.origin.project_id, thread.origin.issue_id.as_deref(), remote.as_ref()).await?;
        let catalog = self.catalog_for(&resolved).await?;
        let thread = get_thread(app, &thread.id).await?;
        if self.is_active(&thread) {
            return Err(busy(&thread.id));
        }
        confirm_danger(&thread, confirmed)?;
        let model = resolve_model(&catalog, Some(&thread.model))?;
        validate_effort(model, &thread.reasoning_effort)?;
        if resolved.workspace_path != thread.origin.workspace_path {
            return Err(workspace_changed());
        }
        let mut skills = Vec::new();
        for id in &skill_ids {
            let skill = catalog.skills.iter().rev().find(|skill| skill.id == *id && skill.id != "manage-broadcast");
            let skill = skill.ok_or_else(|| ApiError::new(400, "INVALID_SKILL", format!("Unknown or unavailable skill '{id}'")))?;
            skills.push(SkillLink { id: skill.id.clone(), path: skill.path.clone() });
        }
        let mut data = Map::new();
        if !skills.is_empty() {
            data.insert("skillIds".into(), json!(skill_ids));
        }
        if !attachments.is_empty() {
            data.insert("attachments".into(), attachments.iter().map(Attachment::summary).collect());
        }
        let data = if data.is_empty() { Value::Null } else { Value::Object(data) };

        if resolved.remote.is_some() {
            let mut input = Vec::new();
            for (index, text) in message.split(exec::SKILL_MARKER).enumerate() {
                if !text.is_empty() {
                    input.push(json!({ "type": "text", "text": text }));
                }
                if let Some(skill) = skills.get(index) {
                    input.push(json!({ "type": "skill", "name": skill.id, "path": skill.path }));
                }
            }
            input.extend(attachments.iter().map(Attachment::remote_input));
            let user_event = EventRecord { kind: "user_message".into(), role: "user", content: message, data };
            return self.start_app_server_run(app, &thread, &resolved, input, user_event, None).await;
        }

        let written = write_attachments(&attachments).await?;
        let args = exec::build_args(&thread, &resolved.add_directories, &written.images);
        let prompt = exec::build_prompt(&thread, &message, &skills, &written.paths, &app.config.skill_path);
        let user_event = EventRecord { kind: "user_message".into(), role: "user", content: message, data };
        self.start_exec(app, &thread, &args, &prompt, user_event, written.directory).await
    }

    /// The local `codex exec` turn: records the run and user message, then drives the owner.
    async fn start_exec(
        &self,
        app: &Arc<App>,
        thread: &AiChatThread,
        args: &[String],
        prompt: &str,
        user_event: EventRecord,
        temporary: Option<PathBuf>,
    ) -> Result<AiChatRun> {
        let recorded = async {
            let run = self.create_run(app, &thread.id).await?;
            self.record(app, &thread.id, &run.id, user_event).await?;
            Ok::<_, Error>(run)
        };
        let run = match recorded.await {
            Ok(run) => run,
            Err(error) => {
                if let Some(directory) = &temporary {
                    remove_dir(directory).await;
                }
                return Err(error);
            }
        };
        let process = exec::spawn_turn(&self.executable, args, prompt).await;
        let pid = process.as_ref().ok().and_then(|process| process.pid);
        let active = Active::new(&run, temporary, Kind::Exec { pid });
        lock(&self.active).insert(run.id.clone(), Arc::clone(&active));
        let app = Arc::clone(app);
        let resuming = thread.codex_thread_id.clone().filter(|id| !id.is_empty());
        tokio::spawn(async move {
            let chat = &app.ai;
            match process {
                Ok(process) => chat.drive_exec(&app, &active, process, resuming.as_deref()).await,
                Err(error) => {
                    let message = failure(&error.to_string(), "Codex turn failed");
                    chat.conclude(&app, &active, "failed", None, Some(message), true).await;
                }
            }
        });
        Ok(run)
    }

    /// `spawnCodexTurn` + `finishRun`: reads the owner's JSONL until it exits, then records the outcome.
    async fn drive_exec(&self, app: &Arc<App>, active: &Arc<Active>, process: TurnProcess, resuming: Option<&str>) {
        let TurnProcess { pid, mut child, control, mut stdout, stderr } = process;
        tokio::spawn(exec::drain(stderr));
        let mut state = ExecState::default();
        let mut line = Vec::new();
        let mut fatal = None;
        loop {
            match exec::read_line(&mut stdout, &mut line, exec::MAX_LINE_BYTES).await {
                Ok(Line::Eof) => break,
                Ok(Line::TooLong) => {
                    fatal = Some(format!("Codex JSONL line exceeded {} bytes", exec::MAX_LINE_BYTES));
                    break;
                }
                Ok(Line::Read) => {
                    if let Err(error) = self.exec_line(app, active, &line, resuming, &mut state).await {
                        fatal = Some(error);
                        break;
                    }
                }
                Err(error) => {
                    fatal = Some(error.to_string());
                    break;
                }
            }
        }
        if fatal.is_some()
            && let Some(pid) = pid
        {
            exec::signal_group(pid, true);
        }
        exec::drain(stdout).await;
        let exit = child.wait().await;
        drop(control);
        let (exit, fatal) = match exit {
            Ok(status) => (Some(status), fatal),
            Err(error) => (None, fatal.or_else(|| Some(error.to_string()))),
        };

        let (status, error) = if active.interrupted() {
            ("interrupted", Some("Interrupted".to_owned()))
        } else if let Some(fatal) = &fatal {
            ("failed", Some(failure(fatal, "Codex turn failed")))
        } else if state.terminal == Some("failed") {
            ("failed", Some(failure(&state.terminal_error, "Codex reported a failed turn")))
        } else if let Some(message) = exit.as_ref().and_then(exit_failure) {
            ("failed", Some(message))
        } else if state.terminal != Some("completed") {
            ("failed", Some(failure(&state.pending_error, "Codex exited without reporting turn completion")))
        } else if resuming.is_none() && state.started.is_none() {
            ("failed", Some("Codex did not provide a thread id".to_owned()))
        } else {
            ("completed", None)
        };
        let exit_code = if fatal.is_some() { None } else { exit.and_then(|status| status.code()).map(i64::from) };
        let error_event = status == "failed" && state.terminal != Some("failed");
        self.conclude(app, active, status, exit_code, error, error_event).await;
    }

    /// One JSONL line from `codex exec`. An error is fatal to the turn.
    async fn exec_line(
        &self,
        app: &Arc<App>,
        active: &Active,
        line: &[u8],
        resuming: Option<&str>,
        state: &mut ExecState,
    ) -> std::result::Result<(), String> {
        let line = String::from_utf8_lossy(line.strip_suffix(b"\r").unwrap_or(line));
        if js::trim(&line).is_empty() {
            return Ok(());
        }
        let raw: Value = serde_json::from_str(&line).map_err(|_| "Codex emitted malformed JSONL".to_owned())?;
        let Some(event) = exec::normalize_event(&raw) else { return Ok(()) };
        match event {
            CodexEvent::ThreadStarted(id) => {
                if resuming.is_some_and(|resuming| resuming != id) || state.started.as_ref().is_some_and(|started| *started != id) {
                    return Err("Codex returned an unexpected thread id".to_owned());
                }
                state.started = Some(id.clone());
                let thread = active.thread_id.clone();
                let changes = AiChatThreadChanges { codex_thread_id: Some(id), ..AiChatThreadChanges::default() };
                app.store(move |store| store.update_ai_chat_thread(&thread, changes)).await.map_err(|error| anyhow::Error::from(error).to_string())?;
            }
            CodexEvent::Event(record) => {
                let content = record.content.clone();
                self.record(app, &active.thread_id, &active.run.id, record).await.map_err(|error| anyhow::Error::from(error).to_string())?;
                match raw.get("type").and_then(Value::as_str) {
                    Some("turn.completed") if state.terminal.is_none() => state.terminal = Some("completed"),
                    Some("turn.failed") => {
                        state.terminal = Some("failed");
                        if state.terminal_error.is_empty() {
                            state.terminal_error = content;
                        }
                    }
                    Some("error") if state.pending_error.is_empty() => state.pending_error = content,
                    _ => {}
                }
            }
        }
        Ok(())
    }

    /// Records a finished run: the error event when one is due, the run row, then releases the turn.
    async fn conclude(
        &self,
        app: &Arc<App>,
        active: &Active,
        status: &'static str,
        exit_code: Option<i64>,
        error: Option<String>,
        error_event: bool,
    ) {
        if error_event && let Some(message) = &error {
            let event = EventRecord { kind: "error".into(), role: "error", content: capped_str(message), data: json!({ "status": "failed" }) };
            let _ = self.record(app, &active.thread_id, &active.run.id, event).await;
        }
        let id = active.run.id.clone();
        let outcome = AiChatRunOutcome { status, exit_code, error: error.map(|error| capped_str(&error)) };
        if let Ok(run) = app.store(move |store| store.finish_ai_chat_run(&id, outcome)).await {
            self.emit(&active.thread_id, json!({ "type": "ai.run", "run": run }));
        }
        lock(&self.active).remove(&active.run.id);
        if let Some(directory) = &active.temporary {
            remove_dir(directory).await;
        }
        active.done.send_replace(true);
    }

    /// `finishAppServerRun`: only the first finish of a run records it.
    async fn finish_app_server(&self, app: &Arc<App>, active: &Active, status: &'static str, error: Option<String>) {
        if lock(&self.active).remove(&active.run.id).is_none() {
            return;
        }
        let error = match status {
            "interrupted" => Some("Interrupted".to_owned()),
            "failed" => Some(failure(error.as_deref().unwrap_or_default(), "Codex turn failed")),
            _ => None,
        };
        self.conclude(app, active, status, None, error, status == "failed").await;
    }

    async fn start_composer_turn(
        &self,
        app: &Arc<App>,
        thread: AiChatThread,
        confirmed: bool,
        attachments: Vec<Attachment>,
        revision: String,
        nodes: Vec<DocumentNode>,
    ) -> Result<AiChatRun> {
        confirm_danger(&thread, confirmed)?;
        let remote = target_of(&thread.origin);
        let resolved = context::resolve(app, &thread.origin.project_id, thread.origin.issue_id.as_deref(), remote.as_ref()).await?;
        let runtime = self.runtime(resolved.remote.as_ref());
        let thread = get_thread(app, &thread.id).await?;
        if self.is_active(&thread) {
            return Err(busy(&thread.id));
        }
        if resolved.workspace_path != thread.origin.workspace_path {
            return Err(workspace_changed());
        }
        if let Some(index) = nodes.iter().position(|node| matches!(node, DocumentNode::Other(_))) {
            let message = format!("Unsupported composer node at index {index}");
            return Err(ApiError::new(422, "COMPOSER_NODE_UNSUPPORTED", message).with_details(json!({ "nodeIndex": index })).into());
        }
        let has_references = nodes.iter().any(DocumentNode::is_reference);
        let references = if has_references {
            runtime.composer.resolve(&resolved.workspace_path, &revision, &nodes).await?
        } else {
            Vec::new()
        };
        let blank = nodes.iter().all(|node| match node {
            DocumentNode::Text(text) => js::trim(text).is_empty(),
            _ => true,
        });
        if blank && !has_references && attachments.is_empty() {
            let message = "A composer message or at least one attachment is required";
            return Err(ApiError::new(400, "INVALID_COMPOSER_DOCUMENT", message).into());
        }

        let written = if resolved.remote.is_some() { Written::default() } else { write_attachments(&attachments).await? };
        let mut input = Vec::new();
        let mut dispatches = Vec::new();
        for (index, node) in nodes.iter().enumerate() {
            match (node, references.get(index).and_then(Option::as_ref)) {
                (DocumentNode::Text(text), _) => input.push(json!({ "type": "text", "text": text })),
                (_, Some(Reference::Skill(skill))) => {
                    input.push(json!({ "type": "text", "text": format!("${}", skill.name) }));
                    input.push(json!({ "type": "skill", "name": skill.name, "path": skill.path }));
                }
                (_, Some(Reference::Agent(agent))) => {
                    input.push(json!({ "type": "text", "text": agent_dispatch_text(agent) }));
                    dispatches.push(json!({ "nodeIndex": index, "id": agent.stable_id, "name": agent.name }));
                }
                _ => {}
            }
        }
        for (index, attachment) in attachments.iter().enumerate() {
            if resolved.remote.is_some() {
                input.push(attachment.remote_input());
            } else if attachment.is_image() {
                input.push(json!({ "type": "localImage", "path": written.paths[index] }));
            } else {
                input.push(json!({ "type": "text", "text": format!("\n\nAttached file: {}", written.paths[index]) }));
            }
        }

        let content: String = nodes
            .iter()
            .map(|node| match node {
                DocumentNode::Text(text) => text.clone(),
                DocumentNode::Reference { label, .. } => format!("@{label}"),
                DocumentNode::Other(_) => String::new(),
            })
            .collect();
        let mut data = Map::new();
        data.insert("contractVersion".into(), json!(composer::CONTRACT_VERSION));
        data.insert("revision".into(), json!(revision));
        data.insert("document".into(), json!({ "version": 1, "nodes": nodes.iter().map(document_node).collect::<Vec<_>>() }));
        if !dispatches.is_empty() {
            data.insert("dispatchProtocol".into(), json!(AGENT_DISPATCH_PROTOCOL));
            data.insert("agentDispatches".into(), Value::Array(dispatches));
        }
        if !attachments.is_empty() {
            data.insert("attachments".into(), attachments.iter().map(Attachment::summary).collect());
        }
        let user_event = EventRecord { kind: "user_message".into(), role: "user", content, data: Value::Object(data) };
        let directory = written.directory.clone();
        let started = self.start_app_server_run(app, &thread, &resolved, input, user_event, written.directory).await;
        if started.is_err()
            && let Some(directory) = &directory
        {
            remove_dir(directory).await;
        }
        started
    }

    /// `startAppServerRun`: resumes or starts the Codex thread, records the run, then starts the turn.
    async fn start_app_server_run(
        &self,
        app: &Arc<App>,
        thread: &AiChatThread,
        resolved: &Resolved,
        input: Vec<Value>,
        user_event: EventRecord,
        temporary: Option<PathBuf>,
    ) -> Result<AiChatRun> {
        let server = self.runtime(resolved.remote.as_ref()).server;
        let permissions = exec::permissions(&thread.sandbox);
        let mut roots = vec![resolved.workspace_path.clone()];
        roots.extend(resolved.add_directories.iter().cloned());
        let mut settings = Map::new();
        settings.insert("model".into(), json!(thread.model));
        settings.insert("cwd".into(), json!(resolved.workspace_path));
        settings.insert("runtimeWorkspaceRoots".into(), json!(roots));
        settings.insert("approvalPolicy".into(), json!(permissions.approval_policy));
        if let Some(reviewer) = permissions.reviewer {
            settings.insert("approvalsReviewer".into(), json!(reviewer));
        }
        settings.insert("sandbox".into(), json!(permissions.sandbox));

        let codex_thread_id = match thread.codex_thread_id.clone().filter(|id| !id.is_empty()) {
            Some(id) => {
                let mut params = Map::new();
                params.insert("threadId".into(), json!(id));
                params.extend(settings);
                let resumed = server.request("thread/resume", Value::Object(params)).await.map_err(Error::Internal)?;
                if resumed.pointer("/thread/id").and_then(Value::as_str) != Some(id.as_str()) {
                    return Err(internal("Codex returned an unexpected resumed thread id"));
                }
                id
            }
            None => {
                let started = server.request("thread/start", Value::Object(settings)).await.map_err(Error::Internal)?;
                let id = started.pointer("/thread/id").and_then(Value::as_str).filter(|id| !id.is_empty());
                let id = id.ok_or_else(|| internal("Codex did not provide a thread id"))?.to_owned();
                let (key, changes) = (thread.id.clone(), AiChatThreadChanges { codex_thread_id: Some(id.clone()), ..AiChatThreadChanges::default() });
                app.store(move |store| store.update_ai_chat_thread(&key, changes)).await?;
                id
            }
        };

        let run = self.create_run(app, &thread.id).await?;
        self.record(app, &thread.id, &run.id, user_event).await?;
        let origin = server.origin().await;
        let kind = Kind::AppServer { server: server.clone(), origin, codex_thread_id: codex_thread_id.clone(), turn_id: Mutex::new(None) };
        let active = Active::new(&run, temporary, kind);
        lock(&self.active).insert(run.id.clone(), Arc::clone(&active));
        let params = json!({ "threadId": codex_thread_id, "input": input, "effort": thread.reasoning_effort });
        let turn_id = match server.request("turn/start", params).await {
            Ok(started) => started
                .pointer("/turn/id")
                .and_then(Value::as_str)
                .filter(|id| !id.is_empty())
                .map(str::to_owned)
                .ok_or_else(|| anyhow::anyhow!("Codex did not provide a turn id")),
            Err(error) => Err(error),
        };
        match turn_id {
            Ok(turn_id) => {
                if let Kind::AppServer { turn_id: known, .. } = &active.kind {
                    *lock(known) = Some(turn_id);
                }
                Ok(run)
            }
            Err(error) => {
                self.finish_app_server(app, &active, "failed", Some(error.to_string())).await;
                Err(Error::Internal(error))
            }
        }
    }

    /// `interrupt`: stops a running turn and returns its run once it settles (or the grace runs out).
    pub async fn interrupt(&self, app: &Arc<App>, run_id: &str) -> Result<AiChatRun> {
        let run = get_run(app, run_id).await?;
        if run.status != "running" {
            return Ok(run);
        }
        let active = lock(&self.active).get(run_id).cloned();
        let Some(active) = active else {
            let id = run_id.to_owned();
            let outcome = AiChatRunOutcome { status: "interrupted", exit_code: None, error: Some("Interrupted".to_owned()) };
            let run = app.store(move |store| store.finish_ai_chat_run(&id, outcome)).await?;
            self.emit(&run.thread_id, json!({ "type": "ai.run", "run": run }));
            return Ok(run);
        };
        active.interrupted.store(true, Ordering::SeqCst);
        match &active.kind {
            Kind::AppServer { server, codex_thread_id, turn_id, .. } => {
                let turn = lock(turn_id).clone();
                if let Some(turn) = turn {
                    let _ = server.request("turn/interrupt", json!({ "threadId": codex_thread_id, "turnId": turn })).await;
                }
                active.settled(INTERRUPT_WAIT).await;
                if lock(&self.active).contains_key(run_id) {
                    self.finish_app_server(app, &active, "interrupted", None).await;
                }
            }
            Kind::Exec { pid } => {
                if let Some(pid) = *pid {
                    exec::signal_group(pid, false);
                    let (app, id) = (Arc::clone(app), run_id.to_owned());
                    tokio::spawn(async move {
                        tokio::time::sleep(KILL_GRACE).await;
                        if lock(&app.ai.active).contains_key(&id) {
                            exec::signal_group(pid, true);
                        }
                    });
                }
                active.settled(INTERRUPT_WAIT).await;
            }
        }
        get_run(app, run_id).await
    }

    /// Drains app-server notifications into the runs they belong to.
    pub async fn consume(app: Arc<App>, mut notifications: mpsc::UnboundedReceiver<Notification>) {
        while let Some(notification) = notifications.recv().await {
            app.ai.notify(&app, notification).await;
        }
    }

    /// `handleAppServerNotification`.
    async fn notify(&self, app: &Arc<App>, notification: Notification) {
        let Notification { origin, method, params } = notification;
        if method == "skills/changed"
            && let Some(composer) = self.composer_for(&origin)
        {
            composer.invalidate();
        }
        let Some(params) = params.as_object() else { return };
        if method == "app-server/terminated" {
            // A remote bridge disconnect does not establish that the remote turn stopped.
            if !matches!(origin, Origin::Local(_)) {
                return;
            }
            let message = params.get("message").and_then(Value::as_str).map(str::to_owned);
            let terminated: Vec<Arc<Active>> = lock(&self.active)
                .values()
                .filter(|active| matches!(&active.kind, Kind::AppServer { origin: Some(known), .. } if *known == origin))
                .cloned()
                .collect();
            for active in terminated {
                let status = if active.interrupted() { "interrupted" } else { "failed" };
                self.finish_app_server(app, &active, status, message.clone()).await;
            }
            return;
        }
        let thread_id = params.get("threadId").and_then(Value::as_str);
        let turn_id = params.get("turnId").and_then(Value::as_str).filter(|id| !id.is_empty());
        let active = lock(&self.active)
            .values()
            .find(|active| match &active.kind {
                Kind::AppServer { origin: Some(known), codex_thread_id, turn_id: current, .. } => {
                    let current = lock(current);
                    *known == origin
                        && Some(codex_thread_id.as_str()) == thread_id
                        && (current.is_none() || turn_id.is_none() || current.as_deref() == turn_id)
                }
                _ => false,
            })
            .cloned();
        let Some(active) = active else { return };
        match method.as_str() {
            "turn/started" => {
                if let (Some(id), Kind::AppServer { turn_id, .. }) = (params.get("turn").and_then(|turn| turn.get("id")).and_then(Value::as_str), &active.kind) {
                    *lock(turn_id) = Some(id.to_owned());
                }
            }
            "item/completed" => {
                if let Some(event) = app_server_item(params.get("item")) {
                    let _ = self.record(app, &active.thread_id, &active.run.id, event).await;
                }
            }
            "turn/completed" => {
                let turn = params.get("turn");
                let status = turn.and_then(|turn| turn.get("status")).and_then(Value::as_str);
                if active.interrupted() || status == Some("interrupted") {
                    self.finish_app_server(app, &active, "interrupted", None).await;
                } else if status == Some("completed") {
                    self.finish_app_server(app, &active, "completed", None).await;
                } else {
                    let message = match turn.and_then(|turn| turn.pointer("/error/message")) {
                        None | Some(Value::Null) => "Codex reported a failed turn".to_owned(),
                        other => js_string(other),
                    };
                    self.finish_app_server(app, &active, "failed", Some(message)).await;
                }
            }
            _ => {}
        }
    }

    /// `close`: interrupts every run, waits out the grace, then kills or abandons what is left.
    pub async fn close(&self, app: &Arc<App>) {
        let entries: Vec<Arc<Active>> = lock(&self.active).values().cloned().collect();
        for active in &entries {
            active.interrupted.store(true, Ordering::SeqCst);
            match &active.kind {
                Kind::AppServer { server, codex_thread_id, turn_id, .. } => {
                    let turn = lock(turn_id).clone();
                    if let Some(turn) = turn {
                        let (server, params) = (server.clone(), json!({ "threadId": codex_thread_id, "turnId": turn }));
                        tokio::spawn(async move {
                            let _ = server.request("turn/interrupt", params).await;
                        });
                    }
                }
                Kind::Exec { pid: Some(pid) } => exec::signal_group(*pid, false),
                Kind::Exec { pid: None } => {}
            }
        }
        if !entries.is_empty() {
            let _ = tokio::time::timeout(KILL_GRACE, futures_util::future::join_all(entries.iter().map(|active| active.wait()))).await;
            for active in &entries {
                let running = lock(&self.active).contains_key(&active.run.id);
                match &active.kind {
                    Kind::Exec { pid: Some(pid) } if running => exec::signal_group(*pid, true),
                    Kind::AppServer { .. } if running => self.finish_app_server(app, active, "interrupted", None).await,
                    _ => {}
                }
            }
            futures_util::future::join_all(entries.iter().map(|active| active.wait())).await;
        }
        self.local_server.close().await;
        lock(&self.remote).clear();
    }
}
