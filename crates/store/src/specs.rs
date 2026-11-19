//! The spec an agent writes onto a task: proposal, specs, design and a tasks checklist.

use broadcast_domain::input::TaskSpecPatch;
use broadcast_domain::records::Task;
use broadcast_domain::spec::{self, TaskSpec};
use broadcast_domain::{ApiError, Result};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};

use crate::tasks::require_task;
use crate::{Store, now};

impl Store {
    /// Writes the given sections and keeps the rest; the task version is unchanged.
    pub fn patch_task_spec(&self, task_id: &str, patch: &TaskSpecPatch) -> Result<Task> {
        let mut connection = self.connection();
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let task = require_task(&transaction, task_id)?;
        let current = read_spec(&transaction, &task.id)?;
        let section = |value: &Option<String>, current: Option<&String>| {
            value
                .clone()
                .or_else(|| current.cloned())
                .unwrap_or_default()
        };
        write_spec(
            &transaction,
            &TaskSpec {
                task_id: task.id.clone(),
                proposal: section(&patch.proposal, current.as_ref().map(|spec| &spec.proposal)),
                specs: section(&patch.specs, current.as_ref().map(|spec| &spec.specs)),
                design: section(&patch.design, current.as_ref().map(|spec| &spec.design)),
                tasks: section(&patch.tasks, current.as_ref().map(|spec| &spec.tasks)),
                updated_at: now(),
            },
        )?;
        let task = require_task(&transaction, &task.id)?;
        transaction.commit()?;
        Ok(task)
    }

    /// Checks or unchecks the task numbered `number` in the tasks checklist.
    pub fn check_task_spec(&self, task_id: &str, number: &str, done: bool) -> Result<Task> {
        let mut connection = self.connection();
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let task = require_task(&transaction, task_id)?;
        let not_found = || {
            ApiError::new(
                404,
                "SPEC_TASK_NOT_FOUND",
                format!("Spec task '{number}' does not exist on '{}'", task.id),
            )
        };
        let mut current = read_spec(&transaction, &task.id)?.ok_or_else(not_found)?;
        current.tasks = spec::check_task(&current.tasks, number, done).ok_or_else(not_found)?;
        current.updated_at = now();
        write_spec(&transaction, &current)?;
        let task = require_task(&transaction, &task.id)?;
        transaction.commit()?;
        Ok(task)
    }

    pub fn get_task_spec(&self, task_id: &str) -> Result<Option<TaskSpec>> {
        let connection = self.connection();
        let task = require_task(&connection, task_id)?;
        read_spec(&connection, &task.id)
    }
}

fn read_spec(connection: &Connection, task_id: &str) -> Result<Option<TaskSpec>> {
    Ok(connection
        .query_row(
            "SELECT task_id, proposal, specs, design, tasks, updated_at FROM task_specs WHERE task_id = ?",
            [task_id],
            |row| {
                Ok(TaskSpec {
                    task_id: row.get("task_id")?,
                    proposal: row.get("proposal")?,
                    specs: row.get("specs")?,
                    design: row.get("design")?,
                    tasks: row.get("tasks")?,
                    updated_at: row.get("updated_at")?,
                })
            },
        )
        .optional()?)
}

fn write_spec(connection: &Connection, spec: &TaskSpec) -> Result<()> {
    connection.execute(
        "INSERT INTO task_specs (task_id, proposal, specs, design, tasks, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)
         ON CONFLICT (task_id) DO UPDATE SET
           proposal = excluded.proposal, specs = excluded.specs, design = excluded.design,
           tasks = excluded.tasks, updated_at = excluded.updated_at",
        params![
            spec.task_id,
            spec.proposal,
            spec.specs,
            spec.design,
            spec.tasks,
            spec.updated_at
        ],
    )?;
    Ok(())
}
