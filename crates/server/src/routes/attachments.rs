//! Task, comment and README attachment uploads, downloads and deletes.

use std::io::ErrorKind;
use std::path::PathBuf;
use std::sync::Arc;

use axum::body::{Body, Bytes};
use axum::extract::{Request, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::Response;
use broadcast_domain::{ApiError, Error, Result, attachment};
use broadcast_store::NewAttachment;
use serde_json::json;
use tokio::io::AsyncWriteExt;
use uuid::Uuid;

use crate::App;
use crate::http::{
    AttachmentHeaders, Reply, after_cursor, attachment_headers, empty, json, method_not_allowed, next_cursor, path_id,
    read_body, reject_query, segment,
};

const BODY_LIMIT: usize = 25 * 1024 * 1024;
const TOO_LARGE: &str = "Attachment cannot exceed 25 MiB";
fn directory(app: &App) -> PathBuf {
    app.config.data_dir.join("attachments")
}

fn not_found(id: &str) -> ApiError {
    ApiError::new(404, "ATTACHMENT_NOT_FOUND", format!("Attachment '{id}' does not exist"))
}

/// An uploaded file written to disk under a fresh id, before its record exists.
pub struct Upload {
    pub id: String,
    pub path: PathBuf,
    pub headers: AttachmentHeaders,
    pub size: i64,
}

impl Upload {
    /// Removes the file when its record could not be written.
    pub async fn discard_on_error<T>(&self, result: Result<T>) -> Result<T> {
        if result.is_err() {
            tokio::fs::remove_file(&self.path).await?;
        }
        result
    }
}

/// Reads the body and writes it to `attachments/<uuid>` (create-new).
pub async fn store_upload(app: &App, headers: AttachmentHeaders, request: Request) -> Result<Upload> {
    let body: Bytes = read_body(request, BODY_LIMIT, TOO_LARGE).await?;
    let id = Uuid::new_v4().to_string();
    let directory = directory(app);
    tokio::fs::create_dir_all(&directory).await?;
    let path = directory.join(&id);
    let mut file = tokio::fs::OpenOptions::new().write(true).create_new(true).open(&path).await?;
    file.write_all(&body).await?;
    file.flush().await?;
    Ok(Upload { id, path, headers, size: i64::try_from(body.len())? })
}

pub async fn for_task(State(app): State<Arc<App>>, request: Request) -> Reply {
    let task_id = path_id(request.uri(), 3, "Task")?;
    if request.method() == "GET" {
        let after = after_cursor(request.uri(), "Attachment routes")?;
        let revision = after.as_ref().map(|cursor| cursor.revision);
        let attachments = app.store(move |store| store.list_attachments(&task_id, revision)).await?;
        let cursor = next_cursor(attachments.iter().map(|attachment| attachment.change_revision), after.as_ref());
        return Ok(json(200, &json!({ "attachments": attachments, "nextCursor": cursor })));
    }
    reject_query(request.uri(), "Attachment routes do not accept query parameters")?;
    if request.method() != "POST" {
        return method_not_allowed(&["GET", "POST"]);
    }
    let lookup = task_id.clone();
    let task = app
        .store(move |store| store.get_task(&lookup))
        .await?
        .ok_or_else(|| ApiError::new(404, "TASK_NOT_FOUND", format!("Task '{task_id}' does not exist")))?;
    let headers = attachment_headers(request.headers())?;
    let upload = store_upload(&app, headers, request).await?;
    let record = {
        let task_id = task_id.clone();
        let upload_record = owned(&upload);
        app.store(move |store| store.create_attachment(&task_id, &upload_record.borrowed())).await
    };
    let attachment = upload.discard_on_error(record).await?;
    app.events.emit("attachment.created", json!({ "attachment": attachment, "task": task }));
    Ok(json(201, &json!({ "attachment": attachment })))
}

pub async fn for_comment(State(app): State<Arc<App>>, request: Request) -> Reply {
    let comment_id = path_id(request.uri(), 3, "Comment")?;
    if request.method() == "GET" {
        let after = after_cursor(request.uri(), "Attachment routes")?;
        let revision = after.as_ref().map(|cursor| cursor.revision);
        let attachments = app.store(move |store| store.list_comment_attachments(&comment_id, revision)).await?;
        let cursor = next_cursor(attachments.iter().map(|attachment| attachment.change_revision), after.as_ref());
        return Ok(json(200, &json!({ "attachments": attachments, "nextCursor": cursor })));
    }
    reject_query(request.uri(), "Attachment routes do not accept query parameters")?;
    if request.method() != "POST" {
        return method_not_allowed(&["GET", "POST"]);
    }
    let lookup = comment_id.clone();
    let comment = app.store(move |store| store.get_comment(&lookup)).await?.ok_or_else(|| {
        ApiError::new(404, "COMMENT_NOT_FOUND", format!("Comment '{comment_id}' does not exist"))
    })?;
    let headers = attachment_headers(request.headers())?;
    let upload = store_upload(&app, headers, request).await?;
    let record = {
        let comment_id = comment_id.clone();
        let upload_record = owned(&upload);
        app.store(move |store| store.create_comment_attachment(&comment_id, &upload_record.borrowed())).await
    };
    let attachment = upload.discard_on_error(record).await?;
    let (comment, task) = app
        .store(move |store| Ok((store.get_comment(&comment_id)?, store.get_task(&comment.task_id)?)))
        .await?;
    app.events.emit("attachment.created", json!({ "attachment": attachment, "comment": comment, "task": task }));
    Ok(json(201, &json!({ "attachment": attachment })))
}

/// `GET|HEAD /api/attachments/:id/(content|download)`.
pub async fn content(State(app): State<Arc<App>>, request: Request) -> Reply {
    let id = path_id(request.uri(), 3, "Attachment")?;
    let open_inline = segment(request.uri(), 4) == "content";
    reject_query(request.uri(), "Attachment routes do not accept query parameters")?;
    let head = request.method() == "HEAD";
    if !head && request.method() != "GET" {
        return method_not_allowed(&["GET", "HEAD"]);
    }
    let lookup = id.clone();
    let (stored_id, filename, content_type) = app
        .store(move |store| {
            if let Some(attachment) = store.get_attachment(&lookup)? {
                return Ok(Some((attachment.id, attachment.filename, attachment.content_type)));
            }
            Ok(store
                .get_project_readme_attachment(&lookup)?
                .map(|attachment| (attachment.id, attachment.filename, attachment.content_type)))
        })
        .await?
        .ok_or_else(|| not_found(&id))?;
    let body = tokio::fs::read(directory(&app).join(&stored_id)).await.map_err(Error::from)?;
    let (disposition, media_type) = attachment::content_headers(&filename, &content_type, open_inline);
    let length = body.len();
    let mut response = Response::new(if head { Body::empty() } else { Body::from(body) });
    let headers = response.headers_mut();
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("private, no-store"));
    headers.insert(header::CONTENT_DISPOSITION, HeaderValue::from_str(&disposition).map_err(Error::from)?);
    headers.insert(header::CONTENT_LENGTH, HeaderValue::from(length));
    headers.insert(header::CONTENT_SECURITY_POLICY, HeaderValue::from_static("sandbox; default-src 'none'"));
    headers.insert(header::CONTENT_TYPE, HeaderValue::from_str(media_type).map_err(Error::from)?);
    Ok(response)
}

/// `DELETE /api/attachments/:id`.
pub async fn item(State(app): State<Arc<App>>, request: Request) -> Reply {
    let id = path_id(request.uri(), 3, "Attachment")?;
    reject_query(request.uri(), "Attachment routes do not accept query parameters")?;
    if request.method() != "DELETE" {
        return method_not_allowed(&["DELETE"]);
    }
    let lookup = id.clone();
    let attachment = app.store(move |store| store.get_attachment(&lookup)).await?.ok_or_else(|| not_found(&id))?;
    match tokio::fs::remove_file(directory(&app).join(&attachment.id)).await {
        Err(error) if error.kind() != ErrorKind::NotFound => return Err(Error::from(error).into()),
        _ => {}
    }
    let task_id = attachment.task_id.clone();
    let task = app
        .store(move |store| {
            store.delete_attachment(&id)?;
            store.get_task(&task_id)
        })
        .await?;
    app.events.emit("attachment.deleted", json!({ "attachment": attachment, "task": task }));
    Ok(empty(StatusCode::NO_CONTENT))
}

/// An owned copy of an upload's record, movable onto the blocking pool.
pub struct OwnedRecord {
    id: String,
    kind: &'static str,
    filename: String,
    content_type: String,
    size: i64,
}

impl OwnedRecord {
    pub fn borrowed(&self) -> NewAttachment<'_> {
        NewAttachment {
            id: &self.id,
            kind: self.kind,
            filename: &self.filename,
            content_type: &self.content_type,
            size: self.size,
        }
    }
}

pub fn owned(upload: &Upload) -> OwnedRecord {
    OwnedRecord {
        id: upload.id.clone(),
        kind: upload.headers.kind,
        filename: upload.headers.filename.clone(),
        content_type: upload.headers.content_type.clone(),
        size: upload.size,
    }
}
