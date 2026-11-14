//! The Codex host runtime the injected page reports, and the thread binding it implies.

use std::sync::Mutex;

use broadcast_domain::fields::{self, nullable_string, required_string};
use broadcast_domain::input::Attribution;
use broadcast_domain::records::{ThreadBinding, ThreadIdentity};
use broadcast_domain::{ApiError, Result};
use serde::Serialize;
use serde_json::Value;

/// `GET /api/local/host-runtime` hides reports older than this.
const TTL_MS: i64 = 3000;

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TodoProgress {
    pub completed: i64,
    pub total: i64,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HostRuntime {
    pub thread_id: String,
    pub thread_running: bool,
    pub thread_todo_progress: Option<TodoProgress>,
    pub codex_project_id: Option<String>,
    pub codex_project_kind: Option<&'static str>,
    pub codex_host_id: Option<String>,
    pub workspace_path: Option<String>,
    pub updated_at: i64,
}

#[derive(Default)]
pub struct Host {
    runtime: Mutex<Option<HostRuntime>>,
}

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

impl Host {
    /// The last report, if it is fresh.
    pub fn current(&self) -> Option<HostRuntime> {
        let runtime = self.runtime.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        runtime.clone().filter(|runtime| now_ms() - runtime.updated_at <= TTL_MS)
    }

    pub fn report(&self, body: &Value) -> Result<HostRuntime> {
        let runtime = parse(body)?;
        *self.runtime.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = Some(runtime.clone());
        Ok(runtime)
    }

    /// `currentHostThreadBinding`: the full identity when the host is on `thread_id`.
    pub fn thread_binding(&self, thread_id: Option<&str>) -> Option<ThreadBinding> {
        let runtime = self.runtime.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let runtime = runtime.as_ref()?;
        if Some(runtime.thread_id.as_str()) != thread_id {
            return None;
        }
        let filled = |value: &Option<String>| value.clone().filter(|value| !value.is_empty());
        Some(ThreadBinding {
            thread_id: runtime.thread_id.clone(),
            identity: Some(ThreadIdentity {
                codex_project_id: filled(&runtime.codex_project_id)?,
                codex_project_kind: runtime.codex_project_kind?.to_owned(),
                codex_host_id: filled(&runtime.codex_host_id)?,
                workspace_path: filled(&runtime.workspace_path)?,
            }),
        })
    }

    /// `resolveInputThreadBinding`: an unbound mutation inherits the host's thread identity.
    pub fn resolve(&self, mut attribution: Attribution) -> Attribution {
        if attribution.thread_binding.is_some() || matches!(attribution.agent_session, Some(Some(_))) {
            return attribution;
        }
        if let Some(binding) = self.thread_binding(attribution.thread_id.as_deref()) {
            attribution.thread_binding = Some(Some(binding));
        }
        attribution
    }
}

fn parse(body: &Value) -> Result<HostRuntime> {
    let body = fields::object(
        body,
        &[
            "threadId",
            "threadRunning",
            "threadTodoProgress",
            "codexProjectId",
            "codexProjectKind",
            "codexHostId",
            "workspacePath",
        ],
    )?;
    let thread_id = required_string(body.get("threadId"), "threadId", 256)?;
    let Some(thread_running) = body.get("threadRunning").and_then(Value::as_bool) else {
        return Err(ApiError::invalid_field("'threadRunning' must be a boolean").into());
    };
    let thread_todo_progress = match fields::nullish(body.get("threadTodoProgress")) {
        None => None,
        Some(progress) => {
            let progress = fields::object(progress, &["completed", "total"])?;
            let integer = |key: &str| progress.get(key).and_then(integer);
            match (integer("completed"), integer("total")) {
                (Some(completed), Some(total)) if completed >= 0 && total >= 1 => {
                    Some(TodoProgress { completed: completed.min(total), total })
                }
                _ => return Err(ApiError::invalid_field("'threadTodoProgress' is invalid").into()),
            }
        }
    };
    let nullable = |key: &str, max: usize| -> Result<Option<String>> {
        Ok(nullable_string(Some(body.get(key).unwrap_or(&Value::Null)), key, max)?.flatten())
    };
    Ok(HostRuntime {
        thread_id,
        thread_running,
        thread_todo_progress,
        codex_project_id: nullable("codexProjectId", 256)?,
        codex_project_kind: match body.get("codexProjectKind").and_then(Value::as_str) {
            Some("local") => Some("local"),
            Some("remote") => Some("remote"),
            _ => None,
        },
        codex_host_id: nullable("codexHostId", 256)?,
        workspace_path: nullable("workspacePath", 4096)?,
        updated_at: now_ms(),
    })
}

/// `Number.isInteger`: integral JSON numbers, including `1.0`.
fn integer(value: &Value) -> Option<i64> {
    let number = value.as_f64()?;
    (number.fract() == 0.0 && number.abs() < 9.3e18).then_some(number as i64)
}
