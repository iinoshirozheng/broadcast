//! Request parsers ported from `shared/task-input.mjs` and the server's body parsers.
//! Each parser evaluates fields in the JavaScript order so the first error matches.

use serde_json::Value;

use crate::ApiError;
use crate::constants::{DEFAULT_PROJECT_ID, is_task_status};
use crate::fields::{self, AssigneeTarget, NULL, Object, nullish};
use crate::js;
use crate::records::{AgentSession, DevelopmentContext, Recurrence, TaskChanges, ThreadBinding, ThreadIdentity};

type Result<T> = std::result::Result<T, ApiError>;

/// `parseAgentSession`: `None` is undefined, `Some(None)` is null.
pub fn agent_session(value: Option<&Value>) -> Result<Option<Option<AgentSession>>> {
    let Some(value) = value else { return Ok(None) };
    if value.is_null() {
        return Ok(Some(None));
    }
    let object = fields::object(value, &["platform", "sessionId"])?;
    let platform = object.get("platform").and_then(Value::as_str);
    let Some(platform) = platform.filter(|platform| ["claude", "pi", "agy", "grok"].contains(platform)) else {
        return Err(ApiError::invalid_field("agentSession.platform must be claude, pi, agy, or grok"));
    };
    let max_length = if platform == "pi" { 4096 } else { 256 };
    let session_id = object.get("sessionId").and_then(Value::as_str).filter(|id| {
        !js::trim(id).is_empty()
            && js::len(id) <= max_length
            && !id.trim_start_matches(js::is_whitespace).starts_with('-')
            && !id.chars().any(|c| c <= '\u{1f}' || c == '\u{7f}')
    });
    let Some(session_id) = session_id else {
        return Err(ApiError::invalid_field(format!(
            "agentSession.sessionId must contain 1 to {max_length} characters, without control characters or a leading option dash"
        )));
    };
    Ok(Some(Some(AgentSession { platform: platform.to_owned(), session_id: session_id.to_owned() })))
}

/// `parseThreadBinding`: `None` is undefined, `Some(None)` is null.
pub fn thread_binding(value: Option<&Value>) -> Result<Option<Option<ThreadBinding>>> {
    let Some(value) = value else { return Ok(None) };
    if value.is_null() {
        return Ok(Some(None));
    }
    let object = fields::object(
        value,
        &["threadId", "codexProjectId", "codexProjectKind", "codexHostId", "workspacePath"],
    )?;
    let thread_id = fields::required_string(object.get("threadId"), "threadBinding.threadId", 256)?;
    let identity = ["codexProjectId", "codexProjectKind", "codexHostId", "workspacePath"];
    if identity.iter().all(|key| !object.contains_key(*key)) {
        return Ok(Some(Some(ThreadBinding { thread_id, identity: None })));
    }
    if identity.iter().any(|key| !object.contains_key(*key)) {
        return Err(ApiError::invalid_field("Thread identity must include project, kind, host, and workspace"));
    }
    let codex_project_id =
        fields::required_string(object.get("codexProjectId"), "threadBinding.codexProjectId", 256)?;
    let codex_host_id = fields::required_string(object.get("codexHostId"), "threadBinding.codexHostId", 256)?;
    let workspace_path =
        fields::required_string(object.get("workspacePath"), "threadBinding.workspacePath", 4096)?;
    let kind = object.get("codexProjectKind").and_then(Value::as_str);
    let Some(kind) = kind.filter(|kind| *kind == "local" || *kind == "remote") else {
        return Err(ApiError::invalid_field("threadBinding.codexProjectKind must be local or remote"));
    };
    if (kind == "local") != (codex_host_id == "local") || workspace_path.contains('\0') {
        return Err(ApiError::invalid_field("Thread project identity is invalid"));
    }
    let identity = ThreadIdentity { codex_project_id, codex_project_kind: kind.to_owned(), codex_host_id, workspace_path };
    Ok(Some(Some(ThreadBinding { thread_id, identity: Some(identity) })))
}

/// Who acted: a Codex thread, a thread binding, or another agent session.
#[derive(Debug, Clone, Default)]
pub struct Attribution {
    pub thread_id: Option<String>,
    pub thread_binding: Option<Option<ThreadBinding>>,
    pub agent_session: Option<Option<AgentSession>>,
}

/// `threadId`, `threadBinding`, then `parseInputAgentSession`.
fn attribution(body: &Object) -> Result<Attribution> {
    let thread_id = fields::thread_id(body.get("threadId"))?;
    let thread_binding = thread_binding(body.get("threadBinding"))?;
    let agent_session = agent_session(body.get("agentSession"))?;
    if matches!(agent_session, Some(Some(_))) && body.contains_key("threadId") {
        return Err(ApiError::invalid_field(
            "Use agentSession or Codex threadId for controller attribution, not both",
        ));
    }
    Ok(Attribution { thread_id, thread_binding, agent_session })
}

/// `parseDevelopmentContext` for a defined value.
pub fn development_context(value: &Value) -> Result<Option<DevelopmentContext>> {
    if value.is_null() {
        return Ok(None);
    }
    let object = fields::plain_object(value)?;
    match object.get("type").and_then(Value::as_str) {
        Some("branch") => {
            fields::assert_allowed_keys(object, &["type", "branch"])?;
            let branch = fields::required_string(object.get("branch"), "developmentContext.branch", 512)?;
            Ok(Some(DevelopmentContext::Branch { branch }))
        }
        Some("worktree") => {
            fields::assert_allowed_keys(object, &["type", "path", "branch"])?;
            let path = fields::required_string(object.get("path"), "developmentContext.path", 4096)?;
            if path.contains('\0') {
                return Err(ApiError::invalid_field("'developmentContext.path' cannot contain null bytes"));
            }
            let branch = fields::nullable_string(
                nullish(object.get("branch")).or(Some(&NULL)),
                "developmentContext.branch",
                512,
            )?
            .flatten();
            Ok(Some(DevelopmentContext::Worktree { path, branch }))
        }
        _ => Err(ApiError::invalid_field("'developmentContext.type' must be branch or worktree")),
    }
}

#[derive(Debug, Clone)]
pub struct ProjectCreate {
    pub id: String,
    pub name: String,
    pub workspace_path: Option<String>,
}

pub fn project_create(body: &Value) -> Result<ProjectCreate> {
    let body = fields::object(body, &["id", "name", "workspacePath"])?;
    let name = fields::required_string(body.get("name"), "name", 120)?;
    let slug = Value::String(fields::slugify(&name));
    let id = fields::project_id(nullish(body.get("id")).or(Some(&slug)))?;
    let workspace_path = fields::path_field(nullish(body.get("workspacePath")).or(Some(&NULL)), "workspacePath")?;
    Ok(ProjectCreate { id, name, workspace_path })
}

#[derive(Debug, Clone)]
pub struct TaskCreate {
    pub project_id: String,
    pub title: String,
    pub description: String,
    pub status: &'static str,
    pub priority: &'static str,
    pub labels: Vec<String>,
    pub sort_order: Option<f64>,
    pub attribution: Attribution,
    pub assignee_target: Option<AssigneeTarget>,
    pub development_context: Option<DevelopmentContext>,
    pub start_date: Option<String>,
    pub due_date: Option<String>,
    pub recurrence: Option<Recurrence>,
}

pub fn task_create(body: &Value) -> Result<TaskCreate> {
    let body = fields::object(
        body,
        &[
            "projectId", "title", "description", "status", "priority", "labels", "sortOrder", "threadId",
            "threadBinding", "agentSession", "assigneeTarget", "developmentContext", "startDate", "dueDate",
            "recurrence",
        ],
    )?;
    let default_project = Value::String(DEFAULT_PROJECT_ID.to_owned());
    let project_id = fields::project_id(nullish(body.get("projectId")).or(Some(&default_project)))?;
    let title = fields::required_string(body.get("title"), "title", 240)?;
    let description = match nullish(body.get("description")) {
        Some(value) => fields::optional_string(Some(value), "description", 100_000)?.unwrap_or_default(),
        None => String::new(),
    };
    let status = fields::status(body.get("status"), Some("backlog"))?;
    let priority = fields::priority(body.get("priority"), Some("none"))?;
    let labels = body.get("labels").map(fields::labels).transpose()?.unwrap_or_default();
    let sort_order = body.get("sortOrder").map(fields::sort_order).transpose()?;
    let attribution = attribution(body)?;
    let assignee_target = fields::assignee_target(body.get("assigneeTarget"))?;
    let development_context = development_context(nullish(body.get("developmentContext")).unwrap_or(&NULL))?;
    let start_date = fields::due_date(nullish(body.get("startDate")).or(Some(&NULL)), "startDate")?.flatten();
    let due_date = fields::due_date(nullish(body.get("dueDate")).or(Some(&NULL)), "dueDate")?.flatten();
    let recurrence = fields::recurrence(nullish(body.get("recurrence")).unwrap_or(&NULL))?;
    if recurrence.is_some() && due_date.is_none() {
        return Err(ApiError::invalid_field("A recurring issue requires 'dueDate'"));
    }
    Ok(TaskCreate {
        project_id,
        title,
        description,
        status,
        priority,
        labels,
        sort_order,
        attribution,
        assignee_target,
        development_context,
        start_date,
        due_date,
        recurrence,
    })
}

#[derive(Debug, Clone)]
pub struct TaskPatch {
    pub version: i64,
    pub changes: TaskChanges,
    pub attribution: Attribution,
    pub assignee_target: Option<AssigneeTarget>,
}

pub fn task_patch(body: &Value) -> Result<TaskPatch> {
    let body = fields::object(
        body,
        &[
            "version", "projectId", "title", "description", "status", "priority", "labels", "threadId",
            "threadBinding", "agentSession", "assigneeTarget", "developmentContext", "startDate", "dueDate",
            "recurrence",
        ],
    )?;
    let version = fields::version(body.get("version"), false)?;
    let attribution = attribution(body)?;
    let assignee_target = fields::assignee_target(body.get("assigneeTarget"))?;
    let changes = TaskChanges {
        project_id: body.get("projectId").map(|value| fields::project_id(Some(value))).transpose()?,
        title: body.get("title").map(|value| fields::required_string(Some(value), "title", 240)).transpose()?,
        description: fields::optional_string(body.get("description"), "description", 100_000)?,
        status: body.get("status").map(|value| fields::status(Some(value), None)).transpose()?,
        priority: body.get("priority").map(|value| fields::priority(Some(value), None)).transpose()?,
        labels: body.get("labels").map(fields::labels).transpose()?,
        development_context: body.get("developmentContext").map(development_context).transpose()?,
        start_date: fields::due_date(body.get("startDate"), "startDate")?,
        due_date: fields::due_date(body.get("dueDate"), "dueDate")?,
        recurrence: body.get("recurrence").map(fields::recurrence).transpose()?,
        assignee: None,
    };
    if matches!(changes.recurrence, Some(Some(_))) && body.get("dueDate").is_some_and(Value::is_null) {
        return Err(ApiError::invalid_field("A recurring issue requires 'dueDate'"));
    }
    if changes.entries().is_empty() && assignee_target.is_none() && attribution.agent_session.is_none() {
        return Err(ApiError::new(400, "INVALID_BODY", "PATCH requires at least one task field"));
    }
    Ok(TaskPatch { version, changes, attribution, assignee_target })
}

#[derive(Debug, Clone)]
pub struct Move {
    pub version: i64,
    pub status: &'static str,
    pub sort_order: Option<f64>,
    pub attribution: Attribution,
}

pub fn task_move(body: &Value) -> Result<Move> {
    let body =
        fields::object(body, &["version", "status", "sortOrder", "threadId", "threadBinding", "agentSession"])?;
    Ok(Move {
        version: fields::version(body.get("version"), false)?,
        status: fields::status(body.get("status"), None)?,
        sort_order: body.get("sortOrder").map(fields::sort_order).transpose()?,
        attribution: attribution(body)?,
    })
}

#[derive(Debug, Clone)]
pub struct VersionMutation {
    pub version: i64,
    pub attribution: Attribution,
}

pub fn version_mutation(body: &Value) -> Result<VersionMutation> {
    let body = fields::object(body, &["version", "threadId", "threadBinding", "agentSession"])?;
    Ok(VersionMutation { version: fields::version(body.get("version"), false)?, attribution: attribution(body)? })
}

#[derive(Debug, Clone)]
pub struct RelationMutation {
    pub version: i64,
    pub attribution: Attribution,
    pub origin: Option<&'static str>,
}

pub fn relation_mutation(body: &Value) -> Result<RelationMutation> {
    let body = fields::object(body, &["version", "threadId", "threadBinding", "agentSession", "origin"])?;
    Ok(RelationMutation {
        version: fields::version(body.get("version"), false)?,
        attribution: attribution(body)?,
        origin: fields::relation_origin(body.get("origin"))?,
    })
}

#[derive(Debug, Clone)]
pub struct CommentCreate {
    pub body: String,
    pub attribution: Attribution,
}

pub fn comment_create(body: &Value) -> Result<CommentCreate> {
    let object = fields::object(body, &["body", "threadId", "threadBinding", "agentSession"])?;
    let text = match nullish(object.get("body")) {
        Some(value) => fields::optional_string(Some(value), "body", 100_000)?.unwrap_or_default(),
        None => String::new(),
    };
    Ok(CommentCreate { body: text, attribution: attribution(object)? })
}

#[derive(Debug, Clone)]
pub struct CommentPatch {
    pub version: i64,
    pub body: String,
    pub attribution: Attribution,
}

pub fn comment_patch(body: &Value) -> Result<CommentPatch> {
    let object = fields::object(body, &["version", "body", "threadId", "threadBinding", "agentSession"])?;
    if !object.contains_key("body") {
        return Err(ApiError::invalid_field("'body' is required"));
    }
    Ok(CommentPatch {
        version: fields::version(object.get("version"), false)?,
        body: fields::optional_string(object.get("body"), "body", 100_000)?.unwrap_or_default(),
        attribution: attribution(object)?,
    })
}

/// Query pairs as `URLSearchParams` yields them.
pub type Query = [(String, String)];

fn assert_query_parameters(query: &Query, allowed: &[&str], repeated_code: &'static str) -> Result<()> {
    for (key, _) in query {
        if !allowed.contains(&key.as_str()) {
            return Err(ApiError::new(400, "UNKNOWN_QUERY_PARAMETER", format!("Unknown query parameter '{key}'")));
        }
        if query.iter().filter(|(other, _)| other == key).count() != 1 {
            return Err(ApiError::new(400, repeated_code, format!("Query parameter '{key}' cannot be repeated")));
        }
    }
    Ok(())
}

fn query_value<'a>(query: &'a Query, key: &str) -> Option<&'a str> {
    query.iter().find(|(name, _)| name == key).map(|(_, value)| value.as_str())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Archived {
    Exclude,
    Only,
    All,
}

#[derive(Debug, Clone)]
pub struct TaskFilters {
    pub project_id: Option<String>,
    pub status: Option<String>,
    pub archived: Archived,
}

pub fn task_filters(query: &Query) -> Result<TaskFilters> {
    assert_query_parameters(query, &["projectId", "status", "archived"], "INVALID_QUERY_PARAMETER")?;
    let status = query_value(query, "status");
    if status.is_some_and(|status| !is_task_status(status)) {
        return Err(ApiError::new(400, "INVALID_QUERY_PARAMETER", "Invalid task status"));
    }
    let archived = match query_value(query, "archived").unwrap_or("false") {
        "false" => Archived::Exclude,
        "true" => Archived::Only,
        "all" => Archived::All,
        _ => {
            return Err(ApiError::new(400, "INVALID_QUERY_PARAMETER", "'archived' must be true, false, or all"));
        }
    };
    let project_id = query_value(query, "projectId")
        .map(|id| fields::project_id(Some(&Value::String(id.to_owned()))))
        .transpose()?;
    Ok(TaskFilters { project_id, status: status.map(str::to_owned), archived })
}

#[derive(Debug, Clone, Copy)]
pub struct TaskTreeQuery {
    pub direction: &'static str,
    pub depth: u32,
}

pub fn task_tree_query(query: &Query) -> Result<TaskTreeQuery> {
    assert_query_parameters(query, &["direction", "depth"], "INVALID_TREE_QUERY")?;
    let direction = match query_value(query, "direction") {
        Some("descendants") => "descendants",
        Some("ancestors") => "ancestors",
        _ => {
            return Err(ApiError::new(400, "INVALID_TREE_QUERY", "'direction' must be descendants or ancestors"));
        }
    };
    let depth = query_value(query, "depth")
        .filter(|raw| !raw.is_empty() && raw.bytes().all(|byte| byte.is_ascii_digit()))
        .and_then(|raw| raw.parse::<u32>().ok())
        .filter(|depth| (1..=25).contains(depth));
    let Some(depth) = depth else {
        return Err(ApiError::new(400, "INVALID_TREE_QUERY", "'depth' must be an integer from 1 to 25"));
    };
    Ok(TaskTreeQuery { direction, depth })
}

#[derive(Debug, Clone)]
pub struct ProjectReadmeSave {
    pub content: String,
    pub version: Option<i64>,
}

pub fn project_readme_save(body: &Value) -> Result<ProjectReadmeSave> {
    let body = fields::object(body, &["content", "version"])?;
    let content = match nullish(body.get("content")) {
        None => String::new(),
        Some(Value::String(content)) => content.clone(),
        Some(_) => return Err(ApiError::invalid_field("'content' must be a string")),
    };
    if js::len(&content) > 500_000 {
        return Err(ApiError::invalid_field("'content' cannot exceed 500000 characters"));
    }
    let version = body.get("version").map(|value| fields::version(Some(value), true)).transpose()?;
    Ok(ProjectReadmeSave { content, version })
}
