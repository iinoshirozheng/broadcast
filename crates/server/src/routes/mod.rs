//! Route table. Handlers match methods themselves so the 405 body and `allow` header match the JS API.

mod projects;
mod tasks;

use std::sync::Arc;

use axum::Router;
use axum::extract::{Request, State};
use axum::middleware;
use axum::routing::any;
use broadcast_domain::ApiError;
use serde_json::json;

use crate::App;
use crate::http::{Reply, json, method_not_allowed, reject_query};
use crate::{static_files, trust};

pub fn router(app: Arc<App>) -> Router {
    Router::new()
        .route("/health", any(health))
        .route("/api/events", any(events))
        .route("/api/projects", any(projects::collection))
        .route("/api/projects/{id}", any(projects::item))
        .route("/api/tasks", any(tasks::collection))
        .route("/api/tasks/{id}", any(|State(app), request| tasks::item(app, request, None)))
        .route("/api/tasks/{id}/move", any(|State(app), request| tasks::item(app, request, Some("move"))))
        .route("/api/tasks/{id}/archive", any(|State(app), request| tasks::item(app, request, Some("archive"))))
        .route("/api/tasks/{id}/restore", any(|State(app), request| tasks::item(app, request, Some("restore"))))
        .fallback(fallback)
        .layer(middleware::from_fn_with_state(Arc::clone(&app), trust::guard))
        .with_state(app)
}

async fn health(request: Request) -> Reply {
    if request.method() != "GET" {
        return method_not_allowed(&["GET"]);
    }
    Ok(json(200, &json!({ "status": "ok" })))
}

async fn events(State(app): State<Arc<App>>, request: Request) -> Reply {
    if request.method() != "GET" {
        return method_not_allowed(&["GET"]);
    }
    reject_query(request.uri(), "GET /api/events does not accept query parameters")?;
    Ok(app.events.connect())
}

async fn fallback(State(app): State<Arc<App>>, request: Request) -> Reply {
    let path = request.uri().path();
    if path.starts_with("/api/") {
        return Err(ApiError::new(404, "NOT_FOUND", "API route not found").into());
    }
    static_files::serve(&app.config.static_dir, request.method(), path)
        .await?
        .ok_or_else(|| ApiError::new(404, "NOT_FOUND", "Resource not found").into())
}
