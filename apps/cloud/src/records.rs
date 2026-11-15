//! API records built from D1 rows, field for field in the JavaScript worker's key order.

use std::collections::HashSet;

use broadcast_domain::input::Attribution;
use broadcast_domain::js;
use broadcast_domain::records::comment_conversation_title;
use serde_json::{Map, Value, json};

use crate::db::{Row, integer, optional_text, text, value};

fn parsed(row: &Row, column: &str) -> Value {
    optional_text(row, column)
        .and_then(|text| serde_json::from_str(text).ok())
        .unwrap_or(Value::Null)
}

fn truthy(row: &Row, column: &str) -> bool {
    row.get(column).is_some_and(js::truthy)
}

pub fn project(row: &Row) -> Value {
    json!({
        "id": value(row, "id"),
        "name": value(row, "name"),
        "workspacePath": null,
        "labels": parsed(row, "labels"),
        "issueCount": integer(row, "issue_count"),
        "createdAt": value(row, "created_at"),
        "updatedAt": value(row, "updated_at"),
    })
}

fn development_context(row: &Row) -> Value {
    match optional_text(row, "development_context_type") {
        Some("worktree") => {
            json!({ "type": "worktree", "path": null, "branch": value(row, "development_branch") })
        }
        Some("branch") => json!({ "type": "branch", "branch": value(row, "development_branch") }),
        _ => Value::Null,
    }
}

const THREAD_IDENTITY: [&str; 4] = [
    "thread_codex_project_id",
    "thread_codex_project_kind",
    "thread_codex_host_id",
    "thread_workspace_path",
];

pub fn thread_binding(row: &Row) -> Value {
    if !truthy(row, "thread_id") || !THREAD_IDENTITY.iter().all(|column| truthy(row, column)) {
        return Value::Null;
    }
    json!({
        "threadId": value(row, "thread_id"),
        "codexProjectId": value(row, "thread_codex_project_id"),
        "codexProjectKind": value(row, "thread_codex_project_kind"),
        "codexHostId": value(row, "thread_codex_host_id"),
        "workspacePath": value(row, "thread_workspace_path"),
    })
}

fn legacy_local_thread_id(row: &Row) -> Value {
    let legacy = truthy(row, "thread_id")
        && THREAD_IDENTITY
            .iter()
            .all(|column| value(row, column).is_null());
    if legacy {
        value(row, "thread_id")
    } else {
        Value::Null
    }
}

fn agent_session(row: &Row) -> Value {
    parsed(row, "agent_session")
}

fn assignee(row: &Row) -> Value {
    json!({
        "type": value(row, "assignee_type"),
        "id": value(row, "assignee_id"),
        "name": value(row, "assignee_name"),
        "avatarUrl": value(row, "assignee_avatar_url"),
    })
}

pub fn task(row: &Row) -> Value {
    let recurrence = if truthy(row, "recurrence_interval") && truthy(row, "recurrence_unit") {
        json!({ "interval": value(row, "recurrence_interval"), "unit": value(row, "recurrence_unit") })
    } else {
        Value::Null
    };
    json!({
        "id": value(row, "id"),
        "identifier": value(row, "identifier"),
        "projectId": value(row, "project_id"),
        "title": value(row, "title"),
        "description": value(row, "description"),
        "status": value(row, "status"),
        "priority": value(row, "priority"),
        "labels": parsed(row, "labels"),
        "sortOrder": value(row, "sort_order"),
        "threadId": value(row, "thread_id"),
        "threadBinding": thread_binding(row),
        "legacyLocalThreadId": legacy_local_thread_id(row),
        "agentSession": agent_session(row),
        "creatorType": value(row, "creator_type"),
        "creatorId": value(row, "creator_id"),
        "creatorName": value(row, "creator_name"),
        "creatorAvatarUrl": value(row, "creator_avatar_url"),
        "assignee": assignee(row),
        "developmentContext": development_context(row),
        "startDate": value(row, "start_date"),
        "dueDate": value(row, "due_date"),
        "recurrence": recurrence,
        "archivedAt": value(row, "archived_at"),
        "version": value(row, "version"),
        "createdAt": value(row, "created_at"),
        "updatedAt": value(row, "updated_at"),
    })
}

pub fn task_relation_summary(row: &Row) -> Value {
    json!({
        "id": value(row, "id"),
        "identifier": value(row, "identifier"),
        "projectId": value(row, "project_id"),
        "title": value(row, "title"),
        "status": value(row, "status"),
        "priority": value(row, "priority"),
        "assignee": assignee(row),
        "archivedAt": value(row, "archived_at"),
    })
}

pub fn comment(row: &Row, attachments: Vec<Value>) -> Value {
    json!({
        "id": value(row, "id"),
        "taskId": value(row, "task_id"),
        "body": value(row, "body"),
        "threadId": value(row, "thread_id"),
        "threadBinding": thread_binding(row),
        "legacyLocalThreadId": legacy_local_thread_id(row),
        "agentSession": agent_session(row),
        "authorType": value(row, "author_type"),
        "authorId": value(row, "author_id"),
        "authorName": value(row, "author_name"),
        "authorAvatarUrl": value(row, "author_avatar_url"),
        "attachments": attachments,
        "version": value(row, "version"),
        "createdAt": value(row, "created_at"),
        "updatedAt": value(row, "updated_at"),
    })
}

pub fn attachment(row: &Row) -> Value {
    json!({
        "id": value(row, "id"),
        "taskId": value(row, "task_id"),
        "commentId": value(row, "comment_id"),
        "kind": value(row, "kind"),
        "bodyFallback": integer(row, "body_fallback") == 1,
        "filename": value(row, "filename"),
        "contentType": value(row, "content_type"),
        "size": value(row, "size"),
        "createdAt": value(row, "created_at"),
    })
}

pub fn project_readme_attachment(row: &Row) -> Value {
    json!({
        "id": value(row, "id"),
        "projectId": value(row, "project_id"),
        "kind": "inline",
        "filename": value(row, "filename"),
        "contentType": value(row, "content_type"),
        "size": value(row, "size"),
        "createdAt": value(row, "created_at"),
    })
}

pub fn task_activity(row: &Row) -> Value {
    json!({
        "id": value(row, "id"),
        "taskId": value(row, "task_id"),
        "actorType": value(row, "actor_type"),
        "actorId": value(row, "actor_id"),
        "actorName": value(row, "actor_name"),
        "actorAvatarUrl": value(row, "actor_avatar_url"),
        "changes": parsed(row, "changes"),
        "createdAt": value(row, "created_at"),
    })
}

pub fn task_tree_node(row: &Row, parent_id: Value, depth: u32, path: Vec<Value>) -> Value {
    json!({
        "id": value(row, "id"),
        "parentId": parent_id,
        "depth": depth,
        "path": path,
        "summary": {
            "identifier": value(row, "identifier"),
            "title": value(row, "title"),
            "status": value(row, "status"),
            "priority": value(row, "priority"),
            "archivedAt": value(row, "archived_at"),
        },
    })
}

pub fn relation_activity_value(kind: &str, task: &Value) -> Value {
    json!({ "type": kind, "identifier": task["identifier"], "title": task["title"] })
}

/// `taskFieldChanges`: the entries whose JSON differs from the task's current value.
#[expect(
    clippy::cmp_owned,
    reason = "JSON.stringify comparison is key-order sensitive"
)]
pub fn field_changes(task: &Value, changes: &[(&str, Value)]) -> Vec<Value> {
    changes
        .iter()
        .filter_map(|(field, after)| {
            let before = task.get(*field).cloned().unwrap_or(Value::Null);
            (before.to_string() != after.to_string())
                .then(|| json!({ "field": field, "before": before, "after": after }))
        })
        .collect()
}

/// `storedThreadBinding`: the five thread columns to write, or `None` to leave them.
pub fn stored_thread_binding(attribution: &Attribution) -> Option<Vec<Value>> {
    let thread_id = || {
        attribution
            .thread_id
            .clone()
            .map_or(Value::Null, Value::String)
    };
    match &attribution.thread_binding {
        None if attribution.thread_id.is_none() => None,
        None => Some(vec![
            thread_id(),
            Value::Null,
            Value::Null,
            Value::Null,
            Value::Null,
        ]),
        Some(None) => Some(vec![Value::Null; 5]),
        Some(Some(binding)) => {
            let identity = binding.identity.as_ref();
            let field = |value: Option<&String>| value.cloned().map_or(Value::Null, Value::String);
            Some(vec![
                Value::String(binding.thread_id.clone()),
                field(identity.map(|identity| &identity.codex_project_id)),
                field(identity.map(|identity| &identity.codex_project_kind)),
                field(identity.map(|identity| &identity.codex_host_id)),
                field(identity.map(|identity| &identity.workspace_path)),
            ])
        }
    }
}

/// `storedThreadBindingForExisting`: a bare thread id keeps the row's identity for that thread.
pub fn stored_thread_binding_for_existing(
    row: &Row,
    attribution: &Attribution,
) -> Option<Vec<Value>> {
    let current = thread_binding(row);
    if attribution.thread_binding.is_none()
        && !current.is_null()
        && current["threadId"].as_str() == attribution.thread_id.as_deref()
    {
        return Some(
            [
                "threadId",
                "codexProjectId",
                "codexProjectKind",
                "codexHostId",
                "workspacePath",
            ]
            .iter()
            .map(|field| current[*field].clone())
            .collect(),
        );
    }
    stored_thread_binding(attribution)
}

/// The `agent_session` column to write, or `None` to leave it.
pub fn stored_agent_session(attribution: &Attribution) -> Option<Value> {
    attribution
        .agent_session
        .as_ref()
        .map(|session| match session {
            Some(session) => Value::String(serde_json::to_string(session).unwrap_or_default()),
            None => Value::Null,
        })
}

fn participant(row: &Row, prefix: &str) -> Value {
    json!({
        "type": value(row, &format!("{prefix}_type")),
        "id": value(row, &format!("{prefix}_id")),
        "name": value(row, &format!("{prefix}_name")),
        "avatarUrl": value(row, &format!("{prefix}_avatar_url")),
    })
}

fn conversation_ref(
    base: Value,
    source: &str,
    source_id: Value,
    title: Value,
    updated_at: Value,
) -> Value {
    let mut entry = match base {
        Value::Object(entry) => entry,
        _ => Map::new(),
    };
    entry.insert("source".into(), source.into());
    entry.insert("sourceId".into(), source_id);
    entry.insert("title".into(), title);
    entry.insert("updatedAt".into(), updated_at);
    Value::Object(entry)
}

/// `attachTaskActivity`: participants, conversation references and the activity key.
pub fn attach_task_activity(
    task: &mut Value,
    mut comments: Vec<Row>,
    mut activities: Vec<Row>,
    preview: Value,
) {
    comments.sort_by(|left, right| js::locale_compare(text(left, "id"), text(right, "id")));
    activities.sort_by(|left, right| js::locale_compare(text(left, "id"), text(right, "id")));

    let mut participants = Vec::new();
    let mut seen = HashSet::new();
    let mut add = |actor: Value| {
        let key = format!("{}:{}", js_string(&actor["type"]), js_string(&actor["id"]));
        if seen.insert(key) {
            participants.push(actor);
        }
    };
    add(json!({
        "type": task["creatorType"],
        "id": task["creatorId"],
        "name": task["creatorName"],
        "avatarUrl": task["creatorAvatarUrl"],
    }));
    add(task["assignee"].clone());
    for comment in &comments {
        add(participant(comment, "author"));
    }
    for activity in &activities {
        add(participant(activity, "actor"));
    }

    let mut refs = Vec::new();
    let (id, title, updated_at) = (
        task["id"].clone(),
        task["title"].clone(),
        task["updatedAt"].clone(),
    );
    if js::truthy(&task["agentSession"]) {
        refs.push(conversation_ref(
            json!({ "agentSession": task["agentSession"] }),
            "task",
            id.clone(),
            title.clone(),
            updated_at.clone(),
        ));
    }
    if js::truthy(&task["threadBinding"]) {
        refs.push(conversation_ref(
            task["threadBinding"].clone(),
            "task",
            id.clone(),
            title.clone(),
            updated_at.clone(),
        ));
    } else if js::truthy(&task["legacyLocalThreadId"]) {
        let base = json!({ "threadId": task["legacyLocalThreadId"], "legacyLocal": true });
        refs.push(conversation_ref(
            base,
            "task",
            id.clone(),
            title.clone(),
            updated_at.clone(),
        ));
    }
    for comment in &comments {
        let title = Value::String(comment_conversation_title(optional_text(comment, "body")));
        let session = agent_session(comment);
        if js::truthy(&session) {
            let base = json!({ "agentSession": session });
            refs.push(conversation_ref(
                base,
                "comment",
                value(comment, "id"),
                title.clone(),
                value(comment, "updated_at"),
            ));
        }
        let binding = thread_binding(comment);
        let legacy = legacy_local_thread_id(comment);
        if js::truthy(&binding) || js::truthy(&legacy) {
            let base = if binding.is_null() {
                json!({ "threadId": legacy, "legacyLocal": true })
            } else {
                binding
            };
            refs.push(conversation_ref(
                base,
                "comment",
                value(comment, "id"),
                title,
                value(comment, "updated_at"),
            ));
        }
    }

    let activity_key = json!({
        "version": 1,
        "task": [task["id"], task["version"], task["updatedAt"]],
        "comments": comments.iter().map(|comment| json!([value(comment, "id"), value(comment, "version"), value(comment, "updated_at")])).collect::<Vec<_>>(),
        "changes": activities.iter().map(|activity| json!([value(activity, "id"), value(activity, "created_at")])).collect::<Vec<_>>(),
    })
    .to_string();
    let mut activity_updated_at = task["updatedAt"].as_str().unwrap_or_default().to_owned();
    for entry in comments.iter().chain(&activities) {
        let updated = optional_text(entry, "updated_at")
            .or_else(|| optional_text(entry, "created_at"))
            .unwrap_or_default();
        if updated > activity_updated_at.as_str() {
            activity_updated_at = updated.to_owned();
        }
    }

    task["conversationRefs"] = Value::Array(refs);
    task["participants"] = Value::Array(participants);
    task["previewImage"] = preview;
    task["activityKey"] = Value::String(activity_key);
    task["activityUpdatedAt"] = Value::String(activity_updated_at);
}

/// `${value}` of a JSON scalar.
fn js_string(value: &Value) -> String {
    match value {
        Value::String(value) => value.clone(),
        Value::Null => "null".into(),
        value => value.to_string(),
    }
}

/// `projectPrefix`: the identifier prefix of a project's issues.
pub fn project_prefix(id: &str, name: &str, first_identifier: Option<&str>) -> String {
    let alphanumeric = |value: &str| {
        value
            .to_uppercase()
            .chars()
            .filter(char::is_ascii_alphanumeric)
            .collect::<String>()
    };
    let mut id_prefix: String = alphanumeric(id).chars().take(12).collect();
    if id_prefix.is_empty() {
        id_prefix = "TASK".into();
    }
    if let Some(first) = first_identifier {
        let existing = match first.rsplit_once('-') {
            Some((head, digits))
                if !digits.is_empty() && digits.bytes().all(|byte| byte.is_ascii_digit()) =>
            {
                head
            }
            _ => first,
        };
        if !existing.is_empty()
            && existing.bytes().all(|byte| byte.is_ascii_alphanumeric())
            && existing != id_prefix
        {
            return existing.to_owned();
        }
    }
    if id_prefix.len() <= 5 {
        return id_prefix;
    }
    let name_prefix: String = alphanumeric(name).chars().take(3).collect();
    if name_prefix.is_empty() {
        id_prefix[..3].to_owned()
    } else {
        name_prefix
    }
}
