//! Daily project summaries written by Codex.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use broadcast_domain::input::{Archived, TaskFilters};
use broadcast_domain::records::{Project, ProjectSummary, Task};
use broadcast_domain::{ApiError, Result};
use chrono::{DateTime, Utc};
use serde_json::{Map, Value, json};
use tokio::sync::watch;
use tokio::task::JoinHandle;

use super::exec::{self, Line, TurnProcess};
use crate::App;

const DAY: chrono::TimeDelta = chrono::TimeDelta::days(1);
const CHECK_INTERVAL: Duration = Duration::from_secs(60 * 60);
const STATUS_LABELS: [(&str, &str); 7] = [
    ("backlog", "积压事项"),
    ("todo", "待办"),
    ("in_progress", "处理中"),
    ("in_review", "等你确认"),
    ("blocked", "遇到阻碍"),
    ("done", "完成"),
    ("canceled", "取消"),
];

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

fn label(status: &str) -> Option<&'static str> {
    STATUS_LABELS.iter().find(|(key, _)| *key == status).map(|(_, label)| *label)
}

fn since(timestamp: &str) -> Option<chrono::TimeDelta> {
    Some(Utc::now() - DateTime::parse_from_rfc3339(timestamp).ok()?.with_timezone(&Utc))
}

/// `isDue`: never attempted, or the last attempt is a day old.
fn is_due(summary: &ProjectSummary) -> bool {
    summary.attempted_at.as_deref().is_none_or(|attempted| since(attempted).is_some_and(|age| age >= DAY))
}

fn build_prompt(project: &Project, tasks: &[Task]) -> String {
    let counts: Map<String, Value> = STATUS_LABELS
        .iter()
        .map(|(status, label)| ((*label).to_owned(), json!(tasks.iter().filter(|task| task.status == *status).count())))
        .collect();
    let issues: Vec<Value> = tasks
        .iter()
        .filter(|task| {
            (task.status != "done" && task.status != "canceled")
                || since(&task.activity_updated_at).is_some_and(|age| age <= chrono::TimeDelta::days(7))
        })
        .map(|task| {
            let mut issue = Map::new();
            issue.insert("id".into(), json!(task.identifier));
            issue.insert("title".into(), json!(task.title));
            if let Some(label) = label(&task.status) {
                issue.insert("status".into(), json!(label));
            }
            issue.insert("priority".into(), json!(task.priority));
            issue.insert("labels".into(), json!(task.labels));
            issue.insert("dueDate".into(), json!(task.due_date));
            issue.insert("updatedAt".into(), json!(task.activity_updated_at));
            Value::Object(issue)
        })
        .collect();
    let snapshot = json!({
        "project": project.name,
        "generatedForDate": Utc::now().format("%Y-%m-%d").to_string(),
        "counts": counts,
        "issues": issues,
    });
    [
        "你是 Codex。请根据下面的任务面板快照，为项目负责人写一段项目总结。".to_owned(),
        "要求：只输出一段 60 至 120 字的简体中文；直接说明当前进展、主要风险或阻碍、下一步重点；不要使用标题、列表或 Markdown；不要调用工具。".to_owned(),
        snapshot.to_string(),
    ]
    .join("\n\n")
}

/// `String(event.error?.message ?? event.message ?? fallback)`.
fn event_error(event: &Value) -> String {
    let present = |value: Option<&Value>| value.filter(|value| !value.is_null()).cloned();
    match present(event.pointer("/error/message")).or_else(|| present(event.get("message"))) {
        Some(Value::String(text)) => text,
        Some(other) => other.to_string(),
        None => "Codex 生成失败".to_owned(),
    }
}

/// A refresh in flight: the turn owner once it runs, and whether the refresh finished.
struct Refresh {
    pid: Mutex<Option<u32>>,
    done: watch::Sender<bool>,
}

impl Refresh {
    async fn wait(&self) {
        let mut done = self.done.subscribe();
        let _ = done.wait_for(|done| *done).await;
    }
}

pub struct ProjectSummaries {
    executable: String,
    workspace: PathBuf,
    active: Mutex<HashMap<String, Arc<Refresh>>>,
    closed: AtomicBool,
    timer: Mutex<Option<JoinHandle<()>>>,
}

impl ProjectSummaries {
    /// Codex runs read-only in `workspace` (the server's directory).
    pub fn new(executable: String, workspace: PathBuf) -> Self {
        Self { executable, workspace, active: Mutex::default(), closed: AtomicBool::new(false), timer: Mutex::default() }
    }

    /// Refreshes due projects now and then every hour.
    pub fn start(app: &Arc<App>) {
        let timer_app = Arc::clone(app);
        let timer = tokio::spawn(async move {
            let mut ticker = tokio::time::interval(CHECK_INTERVAL);
            loop {
                ticker.tick().await;
                let _ = refresh_due(&timer_app).await;
            }
        });
        *lock(&app.summary.timer) = Some(timer);
    }

    /// `get`: the stored summary; a due one starts refreshing in the background.
    pub async fn get(&self, app: &Arc<App>, project_id: &str) -> Result<Value> {
        let id = project_id.to_owned();
        let summary = app
            .store(move |store| match store.get_project(&id)? {
                Some(_) => store.get_project_summary(&id).map(Some),
                None => Ok(None),
            })
            .await?
            .ok_or_else(|| ApiError::new(404, "PROJECT_NOT_FOUND", format!("Project '{project_id}' was not found")))?;
        if is_due(&summary) {
            refresh(app, project_id);
        }
        Ok(json!({
            "projectId": project_id,
            "summary": summary.summary,
            "updatedAt": summary.generated_at,
            "refreshing": lock(&self.active).contains_key(project_id),
            "error": summary.error,
        }))
    }

    /// `close`: stops the timer, terminates running refreshes and waits for them.
    pub async fn close(&self) {
        self.closed.store(true, Ordering::SeqCst);
        if let Some(timer) = lock(&self.timer).take() {
            timer.abort();
        }
        let active: Vec<Arc<Refresh>> = lock(&self.active).values().cloned().collect();
        for refresh in &active {
            if let Some(pid) = *lock(&refresh.pid) {
                exec::signal_group(pid, false);
            }
        }
        futures_util::future::join_all(active.iter().map(|refresh| refresh.wait())).await;
    }

    /// `#generate`: the summary text, or `None` once the project is gone.
    async fn generate(&self, app: &Arc<App>, project_id: &str, refresh: &Refresh) -> std::result::Result<Option<String>, String> {
        let id = project_id.to_owned();
        let loaded = app
            .store(move |store| {
                let Some(project) = store.get_project(&id)? else { return Ok(None) };
                let filters = TaskFilters { project_id: Some(id), status: None, archived: Archived::Exclude };
                Ok(Some((project, store.list_tasks(&filters)?)))
            })
            .await
            .map_err(|error| anyhow::Error::from(error).to_string())?;
        let Some((project, tasks)) = loaded else { return Ok(None) };
        let workspace = self.workspace.to_string_lossy().into_owned();
        let args: Vec<String> = [
            "exec",
            "--ephemeral",
            "--json",
            "--color",
            "never",
            "--skip-git-repo-check",
            "-C",
            &workspace,
            "-s",
            "read-only",
            "-c",
            "approval_policy=\"never\"",
            "-",
        ]
        .into_iter()
        .map(str::to_owned)
        .collect();
        let process = exec::spawn_turn(&self.executable, &args, &build_prompt(&project, &tasks)).await.map_err(|error| error.to_string())?;
        let TurnProcess { pid, mut child, control, mut stdout, stderr } = process;
        *lock(&refresh.pid) = pid;
        tokio::spawn(exec::drain(stderr));

        let mut summary = String::new();
        let mut terminal_error = String::new();
        let mut fatal = None;
        let mut line = Vec::new();
        loop {
            match exec::read_line(&mut stdout, &mut line, exec::MAX_LINE_BYTES).await {
                Ok(Line::Eof) => break,
                Ok(Line::TooLong) => {
                    fatal = Some(format!("Codex JSONL line exceeded {} bytes", exec::MAX_LINE_BYTES));
                    break;
                }
                Err(error) => {
                    fatal = Some(error.to_string());
                    break;
                }
                Ok(Line::Read) => {
                    let text = String::from_utf8_lossy(line.strip_suffix(b"\r").unwrap_or(&line));
                    if text.trim().is_empty() {
                        continue;
                    }
                    let Ok(event) = serde_json::from_str::<Value>(&text) else {
                        fatal = Some("Codex emitted malformed JSONL".to_owned());
                        break;
                    };
                    let kind = event.get("type").and_then(Value::as_str);
                    if kind == Some("item.completed")
                        && event.pointer("/item/type").and_then(Value::as_str) == Some("agent_message")
                        && let Some(text) = event.pointer("/item/text").and_then(Value::as_str)
                    {
                        summary = text.split_whitespace().collect::<Vec<_>>().join(" ");
                    }
                    if kind == Some("turn.failed") || kind == Some("error") {
                        terminal_error = event_error(&event);
                    }
                }
            }
        }
        if fatal.is_some()
            && let Some(pid) = pid
        {
            exec::signal_group(pid, true);
        }
        exec::drain(stdout).await;
        let status = child.wait().await;
        drop(control);
        if let Some(fatal) = fatal {
            return Err(fatal);
        }
        let status = status.map_err(|error| error.to_string())?;
        if status.code() != Some(0) || !terminal_error.is_empty() {
            if terminal_error.is_empty() {
                let code = status.code().map_or_else(|| "null".to_owned(), |code| code.to_string());
                return Err(format!("Codex 退出码 {code}"));
            }
            return Err(terminal_error);
        }
        if summary.is_empty() {
            return Err("Codex 没有返回项目总结".to_owned());
        }
        Ok(Some(summary))
    }
}

/// `refresh`: one generation per project at a time.
fn refresh(app: &Arc<App>, project_id: &str) -> Arc<Refresh> {
    let summaries = &app.summary;
    let mut active = lock(&summaries.active);
    if let Some(current) = active.get(project_id) {
        return Arc::clone(current);
    }
    let entry = Arc::new(Refresh { pid: Mutex::new(None), done: watch::channel(false).0 });
    active.insert(project_id.to_owned(), Arc::clone(&entry));
    let (app, id, running) = (Arc::clone(app), project_id.to_owned(), Arc::clone(&entry));
    tokio::spawn(async move {
        let summaries = &app.summary;
        let outcome = summaries.generate(&app, &id, &running).await;
        let key = id.clone();
        match outcome {
            Ok(Some(text)) => {
                let _ = app.store(move |store| store.save_project_summary(&key, &text)).await;
            }
            Err(message) if !summaries.closed.load(Ordering::SeqCst) => {
                let _ = app.store(move |store| store.save_project_summary_error(&key, &message)).await;
            }
            _ => {}
        }
        lock(&summaries.active).remove(&id);
        running.done.send_replace(true);
    });
    entry
}

/// `refreshDueProjects`.
async fn refresh_due(app: &Arc<App>) -> Result<()> {
    for summary in app.store(|store| store.list_project_summaries()).await? {
        if is_due(&summary) {
            refresh(app, &summary.project_id).wait().await;
        }
    }
    Ok(())
}
