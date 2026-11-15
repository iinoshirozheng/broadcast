//! Task and comment attachment records. File bytes live on disk, owned by the server.

use broadcast_domain::records::Attachment;
use broadcast_domain::{ApiError, Result};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};

use crate::comments::{comment_not_found, require_comment};
use crate::tasks::require_task;
use crate::{Store, next_revision, now, rows};

/// The stored metadata of an uploaded file.
pub struct NewAttachment<'a> {
    pub id: &'a str,
    pub kind: &'a str,
    pub filename: &'a str,
    pub content_type: &'a str,
    pub size: i64,
}

fn collect(
    connection: &Connection,
    sql: &str,
    params: impl rusqlite::Params,
) -> Result<Vec<Attachment>> {
    Ok(connection
        .prepare(sql)?
        .query_map(params, rows::attachment)?
        .collect::<rusqlite::Result<_>>()?)
}

/// `#attachmentsForComment`.
pub(crate) fn attachments_for_comment(
    connection: &Connection,
    comment_id: &str,
    after: Option<i64>,
) -> Result<Vec<Attachment>> {
    match after {
        Some(revision) => collect(
            connection,
            "SELECT * FROM attachments WHERE comment_id = ? AND change_revision > ? ORDER BY change_revision",
            params![comment_id, revision],
        ),
        None => collect(
            connection,
            "SELECT * FROM attachments WHERE comment_id = ? ORDER BY created_at, id",
            [comment_id],
        ),
    }
}

fn get_attachment(connection: &Connection, id: &str) -> Result<Option<Attachment>> {
    Ok(connection
        .query_row(
            "SELECT * FROM attachments WHERE id = ?",
            [id],
            rows::attachment,
        )
        .optional()?)
}

fn insert(
    connection: &Connection,
    task_id: &str,
    comment_id: Option<&str>,
    input: &NewAttachment<'_>,
) -> Result<()> {
    let revision = next_revision(connection)?;
    connection.execute(
        "INSERT INTO attachments (id, task_id, comment_id, kind, filename, content_type, size, created_at, change_revision)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
        params![input.id, task_id, comment_id, input.kind, input.filename, input.content_type, input.size, now(), revision],
    )?;
    Ok(())
}

impl Store {
    /// `listAttachments`: task-level attachments only.
    pub fn list_attachments(&self, task_id: &str, after: Option<i64>) -> Result<Vec<Attachment>> {
        let connection = self.connection();
        let task = require_task(&connection, task_id)?;
        match after {
            Some(revision) => collect(
                &connection,
                "SELECT * FROM attachments WHERE task_id = ? AND comment_id IS NULL AND change_revision > ?
                 ORDER BY change_revision",
                params![task.id, revision],
            ),
            None => collect(
                &connection,
                "SELECT * FROM attachments WHERE task_id = ? AND comment_id IS NULL ORDER BY created_at, id",
                [task.id],
            ),
        }
    }

    pub fn create_attachment(
        &self,
        task_id: &str,
        input: &NewAttachment<'_>,
    ) -> Result<Attachment> {
        let mut connection = self.connection();
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let task = require_task(&transaction, task_id)?;
        insert(&transaction, &task.id, None, input)?;
        transaction.commit()?;
        Ok(get_attachment(&connection, input.id)?.expect("inserted attachment"))
    }

    pub fn list_comment_attachments(
        &self,
        comment_id: &str,
        after: Option<i64>,
    ) -> Result<Vec<Attachment>> {
        let connection = self.connection();
        let exists = connection
            .query_row("SELECT 1 FROM comments WHERE id = ?", [comment_id], |_| {
                Ok(())
            })
            .optional()?;
        if exists.is_none() {
            return Err(comment_not_found(comment_id).into());
        }
        attachments_for_comment(&connection, comment_id, after)
    }

    pub fn create_comment_attachment(
        &self,
        comment_id: &str,
        input: &NewAttachment<'_>,
    ) -> Result<Attachment> {
        let mut connection = self.connection();
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let comment = require_comment(&transaction, comment_id)?;
        insert(&transaction, &comment.task_id, Some(&comment.id), input)?;
        transaction.commit()?;
        Ok(get_attachment(&connection, input.id)?.expect("inserted attachment"))
    }

    pub fn get_attachment(&self, id: &str) -> Result<Option<Attachment>> {
        get_attachment(&self.connection(), id)
    }

    pub fn delete_attachment(&self, id: &str) -> Result<Attachment> {
        let connection = self.connection();
        let attachment = get_attachment(&connection, id)?.ok_or_else(|| {
            ApiError::new(
                404,
                "ATTACHMENT_NOT_FOUND",
                format!("Attachment '{id}' does not exist"),
            )
        })?;
        connection.execute("DELETE FROM attachments WHERE id = ?", [id])?;
        Ok(attachment)
    }
}
