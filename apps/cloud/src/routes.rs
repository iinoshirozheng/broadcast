//! `/api/*` routing for the cloud board, in the JavaScript worker's order and wording.

use broadcast_domain::input::{self, Query};
use broadcast_domain::{ApiError, fields, js};
use serde_json::{Value, json};
use worker::{D1Database, Method, Request, Response, Url};

use crate::attachments::Owner;
use crate::board::{Board, Cursor};
use crate::db::{self, value};
use crate::error::Result;
use crate::http::{self, JSON_BODY_LIMIT, JSON_BODY_TOO_LARGE, method_not_allowed};
use crate::hub;

const PROJECT_README_BODY_LIMIT: usize = 3 * 1024 * 1024;
const MAX_SAFE_INTEGER: u64 = 9_007_199_254_740_991;

fn require_no_query(query: &Query, route: &str) -> Result<()> {
    if query.is_empty() {
        Ok(())
    } else {
        Err(ApiError::new(
            400,
            "UNKNOWN_QUERY_PARAMETER",
            format!("{route} does not accept query parameters"),
        )
        .into())
    }
}

/// `decodePathPart`.
fn path_part(raw: &str, label: &str) -> Result<String> {
    let decoded = js::decode_uri_component(raw).ok_or_else(|| {
        ApiError::new(
            400,
            "INVALID_PATH",
            format!("{label} contains invalid encoding"),
        )
    })?;
    let length = js::len(&decoded);
    if length == 0 || length > 128 {
        return Err(ApiError::new(400, "INVALID_PATH", format!("{label} is invalid")).into());
    }
    Ok(decoded)
}

fn project_id(raw: &str) -> Result<String> {
    Ok(fields::project_id(Some(&Value::String(path_part(
        raw,
        "Project id",
    )?)))?)
}

/// A decimal string that `Number()` reads as a non-negative safe integer.
fn decimal(value: &str) -> Option<i64> {
    if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    value
        .parse::<u64>()
        .ok()
        .filter(|number| *number <= MAX_SAFE_INTEGER)
        .and_then(|number| i64::try_from(number).ok())
}

/// `parseAfterCursor`.
fn after_cursor(query: &Query) -> Result<Option<Cursor>> {
    if let Some((key, _)) = query.iter().find(|(key, _)| key != "after") {
        return Err(ApiError::new(
            400,
            "UNKNOWN_QUERY_PARAMETER",
            format!("Unknown query parameter: {key}"),
        )
        .into());
    }
    match query {
        [] => Ok(None),
        [(_, value)] => {
            let revision = decimal(value).ok_or_else(|| {
                ApiError::new(
                    400,
                    "INVALID_CURSOR",
                    "'after' must be a non-negative decimal integer",
                )
            })?;
            Ok(Some(Cursor {
                value: value.clone(),
                revision,
            }))
        }
        _ => Err(ApiError::new(400, "INVALID_CURSOR", "'after' must be provided once").into()),
    }
}

/// `parseClientStorageUpdate`.
fn client_storage_update(body: &Value) -> Result<Value> {
    let body = fields::object(body, &["key", "value"])?;
    let key = fields::required_string(body.get("key"), "key", 512)?;
    if !key.starts_with(hub::DISPLAY_SETTINGS_PREFIX)
        || key.len() == hub::DISPLAY_SETTINGS_PREFIX.len()
    {
        return Err(
            ApiError::invalid_field("'key' must identify project board display settings").into(),
        );
    }
    let value = fields::nullable_string(body.get("value"), "value", 100_000)?.flatten();
    Ok(json!({ "key": key, "value": value }))
}

/// `readGlobalRevision`.
pub async fn global_revision(db: &D1Database) -> Result<Value> {
    let row = db::first(
        db,
        "SELECT revision FROM global_revision WHERE singleton = 1",
        vec![],
    )
    .await?;
    Ok(row.map_or(Value::Null, |row| value(&row, "revision")))
}

async fn revisions(board: &Board, query: &Query) -> Result<Response> {
    if let Some((key, _)) = query.iter().find(|(key, _)| key != "since") {
        return Err(ApiError::new(
            400,
            "UNKNOWN_QUERY_PARAMETER",
            format!("Unknown query parameter: {key}"),
        )
        .into());
    }
    let [(_, raw)] = query else {
        return Err(ApiError::new(
            400,
            "INVALID_QUERY_PARAMETER",
            "'since' must be provided once",
        )
        .into());
    };
    let since = decimal(raw).ok_or_else(|| {
        ApiError::new(
            400,
            "INVALID_QUERY_PARAMETER",
            "'since' must be a non-negative integer",
        )
    })?;
    let revision = global_revision(&board.db).await?;
    let changed = revision
        .as_f64()
        .is_some_and(|revision| revision > since as f64);
    http::json(200, &json!({ "changed": changed, "revision": revision }))
}

async fn client_storage(board: &Board, request: &mut Request, query: &Query) -> Result<Response> {
    require_no_query(query, "/api/client-storage")?;
    match request.method() {
        Method::Get => Ok(hub::send(&board.env, Method::Get, "/client-storage", None).await?),
        Method::Patch => {
            let update = client_storage_update(
                &http::read_json(request, JSON_BODY_LIMIT, JSON_BODY_TOO_LARGE).await?,
            )?;
            let response =
                hub::send(&board.env, Method::Patch, "/client-storage", Some(&update)).await?;
            if (200..300).contains(&response.status_code()) {
                db::run(
                    &board.db,
                    "UPDATE global_revision SET revision = revision + 1 WHERE singleton = 1",
                    vec![],
                )
                .await?;
            }
            Ok(response)
        }
        _ => method_not_allowed(&["GET", "PATCH"]),
    }
}

async fn body(request: &mut Request) -> Result<Value> {
    http::read_json(request, JSON_BODY_LIMIT, JSON_BODY_TOO_LARGE).await
}

/// `routeApi`.
pub async fn route(board: &Board, request: &mut Request, url: &Url) -> Result<Response> {
    let query: Vec<(String, String)> = url.query_pairs().into_owned().collect();
    let query = query.as_slice();
    let method = request.method();
    let parts: Vec<&str> = url
        .path()
        .strip_prefix("/api/")
        .unwrap_or_default()
        .split('/')
        .collect();
    if parts.iter().any(|part| part.is_empty()) {
        return Err(ApiError::new(404, "NOT_FOUND", "API route not found").into());
    }
    match parts.as_slice() {
        ["meta"] => {
            if method != Method::Get {
                return method_not_allowed(&["GET"]);
            }
            require_no_query(query, "GET /api/meta")?;
            http::json(
                200,
                &json!({
                    "mode": "cloud",
                    "manageBroadcastSkillPath": null,
                    "realtime": { "transport": "websocket", "endpoint": "/api/events" },
                    "localCapabilities": { "available": false },
                }),
            )
        }
        ["client-storage"] => client_storage(board, request, query).await,
        ["revisions"] => {
            if method != Method::Get {
                return method_not_allowed(&["GET"]);
            }
            revisions(board, query).await
        }
        ["device-workspaces"] | ["projects", _, "development-contexts"] => {
            if method != Method::Get {
                return method_not_allowed(&["GET"]);
            }
            Err(ApiError::new(
                409,
                "LOCAL_COMPANION_REQUIRED",
                "This capability requires the local Codex companion",
            )
            .into())
        }
        ["events"] => {
            if method != Method::Get {
                return method_not_allowed(&["GET"]);
            }
            require_no_query(query, "GET /api/events")?;
            let upgrade = request.headers().get("upgrade")?.unwrap_or_default();
            if !upgrade.eq_ignore_ascii_case("websocket") {
                return Err(ApiError::new(
                    426,
                    "WEBSOCKET_REQUIRED",
                    "A WebSocket upgrade is required",
                )
                .into());
            }
            Ok(hub::connect(&board.env, request).await?)
        }
        ["projects"] => match method {
            Method::Get => {
                require_no_query(query, "GET /api/projects")?;
                http::json(200, &json!({ "projects": board.list_projects().await? }))
            }
            Method::Post => {
                let project = board
                    .create_project(&input::project_create(&body(request).await?)?)
                    .await?;
                http::json(201, &json!({ "project": project }))
            }
            _ => method_not_allowed(&["GET", "POST"]),
        },
        ["projects", id] => {
            require_no_query(query, "Project routes")?;
            let id = project_id(id)?;
            if method != Method::Delete {
                return method_not_allowed(&["DELETE"]);
            }
            board.delete_project(&id).await?;
            http::empty(204)
        }
        ["projects", id, "labels"] => {
            require_no_query(query, "Project label routes")?;
            let id = project_id(id)?;
            if method != Method::Post && method != Method::Delete {
                return method_not_allowed(&["POST", "DELETE"]);
            }
            let label = fields::project_label(&body(request).await?)?;
            let project = if method == Method::Post {
                board.add_project_label(&id, &label).await?
            } else {
                board.delete_project_label(&id, &label).await?
            };
            http::json(200, &json!({ "project": project }))
        }
        ["projects", id, "readme", "attachments"] => {
            require_no_query(query, "Project README attachment routes")?;
            let id = project_id(id)?;
            if method != Method::Post {
                return method_not_allowed(&["POST"]);
            }
            let attachment = board.upload_project_readme_attachment(&id, request).await?;
            http::json(201, &json!({ "attachment": attachment }))
        }
        ["projects", id, "readme"] => {
            require_no_query(query, "Project README routes")?;
            let id = project_id(id)?;
            match method {
                Method::Get => http::json(
                    200,
                    &json!({ "readme": board.get_project_readme(&id).await? }),
                ),
                Method::Put => {
                    let body = http::read_json(
                        request,
                        PROJECT_README_BODY_LIMIT,
                        "Project README request cannot exceed 3 MiB",
                    )
                    .await?;
                    let readme = board
                        .save_project_readme(&id, &input::cloud_project_readme_save(&body)?)
                        .await?;
                    http::json(200, &json!({ "readme": readme }))
                }
                _ => method_not_allowed(&["GET", "PUT"]),
            }
        }
        ["tasks"] => match method {
            Method::Get => {
                let tasks = board.list_tasks(&input::cloud_task_filters(query)?).await?;
                http::json(200, &json!({ "tasks": tasks }))
            }
            Method::Post => {
                let input =
                    input::task_create(&body(request).await?, input::cloud_development_context)?;
                http::json(201, &json!({ "task": board.create_task(&input).await? }))
            }
            _ => method_not_allowed(&["GET", "POST"]),
        },
        ["tasks", id, "tree"] => {
            if method != Method::Get {
                return method_not_allowed(&["GET"]);
            }
            let id = path_part(id, "Task id")?;
            let tree = input::cloud_task_tree_query(query)?;
            http::json(
                200,
                &json!({ "tree": board.get_task_tree(&id, tree.direction, tree.depth).await? }),
            )
        }
        ["tasks", id, "relations", kind, related] => {
            require_no_query(query, "Issue relation routes")?;
            let id = path_part(id, "Task id")?;
            let kind = path_part(kind, "Relation type")?;
            let related = path_part(related, "Related task id")?;
            let input = input::relation_mutation(&body(request).await?)?;
            match method {
                Method::Post => http::json(
                    200,
                    &board.add_relation(&id, &kind, &related, &input).await?,
                ),
                Method::Delete => http::json(
                    200,
                    &board.remove_relation(&id, &kind, &related, &input).await?,
                ),
                _ => method_not_allowed(&["POST", "DELETE"]),
            }
        }
        ["tasks", id, "activities"] => {
            require_no_query(query, "Activity routes")?;
            let id = path_part(id, "Task id")?;
            if method != Method::Get {
                return method_not_allowed(&["GET"]);
            }
            http::json(
                200,
                &json!({ "activities": board.list_task_activities(&id).await? }),
            )
        }
        ["tasks", id, "comments"] => {
            let id = path_part(id, "Task id")?;
            if method == Method::Get {
                return http::json(
                    200,
                    &board
                        .list_comments(&id, after_cursor(query)?.as_ref())
                        .await?,
                );
            }
            require_no_query(query, "Comment routes")?;
            if method != Method::Post {
                return method_not_allowed(&["GET", "POST"]);
            }
            let comment = board
                .create_comment(&id, &input::comment_create(&body(request).await?)?)
                .await?;
            http::json(201, &json!({ "comment": comment }))
        }
        ["comments", id, "attachments"] => {
            let id = path_part(id, "Comment id")?;
            if method == Method::Get {
                return http::json(
                    200,
                    &board
                        .list_comment_attachments(&id, after_cursor(query)?.as_ref())
                        .await?,
                );
            }
            require_no_query(query, "Attachment routes")?;
            if method != Method::Post {
                return method_not_allowed(&["GET", "POST"]);
            }
            let attachment = board
                .upload_attachment(Owner::Comment, &id, request)
                .await?;
            http::json(201, &json!({ "attachment": attachment }))
        }
        ["comments", id] => {
            require_no_query(query, "Comment routes")?;
            let id = path_part(id, "Comment id")?;
            match method {
                Method::Patch => {
                    let comment = board
                        .update_comment(&id, &input::comment_patch(&body(request).await?)?)
                        .await?;
                    http::json(200, &json!({ "comment": comment }))
                }
                Method::Delete => {
                    let version = input::version_mutation(&body(request).await?)?.version;
                    board.delete_comment(&id, version).await?;
                    http::empty(204)
                }
                _ => method_not_allowed(&["PATCH", "DELETE"]),
            }
        }
        ["tasks", id, "attachments"] => {
            let id = path_part(id, "Task id")?;
            if method == Method::Get {
                return http::json(
                    200,
                    &board
                        .list_task_attachments(&id, after_cursor(query)?.as_ref())
                        .await?,
                );
            }
            require_no_query(query, "Attachment routes")?;
            if method != Method::Post {
                return method_not_allowed(&["GET", "POST"]);
            }
            let attachment = board.upload_attachment(Owner::Task, &id, request).await?;
            http::json(201, &json!({ "attachment": attachment }))
        }
        ["attachments", id, action @ ("content" | "download")] => {
            require_no_query(query, "Attachment routes")?;
            if method != Method::Get && method != Method::Head {
                return method_not_allowed(&["GET", "HEAD"]);
            }
            let id = path_part(id, "Attachment id")?;
            board
                .attachment_content(&id, method == Method::Head, *action == "download")
                .await
        }
        ["attachments", id] => {
            require_no_query(query, "Attachment routes")?;
            if method != Method::Delete {
                return method_not_allowed(&["DELETE"]);
            }
            board
                .delete_attachment(&path_part(id, "Attachment id")?)
                .await?;
            http::empty(204)
        }
        ["tasks", id] => {
            let id = path_part(id, "Task id")?;
            require_no_query(query, "Task routes")?;
            match method {
                Method::Get => {
                    let task = board.get_task(&id).await?;
                    if task.is_null() {
                        return Err(ApiError::new(
                            404,
                            "TASK_NOT_FOUND",
                            format!("Task '{id}' does not exist"),
                        )
                        .into());
                    }
                    http::json(200, &json!({ "task": task }))
                }
                Method::Patch => {
                    let task = board
                        .update_task(&id, &input::cloud_task_patch(&body(request).await?)?)
                        .await?;
                    http::json(200, &json!({ "task": task }))
                }
                Method::Delete => {
                    let version = input::version_mutation(&body(request).await?)?.version;
                    board.delete_archived_task(&id, version).await?;
                    http::empty(204)
                }
                _ => method_not_allowed(&["GET", "PATCH", "DELETE"]),
            }
        }
        ["tasks", id, action @ ("archive" | "restore" | "move")] => {
            let id = path_part(id, "Task id")?;
            require_no_query(query, "Task routes")?;
            if method != Method::Post {
                return method_not_allowed(&["POST"]);
            }
            let body = body(request).await?;
            let task = match *action {
                "move" => board.move_task(&id, &input::task_move(&body)?).await?,
                "archive" => {
                    board
                        .archive_task(&id, &input::version_mutation(&body)?)
                        .await?
                }
                _ => {
                    board
                        .restore_task(&id, &input::version_mutation(&body)?)
                        .await?
                }
            };
            http::json(200, &json!({ "task": task }))
        }
        _ => Err(ApiError::new(404, "NOT_FOUND", "API route not found").into()),
    }
}
