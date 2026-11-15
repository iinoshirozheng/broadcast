//! Issue relations: one parent, blocking edges and symmetric related links.

use broadcast_domain::ApiError;
use broadcast_domain::input::RelationMutation;
use serde_json::{Value, json};

use crate::board::{
    Board, TASK_CHANGED, assert_task_version, attribution_assignments, now, version_conflict,
};
use crate::db::{self, Row, args, integer, text};
use crate::error::Result;
use crate::records;

struct Endpoints {
    kind: &'static str,
    source: String,
    target: String,
}

/// `relationEndpoints`: the stored edge for a relation seen from `task`.
fn endpoints(kind: &str, task: &str, related: &str) -> Result<Endpoints> {
    let edge = |kind, source: &str, target: &str| Endpoints {
        kind,
        source: source.to_owned(),
        target: target.to_owned(),
    };
    Ok(match kind {
        "parent" => edge("parent", related, task),
        "blocks" => edge("blocks", task, related),
        "blocked_by" => edge("blocks", related, task),
        "related" => {
            let (source, target) = if related < task {
                (related, task)
            } else {
                (task, related)
            };
            edge("related", source, target)
        }
        _ => {
            return Err(ApiError::invalid_field(
                "'relation type' must be parent, blocks, blocked_by, or related",
            )
            .into());
        }
    })
}

fn issue_reference(project: &str, issue: &str) -> String {
    let query = form_urlencoded::Serializer::new(String::new())
        .append_pair("project", project)
        .append_pair("issue", issue)
        .finish();
    format!("](?{query})")
}

impl Board {
    /// `assertRelationTasks`.
    async fn relation_tasks(
        &self,
        task_id: &str,
        related_id: &str,
        version: i64,
    ) -> Result<(Row, Row)> {
        let task = self.require_task_row(task_id).await?;
        let related = self.require_task_row(related_id).await?;
        assert_task_version(&task, version)?;
        if text(&task, "id") == text(&related, "id") {
            return Err(ApiError::new(
                400,
                "SELF_RELATION",
                "An issue cannot be related to itself",
            )
            .into());
        }
        if text(&task, "project_id") != text(&related, "project_id") {
            return Err(ApiError::new(
                400,
                "CROSS_PROJECT_RELATION",
                "Issue relations must stay within one project",
            )
            .into());
        }
        Ok((task, related))
    }

    async fn relation_pair(&self, task: &Row, related: &Row) -> Result<Value> {
        Ok(json!({
            "task": self.get_task(text(task, "id")).await?,
            "relatedTask": self.get_task(text(related, "id")).await?,
        }))
    }

    fn touch_statement(
        &self,
        task: &Row,
        input: &RelationMutation,
        timestamp: &str,
        guard: &str,
    ) -> Result<worker::D1PreparedStatement> {
        let (attributed, mut values) = attribution_assignments(task, &input.attribution);
        values.extend(args![timestamp, text(task, "id"), input.version]);
        db::statement(
            &self.db,
            format!(
                "UPDATE tasks SET {attributed}version = version + 1, updated_at = ? WHERE id = ? AND version = ?{guard}"
            ),
            values,
        )
    }

    pub async fn add_relation(
        &self,
        task_id: &str,
        kind: &str,
        related_id: &str,
        input: &RelationMutation,
    ) -> Result<Value> {
        let (task, related) = self
            .relation_tasks(task_id, related_id, input.version)
            .await?;
        let id = text(&task, "id");
        let edge = endpoints(kind, id, text(&related, "id"))?;
        let timestamp = now();
        let mut previous = Value::Null;
        let mut statements = Vec::new();
        if edge.kind == "parent" {
            let cycle = db::first(
                &self.db,
                "WITH RECURSIVE ancestors(id) AS (
                   SELECT source_task_id FROM task_relations WHERE relation_type = 'parent' AND target_task_id = ?
                   UNION
                   SELECT task_relations.source_task_id
                   FROM task_relations
                   JOIN ancestors ON task_relations.target_task_id = ancestors.id
                   WHERE task_relations.relation_type = 'parent'
                 )
                 SELECT 1 AS found FROM ancestors WHERE id = ?",
                args![text(&related, "id"), id],
            )
            .await?;
            if cycle.is_some() {
                return Err(ApiError::new(
                    409,
                    "RELATION_CYCLE",
                    "This parent would create a cycle",
                )
                .into());
            }
            let existing = db::first(
                &self.db,
                "SELECT source_task_id FROM task_relations WHERE relation_type = 'parent' AND target_task_id = ?",
                args![id],
            )
            .await?;
            if let Some(existing) = existing {
                let parent_id = text(&existing, "source_task_id");
                if parent_id == text(&related, "id") {
                    return Err(ApiError::new(
                        409,
                        "RELATION_EXISTS",
                        "This parent relation already exists",
                    )
                    .into());
                }
                let parent = self.require_task_row(parent_id).await?;
                previous = records::relation_activity_value(kind, &records::task(&parent));
                statements.push(db::statement(
                    &self.db,
                    "DELETE FROM task_relations
                     WHERE relation_type = 'parent' AND target_task_id = ?
                       AND EXISTS (SELECT 1 FROM tasks WHERE id = ? AND version = ?)",
                    args![id, id, input.version],
                )?);
            }
        } else {
            let existing = db::first(
                &self.db,
                "SELECT 1 AS found FROM task_relations WHERE relation_type = ? AND source_task_id = ? AND target_task_id = ?",
                args![edge.kind, edge.source, edge.target],
            )
            .await?;
            if existing.is_some() {
                return Err(ApiError::new(
                    409,
                    "RELATION_EXISTS",
                    "This issue relation already exists",
                )
                .into());
            }
        }
        statements.push(db::statement(
            &self.db,
            "INSERT INTO task_relations (relation_type, source_task_id, target_task_id, origin, created_at)
             SELECT ?, ?, ?, ?, ?
             WHERE EXISTS (SELECT 1 FROM tasks WHERE id = ? AND version = ?)",
            args![edge.kind, edge.source, edge.target, input.origin.unwrap_or("manual"), timestamp, id, input.version],
        )?);
        statements.push(self.touch_statement(&task, input, &timestamp, "")?);
        let update_index = statements.len() - 1;
        let activity = json!({
            "field": "relation",
            "before": previous,
            "after": records::relation_activity_value(kind, &records::task(&related)),
        });
        statements.push(self.task_activity_statement(
            id,
            &[activity],
            &timestamp,
            input.version + 1,
        )?);
        let results = match self.db.batch(statements).await {
            Ok(results) => results,
            Err(error) => {
                let message = db::message(&error);
                if message.contains("CROSS_PROJECT_RELATION") {
                    return Err(ApiError::new(
                        400,
                        "CROSS_PROJECT_RELATION",
                        "Issue relations must stay within one project",
                    )
                    .into());
                }
                if message.contains("RELATION_CYCLE") {
                    return Err(ApiError::new(
                        409,
                        "RELATION_CYCLE",
                        "This parent would create a cycle",
                    )
                    .into());
                }
                if message.contains("UNIQUE constraint failed")
                    && message.contains("task_relations")
                {
                    return Err(ApiError::new(
                        409,
                        "RELATION_EXISTS",
                        "This issue relation already exists",
                    )
                    .into());
                }
                return Err(error.into());
            }
        };
        if !db::changed(&results[update_index])? {
            return Err(self.task_conflict(id, input.version).await);
        }
        self.relation_pair(&task, &related).await
    }

    pub async fn remove_relation(
        &self,
        task_id: &str,
        kind: &str,
        related_id: &str,
        input: &RelationMutation,
    ) -> Result<Value> {
        let (task, related) = self
            .relation_tasks(task_id, related_id, input.version)
            .await?;
        let id = text(&task, "id");
        let related_task_id = text(&related, "id");
        let edge = endpoints(kind, id, related_task_id)?;
        let relation = db::first(
            &self.db,
            "SELECT origin FROM task_relations WHERE relation_type = ? AND source_task_id = ? AND target_task_id = ?",
            args![edge.kind, edge.source, edge.target],
        )
        .await?
        .ok_or_else(|| ApiError::new(404, "RELATION_NOT_FOUND", "This issue relation does not exist"))?;
        if let Some(origin) = input.origin
            && text(&relation, "origin") != origin
        {
            return self.relation_pair(&task, &related).await;
        }
        let timestamp = now();
        let mention_removal = input.origin == Some("mention") && edge.kind == "related";
        let delete = if mention_removal {
            let project = text(&task, "project_id");
            let task_reference = issue_reference(project, text(&related, "identifier"));
            let related_reference = issue_reference(project, text(&task, "identifier"));
            db::statement(
                &self.db,
                "DELETE FROM task_relations
                 WHERE relation_type = ? AND source_task_id = ? AND target_task_id = ? AND origin = 'mention'
                   AND EXISTS (SELECT 1 FROM tasks WHERE id = ? AND version = ?)
                   AND NOT EXISTS (
                     SELECT 1 FROM tasks
                     WHERE (id = ? AND instr(description, ?) > 0) OR (id = ? AND instr(description, ?) > 0)
                   )
                   AND NOT EXISTS (
                     SELECT 1 FROM comments
                     WHERE (task_id = ? AND instr(body, ?) > 0) OR (task_id = ? AND instr(body, ?) > 0)
                   )",
                args![
                    edge.kind,
                    edge.source,
                    edge.target,
                    id,
                    input.version,
                    id,
                    task_reference,
                    related_task_id,
                    related_reference,
                    id,
                    task_reference,
                    related_task_id,
                    related_reference
                ],
            )?
        } else {
            db::statement(
                &self.db,
                "DELETE FROM task_relations
                 WHERE relation_type = ? AND source_task_id = ? AND target_task_id = ?
                   AND EXISTS (SELECT 1 FROM tasks WHERE id = ? AND version = ?)",
                args![edge.kind, edge.source, edge.target, id, input.version],
            )?
        };
        let guard = if mention_removal {
            " AND changes() = 1"
        } else {
            ""
        };
        let activity = json!({
            "field": "relation",
            "before": records::relation_activity_value(kind, &records::task(&related)),
            "after": null,
        });
        let results = self
            .db
            .batch(vec![
                delete,
                self.touch_statement(&task, input, &timestamp, guard)?,
                self.task_activity_statement(id, &[activity], &timestamp, input.version + 1)?,
            ])
            .await?;
        if !db::changed(&results[1])? {
            let latest = self.require_task_row(id).await?;
            let actual = integer(&latest, "version");
            if mention_removal && actual == input.version {
                return self.relation_pair(&task, &related).await;
            }
            return Err(version_conflict(TASK_CHANGED, input.version, actual));
        }
        self.relation_pair(&task, &related).await
    }
}
