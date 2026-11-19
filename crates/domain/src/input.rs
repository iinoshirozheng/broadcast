//! Request body parsers. Each parser evaluates fields in a fixed order, so the first error it
//! reports is stable.

use serde_json::Value;

use crate::ApiError;
use crate::constants::{DEFAULT_PROJECT_ID, is_task_status};
use crate::fields::{self, AssigneeTarget, NULL, Object, nullish};
use crate::js;
use crate::records::{AgentSession, DevelopmentContext, Recurrence, TaskChanges};

type Result<T> = std::result::Result<T, ApiError>;

/// `parseAgentSession`: `None` is undefined, `Some(None)` is null.
pub fn agent_session(value: Option<&Value>) -> Result<Option<Option<AgentSession>>> {
    let Some(value) = value else { return Ok(None) };
    if value.is_null() {
        return Ok(Some(None));
    }
    let object = fields::object(value, &["platform", "sessionId"])?;
    let platform = object.get("platform").and_then(Value::as_str);
    let Some(platform) =
        platform.filter(|platform| ["claude", "codex", "pi", "agy", "grok"].contains(platform))
    else {
        return Err(ApiError::invalid_field(
            "agentSession.platform must be claude, codex, pi, agy, or grok",
        ));
    };
    let max_length = if platform == "pi" { 4096 } else { 256 };
    let session_id = object
        .get("sessionId")
        .and_then(Value::as_str)
        .filter(|id| {
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
    Ok(Some(Some(AgentSession {
        platform: platform.to_owned(),
        session_id: session_id.to_owned(),
    })))
}

/// Which agent conversation acted, if any.
#[derive(Debug, Clone, Default)]
pub struct Attribution {
    pub agent_session: Option<Option<AgentSession>>,
}

fn attribution(body: &Object) -> Result<Attribution> {
    Ok(Attribution {
        agent_session: agent_session(body.get("agentSession"))?,
    })
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
            let branch =
                fields::required_string(object.get("branch"), "developmentContext.branch", 512)?;
            Ok(Some(DevelopmentContext::Branch { branch }))
        }
        Some("worktree") => {
            fields::assert_allowed_keys(object, &["type", "path", "branch"])?;
            let path =
                fields::required_string(object.get("path"), "developmentContext.path", 4096)?;
            if path.contains('\0') {
                return Err(ApiError::invalid_field(
                    "'developmentContext.path' cannot contain null bytes",
                ));
            }
            let branch = fields::nullable_string(
                nullish(object.get("branch")).or(Some(&NULL)),
                "developmentContext.branch",
                512,
            )?
            .flatten();
            Ok(Some(DevelopmentContext::Worktree {
                path: Some(path),
                branch,
            }))
        }
        _ => Err(ApiError::invalid_field(
            "'developmentContext.type' must be branch or worktree",
        )),
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
    let workspace_path = fields::path_field(
        nullish(body.get("workspacePath")).or(Some(&NULL)),
        "workspacePath",
    )?;
    Ok(ProjectCreate {
        id,
        name,
        workspace_path,
    })
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
            "projectId",
            "title",
            "description",
            "status",
            "priority",
            "labels",
            "sortOrder",
            "agentSession",
            "assigneeTarget",
            "developmentContext",
            "startDate",
            "dueDate",
            "recurrence",
        ],
    )?;
    let default_project = Value::String(DEFAULT_PROJECT_ID.to_owned());
    let project_id = fields::project_id(nullish(body.get("projectId")).or(Some(&default_project)))?;
    let title = fields::required_string(body.get("title"), "title", 240)?;
    let description = match nullish(body.get("description")) {
        Some(value) => {
            fields::optional_string(Some(value), "description", 100_000)?.unwrap_or_default()
        }
        None => String::new(),
    };
    let status = fields::status(body.get("status"), Some("backlog"))?;
    let priority = fields::priority(body.get("priority"), Some("none"))?;
    let labels = body
        .get("labels")
        .map(fields::labels)
        .transpose()?
        .unwrap_or_default();
    let sort_order = body.get("sortOrder").map(fields::sort_order).transpose()?;
    let attribution = attribution(body)?;
    let assignee_target = fields::assignee_target(body.get("assigneeTarget"))?;
    let development_context =
        development_context(nullish(body.get("developmentContext")).unwrap_or(&NULL))?;
    let start_date =
        fields::due_date(nullish(body.get("startDate")).or(Some(&NULL)), "startDate")?.flatten();
    let due_date =
        fields::due_date(nullish(body.get("dueDate")).or(Some(&NULL)), "dueDate")?.flatten();
    let recurrence = fields::recurrence(nullish(body.get("recurrence")).unwrap_or(&NULL))?;
    if recurrence.is_some() && due_date.is_none() {
        return Err(ApiError::invalid_field(
            "A recurring issue requires 'dueDate'",
        ));
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

fn task_patch_body(body: &Value) -> Result<&Object> {
    fields::object(
        body,
        &[
            "version",
            "projectId",
            "title",
            "description",
            "status",
            "priority",
            "labels",
            "agentSession",
            "assigneeTarget",
            "developmentContext",
            "startDate",
            "dueDate",
            "recurrence",
        ],
    )
}

fn task_patch_changes(body: &Object) -> Result<TaskChanges> {
    Ok(TaskChanges {
        project_id: body
            .get("projectId")
            .map(|value| fields::project_id(Some(value)))
            .transpose()?,
        title: body
            .get("title")
            .map(|value| fields::required_string(Some(value), "title", 240))
            .transpose()?,
        description: fields::optional_string(body.get("description"), "description", 100_000)?,
        status: body
            .get("status")
            .map(|value| fields::status(Some(value), None))
            .transpose()?,
        priority: body
            .get("priority")
            .map(|value| fields::priority(Some(value), None))
            .transpose()?,
        labels: body.get("labels").map(fields::labels).transpose()?,
        development_context: body
            .get("developmentContext")
            .map(development_context)
            .transpose()?,
        start_date: fields::due_date(body.get("startDate"), "startDate")?,
        due_date: fields::due_date(body.get("dueDate"), "dueDate")?,
        recurrence: body.get("recurrence").map(fields::recurrence).transpose()?,
        assignee: None,
    })
}

fn assert_task_patch_fields(
    changes: &TaskChanges,
    assignee_target: Option<&AssigneeTarget>,
    agent_session: Option<&Option<AgentSession>>,
) -> Result<()> {
    if changes.entries().is_empty() && assignee_target.is_none() && agent_session.is_none() {
        return Err(ApiError::new(
            400,
            "INVALID_BODY",
            "PATCH requires at least one task field",
        ));
    }
    Ok(())
}

pub fn task_patch(body: &Value) -> Result<TaskPatch> {
    let body = task_patch_body(body)?;
    let version = fields::version(body.get("version"), false)?;
    let attribution = attribution(body)?;
    let assignee_target = fields::assignee_target(body.get("assigneeTarget"))?;
    let changes = task_patch_changes(body)?;
    if matches!(changes.recurrence, Some(Some(_)))
        && body.get("dueDate").is_some_and(Value::is_null)
    {
        return Err(ApiError::invalid_field(
            "A recurring issue requires 'dueDate'",
        ));
    }
    assert_task_patch_fields(
        &changes,
        assignee_target.as_ref(),
        attribution.agent_session.as_ref(),
    )?;
    Ok(TaskPatch {
        version,
        changes,
        attribution,
        assignee_target,
    })
}

#[derive(Debug, Clone)]
pub struct Move {
    pub version: i64,
    pub status: &'static str,
    pub sort_order: Option<f64>,
    pub attribution: Attribution,
}

pub fn task_move(body: &Value) -> Result<Move> {
    let body = fields::object(body, &["version", "status", "sortOrder", "agentSession"])?;
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
    let body = fields::object(body, &["version", "agentSession"])?;
    Ok(VersionMutation {
        version: fields::version(body.get("version"), false)?,
        attribution: attribution(body)?,
    })
}

#[derive(Debug, Clone)]
pub struct RelationMutation {
    pub version: i64,
    pub attribution: Attribution,
    pub origin: Option<&'static str>,
}

pub fn relation_mutation(body: &Value) -> Result<RelationMutation> {
    let body = fields::object(body, &["version", "agentSession", "origin"])?;
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
    let object = fields::object(body, &["body", "agentSession"])?;
    let text = match nullish(object.get("body")) {
        Some(value) => fields::optional_string(Some(value), "body", 100_000)?.unwrap_or_default(),
        None => String::new(),
    };
    Ok(CommentCreate {
        body: text,
        attribution: attribution(object)?,
    })
}

#[derive(Debug, Clone)]
pub struct CommentPatch {
    pub version: i64,
    pub body: String,
    pub attribution: Attribution,
}

pub fn comment_patch(body: &Value) -> Result<CommentPatch> {
    let object = fields::object(body, &["version", "body", "agentSession"])?;
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

fn assert_query_parameters(
    query: &Query,
    allowed: &[&str],
    repeated_code: &'static str,
) -> Result<()> {
    for (key, _) in query {
        if !allowed.contains(&key.as_str()) {
            return Err(ApiError::new(
                400,
                "UNKNOWN_QUERY_PARAMETER",
                format!("Unknown query parameter '{key}'"),
            ));
        }
        if query.iter().filter(|(other, _)| other == key).count() != 1 {
            return Err(ApiError::new(
                400,
                repeated_code,
                format!("Query parameter '{key}' cannot be repeated"),
            ));
        }
    }
    Ok(())
}

fn query_value<'a>(query: &'a Query, key: &str) -> Option<&'a str> {
    query
        .iter()
        .find(|(name, _)| name == key)
        .map(|(_, value)| value.as_str())
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
    assert_query_parameters(
        query,
        &["projectId", "status", "archived"],
        "INVALID_QUERY_PARAMETER",
    )?;
    let status = query_value(query, "status");
    if status.is_some_and(|status| !is_task_status(status)) {
        return Err(ApiError::new(
            400,
            "INVALID_QUERY_PARAMETER",
            "Invalid task status",
        ));
    }
    let archived = match query_value(query, "archived").unwrap_or("false") {
        "false" => Archived::Exclude,
        "true" => Archived::Only,
        "all" => Archived::All,
        _ => {
            return Err(ApiError::new(
                400,
                "INVALID_QUERY_PARAMETER",
                "'archived' must be true, false, or all",
            ));
        }
    };
    let project_id = query_value(query, "projectId")
        .map(|id| fields::project_id(Some(&Value::String(id.to_owned()))))
        .transpose()?;
    Ok(TaskFilters {
        project_id,
        status: status.map(str::to_owned),
        archived,
    })
}

#[derive(Debug, Clone, Copy)]
pub struct TaskTreeQuery {
    pub direction: &'static str,
    pub depth: u32,
}

pub fn task_tree_query(query: &Query) -> Result<TaskTreeQuery> {
    assert_query_parameters(query, &["direction", "depth"], "INVALID_TREE_QUERY")?;
    task_tree_query_values(query)
}

fn task_tree_query_values(query: &Query) -> Result<TaskTreeQuery> {
    let direction = match query_value(query, "direction") {
        Some("descendants") => "descendants",
        Some("ancestors") => "ancestors",
        _ => {
            return Err(ApiError::new(
                400,
                "INVALID_TREE_QUERY",
                "'direction' must be descendants or ancestors",
            ));
        }
    };
    let depth = query_value(query, "depth")
        .filter(|raw| !raw.is_empty() && raw.bytes().all(|byte| byte.is_ascii_digit()))
        .and_then(|raw| raw.parse::<u32>().ok())
        .filter(|depth| (1..=25).contains(depth));
    let Some(depth) = depth else {
        return Err(ApiError::new(
            400,
            "INVALID_TREE_QUERY",
            "'depth' must be an integer from 1 to 25",
        ));
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
    let content = project_readme_content(body)?;
    let version = body
        .get("version")
        .map(|value| fields::version(Some(value), true))
        .transpose()?;
    Ok(ProjectReadmeSave { content, version })
}

fn project_readme_content(body: &Object) -> Result<String> {
    let content = match nullish(body.get("content")) {
        None => String::new(),
        Some(Value::String(content)) => content.clone(),
        Some(_) => return Err(ApiError::invalid_field("'content' must be a string")),
    };
    if js::len(&content) > 500_000 {
        return Err(ApiError::invalid_field(
            "'content' cannot exceed 500000 characters",
        ));
    }
    Ok(content)
}

/// `PATCH /api/tasks/:id/spec`: the sections an agent writes; omitted sections are kept.
#[derive(Debug, Clone)]
pub struct TaskSpecPatch {
    pub proposal: Option<String>,
    pub specs: Option<String>,
    pub design: Option<String>,
    pub tasks: Option<String>,
}

pub fn task_spec_patch(body: &Value) -> Result<TaskSpecPatch> {
    let body = fields::object(body, &["proposal", "specs", "design", "tasks"])?;
    Ok(TaskSpecPatch {
        proposal: spec_text(body, "proposal")?,
        specs: spec_text(body, "specs")?,
        design: spec_text(body, "design")?,
        tasks: spec_text(body, "tasks")?,
    })
}

fn spec_text(body: &Object, name: &str) -> Result<Option<String>> {
    match nullish(body.get(name)) {
        None => Ok(None),
        Some(Value::String(text)) => Ok(Some(text.clone())),
        Some(_) => Err(ApiError::invalid_field(format!(
            "'{name}' must be a string"
        ))),
    }
}

/// `PUT /api/tasks/:id/spec/tasks/:number`: `{"done": bool}`.
pub fn task_spec_check(body: &Value) -> Result<bool> {
    let body = fields::object(body, &["done"])?;
    match body.get("done") {
        Some(Value::Bool(done)) => Ok(*done),
        _ => Err(ApiError::invalid_field("'done' must be a boolean")),
    }
}
