//! `/api/local/*`: capabilities only the device-local server offers.

use std::path::Path;
use std::sync::Arc;

use axum::extract::{Request, State};
use broadcast_domain::js::decode_uri_component;
use broadcast_domain::{ApiError, fields};
use serde_json::{Map, Value, json};

use crate::App;
use crate::cloud::config::CloudConfig;
use crate::codex::progress::{is_thread_id, strip_thread_prefix};
use crate::http::{Reply, json, method_not_allowed, query, read_json, reject_query, segment};

pub async fn host_runtime(State(app): State<Arc<App>>, request: Request) -> Reply {
    match request.method().as_str() {
        "GET" => Ok(json(200, &json!({ "runtime": app.host.current() }))),
        "PUT" => {
            let runtime = app.host.report(&read_json(request).await?)?;
            Ok(json(200, &json!({ "runtime": runtime })))
        }
        _ => method_not_allowed(&["GET", "PUT"]),
    }
}

pub async fn codex_thread_progress(State(app): State<Arc<App>>, request: Request) -> Reply {
    if request.method() != "GET" {
        return method_not_allowed(&["GET"]);
    }
    let query = query(request.uri());
    if query.iter().any(|(key, _)| key != "threadId") {
        return Err(ApiError::new(400, "UNKNOWN_QUERY_PARAMETER", "Only 'threadId' is supported").into());
    }
    let mut thread_ids: Vec<&str> = Vec::new();
    for (_, value) in &query {
        let id = strip_thread_prefix(value);
        if !thread_ids.contains(&id) {
            thread_ids.push(id);
        }
    }
    if thread_ids.len() > 64 || !thread_ids.iter().all(|id| is_thread_id(id)) {
        return Err(ApiError::invalid_field("'threadId' must contain valid Codex thread IDs").into());
    }
    let mut progress = Map::new();
    for id in thread_ids {
        let state = app.codex_sessions.state(id).await.map_err(broadcast_domain::Error::from)?;
        progress.insert(id.to_owned(), state);
    }
    Ok(json(200, &json!({ "progress": progress })))
}

fn session(config: &CloudConfig) -> Value {
    match (&config.remote_url, &config.actor_name) {
        (Some(remote_url), actor_name) => {
            json!({ "mode": "cloud", "remoteUrl": remote_url, "actorName": actor_name, "authenticated": true })
        }
        (None, _) => json!({ "mode": "local", "authenticated": false }),
    }
}

pub async fn cloud_session(State(app): State<Arc<App>>, request: Request) -> Reply {
    reject_query(request.uri(), "Cloud session routes do not accept query parameters")?;
    match request.method().as_str() {
        "GET" => Ok(json(200, &session(&app.cloud.read().await.map_err(broadcast_domain::Error::from)?))),
        "PUT" => {
            let body = read_json(request).await?;
            let body = fields::object(&body, &["remoteUrl", "actorName", "sharedKey"])?;
            let config = app
                .cloud
                .configure(body.get("remoteUrl"), body.get("actorName"), body.get("sharedKey"))
                .await
                .map_err(|error| ApiError::new(400, error.code, error.message))?;
            Ok(json(200, &session(&config)))
        }
        "DELETE" => {
            app.cloud.clear().await.map_err(broadcast_domain::Error::from)?;
            Ok(json(200, &json!({ "mode": "local", "authenticated": false })))
        }
        _ => method_not_allowed(&["GET", "PUT", "DELETE"]),
    }
}

pub async fn project_mapping(State(app): State<Arc<App>>, request: Request) -> Reply {
    if request.method() != "PUT" {
        return method_not_allowed(&["PUT"]);
    }
    reject_query(request.uri(), "Project mapping routes do not accept query parameters")?;
    let id = decode_uri_component(segment(request.uri(), 4))
        .ok_or_else(|| ApiError::new(400, "INVALID_PATH", "Project id contains invalid encoding"))?;
    let project_id = fields::project_id(Some(&Value::String(id)))?;
    let body = read_json(request).await?;
    let body = fields::object(&body, &["workspacePath"])?;
    let workspace = fields::path_field(body.get("workspacePath"), "workspacePath")?
        .filter(|path| Path::new(path).is_absolute())
        .ok_or_else(|| ApiError::invalid_field("'workspacePath' must be absolute"))?;
    app.cloud.set_project_workspace(&project_id, &workspace).await.map_err(broadcast_domain::Error::from)?;
    Ok(json(200, &json!({ "projectId": project_id, "workspacePath": workspace })))
}
