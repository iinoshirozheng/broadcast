//! Project README, its inline attachments, and AI project summaries.

use broadcast_domain::records::{ProjectReadme, ProjectReadmeAttachment, ProjectSummary};
use broadcast_domain::{ApiError, Result};
use rusqlite::{Connection, OptionalExtension, Row, TransactionBehavior, params};
use serde_json::json;

use crate::projects::project_not_found;
use crate::{Store, now, rows};

fn require_project_exists(connection: &Connection, id: &str) -> Result<()> {
    let exists = connection.query_row("SELECT 1 FROM projects WHERE id = ?", [id], |_| Ok(())).optional()?;
    exists.ok_or_else(|| project_not_found(id).into())
}

fn readme(row: &Row<'_>) -> rusqlite::Result<ProjectReadme> {
    Ok(ProjectReadme {
        project_id: row.get("project_id")?,
        content: row.get("content")?,
        version: row.get("version")?,
        created_at: row.get("created_at")?,
        updated_at: row.get("updated_at")?,
    })
}

fn summary(row: &Row<'_>) -> rusqlite::Result<ProjectSummary> {
    Ok(ProjectSummary {
        project_id: row.get("project_id")?,
        summary: row.get("summary")?,
        generated_at: row.get("generated_at")?,
        attempted_at: row.get("attempted_at")?,
        error: row.get("error")?,
    })
}

fn get_readme(connection: &Connection, project_id: &str) -> Result<ProjectReadme> {
    require_project_exists(connection, project_id)?;
    let row = connection
        .query_row(
            "SELECT project_id, content, version, created_at, updated_at FROM project_readmes WHERE project_id = ?",
            [project_id],
            readme,
        )
        .optional()?;
    Ok(row.unwrap_or_else(|| ProjectReadme {
        project_id: project_id.to_owned(),
        content: String::new(),
        version: 0,
        created_at: None,
        updated_at: None,
    }))
}

fn get_summary(connection: &Connection, project_id: &str) -> Result<ProjectSummary> {
    let row = connection
        .query_row(
            "SELECT project_id, summary, generated_at, attempted_at, error FROM project_summaries WHERE project_id = ?",
            [project_id],
            summary,
        )
        .optional()?;
    Ok(row.unwrap_or_else(|| ProjectSummary {
        project_id: project_id.to_owned(),
        summary: None,
        generated_at: None,
        attempted_at: None,
        error: None,
    }))
}

impl Store {
    pub fn get_project_readme(&self, project_id: &str) -> Result<ProjectReadme> {
        get_readme(&self.connection(), project_id)
    }

    pub fn save_project_readme(&self, project_id: &str, content: &str, expected: Option<i64>) -> Result<ProjectReadme> {
        let timestamp = now();
        let mut connection = self.connection();
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        require_project_exists(&transaction, project_id)?;
        let current: Option<i64> = transaction
            .query_row("SELECT version FROM project_readmes WHERE project_id = ?", [project_id], |row| row.get(0))
            .optional()?;
        if let Some(expected) = expected {
            let actual = current.unwrap_or(0);
            if actual != expected {
                return Err(ApiError::new(409, "VERSION_CONFLICT", "Project README changed since it was last read")
                    .with_details(json!({ "expectedVersion": expected, "actualVersion": actual }))
                    .into());
            }
        }
        if current.is_some() {
            transaction.execute(
                "UPDATE project_readmes SET content = ?, version = version + 1, updated_at = ? WHERE project_id = ?",
                params![content, timestamp, project_id],
            )?;
        } else {
            transaction.execute(
                "INSERT INTO project_readmes (project_id, content, version, created_at, updated_at) VALUES (?, ?, 1, ?, ?)",
                params![project_id, content, timestamp, timestamp],
            )?;
        }
        transaction.commit()?;
        get_readme(&connection, project_id)
    }

    pub fn create_project_readme_attachment(
        &self,
        project_id: &str,
        id: &str,
        filename: &str,
        content_type: &str,
        size: i64,
    ) -> Result<ProjectReadmeAttachment> {
        let connection = self.connection();
        require_project_exists(&connection, project_id)?;
        connection.execute(
            "INSERT INTO project_readme_attachments (id, project_id, filename, content_type, size, created_at)
             VALUES (?, ?, ?, ?, ?, ?)",
            params![id, project_id, filename, content_type, size, now()],
        )?;
        Ok(connection.query_row("SELECT * FROM project_readme_attachments WHERE id = ?", [id], rows::readme_attachment)?)
    }

    pub fn get_project_readme_attachment(&self, id: &str) -> Result<Option<ProjectReadmeAttachment>> {
        let connection = self.connection();
        Ok(connection
            .query_row("SELECT * FROM project_readme_attachments WHERE id = ?", [id], rows::readme_attachment)
            .optional()?)
    }

    pub fn get_project_summary(&self, project_id: &str) -> Result<ProjectSummary> {
        get_summary(&self.connection(), project_id)
    }

    pub fn list_project_summaries(&self) -> Result<Vec<ProjectSummary>> {
        let connection = self.connection();
        let summaries = connection
            .prepare(
                "SELECT project_id, summary, generated_at, attempted_at, error FROM project_summaries ORDER BY project_id",
            )?
            .query_map([], summary)?
            .collect::<rusqlite::Result<_>>()?;
        Ok(summaries)
    }

    pub fn save_project_summary(&self, project_id: &str, text: &str) -> Result<ProjectSummary> {
        let timestamp = now();
        let connection = self.connection();
        connection.execute(
            "INSERT INTO project_summaries (project_id, summary, generated_at, attempted_at, error)
             VALUES (?, ?, ?, ?, NULL)
             ON CONFLICT(project_id) DO UPDATE SET
               summary = excluded.summary, generated_at = excluded.generated_at,
               attempted_at = excluded.attempted_at, error = NULL",
            params![project_id, text, timestamp, timestamp],
        )?;
        get_summary(&connection, project_id)
    }

    pub fn save_project_summary_error(&self, project_id: &str, error: &str) -> Result<ProjectSummary> {
        let timestamp = now();
        let connection = self.connection();
        connection.execute(
            "INSERT INTO project_summaries (project_id, summary, generated_at, attempted_at, error)
             VALUES (?, NULL, NULL, ?, ?)
             ON CONFLICT(project_id) DO UPDATE SET attempted_at = excluded.attempted_at, error = excluded.error",
            params![project_id, timestamp, error],
        )?;
        get_summary(&connection, project_id)
    }
}
