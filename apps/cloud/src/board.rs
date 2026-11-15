//! The board in D1: lookups, version checks, activity rows and hydrated task records.

use std::collections::{HashMap, HashSet};

use broadcast_domain::input::Attribution;
use broadcast_domain::{ApiError, sql};
use serde_json::{Value, json};
use worker::wasm_bindgen::JsValue;
use worker::{D1Database, D1PreparedStatement, Env};

use crate::auth::Actor;
use crate::db::{self, CHUNK, Row, args, integer, placeholders, text, value};
use crate::error::{Failure, Result};
use crate::records;

const TASK_TREE_MAX_NODES: usize = 1000;

/// `crypto.randomUUID()`.
pub fn uuid() -> String {
    let crypto = worker::js_sys::Reflect::get(&worker::js_sys::global(), &"crypto".into())
        .unwrap_or_default();
    let random = worker::js_sys::Reflect::get(&crypto, &"randomUUID".into()).unwrap_or_default();
    worker::js_sys::Function::from(random)
        .call0(&crypto)
        .ok()
        .and_then(|id| id.as_string())
        .unwrap_or_default()
}

/// `new Date().toISOString()`.
pub fn now() -> String {
    String::from(worker::js_sys::Date::new_0().to_iso_string())
}

/// The request's D1 database, environment and signed-in actor.
pub struct Board {
    pub db: D1Database,
    pub env: Env,
    pub actor: Actor,
}

pub fn version_conflict(message: &str, expected: i64, actual: i64) -> Failure {
    ApiError::new(409, "VERSION_CONFLICT", message.to_owned())
        .with_details(json!({ "expectedVersion": expected, "actualVersion": actual }))
        .into()
}

/// An `after` cursor: the change revision a client last saw.
pub struct Cursor {
    pub value: String,
    pub revision: i64,
}

/// `nextCursor`: the highest change revision among `rows`, or the cursor the client sent.
pub fn next_cursor(rows: &[Row], after: Option<&Cursor>) -> String {
    match rows.iter().map(|row| integer(row, "change_revision")).max() {
        Some(revision) => revision.to_string(),
        None => after.map_or_else(|| "0".to_owned(), |after| after.value.clone()),
    }
}

pub fn assert_task_version(row: &Row, expected: i64) -> Result<()> {
    let actual = integer(row, "version");
    if actual != expected {
        return Err(version_conflict(TASK_CHANGED, expected, actual));
    }
    Ok(())
}

pub const TASK_CHANGED: &str = "Task was changed by another client";

/// The thread and agent-session assignments an attributed write adds, each ending in `, `.
pub fn attribution_assignments(row: &Row, attribution: &Attribution) -> (String, Vec<JsValue>) {
    let mut sql = String::new();
    let mut values = Vec::new();
    if let Some(binding) = records::stored_thread_binding_for_existing(row, attribution) {
        sql.push_str(THREAD_ASSIGNMENTS);
        values.extend(binding.iter().map(db::arg));
    }
    if let Some(session) = records::stored_agent_session(attribution) {
        sql.push_str("agent_session = ?, ");
        values.push(db::arg(&session));
    }
    (sql, values)
}

pub const THREAD_ASSIGNMENTS: &str = "thread_id = ?, thread_codex_project_id = ?, thread_codex_project_kind = ?, \
                                      thread_codex_host_id = ?, thread_workspace_path = ?, ";

fn by_task(rows: Vec<Row>, column: &str) -> HashMap<String, Vec<Row>> {
    let mut grouped: HashMap<String, Vec<Row>> = HashMap::new();
    for row in rows {
        grouped
            .entry(text(&row, column).to_owned())
            .or_default()
            .push(row);
    }
    grouped
}

impl Board {
    pub async fn require_project(&self, id: &str) -> Result<Row> {
        db::first(&self.db, "SELECT * FROM projects WHERE id = ?", args![id])
            .await?
            .ok_or_else(|| {
                ApiError::new(
                    404,
                    "PROJECT_NOT_FOUND",
                    format!("Project '{id}' does not exist"),
                )
                .into()
            })
    }

    pub async fn task_row(&self, id: &str) -> Result<Option<Row>> {
        db::first(
            &self.db,
            "SELECT * FROM tasks WHERE id = ? OR identifier = ?",
            args![id, id],
        )
        .await
    }

    /// Deletes stored attachment content.
    pub async fn delete_objects(&self, ids: &[String]) -> Result<()> {
        let bucket = self.env.bucket("ATTACHMENTS")?;
        for id in ids {
            bucket.delete(id).await?;
        }
        Ok(())
    }

    /// The conflict after a guarded write changed nothing: the task's latest version.
    pub async fn task_conflict(&self, id: &str, expected: i64) -> Failure {
        match self.require_task_row(id).await {
            Ok(latest) => version_conflict(TASK_CHANGED, expected, integer(&latest, "version")),
            Err(failure) => failure,
        }
    }

    pub async fn require_task_row(&self, id: &str) -> Result<Row> {
        self.task_row(id).await?.ok_or_else(|| {
            ApiError::new(404, "TASK_NOT_FOUND", format!("Task '{id}' does not exist")).into()
        })
    }

    /// `taskActivityStatement`: recorded only while the task still has `version` at `timestamp`.
    pub fn task_activity_statement(
        &self,
        task_id: &str,
        changes: &[Value],
        timestamp: &str,
        version: i64,
    ) -> Result<D1PreparedStatement> {
        let actor = &self.actor;
        db::statement(
            &self.db,
            "INSERT INTO task_activities (
               id, task_id, actor_type, actor_id, actor_name, actor_avatar_url, changes, created_at
             )
             SELECT ?, ?, ?, ?, ?, ?, ?, ?
             WHERE EXISTS (SELECT 1 FROM tasks WHERE id = ? AND version = ? AND updated_at = ?)",
            args![
                uuid(),
                task_id,
                actor.kind,
                actor.id,
                actor.name,
                Value::Null,
                Value::String(Value::Array(changes.to_vec()).to_string()),
                timestamp,
                task_id,
                version,
                timestamp
            ],
        )
    }

    /// `SELECT ... WHERE column IN (...)` over `ids` in chunks.
    async fn chunked(&self, ids: &[String], query: impl Fn(&str) -> String) -> Result<Vec<Row>> {
        let mut rows = Vec::new();
        for chunk in ids.chunks(CHUNK) {
            let bound = chunk.iter().map(db::arg).collect();
            rows.extend(db::all(&self.db, query(&placeholders(chunk.len())), bound).await?);
        }
        Ok(rows)
    }

    pub async fn attachments_for_comment(&self, comment_id: &str) -> Result<Vec<Value>> {
        let rows = db::all(
            &self.db,
            "SELECT * FROM attachments WHERE comment_id = ? ORDER BY created_at, id",
            args![comment_id],
        )
        .await?;
        Ok(rows.iter().map(records::attachment).collect())
    }

    pub async fn hydrate_comment(&self, row: &Row) -> Result<Value> {
        Ok(records::comment(
            row,
            self.attachments_for_comment(text(row, "id")).await?,
        ))
    }

    pub async fn hydrate_comments(&self, rows: &[Row]) -> Result<Vec<Value>> {
        let ids: Vec<String> = rows.iter().map(|row| text(row, "id").to_owned()).collect();
        let attachments = self
            .chunked(&ids, |placeholders| {
                format!(
                    "SELECT * FROM attachments WHERE comment_id IN ({placeholders})
                     ORDER BY comment_id, created_at, id"
                )
            })
            .await?;
        let mut grouped = by_task(attachments, "comment_id");
        Ok(rows
            .iter()
            .map(|row| {
                let attachments = grouped.remove(text(row, "id")).unwrap_or_default();
                records::comment(row, attachments.iter().map(records::attachment).collect())
            })
            .collect())
    }

    async fn relations_for_tasks(&self, ids: &[String]) -> Result<HashMap<String, Value>> {
        let mut relations: HashMap<String, Value> = ids
            .iter()
            .map(|id| {
                let empty = json!({ "parent": null, "subIssues": [], "blockedBy": [], "blocks": [], "related": [] });
                (id.clone(), empty)
            })
            .collect();
        for row in self.chunked(ids, sql::task_relations_query).await? {
            let Some(entry) = relations.get_mut(text(&row, "relation_task_id")) else {
                continue;
            };
            let summary = records::task_relation_summary(&row);
            match text(&row, "relation_field") {
                "parent" => entry["parent"] = summary,
                field => {
                    if let Some(list) = entry.get_mut(field).and_then(Value::as_array_mut) {
                        list.push(summary);
                    }
                }
            }
        }
        Ok(relations)
    }

    async fn preview_images(&self, ids: &[String]) -> Result<HashMap<String, Value>> {
        let rows = self
            .chunked(ids, |placeholders| {
                format!(
                    "SELECT attachments.*
                     FROM attachments
                     JOIN tasks ON tasks.id = attachments.task_id
                     WHERE attachments.task_id IN ({placeholders})
                       AND attachments.comment_id IS NULL
                       AND attachments.content_type LIKE 'image/%'
                       AND instr(tasks.description, 'api/attachments/' || attachments.id || '/content') > 0
                     ORDER BY attachments.task_id, attachments.created_at, attachments.id"
                )
            })
            .await?;
        let mut previews = HashMap::new();
        for row in rows {
            previews
                .entry(text(&row, "task_id").to_owned())
                .or_insert_with(|| records::attachment(&row));
        }
        Ok(previews)
    }

    /// `hydrateTasks`: rows with their relations, activity and preview image.
    pub async fn hydrate_tasks(&self, rows: Vec<Row>) -> Result<Vec<Value>> {
        let ids: Vec<String> = rows.iter().map(|row| text(row, "id").to_owned()).collect();
        let mut relations = self.relations_for_tasks(&ids).await?;
        let comments = self
            .chunked(&ids, |placeholders| {
                format!(
                    "SELECT
                       id, task_id,
                       CASE WHEN thread_id IS NULL AND agent_session IS NULL THEN NULL ELSE substr(body, 1, 512) END AS body,
                       thread_id, thread_codex_project_id, thread_codex_project_kind,
                       thread_codex_host_id, thread_workspace_path, agent_session,
                       author_type, author_id, author_name,
                       author_avatar_url, version, updated_at
                     FROM comments
                     WHERE task_id IN ({placeholders})
                     ORDER BY task_id, id"
                )
            })
            .await?;
        let mut comments = by_task(comments, "task_id");
        let activities = self
            .chunked(&ids, |placeholders| {
                format!(
                    "SELECT id, task_id, actor_type, actor_id, actor_name, actor_avatar_url, created_at
                     FROM task_activities
                     WHERE task_id IN ({placeholders})
                     ORDER BY task_id, created_at, id"
                )
            })
            .await?;
        let mut activities = by_task(activities, "task_id");
        let mut previews = self.preview_images(&ids).await?;
        Ok(rows
            .iter()
            .map(|row| {
                let id = text(row, "id");
                let mut task = records::task(row);
                task["relations"] = relations.remove(id).unwrap_or(Value::Null);
                records::attach_task_activity(
                    &mut task,
                    comments.remove(id).unwrap_or_default(),
                    activities.remove(id).unwrap_or_default(),
                    previews.remove(id).unwrap_or(Value::Null),
                );
                task
            })
            .collect())
    }

    pub async fn hydrate_task(&self, row: Row) -> Result<Value> {
        Ok(self
            .hydrate_tasks(vec![row])
            .await?
            .pop()
            .unwrap_or(Value::Null))
    }

    /// `getTask`: the hydrated task, or `null`.
    pub async fn get_task(&self, id: &str) -> Result<Value> {
        match self.task_row(id).await? {
            Some(row) => self.hydrate_task(row).await,
            None => Ok(Value::Null),
        }
    }

    /// `getTaskTree`: the parent hierarchy around a task, breadth first.
    pub async fn get_task_tree(&self, id: &str, direction: &str, depth: u32) -> Result<Value> {
        let root = self.require_task_row(id).await?;
        let root_id = value(&root, "id");
        let mut nodes = vec![records::task_tree_node(
            &root,
            Value::Null,
            0,
            vec![root_id.clone()],
        )];
        let mut seen = HashSet::from([text(&root, "id").to_owned()]);
        let mut frontier = vec![nodes[0].clone()];
        let (join_column, filter_column, parent_column) = if direction == "descendants" {
            (
                "target_task_id",
                "source_task_id",
                "task_relations.source_task_id",
            )
        } else {
            (
                "source_task_id",
                "target_task_id",
                "task_relations.target_task_id",
            )
        };
        let mut level = 1;
        while level <= depth && !frontier.is_empty() {
            let ids: Vec<String> = frontier
                .iter()
                .map(|node| node["id"].as_str().unwrap_or_default().to_owned())
                .collect();
            let rows = self
                .chunked(&ids, |placeholders| {
                    format!(
                        "SELECT tasks.*, {parent_column} AS tree_parent_id
                         FROM task_relations
                         JOIN tasks ON tasks.id = task_relations.{join_column}
                         WHERE task_relations.relation_type = 'parent'
                           AND task_relations.{filter_column} IN ({placeholders})
                         ORDER BY tasks.sort_order, tasks.created_at, tasks.id"
                    )
                })
                .await?;
            let mut by_parent = by_task(rows, "tree_parent_id");
            let mut next = Vec::new();
            for parent in &frontier {
                let parent_id = parent["id"].as_str().unwrap_or_default();
                for row in by_parent.remove(parent_id).unwrap_or_default() {
                    if seen.contains(text(&row, "id")) {
                        continue;
                    }
                    if nodes.len() >= TASK_TREE_MAX_NODES {
                        return Err(ApiError::new(
                            413,
                            "TREE_TOO_LARGE",
                            format!("Task tree cannot exceed {TASK_TREE_MAX_NODES} nodes"),
                        )
                        .into());
                    }
                    let mut path = parent["path"].as_array().cloned().unwrap_or_default();
                    path.push(value(&row, "id"));
                    let node = records::task_tree_node(&row, parent["id"].clone(), level, path);
                    seen.insert(text(&row, "id").to_owned());
                    nodes.push(node.clone());
                    next.push(node);
                }
            }
            frontier = next;
            level += 1;
        }
        Ok(json!({
            "rootId": root_id,
            "direction": direction,
            "depth": depth,
            "nodeCount": nodes.len(),
            "nodes": nodes,
        }))
    }
}
