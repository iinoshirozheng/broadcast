//! Claude runner. It picks up `todo` issues assigned to Claude Agent and issues with a new
//! comment that mentions @claude, claims one by moving it to `in_progress` (the version check
//! makes the claim exclusive), runs `claude -p` in its working directory, posts the result as a
//! comment, and moves the issue to `in_review` (or `blocked` when Claude fails).

use std::path::PathBuf;
use std::process::Stdio;
use std::sync::LazyLock;
use std::time::Duration;

use anyhow::{Context, bail};
use regex::Regex;
use reqwest::{Method, StatusCode};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use url::Url;

const CLAUDE_AGENT_ID: &str = "claude-agent";
/// Issues already in progress, done or canceled are not picked up.
const SUMMONABLE_STATUSES: [&str; 4] = ["backlog", "todo", "in_review", "blocked"];
static MENTION: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)(^|\s)@claude(?-u:\b)").expect("mention pattern"));

pub struct Options {
    pub url: Url,
    pub cwd: PathBuf,
    pub permission_mode: String,
    pub interval: Duration,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Task {
    id: String,
    identifier: String,
    title: String,
    description: String,
    status: String,
    version: Value,
    assignee: Assignee,
    agent_session: Option<AgentSession>,
}

#[derive(Deserialize)]
struct Assignee {
    id: String,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct AgentSession {
    platform: String,
    session_id: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Comment {
    author_id: String,
    author_name: String,
    body: String,
}

#[derive(Deserialize)]
struct Tasks {
    tasks: Vec<Task>,
}

#[derive(Deserialize)]
struct Comments {
    comments: Vec<Comment>,
}

#[derive(Deserialize)]
struct TaskResponse {
    task: Task,
}

/// A non-2xx board response.
#[derive(Debug)]
struct ApiRequestError {
    status: StatusCode,
    message: String,
}

impl std::fmt::Display for ApiRequestError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for ApiRequestError {}

struct Outcome {
    ok: bool,
    text: String,
}

fn log(message: &str) {
    let at = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%S%.3fZ");
    println!("[claude-runner] {at} {message}");
}

struct Runner {
    options: Options,
    http: reqwest::Client,
}

impl Runner {
    async fn api<T: for<'de> Deserialize<'de>>(&self, method: Method, pathname: &str, body: Option<Value>) -> anyhow::Result<T> {
        let mut request = self
            .http
            .request(method, self.options.url.join(pathname)?)
            .header("accept", "application/json")
            .header("content-type", "application/json")
            .header("x-broadcast-client", "claude-runner");
        if let Some(body) = body {
            request = request.body(body.to_string());
        }
        let response = request.send().await?;
        let status = response.status();
        let data = serde_json::from_slice::<Value>(&response.bytes().await?).unwrap_or(Value::Null);
        if !status.is_success() {
            let message = match data.pointer("/error/message") {
                Some(Value::String(message)) => message.clone(),
                Some(message) if !message.is_null() => message.to_string(),
                _ => format!("HTTP {}", status.as_u16()),
            };
            bail!(ApiRequestError { status, message });
        }
        Ok(serde_json::from_value(data)?)
    }

    async fn run_claude(&self, prompt: &str, session_args: [&str; 2]) -> std::io::Result<Outcome> {
        let output = tokio::process::Command::new("claude")
            .args(["-p", prompt, "--output-format", "json"])
            .args(session_args)
            .args(["--permission-mode", &self.options.permission_mode])
            .current_dir(&self.options.cwd)
            .stdin(Stdio::null())
            .kill_on_drop(true)
            .output()
            .await?;
        if let Ok(result) = serde_json::from_slice::<Value>(&output.stdout)
            && !result.is_null()
        {
            let text = match result.get("result") {
                Some(Value::String(text)) => text.clone(),
                None | Some(Value::Null) => String::new(),
                Some(other) => other.to_string(),
            };
            let failed = result.get("is_error").and_then(Value::as_bool).unwrap_or(false);
            return Ok(Outcome { ok: !failed, text });
        }
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
        let code = output.status.code().map_or_else(|| "null".to_owned(), |code| code.to_string());
        Ok(Outcome { ok: false, text: if stderr.is_empty() { format!("claude exited with code {code}") } else { stderr } })
    }

    async fn work(&self, task: &Task, agent_session: &AgentSession, prompt: &str, session_args: [&str; 2]) -> anyhow::Result<()> {
        let claim = json!({ "status": "in_progress", "version": task.version, "agentSession": agent_session });
        if let Err(error) = self.api::<Value>(Method::POST, &format!("/api/tasks/{}/move", task.id), Some(claim)).await {
            if error.downcast_ref::<ApiRequestError>().is_some_and(|error| error.status == StatusCode::CONFLICT) {
                log(&format!("{} was changed or claimed elsewhere; skipping", task.identifier));
                return Ok(());
            }
            return Err(error);
        }
        log(&format!(
            "{} claimed; running claude (session {}) in {}",
            task.identifier,
            agent_session.session_id,
            self.options.cwd.display()
        ));

        let outcome = self.run_claude(prompt, session_args).await.unwrap_or_else(|error| Outcome {
            ok: false,
            text: format!("Claude runner could not start claude: {error}"),
        });

        let comment = json!({ "body": outcome.text, "agentSession": agent_session });
        self.api::<Value>(Method::POST, &format!("/api/tasks/{}/comments", task.id), Some(comment)).await?;
        let TaskResponse { task: current } = self.api(Method::GET, &format!("/api/tasks/{}", task.id), None).await?;
        let status = if outcome.ok { "in_review" } else { "blocked" };
        let finish = json!({ "status": status, "version": current.version, "agentSession": agent_session });
        self.api::<Value>(Method::POST, &format!("/api/tasks/{}/move", task.id), Some(finish)).await?;
        log(&format!("{} -> {status}", task.identifier));
        Ok(())
    }

    /// Returns true when it handled an issue. Claude works on an issue when it is delegated
    /// (`todo` and assigned to Claude Agent) or when a comment since Claude's last reply
    /// mentions @claude. It resumes the Claude session already bound to the issue, or starts a
    /// new one.
    async fn work_on_next_issue(&self) -> anyhow::Result<bool> {
        let Tasks { tasks } = self.api(Method::GET, "/api/tasks", None).await?;
        for task in tasks {
            if !SUMMONABLE_STATUSES.contains(&task.status.as_str()) {
                continue;
            }
            let Comments { comments } = self.api(Method::GET, &format!("/api/tasks/{}/comments", task.id), None).await?;
            let start = comments.iter().rposition(|comment| comment.author_id == CLAUDE_AGENT_ID).map_or(0, |last| last + 1);
            let pending = &comments[start..];
            let delegated = task.status == "todo" && task.assignee.id == CLAUDE_AGENT_ID;
            if !delegated && !pending.iter().any(|comment| MENTION.is_match(&comment.body)) {
                continue;
            }

            let existing = task.agent_session.clone().filter(|session| session.platform == "claude");
            let resuming = existing.is_some();
            let agent_session = existing
                .unwrap_or_else(|| AgentSession { platform: "claude".to_owned(), session_id: uuid::Uuid::new_v4().to_string() });
            let prompt = build_prompt(&task, pending, resuming);
            let flag = if resuming { "--resume" } else { "--session-id" };
            self.work(&task, &agent_session, &prompt, [flag, &agent_session.session_id]).await?;
            return Ok(true);
        }
        Ok(false)
    }
}

fn build_prompt(task: &Task, comments: &[Comment], resuming: bool) -> String {
    let heading = if resuming {
        format!("New comments on Broadcast issue {}:", task.identifier)
    } else {
        format!("You are working on Broadcast issue {}: {}\n\n{}", task.identifier, task.title, task.description)
    };
    std::iter::once(heading)
        .chain(comments.iter().map(|comment| format!("{}: {}", comment.author_name, comment.body)))
        .chain(std::iter::once(
            "Do what is asked in the current directory. When you are done, reply concisely with the answer \
             or a summary of what you did and anything the reviewer should check, in the same language as the request."
                .to_owned(),
        ))
        .collect::<Vec<_>>()
        .join("\n\n")
}

/// Polls the board until the future is dropped; dropping it also kills a running `claude`.
pub async fn run(options: Options) -> anyhow::Result<()> {
    let http = reqwest::Client::builder().build().context("HTTP client")?;
    let runner = Runner { options, http };
    log(&format!("watching {} for issues delegated to or mentioning @claude", runner.options.url));
    loop {
        match runner.work_on_next_issue().await {
            Ok(true) => continue,
            Ok(false) => {}
            Err(error) => log(&format!("error: {error}")),
        }
        tokio::time::sleep(runner.options.interval).await;
    }
}
