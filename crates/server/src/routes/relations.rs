//! `/api/tasks/:id/relations/:type/:related`, `/api/tasks/:id/activities` and `/api/tasks/:id/tree`.

use std::sync::Arc;

use axum::extract::{Request, State};
use broadcast_domain::js::{self, decode_uri_component};
use broadcast_domain::{ApiError, input};
use serde_json::json;

use crate::App;
use crate::actor;
use crate::http::{
    Reply, json, method_not_allowed, path_id, query, read_json, reject_query, segment,
};

/// `parseIssueRelationType`.
fn relation_kind(value: &str) -> Result<&'static str, ApiError> {
    match value {
        "parent" => Ok("parent"),
        "blocks" => Ok("blocks"),
        "blocked_by" => Ok("blocked_by"),
        "related" => Ok("related"),
        _ => Err(ApiError::invalid_field(
            "'relation type' must be parent, blocks, blocked_by, or related",
        )),
    }
}

pub async fn relation(State(app): State<Arc<App>>, request: Request) -> Reply {
    let uri = request.uri();
    let decode = |n: usize| {
        decode_uri_component(segment(uri, n)).ok_or_else(|| {
            ApiError::new(
                400,
                "INVALID_PATH",
                "Issue relation path contains invalid encoding",
            )
        })
    };
    let (task_id, kind, related_id) = (decode(3)?, decode(5)?, decode(6)?);
    let valid = |id: &str| !id.is_empty() && js::len(id) <= 128;
    if !valid(&task_id) || !valid(&related_id) {
        return Err(ApiError::new(400, "INVALID_PATH", "Issue relation task id is invalid").into());
    }
    reject_query(uri, "Issue relation routes do not accept query parameters")?;
    let kind = relation_kind(&kind)?;
    let remove = match request.method().as_str() {
        "POST" => false,
        "DELETE" => true,
        _ => return method_not_allowed(&["POST", "DELETE"]),
    };
    let actor = actor::from_headers(request.headers())?;
    let mut mutation = input::relation_mutation(&read_json(request).await?)?;
    mutation.attribution = app.host.resolve(mutation.attribution);
    let change = app
        .store(move |store| {
            if remove {
                store.remove_task_relation(&task_id, kind, &related_id, &mutation, &actor)
            } else {
                store.add_task_relation(&task_id, kind, &related_id, &mutation, &actor)
            }
        })
        .await?;
    let body = serde_json::to_value(&change).map_err(broadcast_domain::Error::from)?;
    app.events.emit("task.relation.updated", body.clone());
    Ok(json(200, &body))
}

pub async fn activities(State(app): State<Arc<App>>, request: Request) -> Reply {
    let task_id = path_id(request.uri(), 3, "Task")?;
    reject_query(
        request.uri(),
        "Activity routes do not accept query parameters",
    )?;
    if request.method() != "GET" {
        return method_not_allowed(&["GET"]);
    }
    let activities = app
        .store(move |store| store.list_task_activities(&task_id))
        .await?;
    Ok(json(200, &json!({ "activities": activities })))
}

pub async fn tree(State(app): State<Arc<App>>, request: Request) -> Reply {
    let id = path_id(request.uri(), 3, "Task")?;
    if request.method() != "GET" {
        return method_not_allowed(&["GET"]);
    }
    let tree_query = input::task_tree_query(&query(request.uri()))?;
    let tree = app
        .store(move |store| store.get_task_tree(&id, tree_query.direction, tree_query.depth))
        .await?;
    Ok(json(200, &json!({ "tree": tree })))
}
