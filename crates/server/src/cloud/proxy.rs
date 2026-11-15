//! Cloud mode: business APIs go to the cloud board under this
//! device's Basic credentials, while workspace paths stay on this device.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use axum::body::{Body, HttpBody, to_bytes};
use axum::extract::{Request, State};
use axum::http::{HeaderMap, HeaderName, HeaderValue, Method, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use broadcast_domain::constants::DEFAULT_PROJECT_ID;
use broadcast_domain::js::{decode_uri_component, truthy};
use broadcast_domain::{ApiError, Error, Result};
use serde_json::{Map, Value, json};

use crate::App;
use crate::cloud::config::CloudConfig;
use crate::cloud::realtime;
use crate::codex;
use crate::http::{Failure, header as text};
use crate::trust::Peer;

/// Routes answered by this device even when a cloud board is configured.
pub fn is_local_companion_route(path: &str) -> bool {
    matches!(
        path,
        "/health" | "/api/meta" | "/api/device-workspaces" | "/api/local/cloud-session"
    ) || path.starts_with("/api/local/")
        || path
            .strip_prefix("/api/projects/")
            .and_then(|rest| rest.strip_suffix("/development-contexts"))
            .is_some_and(|id| !id.is_empty() && !id.contains('/'))
}

/// The cloud board's origin and the credentials this device signs in with.
pub struct Remote {
    pub origin: String,
    pub authorization: HeaderValue,
}

impl Remote {
    pub fn of(config: &CloudConfig) -> Result<Self, ApiError> {
        let (Some(origin), Some(actor), Some(key)) =
            (&config.remote_url, &config.actor_name, &config.shared_key)
        else {
            return Err(ApiError::new(
                409,
                "CLOUD_NOT_CONFIGURED",
                "Cloud collaboration is not configured",
            ));
        };
        let authorization = format!("Basic {}", STANDARD.encode(format!("{actor}:{key}")));
        Ok(Self {
            origin: origin.clone(),
            authorization: HeaderValue::from_str(&authorization).expect("base64 is a header value"),
        })
    }
}

/// In cloud mode every `/api/*` call must come from this device, and all but its own routes go to the cloud.
pub async fn route(State(app): State<Arc<App>>, request: Request, next: Next) -> Response {
    if realtime::is_upgrade(&request) {
        return realtime::relay(app, request).await;
    }
    let path = request.uri().path();
    let local = !path.starts_with("/api/")
        || path == "/api/client-storage"
        || is_local_companion_route(path);
    if local {
        return next.run(request).await;
    }
    let config = match app.cloud.read().await {
        Ok(config) if config.remote_url.is_some() => config,
        Ok(_) => return next.run(request).await,
        Err(error) => return Failure::from(Error::from(error)).into_response(),
    };
    let peer = *request
        .extensions()
        .get::<Peer>()
        .expect("the trust gate identifies the peer");
    let forwarded = async {
        peer.assert_local()?;
        app.cloud_proxy.forward(&app, &config, request).await
    };
    forwarded
        .await
        .unwrap_or_else(|error| Failure::from(error).into_response())
}

/// A project create names the device path its new project maps to.
struct Workspace {
    project_id: Option<String>,
    path: String,
}

pub struct Proxy {
    client: reqwest::Client,
}

impl Proxy {
    pub fn new() -> reqwest::Result<Self> {
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .no_proxy()
            .connect_timeout(Duration::from_secs(10))
            .build()?;
        Ok(Self { client })
    }

    /// `forward`: exactly one upstream request, then this device's paths overlaid on the reply.
    pub async fn forward(
        &self,
        app: &Arc<App>,
        config: &CloudConfig,
        request: Request,
    ) -> Result<Response> {
        let remote = Remote::of(config)?;
        let (parts, body) = request.into_parts();
        let path = parts.uri.path().to_owned();
        let target = format!(
            "{}{}",
            remote.origin,
            parts
                .uri
                .path_and_query()
                .map_or("/", |value| value.as_str())
        );
        let mut headers = parts.headers;
        for name in [
            header::AUTHORIZATION,
            header::HOST,
            header::CONNECTION,
            header::TRANSFER_ENCODING,
            header::ACCEPT_ENCODING,
        ] {
            headers.remove(name);
        }
        let spoofed: Vec<HeaderName> = headers
            .keys()
            .filter(|name| name.as_str().starts_with("x-broadcast-user-"))
            .cloned()
            .collect();
        for name in spoofed {
            headers.remove(name);
        }
        headers.insert(header::AUTHORIZATION, remote.authorization);
        let (body, workspace) = prepare(app, &parts.method, &path, &mut headers, body).await?;
        let mut upstream = self
            .client
            .request(parts.method.clone(), &target)
            .headers(headers);
        if let Some(body) =
            body.filter(|_| parts.method != Method::GET && parts.method != Method::HEAD)
        {
            upstream = upstream.body(body);
        }
        let response = upstream.send().await.map_err(|error| {
            ApiError::new(
                502,
                "REMOTE_UNAVAILABLE",
                format!("Cannot reach cloud broadcast at {}", remote.origin),
            )
            .with_details(json!(error.to_string()))
        })?;
        localize(app, response, workspace).await
    }

    /// `readCloudJson`: a cloud GET whose JSON reply, or error, the caller uses directly.
    pub async fn read_json(
        &self,
        app: &Arc<App>,
        config: &CloudConfig,
        path: &str,
    ) -> Result<Value> {
        let request = Request::get(path)
            .header(header::ACCEPT, "application/json")
            .body(Body::empty())
            .expect("valid request");
        let response = self.forward(app, config, request).await?;
        let status = response.status();
        let bytes = to_bytes(response.into_body(), usize::MAX)
            .await
            .map_err(|error| Error::Internal(error.into()))?;
        let Ok(payload) = serde_json::from_slice::<Value>(&bytes) else {
            let status = if status.is_success() {
                502
            } else {
                status.as_u16()
            };
            return Err(ApiError::new(
                status,
                "INVALID_CLOUD_RESPONSE",
                "Cloud broadcast returned an invalid JSON response",
            )
            .into());
        };
        if !status.is_success() {
            let error = &payload["error"];
            let code = error["code"]
                .as_str()
                .map_or("CLOUD_REQUEST_FAILED".into(), |code| code.to_owned());
            let message = error["message"]
                .as_str()
                .unwrap_or("Cloud broadcast request failed");
            let mut failure = ApiError::new(status.as_u16(), code, message);
            failure.details = error.get("details").cloned().map(Box::new);
            return Err(failure.into());
        }
        Ok(payload)
    }
}

/// `prepareRequest`: strips device paths from JSON mutations and attaches the host thread's identity.
async fn prepare(
    app: &Arc<App>,
    method: &Method,
    path: &str,
    headers: &mut HeaderMap,
    body: Body,
) -> Result<(Option<reqwest::Body>, Option<Workspace>)> {
    let json = text(headers, header::CONTENT_TYPE.as_str())
        .is_some_and(|value| value.contains("application/json"));
    let project_create = method == Method::POST && path == "/api/projects";
    let task_patch = path
        .strip_prefix("/api/tasks/")
        .filter(|id| method == Method::PATCH && !id.is_empty() && !id.contains('/'));
    let task_mutation = (method == Method::POST && path == "/api/tasks") || task_patch.is_some();
    let conversation = method != Method::GET
        && (path == "/api/tasks"
            || path.starts_with("/api/tasks/")
            || path.starts_with("/api/comments/"));
    if !(json && (project_create || task_mutation || conversation)) {
        let streamed = (body.size_hint().exact() != Some(0))
            .then(|| reqwest::Body::wrap_stream(body.into_data_stream()));
        return Ok((streamed, None));
    }
    let invalid_json =
        || ApiError::new(400, "INVALID_JSON", "Request body must contain valid JSON");
    let bytes = to_bytes(body, usize::MAX)
        .await
        .map_err(|_| invalid_json())?;
    let mut payload: Value =
        serde_json::from_str(&String::from_utf8_lossy(&bytes)).map_err(|_| invalid_json())?;
    let Value::Object(object) = &mut payload else {
        return Err(
            ApiError::new(400, "INVALID_BODY", "Request body must be a JSON object").into(),
        );
    };
    if let Some(id) = task_patch
        && let Some(target) = object.get("projectId").and_then(Value::as_str)
    {
        let task_id = decode_uri_component(id).ok_or_else(|| {
            ApiError::new(400, "INVALID_PATH", "Task id contains invalid encoding")
        })?;
        let target = target.to_owned();
        if app
            .store(move |store| store.has_ai_chat_project_conflict(&task_id, &target))
            .await?
        {
            return Err(ApiError::new(
                409,
                "AI_CHAT_PROJECT_MOVE_BLOCKED",
                "Delete issue-linked AI conversations before moving the issue to another project",
            )
            .into());
        }
    }
    let mut workspace = None;
    if project_create && let Some(Value::String(path)) = object.shift_remove("workspacePath") {
        if !Path::new(&path).is_absolute() {
            return Err(ApiError::new(
                400,
                "INVALID_PROJECT_MAPPING",
                "Project workspacePath must be absolute",
            )
            .into());
        }
        workspace = Some(Workspace {
            project_id: object.get("id").and_then(Value::as_str).map(str::to_owned),
            path,
        });
    }
    if task_mutation
        && let Some(context) = object
            .get_mut("developmentContext")
            .filter(|context| context["type"] == "worktree")
    {
        *context = cloud_context(context);
    }
    let agent_session = object.get("agentSession").is_some_and(truthy);
    if conversation && !agent_session && !object.contains_key("threadBinding") {
        let binding = object
            .get("threadId")
            .and_then(Value::as_str)
            .and_then(|id| app.host.thread_binding(Some(id)));
        if let Some(binding) = binding {
            object.insert("threadBinding".into(), serde_json::to_value(binding)?);
        }
    }
    headers.remove(header::CONTENT_LENGTH);
    Ok((
        Some(reqwest::Body::from(serde_json::to_vec(&payload)?)),
        workspace,
    ))
}

/// What the cloud may know about a worktree: its branch, never this device's path.
fn cloud_context(context: &Value) -> Value {
    let mut cloud = Map::from_iter([("type".to_owned(), json!("worktree"))]);
    if let Some(branch) = context.get("branch") {
        cloud.insert("branch".into(), branch.clone());
    }
    Value::Object(cloud)
}

/// `localizeResponse` and `sendFetchResponse`: JSON replies get this device's workspace paths.
async fn localize(
    app: &App,
    response: reqwest::Response,
    workspace: Option<Workspace>,
) -> Result<Response> {
    let status = response.status();
    let mut headers = response.headers().clone();
    for name in [
        header::CONNECTION,
        header::CONTENT_LENGTH,
        header::TRANSFER_ENCODING,
    ] {
        headers.remove(name);
    }
    let json = text(&headers, header::CONTENT_TYPE.as_str())
        .is_some_and(|value| value.contains("application/json"));
    if status == 401 || !json {
        let mut reply = Body::from_stream(response.bytes_stream()).into_response();
        *reply.status_mut() = status;
        *reply.headers_mut() = headers;
        return Ok(reply);
    }
    headers.remove(header::CONTENT_ENCODING);
    let mut payload: Value = serde_json::from_slice(
        &response
            .bytes()
            .await
            .map_err(|error| Error::Internal(error.into()))?,
    )?;
    if status.is_success()
        && let Some(workspace) = workspace
        && let Some(project_id) = workspace
            .project_id
            .or_else(|| payload["project"]["id"].as_str().map(str::to_owned))
        && !project_id.is_empty()
    {
        app.cloud
            .set_project_workspace(&project_id, &workspace.path)
            .await
            .map_err(Error::from)?;
    }
    let config = app.cloud.read().await.map_err(Error::from)?;
    let mapped = |project: &mut Value| {
        let Value::Object(project) = project else {
            return;
        };
        let id = project
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let path = if id == DEFAULT_PROJECT_ID {
            None
        } else {
            config.workspace(id)
        };
        project.insert("workspacePath".into(), json!(path));
    };
    if let Some(projects) = payload.get_mut("projects").and_then(Value::as_array_mut) {
        projects.iter_mut().for_each(mapped);
    }
    if let Some(project) = payload
        .get_mut("project")
        .filter(|project| project.is_object())
    {
        mapped(project);
    }
    let mut scans = HashMap::new();
    if let Some(task) = payload.get_mut("task").filter(|task| truthy(task)) {
        localize_task(&config, task, &mut scans).await;
    }
    if let Some(tasks) = payload.get_mut("tasks").and_then(Value::as_array_mut) {
        for task in tasks {
            localize_task(&config, task, &mut scans).await;
        }
    }
    let mut reply = Body::from(serde_json::to_vec(&payload)?).into_response();
    *reply.status_mut() = status;
    *reply.headers_mut() = headers;
    Ok(reply)
}

/// `localizeTask`: a worktree context gets this device's checkout of its branch, or a null path.
async fn localize_task(config: &CloudConfig, task: &mut Value, scans: &mut HashMap<String, Value>) {
    let Some(context) = task
        .get("developmentContext")
        .filter(|context| context["type"] == "worktree")
    else {
        return;
    };
    let cloud = cloud_context(context);
    let branch = cloud["branch"].as_str().filter(|branch| !branch.is_empty());
    let workspace = task["projectId"]
        .as_str()
        .and_then(|id| config.workspace(id));
    let mut local = None;
    if let (Some(branch), Some(workspace)) = (branch, workspace) {
        if !scans.contains_key(workspace) {
            scans.insert(
                workspace.to_owned(),
                codex::development_contexts(Some(workspace)).await,
            );
        }
        local = scans[workspace]["contexts"]
            .as_array()
            .and_then(|contexts| {
                contexts.iter().find(|candidate| {
                    candidate["type"] == "worktree" && candidate["branch"] == branch
                })
            })
            .cloned();
    }
    task["developmentContext"] = local.unwrap_or_else(|| {
        let mut cloud = cloud;
        cloud["path"] = Value::Null;
        cloud
    });
}
