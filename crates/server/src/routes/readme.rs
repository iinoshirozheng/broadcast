//! `/api/projects/:id/labels`, `/api/projects/:id/readme` and `/api/projects/:id/readme/attachments`.

use std::sync::Arc;

use axum::extract::{Request, State};
use broadcast_domain::constants::JIRA_PROJECT_ID;
use broadcast_domain::js::decode_uri_component;
use broadcast_domain::{ApiError, Result, fields, input};
use serde_json::{Value, json};

use crate::App;
use crate::http::{Reply, attachment_headers, json, method_not_allowed, read_json, read_json_limit, reject_query, segment};
use crate::routes::attachments::{owned, store_upload};

const README_LIMIT: usize = 3 * 1024 * 1024;

fn project_id(request: &Request, routes: &str) -> Result<String> {
    reject_query(request.uri(), &format!("{routes} do not accept query parameters"))?;
    let id = decode_uri_component(segment(request.uri(), 3))
        .ok_or_else(|| ApiError::new(400, "INVALID_PATH", "Project id contains invalid encoding"))?;
    Ok(fields::project_id(Some(&Value::String(id)))?)
}

pub async fn labels(State(app): State<Arc<App>>, request: Request) -> Reply {
    let project_id = project_id(&request, "Project label routes")?;
    let remove = match request.method().as_str() {
        "POST" => false,
        "DELETE" => true,
        _ => return method_not_allowed(&["POST", "DELETE"]),
    };
    if remove && project_id == JIRA_PROJECT_ID {
        return Err(ApiError::new(
            409,
            "JIRA_LABEL_CATALOG_DELETE_UNAVAILABLE",
            "Jira 标签目录由同步管理，不能在 Broadcast 中删除",
        )
        .into());
    }
    let label = fields::project_label(&read_json(request).await?)?;
    let project = app
        .store(move |store| {
            if remove {
                store.delete_project_label(&project_id, &label)
            } else {
                store.add_project_label(&project_id, &label)
            }
        })
        .await?;
    app.events.emit("project.labels.updated", json!({ "project": project }));
    Ok(json(200, &json!({ "project": project })))
}

pub async fn readme(State(app): State<Arc<App>>, request: Request) -> Reply {
    let project_id = project_id(&request, "Project README routes")?;
    match request.method().as_str() {
        "GET" => {
            let readme = app.store(move |store| store.get_project_readme(&project_id)).await?;
            Ok(json(200, &json!({ "readme": readme })))
        }
        "PUT" => {
            let body = read_json_limit(request, README_LIMIT, "Project README request cannot exceed 3 MiB").await?;
            let save = input::project_readme_save(&body)?;
            let id = project_id.clone();
            let readme = app.store(move |store| store.save_project_readme(&id, &save.content, save.version)).await?;
            app.events.emit("project.readme.updated", json!({ "projectId": project_id, "readmeVersion": readme.version }));
            Ok(json(200, &json!({ "readme": readme })))
        }
        _ => method_not_allowed(&["GET", "PUT"]),
    }
}

pub async fn readme_attachments(State(app): State<Arc<App>>, request: Request) -> Reply {
    let project_id = project_id(&request, "Project README attachment routes")?;
    if request.method() != "POST" {
        return method_not_allowed(&["POST"]);
    }
    let headers = attachment_headers(request.headers())?;
    if headers.kind != "inline" {
        return Err(ApiError::new(400, "INVALID_ATTACHMENT_KIND", "Project README attachments must be inline").into());
    }
    let upload = store_upload(&app, headers, request).await?;
    let record = owned(&upload);
    let result = app
        .store(move |store| {
            let record = record.borrowed();
            store.create_project_readme_attachment(
                &project_id,
                record.id,
                record.filename,
                record.content_type,
                record.size,
            )
        })
        .await;
    let attachment = upload.discard_on_error(result).await?;
    Ok(json(201, &json!({ "attachment": attachment })))
}
