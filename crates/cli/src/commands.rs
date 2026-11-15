//! Board commands. Each one checks its arguments in the order the contract reports errors, then
//! makes its request; the reply object becomes the command's JSON result.

use std::cmp::Reverse;
use std::path::{Component, Path, PathBuf};

use broadcast_domain::constants::{
    DEFAULT_PROJECT_ID, TASK_STATUSES, is_task_priority, is_task_status,
};
use broadcast_domain::js::{self, encode_uri_component as encode};
use serde_json::{Map, Value, json};
use url::form_urlencoded;

use crate::args::Parsed;
use crate::client::{Api, js_string};
use crate::error::{CliError, Result, file_read, usage};
use crate::secret;
use crate::target::{self, Env};

type Object = Map<String, Value>;

pub async fn execute(parsed: &Parsed, command: &str) -> Result<Object> {
    let env = Env::new(parsed.option("runtime-file"));
    let companion = command.starts_with("cloud ") || command == "project map";
    let target = if companion || env.var("BROADCAST_COMPANION_URL").is_some() {
        target::companion_url(&env)?
    } else {
        target::broadcast_base_url(&env)?
    };
    let board = Board {
        parsed,
        api: Api::new(target)?,
    };
    board.run(command).await
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
            "project map" => {
                p.expect_operands(1)?;
                let pathname = format!("/api/local/project-mappings/{}", encode(p.operand(0)));
                let body =
                    json!({ "workspacePath": resolve_input(p.required("workspace-path")?)? });
                self.api.request("PUT", &pathname, Some(body)).await
            }
            "project readme" => self.project_readme().await,
            "cloud login" => {
                p.expect_operands(0)?;
                let url = p.required("url")?;
                let actor_name = p.required("actor-name")?;
                let remote_url = broadcast_domain::cloud::origin(url)
                    .map_err(|message| CliError::new(message, "INVALID_CLOUD_URL", 2))?;
                let shared_key = secret::read()?;
                if shared_key.is_empty() {
                    return Err(usage("Cloud shared key cannot be empty"));
                }
                let body = json!({ "remoteUrl": remote_url, "actorName": actor_name, "sharedKey": shared_key });
                self.api
                    .request("PUT", "/api/local/cloud-session", Some(body))
                    .await
            }
            "cloud status" => {
                p.expect_operands(0)?;
                self.api
                    .request("GET", "/api/local/cloud-session", None)
                    .await
            }
            "cloud logout" => {
                p.expect_operands(0)?;
                self.api
                    .request("DELETE", "/api/local/cloud-session", None)
                    .await
            }
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
                let (key, attribution) = self.attribution()?;
                let binding = self.thread_binding()?;
                let pathname = format!("{}/move", task_path(task_id)?);
                let mut body = Object::new();
                body.insert("status".into(), status.into());
                body.insert(key.into(), attribution);
                put(&mut body, "threadBinding", binding);
                body.insert(
                    "version".into(),
                    self.version(task_id, p.option("if-version")).await?,
                );
                self.api.request("POST", &pathname, Some(body.into())).await
            }
            "issue archive" | "issue restore" => {
                p.expect_operands(1)?;
                let task_id = p.operand(0);
                let (key, attribution) = self.attribution()?;
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
                        Some(json!({ key: attribution, "version": version })),
                    )
                    .await
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
                let (key, attribution) = self.attribution()?;
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
                        Some(json!({ key: attribution, "version": version })),
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
                let (key, attribution) = self.attribution()?;
                let mut payload = Object::new();
                payload.insert("body".into(), body.into());
                payload.insert(key.into(), attribution);
                put(&mut payload, "threadBinding", self.thread_binding()?);
                self.api
                    .request("POST", &pathname, Some(payload.into()))
                    .await
            }
            "comment update" => {
                p.expect_operands(1)?;
                let pathname = comment_path(p.operand(0))?;
                let body = p.required("body")?;
                let (key, attribution) = self.attribution()?;
                let version = explicit_version(p.option("if-version"), false)?;
                self.api
                    .request(
                        "PATCH",
                        &pathname,
                        Some(json!({ "body": body, key: attribution, "version": version })),
                    )
                    .await
            }
            "comment delete" => {
                p.expect_operands(1)?;
                let pathname = comment_path(p.operand(0))?;
                let (key, attribution) = self.attribution()?;
                let version = explicit_version(p.option("if-version"), false)?;
                self.api
                    .request(
                        "DELETE",
                        &pathname,
                        Some(json!({ key: attribution, "version": version })),
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
            "context current" => {
                p.expect_operands(0)?;
                self.current_context(p.option("cwd")).await
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
        let project_id = match raw_project_id.filter(|id| !id.is_empty()) {
            Some(id) => id.clone(),
            None => match self
                .current_context(None)
                .await?
                .get("project")
                .and_then(|project| project.get("id"))
            {
                None | Some(Value::Null) => DEFAULT_PROJECT_ID.to_owned(),
                Some(id) => js_string(id),
            },
        };
        let pathname = format!("/api/projects/{}/readme", encode(&project_id));

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
        let (key, attribution) = self.attribution()?;
        let mut body = Object::new();
        body.insert("projectId".into(), p.required("project")?.into());
        body.insert("title".into(), p.required("title")?.into());
        body.insert("description".into(), self.description()?.into());
        body.insert("status".into(), status.into());
        body.insert("priority".into(), priority.into());
        body.insert("labels".into(), labels(p.option("labels")));
        body.insert(key.into(), attribution);
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
        let (key, attribution) = self.attribution()?;
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
        if patch.is_empty() && key != "agentSession" {
            return Err(usage("issue update requires at least one field to update"));
        }
        patch.insert(key.into(), attribution);
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

    async fn current_context(&self, cwd: Option<&str>) -> Result<Object> {
        let base = current_dir()?;
        let cwd = resolve(&base, cwd.unwrap_or(""));
        let response = self.api.request("GET", "/api/projects", None).await?;
        let projects = match response.get("projects") {
            Some(Value::Array(projects)) => projects.as_slice(),
            _ => &[],
        };
        let mut matching: Vec<&Value> = projects
            .iter()
            .filter(|candidate| {
                workspace(candidate).is_some_and(|path| cwd.starts_with(resolve(&base, path)))
            })
            .collect();
        matching.sort_by_key(|candidate| Reverse(workspace(candidate).map_or(0, js::len)));
        let project = matching
            .first()
            .copied()
            .or_else(|| {
                projects.iter().find(|candidate| {
                    candidate.get("id").and_then(Value::as_str) == Some(DEFAULT_PROJECT_ID)
                })
            })
            .or(projects.first())
            .cloned()
            .unwrap_or(Value::Null);
        Ok(object(json!({ "cwd": display(&cwd), "project": project })))
    }

    /// Which conversation a write is attributed to: an external agent session, or a Codex thread.
    fn attribution(&self) -> Result<(&'static str, Value)> {
        let p = self.parsed;
        if p.has("agent-platform") || p.has("session-id") {
            if p.has("thread-id") {
                return Err(usage(
                    "Use --agent-platform with --session-id, or Codex --thread-id, not both",
                ));
            }
            let candidate = json!({ "platform": p.required("agent-platform")?, "sessionId": p.required("session-id")? });
            let session = broadcast_domain::input::agent_session(Some(&candidate))
                .map_err(|error| usage(error.message))?;
            let session = session.flatten().map(
                |session| json!({ "platform": session.platform, "sessionId": session.session_id }),
            );
            return Ok(("agentSession", session.unwrap_or(Value::Null)));
        }
        let thread_id = p
            .option("thread-id")
            .map(str::to_owned)
            .or_else(|| std::env::var("CODEX_THREAD_ID").ok());
        let thread_id = thread_id.as_deref().map(js::trim).unwrap_or("");
        if thread_id.is_empty() {
            return Err(usage(
                "Conversation attribution requires --agent-platform with --session-id, or Codex --thread-id or CODEX_THREAD_ID",
            ));
        }
        if js::len(thread_id) > 256 {
            return Err(usage(
                "--thread-id and CODEX_THREAD_ID cannot exceed 256 characters",
            ));
        }
        Ok(("threadId", thread_id.into()))
    }

    /// `None` leaves the binding alone; `Some(null)` clears it.
    fn thread_binding(&self) -> Result<Option<Value>> {
        let p = self.parsed;
        let fields = [
            "binding-thread-id",
            "binding-codex-project-id",
            "binding-codex-project-kind",
            "binding-codex-host-id",
            "binding-workspace-path",
        ];
        if p.flag("clear-binding-thread") {
            if fields.iter().any(|field| p.has(field)) {
                return Err(usage(
                    "--clear-binding-thread cannot be combined with binding identity options",
                ));
            }
            return Ok(Some(Value::Null));
        }
        if !fields.iter().any(|field| p.has(field)) {
            return Ok(None);
        }
        let thread_id = js::trim(p.required("binding-thread-id")?);
        if thread_id.is_empty() || js::len(thread_id) > 256 {
            return Err(usage(
                "--binding-thread-id must contain 1 to 256 characters",
            ));
        }
        let identity = &fields[1..];
        if !identity.iter().any(|field| p.has(field)) {
            return Ok(Some(json!({ "threadId": thread_id })));
        }
        if !identity.iter().all(|field| p.has(field)) {
            return Err(usage(
                "Binding identity requires project id, kind, host id, and workspace path",
            ));
        }
        let value = |name| p.option(name).unwrap_or("");
        let project_id = js::trim(value("binding-codex-project-id"));
        let kind = value("binding-codex-project-kind");
        let host_id = js::trim(value("binding-codex-host-id"));
        let workspace_path = value("binding-workspace-path");
        if project_id.is_empty() || js::len(project_id) > 256 {
            return Err(usage(
                "--binding-codex-project-id must contain 1 to 256 characters",
            ));
        }
        if kind != "local" && kind != "remote" {
            return Err(usage(
                "--binding-codex-project-kind must be local or remote",
            ));
        }
        if host_id.is_empty() || js::len(host_id) > 256 || (kind == "local") != (host_id == "local")
        {
            return Err(usage(
                "--binding-codex-host-id does not match the project kind",
            ));
        }
        if !is_absolute(workspace_path) {
            return Err(usage("--binding-workspace-path must be absolute"));
        }
        Ok(Some(json!({
            "threadId": thread_id,
            "codexProjectId": project_id,
            "codexProjectKind": kind,
            "codexHostId": host_id,
            "workspacePath": workspace_path,
        })))
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

/// Absolute as a POSIX or a Windows path.
fn is_absolute(path: &str) -> bool {
    let bytes = path.as_bytes();
    matches!(bytes.first(), Some(b'/' | b'\\'))
        || (bytes.len() > 2
            && bytes[0].is_ascii_alphabetic()
            && bytes[1] == b':'
            && matches!(bytes[2], b'/' | b'\\'))
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

fn workspace(project: &Value) -> Option<&str> {
    project
        .get("workspacePath")
        .and_then(Value::as_str)
        .filter(|path| !path.is_empty())
}
