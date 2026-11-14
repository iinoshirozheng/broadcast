//! `syncJiraTasks`: the Jira project mirrors the issues assigned to the connected Jira user.

use std::collections::HashSet;

use broadcast_domain::Result;
use broadcast_domain::constants::JIRA_PROJECT_ID;
use rusqlite::{OptionalExtension, TransactionBehavior, params};

use crate::{Store, now};

/// A Jira user as a Broadcast actor; Jira actors are always users.
pub struct JiraActor {
    pub id: String,
    pub name: String,
    pub avatar_url: Option<String>,
}

/// A Jira issue normalized into task columns. `id` is also the task identifier.
pub struct JiraTask {
    pub id: String,
    pub title: String,
    pub description: String,
    pub status: &'static str,
    pub priority: &'static str,
    pub labels: Vec<String>,
    pub sort_order: f64,
    pub creator: JiraActor,
    pub assignee: JiraActor,
    pub due_date: Option<String>,
    pub external_origin: String,
    pub external_id: String,
    pub external_key: String,
    pub external_url: String,
    pub created_at: String,
    pub updated_at: String,
}

impl Store {
    /// Upserts the Jira project and its tasks; a task is rewritten only when a synced column differs.
    /// With `archive_missing`, tasks no longer returned by Jira are archived.
    pub fn sync_jira_tasks(&self, tasks: &[JiraTask], project_name: &str, archive_missing: bool) -> Result<()> {
        let timestamp = now();
        let mut project_labels: Vec<&str> = Vec::new();
        for label in tasks.iter().flat_map(|task| &task.labels) {
            if !project_labels.contains(&label.as_str()) {
                project_labels.push(label);
            }
        }
        let mut connection = self.connection();
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        transaction.execute(
            "INSERT INTO projects (id, name, workspace_path, labels, next_task_number, created_at, updated_at)
             VALUES (?1, ?2, NULL, ?3, 1, ?4, ?4)
             ON CONFLICT(id) DO UPDATE SET name = excluded.name, labels = excluded.labels, updated_at = excluded.updated_at",
            params![JIRA_PROJECT_ID, project_name, serde_json::to_string(&project_labels)?, timestamp],
        )?;
        let mut seen = HashSet::new();
        for task in tasks {
            let labels = serde_json::to_string(&task.labels)?;
            let existing: Option<String> = transaction
                .query_row(
                    "SELECT id FROM tasks WHERE external_source = 'jira' AND external_origin = ?1 AND external_id = ?2",
                    params![task.external_origin, task.external_id],
                    |row| row.get(0),
                )
                .optional()?;
            let Some(existing) = existing else {
                transaction.execute(
                    "INSERT INTO tasks (
                       id, identifier, project_id, title, description, status, priority, labels, sort_order,
                       creator_type, creator_id, creator_name, creator_avatar_url,
                       assignee_type, assignee_id, assignee_name, assignee_avatar_url,
                       due_date, external_source, external_origin, external_id, external_key, external_url,
                       version, created_at, updated_at
                     ) VALUES (
                       ?1, ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8,
                       'user', ?9, ?10, ?11,
                       'user', ?12, ?13, ?14,
                       ?15, 'jira', ?16, ?17, ?18, ?19,
                       1, ?20, ?21
                     )",
                    params![
                        task.id,
                        JIRA_PROJECT_ID,
                        task.title,
                        task.description,
                        task.status,
                        task.priority,
                        labels,
                        task.sort_order,
                        task.creator.id,
                        task.creator.name,
                        task.creator.avatar_url,
                        task.assignee.id,
                        task.assignee.name,
                        task.assignee.avatar_url,
                        task.due_date,
                        task.external_origin,
                        task.external_id,
                        task.external_key,
                        task.external_url,
                        task.created_at,
                        task.updated_at,
                    ],
                )?;
                seen.insert(task.id.clone());
                continue;
            };
            transaction.execute(
                "UPDATE tasks SET
                   identifier = ?1, title = ?2, description = ?3, status = ?4, priority = ?5, labels = ?6,
                   sort_order = ?7, creator_type = 'user', creator_id = ?8, creator_name = ?9, creator_avatar_url = ?10,
                   assignee_type = 'user', assignee_id = ?11, assignee_name = ?12, assignee_avatar_url = ?13,
                   due_date = ?14, external_origin = ?15, external_id = ?16, external_key = ?17, external_url = ?18,
                   archived_at = NULL, version = version + 1, updated_at = ?19
                 WHERE id = ?20 AND (
                   identifier IS NOT ?1 OR title IS NOT ?2 OR description IS NOT ?3 OR status IS NOT ?4
                   OR priority IS NOT ?5 OR labels IS NOT ?6 OR sort_order IS NOT ?7
                   OR creator_type IS NOT 'user' OR creator_id IS NOT ?8 OR creator_name IS NOT ?9
                   OR creator_avatar_url IS NOT ?10 OR assignee_type IS NOT 'user' OR assignee_id IS NOT ?11
                   OR assignee_name IS NOT ?12 OR assignee_avatar_url IS NOT ?13 OR due_date IS NOT ?14
                   OR external_origin IS NOT ?15 OR external_id IS NOT ?16 OR external_key IS NOT ?17
                   OR external_url IS NOT ?18 OR archived_at IS NOT NULL
                 )",
                params![
                    task.id,
                    task.title,
                    task.description,
                    task.status,
                    task.priority,
                    labels,
                    task.sort_order,
                    task.creator.id,
                    task.creator.name,
                    task.creator.avatar_url,
                    task.assignee.id,
                    task.assignee.name,
                    task.assignee.avatar_url,
                    task.due_date,
                    task.external_origin,
                    task.external_id,
                    task.external_key,
                    task.external_url,
                    task.updated_at,
                    existing,
                ],
            )?;
            seen.insert(existing);
        }
        if archive_missing {
            let active: Vec<String> = transaction
                .prepare("SELECT id FROM tasks WHERE project_id = ?1 AND external_source = 'jira' AND archived_at IS NULL")?
                .query_map([JIRA_PROJECT_ID], |row| row.get(0))?
                .collect::<rusqlite::Result<_>>()?;
            for id in active.iter().filter(|id| !seen.contains(*id)) {
                transaction.execute(
                    "UPDATE tasks SET archived_at = ?1, version = version + 1, updated_at = ?1 WHERE id = ?2",
                    params![timestamp, id],
                )?;
            }
        }
        transaction.execute("UPDATE projects SET updated_at = ?1 WHERE id = ?2", params![timestamp, JIRA_PROJECT_ID])?;
        transaction.commit()?;
        Ok(())
    }
}
