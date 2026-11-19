//! Board commands. Each one checks its arguments in the order the contract reports errors, then
//! makes its request; the reply object becomes the command's JSON result.

use std::path::{Component, Path, PathBuf};

use broadcast_domain::constants::{TASK_STATUSES, is_task_priority, is_task_status};
use broadcast_domain::js::{self, encode_uri_component as encode};
use serde_json::{Map, Value, json};
use url::form_urlencoded;

use crate::args::Parsed;
use crate::client::Api;
use crate::error::{CliError, Result, file_read, usage};
use crate::target;

type Object = Map<String, Value>;

pub async fn execute(parsed: &Parsed, command: &str) -> Result<Object> {
    let board = Board {
        parsed,
        api: Api::new(&target::base_url(), acting_agent(parsed))?,
    };
    board.run(command).await
}

/// The agent the CLI writes as: Codex when the write is attributed to a Codex session
/// (`--agent-platform codex`, or `CODEX_THREAD_ID` without `--agent-platform`), Claude otherwise.
fn acting_agent(parsed: &Parsed) -> &'static str {
    let codex = match parsed.option("agent-platform") {
        Some(platform) => platform == "codex",
        None => std::env::var("CODEX_THREAD_ID").is_ok_and(|id| !js::trim(&id).is_empty()),
    };
    if codex { "codex" } else { "claude" }
}

struct Board<'a> {
    parsed: &'a Parsed,
    api: Api,
}

impl Board<'_> {
    async fn run(&self, command: &str) -> Result<Object> {
        let p = self.parsed;
        match command {
            "project list" => {
                p.expect_operands(0)?;
                self.api.request("GET", "/api/projects", None).await
            }
            "project create" => {
                p.expect_operands(0)?;
                let mut body = Object::new();
                put(&mut body, "id", p.option("id"));
                body.insert("name".into(), p.required("name")?.into());
                put(
                    &mut body,
                    "workspacePath",
                    p.option("workspace-path").map(resolve_input).transpose()?,
                );
                self.api
                    .request("POST", "/api/projects", Some(body.into()))
                    .await
            }
            "project readme" => self.project_readme().await,
            "issue list" => {
                p.expect_operands(0)?;
                if let Some(status) = p.option("status") {
                    assert_status(status)?;
                }
                if p.option("archived")
                    .is_some_and(|archived| !["true", "false", "all"].contains(&archived))
                {
                    return Err(usage("--archived must be true, false, or all"));
                }
                let search = query(&[
                    ("projectId", p.option("project")),
                    ("status", p.option("status")),
                    ("archived", p.option("archived")),
                ]);
                self.api
                    .request("GET", &format!("/api/tasks{search}"), None)
                    .await
            }
            "issue get" => {
                p.expect_operands(1)?;
                self.api
                    .request("GET", &task_path(p.operand(0))?, None)
                    .await
            }
            "issue create" => {
                p.expect_operands(0)?;
                self.create_issue().await
            }
            "issue update" => {
                p.expect_operands(1)?;
                self.update_issue(p.operand(0)).await
            }
            "issue move" => {
                p.expect_operands(1)?;
                let task_id = p.operand(0);
                let status = p.required("status")?;
                assert_status(status)?;
                let attribution = self.attribution()?;
                let pathname = format!("{}/move", task_path(task_id)?);
                let mut body = Object::new();
                body.insert("status".into(), status.into());
                put(&mut body, "agentSession", attribution);
                body.insert(
                    "version".into(),
                    self.version(task_id, p.option("if-version")).await?,
                );
                self.api.request("POST", &pathname, Some(body.into())).await
            }
            "issue archive" | "issue restore" => {
                p.expect_operands(1)?;
                let task_id = p.operand(0);
                let attribution = self.attribution()?;
                let pathname = format!(
                    "{}/{}",
                    task_path(task_id)?,
                    p.action.as_deref().unwrap_or("")
                );
                let version = self.version(task_id, p.option("if-version")).await?;
                self.api
                    .request(
                        "POST",
                        &pathname,
                        Some(attributed(json!({ "version": version }), attribution)),
                    )
                    .await
            }
            "issue spec" => {
                let action = p.operand(0);
                match action {
                    "get" => {
                        p.expect_operands(2)?;
                        self.api
                            .request("GET", &format!("{}/spec", task_path(p.operand(1))?), None)
                            .await
                    }
                    "set" => {
                        p.expect_operands(2)?;
                        let mut body = Map::new();
                        for section in ["proposal", "specs", "design", "tasks"] {
                            let option = format!("{section}-file");
                            if let Some(file) = p.option(&option) {
                                let text = read_text(file).map_err(|error| {
                                    file_read(format!("Cannot read --{option}: {file}"), error)
                                })?;
                                body.insert(section.to_owned(), Value::String(text));
                            }
                        }
                        if body.is_empty() {
                            return Err(usage(
                                "issue spec set needs --proposal-file, --specs-file, --design-file, or --tasks-file",
                            ));
                        }
                        self.api
                            .request(
                                "PATCH",
                                &format!("{}/spec", task_path(p.operand(1))?),
                                Some(Value::Object(body)),
                            )
                            .await
                    }
                    "check" => {
                        p.expect_operands(3)?;
                        self.api
                            .request(
                                "PUT",
                                &format!(
                                    "{}/spec/tasks/{}",
                                    task_path(p.operand(1))?,
                                    encode(p.operand(2))
                                ),
                                Some(json!({ "done": !p.has("undo") })),
                            )
                            .await
                    }
                    _ => Err(usage("issue spec action must be get, set, or check")),
                }
            }
            "issue tree" => {
                p.expect_operands(1)?;
                let direction = p.required("direction")?;
                if direction != "descendants" && direction != "ancestors" {
                    return Err(usage("--direction must be descendants or ancestors"));
                }
                let raw_depth = p.required("depth")?;
                let depth = js::string_to_number(raw_depth);
                if !raw_depth.bytes().all(|byte| byte.is_ascii_digit())
                    || !js::is_safe_integer(depth)
                    || !(1.0..=25.0).contains(&depth)
                {
                    return Err(usage("--depth must be an integer from 1 to 25"));
                }
                let search = query(&[
                    ("direction", Some(direction)),
                    ("depth", Some(&(depth as i64).to_string())),
                ]);
                self.api
                    .request(
                        "GET",
                        &format!("{}/tree{search}", task_path(p.operand(0))?),
                        None,
                    )
                    .await
            }
            "issue relation" => {
                p.expect_operands(2)?;
                let (action, task_id) = (p.operand(0), p.operand(1));
                if action != "add" && action != "remove" {
                    return Err(usage("issue relation action must be add or remove"));
                }
                let kind = p.required("type")?;
                if !["parent", "blocks", "blocked_by", "related"].contains(&kind) {
                    return Err(usage(
                        "--type must be parent, blocks, blocked_by, or related",
                    ));
                }
                let related = p.required("issue")?;
                let attribution = self.attribution()?;
                let version = self.version(task_id, p.option("if-version")).await?;
                let method = if action == "add" { "POST" } else { "DELETE" };
                let pathname = format!(
                    "{}/relations/{kind}/{}",
                    task_path(task_id)?,
                    encode(related)
                );
                self.api
                    .request(
                        method,
                        &pathname,
                        Some(attributed(json!({ "version": version }), attribution)),
                    )
                    .await
            }
            "comment list" => {
                p.expect_operands(1)?;
                let search = query(&[("after", p.option("after"))]);
                self.api
                    .request(
                        "GET",
                        &format!("{}/comments{search}", task_path(p.operand(0))?),
                        None,
                    )
                    .await
            }
            "comment add" => {
                p.expect_operands(1)?;
                if p.has("body") && p.has("body-file") {
                    return Err(usage("Use either --body or --body-file, not both"));
                }
                let body = match p.option("body-file") {
                    None => p.required("body")?.to_owned(),
                    Some(file) => read_text(file).map_err(|error| {
                        file_read(format!("Cannot read comment body file: {file}"), error)
                    })?,
                };
                let pathname = format!("{}/comments", task_path(p.operand(0))?);
                let attribution = self.attribution()?;
                let mut payload = Object::new();
                payload.insert("body".into(), body.into());
                put(&mut payload, "agentSession", attribution);
                self.api
                    .request("POST", &pathname, Some(payload.into()))
                    .await
            }
            "comment update" => {
                p.expect_operands(1)?;
                let pathname = comment_path(p.operand(0))?;
                let body = p.required("body")?;
                let attribution = self.attribution()?;
                let version = explicit_version(p.option("if-version"), false)?;
                self.api
                    .request(
                        "PATCH",
                        &pathname,
                        Some(attributed(
                            json!({ "body": body, "version": version }),
                            attribution,
                        )),
                    )
                    .await
            }
            "comment delete" => {
                p.expect_operands(1)?;
                let pathname = comment_path(p.operand(0))?;
                let attribution = self.attribution()?;
                let version = explicit_version(p.option("if-version"), false)?;
                self.api
                    .request(
                        "DELETE",
                        &pathname,
                        Some(attributed(json!({ "version": version }), attribution)),
                    )
                    .await
            }
            "attachment list" => {
                p.expect_operands(0)?;
                let target = attachment_target(p, "list")?;
                let search = query(&[("after", p.option("after"))]);
                self.api
                    .request(
                        "GET",
                        &format!("{}/attachments{search}", target.path()?),
                        None,
                    )
                    .await
            }
            "attachment download" => {
                p.expect_operands(1)?;
                self.download_attachment(p.operand(0)).await
            }
            "attachment upload" => {
                p.expect_operands(0)?;
                self.upload_attachment().await
            }
            _ => Err(usage(format!("Unsupported command: {command}"))),
        }
    }

    async fn project_readme(&self) -> Result<Object> {
        let p = self.parsed;
        let first = p.operands.first().map(String::as_str);
        let (explicit_set, explicit_get) = (first == Some("set"), first == Some("get"));
        let is_set = explicit_set || (!explicit_get && (p.has("content") || p.has("file")));
        let raw_project_id = if explicit_set || explicit_get {
            if p.operands.len() > 2 {
                let action = first.unwrap_or("");
                return Err(usage(format!(
                    "project readme {action} accepts at most 1 positional argument (project id)"
                )));
            }
            p.operands.get(1)
        } else {
            if p.operands.len() > 1 {
                return Err(usage(
                    "project readme accepts at most 1 positional argument (project id)",
                ));
            }
            p.operands.first()
        };
        let project_id = raw_project_id
            .filter(|id| !id.is_empty())
            .ok_or_else(|| usage("project readme requires a project id"))?;
        let pathname = format!("/api/projects/{}/readme", encode(project_id));

        if is_set {
            if p.has("content") && p.has("file") {
                return Err(usage("Use either --content or --file, not both"));
            }
            let content = match (p.option("file"), p.option("content")) {
                (Some(file), _) => read_text(file)
                    .map_err(|error| file_read(format!("Cannot read file: {file}"), error))?,
                (None, Some(content)) => content.to_owned(),
                (None, None) => {
                    return Err(usage("project readme set requires --content or --file"));
                }
            };
            let mut body = Object::new();
            body.insert("content".into(), content.into());
            if let Some(raw) = p.option("if-version") {
                body.insert("version".into(), explicit_version(Some(raw), true)?);
            }
            return self.api.request("PUT", &pathname, Some(body.into())).await;
        }
        if p.has("content") || p.has("file") || p.has("if-version") {
            return Err(usage(
                "project readme get does not accept --content, --file, or --if-version",
            ));
        }
        self.api.request("GET", &pathname, None).await
    }

    async fn create_issue(&self) -> Result<Object> {
        let p = self.parsed;
        let status = p.option("status").unwrap_or("backlog");
        let priority = p.option("priority").unwrap_or("none");
        assert_status(status)?;
        assert_priority(priority)?;
        let development_context = self.development_context()?;
        let recurrence = self.recurrence()?;
        let attribution = self.attribution()?;
        let mut body = Object::new();
        body.insert("projectId".into(), p.required("project")?.into());
        body.insert("title".into(), p.required("title")?.into());
        body.insert("description".into(), self.description()?.into());
        body.insert("status".into(), status.into());
        body.insert("priority".into(), priority.into());
        body.insert("labels".into(), labels(p.option("labels")));
        put(&mut body, "agentSession", attribution);
        put(&mut body, "developmentContext", development_context);
        put(&mut body, "startDate", p.option("start-date"));
        put(&mut body, "dueDate", p.option("due-date"));
        put(&mut body, "recurrence", recurrence);
        self.api
            .request("POST", "/api/tasks", Some(body.into()))
            .await
    }

    async fn update_issue(&self, task_id: &str) -> Result<Object> {
        let p = self.parsed;
        if let Some(status) = p.option("status") {
            assert_status(status)?;
        }
        if let Some(priority) = p.option("priority") {
            assert_priority(priority)?;
        }
        let development_context = self.development_context()?;
        let recurrence = self.recurrence()?;
        let attribution = self.attribution()?;
        let mut patch = Object::new();
        put(&mut patch, "projectId", p.option("project"));
        put(&mut patch, "title", p.option("title"));
        put(&mut patch, "status", p.option("status"));
        put(&mut patch, "priority", p.option("priority"));
        put(
            &mut patch,
            "labels",
            p.has("labels").then(|| labels(p.option("labels"))),
        );
        put(&mut patch, "developmentContext", development_context);
        put(&mut patch, "startDate", p.option("start-date"));
        put(&mut patch, "dueDate", p.option("due-date"));
        put(&mut patch, "recurrence", recurrence);
        if p.has("description") || p.has("description-file") {
            patch.insert("description".into(), self.description()?.into());
        }
        if patch.is_empty() && !p.has("agent-platform") {
            return Err(usage("issue update requires at least one field to update"));
        }
        put(&mut patch, "agentSession", attribution);
        patch.insert(
            "version".into(),
            self.version(task_id, p.option("if-version")).await?,
        );
        self.api
            .request("PATCH", &task_path(task_id)?, Some(patch.into()))
            .await
    }

    async fn download_attachment(&self, attachment_id: &str) -> Result<Object> {
        let output = resolve_input(self.parsed.required("output")?)?;
        let pathname = attachment_content_path(attachment_id)?;
        let downloaded = self.api.download(&pathname).await?;
        std::fs::write(&output, &downloaded.body).map_err(|error| {
            CliError::new(
                format!("Cannot write attachment file: {output}"),
                "FILE_WRITE_FAILED",
                2,
            )
            .details(error)
        })?;
        let declared = downloaded
            .content_length
            .as_deref()
            .map(js::string_to_number)
            .filter(|size| *size != 0.0 && !size.is_nan());
        let size = declared.map_or_else(|| json!(downloaded.body.len()), js::number);
        Ok(object(json!({
            "attachmentId": attachment_id,
            "output": output,
            "contentType": downloaded.content_type,
            "size": size,
        })))
    }

    async fn upload_attachment(&self) -> Result<Object> {
        let p = self.parsed;
        let target = attachment_target(p, "upload")?;
        let file_path = resolve_input(p.required("file")?)?;
        let bytes = std::fs::read(&file_path).map_err(|error| {
            file_read(format!("Cannot read attachment file: {file_path}"), error)
        })?;
        let filename = Path::new(&file_path)
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default();
        if filename.is_empty() || filename == "." || filename == ".." {
            return Err(usage("Attachment --file must include a valid filename"));
        }
        let content_type = match p.option("content-type").filter(|value| !value.is_empty()) {
            Some(value) => js::trim(value).to_lowercase(),
            None => guess_content_type(&filename).to_owned(),
        };
        if content_type.is_empty() {
            return Err(usage("--content-type cannot be empty"));
        }
        let kind = p
            .option("kind")
            .unwrap_or(if content_type.starts_with("image/") {
                "inline"
            } else {
                "attachment"
            });
        if kind != "inline" && kind != "attachment" {
            return Err(usage("--kind must be inline or attachment"));
        }
        let pathname = format!("{}/attachments", target.path()?);
        let payload = self
            .api
            .upload(&pathname, bytes, &content_type, &filename, kind)
            .await?;
        Ok(object(json!({
            "attachment": payload.get("attachment").cloned().unwrap_or(Value::Null),
            "file": file_path,
            "kind": kind,
            "target": { "type": target.kind, "id": target.id },
        })))
    }

    /// The agent conversation a write is attributed to: `--agent-platform` with `--session-id`,
    /// or the Codex session in `CODEX_THREAD_ID`.
    fn attribution(&self) -> Result<Option<Value>> {
        let p = self.parsed;
        let session = if p.has("agent-platform") || p.has("session-id") {
            json!({ "platform": p.required("agent-platform")?, "sessionId": p.required("session-id")? })
        } else {
            match std::env::var("CODEX_THREAD_ID") {
                Ok(id) if !js::trim(&id).is_empty() => {
                    json!({ "platform": "codex", "sessionId": js::trim(&id) })
                }
                _ => return Ok(None),
            }
        };
        broadcast_domain::input::agent_session(Some(&session))
            .map_err(|error| usage(error.message))?;
        Ok(Some(session))
    }

    fn development_context(&self) -> Result<Option<Value>> {
        let p = self.parsed;
        let (branch, worktree_path, worktree_branch) = (
            p.option("git-branch"),
            p.option("worktree-path"),
            p.option("worktree-branch"),
        );
        if branch.is_some() && (worktree_path.is_some() || worktree_branch.is_some()) {
            return Err(usage(
                "Use either --git-branch or --worktree-path/--worktree-branch, not both",
            ));
        }
        if worktree_branch.is_some() && worktree_path.is_none() {
            return Err(usage("--worktree-branch requires --worktree-path"));
        }
        if let Some(branch) = branch {
            return Ok(Some(json!({ "type": "branch", "branch": branch })));
        }
        let Some(worktree_path) = worktree_path else {
            return Ok(None);
        };
        Ok(Some(
            json!({ "type": "worktree", "path": resolve_input(worktree_path)?, "branch": worktree_branch }),
        ))
    }

    fn recurrence(&self) -> Result<Option<Value>> {
        let p = self.parsed;
        let (raw_interval, unit) =
            match (p.option("recurrence-interval"), p.option("recurrence-unit")) {
                (None, None) => return Ok(None),
                (Some(interval), Some(unit)) => (interval, unit),
                _ => {
                    return Err(usage(
                        "Use --recurrence-interval and --recurrence-unit together",
                    ));
                }
            };
        let interval = js::string_to_number(raw_interval);
        if !js::is_safe_integer(interval) || !(1.0..=365.0).contains(&interval) {
            return Err(usage(
                "--recurrence-interval must be an integer from 1 to 365",
            ));
        }
        if !["day", "week", "month", "year"].contains(&unit) {
            return Err(usage("--recurrence-unit must be day, week, month, or year"));
        }
        Ok(Some(
            json!({ "interval": js::number(interval), "unit": unit }),
        ))
    }

    fn description(&self) -> Result<String> {
        let p = self.parsed;
        if p.has("description") && p.has("description-file") {
            return Err(usage(
                "Use either --description or --description-file, not both",
            ));
        }
        match p.option("description-file") {
            None => Ok(p.option("description").unwrap_or("").to_owned()),
            Some(file) => read_text(file)
                .map_err(|error| file_read(format!("Cannot read description file: {file}"), error)),
        }
    }

    /// `--if-version`, or the task's current version.
    async fn version(&self, task_id: &str, raw: Option<&str>) -> Result<Value> {
        if let Some(raw) = raw {
            let version = js::string_to_number(raw);
            if !js::is_safe_integer(version) || version < 1.0 {
                return Err(usage("--if-version must be a positive integer"));
            }
            return Ok(js::number(version));
        }
        let response = self.api.request("GET", &task_path(task_id)?, None).await?;
        match js::safe_integer(response.get("task").and_then(|task| task.get("version"))) {
            Some(version) if version >= 1 => Ok(version.into()),
            _ => Err(CliError::new(
                "Broadcast service returned a task without a valid version",
                "INVALID_RESPONSE",
                4,
            )),
        }
    }
}

struct AttachmentTarget<'a> {
    kind: &'static str,
    id: &'a str,
}

impl AttachmentTarget<'_> {
    fn path(&self) -> Result<String> {
        if self.kind == "task" {
            task_path(self.id)
        } else {
            comment_path(self.id)
        }
    }
}

fn attachment_target<'a>(p: &'a Parsed, action: &str) -> Result<AttachmentTarget<'a>> {
    let task = p.option("task").filter(|id| !id.is_empty());
    let comment = p.option("comment").filter(|id| !id.is_empty());
    match (task, comment) {
        (Some(id), None) => Ok(AttachmentTarget { kind: "task", id }),
        (None, Some(id)) => Ok(AttachmentTarget {
            kind: "comment",
            id,
        }),
        _ => Err(usage(format!(
            "attachment {action} requires exactly one of --task or --comment"
        ))),
    }
}

fn put(object: &mut Object, name: &str, value: Option<impl Into<Value>>) {
    if let Some(value) = value {
        object.insert(name.to_owned(), value.into());
    }
}

/// `body` with `agentSession` added when the write is attributed.
fn attributed(body: Value, attribution: Option<Value>) -> Value {
    let mut body = object(body);
    put(&mut body, "agentSession", attribution);
    body.into()
}

fn object(value: Value) -> Object {
    match value {
        Value::Object(object) => object,
        _ => Object::new(),
    }
}

fn query(pairs: &[(&str, Option<&str>)]) -> String {
    let mut serializer = form_urlencoded::Serializer::new(String::new());
    for (name, value) in pairs {
        if let Some(value) = value {
            serializer.append_pair(name, value);
        }
    }
    let query = serializer.finish();
    if query.is_empty() {
        query
    } else {
        format!("?{query}")
    }
}

fn task_path(id: &str) -> Result<String> {
    if id.is_empty() {
        Err(usage("Missing issue id"))
    } else {
        Ok(format!("/api/tasks/{}", encode(id)))
    }
}

fn comment_path(id: &str) -> Result<String> {
    if id.is_empty() {
        Err(usage("Missing comment id"))
    } else {
        Ok(format!("/api/comments/{}", encode(id)))
    }
}

fn attachment_content_path(id: &str) -> Result<String> {
    if id.is_empty() {
        Err(usage("Missing attachment id"))
    } else {
        Ok(format!("/api/attachments/{}/content", encode(id)))
    }
}

fn assert_status(status: &str) -> Result<()> {
    if is_task_status(status) {
        return Ok(());
    }
    Err(usage(format!(
        "Invalid status: {status}. Expected one of: {}",
        TASK_STATUSES.join(", ")
    )))
}

fn assert_priority(priority: &str) -> Result<()> {
    if is_task_priority(priority) {
        Ok(())
    } else {
        Err(usage(format!("Invalid priority: {priority}")))
    }
}

/// Distinct, trimmed, non-empty labels from `a,b`.
fn labels(raw: Option<&str>) -> Value {
    let mut labels: Vec<&str> = Vec::new();
    for label in raw
        .unwrap_or("")
        .split(',')
        .map(js::trim)
        .filter(|label| !label.is_empty())
    {
        if !labels.contains(&label) {
            labels.push(label);
        }
    }
    json!(labels)
}

fn explicit_version(raw: Option<&str>, allow_zero: bool) -> Result<Value> {
    let raw = raw.ok_or_else(|| usage("Missing required option --if-version"))?;
    let version = js::string_to_number(raw);
    if !js::is_safe_integer(version) || version < if allow_zero { 0.0 } else { 1.0 } {
        let kind = if allow_zero {
            "non-negative"
        } else {
            "positive"
        };
        return Err(usage(format!("--if-version must be a {kind} integer")));
    }
    Ok(js::number(version))
}

fn guess_content_type(filename: &str) -> &'static str {
    let extension = filename
        .rfind('.')
        .filter(|index| *index > 0 && filename != "..")
        .map(|index| filename[index..].to_lowercase());
    match extension.as_deref() {
        Some(".png") => "image/png",
        Some(".jpg" | ".jpeg") => "image/jpeg",
        Some(".gif") => "image/gif",
        Some(".webp") => "image/webp",
        Some(".svg") => "image/svg+xml",
        Some(".md") => "text/markdown",
        Some(".txt") => "text/plain",
        Some(".json") => "application/json",
        Some(".pdf") => "application/pdf",
        Some(".html" | ".htm") => "text/html",
        _ => "application/octet-stream",
    }
}

fn read_text(path: &str) -> std::io::Result<String> {
    std::fs::read(path).map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
}

fn current_dir() -> Result<PathBuf> {
    std::env::current_dir().map_err(|error| CliError::new(error.to_string(), "INTERNAL_ERROR", 1))
}

/// `path.resolve(base, value)`: joined, with `.` and `..` resolved lexically.
fn resolve(base: &Path, value: &str) -> PathBuf {
    let mut resolved = PathBuf::new();
    for component in base.join(value).components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                resolved.pop();
            }
            other => resolved.push(other),
        }
    }
    resolved
}

fn resolve_input(value: &str) -> Result<String> {
    Ok(display(&resolve(&current_dir()?, value)))
}

fn display(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}
