//! Route table. Handlers match methods themselves so the 405 body and `allow` header match the JS API.

mod attachments;
mod client_storage;
mod comments;
mod jira;
mod projects;
mod readme;
mod relations;
mod runners;
mod specs;
mod tasks;

use std::sync::Arc;

use axum::Router;
use axum::extract::{Request, State};
use axum::routing::any;
use broadcast_domain::ApiError;
use serde_json::json;

use crate::App;
use crate::http::{Reply, json, method_not_allowed, reject_query};
use crate::static_files;

pub fn router(app: Arc<App>) -> Router {
    Router::new()
        .route("/health", any(health))
        .route("/api/events", any(events))
        .route("/api/projects", any(projects::collection))
        .route("/api/projects/{id}", any(projects::item))
        .route("/api/projects/{id}/labels", any(readme::labels))
        .route("/api/projects/{id}/readme", any(readme::readme))
        .route(
            "/api/projects/{id}/readme/attachments",
            any(readme::readme_attachments),
        )
        .route("/api/tasks", any(tasks::collection))
        .route(
            "/api/tasks/{id}",
            any(|State(app), request| tasks::item(app, request, None)),
        )
        .route(
            "/api/tasks/{id}/move",
            any(|State(app), request| tasks::item(app, request, Some("move"))),
        )
        .route(
            "/api/tasks/{id}/archive",
            any(|State(app), request| tasks::item(app, request, Some("archive"))),
        )
        .route(
            "/api/tasks/{id}/restore",
            any(|State(app), request| tasks::item(app, request, Some("restore"))),
        )
        .route("/api/tasks/{id}/comments", any(comments::for_task))
        .route("/api/tasks/{id}/attachments", any(attachments::for_task))
        .route("/api/tasks/{id}/activities", any(relations::activities))
        .route("/api/tasks/{id}/tree", any(relations::tree))
        .route("/api/tasks/{id}/spec", any(specs::spec))
        .route("/api/tasks/{id}/spec/tasks/{number}", any(specs::check))
        .route(
            "/api/tasks/{id}/relations/{kind}/{related}",
            any(relations::relation),
        )
        .route("/api/comments/{id}", any(comments::item))
        .route(
            "/api/comments/{id}/attachments",
            any(attachments::for_comment),
        )
        .route("/api/attachments/{id}", any(attachments::item))
        .route("/api/attachments/{id}/content", any(attachments::content))
        .route("/api/attachments/{id}/download", any(attachments::content))
        .route("/api/client-storage", any(client_storage::client_storage))
        .route("/api/runners", any(runners::collection))
        .route("/api/runners/{agent}", any(runners::heartbeat))
        .route("/api/local/jira-connection", any(jira::connection))
        .route("/api/local/jira-connection/sync", any(jira::sync))
        .fallback(fallback)
        .with_state(app)
}

async fn health(State(app): State<Arc<App>>, request: Request) -> Reply {
    if request.method() != "GET" {
        return method_not_allowed(&["GET"]);
    }
    Ok(json(
        200,
        &json!({ "status": "ok", "product": "broadcast", "version": app.config.version }),
    ))
}

async fn events(State(app): State<Arc<App>>, request: Request) -> Reply {
    if request.method() != "GET" {
        return method_not_allowed(&["GET"]);
    }
    reject_query(
        request.uri(),
        "GET /api/events does not accept query parameters",
    )?;
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
