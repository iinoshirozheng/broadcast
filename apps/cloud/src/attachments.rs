//! Files attached to tasks, comments and project READMEs. Content lives in R2, metadata in D1.

use broadcast_domain::{ApiError, attachment};
use serde_json::{Value, json};
use worker::{Headers, HttpMetadata, Request, Response};

use crate::board::{Board, Cursor, next_cursor, now, uuid};
use crate::db::{self, NEXT_REVISION, args, text};
use crate::error::{Failure, Result};
use crate::http;
use crate::records;

pub enum Owner {
    Task,
    Comment,
}

fn not_found(id: &str) -> Failure {
    ApiError::new(404, "ATTACHMENT_NOT_FOUND", format!("Attachment '{id}' does not exist")).into()
}

impl Board {
    async fn put_object(&self, id: &str, body: Vec<u8>, content_type: &str) -> Result<()> {
        let metadata = HttpMetadata {
            content_type: Some(content_type.to_owned()),
            content_language: None,
            content_disposition: None,
            content_encoding: None,
            cache_control: None,
            cache_expiry: None,
        };
        self.env.bucket("ATTACHMENTS")?.put(id, body).http_metadata(metadata).execute().await?;
        Ok(())
    }

    /// Runs the metadata insert for freshly stored content, removing the content if it fails.
    async fn insert_stored(&self, id: &str, sql: String, values: Vec<worker::wasm_bindgen::JsValue>) -> Result<()> {
        if let Err(error) = db::run(&self.db, sql, values).await {
            self.delete_objects(&[id.to_owned()]).await?;
            return Err(error);
        }
        Ok(())
    }

    async fn attachment_page(&self, filter: &str, owner: &str, after: Option<&Cursor>) -> Result<Value> {
        let rows = match after {
            Some(after) => {
                db::all(
                    &self.db,
                    format!("SELECT * FROM attachments WHERE {filter} AND change_revision > ? ORDER BY change_revision"),
                    args![owner, after.revision],
                )
                .await?
            }
            None => {
                db::all(&self.db, format!("SELECT * FROM attachments WHERE {filter} ORDER BY created_at, id"), args![owner])
                    .await?
            }
        };
        let attachments: Vec<Value> = rows.iter().map(records::attachment).collect();
        Ok(json!({ "attachments": attachments, "nextCursor": next_cursor(&rows, after) }))
    }

    pub async fn list_task_attachments(&self, task_id: &str, after: Option<&Cursor>) -> Result<Value> {
        let task = self.require_task_row(task_id).await?;
        self.attachment_page("task_id = ? AND comment_id IS NULL", text(&task, "id"), after).await
    }

    pub async fn list_comment_attachments(&self, comment_id: &str, after: Option<&Cursor>) -> Result<Value> {
        self.require_comment_row(comment_id).await?;
        self.attachment_page("comment_id = ?", comment_id, after).await
    }

    pub async fn upload_attachment(&self, owner: Owner, owner_id: &str, request: &mut Request) -> Result<Value> {
        let (task_id, comment_id) = match owner {
            Owner::Task => (text(&self.require_task_row(owner_id).await?, "id").to_owned(), Value::Null),
            Owner::Comment => {
                let comment = self.require_comment_row(owner_id).await?;
                (text(&comment, "task_id").to_owned(), Value::from(text(&comment, "id")))
            }
        };
        let headers = http::attachment_headers(request)?;
        let body = http::read_attachment(request).await?;
        let id = uuid();
        let size = body.len();
        self.put_object(&id, body, &headers.content_type).await?;
        self.insert_stored(
            &id,
            format!(
                "INSERT INTO attachments (
                   id, task_id, comment_id, kind, filename, content_type, size, created_at, change_revision
                 ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, {NEXT_REVISION})"
            ),
            args![id, task_id, comment_id, headers.kind, headers.filename, headers.content_type, size, now()],
        )
        .await?;
        let row = db::first(&self.db, "SELECT * FROM attachments WHERE id = ?", args![id]).await?.ok_or_else(|| not_found(&id))?;
        Ok(records::attachment(&row))
    }

    pub async fn upload_project_readme_attachment(&self, project_id: &str, request: &mut Request) -> Result<Value> {
        self.require_project(project_id).await?;
        let headers = http::attachment_headers(request)?;
        if headers.kind != "inline" {
            return Err(ApiError::new(400, "INVALID_ATTACHMENT_KIND", "Project README attachments must be inline").into());
        }
        let body = http::read_attachment(request).await?;
        let id = uuid();
        let size = body.len();
        self.put_object(&id, body, &headers.content_type).await?;
        self.insert_stored(
            &id,
            "INSERT INTO project_readme_attachments (id, project_id, filename, content_type, size, created_at)
             VALUES (?, ?, ?, ?, ?, ?)"
                .to_owned(),
            args![id, project_id, headers.filename, headers.content_type, size, now()],
        )
        .await?;
        let row = db::first(&self.db, "SELECT * FROM project_readme_attachments WHERE id = ?", args![id])
            .await?
            .ok_or_else(|| not_found(&id))?;
        Ok(records::project_readme_attachment(&row))
    }

    /// `requireAttachment`: a task or comment attachment, else a project README attachment.
    async fn require_attachment(&self, id: &str) -> Result<Value> {
        if let Some(row) = db::first(&self.db, "SELECT * FROM attachments WHERE id = ?", args![id]).await? {
            return Ok(records::attachment(&row));
        }
        let row = db::first(&self.db, "SELECT * FROM project_readme_attachments WHERE id = ?", args![id]).await?;
        row.map(|row| records::project_readme_attachment(&row)).ok_or_else(|| not_found(id))
    }

    pub async fn delete_attachment(&self, id: &str) -> Result<Value> {
        let attachment = self.require_attachment(id).await?;
        let stored = attachment["id"].as_str().unwrap_or_default().to_owned();
        let table = if attachment.get("projectId").is_some() { "project_readme_attachments" } else { "attachments" };
        db::run(&self.db, format!("DELETE FROM {table} WHERE id = ?"), args![stored]).await?;
        self.delete_objects(std::slice::from_ref(&stored)).await?;
        Ok(attachment)
    }

    /// `GET|HEAD /api/attachments/:id/(content|download)`.
    pub async fn attachment_content(&self, id: &str, head: bool, download: bool) -> Result<Response> {
        let attachment = self.require_attachment(id).await?;
        let object = self
            .env
            .bucket("ATTACHMENTS")?
            .get(attachment["id"].as_str().unwrap_or_default())
            .execute()
            .await?
            .ok_or_else(|| not_found(id))?;
        let (disposition, media_type) = attachment::content_headers(
            attachment["filename"].as_str().unwrap_or_default(),
            attachment["contentType"].as_str().unwrap_or_default(),
            !download,
        );
        let headers = Headers::new();
        headers.set("cache-control", "private, no-store")?;
        headers.set("content-disposition", &disposition)?;
        headers.set("content-length", &attachment["size"].to_string())?;
        headers.set("content-security-policy", "sandbox; default-src 'none'")?;
        headers.set("content-type", media_type)?;
        let response = match object.body() {
            Some(body) if !head => Response::from_body(body.response_body()?)?,
            _ => Response::empty()?,
        };
        Ok(response.with_headers(headers))
    }
}
