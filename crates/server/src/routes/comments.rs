//! `/api/tasks/:id/comments` and `/api/comments/:id`.

use std::io::ErrorKind;
use std::sync::Arc;

use axum::extract::{Request, State};
use axum::http::StatusCode;
use broadcast_domain::{Error, input};
use serde_json::json;

use crate::App;
use crate::actor;
use crate::http::{
    Reply, after_cursor, empty, json, method_not_allowed, next_cursor, path_id, read_json,
    reject_query,
};

pub async fn for_task(State(app): State<Arc<App>>, request: Request) -> Reply {
    let task_id = path_id(request.uri(), 3, "Task")?;
    if request.method() == "GET" {
        let after = after_cursor(request.uri(), "Comment routes")?;
        let revision = after.as_ref().map(|cursor| cursor.revision);
        let comments = app
            .store(move |store| store.list_comments(&task_id, revision))
            .await?;
        let cursor = next_cursor(
            comments.iter().map(|comment| comment.change_revision),
            after.as_ref(),
        );
        return Ok(json(
            200,
            &json!({ "comments": comments, "nextCursor": cursor }),
        ));
    }
    reject_query(
        request.uri(),
        "Comment routes do not accept query parameters",
    )?;
    if request.method() != "POST" {
        return method_not_allowed(&["GET", "POST"]);
    }
    let actor = actor::from_headers(request.headers())?;
    let create = input::comment_create(&read_json(request).await?)?;
    let (comment, task) = app
        .store(move |store| {
            let comment = store.create_comment(&task_id, &create, &actor)?;
            Ok((comment, store.get_task(&task_id)?))
        })
        .await?;
    app.events.emit(
        "comment.created",
        json!({ "comment": comment, "task": task }),
    );
    Ok(json(201, &json!({ "comment": comment })))
}

pub async fn item(State(app): State<Arc<App>>, request: Request) -> Reply {
    let id = path_id(request.uri(), 3, "Comment")?;
    reject_query(
        request.uri(),
        "Comment routes do not accept query parameters",
    )?;
    match request.method().as_str() {
        "PATCH" => {
            let patch = input::comment_patch(&read_json(request).await?)?;
            let (comment, task) = app
                .store(move |store| {
                    let comment = store.update_comment(&id, &patch)?;
                    let task = store.get_task(&comment.task_id)?;
                    Ok((comment, task))
                })
                .await?;
            app.events.emit(
                "comment.updated",
                json!({ "comment": comment, "task": task }),
            );
            Ok(json(200, &json!({ "comment": comment })))
        }
        "DELETE" => {
            let mutation = input::version_mutation(&read_json(request).await?)?;
            let comment = app
                .store(move |store| store.delete_comment(&id, mutation.version))
                .await?;
            let directory = app.config.data_dir.join("attachments");
            for attachment in &comment.attachments {
                match tokio::fs::remove_file(directory.join(&attachment.id)).await {
                    Err(error) if error.kind() != ErrorKind::NotFound => {
                        return Err(Error::from(error).into());
                    }
                    _ => {}
                }
            }
            let task_id = comment.task_id.clone();
            let task = app.store(move |store| store.get_task(&task_id)).await?;
            app.events.emit(
                "comment.deleted",
                json!({ "comment": comment, "task": task }),
            );
            Ok(empty(StatusCode::NO_CONTENT))
        }
        _ => method_not_allowed(&["PATCH", "DELETE"]),
    }
}
