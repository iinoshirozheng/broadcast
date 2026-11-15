//! Creating, editing, moving, archiving and deleting issues.

use broadcast_domain::input::{Move, TaskCreate, TaskPatch, VersionMutation};
use broadcast_domain::records::DevelopmentContext;
use broadcast_domain::{ApiError, js};
use serde_json::{Value, json};
use worker::wasm_bindgen::JsValue;

use crate::auth::resolve_assignee;
use crate::board::{Board, THREAD_ASSIGNMENTS, assert_task_version, attribution_assignments, now, uuid};
use crate::db::{self, NEXT_REVISION, Row, args, text, value};
use crate::error::Result;
use crate::records;

/// The project's labels followed by the bound JSON array's new labels, first occurrence kept.
const MERGED_LABELS: &str = "(
  SELECT json_group_array(value)
  FROM (
    SELECT value
    FROM (
      SELECT value, source_order, label_order,
        ROW_NUMBER() OVER (PARTITION BY value ORDER BY source_order, label_order) AS occurrence_rank
      FROM (
        SELECT value, 0 AS source_order, key AS label_order FROM json_each(projects.labels)
        UNION ALL
        SELECT value, 1 AS source_order, key AS label_order FROM json_each(?)
      )
    )
    WHERE occurrence_rank = 1
    ORDER BY source_order, label_order
  )
)";

fn project_not_found(id: &str) -> ApiError {
    ApiError::new(404, "PROJECT_NOT_FOUND", format!("Project '{id}' does not exist"))
}

fn cross_project_relation() -> ApiError {
    ApiError::new(409, "CROSS_PROJECT_RELATION", "Remove issue relations before moving the issue to another project")
}

fn development_columns(context: Option<&DevelopmentContext>) -> [Value; 2] {
    match context {
        Some(DevelopmentContext::Branch { branch }) => [json!("branch"), json!(branch)],
        Some(DevelopmentContext::Worktree { branch, .. }) => [json!("worktree"), json!(branch)],
        None => [Value::Null, Value::Null],
    }
}

fn assignee_columns(assignee: &Value) -> Vec<JsValue> {
    ["type", "id", "name", "avatarUrl"].iter().map(|field| db::arg(&assignee[*field])).collect()
}

impl Board {
    /// The sort order that puts a task first (`MIN`) or last (`MAX`) in a column.
    async fn column_edge(&self, project_id: &str, status: &str, exclude: Option<&str>, first: bool) -> Result<f64> {
        let exclude_sql = if exclude.is_some() { " AND id != ?" } else { "" };
        let aggregate = if first { "MIN(sort_order)" } else { "COALESCE(MAX(sort_order), 0)" };
        let mut bound = args![project_id, status];
        if let Some(id) = exclude {
            bound.push(db::arg(id));
        }
        let row = db::first(
            &self.db,
            format!(
                "SELECT {aggregate} AS edge FROM tasks
                 WHERE project_id = ? AND status = ? AND archived_at IS NULL{exclude_sql}"
            ),
            bound,
        )
        .await?;
        let edge = row.as_ref().and_then(|row| row.get("edge")).and_then(Value::as_f64);
        Ok(match (first, edge) {
            (true, None) => 1000.0,
            (true, Some(minimum)) => minimum - 1000.0,
            (false, maximum) => maximum.unwrap_or(0.0) + 1000.0,
        })
    }

    async fn has_relations(&self, id: &str) -> Result<bool> {
        let sql = "SELECT 1 AS found FROM task_relations WHERE source_task_id = ? OR target_task_id = ? LIMIT 1";
        Ok(db::first(&self.db, sql, args![id, id]).await?.is_some())
    }

    pub async fn create_task(&self, input: &TaskCreate) -> Result<Value> {
        let project = db::first(
            &self.db,
            "SELECT
               projects.id,
               projects.name,
               (SELECT tasks.identifier FROM tasks WHERE tasks.project_id = projects.id
                ORDER BY tasks.created_at, tasks.id LIMIT 1) AS first_identifier
             FROM projects
             WHERE projects.id = ?",
            args![input.project_id],
        )
        .await?
        .ok_or_else(|| project_not_found(&input.project_id))?;
        let prefix = records::project_prefix(
            text(&project, "id"),
            text(&project, "name"),
            db::optional_text(&project, "first_identifier"),
        );
        let suffix_start = prefix.len() + 2;
        let sort_order = match input.sort_order {
            Some(sort_order) => sort_order,
            None => self.column_edge(&input.project_id, input.status, None, false).await?,
        };
        let id = uuid();
        let timestamp = now();
        let assignee = resolve_assignee(input.assignee_target, &self.actor);
        let labels = json!(input.labels).to_string();
        let binding = records::stored_thread_binding(&input.attribution).unwrap_or_else(|| vec![Value::Null; 5]);
        let session = records::stored_agent_session(&input.attribution).unwrap_or(Value::Null);
        let [context_type, context_branch] = development_columns(input.development_context.as_ref());
        let recurrence = input.recurrence.as_ref();

        let mut bound = args![
            id,
            prefix,
            suffix_start,
            format!("{prefix}-[0-9]*"),
            input.title,
            input.description,
            input.status,
            input.priority,
            labels,
            js::number(sort_order)
        ];
        bound.extend(binding.iter().map(db::arg));
        bound.extend(args![self.actor.kind, self.actor.id, self.actor.name, Value::Null]);
        bound.extend(assignee_columns(&assignee));
        bound.extend(args![
            context_type,
            context_branch,
            input.start_date,
            input.due_date,
            recurrence.map(|recurrence| recurrence.interval),
            recurrence.map(|recurrence| recurrence.unit.clone()),
            timestamp,
            timestamp,
            session,
            input.project_id
        ]);
        let insert = db::statement(
            &self.db,
            "INSERT INTO tasks (
               id, identifier, project_id, title, description, status, priority, labels,
               sort_order, thread_id, thread_codex_project_id, thread_codex_project_kind,
               thread_codex_host_id, thread_workspace_path,
               creator_type, creator_id, creator_name, creator_avatar_url,
               assignee_type, assignee_id, assignee_name, assignee_avatar_url,
               development_context_type, development_branch,
               start_date, due_date, recurrence_interval, recurrence_unit,
               archived_at, version, created_at, updated_at, agent_session
             )
             SELECT
               ?,
               ? || '-' || CAST(MAX(
                 projects.next_task_number,
                 COALESCE((
                   SELECT MAX(CAST(substr(tasks.identifier, ?) AS INTEGER)) + 1
                   FROM tasks
                   WHERE tasks.identifier GLOB ?
                 ), 1)
               ) AS TEXT),
               projects.id,
               ?, ?, ?, ?, ?, ?,
               ?, ?, ?, ?, ?,
               ?, ?, ?, ?,
               ?, ?, ?, ?,
               ?, ?,
               ?, ?, ?, ?,
               NULL, 1, ?, ?, ?
             FROM projects
             WHERE projects.id = ?",
            bound,
        )?;
        let numbering = db::statement(
            &self.db,
            format!(
                "UPDATE projects
                 SET
                   next_task_number = (SELECT CAST(substr(identifier, ?) AS INTEGER) + 1 FROM tasks WHERE id = ?),
                   labels = {MERGED_LABELS},
                   updated_at = ?
                 WHERE id = ?"
            ),
            args![suffix_start, id, labels, timestamp, input.project_id],
        )?;
        let results = self.db.batch(vec![insert, numbering]).await?;
        if !db::changed(&results[0])? || !db::changed(&results[1])? {
            return Err(project_not_found(&input.project_id).into());
        }
        self.get_task(&id).await
    }

    pub async fn update_task(&self, id: &str, input: &TaskPatch) -> Result<Value> {
        let current = self.require_task_row(id).await?;
        assert_task_version(&current, input.version)?;
        let current_task = records::task(&current);
        let current_id = text(&current, "id");
        let current_project = text(&current, "project_id");
        let changes = &input.changes;
        let target_project = match &changes.project_id {
            Some(project_id) => Some(self.require_project(project_id).await?),
            None => None,
        };
        let target_project_id = target_project.as_ref().map(|project| text(project, "id"));
        let project_changed = target_project_id.is_some_and(|target| target != current_project);
        let destination = target_project_id.unwrap_or(current_project);
        let task_labels = match &changes.labels {
            Some(labels) => json!(labels),
            None => current_task["labels"].clone(),
        };
        if project_changed && self.has_relations(current_id).await? {
            return Err(cross_project_relation().into());
        }

        let mut activity_values = changes.entries();
        let due_date = match &changes.due_date {
            Some(due_date) => json!(due_date),
            None => current_task["dueDate"].clone(),
        };
        let recurrence = match &changes.recurrence {
            Some(recurrence) => json!(recurrence),
            None => current_task["recurrence"].clone(),
        };
        if js::truthy(&recurrence) && !js::truthy(&due_date) {
            return Err(ApiError::invalid_field("A recurring issue requires a due date").into());
        }

        let mut assignments: Vec<String> = Vec::new();
        let mut values: Vec<JsValue> = Vec::new();
        for (key, change) in &activity_values {
            match *key {
                "developmentContext" => {
                    assignments.push("development_context_type = ?, development_branch = ?".into());
                    let context = changes.development_context.as_ref().and_then(Option::as_ref);
                    values.extend(development_columns(context).iter().map(db::arg));
                }
                "recurrence" => {
                    assignments.push("recurrence_interval = ?, recurrence_unit = ?".into());
                    values.extend(args![change["interval"], change["unit"]]);
                }
                key => {
                    let column = match key {
                        "projectId" => "project_id",
                        "startDate" => "start_date",
                        "dueDate" => "due_date",
                        column => column,
                    };
                    assignments.push(format!("{column} = ?"));
                    values.push(db::arg(change));
                }
            }
        }
        if let Some(status) = changes.status.filter(|status| current_task["status"] != *status) {
            let placement = if project_changed { destination } else { current_project };
            assignments.push("sort_order = ?".into());
            values.push(db::arg(&js::number(self.column_edge(placement, status, Some(current_id), true).await?)));
        }
        if input.assignee_target.is_some() {
            let assignee = resolve_assignee(input.assignee_target, &self.actor);
            assignments.push("assignee_type = ?, assignee_id = ?, assignee_name = ?, assignee_avatar_url = ?".into());
            values.extend(assignee_columns(&assignee));
            activity_values.push(("assignee", assignee));
        }
        if let Some(binding) = records::stored_thread_binding_for_existing(&current, &input.attribution)
            && changes.project_id.is_none()
        {
            assignments.push(THREAD_ASSIGNMENTS.trim_end_matches(", ").into());
            values.extend(binding.iter().map(db::arg));
        }
        if let Some(session) = records::stored_agent_session(&input.attribution) {
            assignments.push("agent_session = ?".into());
            values.push(db::arg(&session));
        }
        assignments.push("version = version + 1, updated_at = ?".into());
        let timestamp = now();
        values.extend(args![timestamp, current_id, input.version]);
        let relation_guard = if project_changed {
            values.extend(args![current_id, current_id]);
            " AND NOT EXISTS (SELECT 1 FROM task_relations WHERE source_task_id = ? OR target_task_id = ?)"
        } else {
            ""
        };
        let mut statements = vec![db::statement(
            &self.db,
            format!("UPDATE tasks SET {} WHERE id = ? AND version = ?{relation_guard}", assignments.join(", ")),
            values,
        )?];
        if changes.description.is_some() {
            // Immediately after the body UPDATE: changes() must describe that statement.
            statements.push(db::statement(
                &self.db,
                format!(
                    "UPDATE attachments SET body_fallback = 0, change_revision = {NEXT_REVISION}
                     WHERE task_id = ? AND comment_id IS NULL AND body_fallback = 1 AND changes() = 1"
                ),
                args![current_id],
            )?);
        }
        let activity = records::field_changes(&current_task, &activity_values);
        if !activity.is_empty() {
            statements.push(self.task_activity_statement(current_id, &activity, &timestamp, input.version + 1)?);
        }
        if project_changed {
            statements.push(db::statement(
                &self.db,
                "UPDATE projects SET updated_at = ?
                 WHERE id IN (?, ?)
                   AND EXISTS (SELECT 1 FROM tasks WHERE id = ? AND version = ? AND updated_at = ?)",
                args![timestamp, current_project, destination, current_id, input.version + 1, timestamp],
            )?);
        }
        if task_labels.as_array().is_some_and(|labels| !labels.is_empty()) {
            let labels = task_labels.to_string();
            statements.push(db::statement(
                &self.db,
                format!(
                    "UPDATE projects
                     SET labels = {MERGED_LABELS}, updated_at = ?
                     WHERE id = ?
                       AND EXISTS (SELECT 1 FROM tasks WHERE id = ? AND version = ? AND updated_at = ?)
                       AND EXISTS (
                         SELECT 1 FROM json_each(?) AS task_labels
                         WHERE NOT EXISTS (
                           SELECT 1 FROM json_each(projects.labels) AS project_labels
                           WHERE project_labels.value = task_labels.value
                         )
                       )"
                ),
                args![labels, timestamp, destination, current_id, input.version + 1, timestamp, labels],
            )?);
        }
        let results = self.db.batch(statements).await?;
        if !db::changed(&results[0])? {
            if project_changed && self.has_relations(current_id).await? {
                return Err(cross_project_relation().into());
            }
            return Err(self.task_conflict(current_id, input.version).await);
        }
        self.get_task(current_id).await
    }

    /// Runs a guarded task write with its activity, then returns the task.
    async fn guarded_write(&self, current: &Row, version: i64, update: (String, Vec<JsValue>), activity: Vec<Value>, timestamp: &str) -> Result<Value> {
        let id = text(current, "id");
        let mut statements = vec![db::statement(&self.db, update.0, update.1)?];
        if !activity.is_empty() {
            statements.push(self.task_activity_statement(id, &activity, timestamp, version + 1)?);
        }
        let results = self.db.batch(statements).await?;
        if !db::changed(&results[0])? {
            return Err(self.task_conflict(id, version).await);
        }
        self.get_task(id).await
    }

    pub async fn move_task(&self, id: &str, input: &Move) -> Result<Value> {
        let current = self.require_task_row(id).await?;
        assert_task_version(&current, input.version)?;
        if !value(&current, "archived_at").is_null() {
            return Err(ApiError::new(409, "TASK_ARCHIVED", "Archived tasks cannot be moved").into());
        }
        let current_id = text(&current, "id");
        let project_id = text(&current, "project_id");
        let sort_order = match input.sort_order {
            Some(sort_order) => sort_order,
            None if input.status != text(&current, "status") => {
                self.column_edge(project_id, input.status, Some(current_id), true).await?
            }
            None => self.column_edge(project_id, input.status, Some(current_id), false).await?,
        };
        let (attributed, attributed_values) = attribution_assignments(&current, &input.attribution);
        let timestamp = now();
        let mut values = args![input.status, js::number(sort_order)];
        values.extend(attributed_values);
        values.extend(args![timestamp, current_id, input.version]);
        let sql = format!(
            "UPDATE tasks SET status = ?, sort_order = ?, {attributed}version = version + 1, updated_at = ?
             WHERE id = ? AND version = ?"
        );
        let activity = records::field_changes(&records::task(&current), &[("status", json!(input.status))]);
        self.guarded_write(&current, input.version, (sql, values), activity, &timestamp).await
    }

    pub async fn archive_task(&self, id: &str, input: &VersionMutation) -> Result<Value> {
        let current = self.require_task_row(id).await?;
        assert_task_version(&current, input.version)?;
        let (attributed, attributed_values) = attribution_assignments(&current, &input.attribution);
        let timestamp = now();
        let mut values = args![timestamp];
        values.extend(attributed_values);
        values.extend(args![timestamp, text(&current, "id"), input.version]);
        let sql = format!(
            "UPDATE tasks SET archived_at = ?, {attributed}version = version + 1, updated_at = ?
             WHERE id = ? AND version = ?"
        );
        let activity = vec![json!({ "field": "archivedAt", "before": value(&current, "archived_at"), "after": timestamp })];
        self.guarded_write(&current, input.version, (sql, values), activity, &timestamp).await
    }

    pub async fn restore_task(&self, id: &str, input: &VersionMutation) -> Result<Value> {
        let current = self.require_task_row(id).await?;
        assert_task_version(&current, input.version)?;
        if value(&current, "archived_at").is_null() {
            return Err(ApiError::new(409, "TASK_NOT_ARCHIVED", "Only archived tasks can be restored").into());
        }
        let (attributed, attributed_values) = attribution_assignments(&current, &input.attribution);
        let timestamp = now();
        let mut values = attributed_values;
        values.extend(args![timestamp, text(&current, "id"), input.version]);
        let sql = format!(
            "UPDATE tasks SET archived_at = NULL, {attributed}version = version + 1, updated_at = ?
             WHERE id = ? AND version = ?"
        );
        let activity = vec![json!({ "field": "archivedAt", "before": value(&current, "archived_at"), "after": null })];
        self.guarded_write(&current, input.version, (sql, values), activity, &timestamp).await
    }

    pub async fn delete_archived_task(&self, id: &str, version: i64) -> Result<()> {
        let current = self.require_task_row(id).await?;
        assert_task_version(&current, version)?;
        let not_archived = || ApiError::new(409, "TASK_NOT_ARCHIVED", "Only archived tasks can be deleted");
        if value(&current, "archived_at").is_null() {
            return Err(not_archived().into());
        }
        let current_id = text(&current, "id");
        let results = self
            .db
            .batch(vec![
                db::statement(&self.db, "SELECT id FROM attachments WHERE task_id = ?", args![current_id])?,
                db::statement(
                    &self.db,
                    "DELETE FROM tasks WHERE id = ? AND version = ? AND archived_at IS NOT NULL",
                    args![current_id, version],
                )?,
            ])
            .await?;
        if !db::changed(&results[1])? {
            let latest = self.require_task_row(current_id).await?;
            assert_task_version(&latest, version)?;
            return Err(not_archived().into());
        }
        let ids: Vec<String> =
            results[0].results::<Row>()?.iter().map(|row| text(row, "id").to_owned()).collect();
        self.delete_objects(&ids).await
    }
}
