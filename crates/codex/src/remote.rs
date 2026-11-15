//! Automation work this process does itself: the ephemeral "may this todo start?" turn and the
//! SSH remote run, both through a Codex window's App Server bridge, with task reads and writes
//! against the Broadcast service.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Duration;

use anyhow::{Result, anyhow, bail};
use reqwest::Method;
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use tokio::sync::mpsc;
use tokio::time::Instant;

use crate::automation::{Request, display, js_space, truthy};
use crate::cdp::Session;
use crate::host::{self, Context, encode_component, lock};

/// `remoteAutomationTurnTimeoutMs`.
const TURN_TIMEOUT: Duration = Duration::from_secs(30 * 60);

/// `remoteAutomationTurnWaiters`: one waiter per host and thread for `turn/completed`.
#[derive(Default)]
pub struct Turns(Mutex<HashMap<String, mpsc::UnboundedSender<Value>>>);

impl Turns {
    /// `handleRemoteAutomationTurnNotification`.
    pub fn deliver(&self, host_id: &str, method: &str, params: &Value) {
        if method != "turn/completed"
            || !params["turn"]["id"]
                .as_str()
                .is_some_and(|id| !id.is_empty())
        {
            return;
        }
        let key = format!("{host_id}\0{}", display(params.get("threadId")));
        if let Some(waiter) = lock(&self.0).get(&key) {
            let _ = waiter.send(params["turn"].clone());
        }
    }

    /// `waitForRemoteAutomationTurn`: turns that complete before their id is known are kept.
    fn waiter(&self, host_id: &str, thread_id: &str) -> Waiter<'_> {
        let key = format!("{host_id}\0{thread_id}");
        let (sender, receiver) = mpsc::unbounded_channel();
        lock(&self.0).insert(key.clone(), sender.clone());
        Waiter {
            turns: self,
            key,
            sender,
            receiver,
        }
    }
}

struct Waiter<'a> {
    turns: &'a Turns,
    key: String,
    sender: mpsc::UnboundedSender<Value>,
    receiver: mpsc::UnboundedReceiver<Value>,
}

impl Waiter<'_> {
    async fn wait(
        mut self,
        turn_id: &str,
        timeout_message: &str,
        timeout: Duration,
    ) -> Result<Value> {
        let completed = async {
            while let Some(turn) = self.receiver.recv().await {
                if turn["id"] == turn_id {
                    return Some(turn);
                }
            }
            None
        };
        match tokio::time::timeout(timeout, completed).await {
            Ok(Some(turn)) => Ok(turn),
            _ => bail!(timeout_message.to_owned()),
        }
    }
}

impl Drop for Waiter<'_> {
    fn drop(&mut self) {
        let mut waiters = lock(&self.turns.0);
        if waiters
            .get(&self.key)
            .is_some_and(|sender| sender.same_channel(&self.sender))
        {
            waiters.remove(&self.key);
        }
    }
}

/// `broadcastRequest`: a JSON request to this instance's Broadcast service.
pub async fn broadcast_request(
    context: &Context,
    method: Method,
    path: &str,
    body: Option<Value>,
) -> Result<Value> {
    let mut request = context
        .http
        .request(method.clone(), format!("{}{path}", context.base_url))
        .header("accept", "application/json")
        .header("x-broadcast-client", "broadcast");
    if let Some(body) = body {
        request = request
            .header("content-type", "application/json")
            .body(body.to_string());
    }
    let response = request.send().await?;
    let status = response.status();
    let text = response.text().await?;
    let payload = if text.is_empty() {
        json!({})
    } else {
        serde_json::from_str(&text)
            .map_err(|_| anyhow!("Broadcast returned invalid JSON for {method} {path}"))?
    };
    if !status.is_success() {
        let message = payload["error"]["message"]
            .as_str()
            .filter(|message| !message.is_empty())
            .map_or_else(
                || {
                    format!(
                        "Broadcast returned HTTP {} for {method} {path}",
                        status.as_u16()
                    )
                },
                str::to_owned,
            );
        bail!(message);
    }
    Ok(payload)
}

pub async fn get(context: &Context, path: &str) -> Result<Value> {
    broadcast_request(context, Method::GET, path, None).await
}

async fn post(context: &Context, path: &str, body: Value) -> Result<Value> {
    broadcast_request(context, Method::POST, path, Some(body)).await
}

pub fn todo_path(request: &Request) -> String {
    format!(
        "/api/tasks?projectId={}&status=todo",
        encode_component(&request.broadcast_project_id)
    )
}

pub fn list(value: &Value, field: &str) -> Vec<Value> {
    value[field].as_array().cloned().unwrap_or_default()
}

/// The fields of `value` that are present, as `JSON.stringify` keeps them.
fn pick(value: &Value, fields: &[&str]) -> Value {
    Value::Object(
        fields
            .iter()
            .filter_map(|field| {
                value
                    .get(*field)
                    .map(|found| ((*field).to_owned(), found.clone()))
            })
            .collect(),
    )
}

fn dependencies_done(task: &Value) -> bool {
    task["relations"]["blockedBy"]
        .as_array()
        .into_iter()
        .flatten()
        .all(|dependency| dependency["status"] == "done")
}

/// `localAutomationTodoInputs`: the todo candidates with their comments, and a digest of what a
/// start decision depends on.
pub async fn todo_inputs(
    context: &Context,
    request: &Request,
    tasks: &[Value],
) -> Result<(Vec<(Value, Vec<Value>)>, String)> {
    let candidates = tasks.iter().filter(|task| {
        task["projectId"] == request.broadcast_project_id.as_str()
            && task["status"] == "todo"
            && task.get("archivedAt") == Some(&Value::Null)
            && dependencies_done(task)
    });
    let candidates = futures_util::future::try_join_all(candidates.map(|task| async move {
        let comments = get(
            context,
            &format!(
                "/api/tasks/{}/comments",
                encode_component(&display(task.get("id")))
            ),
        )
        .await?;
        anyhow::Ok((task.clone(), list(&comments, "comments")))
    }))
    .await?;
    let field = |task: &Value, name: &str| task.get(name).cloned().unwrap_or(Value::Null);
    let digest: Vec<Value> = candidates
        .iter()
        .map(|(task, comments)| {
            json!([
                field(task, "id"),
                field(task, "version"),
                field(task, "title"),
                field(task, "description"),
                field(task, "threadId"),
                field(task, "threadBinding"),
                task.get("relations")
                    .and_then(|relations| relations.get("blockedBy"))
                    .cloned()
                    .unwrap_or(Value::Null),
                comments.last().cloned().unwrap_or(Value::Null),
            ])
        })
        .collect();
    let snapshot = Sha256::digest(Value::Array(digest).to_string().as_bytes())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    Ok((candidates, snapshot))
}

/// `remoteAutomationTurnText`: the turn's last agent message.
fn turn_text(turn: &Value) -> String {
    turn["items"]
        .as_array()
        .into_iter()
        .flatten()
        .rev()
        .find(|item| item["type"] == "agentMessage")
        .and_then(|item| item["text"].as_str())
        .map(|text| text.trim_matches(js_space).to_owned())
        .unwrap_or_default()
}

fn turn_error(turn: &Value) -> Option<String> {
    turn["error"]["message"]
        .as_str()
        .filter(|message| !message.is_empty())
        .map(str::to_owned)
}

/// `remoteAutomationCanStart`: an ephemeral read-only turn decides whether a todo's description
/// and latest comment allow it to start.
pub async fn can_start(
    context: &Context,
    session: &Session,
    request: &Request,
    task: &Value,
    comments: &[Value],
) -> Result<bool> {
    let host_id = &request.codex_host_id;
    let started = host::app_server_request(
        session,
        host_id,
        "thread/start",
        json!({
            "ephemeral": true,
            "model": request.model,
            "cwd": request.workspace_path,
            "runtimeWorkspaceRoots": [request.workspace_path],
            "approvalPolicy": "never",
            "sandbox": "read-only",
        }),
        host::APP_SERVER_TIMEOUT,
    )
    .await?;
    let thread_id = started["thread"]["id"]
        .as_str()
        .filter(|id| !id.is_empty() && started["thread"]["ephemeral"] == true)
        .ok_or_else(|| anyhow!("Codex 未创建临时自动认领判断线程"))?
        .to_owned();

    let deadline = Instant::now() + TURN_TIMEOUT;
    let completion = context.turns.waiter(host_id, &thread_id);
    let latest = comments.last().map_or(Value::Null, |comment| {
        pick(comment, &["authorName", "createdAt", "body"])
    });
    let mut question = pick(task, &["identifier", "title", "description"]);
    question["latestComment"] = latest;
    let text = [
        "你是 Broadcast 自动认领 Agent。只判断下面的议题当前是否允许开始。".to_owned(),
        "根据完整描述和最新评论做语义判断：若任一处明确要求等待、暂不执行或当前不应开始，decision 为 wait；否则 decision 为 start。不要调用工具，不要解释。".to_owned(),
        question.to_string(),
    ]
    .join("\n\n");
    let turn_started = host::app_server_request(
        session,
        host_id,
        "turn/start",
        json!({
            "threadId": thread_id,
            "input": [{ "type": "text", "text": text }],
            "effort": request.reasoning_effort,
            "outputSchema": {
                "type": "object",
                "properties": { "decision": { "type": "string", "enum": ["start", "wait"] } },
                "required": ["decision"],
                "additionalProperties": false,
            },
        }),
        host::APP_SERVER_TIMEOUT,
    )
    .await?;
    let turn_id = turn_started["turn"]["id"]
        .as_str()
        .filter(|id| !id.is_empty())
        .ok_or_else(|| anyhow!("Codex 未返回自动认领判断 turn"))?;
    let turn = completion
        .wait(
            turn_id,
            "Codex 自动认领判断超时",
            deadline.saturating_duration_since(Instant::now()),
        )
        .await?;
    if turn["status"] != "completed" {
        bail!(turn_error(&turn).unwrap_or_else(|| "Codex 自动认领判断失败".to_owned()));
    }
    let decision = serde_json::from_str::<Value>(&turn_text(&turn))
        .ok()
        .map(|answer| answer["decision"].clone());
    match decision.as_ref().and_then(Value::as_str) {
        Some("start") => Ok(true),
        Some("wait") => Ok(false),
        _ => bail!("Codex 未返回有效的自动认领判断"),
    }
}

/// `normalizeRemoteWorkspace`.
fn normalize_workspace(value: Option<&Value>) -> String {
    let text = if truthy(value) {
        display(value)
    } else {
        String::new()
    };
    let path = text.trim_matches(js_space).replace('\\', "/");
    let path = path.trim_end_matches('/');
    let mut characters = path.chars();
    match (characters.next(), characters.next()) {
        (Some(drive), Some(':')) if drive.is_ascii_alphabetic() => {
            format!("{}{}", drive.to_ascii_lowercase(), &path[1..])
        }
        _ => path.to_owned(),
    }
}

fn worktree(task: &Value) -> bool {
    task["developmentContext"]["type"] == "worktree"
}

/// `remoteAutomationTarget`: the one remote project mapping for the task's workspace.
fn mapped_target(request: &Request, task: &Value) -> Option<Value> {
    let root = Value::String(request.workspace_path.clone());
    let workspace = normalize_workspace(if worktree(task) {
        task["developmentContext"].get("path")
    } else {
        Some(&root)
    });
    let matches: Vec<&Value> = request
        .remote_projects
        .iter()
        .filter(|project| {
            project["codexProjectKind"] == "remote"
                && project["codexHostId"] == request.codex_host_id.as_str()
                && normalize_workspace(project.get("workspacePath")) == workspace
                && (worktree(task)
                    || project["codexProjectId"] == request.codex_project_id.as_str())
        })
        .collect();
    match matches.as_slice() {
        [only] => Some((*only).clone()),
        _ => None,
    }
}

/// `eligibleRemoteAutomationTask`.
fn eligible(task: &Value) -> bool {
    let binding = &task["threadBinding"];
    let remote_binding =
        binding["codexProjectKind"] == "remote" && task.get("threadId") == binding.get("threadId");
    task["status"] == "todo"
        && task.get("archivedAt") == Some(&Value::Null)
        && ((!truthy(task.get("threadId")) && !truthy(task.get("threadBinding"))) || remote_binding)
        && dependencies_done(task)
}

fn attachment_tuple(attachment: &Value) -> Value {
    let field = |name: &str| attachment.get(name).cloned().unwrap_or(Value::Null);
    json!([
        field("id"),
        field("filename"),
        field("contentType"),
        field("size")
    ])
}

/// `remoteAutomationSnapshot`: what must not change between choosing a task and claiming it.
fn task_snapshot(task: &Value, comments: &[Value], attachments: &[Value]) -> String {
    let field = |value: &Value, name: &str| value.get(name).cloned().unwrap_or(Value::Null);
    let mut summary = pick(
        task,
        &[
            "version",
            "projectId",
            "title",
            "description",
            "developmentContext",
        ],
    );
    summary["blockedBy"] = task["relations"]["blockedBy"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|dependency| json!([field(dependency, "id"), field(dependency, "status")]))
        .collect();
    let comments: Vec<Value> = comments
        .iter()
        .map(|comment| {
            let attachments: Vec<Value> = comment["attachments"]
                .as_array()
                .into_iter()
                .flatten()
                .map(attachment_tuple)
                .collect();
            json!([
                field(comment, "id"),
                field(comment, "version"),
                field(comment, "body"),
                attachments
            ])
        })
        .collect();
    let attachments: Vec<Value> = attachments.iter().map(attachment_tuple).collect();
    json!({ "task": summary, "comments": comments, "attachments": attachments }).to_string()
}

/// `remoteAutomationPrompt`: the remote worker's instructions; it never runs `broadcast`.
fn prompt(task: &Value, comments: &[Value], attachments: &[Value], target: &Value) -> String {
    let comment_text = if comments.is_empty() {
        "（无）".to_owned()
    } else {
        comments
            .iter()
            .map(|comment| {
                format!(
                    "- {} ({}):\n{}",
                    display(comment.get("authorName")),
                    display(comment.get("createdAt")),
                    display(comment.get("body"))
                )
            })
            .collect::<Vec<_>>()
            .join("\n\n")
    };
    let items: Vec<&Value> = attachments
        .iter()
        .chain(
            comments
                .iter()
                .flat_map(|comment| comment["attachments"].as_array().into_iter().flatten()),
        )
        .collect();
    let attachment_text = if items.is_empty() {
        "（无）".to_owned()
    } else {
        items
            .iter()
            .map(|attachment| {
                format!(
                    "- {} ({}, {} bytes)",
                    display(attachment.get("filename")),
                    display(attachment.get("contentType")),
                    display(attachment.get("size")),
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
    };
    let development_context = if truthy(task.get("developmentContext")) {
        task["developmentContext"].to_string()
    } else {
        "（项目根目录）".to_owned()
    };
    let description = if truthy(task.get("description")) {
        display(task.get("description"))
    } else {
        "（无）".to_owned()
    };
    [
        format!("处理 Broadcast 议题 {}：{}", display(task.get("identifier")), display(task.get("title"))),
        String::new(),
        format!("远程工作目录：{}", display(target.get("workspacePath"))),
        format!("开发上下文：{development_context}"),
        String::new(),
        "完整描述：".to_owned(),
        description,
        String::new(),
        "全部评论：".to_owned(),
        comment_text,
        String::new(),
        "附件：".to_owned(),
        attachment_text,
        String::new(),
        "你只负责在当前远程项目和工作目录内完成实现与直接验证。不要运行 broadcast，也不要访问或修改 Broadcast。完成后返回改动、验证结果、执行结果和剩余限制。".to_owned(),
    ]
    .join("\n")
}

/// JavaScript's `.slice(0, max)` on a string, at a character boundary.
fn truncate(text: &str, max: usize) -> String {
    let mut length = 0;
    text.chars()
        .take_while(|character| {
            length += character.len_utf16();
            length <= max
        })
        .collect()
}

struct Selected {
    task_path: String,
    comments_path: String,
    task: Value,
    comments: Vec<Value>,
    attachments: Vec<Value>,
    session: Session,
}

async fn read_task(context: &Context, task_path: &str) -> Result<(Value, Vec<Value>, Vec<Value>)> {
    let comments_path = format!("{task_path}/comments");
    let attachments_path = format!("{task_path}/attachments");
    let (task, comments, attachments) = tokio::try_join!(
        get(context, task_path),
        get(context, &comments_path),
        get(context, &attachments_path)
    )?;
    Ok((
        task["task"].clone(),
        list(&comments, "comments"),
        list(&attachments, "attachments"),
    ))
}

/// `runRemoteBroadcastAutomation`: claims the first todo allowed to start, runs it in its SSH
/// workspace and writes the result back as a comment and a move to review or blocked.
pub async fn run(context: &Context, request: &Request, version: u64) -> Result<()> {
    if !request.enabled_by_user
        || !request.remote
        || context.policies.version(&request.broadcast_project_id) != Some(version)
    {
        return Ok(());
    }
    let listed = get(context, &todo_path(request)).await?;
    let mut selected = None;
    for listed_task in list(&listed, "tasks") {
        if !eligible(&listed_task) {
            continue;
        }
        let task_path = format!(
            "/api/tasks/{}",
            encode_component(&display(listed_task.get("id")))
        );
        let (task, comments, attachments) = read_task(context, &task_path).await?;
        if !eligible(&task) || task["projectId"] != request.broadcast_project_id.as_str() {
            return Ok(());
        }
        let session = context.routes.latest()?;
        if !can_start(context, &session, request, &task, &comments).await? {
            continue;
        }
        selected = Some(Selected {
            comments_path: format!("{task_path}/comments"),
            task_path,
            task,
            comments,
            attachments,
            session,
        });
        break;
    }
    let Some(Selected {
        task_path,
        comments_path,
        task,
        comments,
        attachments,
        session,
    }) = selected
    else {
        return Ok(());
    };
    let existing_binding = (task["threadBinding"]["codexProjectKind"] == "remote")
        .then(|| task["threadBinding"].clone());
    let Some(target) = existing_binding
        .clone()
        .or_else(|| mapped_target(request, &task))
    else {
        post(
            context,
            &comments_path,
            json!({ "body": "自动认领未开始：目标 SSH 工作目录没有唯一的已登记 Codex 项目映射。" }),
        )
        .await?;
        return Ok(());
    };
    let snapshot = task_snapshot(&task, &comments, &attachments);
    let host_id = display(target.get("codexHostId"));
    let mut params = Map::new();
    if let Some(thread_id) = existing_binding
        .as_ref()
        .and_then(|binding| binding.get("threadId"))
    {
        params.insert("threadId".to_owned(), thread_id.clone());
    }
    params.extend(
        json!({
            "model": request.model,
            "cwd": target["workspacePath"],
            "runtimeWorkspaceRoots": [target["workspacePath"]],
            "approvalPolicy": "never",
            "sandbox": "danger-full-access",
        })
        .as_object()
        .cloned()
        .unwrap_or_default(),
    );
    let method = if existing_binding.is_some() {
        "thread/resume"
    } else {
        "thread/start"
    };
    let started = host::app_server_request(
        &session,
        &host_id,
        method,
        Value::Object(params),
        host::APP_SERVER_TIMEOUT,
    )
    .await?;
    let thread_id = started["thread"]["id"]
        .as_str()
        .filter(|id| {
            !id.is_empty()
                && normalize_workspace(started["thread"].get("cwd"))
                    == normalize_workspace(target.get("workspacePath"))
        })
        .ok_or_else(|| {
            anyhow!(
                "Codex did not {} the automation thread in the selected SSH workspace",
                if existing_binding.is_some() {
                    "resume"
                } else {
                    "create"
                }
            )
        })?
        .to_owned();

    let (refreshed_task, refreshed_comments, refreshed_attachments) =
        read_task(context, &task_path).await?;
    let refreshed_target = if refreshed_task["threadBinding"]["codexProjectKind"] == "remote" {
        Some(refreshed_task["threadBinding"].clone())
    } else {
        mapped_target(request, &refreshed_task)
    };
    let same_target = [
        "codexProjectId",
        "codexProjectKind",
        "codexHostId",
        "workspacePath",
        "threadId",
    ]
    .iter()
    .all(|field| {
        refreshed_target
            .as_ref()
            .and_then(|refreshed| refreshed.get(*field))
            == target.get(*field)
    });
    if !eligible(&refreshed_task)
        || task_snapshot(&refreshed_task, &refreshed_comments, &refreshed_attachments) != snapshot
        || !same_target
    {
        return Ok(());
    }

    let thread_binding = existing_binding.clone().unwrap_or_else(|| {
        json!({
            "threadId": thread_id,
            "codexProjectId": target["codexProjectId"],
            "codexProjectKind": "remote",
            "codexHostId": target["codexHostId"],
            "workspacePath": target["workspacePath"],
        })
    });
    let move_path = format!("{task_path}/move");
    let claimed = post(
        context,
        &move_path,
        json!({ "version": refreshed_task["version"], "status": "in_progress", "threadId": thread_id, "threadBinding": thread_binding }),
    )
    .await?;
    let owned_version = claimed["task"]["version"].clone();

    let completion = context.turns.waiter(&host_id, &thread_id);
    let outcome = async {
        let turn_started = host::app_server_request(
            &session,
            &host_id,
            "turn/start",
            json!({
                "threadId": thread_id,
                "input": [{ "type": "text", "text": prompt(&refreshed_task, &refreshed_comments, &refreshed_attachments, &target) }],
                "effort": request.reasoning_effort,
            }),
            host::APP_SERVER_TIMEOUT,
        )
        .await?;
        let turn_id = turn_started["turn"]["id"].as_str().filter(|id| !id.is_empty()).ok_or_else(|| anyhow!("Codex did not return the remote automation turn id"))?;
        let turn = completion.wait(turn_id, "Codex remote automation turn timed out", TURN_TIMEOUT).await?;
        if turn["status"] != "completed" {
            bail!(turn_error(&turn).unwrap_or_else(|| format!("Codex remote turn {}", display(turn.get("status")))));
        }
        let mut final_text = turn_text(&turn);
        if final_text.is_empty() {
            // Some completion notifications omit items; read the completed turn once.
            let read = host::app_server_request(&session, &host_id, "thread/read", json!({ "threadId": thread_id, "includeTurns": true }), host::APP_SERVER_TIMEOUT).await?;
            let saved = read["thread"]["turns"].as_array().into_iter().flatten().find(|candidate| candidate["id"] == turn_id && candidate["status"] == "completed");
            final_text = saved.map(turn_text).unwrap_or_default();
        }
        if final_text.is_empty() {
            bail!("Codex completed without a final result");
        }
        let body = [
            "自动认领远程执行完成。".to_owned(),
            format!("- Codex host：{host_id}"),
            format!("- 远程目录：{}", display(target.get("workspacePath"))),
            format!("- 远程 thread：{thread_id}"),
            String::new(),
            final_text,
        ]
        .join("\n");
        post(context, &comments_path, json!({ "body": truncate(&body, 100_000), "threadId": thread_id, "threadBinding": thread_binding })).await?;
        post(context, &move_path, json!({ "version": owned_version, "status": "in_review", "threadId": thread_id, "threadBinding": thread_binding })).await?;
        anyhow::Ok(())
    }
    .await;
    if let Err(error) = outcome {
        let body = truncate(&format!("自动认领远程执行失败：{error}"), 100_000);
        post(
            context,
            &comments_path,
            json!({ "body": body, "threadId": thread_id, "threadBinding": thread_binding }),
        )
        .await?;
        post(context, &move_path, json!({ "version": owned_version, "status": "blocked", "threadId": thread_id, "threadBinding": thread_binding })).await?;
    }
    Ok(())
}

/// `remoteAutomationItem`: the automation a remote project stands for; Codex has no cron for it.
pub fn item(request: &Request, status: &str, next_run_at: Option<Value>) -> Value {
    let mut item = json!({
        "id": request.automation_id.clone().unwrap_or_else(|| format!("broadcast-{}", request.broadcast_project_id)),
        "status": status,
        "model": request.model,
        "reasoningEffort": request.reasoning_effort,
        "rrule": request.rrule(),
    });
    if status != "ACTIVE" {
        item["nextRunAt"] = Value::Null;
    } else if let Some(next_run_at) = next_run_at {
        item["nextRunAt"] = next_run_at;
    }
    item
}
