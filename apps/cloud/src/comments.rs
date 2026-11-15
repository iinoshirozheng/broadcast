//! Task discussion: comments and the activity log.

use broadcast_domain::ApiError;
use broadcast_domain::input::{CommentCreate, CommentPatch};
use serde_json::{Value, json};

use crate::board::{Board, Cursor, THREAD_ASSIGNMENTS, next_cursor, now, uuid, version_conflict};
use crate::db::{self, NEXT_REVISION, Row, args, integer, text};
use crate::error::{Failure, Result};
use crate::records;

const COMMENT_CHANGED: &str = "Comment was changed by another client";

fn assert_comment_version(row: &Row, expected: i64) -> Result<()> {
    let actual = integer(row, "version");
    if actual == expected { Ok(()) } else { Err(version_conflict(COMMENT_CHANGED, expected, actual)) }
}

impl Board {
    pub async fn require_comment_row(&self, id: &str) -> Result<Row> {
        db::first(&self.db, "SELECT * FROM comments WHERE id = ?", args![id])
            .await?
            .ok_or_else(|| ApiError::new(404, "COMMENT_NOT_FOUND", format!("Comment '{id}' does not exist")).into())
    }

    async fn comment_conflict(&self, id: &str, expected: i64) -> Failure {
        match self.require_comment_row(id).await {
            Ok(latest) => version_conflict(COMMENT_CHANGED, expected, integer(&latest, "version")),
            Err(error) => error,
        }
    }

    pub async fn list_task_activities(&self, task_id: &str) -> Result<Vec<Value>> {
        let task = self.require_task_row(task_id).await?;
        let rows = db::all(
            &self.db,
            "SELECT * FROM task_activities WHERE task_id = ? ORDER BY created_at, id",
            args![text(&task, "id")],
        )
        .await?;
        Ok(rows.iter().map(records::task_activity).collect())
    }

    pub async fn list_comments(&self, task_id: &str, after: Option<&Cursor>) -> Result<Value> {
        let task = self.require_task_row(task_id).await?;
        let id = text(&task, "id");
        let rows = match after {
            Some(after) => {
                db::all(
                    &self.db,
                    "SELECT * FROM comments WHERE task_id = ? AND change_revision > ? ORDER BY change_revision",
                    args![id, after.revision],
                )
                .await?
            }
            None => db::all(&self.db, "SELECT * FROM comments WHERE task_id = ? ORDER BY created_at, id", args![id]).await?,
        };
        Ok(json!({ "comments": self.hydrate_comments(&rows).await?, "nextCursor": next_cursor(&rows, after) }))
    }

    pub async fn create_comment(&self, task_id: &str, input: &CommentCreate) -> Result<Value> {
        let task = self.require_task_row(task_id).await?;
        let id = uuid();
        let timestamp = now();
        let binding = records::stored_thread_binding(&input.attribution).unwrap_or_else(|| vec![Value::Null; 5]);
        let session = records::stored_agent_session(&input.attribution).unwrap_or(Value::Null);
        let mut values = args![id, text(&task, "id"), input.body];
        values.extend(binding.iter().map(db::arg));
        values.extend(args![self.actor.kind, self.actor.id, self.actor.name, Value::Null, timestamp, timestamp, session]);
        db::run(
            &self.db,
            format!(
                "INSERT INTO comments (
                   id, task_id, body, thread_id, thread_codex_project_id, thread_codex_project_kind,
                   thread_codex_host_id, thread_workspace_path, author_type, author_id, author_name,
                   author_avatar_url, version, created_at, updated_at, change_revision, agent_session
                 ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, 1, ?, ?, {NEXT_REVISION}, ?)"
            ),
            values,
        )
        .await?;
        let row = self.require_comment_row(&id).await?;
        self.hydrate_comment(&row).await
    }

    pub async fn update_comment(&self, id: &str, input: &CommentPatch) -> Result<Value> {
        let current = self.require_comment_row(id).await?;
        assert_comment_version(&current, input.version)?;
        let comment_id = text(&current, "id");
        let mut assignments = String::new();
        let mut values = args![input.body];
        if let Some(binding) = records::stored_thread_binding(&input.attribution) {
            assignments.push_str(THREAD_ASSIGNMENTS);
            values.extend(binding.iter().map(db::arg));
        }
        if let Some(session) = records::stored_agent_session(&input.attribution) {
            assignments.push_str("agent_session = ?, ");
            values.push(db::arg(&session));
        }
        values.extend(args![now(), comment_id, input.version]);
        let results = self
            .db
            .batch(vec![
                db::statement(
                    &self.db,
                    format!(
                        "UPDATE comments
                         SET body = ?, {assignments}version = version + 1, updated_at = ?, change_revision = {NEXT_REVISION}
                         WHERE id = ? AND version = ?"
                    ),
                    values,
                )?,
                db::statement(
                    &self.db,
                    format!(
                        "UPDATE attachments SET body_fallback = 0, change_revision = {NEXT_REVISION}
                         WHERE comment_id = ? AND body_fallback = 1 AND changes() = 1"
                    ),
                    args![comment_id],
                )?,
            ])
            .await?;
        if !db::changed(&results[0])? {
            return Err(self.comment_conflict(comment_id, input.version).await);
        }
        let row = self.require_comment_row(comment_id).await?;
        self.hydrate_comment(&row).await
    }

    pub async fn delete_comment(&self, id: &str, version: i64) -> Result<()> {
        let current = self.require_comment_row(id).await?;
        assert_comment_version(&current, version)?;
        let comment_id = text(&current, "id");
        let attachments = db::all(&self.db, "SELECT id FROM attachments WHERE comment_id = ?", args![comment_id]).await?;
        let result =
            db::run(&self.db, "DELETE FROM comments WHERE id = ? AND version = ?", args![comment_id, version]).await?;
        if !db::changed(&result)? {
            return Err(self.comment_conflict(comment_id, version).await);
        }
        let ids: Vec<String> = attachments.iter().map(|row| text(row, "id").to_owned()).collect();
        self.delete_objects(&ids).await
    }
}
