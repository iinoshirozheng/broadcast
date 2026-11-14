use broadcast_domain::constants::DEFAULT_LABEL_NAMES;
use broadcast_domain::input::ProjectCreate;
use broadcast_domain::records::Project;
use broadcast_domain::{ApiError, Result};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use serde_json::json;

use crate::{Store, now, rows};

const PROJECT_COLUMNS: &str = "
    SELECT
      projects.id, projects.name, projects.workspace_path, projects.labels,
      projects.created_at, projects.updated_at, COUNT(tasks.id) AS issue_count
    FROM projects
    LEFT JOIN tasks ON tasks.project_id = projects.id AND tasks.archived_at IS NULL";
const PROJECT_GROUP: &str = "
    GROUP BY projects.id, projects.name, projects.workspace_path, projects.labels,
      projects.created_at, projects.updated_at";

pub(crate) fn project_not_found(id: &str) -> ApiError {
    ApiError::new(404, "PROJECT_NOT_FOUND", format!("Project '{id}' does not exist"))
}

fn get_project(connection: &Connection, id: &str) -> Result<Option<Project>> {
    let sql = format!("{PROJECT_COLUMNS} WHERE projects.id = ? {PROJECT_GROUP}");
    Ok(connection.query_row(&sql, [id], rows::project).optional()?)
}

impl Store {
    pub fn list_projects(&self) -> Result<Vec<Project>> {
        let connection = self.connection();
        let sql = format!("{PROJECT_COLUMNS} {PROJECT_GROUP} ORDER BY projects.created_at, projects.id");
        let mut statement = connection.prepare(&sql)?;
        let projects = statement.query_map([], rows::project)?.collect::<rusqlite::Result<_>>()?;
        Ok(projects)
    }

    pub fn get_project(&self, id: &str) -> Result<Option<Project>> {
        get_project(&self.connection(), id)
    }

    pub fn create_project(&self, input: &ProjectCreate) -> Result<Project> {
        let connection = self.connection();
        let timestamp = now();
        let inserted = connection.execute(
            "INSERT INTO projects (id, name, workspace_path, labels, next_task_number, created_at, updated_at)
             VALUES (?, ?, ?, ?, 1, ?, ?)",
            params![
                input.id,
                input.name,
                input.workspace_path,
                serde_json::to_string(&DEFAULT_LABEL_NAMES)?,
                timestamp,
                timestamp,
            ],
        );
        if let Err(rusqlite::Error::SqliteFailure(_, Some(message))) = &inserted
            && message.contains("UNIQUE constraint failed")
        {
            return Err(ApiError::new(409, "PROJECT_EXISTS", format!("Project '{}' already exists", input.id)).into());
        }
        inserted?;
        get_project(&connection, &input.id)?.ok_or_else(|| project_not_found(&input.id).into())
    }

    pub fn delete_project(&self, id: &str) -> Result<()> {
        let connection = self.connection();
        if get_project(&connection, id)?.is_none() {
            return Err(project_not_found(id).into());
        }
        if !id.starts_with("temp-") {
            return Err(
                ApiError::new(403, "PROJECT_DELETE_FORBIDDEN", "Only manually created projects can be deleted").into()
            );
        }
        let deleted = connection.execute(
            "DELETE FROM projects WHERE id = ?1 AND NOT EXISTS (SELECT 1 FROM tasks WHERE project_id = ?1)",
            [id],
        )?;
        if deleted != 1 {
            let issue_count: i64 =
                connection.query_row("SELECT COUNT(*) FROM tasks WHERE project_id = ?", [id], |row| row.get(0))?;
            return Err(ApiError::new(409, "PROJECT_NOT_EMPTY", "Project still contains issues")
                .with_details(json!({ "issueCount": issue_count }))
                .into());
        }
        Ok(())
    }
}

fn project_labels(connection: &Connection, id: &str) -> Result<Vec<String>> {
    let labels: Option<String> =
        connection.query_row("SELECT labels FROM projects WHERE id = ?", [id], |row| row.get(0)).optional()?;
    let labels = labels.ok_or_else(|| project_not_found(id))?;
    Ok(serde_json::from_str(&labels)?)
}

fn require_project(connection: &Connection, id: &str) -> Result<Project> {
    get_project(connection, id)?.ok_or_else(|| project_not_found(id).into())
}

impl Store {
    /// `addProjectLabel`.
    pub fn add_project_label(&self, project_id: &str, label: &str) -> Result<Project> {
        let connection = self.connection();
        let mut labels = project_labels(&connection, project_id)?;
        if !labels.iter().any(|current| current == label) {
            labels.push(label.to_owned());
            connection.execute(
                "UPDATE projects SET labels = ?, updated_at = ? WHERE id = ?",
                params![serde_json::to_string(&labels)?, now(), project_id],
            )?;
        }
        require_project(&connection, project_id)
    }

    /// `deleteProjectLabel`: also strips the label from every task in the project.
    pub fn delete_project_label(&self, project_id: &str, label: &str) -> Result<Project> {
        let mut connection = self.connection();
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let labels = project_labels(&transaction, project_id)?;
        let timestamp = now();
        if labels.iter().any(|current| current == label) {
            let kept: Vec<&String> = labels.iter().filter(|current| *current != label).collect();
            transaction.execute(
                "UPDATE projects SET labels = ?, updated_at = ? WHERE id = ?",
                params![serde_json::to_string(&kept)?, timestamp, project_id],
            )?;
        }
        let tasks = transaction
            .prepare("SELECT id, labels FROM tasks WHERE project_id = ?")?
            .query_map([project_id], |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        for (id, task_labels) in tasks {
            let task_labels: Vec<String> = serde_json::from_str(&task_labels)?;
            if task_labels.iter().any(|current| current == label) {
                let kept: Vec<&String> = task_labels.iter().filter(|current| *current != label).collect();
                transaction.execute(
                    "UPDATE tasks SET labels = ?, version = version + 1, updated_at = ? WHERE id = ?",
                    params![serde_json::to_string(&kept)?, timestamp, id],
                )?;
            }
        }
        transaction.commit()?;
        require_project(&connection, project_id)
    }
}
