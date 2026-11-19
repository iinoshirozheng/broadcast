use std::collections::{HashMap, HashSet};

use broadcast_domain::input::{Archived, Attribution, Move, TaskCreate, TaskFilters};
use broadcast_domain::records::{
    Actor, AgentSession, Attachment, ConversationRef, DevelopmentContext, FieldChange, Task,
    TaskChanges, TaskRelations, comment_conversation_title,
};
use broadcast_domain::spec::{self, TaskSpecSummary};
use broadcast_domain::{ApiError, Result, sql};
use rusqlite::types::Value as SqlValue;
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params, params_from_iter};
use serde_json::{Value, json};
use uuid::Uuid;

use crate::projects::project_not_found;
use crate::rows;
use crate::{Store, now};

/// Bound parameters per enrichment query.
pub(crate) const CHUNK: usize = 400;

pub(crate) type Columns = Vec<(&'static str, SqlValue)>;

pub struct DeletedTask {
    pub task: Task,
    pub attachment_ids: Vec<String>,
}

impl Store {
    pub fn list_tasks(&self, filters: &TaskFilters) -> Result<Vec<Task>> {
        let connection = self.connection();
        let mut clauses = Vec::new();
        let mut values = Vec::new();
        if let Some(project_id) = filters.project_id.as_deref().filter(|id| !id.is_empty()) {
            clauses.push("project_id = ?");
            values.push(project_id);
        }
        if let Some(status) = filters
            .status
            .as_deref()
            .filter(|status| !status.is_empty())
        {
            clauses.push("status = ?");
            values.push(status);
        }
        match filters.archived {
            Archived::Exclude => clauses.push("archived_at IS NULL"),
            Archived::Only => clauses.push("archived_at IS NOT NULL"),
            Archived::All => {}
        }
        let filter = if clauses.is_empty() {
            String::new()
        } else {
            format!("WHERE {}", clauses.join(" AND "))
        };
        let sql = format!(
            "SELECT * FROM tasks {filter}
             ORDER BY
               CASE status
                 WHEN 'backlog' THEN 1 WHEN 'todo' THEN 2 WHEN 'in_progress' THEN 3 WHEN 'in_review' THEN 4
                 WHEN 'blocked' THEN 5 WHEN 'done' THEN 6 WHEN 'canceled' THEN 7
               END,
               sort_order, created_at, id"
        );
        let tasks = connection
            .prepare(&sql)?
            .query_map(params_from_iter(values), rows::task)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        enrich(&connection, tasks)
    }

    pub fn get_task(&self, id: &str) -> Result<Option<Task>> {
        get_task(&self.connection(), id)
    }

    /// `getTaskSource`: `None` when the task does not exist.
    pub fn get_task_source(&self, id: &str) -> Result<Option<&'static str>> {
        let source: Option<Option<String>> = self
            .connection()
            .query_row(
                "SELECT external_source FROM tasks WHERE id = ?1 OR identifier = ?1",
                [id],
                |row| row.get(0),
            )
            .optional()?;
        Ok(source.map(|source| {
            if source.as_deref() == Some("jira") {
                "jira"
            } else {
                "local"
            }
        }))
    }

    pub fn create_task(&self, input: &TaskCreate, actor: &Actor, assignee: &Actor) -> Result<Task> {
        let mut connection = self.connection();
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let project = transaction
            .query_row(
                "SELECT
                   projects.id, projects.name, projects.labels, projects.next_task_number,
                   (SELECT tasks.identifier FROM tasks WHERE tasks.project_id = projects.id
                    ORDER BY tasks.created_at, tasks.id LIMIT 1) AS first_identifier
                 FROM projects WHERE projects.id = ?",
                [&input.project_id],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, i64>(3)?,
                        row.get::<_, Option<String>>(4)?,
                    ))
                },
            )
            .optional()?;
        let Some((project_id, name, project_labels, next_task_number, first_identifier)) = project
        else {
            return Err(project_not_found(&input.project_id).into());
        };

        let prefix = project_prefix(&project_id, &name, first_identifier.as_deref());
        let maximum: Option<i64> = transaction.query_row(
            "SELECT MAX(CAST(substr(identifier, ?) AS INTEGER)) FROM tasks WHERE identifier GLOB ?",
            params![(prefix.len() + 2) as i64, format!("{prefix}-[0-9]*")],
            |row| row.get(0),
        )?;
        let number = next_task_number.max(maximum.map_or(1, |maximum| maximum + 1));
        let identifier = format!("{prefix}-{number}");
        let id = Uuid::new_v4().to_string();
        let timestamp = now();
        let sort_order = match input.sort_order {
            Some(sort_order) => sort_order,
            None => top_sort_order(&transaction, &input.project_id, input.status, None)?,
        };

        let labels = merged_labels(
            &serde_json::from_str::<Vec<String>>(&project_labels)?,
            &input.labels,
        );
        transaction.execute(
            "UPDATE projects SET next_task_number = ?, labels = ?, updated_at = ? WHERE id = ?",
            params![
                number + 1,
                serde_json::to_string(&labels)?,
                timestamp,
                input.project_id
            ],
        )?;
        let [git_branch, worktree_path, worktree_branch] =
            context_values(input.development_context.as_ref());
        let recurrence = input.recurrence.as_ref();
        let agent_session = input.attribution.agent_session.clone().flatten();
        transaction.execute(
            "INSERT INTO tasks (
               id, identifier, project_id, title, description, status, priority, labels,
               sort_order, creator_type, creator_id, creator_name, creator_avatar_url,
               assignee_type, assignee_id, assignee_name, assignee_avatar_url,
               git_branch, worktree_path, worktree_branch,
               start_date, due_date, recurrence_interval, recurrence_unit,
               archived_at, version, created_at, updated_at, agent_session
             ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?,
                       NULL, 1, ?, ?, ?)",
            params![
                id,
                identifier,
                input.project_id,
                input.title,
                input.description,
                input.status,
                input.priority,
                serde_json::to_string(&input.labels)?,
                sort_order,
                actor.kind,
                actor.id,
                actor.name,
                actor.avatar_url,
                assignee.kind,
                assignee.id,
                assignee.name,
                assignee.avatar_url,
                git_branch,
                worktree_path,
                worktree_branch,
                input.start_date,
                input.due_date,
                recurrence.map(|recurrence| recurrence.interval),
                recurrence.map(|recurrence| recurrence.unit.as_str()),
                timestamp,
                timestamp,
                agent_session
                    .map(|session| serde_json::to_string(&session))
                    .transpose()?,
            ],
        )?;
        transaction.commit()?;
        require_task(&connection, &id)
    }

    pub fn update_task(
        &self,
        id: &str,
        version: i64,
        changes: &TaskChanges,
        attribution: &Attribution,
        actor: &Actor,
    ) -> Result<Task> {
        let mut connection = self.connection();
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let current = require_task(&transaction, id)?;
        require_version(&current, version)?;
        let activity = changes.diff(&current);
        let target_project = match &changes.project_id {
            Some(project_id) => Some(
                transaction
                    .query_row(
                        "SELECT id FROM projects WHERE id = ?",
                        [project_id],
                        |row| row.get::<_, String>(0),
                    )
                    .optional()?
                    .ok_or_else(|| project_not_found(project_id))?,
            ),
            None => None,
        };
        let moved_to = target_project.filter(|target| *target != current.project_id);
        if moved_to.is_some() {
            let related: bool = transaction.query_row(
                "SELECT EXISTS (SELECT 1 FROM task_relations WHERE source_task_id = ?1 OR target_task_id = ?1)",
                [&current.id],
                |row| row.get(0),
            )?;
            if related {
                return Err(ApiError::new(
                    409,
                    "CROSS_PROJECT_RELATION",
                    "Remove issue relations before moving the issue to another project",
                )
                .into());
            }
        }
        let due_date = changes.due_date.as_ref().unwrap_or(&current.due_date);
        let recurring = changes
            .recurrence
            .as_ref()
            .unwrap_or(&current.recurrence)
            .is_some();
        if recurring && due_date.is_none() {
            return Err(ApiError::invalid_field("A recurring issue requires a due date").into());
        }

        let mut columns = change_columns(changes)?;
        if let Some(status) = changes.status
            && status != current.status
        {
            let placement = moved_to.as_deref().unwrap_or(&current.project_id);
            let sort_order = top_sort_order(&transaction, placement, status, Some(&current.id))?;
            columns.push(("sort_order", sort_order.into()));
        }
        columns.extend(attribution_columns(attribution)?);
        let timestamp = now();
        update_columns(&transaction, &current.id, columns, &timestamp)?;
        if changes.description.is_some() {
            consume_attachment_body_fallback(&transaction, &current.id, None)?;
        }
        if let Some(target) = &moved_to {
            transaction.execute(
                "UPDATE projects SET updated_at = ? WHERE id IN (?, ?)",
                params![timestamp, current.project_id, target],
            )?;
        }
        let destination = moved_to.as_deref().unwrap_or(&current.project_id);
        let project_labels: Vec<String> = serde_json::from_str(&transaction.query_row(
            "SELECT labels FROM projects WHERE id = ?",
            [destination],
            |row| row.get::<_, String>(0),
        )?)?;
        let merged = merged_labels(
            &project_labels,
            changes.labels.as_ref().unwrap_or(&current.labels),
        );
        if merged.len() != project_labels.len() {
            transaction.execute(
                "UPDATE projects SET labels = ?, updated_at = ? WHERE id = ?",
                params![serde_json::to_string(&merged)?, timestamp, destination],
            )?;
        }
        record_activity(&transaction, &current.id, actor, &activity, &timestamp)?;
        transaction.commit()?;
        require_task(&connection, &current.id)
    }

    pub fn move_task(&self, id: &str, input: &Move, actor: &Actor) -> Result<Task> {
        let mut connection = self.connection();
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let current = require_task(&transaction, id)?;
        require_version(&current, input.version)?;
        if current.archived_at.is_some() {
            return Err(
                ApiError::new(409, "TASK_ARCHIVED", "Archived tasks cannot be moved").into(),
            );
        }
        let sort_order = match input.sort_order {
            Some(sort_order) => sort_order,
            None if input.status != current.status => top_sort_order(
                &transaction,
                &current.project_id,
                input.status,
                Some(&current.id),
            )?,
            None => transaction.query_row(
                "SELECT COALESCE(MAX(sort_order), 0) + 1000 FROM tasks
                 WHERE project_id = ? AND status = ? AND archived_at IS NULL AND id != ?",
                params![current.project_id, input.status, current.id],
                |row| row.get(0),
            )?,
        };
        let mut columns: Columns = vec![
            ("status", input.status.to_owned().into()),
            ("sort_order", sort_order.into()),
        ];
        columns.extend(attribution_columns(&input.attribution)?);
        let timestamp = now();
        update_columns(&transaction, &current.id, columns, &timestamp)?;
        let activity = TaskChanges {
            status: Some(input.status),
            ..TaskChanges::default()
        }
        .diff(&current);
        record_activity(&transaction, &current.id, actor, &activity, &timestamp)?;
        transaction.commit()?;
        require_task(&connection, &current.id)
    }

    pub fn archive_task(
        &self,
        id: &str,
        version: i64,
        attribution: &Attribution,
        actor: &Actor,
    ) -> Result<Task> {
        self.set_archived(id, version, attribution, actor, true)
    }

    pub fn restore_task(
        &self,
        id: &str,
        version: i64,
        attribution: &Attribution,
        actor: &Actor,
    ) -> Result<Task> {
        self.set_archived(id, version, attribution, actor, false)
    }

    fn set_archived(
        &self,
        id: &str,
        version: i64,
        attribution: &Attribution,
        actor: &Actor,
        archive: bool,
    ) -> Result<Task> {
        let mut connection = self.connection();
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let current = require_task(&transaction, id)?;
        require_version(&current, version)?;
        if !archive && current.archived_at.is_none() {
            return Err(ApiError::new(
                409,
                "TASK_NOT_ARCHIVED",
                "Only archived tasks can be restored",
            )
            .into());
        }
        let timestamp = now();
        let archived_at = archive.then(|| timestamp.clone());
        let mut columns: Columns = vec![("archived_at", archived_at.clone().into())];
        columns.extend(attribution_columns(attribution)?);
        update_columns(&transaction, &current.id, columns, &timestamp)?;
        let change = FieldChange {
            field: "archivedAt",
            before: current
                .archived_at
                .clone()
                .map_or(Value::Null, Value::String),
            after: archived_at.map_or(Value::Null, Value::String),
        };
        record_activity(&transaction, &current.id, actor, &[change], &timestamp)?;
        transaction.commit()?;
        require_task(&connection, &current.id)
    }

    /// Deletes an archived task; the caller removes the returned attachment files.
    pub fn delete_archived_task(&self, id: &str, version: i64) -> Result<DeletedTask> {
        let mut connection = self.connection();
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let task = require_task(&transaction, id)?;
        require_version(&task, version)?;
        if task.archived_at.is_none() {
            return Err(ApiError::new(
                409,
                "TASK_NOT_ARCHIVED",
                "Only archived tasks can be deleted",
            )
            .into());
        }
        let attachment_ids = transaction
            .prepare("SELECT id FROM attachments WHERE task_id = ? ORDER BY created_at, id")?
            .query_map([&task.id], |row| row.get(0))?
            .collect::<rusqlite::Result<Vec<String>>>()?;
        transaction.execute("DELETE FROM tasks WHERE id = ?", [&task.id])?;
        transaction.commit()?;
        Ok(DeletedTask {
            task,
            attachment_ids,
        })
    }
}

pub(crate) fn get_task(connection: &Connection, id: &str) -> Result<Option<Task>> {
    let task = connection
        .query_row(
            "SELECT * FROM tasks WHERE id = ?1 OR identifier = ?1",
            [id],
            rows::task,
        )
        .optional()?;
    let Some(task) = task else { return Ok(None) };
    Ok(enrich(connection, vec![task])?.pop())
}

pub(crate) fn require_task(connection: &Connection, id: &str) -> Result<Task> {
    get_task(connection, id)?.ok_or_else(|| {
        ApiError::new(404, "TASK_NOT_FOUND", format!("Task '{id}' does not exist")).into()
    })
}

pub(crate) fn require_version(task: &Task, expected: i64) -> Result<()> {
    if task.version == expected {
        return Ok(());
    }
    Err(ApiError::new(
        409,
        "VERSION_CONFLICT",
        "Task was changed by another client",
    )
    .with_details(json!({ "expectedVersion": expected, "actualVersion": task.version }))
    .into())
}

/// `projectPrefix`: the identifier prefix for a project's next task.
fn project_prefix(id: &str, name: &str, first_identifier: Option<&str>) -> String {
    let alphanumeric = |value: &str| {
        value
            .to_uppercase()
            .chars()
            .filter(|c| c.is_ascii_uppercase() || c.is_ascii_digit())
            .collect::<String>()
    };
    let id_prefix = Some(alphanumeric(id))
        .filter(|prefix| !prefix.is_empty())
        .map_or_else(
            || "TASK".to_owned(),
            |mut prefix| {
                prefix.truncate(12);
                prefix
            },
        );
    let existing = first_identifier.map(|identifier| match identifier.rfind('-') {
        Some(dash)
            if dash + 1 < identifier.len()
                && identifier[dash + 1..].bytes().all(|b| b.is_ascii_digit()) =>
        {
            &identifier[..dash]
        }
        _ => identifier,
    });
    if let Some(existing) = existing
        && !existing.is_empty()
        && existing.bytes().all(|b| b.is_ascii_alphanumeric())
        && existing != id_prefix
    {
        return existing.to_owned();
    }
    if id_prefix.len() <= 5 {
        return id_prefix;
    }
    let mut name_prefix = alphanumeric(name);
    name_prefix.truncate(3);
    if name_prefix.is_empty() {
        id_prefix[..3].to_owned()
    } else {
        name_prefix
    }
}

/// `MIN(sort_order) - 1000` among active tasks in a column, or 1000 for an empty column.
fn top_sort_order(
    connection: &Connection,
    project_id: &str,
    status: &str,
    exclude: Option<&str>,
) -> Result<f64> {
    let minimum: Option<f64> = connection.query_row(
        "SELECT MIN(sort_order) FROM tasks
         WHERE project_id = ? AND status = ? AND archived_at IS NULL AND id IS NOT ?",
        params![project_id, status, exclude],
        |row| row.get(0),
    )?;
    Ok(minimum.map_or(1000.0, |minimum| minimum - 1000.0))
}

/// `[...new Set([...project, ...extra])]`.
fn merged_labels(project: &[String], extra: &[String]) -> Vec<String> {
    let mut merged: Vec<String> = Vec::with_capacity(project.len() + extra.len());
    for label in project.iter().chain(extra) {
        if !merged.contains(label) {
            merged.push(label.clone());
        }
    }
    merged
}

/// `git_branch`, `worktree_path`, `worktree_branch`.
fn context_values(context: Option<&DevelopmentContext>) -> [Option<String>; 3] {
    match context {
        Some(DevelopmentContext::Branch { branch }) => [Some(branch.clone()), None, None],
        Some(DevelopmentContext::Worktree { path, branch }) => [None, path.clone(), branch.clone()],
        None => [None, None, None],
    }
}

/// The agent-session column written by an attributed mutation.
pub(crate) fn attribution_columns(attribution: &Attribution) -> Result<Columns> {
    let mut columns = Columns::new();
    if let Some(session) = &attribution.agent_session {
        columns.push((
            "agent_session",
            session
                .as_ref()
                .map(serde_json::to_string)
                .transpose()?
                .into(),
        ));
    }
    Ok(columns)
}

/// Column assignments for the fields present in a task patch.
fn change_columns(changes: &TaskChanges) -> Result<Columns> {
    let mut columns = Columns::new();
    let text = |value: &str| SqlValue::from(value.to_owned());
    if let Some(project_id) = &changes.project_id {
        columns.push(("project_id", text(project_id)));
    }
    if let Some(title) = &changes.title {
        columns.push(("title", text(title)));
    }
    if let Some(description) = &changes.description {
        columns.push(("description", text(description)));
    }
    if let Some(status) = changes.status {
        columns.push(("status", text(status)));
    }
    if let Some(priority) = changes.priority {
        columns.push(("priority", text(priority)));
    }
    if let Some(labels) = &changes.labels {
        columns.push(("labels", serde_json::to_string(labels)?.into()));
    }
    if let Some(context) = &changes.development_context {
        let names = ["git_branch", "worktree_path", "worktree_branch"];
        columns.extend(
            names
                .into_iter()
                .zip(context_values(context.as_ref()).map(SqlValue::from)),
        );
    }
    if let Some(start_date) = &changes.start_date {
        columns.push(("start_date", start_date.clone().into()));
    }
    if let Some(due_date) = &changes.due_date {
        columns.push(("due_date", due_date.clone().into()));
    }
    if let Some(recurrence) = &changes.recurrence {
        let recurrence = recurrence.as_ref();
        columns.push((
            "recurrence_interval",
            recurrence.map(|recurrence| recurrence.interval).into(),
        ));
        columns.push((
            "recurrence_unit",
            recurrence.map(|recurrence| recurrence.unit.clone()).into(),
        ));
    }
    if let Some(assignee) = &changes.assignee {
        columns.push(("assignee_type", text(&assignee.kind)));
        columns.push(("assignee_id", text(&assignee.id)));
        columns.push(("assignee_name", text(&assignee.name)));
        columns.push(("assignee_avatar_url", assignee.avatar_url.clone().into()));
    }
    Ok(columns)
}

/// `UPDATE tasks SET <columns>, version = version + 1, updated_at = ? WHERE id = ?`.
pub(crate) fn update_columns(
    connection: &Connection,
    id: &str,
    columns: Columns,
    timestamp: &str,
) -> Result<()> {
    let mut sql = String::from("UPDATE tasks SET ");
    let mut values = Vec::with_capacity(columns.len() + 2);
    for (column, value) in columns {
        sql.push_str(column);
        sql.push_str(" = ?, ");
        values.push(value);
    }
    sql.push_str("version = version + 1, updated_at = ? WHERE id = ?");
    values.push(timestamp.to_owned().into());
    values.push(id.to_owned().into());
    connection.execute(&sql, params_from_iter(values))?;
    Ok(())
}

/// `#consumeAttachmentBodyFallback`.
pub(crate) fn consume_attachment_body_fallback(
    connection: &Connection,
    task_id: &str,
    comment_id: Option<&str>,
) -> Result<()> {
    let ids = connection
        .prepare("SELECT id FROM attachments WHERE task_id = ? AND comment_id IS ? AND body_fallback = 1")?
        .query_map(params![task_id, comment_id], |row| row.get(0))?
        .collect::<rusqlite::Result<Vec<String>>>()?;
    for id in ids {
        let revision = crate::next_revision(connection)?;
        connection.execute(
            "UPDATE attachments SET body_fallback = 0, change_revision = ? WHERE id = ?",
            params![revision, id],
        )?;
    }
    Ok(())
}

/// `#recordTaskActivity`.
pub(crate) fn record_activity(
    connection: &Connection,
    task_id: &str,
    actor: &Actor,
    changes: &[FieldChange],
    timestamp: &str,
) -> Result<()> {
    if changes.is_empty() {
        return Ok(());
    }
    connection.execute(
        "INSERT INTO task_activities (id, task_id, actor_type, actor_id, actor_name, actor_avatar_url, changes, created_at)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
        params![
            Uuid::new_v4().to_string(),
            task_id,
            actor.kind,
            actor.id,
            actor.name,
            actor.avatar_url,
            serde_json::to_string(changes)?,
            timestamp,
        ],
    )?;
    Ok(())
}

/// A comment as the activity summary reads it.
struct CommentActivity {
    id: String,
    body: Option<String>,
    agent_session: Option<AgentSession>,
    author: Actor,
    version: i64,
    updated_at: String,
}

struct ChangeActivity {
    id: String,
    actor: Actor,
    created_at: String,
}

/// Adds relations, participants, conversation refs, activity keys and preview images.
fn enrich(connection: &Connection, mut tasks: Vec<Task>) -> Result<Vec<Task>> {
    let ids: Vec<String> = tasks.iter().map(|task| task.id.clone()).collect();
    let mut relations: HashMap<String, TaskRelations> = HashMap::new();
    let mut comments: HashMap<String, Vec<CommentActivity>> = HashMap::new();
    let mut changes: HashMap<String, Vec<ChangeActivity>> = HashMap::new();
    let mut previews: HashMap<String, Attachment> = HashMap::new();
    let mut specs: HashMap<String, TaskSpecSummary> = HashMap::new();
    for chunk in ids.chunks(CHUNK) {
        let placeholders = vec!["?"; chunk.len()].join(", ");

        let mut statement = connection.prepare(&sql::task_relations_query(&placeholders))?;
        let mut rows = statement.query(params_from_iter(chunk))?;
        while let Some(row) = rows.next()? {
            let task_relations = relations.entry(row.get("relation_task_id")?).or_default();
            let summary = rows::relation_summary(row)?;
            match row.get::<_, String>("relation_field")?.as_str() {
                "parent" => task_relations.parent = Some(summary),
                "subIssues" => task_relations.sub_issues.push(summary),
                "blockedBy" => task_relations.blocked_by.push(summary),
                "blocks" => task_relations.blocks.push(summary),
                _ => task_relations.related.push(summary),
            }
        }

        let mut statement = connection.prepare(&format!(
            "SELECT
               id, task_id,
               CASE WHEN agent_session IS NULL THEN NULL ELSE substr(body, 1, 512) END AS body,
               agent_session,
               author_type, author_id, author_name, author_avatar_url, version, updated_at
             FROM comments WHERE task_id IN ({placeholders}) ORDER BY task_id, id"
        ))?;
        let mut rows = statement.query(params_from_iter(chunk))?;
        while let Some(row) = rows.next()? {
            comments
                .entry(row.get("task_id")?)
                .or_default()
                .push(CommentActivity {
                    id: row.get("id")?,
                    body: row.get("body")?,
                    agent_session: rows::optional_json(row, "agent_session")?,
                    author: rows::actor(row, "author")?,
                    version: row.get("version")?,
                    updated_at: row.get("updated_at")?,
                });
        }

        let mut statement = connection.prepare(&format!(
            "SELECT id, task_id, actor_type, actor_id, actor_name, actor_avatar_url, created_at
             FROM task_activities WHERE task_id IN ({placeholders}) ORDER BY task_id, created_at, id"
        ))?;
        let mut rows = statement.query(params_from_iter(chunk))?;
        while let Some(row) = rows.next()? {
            changes
                .entry(row.get("task_id")?)
                .or_default()
                .push(ChangeActivity {
                    id: row.get("id")?,
                    actor: rows::actor(row, "actor")?,
                    created_at: row.get("created_at")?,
                });
        }

        let mut statement = connection.prepare(&format!(
            "SELECT attachments.* FROM attachments
             JOIN tasks ON tasks.id = attachments.task_id
             WHERE attachments.task_id IN ({placeholders})
               AND attachments.comment_id IS NULL
               AND attachments.content_type LIKE 'image/%'
               AND instr(tasks.description, 'api/attachments/' || attachments.id || '/content') > 0
             ORDER BY attachments.task_id, attachments.created_at, attachments.id"
        ))?;
        let mut rows = statement.query(params_from_iter(chunk))?;
        while let Some(row) = rows.next()? {
            let attachment = rows::attachment(row)?;
            previews
                .entry(attachment.task_id.clone())
                .or_insert(attachment);
        }

        let mut statement = connection.prepare(&format!(
            "SELECT task_id, tasks, updated_at FROM task_specs WHERE task_id IN ({placeholders})"
        ))?;
        let mut rows = statement.query(params_from_iter(chunk))?;
        while let Some(row) = rows.next()? {
            specs.insert(
                row.get("task_id")?,
                TaskSpecSummary {
                    updated_at: row.get("updated_at")?,
                    groups: spec::parse_tasks(&row.get::<_, String>("tasks")?),
                },
            );
        }
    }
    for task in &mut tasks {
        task.relations = relations.remove(&task.id).unwrap_or_default();
        task.spec = specs.remove(&task.id);
        let task_comments = comments.remove(&task.id).unwrap_or_default();
        let task_changes = changes.remove(&task.id).unwrap_or_default();
        attach_activity(task, task_comments, task_changes, previews.remove(&task.id));
    }
    Ok(tasks)
}

/// `attachTaskActivity`.
fn attach_activity(
    task: &mut Task,
    mut comments: Vec<CommentActivity>,
    mut changes: Vec<ChangeActivity>,
    preview_image: Option<Attachment>,
) {
    comments.sort_by(|left, right| left.id.cmp(&right.id));
    changes.sort_by(|left, right| left.id.cmp(&right.id));

    let mut participants = Vec::new();
    let mut seen = HashSet::new();
    let actors = [task.creator(), task.assignee.clone()]
        .into_iter()
        .chain(comments.iter().map(|comment| comment.author.clone()))
        .chain(changes.iter().map(|change| change.actor.clone()));
    for actor in actors {
        if seen.insert(format!("{}:{}", actor.kind, actor.id)) {
            participants.push(actor);
        }
    }

    let mut refs = Vec::new();
    if let Some(agent_session) = &task.agent_session {
        refs.push(ConversationRef {
            agent_session: agent_session.clone(),
            source: "task",
            source_id: task.id.clone(),
            title: task.title.clone(),
            updated_at: task.updated_at.clone(),
        });
    }
    for comment in &comments {
        if let Some(agent_session) = &comment.agent_session {
            refs.push(ConversationRef {
                agent_session: agent_session.clone(),
                source: "comment",
                source_id: comment.id.clone(),
                title: comment_conversation_title(comment.body.as_deref()),
                updated_at: comment.updated_at.clone(),
            });
        }
    }

    let activity_key = json!({
        "version": 1,
        "task": [task.id, task.version, task.updated_at],
        "comments": comments.iter().map(|comment| json!([comment.id, comment.version, comment.updated_at])).collect::<Vec<_>>(),
        "changes": changes.iter().map(|change| json!([change.id, change.created_at])).collect::<Vec<_>>(),
    })
    .to_string();
    let activity_updated_at = comments
        .iter()
        .map(|comment| &comment.updated_at)
        .chain(changes.iter().map(|change| &change.created_at))
        .fold(&task.updated_at, |latest, timestamp| {
            if timestamp > latest {
                timestamp
            } else {
                latest
            }
        })
        .clone();

    task.conversation_refs = refs;
    task.participants = participants;
    task.preview_image = preview_image;
    task.activity_key = activity_key;
    task.activity_updated_at = activity_updated_at;
}
