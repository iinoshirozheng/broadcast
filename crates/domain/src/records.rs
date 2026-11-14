//! API records. Field order follows the JavaScript object literals they replace.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::js;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Recurrence {
    pub interval: i64,
    pub unit: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentSession {
    pub platform: String,
    pub session_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ThreadIdentity {
    pub codex_project_id: String,
    pub codex_project_kind: String,
    pub codex_host_id: String,
    pub workspace_path: String,
}

/// A Codex thread; input may omit the identity, stored records always carry it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ThreadBinding {
    pub thread_id: String,
    #[serde(flatten)]
    pub identity: Option<ThreadIdentity>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum DevelopmentContext {
    Branch { branch: String },
    Worktree { path: String, branch: Option<String> },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Actor {
    #[serde(rename = "type")]
    pub kind: String,
    pub id: String,
    pub name: String,
    pub avatar_url: Option<String>,
}

impl Actor {
    fn constant(kind: &str, id: &str, name: &str) -> Self {
        Self { kind: kind.into(), id: id.into(), name: name.into(), avatar_url: None }
    }

    pub fn local_user() -> Self {
        Self::constant("user", "local-user", "本地用户")
    }

    pub fn codex_agent() -> Self {
        Self::constant("agent", "codex-agent", "Codex Agent")
    }

    pub fn claude_agent() -> Self {
        Self::constant("agent", "claude-agent", "Claude Agent")
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Project {
    pub id: String,
    pub name: String,
    pub workspace_path: Option<String>,
    pub source: &'static str,
    pub labels: Vec<String>,
    pub issue_count: i64,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Attachment {
    pub id: String,
    pub task_id: String,
    pub comment_id: Option<String>,
    pub kind: String,
    pub body_fallback: bool,
    pub filename: String,
    pub content_type: String,
    pub size: i64,
    pub created_at: String,
    /// Cursor position; not part of the JSON record.
    #[serde(skip)]
    pub change_revision: i64,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Comment {
    pub id: String,
    pub task_id: String,
    pub body: String,
    pub thread_id: Option<String>,
    pub thread_binding: Option<ThreadBinding>,
    pub legacy_local_thread_id: Option<String>,
    pub agent_session: Option<AgentSession>,
    pub author_type: String,
    pub author_id: String,
    pub author_name: String,
    pub author_avatar_url: Option<String>,
    pub attachments: Vec<Attachment>,
    pub version: i64,
    pub created_at: String,
    pub updated_at: String,
    /// Cursor position; not part of the JSON record.
    #[serde(skip)]
    pub change_revision: i64,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskActivity {
    pub id: String,
    pub task_id: String,
    pub actor_type: String,
    pub actor_id: String,
    pub actor_name: String,
    pub actor_avatar_url: Option<String>,
    pub changes: Value,
    pub created_at: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskTreeSummary {
    pub identifier: String,
    pub title: String,
    pub status: String,
    pub priority: String,
    pub archived_at: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskTreeNode {
    pub id: String,
    pub parent_id: Option<String>,
    pub depth: u32,
    pub path: Vec<String>,
    pub summary: TaskTreeSummary,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskTree {
    pub root_id: String,
    pub direction: &'static str,
    pub depth: u32,
    pub node_count: usize,
    pub nodes: Vec<TaskTreeNode>,
}

/// A task and the other end of the relation that changed.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RelationChange {
    pub task: Task,
    pub related_task: Task,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProjectReadme {
    pub project_id: String,
    pub content: String,
    pub version: i64,
    pub created_at: Option<String>,
    pub updated_at: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProjectReadmeAttachment {
    pub id: String,
    pub project_id: String,
    pub kind: &'static str,
    pub filename: String,
    pub content_type: String,
    pub size: i64,
    pub created_at: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProjectSummary {
    pub project_id: String,
    pub summary: Option<String>,
    pub generated_at: Option<String>,
    pub attempted_at: Option<String>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskRelationSummary {
    pub id: String,
    pub identifier: String,
    pub external_key: Option<String>,
    pub project_id: String,
    pub title: String,
    pub status: String,
    pub priority: String,
    pub assignee: Actor,
    pub archived_at: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskRelations {
    pub parent: Option<TaskRelationSummary>,
    pub sub_issues: Vec<TaskRelationSummary>,
    pub blocked_by: Vec<TaskRelationSummary>,
    pub blocks: Vec<TaskRelationSummary>,
    pub related: Vec<TaskRelationSummary>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(untagged, rename_all_fields = "camelCase")]
pub enum ConversationTarget {
    AgentSession { agent_session: AgentSession },
    Thread(ThreadBinding),
    LegacyLocal { thread_id: String, legacy_local: bool },
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ConversationRef {
    #[serde(flatten)]
    pub target: ConversationTarget,
    pub source: &'static str,
    pub source_id: String,
    pub title: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Task {
    pub id: String,
    pub identifier: String,
    pub project_id: String,
    pub title: String,
    pub description: String,
    pub status: String,
    pub priority: String,
    pub labels: Vec<String>,
    #[serde(serialize_with = "js::serialize_number")]
    pub sort_order: f64,
    pub thread_id: Option<String>,
    pub thread_binding: Option<ThreadBinding>,
    pub legacy_local_thread_id: Option<String>,
    pub agent_session: Option<AgentSession>,
    pub creator_type: String,
    pub creator_id: String,
    pub creator_name: String,
    pub creator_avatar_url: Option<String>,
    pub assignee: Actor,
    pub development_context: Option<DevelopmentContext>,
    pub start_date: Option<String>,
    pub due_date: Option<String>,
    pub recurrence: Option<Recurrence>,
    pub source: &'static str,
    pub external_origin: Option<String>,
    pub external_key: Option<String>,
    pub external_url: Option<String>,
    pub archived_at: Option<String>,
    pub version: i64,
    pub created_at: String,
    pub updated_at: String,
    pub relations: TaskRelations,
    pub conversation_refs: Vec<ConversationRef>,
    pub participants: Vec<Actor>,
    pub preview_image: Option<Attachment>,
    pub activity_key: String,
    pub activity_updated_at: String,
}

impl Task {
    pub fn creator(&self) -> Actor {
        Actor {
            kind: self.creator_type.clone(),
            id: self.creator_id.clone(),
            name: self.creator_name.clone(),
            avatar_url: self.creator_avatar_url.clone(),
        }
    }
}

/// One `{ field, before, after }` entry of a task activity.
#[derive(Debug, Clone, Serialize)]
pub struct FieldChange {
    pub field: &'static str,
    pub before: Value,
    pub after: Value,
}

/// Parsed task changes; `entries` yields them in the JavaScript `changes` key order.
#[derive(Debug, Clone, Default)]
pub struct TaskChanges {
    pub project_id: Option<String>,
    pub title: Option<String>,
    pub description: Option<String>,
    pub status: Option<&'static str>,
    pub priority: Option<&'static str>,
    pub labels: Option<Vec<String>>,
    pub development_context: Option<Option<DevelopmentContext>>,
    pub start_date: Option<Option<String>>,
    pub due_date: Option<Option<String>>,
    pub recurrence: Option<Option<Recurrence>>,
    pub assignee: Option<Actor>,
}

impl TaskChanges {
    pub fn entries(&self) -> Vec<(&'static str, Value)> {
        fn push<T: Serialize>(entries: &mut Vec<(&'static str, Value)>, field: &'static str, value: &Option<T>) {
            if let Some(value) = value {
                entries.push((field, serde_json::to_value(value).unwrap_or(Value::Null)));
            }
        }
        let mut entries = Vec::new();
        push(&mut entries, "projectId", &self.project_id);
        push(&mut entries, "title", &self.title);
        push(&mut entries, "description", &self.description);
        push(&mut entries, "status", &self.status);
        push(&mut entries, "priority", &self.priority);
        push(&mut entries, "labels", &self.labels);
        push(&mut entries, "developmentContext", &self.development_context);
        push(&mut entries, "startDate", &self.start_date);
        push(&mut entries, "dueDate", &self.due_date);
        push(&mut entries, "recurrence", &self.recurrence);
        push(&mut entries, "assignee", &self.assignee);
        entries
    }

    /// `taskFieldChanges`: entries whose JSON differs from the current task.
    #[expect(clippy::cmp_owned, reason = "JSON.stringify comparison is key-order sensitive; Value equality is not")]
    pub fn diff(&self, task: &Task) -> Vec<FieldChange> {
        let current = serde_json::to_value(task).unwrap_or(Value::Null);
        self.entries()
            .into_iter()
            .filter_map(|(field, after)| {
                let before = current.get(field).cloned().unwrap_or(Value::Null);
                (before.to_string() != after.to_string()).then_some(FieldChange { field, before, after })
            })
            .collect()
    }
}

/// `commentConversationTitle`.
pub fn comment_conversation_title(body: Option<&str>) -> String {
    let first = body
        .unwrap_or_default()
        .split('\n')
        .map(js::trim)
        .find(|line| !line.is_empty());
    let Some(first) = first else { return "评论".to_owned() };
    let compact = first.split(js::is_whitespace).filter(|part| !part.is_empty()).collect::<Vec<_>>().join(" ");
    if js::len(&compact) > 80 { format!("{}…", js::prefix(&compact, 77)) } else { compact }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AiChatRun {
    pub id: String,
    pub thread_id: String,
    pub status: String,
    pub exit_code: Option<i64>,
    pub error: Option<String>,
    pub started_at: String,
    pub finished_at: Option<String>,
}

/// Where a conversation runs: the board project, its workspace and, for a Codex
/// project on another host, that host's identity.
#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AiChatOrigin {
    pub project_id: String,
    pub project_name: String,
    pub workspace_path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub codex_project_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub codex_project_kind: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub codex_host_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub issue_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub issue_identifier: Option<String>,
}

impl AiChatOrigin {
    pub fn remote(&self) -> bool {
        self.codex_project_kind.as_deref() == Some("remote")
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AiChatTodo {
    pub completed: usize,
    pub total: usize,
    pub event_id: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AiChatThread {
    pub id: String,
    pub title: String,
    pub status: String,
    pub origin: AiChatOrigin,
    pub codex_thread_id: Option<String>,
    pub model: String,
    pub reasoning_effort: String,
    pub sandbox: String,
    pub current_run: Option<AiChatRun>,
    pub latest_todo: Option<AiChatTodo>,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AiChatEvent {
    pub id: String,
    pub thread_id: String,
    pub run_id: Option<String>,
    #[serde(rename = "type")]
    pub kind: String,
    pub role: String,
    pub content: String,
    pub data: Option<Value>,
    pub created_at: String,
}
