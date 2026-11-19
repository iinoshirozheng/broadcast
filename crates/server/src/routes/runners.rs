//! `/api/runners`: the agent runners online on this board, and the heartbeat they send.

use std::sync::Arc;

use axum::extract::{Request, State};
use broadcast_domain::ApiError;
use broadcast_domain::records::Actor;
use serde_json::json;

use crate::App;
use crate::actor;
use crate::http::{Reply, json, method_not_allowed, reject_query};

pub async fn collection(State(app): State<Arc<App>>, request: Request) -> Reply {
    if request.method() != "GET" {
        return method_not_allowed(&["GET"]);
    }
    reject_query(
        request.uri(),
        "GET /api/runners does not accept query parameters",
    )?;
    Ok(json(200, &json!({ "runners": app.runners.online() })))
}

/// `PUT /api/runners/claude` or `/api/runners/codex`: only that agent's runner keeps it online.
pub async fn heartbeat(State(app): State<Arc<App>>, request: Request) -> Reply {
    if request.method() != "PUT" {
        return method_not_allowed(&["PUT"]);
    }
    let expected = match request.uri().path().rsplit('/').next() {
        Some("claude") => Actor::claude_agent(),
        Some("codex") => Actor::codex_agent(),
        _ => return Err(ApiError::new(404, "NOT_FOUND", "Unknown runner").into()),
    };
    let agent = actor::from_headers(request.headers())?;
    if agent.id != expected.id {
        return Err(ApiError::new(
            403,
            "RUNNER_REQUIRED",
            format!("Only the {} runner sends this heartbeat", expected.name),
        )
        .into());
    }
    Ok(json(200, &json!({ "runner": app.runners.beat(&agent) })))
}
