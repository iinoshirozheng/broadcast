//! Projects and their label lists.

use broadcast_domain::ApiError;
use broadcast_domain::constants::DEFAULT_LABEL_NAMES;
use broadcast_domain::input::{Archived, ProjectCreate, TaskFilters};
use serde_json::{Value, json};

use crate::board::{Board, now};
use crate::db::{self, args, integer};
use crate::error::Result;
use crate::records;

const PROJECT_COLUMNS: &str = "projects.id, projects.name, projects.workspace_path, projects.labels, \
                               projects.created_at, projects.updated_at";

fn projects_query(filter: &str, order: &str) -> String {
    format!(
        "SELECT {PROJECT_COLUMNS}, COUNT(tasks.id) AS issue_count
         FROM projects
         LEFT JOIN tasks ON tasks.project_id = projects.id AND tasks.archived_at IS NULL
         {filter}
         GROUP BY {PROJECT_COLUMNS}
         {order}"
    )
}

fn not_found(id: &str) -> ApiError {
    ApiError::new(404, "PROJECT_NOT_FOUND", format!("Project '{id}' does not exist"))
}

impl Board {
    pub async fn list_projects(&self) -> Result<Vec<Value>> {
        let rows = db::all(&self.db, projects_query("", "ORDER BY projects.created_at, projects.id"), vec![]).await?;
        Ok(rows.iter().map(records::project).collect())
    }

    pub async fn get_project(&self, id: &str) -> Result<Option<Value>> {
        let row = db::first(&self.db, projects_query("WHERE projects.id = ?", ""), args![id]).await?;
        Ok(row.as_ref().map(records::project))
    }

    async fn project_record(&self, id: &str) -> Result<Value> {
        Ok(self.get_project(id).await?.unwrap_or(Value::Null))
    }

    pub async fn create_project(&self, input: &ProjectCreate) -> Result<Value> {
        let timestamp = now();
        let labels = json!(DEFAULT_LABEL_NAMES).to_string();
        let insert = db::statement(
            &self.db,
            "INSERT INTO projects (id, name, workspace_path, labels, next_task_number, created_at, updated_at)
             VALUES (?, ?, NULL, ?, 1, ?, ?)",
            args![input.id, input.name, labels, timestamp, timestamp],
        )?;
        match insert.run().await {
            Err(error) if db::message(&error).contains("UNIQUE constraint failed") => {
                return Err(ApiError::new(409, "PROJECT_EXISTS", format!("Project '{}' already exists", input.id)).into());
            }
            result => {
                result?;
            }
        }
        self.project_record(&input.id).await
    }

    pub async fn add_project_label(&self, project_id: &str, label: &str) -> Result<Value> {
        self.require_project(project_id).await?;
        db::run(
            &self.db,
            "UPDATE projects
             SET labels = json_insert(labels, '$[#]', ?), updated_at = ?
             WHERE id = ? AND NOT EXISTS (SELECT 1 FROM json_each(projects.labels) WHERE value = ?)",
            args![label, now(), project_id, label],
        )
        .await?;
        self.project_record(project_id).await
    }

    pub async fn delete_project_label(&self, project_id: &str, label: &str) -> Result<Value> {
        self.require_project(project_id).await?;
        let timestamp = now();
        let remove = |table: &str, extra: &str, filter: &str| {
            db::statement(
                &self.db,
                format!(
                    "UPDATE {table}
                     SET
                       labels = (SELECT COALESCE(json_group_array(value), '[]') FROM json_each({table}.labels) WHERE value != ?),
                       {extra}updated_at = ?
                     WHERE {filter} = ?
                       AND EXISTS (SELECT 1 FROM json_each({table}.labels) WHERE value = ?)"
                ),
                args![label, timestamp, project_id, label],
            )
        };
        self.db
            .batch(vec![remove("projects", "", "id")?, remove("tasks", "version = version + 1, ", "project_id")?])
            .await?;
        self.project_record(project_id).await
    }

    pub async fn delete_project(&self, id: &str) -> Result<Value> {
        let project = self.get_project(id).await?.ok_or_else(|| not_found(id))?;
        if !id.starts_with("temp-") {
            return Err(
                ApiError::new(403, "PROJECT_DELETE_FORBIDDEN", "Only manually created projects can be deleted").into()
            );
        }
        let result = db::run(
            &self.db,
            "DELETE FROM projects WHERE id = ? AND NOT EXISTS (SELECT 1 FROM tasks WHERE project_id = ?)",
            args![id, id],
        )
        .await?;
        if !db::changed(&result)? {
            let count = db::first(&self.db, "SELECT COUNT(*) AS issue_count FROM tasks WHERE project_id = ?", args![id])
                .await?
                .map_or(0, |row| integer(&row, "issue_count"));
            return Err(ApiError::new(409, "PROJECT_NOT_EMPTY", "Project still contains issues")
                .with_details(json!({ "issueCount": count }))
                .into());
        }
        Ok(project)
    }

    pub async fn list_tasks(&self, filters: &TaskFilters) -> Result<Vec<Value>> {
        let mut clauses = Vec::new();
        let mut values = Vec::new();
        if let Some(project_id) = filters.project_id.as_deref().filter(|id| !id.is_empty()) {
            clauses.push("project_id = ?");
            values.push(db::arg(project_id));
        }
        if let Some(status) = filters.status.as_deref().filter(|status| !status.is_empty()) {
            clauses.push("status = ?");
            values.push(db::arg(status));
        }
        match filters.archived {
            Archived::Exclude => clauses.push("archived_at IS NULL"),
            Archived::Only => clauses.push("archived_at IS NOT NULL"),
            Archived::All => {}
        }
        let filter = if clauses.is_empty() { String::new() } else { format!("WHERE {}", clauses.join(" AND ")) };
        let rows = db::all(
            &self.db,
            format!(
                "SELECT * FROM tasks
                 {filter}
                 ORDER BY
                   CASE status
                     WHEN 'backlog' THEN 1 WHEN 'todo' THEN 2 WHEN 'in_progress' THEN 3 WHEN 'in_review' THEN 4
                     WHEN 'blocked' THEN 5 WHEN 'done' THEN 6 WHEN 'canceled' THEN 7
                   END,
                   sort_order, created_at, id"
            ),
            values,
        )
        .await?;
        self.hydrate_tasks(rows).await
    }
}
