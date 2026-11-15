//! Claude runner. It reports itself online with a heartbeat, which also tells it the Claude
//! agent it acts as (on a cloud board, its signed-in user's own Claude Agent). It picks up `todo`
//! issues assigned to that agent and issues with a new comment that mentions @claude (on a cloud
//! board, only its owner's comments), claims one by moving it to `in_progress` (the version check
//! makes the claim exclusive), runs `claude -p` in its working directory, posts the result as a
//! comment, and moves the issue to `in_review` (or `blocked` when Claude fails).

use std::convert::Infallible;
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

/// The board shows the runner offline once heartbeats stop for longer than its presence window.
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(15);
/// Issues already in progress, done or canceled are not picked up.
const SUMMONABLE_STATUSES: [&str; 4] = ["backlog", "todo", "in_review", "blocked"];
static MENTION: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)(^|\s)@claude(?-u:\b)").expect("mention pattern"));

pub struct Options {
    /// The board's base URL, including the launcher's token path when it has one.
    pub url: Url,
    pub cwd: PathBuf,
    pub permission_mode: String,
    pub interval: Duration,
}

/// The `claude` on `PATH`, then the native installer's `~/.local/bin`, which an app started
/// outside a login shell does not have on `PATH`.
pub fn executable() -> PathBuf {
    let name = if cfg!(windows) {
        "claude.exe"
    } else {
        "claude"
    };
    std::env::var_os("PATH")
        .into_iter()
        .flat_map(|paths| std::env::split_paths(&paths).collect::<Vec<_>>())
        .chain(std::env::home_dir().map(|home| home.join(".local").join("bin")))
        .map(|directory| directory.join(name))
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
}

fn log(message: &str) {
    let at = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%S%.3fZ");
    println!("[claude-runner] {at} {message}");
}

struct Runner {
    options: Options,
    claude: PathBuf,
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
        let output = tokio::process::Command::new(&self.claude)
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
            let failed = result
                .get("is_error")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            return Ok(Outcome { ok: !failed, text });
        }
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
        let code = output
            .status
            .code()
            .map_or_else(|| "null".to_owned(), |code| code.to_string());
        Ok(Outcome {
            ok: false,
            text: if stderr.is_empty() {
                format!("claude exited with code {code}")
            } else {
                stderr
            },
        })
    }

    async fn work(
        &self,
        task: &Task,
        agent_session: &AgentSession,
        prompt: &str,
        session_args: [&str; 2],
    ) -> anyhow::Result<()> {
        let claim = json!({ "status": "in_progress", "version": task.version, "agentSession": agent_session });
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
                log(&format!(
                    "{} was changed or claimed elsewhere; skipping",
                    task.identifier
                ));
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

        let outcome = self
            .run_claude(prompt, session_args)
            .await
            .unwrap_or_else(|error| Outcome {
                ok: false,
                text: format!("Claude runner could not start claude: {error}"),
            });

        let comment = json!({ "body": outcome.text, "agentSession": agent_session });
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
        let finish =
            json!({ "status": status, "version": current.version, "agentSession": agent_session });
        self.api::<Value>(
            Method::POST,
            &format!("/api/tasks/{}/move", task.id),
            Some(finish),
        )
        .await?;
        log(&format!("{} -> {status}", task.identifier));
        Ok(())
    }

    async fn heartbeat(&self) -> anyhow::Result<Identity> {
        let Heartbeat { runner } = self
            .api(Method::PUT, "/api/runners/claude", Some(json!({})))
            .await?;
        Ok(runner)
    }

    /// Returns true when it handled an issue. Claude works on an issue when it is delegated
    /// (`todo` and assigned to this runner's agent) or when a comment since that agent's last
    /// reply mentions @claude, written by the runner's owner when it has one. It resumes the
    /// session of the agent's last reply, or starts a new one.
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
                MENTION.is_match(&comment.body)
                    && identity
                        .owner_id
                        .as_ref()
                        .is_none_or(|owner| comment.author_id == *owner)
            });
            if !delegated && !summoned {
                continue;
            }

            let existing = last_reply
                .and_then(|last| comments[last].agent_session.clone())
                .filter(|session| session.platform == "claude");
            let resuming = existing.is_some();
            let agent_session = existing.unwrap_or_else(|| AgentSession {
                platform: "claude".to_owned(),
                session_id: uuid::Uuid::new_v4().to_string(),
            });
            let prompt = build_prompt(&task, pending, resuming);
            let flag = if resuming { "--resume" } else { "--session-id" };
            self.work(
                &task,
                &agent_session,
                &prompt,
                [flag, &agent_session.session_id],
            )
            .await?;
            return Ok(true);
        }
        Ok(false)
    }

    async fn keep_alive(&self) -> Infallible {
        loop {
            tokio::time::sleep(HEARTBEAT_INTERVAL).await;
            if let Err(error) = self.heartbeat().await {
                log(&format!("heartbeat error: {error}"));
            }
        }
    }

    async fn poll(&self, identity: &Identity) -> Infallible {
        loop {
            match self.work_on_next_issue(identity).await {
                Ok(true) => continue,
                Ok(false) => {}
                Err(error) => log(&format!("error: {error}")),
            }
            tokio::time::sleep(self.options.interval).await;
        }
    }
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

/// Polls the board until the future is dropped; dropping it also kills a running `claude`.
pub async fn run(mut options: Options) -> anyhow::Result<()> {
    // API paths join under the base, so a token path must end with a slash to be kept.
    if !options.url.path().ends_with('/') {
        let path = format!("{}/", options.url.path());
        options.url.set_path(&path);
    }
    let http = reqwest::Client::builder().build().context("HTTP client")?;
    let runner = Runner {
        options,
        claude: executable(),
        http,
    };
    let identity = loop {
        match runner.heartbeat().await {
            Ok(identity) => break identity,
            Err(error) => log(&format!("heartbeat error: {error}")),
        }
        tokio::time::sleep(runner.options.interval).await;
    };
    log(&format!(
        "watching {} as {} for issues delegated to or mentioning @claude",
        runner.options.url, identity.agent_id
    ));
    tokio::select! {
        never = runner.keep_alive() => match never {},
        never = runner.poll(&identity) => match never {},
    }
}
