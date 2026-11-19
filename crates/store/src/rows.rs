//! Row mappers, mirroring the `*FromRow` functions of the Node store.

use broadcast_domain::constants::JIRA_PROJECT_ID;
use broadcast_domain::records::{
    Actor, Attachment, Comment, DevelopmentContext, Project, ProjectReadmeAttachment, Recurrence,
    Task, TaskActivity, TaskRelationSummary, TaskRelations,
};
use rusqlite::Row;
use rusqlite::types::Type;
use serde::de::DeserializeOwned;

type Result<T> = rusqlite::Result<T>;

/// JavaScript truthiness for a text column.
fn filled(value: Option<String>) -> Option<String> {
    value.filter(|value| !value.is_empty())
}

pub(crate) fn optional_json<T: DeserializeOwned>(row: &Row<'_>, column: &str) -> Result<Option<T>> {
    let index = row.as_ref().column_index(column)?;
    let text: Option<String> = row.get(index)?;
    text.map(|text| {
        serde_json::from_str(&text).map_err(|error| {
            rusqlite::Error::FromSqlConversionFailure(index, Type::Text, Box::new(error))
        })
    })
    .transpose()
}

fn json<T: DeserializeOwned>(row: &Row<'_>, column: &str) -> Result<T> {
    optional_json(row, column)?
        .ok_or_else(|| rusqlite::Error::InvalidColumnType(0, column.to_owned(), Type::Null))
}

/// `<prefix>_type`, `<prefix>_id`, `<prefix>_name`, `<prefix>_avatar_url`.
pub(crate) fn actor(row: &Row<'_>, prefix: &str) -> Result<Actor> {
    Ok(Actor {
        kind: row.get(format!("{prefix}_type").as_str())?,
        id: row.get(format!("{prefix}_id").as_str())?,
        name: row.get(format!("{prefix}_name").as_str())?,
        avatar_url: row.get(format!("{prefix}_avatar_url").as_str())?,
    })
}

/// `taskFromRow`, with empty enrichment fields.
pub(crate) fn task(row: &Row<'_>) -> Result<Task> {
    let development_context = match (
        filled(row.get("worktree_path")?),
        filled(row.get("git_branch")?),
    ) {
        (Some(path), _) => Some(DevelopmentContext::Worktree {
            path: Some(path),
            branch: row.get("worktree_branch")?,
        }),
        (None, Some(branch)) => Some(DevelopmentContext::Branch { branch }),
        (None, None) => None,
    };
    let interval: Option<i64> = row.get("recurrence_interval")?;
    let recurrence = match (
        interval.filter(|interval| *interval != 0),
        filled(row.get("recurrence_unit")?),
    ) {
        (Some(interval), Some(unit)) => Some(Recurrence { interval, unit }),
        _ => None,
    };
    let external_source: Option<String> = row.get("external_source")?;
    Ok(Task {
        id: row.get("id")?,
        identifier: row.get("identifier")?,
        project_id: row.get("project_id")?,
        title: row.get("title")?,
        description: row.get("description")?,
        status: row.get("status")?,
        priority: row.get("priority")?,
        labels: json(row, "labels")?,
        sort_order: row.get("sort_order")?,
        agent_session: optional_json(row, "agent_session")?,
        creator_type: row.get("creator_type")?,
        creator_id: row.get("creator_id")?,
        creator_name: row.get("creator_name")?,
        creator_avatar_url: row.get("creator_avatar_url")?,
        assignee: actor(row, "assignee")?,
        development_context,
        start_date: row.get("start_date")?,
        due_date: row.get("due_date")?,
        recurrence,
        source: if external_source.as_deref() == Some("jira") {
            "jira"
        } else {
            "local"
        },
        external_origin: row.get("external_origin")?,
        external_key: row.get("external_key")?,
        external_url: row.get("external_url")?,
        archived_at: row.get("archived_at")?,
        version: row.get("version")?,
        created_at: row.get("created_at")?,
        updated_at: row.get("updated_at")?,
        relations: TaskRelations::default(),
        conversation_refs: Vec::new(),
        participants: Vec::new(),
        preview_image: None,
        spec: None,
        activity_key: String::new(),
        activity_updated_at: String::new(),
    })
}

/// `taskRelationSummaryFromRow`.
pub(crate) fn relation_summary(row: &Row<'_>) -> Result<TaskRelationSummary> {
    Ok(TaskRelationSummary {
        id: row.get("id")?,
        identifier: row.get("identifier")?,
        external_key: row.get("external_key")?,
        project_id: row.get("project_id")?,
        title: row.get("title")?,
        status: row.get("status")?,
        priority: row.get("priority")?,
        assignee: actor(row, "assignee")?,
        archived_at: row.get("archived_at")?,
    })
}

/// `attachmentFromRow`.
pub(crate) fn attachment(row: &Row<'_>) -> Result<Attachment> {
    Ok(Attachment {
        id: row.get("id")?,
        task_id: row.get("task_id")?,
        comment_id: row.get("comment_id")?,
        kind: row.get("kind")?,
        body_fallback: row.get::<_, i64>("body_fallback")? == 1,
        filename: row.get("filename")?,
        content_type: row.get("content_type")?,
        size: row.get("size")?,
        created_at: row.get("created_at")?,
        change_revision: row.get("change_revision")?,
    })
}

/// `commentFromRow`, without attachments.
pub(crate) fn comment(row: &Row<'_>) -> Result<Comment> {
    Ok(Comment {
        id: row.get("id")?,
        task_id: row.get("task_id")?,
        body: row.get("body")?,
        agent_session: optional_json(row, "agent_session")?,
        author_type: row.get("author_type")?,
        author_id: row.get("author_id")?,
        author_name: row.get("author_name")?,
        author_avatar_url: row.get("author_avatar_url")?,
        attachments: Vec::new(),
        version: row.get("version")?,
        created_at: row.get("created_at")?,
        updated_at: row.get("updated_at")?,
        change_revision: row.get("change_revision")?,
    })
}

/// `taskActivityFromRow`.
pub(crate) fn activity(row: &Row<'_>) -> Result<TaskActivity> {
    Ok(TaskActivity {
        id: row.get("id")?,
        task_id: row.get("task_id")?,
        actor_type: row.get("actor_type")?,
        actor_id: row.get("actor_id")?,
        actor_name: row.get("actor_name")?,
        actor_avatar_url: row.get("actor_avatar_url")?,
        changes: json(row, "changes")?,
        created_at: row.get("created_at")?,
    })
}

/// `projectReadmeAttachmentFromRow`.
pub(crate) fn readme_attachment(row: &Row<'_>) -> Result<ProjectReadmeAttachment> {
    Ok(ProjectReadmeAttachment {
        id: row.get("id")?,
        project_id: row.get("project_id")?,
        kind: "inline",
        filename: row.get("filename")?,
        content_type: row.get("content_type")?,
        size: row.get("size")?,
        created_at: row.get("created_at")?,
    })
}

/// `projectFromRow`.
pub(crate) fn project(row: &Row<'_>) -> Result<Project> {
    let id: String = row.get("id")?;
    Ok(Project {
        source: if id == JIRA_PROJECT_ID {
            "jira"
        } else {
            "local"
        },
        id,
        name: row.get("name")?,
        workspace_path: row.get("workspace_path")?,
        labels: json(row, "labels")?,
        issue_count: row.get("issue_count")?,
        created_at: row.get("created_at")?,
        updated_at: row.get("updated_at")?,
    })
}
