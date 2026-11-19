//! Agent runner. It reports itself online with a heartbeat, which also tells it the agent it acts
//! as. It picks up `todo` issues assigned to that agent and issues with a new comment that mentions
//! the agent (@claude or @codex; when the runner has an owner, only the owner's comments), claims
//! one by moving it to `in_progress` (the version check makes the claim exclusive), runs the
//! agent's CLI in its working directory, posts the result as a comment, and moves the issue to
//! `in_review` (or `blocked` when the agent fails).

use std::convert::Infallible;
use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context, bail};
use regex::Regex;
use reqwest::{Method, StatusCode};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use url::Url;

/// The board shows the runner offline once heartbeats stop for longer than its presence window.
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(15);
/// Issues already in progress, done or canceled are not picked up.
const SUMMONABLE_STATUSES: [&str; 4] = ["backlog", "todo", "in_review", "blocked"];

/// The agent CLI a runner drives.
#[derive(Clone, Copy)]
pub enum Agent {
    Claude,
    Codex,
}

impl Agent {
    /// The agent's session platform, @-mention handle, executable and runner path.
    fn name(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
        }
    }
}

pub struct Options {
    pub agent: Agent,
    /// The board's base URL.
    pub url: Url,
    pub cwd: PathBuf,
    /// Claude's `--permission-mode` or Codex's `sandbox_mode`.
    pub permission_mode: String,
    pub interval: Duration,
}

/// The agent's executable on `PATH`, then `~/.local/bin` (Claude's native installer), which a
/// process started outside a login shell does not have on `PATH`.
fn executable(agent: Agent) -> PathBuf {
    let name = if cfg!(windows) {
        format!("{}.exe", agent.name())
    } else {
        agent.name().to_owned()
    };
    std::env::var_os("PATH")
        .into_iter()
        .flat_map(|paths| std::env::split_paths(&paths).collect::<Vec<_>>())
        .chain(std::env::home_dir().map(|home| home.join(".local").join("bin")))
        .map(|directory| directory.join(&name))
        .find(|candidate| candidate.is_file())
        .unwrap_or_else(|| PathBuf::from(name))
}

/// The agent this runner acts as and, on a cloud board, the user it acts for.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Identity {
    agent_id: String,
    owner_id: Option<String>,
}

#[derive(Deserialize)]
struct Heartbeat {
    runner: Identity,
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
    agent_session: Option<AgentSession>,
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
    /// The session Codex started; Claude's is chosen before it runs.
    session_id: Option<String>,
}

impl Outcome {
    fn failed(text: String) -> Self {
        Self {
            ok: false,
            text,
            session_id: None,
        }
    }
}

fn log(agent: Agent, message: &str) {
    let at = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%S%.3fZ");
    println!("[{}-runner] {at} {message}", agent.name());
}

struct Runner {
    options: Options,
    executable: PathBuf,
    mention: Regex,
    http: reqwest::Client,
}

impl Runner {
    async fn api<T: for<'de> Deserialize<'de>>(
        &self,
        method: Method,
        pathname: &str,
        body: Option<Value>,
    ) -> anyhow::Result<T> {
        let mut request = self
            .http
            .request(
                method,
                self.options.url.join(pathname.trim_start_matches('/'))?,
            )
            .header("accept", "application/json")
            .header("content-type", "application/json")
            .header("x-broadcast-client", self.options.agent.name());
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

    async fn run_agent(
        &self,
        prompt: &str,
        session: Option<&AgentSession>,
        resuming: bool,
    ) -> std::io::Result<Outcome> {
        let mut command = tokio::process::Command::new(&self.executable);
        let mode = &self.options.permission_mode;
        match (self.options.agent, session) {
            (Agent::Claude, Some(session)) => {
                let flag = if resuming { "--resume" } else { "--session-id" };
                command
                    .args(["-p", prompt, "--output-format", "json"])
                    .args([flag, &session.session_id])
                    .args(["--permission-mode", mode]);
            }
            (Agent::Claude, None) => unreachable!("a Claude session id is chosen before it runs"),
            (Agent::Codex, session) => {
                command.arg("exec");
                if session.is_some() {
                    command.arg("resume");
                }
                command.args(["--json", "-c", &format!("sandbox_mode=\"{mode}\"")]);
                if let Some(session) = session {
                    command.arg(&session.session_id);
                }
                command.arg(prompt);
            }
        }
        let output = command
            .current_dir(&self.options.cwd)
            .stdin(Stdio::null())
            .kill_on_drop(true)
            .output()
            .await?;
        let parsed = match self.options.agent {
            Agent::Claude => claude_outcome(&output.stdout),
            Agent::Codex => codex_outcome(&output.stdout, output.status.success()),
        };
        if let Some(outcome) = parsed {
            return Ok(outcome);
        }
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
        let code = output
            .status
            .code()
            .map_or_else(|| "null".to_owned(), |code| code.to_string());
        Ok(Outcome::failed(if stderr.is_empty() {
            format!("{} exited with code {code}", self.options.agent.name())
        } else {
            stderr
        }))
    }

    async fn work(
        &self,
        task: &Task,
        session: Option<AgentSession>,
        prompt: &str,
        resuming: bool,
    ) -> anyhow::Result<()> {
        let agent = self.options.agent;
        let mut claim = json!({ "status": "in_progress", "version": task.version });
        if let Some(session) = &session {
            claim["agentSession"] = json!(session);
        }
        if let Err(error) = self
            .api::<Value>(
                Method::POST,
                &format!("/api/tasks/{}/move", task.id),
                Some(claim),
            )
            .await
        {
            if error
                .downcast_ref::<ApiRequestError>()
                .is_some_and(|error| error.status == StatusCode::CONFLICT)
            {
                log(
                    agent,
                    &format!(
                        "{} was changed or claimed elsewhere; skipping",
                        task.identifier
                    ),
                );
                return Ok(());
            }
            return Err(error);
        }
        log(
            agent,
            &format!(
                "{} claimed; running {} in {}",
                task.identifier,
                agent.name(),
                self.options.cwd.display()
            ),
        );

        let outcome = self
            .run_agent(prompt, session.as_ref(), resuming)
            .await
            .unwrap_or_else(|error| {
                Outcome::failed(format!(
                    "The runner could not start {}: {error}",
                    agent.name()
                ))
            });
        let session = session.or_else(|| {
            outcome.session_id.map(|session_id| AgentSession {
                platform: agent.name().to_owned(),
                session_id,
            })
        });

        let mut comment = json!({ "body": outcome.text });
        if let Some(session) = &session {
            comment["agentSession"] = json!(session);
        }
        self.api::<Value>(
            Method::POST,
            &format!("/api/tasks/{}/comments", task.id),
            Some(comment),
        )
        .await?;
        let TaskResponse { task: current } = self
            .api(Method::GET, &format!("/api/tasks/{}", task.id), None)
            .await?;
        let status = if outcome.ok { "in_review" } else { "blocked" };
        let mut finish = json!({ "status": status, "version": current.version });
        if let Some(session) = &session {
            finish["agentSession"] = json!(session);
        }
        self.api::<Value>(
            Method::POST,
            &format!("/api/tasks/{}/move", task.id),
            Some(finish),
        )
        .await?;
        log(agent, &format!("{} -> {status}", task.identifier));
        Ok(())
    }

    async fn heartbeat(&self) -> anyhow::Result<Identity> {
        let Heartbeat { runner } = self
            .api(
                Method::PUT,
                &format!("/api/runners/{}", self.options.agent.name()),
                Some(json!({})),
            )
            .await?;
        Ok(runner)
    }

    /// Returns true when it handled an issue. The agent works on an issue when it is delegated
    /// (`todo` and assigned to this runner's agent) or when a comment since that agent's last
    /// reply mentions it, written by the runner's owner when it has one. It resumes the session
    /// of the agent's last reply, or starts a new one.
    async fn work_on_next_issue(&self, identity: &Identity) -> anyhow::Result<bool> {
        let Tasks { tasks } = self.api(Method::GET, "/api/tasks", None).await?;
        for task in tasks {
            if !SUMMONABLE_STATUSES.contains(&task.status.as_str()) {
                continue;
            }
            let Comments { comments } = self
                .api(
                    Method::GET,
                    &format!("/api/tasks/{}/comments", task.id),
                    None,
                )
                .await?;
            let last_reply = comments
                .iter()
                .rposition(|comment| comment.author_id == identity.agent_id);
            let pending = &comments[last_reply.map_or(0, |last| last + 1)..];
            let delegated = task.status == "todo" && task.assignee.id == identity.agent_id;
            let summoned = pending.iter().any(|comment| {
                self.mention.is_match(&comment.body)
                    && identity
                        .owner_id
                        .as_ref()
                        .is_none_or(|owner| comment.author_id == *owner)
            });
            if !delegated && !summoned {
                continue;
            }

            let agent = self.options.agent;
            let existing = last_reply
                .and_then(|last| comments[last].agent_session.clone())
                .filter(|session| session.platform == agent.name());
            let resuming = existing.is_some();
            // Claude takes a new session id up front; Codex reports the one it starts.
            let session = existing.or_else(|| match agent {
                Agent::Claude => Some(AgentSession {
                    platform: agent.name().to_owned(),
                    session_id: uuid::Uuid::new_v4().to_string(),
                }),
                Agent::Codex => None,
            });
            let prompt = build_prompt(&task, pending, resuming);
            self.work(&task, session, &prompt, resuming).await?;
            return Ok(true);
        }
        Ok(false)
    }

    async fn keep_alive(&self) -> Infallible {
        loop {
            tokio::time::sleep(HEARTBEAT_INTERVAL).await;
            if let Err(error) = self.heartbeat().await {
                log(self.options.agent, &format!("heartbeat error: {error}"));
            }
        }
    }

    async fn poll(&self, identity: &Identity) -> Infallible {
        loop {
            match self.work_on_next_issue(identity).await {
                Ok(true) => continue,
                Ok(false) => {}
                Err(error) => log(self.options.agent, &format!("error: {error}")),
            }
            tokio::time::sleep(self.options.interval).await;
        }
    }
}

/// `claude -p --output-format json` prints one result object.
fn claude_outcome(stdout: &[u8]) -> Option<Outcome> {
    let result = serde_json::from_slice::<Value>(stdout).ok()?;
    if result.is_null() {
        return None;
    }
    let text = match result.get("result") {
        Some(Value::String(text)) => text.clone(),
        None | Some(Value::Null) => String::new(),
        Some(other) => other.to_string(),
    };
    let failed = result
        .get("is_error")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    Some(Outcome {
        ok: !failed,
        text,
        session_id: None,
    })
}

/// `codex exec --json` prints JSONL events: the session in `thread.started`, the answer in the
/// last `agent_message` item, and failures as `turn.failed` or `error`.
fn codex_outcome(stdout: &[u8], exited_ok: bool) -> Option<Outcome> {
    let mut session_id = None;
    let mut message = None;
    let mut error = None;
    for event in String::from_utf8_lossy(stdout)
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
    {
        match event.get("type").and_then(Value::as_str) {
            Some("thread.started") => {
                session_id = event
                    .get("thread_id")
                    .and_then(Value::as_str)
                    .map(str::to_owned);
            }
            Some("item.completed")
                if event.pointer("/item/type").and_then(Value::as_str) == Some("agent_message") =>
            {
                message = event
                    .pointer("/item/text")
                    .and_then(Value::as_str)
                    .map(str::to_owned);
            }
            Some("turn.failed") => {
                error = event
                    .pointer("/error/message")
                    .and_then(Value::as_str)
                    .map(str::to_owned);
            }
            Some("error") => {
                error = event
                    .get("message")
                    .and_then(Value::as_str)
                    .map(str::to_owned);
            }
            _ => {}
        }
    }
    if session_id.is_none() && message.is_none() && error.is_none() {
        return None;
    }
    let ok = exited_ok && error.is_none() && message.is_some();
    Some(Outcome {
        ok,
        text: error.or(message).unwrap_or_default(),
        session_id,
    })
}

fn build_prompt(task: &Task, comments: &[Comment], resuming: bool) -> String {
    let heading = if resuming {
        format!("New comments on Broadcast issue {}:", task.identifier)
    } else {
        format!(
            "You are working on Broadcast issue {}: {}\n\n{}",
            task.identifier, task.title, task.description
        )
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

/// Polls the board until the future is dropped; dropping it also kills a running agent.
pub async fn run(mut options: Options) -> anyhow::Result<()> {
    // API paths join under the base, so a base path must end with a slash to be kept.
    if !options.url.path().ends_with('/') {
        let path = format!("{}/", options.url.path());
        options.url.set_path(&path);
    }
    let http = reqwest::Client::builder().build().context("HTTP client")?;
    let runner = Runner {
        executable: executable(options.agent),
        mention: Regex::new(&format!(r"(?i)(^|\s)@{}(?-u:\b)", options.agent.name()))
            .context("mention pattern")?,
        http,
        options,
    };
    let identity = loop {
        match runner.heartbeat().await {
            Ok(identity) => break identity,
            Err(error) => log(runner.options.agent, &format!("heartbeat error: {error}")),
        }
        tokio::time::sleep(runner.options.interval).await;
    };
    let agent = runner.options.agent;
    log(
        agent,
        &format!(
            "watching {} as {} for issues delegated to or mentioning @{}",
            runner.options.url,
            identity.agent_id,
            agent.name()
        ),
    );
    tokio::select! {
        never = runner.keep_alive() => match never {},
        never = runner.poll(&identity) => match never {},
    }
}
