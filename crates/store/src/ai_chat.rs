//! Local AI conversations: threads, their Codex runs and the event log.

use broadcast_domain::records::{AiChatEvent, AiChatOrigin, AiChatRun, AiChatThread, AiChatTodo};
use broadcast_domain::{ApiError, Result, js};
use rusqlite::{Connection, OptionalExtension, Row, TransactionBehavior, params};
use serde_json::Value;
use uuid::Uuid;

use crate::rows::optional_json;
use crate::{Store, now};

pub struct NewAiChatThread {
    pub title: String,
    pub origin: AiChatOrigin,
    pub model: String,
    pub reasoning_effort: String,
    pub sandbox: String,
}

/// Columns a thread update may set; `None` leaves a column unchanged.
#[derive(Default)]
pub struct AiChatThreadChanges {
    pub title: Option<String>,
    pub codex_thread_id: Option<String>,
    pub model: Option<String>,
    pub reasoning_effort: Option<String>,
    pub sandbox: Option<String>,
}

/// A finished run: `running` runs are only ever created, never updated back to running.
pub struct AiChatRunOutcome {
    pub status: &'static str,
    pub exit_code: Option<i64>,
    pub error: Option<String>,
}

pub struct NewAiChatEvent<'a> {
    pub thread_id: &'a str,
    pub run_id: Option<&'a str>,
    pub kind: &'a str,
    pub role: &'a str,
    pub content: &'a str,
    pub data: Option<&'a Value>,
}

fn thread_not_found(id: &str) -> ApiError {
    ApiError::new(404, "AI_CHAT_THREAD_NOT_FOUND", format!("AI chat thread '{id}' does not exist"))
}

fn run(row: &Row<'_>) -> rusqlite::Result<AiChatRun> {
    Ok(AiChatRun {
        id: row.get("id")?,
        thread_id: row.get("thread_id")?,
        status: row.get("status")?,
        exit_code: row.get("exit_code")?,
        error: row.get("error")?,
        started_at: row.get("started_at")?,
        finished_at: row.get("finished_at")?,
    })
}

fn thread(row: &Row<'_>) -> rusqlite::Result<AiChatThread> {
    let filled = |column: &str| -> rusqlite::Result<Option<String>> {
        Ok(row.get::<_, Option<String>>(column)?.filter(|value| !value.is_empty()))
    };
    Ok(AiChatThread {
        id: row.get("id")?,
        title: row.get("title")?,
        status: row.get("status")?,
        origin: AiChatOrigin {
            project_id: row.get("origin_project_id")?,
            project_name: row.get("origin_project_name")?,
            workspace_path: row.get("origin_workspace_path")?,
            codex_project_id: filled("origin_codex_project_id")?,
            codex_project_kind: filled("origin_codex_project_kind")?,
            codex_host_id: filled("origin_codex_host_id")?,
            issue_id: filled("origin_issue_id")?,
            issue_identifier: filled("origin_issue_identifier")?,
        },
        codex_thread_id: row.get("codex_thread_id")?,
        model: row.get("model")?,
        reasoning_effort: row.get("reasoning_effort")?,
        sandbox: row.get("sandbox")?,
        current_run: None,
        latest_todo: None,
        created_at: row.get("created_at")?,
        updated_at: row.get("updated_at")?,
    })
}

fn event(row: &Row<'_>) -> rusqlite::Result<AiChatEvent> {
    Ok(AiChatEvent {
        id: row.get("id")?,
        thread_id: row.get("thread_id")?,
        run_id: row.get("run_id")?,
        kind: row.get("type")?,
        role: row.get("role")?,
        content: row.get("content")?,
        data: optional_json(row, "data")?,
        created_at: row.get("created_at")?,
    })
}

/// A `todo_list` event row: id, run id, data and creation time.
struct TodoRow {
    id: String,
    run_id: Option<String>,
    data: Option<String>,
    created_at: String,
}

fn todo_row(row: &Row<'_>) -> rusqlite::Result<(String, TodoRow)> {
    Ok((
        row.get("thread_id")?,
        TodoRow { id: row.get("id")?, run_id: row.get("run_id")?, data: row.get("data")?, created_at: row.get("created_at")? },
    ))
}

/// `parseAiChatTodoProgress`: the plan items of a `todo_list` event, if it has any.
fn todo_progress(row: &TodoRow) -> Option<AiChatTodo> {
    let data: Value = serde_json::from_str(row.data.as_deref()?).ok()?;
    let detail = match &data["detail"] {
        Value::String(text) => serde_json::from_str(text).ok()?,
        detail => detail.clone(),
    };
    let items: Vec<&Value> = detail
        .as_array()?
        .iter()
        .filter(|item| item["text"].as_str().is_some_and(|text| !js::trim(text).is_empty()))
        .collect();
    if items.is_empty() {
        return None;
    }
    Some(AiChatTodo {
        completed: items.iter().filter(|item| item["completed"] == Value::Bool(true)).count(),
        total: items.len(),
        event_id: row.id.clone(),
        updated_at: row.created_at.clone(),
    })
}

/// The newest parseable plan, from the current run when one is running.
fn latest_todo<'a>(rows: impl Iterator<Item = &'a TodoRow>, current_run: Option<&AiChatRun>) -> Option<AiChatTodo> {
    rows.filter(|row| current_run.is_none_or(|run| row.run_id.as_deref() == Some(run.id.as_str())))
        .find_map(todo_progress)
}

fn get_thread(connection: &Connection, id: &str) -> Result<Option<AiChatThread>> {
    let Some(mut thread) =
        connection.query_row("SELECT * FROM ai_chat_threads WHERE id = ?", [id], thread).optional()?
    else {
        return Ok(None);
    };
    thread.current_run = connection
        .query_row(
            "SELECT * FROM ai_chat_runs WHERE thread_id = ? AND status = 'running'
             ORDER BY started_at DESC, id DESC LIMIT 1",
            [id],
            run,
        )
        .optional()?;
    let todos: Vec<TodoRow> = connection
        .prepare(
            "SELECT id, thread_id, run_id, data, created_at FROM ai_chat_events
             WHERE thread_id = ? AND type = 'todo_list' ORDER BY created_at DESC, rowid DESC",
        )?
        .query_map([id], todo_row)?
        .map(|row| row.map(|(_, todo)| todo))
        .collect::<rusqlite::Result<_>>()?;
    thread.latest_todo = latest_todo(todos.iter(), thread.current_run.as_ref());
    Ok(Some(thread))
}

fn get_run(connection: &Connection, id: &str) -> Result<Option<AiChatRun>> {
    Ok(connection.query_row("SELECT * FROM ai_chat_runs WHERE id = ?", [id], run).optional()?)
}

impl Store {
    pub fn list_ai_chat_threads(&self) -> Result<Vec<AiChatThread>> {
        let connection = self.connection();
        let mut threads: Vec<AiChatThread> = connection
            .prepare("SELECT * FROM ai_chat_threads ORDER BY updated_at DESC, id")?
            .query_map([], thread)?
            .collect::<rusqlite::Result<_>>()?;
        if threads.is_empty() {
            return Ok(threads);
        }
        let mut current = std::collections::HashMap::new();
        for found in connection
            .prepare("SELECT * FROM ai_chat_runs WHERE status = 'running' ORDER BY thread_id, started_at DESC, id DESC")?
            .query_map([], run)?
        {
            let found = found?;
            current.entry(found.thread_id.clone()).or_insert(found);
        }
        let mut todos: std::collections::HashMap<String, Vec<TodoRow>> = std::collections::HashMap::new();
        for found in connection
            .prepare(
                "SELECT id, thread_id, run_id, data, created_at FROM ai_chat_events
                 WHERE type = 'todo_list' ORDER BY thread_id, created_at DESC, rowid DESC",
            )?
            .query_map([], todo_row)?
        {
            let (thread_id, todo) = found?;
            todos.entry(thread_id).or_default().push(todo);
        }
        for thread in &mut threads {
            thread.current_run = current.remove(&thread.id);
            let rows = todos.get(&thread.id).map(Vec::as_slice).unwrap_or_default();
            thread.latest_todo = latest_todo(rows.iter(), thread.current_run.as_ref());
        }
        Ok(threads)
    }

    pub fn get_ai_chat_thread(&self, id: &str) -> Result<Option<AiChatThread>> {
        get_thread(&self.connection(), id)
    }

    /// Whether a conversation linked to `issue_ref` (id or identifier) belongs to another project.
    pub fn has_ai_chat_thread_project_conflict(&self, issue_ref: &str, project_id: &str) -> Result<bool> {
        let found = self
            .connection()
            .query_row(
                "SELECT 1 FROM ai_chat_threads
                 WHERE (origin_issue_id = ?1 OR origin_issue_identifier = ?1) AND origin_project_id != ?2 LIMIT 1",
                params![issue_ref, project_id],
                |_| Ok(()),
            )
            .optional()?;
        Ok(found.is_some())
    }

    pub fn create_ai_chat_thread(&self, input: NewAiChatThread) -> Result<AiChatThread> {
        let id = Uuid::new_v4().to_string();
        let timestamp = now();
        let connection = self.connection();
        let origin = &input.origin;
        connection.execute(
            "INSERT INTO ai_chat_threads (
               id, title, status,
               origin_project_id, origin_project_name, origin_workspace_path,
               origin_codex_project_id, origin_codex_project_kind, origin_codex_host_id,
               origin_issue_id, origin_issue_identifier,
               codex_thread_id, model, reasoning_effort, sandbox, created_at, updated_at
             ) VALUES (?, ?, 'idle', ?, ?, ?, ?, ?, ?, ?, ?, NULL, ?, ?, ?, ?, ?)",
            params![
                id,
                input.title,
                origin.project_id,
                origin.project_name,
                origin.workspace_path,
                origin.codex_project_id,
                origin.codex_project_kind,
                origin.codex_host_id,
                origin.issue_id,
                origin.issue_identifier,
                input.model,
                input.reasoning_effort,
                input.sandbox,
                timestamp,
                timestamp,
            ],
        )?;
        get_thread(&connection, &id)?.ok_or_else(|| thread_not_found(&id).into())
    }

    pub fn update_ai_chat_thread(&self, id: &str, changes: AiChatThreadChanges) -> Result<AiChatThread> {
        let connection = self.connection();
        let current = get_thread(&connection, id)?.ok_or_else(|| thread_not_found(id))?;
        let columns = [
            ("title", changes.title),
            ("codex_thread_id", changes.codex_thread_id),
            ("model", changes.model),
            ("reasoning_effort", changes.reasoning_effort),
            ("sandbox", changes.sandbox),
        ];
        let (assignments, mut values): (Vec<String>, Vec<String>) = columns
            .into_iter()
            .filter_map(|(column, value)| value.map(|value| (format!("{column} = ?"), value)))
            .unzip();
        if assignments.is_empty() {
            return Ok(current);
        }
        values.extend([now(), id.to_owned()]);
        connection.execute(
            &format!("UPDATE ai_chat_threads SET {}, updated_at = ? WHERE id = ?", assignments.join(", ")),
            rusqlite::params_from_iter(values),
        )?;
        get_thread(&connection, id)?.ok_or_else(|| thread_not_found(id).into())
    }

    pub fn delete_ai_chat_thread(&self, id: &str) -> Result<AiChatThread> {
        let connection = self.connection();
        let current = get_thread(&connection, id)?.ok_or_else(|| thread_not_found(id))?;
        connection.execute("DELETE FROM ai_chat_threads WHERE id = ?", [id])?;
        Ok(current)
    }

    pub fn list_ai_chat_runs(&self, thread_id: &str) -> Result<Vec<AiChatRun>> {
        let connection = self.connection();
        let runs = connection
            .prepare("SELECT * FROM ai_chat_runs WHERE thread_id = ? ORDER BY started_at, id")?
            .query_map([thread_id], run)?
            .collect::<rusqlite::Result<_>>()?;
        Ok(runs)
    }

    pub fn get_ai_chat_run(&self, id: &str) -> Result<Option<AiChatRun>> {
        get_run(&self.connection(), id)
    }

    /// Starts a running run and marks its thread running; the unique index allows one per thread.
    pub fn create_ai_chat_run(&self, thread_id: &str) -> Result<AiChatRun> {
        let id = Uuid::new_v4().to_string();
        let timestamp = now();
        let mut connection = self.connection();
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        transaction.execute(
            "INSERT INTO ai_chat_runs (id, thread_id, status, exit_code, error, started_at, finished_at)
             VALUES (?, ?, 'running', NULL, NULL, ?, NULL)",
            params![id, thread_id, timestamp],
        )?;
        transaction.execute(
            "UPDATE ai_chat_threads SET status = 'running', updated_at = ? WHERE id = ?",
            params![timestamp, thread_id],
        )?;
        transaction.commit()?;
        get_run(&connection, &id)?.ok_or_else(|| run_not_found(&id).into())
    }

    /// Finishes a run; its thread turns `failed` or `idle` once no run is left running.
    pub fn finish_ai_chat_run(&self, id: &str, outcome: AiChatRunOutcome) -> Result<AiChatRun> {
        let finished_at = now();
        let mut connection = self.connection();
        let current = get_run(&connection, id)?.ok_or_else(|| run_not_found(id))?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        transaction.execute(
            "UPDATE ai_chat_runs SET status = ?, exit_code = ?, error = ?, finished_at = ? WHERE id = ?",
            params![outcome.status, outcome.exit_code, outcome.error, finished_at, id],
        )?;
        let thread_status = if outcome.status == "failed" { "failed" } else { "idle" };
        transaction.execute(
            "UPDATE ai_chat_threads SET status = ?1, updated_at = ?2
             WHERE id = ?3 AND NOT EXISTS (SELECT 1 FROM ai_chat_runs WHERE thread_id = ?3 AND status = 'running')",
            params![thread_status, finished_at, current.thread_id],
        )?;
        transaction.commit()?;
        get_run(&connection, id)?.ok_or_else(|| run_not_found(id).into())
    }

    pub fn insert_ai_chat_event(&self, input: NewAiChatEvent<'_>) -> Result<AiChatEvent> {
        let id = Uuid::new_v4().to_string();
        let connection = self.connection();
        connection.execute(
            "INSERT INTO ai_chat_events (id, thread_id, run_id, type, role, content, data, created_at)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
            params![
                id,
                input.thread_id,
                input.run_id,
                input.kind,
                input.role,
                input.content,
                input.data.filter(|data| !data.is_null()).map(Value::to_string),
                now(),
            ],
        )?;
        Ok(connection.query_row("SELECT * FROM ai_chat_events WHERE id = ?", [&id], event)?)
    }

    pub fn list_ai_chat_events(&self, thread_id: &str) -> Result<Vec<AiChatEvent>> {
        let connection = self.connection();
        let events = connection
            .prepare("SELECT * FROM ai_chat_events WHERE thread_id = ? ORDER BY created_at, rowid")?
            .query_map([thread_id], event)?
            .collect::<rusqlite::Result<_>>()?;
        Ok(events)
    }
}

fn run_not_found(id: &str) -> ApiError {
    ApiError::new(404, "AI_CHAT_RUN_NOT_FOUND", format!("AI chat run '{id}' does not exist"))
}

/// `interruptAbandonedAiChatRuns`: runs left running by a previous process cannot finish.
pub(crate) fn interrupt_abandoned_runs(connection: &mut Connection) -> Result<()> {
    let timestamp = now();
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let changed = transaction.execute(
        "UPDATE ai_chat_runs SET status = 'interrupted',
           error = COALESCE(error, 'Broadcast service restarted'), finished_at = COALESCE(finished_at, ?)
         WHERE status = 'running'",
        [&timestamp],
    )?;
    if changed > 0 {
        transaction.execute(
            "UPDATE ai_chat_threads SET status = 'idle', updated_at = ?
             WHERE status = 'running' AND NOT EXISTS (
               SELECT 1 FROM ai_chat_runs WHERE ai_chat_runs.thread_id = ai_chat_threads.id AND ai_chat_runs.status = 'running'
             )",
            [&timestamp],
        )?;
    }
    transaction.commit()?;
    Ok(())
}
