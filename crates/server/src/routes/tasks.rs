//! `/api/tasks`, `/api/tasks/:id` and its `move`, `archive` and `restore` actions.

use std::io::ErrorKind;
use std::sync::Arc;

use axum::extract::{Request, State};
use axum::http::StatusCode;
use broadcast_domain::constants::JIRA_PROJECT_ID;
use broadcast_domain::js::{self, decode_uri_component};
use broadcast_domain::records::Task;
use broadcast_domain::{ApiError, Error, input};
use serde_json::json;

use crate::App;
use crate::actor::{self, resolve_assignee};
use crate::http::{Reply, empty, json, method_not_allowed, query, read_json, reject_query, segment};
use crate::jira;

fn not_found(id: &str) -> ApiError {
    ApiError::new(404, "TASK_NOT_FOUND", format!("Task '{id}' does not exist"))
}

fn conflict(code: &'static str, message: &str) -> ApiError {
    ApiError::new(409, code, message)
}

/// Replies `200 { task }` and broadcasts it as `kind`.
fn changed(app: &App, kind: &'static str, task: Task) -> Reply {
    let body = json!({ "task": task });
    app.events.emit(kind, body.clone());
    Ok(json(200, &body))
}

pub async fn collection(State(app): State<Arc<App>>, request: Request) -> Reply {
    match request.method().as_str() {
        "GET" => {
            let filters = input::task_filters(&query(request.uri()))?;
            if filters.project_id.as_deref().is_none_or(|project| project.is_empty() || project == JIRA_PROJECT_ID) {
                jira::sync(&app, false).await?;
            }
            let tasks = app.store(move |store| store.list_tasks(&filters)).await?;
            Ok(json(200, &json!({ "tasks": tasks })))
        }
        "POST" => {
            let actor = actor::from_headers(request.headers())?;
            let mut input = input::task_create(&read_json(request).await?)?;
            input.attribution = app.host.resolve(input.attribution);
            if input.project_id == JIRA_PROJECT_ID {
                return Err(conflict(
                    "JIRA_CREATE_UNAVAILABLE",
                    "请在 Jira 中新建议题，Broadcast 当前只同步已分配给你的任务",
                )
                .into());
            }
            let assignee = resolve_assignee(input.assignee_target, &actor)?;
            let task = app.store(move |store| store.create_task(&input, &actor, &assignee)).await?;
            let body = json!({ "task": task });
            app.events.emit("task.created", body.clone());
            Ok(json(201, &body))
        }
        _ => method_not_allowed(&["GET", "POST"]),
    }
}

pub async fn item(app: Arc<App>, request: Request, action: Option<&'static str>) -> Reply {
    let id = decode_uri_component(segment(request.uri(), 3))
        .ok_or_else(|| ApiError::new(400, "INVALID_PATH", "Task id contains invalid encoding"))?;
    if id.is_empty() || js::len(&id) > 128 {
        return Err(ApiError::new(400, "INVALID_PATH", "Task id is invalid").into());
    }
    match (action, request.method().as_str()) {
        (None, "GET") => {
            reject_query(request.uri(), "GET /api/tasks/:id does not accept query parameters")?;
            let lookup = id.clone();
            let task = app.store(move |store| store.get_task(&lookup)).await?.ok_or_else(|| not_found(&id))?;
            Ok(json(200, &json!({ "task": task })))
        }
        (None, "PATCH") => patch(app, id, request).await,
        (None, "DELETE") => delete(app, id, request).await,
        (Some("move"), "POST") => move_task(app, id, request).await,
        (Some("archive"), "POST") => archive_or_restore(app, id, request, true).await,
        (Some("restore"), "POST") => archive_or_restore(app, id, request, false).await,
        (Some(_), _) => method_not_allowed(&["POST"]),
        (None, _) => method_not_allowed(&["GET", "PATCH", "DELETE"]),
    }
}

async fn source(app: &Arc<App>, id: &str) -> Result<Option<&'static str>, Error> {
    let id = id.to_owned();
    app.store(move |store| store.get_task_source(&id)).await
}

async fn patch(app: Arc<App>, id: String, request: Request) -> Reply {
    let actor = actor::from_headers(request.headers())?;
    let mut patch = input::task_patch(&read_json(request).await?)?;
    patch.attribution = app.host.resolve(patch.attribution);
    let source = source(&app, &id).await?.ok_or_else(|| not_found(&id))?;
    if source != "jira" && patch.changes.project_id.as_deref() == Some(JIRA_PROJECT_ID) {
        return Err(conflict("JIRA_PROJECT_MOVE_UNAVAILABLE", "本地任务不能移入 Jira 同步项目").into());
    }
    let mut jira_changed = false;
    if source == "jira" {
        let current = live_jira_task(&app, &id, patch.version, "Archived tasks cannot be updated").await?;
        if patch.changes.project_id.is_some() {
            return Err(conflict("JIRA_PROJECT_MOVE_UNAVAILABLE", "Jira 任务不能移到本地项目").into());
        }
        if patch.assignee_target.is_some() {
            return Err(conflict("JIRA_ASSIGNEE_UNAVAILABLE", "请在 Jira 中修改经办人").into());
        }
        let changes = &patch.changes;
        let due = changes.due_date.as_ref().map_or(current.due_date.is_some(), Option::is_some);
        let recurring = changes.recurrence.as_ref().map_or(current.recurrence.is_some(), Option::is_some);
        if recurring && !due {
            return Err(ApiError::invalid_field("A recurring issue requires a due date").into());
        }
        jira_changed = jira::update_task(&app, &current, changes).await?;
    }
    if patch.assignee_target.is_some() {
        patch.changes.assignee = Some(resolve_assignee(patch.assignee_target, &actor)?);
    }
    let updated = app
        .store(move |store| store.update_task(&id, patch.version, &patch.changes, &patch.attribution, &actor))
        .await;
    match updated {
        Ok(task) => changed(&app, "task.updated", task),
        // Jira already took the change, so re-read it rather than leave the board stale.
        Err(error) if jira_changed && jira::reconcile(&app).await.is_err() => {
            drop(error);
            Err(ApiError::new(502, "JIRA_RECONCILE_FAILED", "Jira 已更新，但 Broadcast 重新同步失败，请手动同步").into())
        }
        Err(error) => Err(error.into()),
    }
}

/// A Jira task about to change through Jira: the client must hold its current version and it must be live.
async fn live_jira_task(app: &Arc<App>, id: &str, version: i64, archived: &str) -> Result<Task, Error> {
    let lookup = id.to_owned();
    let current = app.store(move |store| store.get_task(&lookup)).await?.ok_or_else(|| not_found(id))?;
    if current.version != version {
        return Err(ApiError::new(409, "VERSION_CONFLICT", "Task changed since it was last read")
            .with_details(json!({ "expectedVersion": version, "actualVersion": current.version }))
            .into());
    }
    if current.archived_at.is_some() {
        return Err(conflict("TASK_ARCHIVED", archived).into());
    }
    Ok(current)
}

async fn delete(app: Arc<App>, id: String, request: Request) -> Reply {
    if source(&app, &id).await? == Some("jira") {
        return Err(conflict("JIRA_DELETE_UNAVAILABLE", "Jira 任务不能从 Broadcast 永久删除").into());
    }
    let mutation = input::version_mutation(&read_json(request).await?)?;
    let deleted = app.store(move |store| store.delete_archived_task(&id, mutation.version)).await?;
    let attachments = app.config.data_dir.join("attachments");
    for attachment_id in &deleted.attachment_ids {
        match tokio::fs::remove_file(attachments.join(attachment_id)).await {
            Err(error) if error.kind() != ErrorKind::NotFound => return Err(Error::from(error).into()),
            _ => {}
        }
    }
    app.events.emit("task.deleted", json!({ "task": deleted.task }));
    Ok(empty(StatusCode::NO_CONTENT))
}

async fn move_task(app: Arc<App>, id: String, request: Request) -> Reply {
    let headers = request.headers().clone();
    let mut movement = input::task_move(&read_json(request).await?)?;
    movement.attribution = app.host.resolve(movement.attribution);
    if source(&app, &id).await?.ok_or_else(|| not_found(&id))? == "jira" {
        let current = live_jira_task(&app, &id, movement.version, "Archived tasks cannot be moved").await?;
        jira::move_task(&app, &current, movement.status).await?;
    }
    let actor = actor::from_headers(&headers)?;
    let task = app.store(move |store| store.move_task(&id, &movement, &actor)).await?;
    changed(&app, "task.moved", task)
}

async fn archive_or_restore(app: Arc<App>, id: String, request: Request, archive: bool) -> Reply {
    if source(&app, &id).await? == Some("jira") {
        return Err(if archive {
            conflict("JIRA_ARCHIVE_UNAVAILABLE", "Jira 任务由同步范围自动管理，不能手动归档")
        } else {
            conflict("JIRA_RESTORE_UNAVAILABLE", "Jira 任务由同步范围自动管理，不能手动恢复")
        }
        .into());
    }
    let headers = request.headers().clone();
    let mut mutation = input::version_mutation(&read_json(request).await?)?;
    mutation.attribution = app.host.resolve(mutation.attribution);
    let actor = actor::from_headers(&headers)?;
    let task = app
        .store(move |store| {
            let input::VersionMutation { version, attribution } = &mutation;
            if archive {
                store.archive_task(&id, *version, attribution, &actor)
            } else {
                store.restore_task(&id, *version, attribution, &actor)
            }
        })
        .await?;
    changed(&app, if archive { "task.archived" } else { "task.restored" }, task)
}
