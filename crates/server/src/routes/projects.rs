//! `/api/projects` and `/api/projects/:id`.

use std::sync::Arc;

use axum::extract::{Request, State};
use axum::http::StatusCode;
use broadcast_domain::constants::DEFAULT_PROJECT_ID;
use broadcast_domain::js::decode_uri_component;
use broadcast_domain::{ApiError, fields, input};
use serde_json::{Value, json};

use crate::App;
use crate::http::{Reply, empty, json, method_not_allowed, read_json, reject_query, segment};

pub async fn collection(State(app): State<Arc<App>>, request: Request) -> Reply {
    match request.method().as_str() {
        "GET" => {
            reject_query(request.uri(), "GET /api/projects does not accept query parameters")?;
            let mut projects = app.store(|store| store.list_projects()).await?;
            for project in &mut projects {
                if project.id == DEFAULT_PROJECT_ID {
                    project.workspace_path = None;
                }
            }
            Ok(json(200, &json!({ "projects": projects })))
        }
        "POST" => {
            let project = input::project_create(&read_json(request).await?)?;
            let project = app.store(move |store| store.create_project(&project)).await?;
            let body = json!({ "project": project });
            app.events.emit("project.created", body.clone());
            Ok(json(201, &body))
        }
        _ => method_not_allowed(&["GET", "POST"]),
    }
}

pub async fn item(State(app): State<Arc<App>>, request: Request) -> Reply {
    reject_query(request.uri(), "Project routes do not accept query parameters")?;
    let id = decode_uri_component(segment(request.uri(), 3))
        .ok_or_else(|| ApiError::new(400, "INVALID_PATH", "Project id contains invalid encoding"))?;
    let id = fields::project_id(Some(&Value::String(id)))?;
    if request.method() != "DELETE" {
        return method_not_allowed(&["DELETE"]);
    }
    app.store(move |store| store.delete_project(&id)).await?;
    Ok(empty(StatusCode::NO_CONTENT))
}
