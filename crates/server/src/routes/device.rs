//! Machine capabilities: `/api/meta`, `/api/device-workspaces` and `/api/projects/:id/development-contexts`.

use std::sync::Arc;

use axum::Extension;
use axum::extract::{Request, State};
use broadcast_domain::constants::DEFAULT_PROJECT_ID;
use broadcast_domain::js::decode_uri_component;
use broadcast_domain::{ApiError, Result, fields};
use serde_json::{Value, json};

use crate::App;
use crate::cloud::config::CloudConfig;
use crate::codex;
use crate::http::{Reply, json, method_not_allowed, query, reject_query, segment};
use crate::trust::Peer;

/// Machine capabilities of a cloud companion stay with this device, so only its loopback may ask.
async fn capability_config(app: &App, peer: &Peer) -> Result<CloudConfig> {
    let config = app.cloud.read().await?;
    if config.remote_url.is_some() {
        peer.assert_local()?;
    }
    Ok(config)
}

pub async fn meta(State(app): State<Arc<App>>, Extension(peer): Extension<Peer>, request: Request) -> Reply {
    let config = capability_config(&app, &peer).await?;
    if request.method() != "GET" {
        return method_not_allowed(&["GET"]);
    }
    reject_query(request.uri(), "GET /api/meta does not accept query parameters")?;
    let mut body = serde_json::Map::new();
    if !peer.configured {
        body.insert("manageBroadcastSkillPath".into(), json!(app.config.skill_path));
    }
    body.insert("capabilities".into(), json!({ "localAiChat": !peer.configured && peer.loopback() }));
    if config.remote_url.is_some() {
        body.insert("mode".into(), json!("cloud"));
        body.insert("realtime".into(), json!({ "transport": "websocket", "endpoint": "/api/events" }));
        body.insert("localCapabilities".into(), json!({ "available": !peer.configured }));
    }
    Ok(json(200, &Value::Object(body)))
}

pub async fn workspaces(State(app): State<Arc<App>>, Extension(peer): Extension<Peer>, request: Request) -> Reply {
    capability_config(&app, &peer).await?;
    if request.method() != "GET" {
        return method_not_allowed(&["GET"]);
    }
    reject_query(request.uri(), "GET /api/device-workspaces does not accept query parameters")?;
    Ok(json(200, &json!({ "workspaces": codex::workspaces(&app.config.codex_home).await })))
}

pub async fn development_contexts(
    State(app): State<Arc<App>>,
    Extension(peer): Extension<Peer>,
    request: Request,
) -> Reply {
    let config = capability_config(&app, &peer).await?;
    if request.method() != "GET" {
        return method_not_allowed(&["GET"]);
    }
    let query = query(request.uri());
    const ALLOWED: [&str; 3] = ["codexProjectId", "codexThreadId", "workspacePath"];
    if let Some((key, _)) = query.iter().find(|(key, _)| !ALLOWED.contains(&key.as_str())) {
        return Err(ApiError::new(400, "UNKNOWN_QUERY_PARAMETER", format!("Unknown query parameter: {key}")).into());
    }
    let id = decode_uri_component(segment(request.uri(), 3))
        .ok_or_else(|| ApiError::new(400, "INVALID_PATH", "Project id contains invalid encoding"))?;
    let id = fields::project_id(Some(&Value::String(id)))?;
    // A cloud board's projects live remotely; this device only knows its own workspace mappings.
    let project_workspace = if config.remote_url.is_some() {
        (id != DEFAULT_PROJECT_ID).then(|| config.workspace(&id).map(str::to_owned)).flatten()
    } else {
        let lookup = id.clone();
        app.store(move |store| store.get_project(&lookup))
            .await?
            .ok_or_else(|| ApiError::new(404, "PROJECT_NOT_FOUND", format!("Project '{id}' does not exist")))?
            .workspace_path
    };
    let param = |name: &str, max: usize| {
        let value = query.iter().find(|(key, _)| key == name).map_or(Value::Null, |(_, value)| json!(value));
        fields::nullable_string(Some(&value), name, max).map(Option::flatten)
    };
    let codex_project_id = param("codexProjectId", 128)?;
    let thread_id = param("codexThreadId", 256)?;
    let workspace = param("workspacePath", 4096)?;
    if workspace.as_deref().is_some_and(|path| path.contains('\0')) {
        return Err(ApiError::invalid_field("'workspacePath' cannot contain null bytes").into());
    }
    let workspace = match workspace {
        Some(path) => Some(path),
        None => {
            codex::resolve_workspace(
                &app.config.codex_home,
                &id,
                project_workspace.as_deref(),
                codex_project_id.as_deref(),
                thread_id.as_deref(),
            )
            .await
        }
    };
    Ok(json(200, &codex::development_contexts(workspace.as_deref()).await))
}
