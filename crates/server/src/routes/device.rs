//! Machine capabilities: `/api/meta`, `/api/device-workspaces` and `/api/projects/:id/development-contexts`.

use std::sync::Arc;

use axum::Extension;
use axum::extract::{Request, State};
use broadcast_domain::js::decode_uri_component;
use broadcast_domain::{ApiError, fields};
use serde_json::{Value, json};

use crate::App;
use crate::codex;
use crate::http::{Reply, json, method_not_allowed, query, reject_query, segment};
use crate::trust::Peer;

pub async fn meta(State(app): State<Arc<App>>, Extension(peer): Extension<Peer>, request: Request) -> Reply {
    if request.method() != "GET" {
        return method_not_allowed(&["GET"]);
    }
    reject_query(request.uri(), "GET /api/meta does not accept query parameters")?;
    let mut body = serde_json::Map::new();
    if !peer.configured {
        body.insert("manageBroadcastSkillPath".into(), json!(app.config.skill_path));
    }
    body.insert("capabilities".into(), json!({ "localAiChat": !peer.configured && peer.loopback }));
    Ok(json(200, &Value::Object(body)))
}

pub async fn workspaces(State(app): State<Arc<App>>, request: Request) -> Reply {
    if request.method() != "GET" {
        return method_not_allowed(&["GET"]);
    }
    reject_query(request.uri(), "GET /api/device-workspaces does not accept query parameters")?;
    Ok(json(200, &json!({ "workspaces": codex::workspaces(&app.config.codex_home).await })))
}

pub async fn development_contexts(State(app): State<Arc<App>>, request: Request) -> Reply {
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
    let lookup = id.clone();
    let project = app
        .store(move |store| store.get_project(&lookup))
        .await?
        .ok_or_else(|| ApiError::new(404, "PROJECT_NOT_FOUND", format!("Project '{id}' does not exist")))?;
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
                &project.id,
                project.workspace_path.as_deref(),
                codex_project_id.as_deref(),
                thread_id.as_deref(),
            )
            .await
        }
    };
    Ok(json(200, &codex::development_contexts(workspace.as_deref()).await))
}
