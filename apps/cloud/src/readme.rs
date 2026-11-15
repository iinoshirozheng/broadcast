//! A project's README document.

use broadcast_domain::ApiError;
use broadcast_domain::input::ProjectReadmeSave;
use serde_json::{Value, json};

use crate::board::{Board, now};
use crate::db::{self, args, integer, value};
use crate::error::{Failure, Result};

fn conflict(expected: i64, actual: i64) -> Failure {
    ApiError::new(409, "VERSION_CONFLICT", "Project README changed since it was last read")
        .with_details(json!({ "expectedVersion": expected, "actualVersion": actual }))
        .into()
}

impl Board {
    async fn readme_version(&self, project_id: &str) -> Result<Option<i64>> {
        let row = db::first(&self.db, "SELECT version FROM project_readmes WHERE project_id = ?", args![project_id]).await?;
        Ok(row.map(|row| integer(&row, "version")))
    }

    pub async fn get_project_readme(&self, project_id: &str) -> Result<Value> {
        self.require_project(project_id).await?;
        let row = db::first(
            &self.db,
            "SELECT project_id, content, version, created_at, updated_at FROM project_readmes WHERE project_id = ?",
            args![project_id],
        )
        .await?;
        Ok(match row {
            Some(row) => json!({
                "projectId": value(&row, "project_id"),
                "content": value(&row, "content"),
                "version": value(&row, "version"),
                "createdAt": value(&row, "created_at"),
                "updatedAt": value(&row, "updated_at"),
            }),
            None => json!({ "projectId": project_id, "content": "", "version": 0, "createdAt": null, "updatedAt": null }),
        })
    }

    pub async fn save_project_readme(&self, project_id: &str, input: &ProjectReadmeSave) -> Result<Value> {
        self.require_project(project_id).await?;
        let timestamp = now();
        let Some(expected) = input.version else {
            db::run(
                &self.db,
                "INSERT INTO project_readmes (project_id, content, version, created_at, updated_at)
                 VALUES (?, ?, 1, ?, ?)
                 ON CONFLICT(project_id) DO UPDATE SET
                   content = excluded.content,
                   version = project_readmes.version + 1,
                   updated_at = excluded.updated_at",
                args![project_id, input.content, timestamp, timestamp],
            )
            .await?;
            return self.get_project_readme(project_id).await;
        };
        let current = self.readme_version(project_id).await?;
        let actual = current.unwrap_or(0);
        if actual != expected {
            return Err(conflict(expected, actual));
        }
        if current.is_some() {
            let result = db::run(
                &self.db,
                "UPDATE project_readmes SET content = ?, version = version + 1, updated_at = ?
                 WHERE project_id = ? AND version = ?",
                args![input.content, timestamp, project_id, expected],
            )
            .await?;
            if !db::changed(&result)? {
                return Err(conflict(expected, self.readme_version(project_id).await?.unwrap_or(0)));
            }
        } else {
            let insert = db::statement(
                &self.db,
                "INSERT INTO project_readmes (project_id, content, version, created_at, updated_at) VALUES (?, ?, 1, ?, ?)",
                args![project_id, input.content, timestamp, timestamp],
            )?;
            if let Err(error) = insert.run().await {
                if db::message(&error).contains("UNIQUE constraint failed") {
                    return Err(conflict(expected, self.readme_version(project_id).await?.unwrap_or(0)));
                }
                return Err(error.into());
            }
        }
        self.get_project_readme(project_id).await
    }
}
