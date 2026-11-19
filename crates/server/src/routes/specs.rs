//! `/api/tasks/:id/spec`: the spec an agent writes onto a task; the board only displays it.

use std::sync::Arc;

use axum::extract::{Request, State};
use axum::response::Response;
use broadcast_domain::input;
use broadcast_domain::records::Task;
use serde_json::json;

use crate::App;
use crate::http::{
    Reply, json, method_not_allowed, path_id, read_json, read_json_limit, reject_query,
};

const SPEC_LIMIT: usize = 3 * 1024 * 1024;

pub async fn spec(State(app): State<Arc<App>>, request: Request) -> Reply {
    let task_id = path_id(request.uri(), 3, "Task")?;
    reject_query(request.uri(), "Spec routes do not accept query parameters")?;
    match request.method().as_str() {
        "GET" => {
            let spec = app
                .store(move |store| store.get_task_spec(&task_id))
                .await?;
            Ok(json(200, &json!({ "spec": spec })))
        }
        "PATCH" => {
            let body =
                read_json_limit(request, SPEC_LIMIT, "Spec request cannot exceed 3 MiB").await?;
            let patch = input::task_spec_patch(&body)?;
            let task = app
                .store(move |store| store.patch_task_spec(&task_id, &patch))
                .await?;
            Ok(updated(&app, task))
        }
        _ => method_not_allowed(&["GET", "PATCH"]),
    }
}

/// `PUT /api/tasks/:id/spec/tasks/:number` with `{"done": bool}`.
pub async fn check(State(app): State<Arc<App>>, request: Request) -> Reply {
    let task_id = path_id(request.uri(), 3, "Task")?;
    let number = path_id(request.uri(), 6, "Spec task")?;
    reject_query(request.uri(), "Spec routes do not accept query parameters")?;
    if request.method() != "PUT" {
        return method_not_allowed(&["PUT"]);
    }
    let done = input::task_spec_check(&read_json(request).await?)?;
    let task = app
        .store(move |store| store.check_task_spec(&task_id, &number, done))
        .await?;
    Ok(updated(&app, task))
}

fn updated(app: &App, task: Task) -> Response {
    let body = json!({ "task": task });
    app.events.emit("task.updated", body.clone());
    json(200, &body)
}
