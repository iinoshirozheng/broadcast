//! Issue relations (parent, blocks, related), the parent tree, and the activity log.

use std::collections::{HashMap, HashSet};

use broadcast_domain::input::{Attribution, RelationMutation};
use broadcast_domain::records::{
    Actor, FieldChange, RelationChange, Task, TaskActivity, TaskTree, TaskTreeNode, TaskTreeSummary,
};
use broadcast_domain::{ApiError, Result};
use rusqlite::{Connection, OptionalExtension, Row, TransactionBehavior, params, params_from_iter};
use serde_json::{Value, json};

use crate::tasks::{attribution_columns, get_task, record_activity, require_task, require_version, update_columns};
use crate::{Store, now, rows};

const TASK_TREE_MAX_NODES: usize = 1000;

/// `relationActivityValue`.
fn activity_value(kind: &str, identifier: &str, external_key: Option<&str>, title: &str) -> Value {
    json!({ "type": kind, "identifier": identifier, "externalKey": external_key, "title": title })
}

fn task_activity_value(kind: &str, task: &Task) -> Value {
    activity_value(kind, &task.identifier, task.external_key.as_deref(), &task.title)
}

/// `#relationEndpoints`: (stored type, source, target).
fn endpoints<'a>(kind: &str, task_id: &'a str, related_id: &'a str) -> (&'static str, &'a str, &'a str) {
    match kind {
        "parent" => ("parent", related_id, task_id),
        "blocks" => ("blocks", task_id, related_id),
        "blocked_by" => ("blocks", related_id, task_id),
        _ if task_id <= related_id => ("related", task_id, related_id),
        _ => ("related", related_id, task_id),
    }
}

/// Both tasks, version-checked and in the same project.
fn relation_tasks(connection: &Connection, id: &str, related_id: &str, version: i64) -> Result<(Task, Task)> {
    let task = require_task(connection, id)?;
    let related = require_task(connection, related_id)?;
    require_version(&task, version)?;
    if task.id == related.id {
        return Err(ApiError::new(400, "SELF_RELATION", "An issue cannot be related to itself").into());
    }
    if task.project_id != related.project_id {
        return Err(ApiError::new(400, "CROSS_PROJECT_RELATION", "Issue relations must stay within one project").into());
    }
    Ok((task, related))
}

fn assert_no_parent_cycle(connection: &Connection, child_id: &str, parent_id: &str) -> Result<()> {
    let cycle = connection
        .query_row(
            "WITH RECURSIVE ancestors(id) AS (
               SELECT source_task_id FROM task_relations WHERE relation_type = 'parent' AND target_task_id = ?
               UNION
               SELECT task_relations.source_task_id FROM task_relations
               JOIN ancestors ON task_relations.target_task_id = ancestors.id
               WHERE task_relations.relation_type = 'parent'
             )
             SELECT 1 FROM ancestors WHERE id = ?",
            params![parent_id, child_id],
            |_| Ok(()),
        )
        .optional()?;
    if cycle.is_some() {
        return Err(ApiError::new(409, "RELATION_CYCLE", "This parent would create a cycle").into());
    }
    Ok(())
}

/// `#touchTask`: bump the version and record the mutation's attribution.
fn touch_task(connection: &Connection, task: &Task, attribution: &Attribution, timestamp: &str) -> Result<()> {
    let columns = attribution_columns(task, attribution, true)?;
    update_columns(connection, &task.id, columns, timestamp)
}

fn relation_change(connection: &Connection, task_id: &str, related_id: &str) -> Result<RelationChange> {
    Ok(RelationChange { task: require_task(connection, task_id)?, related_task: require_task(connection, related_id)? })
}

/// `](?project=..&issue=..)`, the Markdown link a mention writes.
fn mention_reference(project_id: &str, identifier: &str) -> String {
    let query = form_urlencoded::Serializer::new(String::new())
        .append_pair("project", project_id)
        .append_pair("issue", identifier)
        .finish();
    format!("](?{query})")
}

struct TreeRow {
    id: String,
    parent_id: Option<String>,
    summary: TaskTreeSummary,
}

fn tree_row(row: &Row<'_>, with_parent: bool) -> rusqlite::Result<TreeRow> {
    Ok(TreeRow {
        id: row.get("id")?,
        parent_id: if with_parent { row.get("tree_parent_id")? } else { None },
        summary: TaskTreeSummary {
            identifier: row.get("identifier")?,
            title: row.get("title")?,
            status: row.get("status")?,
            priority: row.get("priority")?,
            archived_at: row.get("archived_at")?,
        },
    })
}

impl Store {
    pub fn add_task_relation(
        &self,
        id: &str,
        kind: &str,
        related_id: &str,
        input: &RelationMutation,
        actor: &Actor,
    ) -> Result<RelationChange> {
        let mut connection = self.connection();
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let (task, related) = relation_tasks(&transaction, id, related_id, input.version)?;
        let (stored_kind, source, target) = endpoints(kind, &task.id, &related.id);
        if stored_kind == "parent" {
            assert_no_parent_cycle(&transaction, &task.id, &related.id)?;
            let existing: Option<String> = transaction
                .query_row(
                    "SELECT source_task_id FROM task_relations WHERE relation_type = 'parent' AND target_task_id = ?",
                    [&task.id],
                    |row| row.get(0),
                )
                .optional()?;
            if existing.as_deref() == Some(related.id.as_str()) {
                return Err(ApiError::new(409, "RELATION_EXISTS", "This parent relation already exists").into());
            }
            if existing.is_some() {
                transaction.execute(
                    "DELETE FROM task_relations WHERE relation_type = 'parent' AND target_task_id = ?",
                    [&task.id],
                )?;
            }
        } else {
            let existing = transaction
                .query_row(
                    "SELECT 1 FROM task_relations WHERE relation_type = ? AND source_task_id = ? AND target_task_id = ?",
                    params![stored_kind, source, target],
                    |_| Ok(()),
                )
                .optional()?;
            if existing.is_some() {
                return Err(ApiError::new(409, "RELATION_EXISTS", "This issue relation already exists").into());
            }
        }
        let timestamp = now();
        let previous = match (&task.relations.parent, kind) {
            (Some(parent), "parent") => {
                activity_value(kind, &parent.identifier, parent.external_key.as_deref(), &parent.title)
            }
            _ => Value::Null,
        };
        transaction.execute(
            "INSERT INTO task_relations (relation_type, source_task_id, target_task_id, origin, created_at)
             VALUES (?, ?, ?, ?, ?)",
            params![stored_kind, source, target, input.origin.unwrap_or("manual"), timestamp],
        )?;
        touch_task(&transaction, &task, &input.attribution, &timestamp)?;
        let change = FieldChange { field: "relation", before: previous, after: task_activity_value(kind, &related) };
        record_activity(&transaction, &task.id, actor, &[change], &timestamp)?;
        let result = relation_change(&transaction, &task.id, &related.id)?;
        transaction.commit()?;
        Ok(result)
    }

    pub fn remove_task_relation(
        &self,
        id: &str,
        kind: &str,
        related_id: &str,
        input: &RelationMutation,
        actor: &Actor,
    ) -> Result<RelationChange> {
        let mut connection = self.connection();
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let (task, related) = relation_tasks(&transaction, id, related_id, input.version)?;
        let (stored_kind, source, target) = endpoints(kind, &task.id, &related.id);
        let stored_origin: String = transaction
            .query_row(
                "SELECT origin FROM task_relations WHERE relation_type = ? AND source_task_id = ? AND target_task_id = ?",
                params![stored_kind, source, target],
                |row| row.get(0),
            )
            .optional()?
            .ok_or_else(|| ApiError::new(404, "RELATION_NOT_FOUND", "This issue relation does not exist"))?;
        if let Some(origin) = input.origin
            && stored_origin != origin
        {
            let result = relation_change(&transaction, &task.id, &related.id)?;
            transaction.commit()?;
            return Ok(result);
        }
        if input.origin == Some("mention") && stored_kind == "related" {
            let task_reference = mention_reference(&task.project_id, &related.identifier);
            let related_reference = mention_reference(&task.project_id, &task.identifier);
            let deleted = transaction.execute(
                "DELETE FROM task_relations
                 WHERE relation_type = ? AND source_task_id = ? AND target_task_id = ? AND origin = 'mention'
                   AND NOT EXISTS (
                     SELECT 1 FROM tasks
                     WHERE (id = ? AND instr(description, ?) > 0) OR (id = ? AND instr(description, ?) > 0)
                   )
                   AND NOT EXISTS (
                     SELECT 1 FROM comments
                     WHERE (task_id = ? AND instr(body, ?) > 0) OR (task_id = ? AND instr(body, ?) > 0)
                   )",
                params![
                    stored_kind,
                    source,
                    target,
                    task.id,
                    task_reference,
                    related.id,
                    related_reference,
                    task.id,
                    task_reference,
                    related.id,
                    related_reference,
                ],
            )?;
            if deleted == 0 {
                let result = relation_change(&transaction, &task.id, &related.id)?;
                transaction.commit()?;
                return Ok(result);
            }
        } else {
            transaction.execute(
                "DELETE FROM task_relations WHERE relation_type = ? AND source_task_id = ? AND target_task_id = ?",
                params![stored_kind, source, target],
            )?;
        }
        let timestamp = now();
        touch_task(&transaction, &task, &input.attribution, &timestamp)?;
        let change = FieldChange { field: "relation", before: task_activity_value(kind, &related), after: Value::Null };
        record_activity(&transaction, &task.id, actor, &[change], &timestamp)?;
        let result = relation_change(&transaction, &task.id, &related.id)?;
        transaction.commit()?;
        Ok(result)
    }

    /// `getTaskTree`: breadth-first walk over parent relations.
    pub fn get_task_tree(&self, id: &str, direction: &'static str, depth: u32) -> Result<TaskTree> {
        let connection = self.connection();
        let root = connection
            .query_row("SELECT * FROM tasks WHERE id = ?1 OR identifier = ?1", [id], |row| tree_row(row, false))
            .optional()?
            .ok_or_else(|| ApiError::new(404, "TASK_NOT_FOUND", format!("Task '{id}' does not exist")))?;
        let (join_column, filter_column) = if direction == "descendants" {
            ("target_task_id", "source_task_id")
        } else {
            ("source_task_id", "target_task_id")
        };
        let root_id = root.id.clone();
        let mut nodes =
            vec![TaskTreeNode { id: root.id.clone(), parent_id: None, depth: 0, path: vec![root.id.clone()], summary: root.summary }];
        let mut seen = HashSet::from([root_id.clone()]);
        let mut frontier = vec![0usize];
        let mut level = 1;
        while level <= depth && !frontier.is_empty() {
            let placeholders = vec!["?"; frontier.len()].join(", ");
            let sql = format!(
                "SELECT tasks.*, task_relations.{filter_column} AS tree_parent_id
                 FROM task_relations
                 JOIN tasks ON tasks.id = task_relations.{join_column}
                 WHERE task_relations.relation_type = 'parent'
                   AND task_relations.{filter_column} IN ({placeholders})
                 ORDER BY tasks.sort_order, tasks.created_at, tasks.id"
            );
            let ids: Vec<String> = frontier.iter().map(|&index| nodes[index].id.clone()).collect();
            let mut by_parent: HashMap<String, Vec<TreeRow>> = HashMap::new();
            for row in connection.prepare(&sql)?.query_map(params_from_iter(&ids), |row| tree_row(row, true))? {
                let row = row?;
                by_parent.entry(row.parent_id.clone().unwrap_or_default()).or_default().push(row);
            }
            let mut next = Vec::new();
            for &parent in &frontier {
                let parent_id = nodes[parent].id.clone();
                for row in by_parent.remove(&parent_id).unwrap_or_default() {
                    if !seen.insert(row.id.clone()) {
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
                    let mut path = nodes[parent].path.clone();
                    path.push(row.id.clone());
                    nodes.push(TaskTreeNode {
                        id: row.id,
                        parent_id: Some(parent_id.clone()),
                        depth: level,
                        path,
                        summary: row.summary,
                    });
                    next.push(nodes.len() - 1);
                }
            }
            frontier = next;
            level += 1;
        }
        Ok(TaskTree { root_id, direction, depth, node_count: nodes.len(), nodes })
    }

    /// `listTaskActivities`.
    pub fn list_task_activities(&self, task_id: &str) -> Result<Vec<TaskActivity>> {
        let connection = self.connection();
        let task = get_task(&connection, task_id)?
            .ok_or_else(|| ApiError::new(404, "TASK_NOT_FOUND", format!("Task '{task_id}' does not exist")))?;
        let activities = connection
            .prepare("SELECT * FROM task_activities WHERE task_id = ? ORDER BY created_at, id")?
            .query_map([task.id], rows::activity)?
            .collect::<rusqlite::Result<_>>()?;
        Ok(activities)
    }
}
