//! Task comments and their attachments.

use std::collections::HashMap;

use broadcast_domain::input::{CommentCreate, CommentPatch};
use broadcast_domain::records::{Actor, Attachment, Comment};
use broadcast_domain::{ApiError, Result};
use rusqlite::types::Value as SqlValue;
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params, params_from_iter};
use serde_json::json;
use uuid::Uuid;

use crate::attachments::attachments_for_comment;
use crate::tasks::{CHUNK, consume_attachment_body_fallback, require_task};
use crate::{Store, next_revision, now, rows};

/// `#requireTaskRecord`: the raw task id for an id or identifier.
pub(crate) fn require_task_id(connection: &Connection, id: &str) -> Result<String> {
    connection
        .query_row(
            "SELECT id FROM tasks WHERE id = ?1 OR identifier = ?1",
            [id],
            |row| row.get(0),
        )
        .optional()?
        .ok_or_else(|| {
            ApiError::new(404, "TASK_NOT_FOUND", format!("Task '{id}' does not exist")).into()
        })
}

pub(crate) fn comment_not_found(id: &str) -> ApiError {
    ApiError::new(
        404,
        "COMMENT_NOT_FOUND",
        format!("Comment '{id}' does not exist"),
    )
}

pub(crate) fn get_comment(connection: &Connection, id: &str) -> Result<Option<Comment>> {
    let comment = connection
        .query_row("SELECT * FROM comments WHERE id = ?", [id], rows::comment)
        .optional()?;
    let Some(mut comment) = comment else {
        return Ok(None);
    };
    comment.attachments = attachments_for_comment(connection, &comment.id, None)?;
    Ok(Some(comment))
}

pub(crate) fn require_comment(connection: &Connection, id: &str) -> Result<Comment> {
    get_comment(connection, id)?.ok_or_else(|| comment_not_found(id).into())
}

fn require_comment_version(comment: &Comment, expected: i64) -> Result<()> {
    if comment.version == expected {
        return Ok(());
    }
    Err(ApiError::new(
        409,
        "VERSION_CONFLICT",
        "Comment was changed by another client",
    )
    .with_details(json!({ "expectedVersion": expected, "actualVersion": comment.version }))
    .into())
}

/// `#commentsWithAttachments`: one attachment query per chunk of comments.
fn with_attachments(connection: &Connection, mut comments: Vec<Comment>) -> Result<Vec<Comment>> {
    let mut by_comment: HashMap<String, Vec<Attachment>> = HashMap::new();
    for chunk in comments.chunks(CHUNK) {
        let placeholders = vec!["?"; chunk.len()].join(", ");
        let sql = format!(
            "SELECT * FROM attachments WHERE comment_id IN ({placeholders}) ORDER BY comment_id, created_at, id"
        );
        let mut statement = connection.prepare(&sql)?;
        let attachments = statement.query_map(
            params_from_iter(chunk.iter().map(|comment| &comment.id)),
            rows::attachment,
        )?;
        for attachment in attachments {
            let attachment = attachment?;
            if let Some(comment_id) = attachment.comment_id.clone() {
                by_comment.entry(comment_id).or_default().push(attachment);
            }
        }
    }
    for comment in &mut comments {
        comment.attachments = by_comment.remove(&comment.id).unwrap_or_default();
    }
    Ok(comments)
}

impl Store {
    /// `listComments` / `listCommentsAfter`.
    pub fn list_comments(&self, task_id: &str, after: Option<i64>) -> Result<Vec<Comment>> {
        let connection = self.connection();
        let task_id = require_task_id(&connection, task_id)?;
        let comments = match after {
            Some(revision) => connection
                .prepare("SELECT * FROM comments WHERE task_id = ? AND change_revision > ? ORDER BY change_revision")?
                .query_map(params![task_id, revision], rows::comment)?
                .collect::<rusqlite::Result<Vec<_>>>()?,
            None => connection
                .prepare("SELECT * FROM comments WHERE task_id = ? ORDER BY created_at, id")?
                .query_map([task_id], rows::comment)?
                .collect::<rusqlite::Result<Vec<_>>>()?,
        };
        with_attachments(&connection, comments)
    }

    pub fn get_comment(&self, id: &str) -> Result<Option<Comment>> {
        get_comment(&self.connection(), id)
    }

    pub fn create_comment(
        &self,
        task_id: &str,
        input: &CommentCreate,
        actor: &Actor,
    ) -> Result<Comment> {
        let id = Uuid::new_v4().to_string();
        let timestamp = now();
        let attribution = &input.attribution;
        let mut connection = self.connection();
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let task = require_task(&transaction, task_id)?;
        let revision = next_revision(&transaction)?;
        let session = attribution
            .agent_session
            .clone()
            .flatten()
            .map(|session| serde_json::to_string(&session))
            .transpose()?;
        transaction.execute(
            "INSERT INTO comments (
               id, task_id, body, author_type, author_id, author_name, author_avatar_url,
               version, created_at, updated_at, change_revision, agent_session
             ) VALUES (?, ?, ?, ?, ?, ?, ?, 1, ?, ?, ?, ?)",
            params![
                id,
                task.id,
                input.body,
                actor.kind,
                actor.id,
                actor.name,
                actor.avatar_url,
                timestamp,
                timestamp,
                revision,
                session,
            ],
        )?;
        transaction.commit()?;
        require_comment(&connection, &id)
    }

    pub fn update_comment(&self, id: &str, input: &CommentPatch) -> Result<Comment> {
        let attribution = &input.attribution;
        let mut assignments = String::from("body = ?, ");
        let mut values: Vec<SqlValue> = vec![input.body.clone().into()];
        if let Some(session) = &attribution.agent_session {
            assignments.push_str("agent_session = ?, ");
            values.push(
                session
                    .as_ref()
                    .map(serde_json::to_string)
                    .transpose()?
                    .into(),
            );
        }
        let mut connection = self.connection();
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let current = require_comment(&transaction, id)?;
        require_comment_version(&current, input.version)?;
        let revision = next_revision(&transaction)?;
        values.extend([
            now().into(),
            revision.into(),
            id.to_owned().into(),
            input.version.into(),
        ]);
        transaction.execute(
            &format!(
                "UPDATE comments SET {assignments}version = version + 1, updated_at = ?, change_revision = ?
                 WHERE id = ? AND version = ?"
            ),
            params_from_iter(values),
        )?;
        consume_attachment_body_fallback(&transaction, &current.task_id, Some(&current.id))?;
        transaction.commit()?;
        require_comment(&connection, id)
    }

    /// `deleteComment`: returns the deleted comment with its attachments.
    pub fn delete_comment(&self, id: &str, version: i64) -> Result<Comment> {
        let connection = self.connection();
        let current = require_comment(&connection, id)?;
        require_comment_version(&current, version)?;
        connection.execute(
            "DELETE FROM comments WHERE id = ? AND version = ?",
            params![id, version],
        )?;
        Ok(current)
    }
}
