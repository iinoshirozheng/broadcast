//! Codex quota: whether the signed-in account can run Codex right now, read from a short-lived
//! `codex app-server`.

use std::process::Stdio;
use std::time::Duration;

use anyhow::{Result, anyhow, bail};
use serde_json::{Map, Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines};
use tokio::process::{Child, ChildStdin, ChildStdout};

use crate::automation::{js_number, number, truthy};
use crate::host;

const REQUEST_TIMEOUT: Duration = Duration::from_secs(8);

/// `readCodexQuotaStatus`: `available`, `blocked` (with `resetsAt` when known), `unavailable`
/// for API-key accounts, or `unknown`.
pub async fn read(codex: &str, model: &str) -> Value {
    let checked_at = json!(host::chrono_millis());
    let Ok(mut server) = AppServer::start(codex) else {
        return json!({ "state": "unknown", "checkedAt": checked_at });
    };
    let status = server.status(model, &checked_at).await;
    server.close();
    status.unwrap_or_else(|_| json!({ "state": "unknown", "checkedAt": checked_at }))
}

struct AppServer {
    child: Child,
    stdin: Option<ChildStdin>,
    lines: Lines<BufReader<ChildStdout>>,
    sequence: i64,
}

impl AppServer {
    fn start(codex: &str) -> Result<Self> {
        let mut child = crate::command(codex)
            .args(["app-server", "--stdio"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()?;
        let stdin = child.stdin.take();
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| anyhow!("Codex App Server has no stdout"))?;
        Ok(Self {
            child,
            stdin,
            lines: BufReader::new(stdout).lines(),
            sequence: 0,
        })
    }

    async fn status(&mut self, model: &str, checked_at: &Value) -> Result<Value> {
        self.request(
            "initialize",
            json!({ "clientInfo": { "name": "broadcast", "version": "0.1.0" } }),
        )
        .await?;
        self.write(&json!({ "method": "initialized", "params": {} }))
            .await?;
        let account = self
            .request("account/read", json!({ "refreshToken": false }))
            .await?;
        let kind = &account["account"]["type"];
        if kind == "apiKey" {
            return Ok(
                json!({ "state": "unavailable", "reason": "api-key", "checkedAt": checked_at }),
            );
        }
        if kind != "chatgpt" {
            return Ok(json!({ "state": "unknown", "checkedAt": checked_at }));
        }
        let limits = self.request("account/rateLimits/read", json!({})).await?;
        Ok(evaluate(&limits, model, checked_at))
    }

    async fn write(&mut self, message: &Value) -> Result<()> {
        let stdin = self
            .stdin
            .as_mut()
            .ok_or_else(|| anyhow!("Codex App Server stopped"))?;
        stdin.write_all(format!("{message}\n").as_bytes()).await?;
        Ok(())
    }

    async fn request(&mut self, method: &str, params: Value) -> Result<Value> {
        self.sequence += 1;
        let id = self.sequence;
        self.write(&json!({ "id": id, "method": method, "params": params }))
            .await?;
        let response = async {
            loop {
                let Some(line) = self.lines.next_line().await? else {
                    bail!("Codex App Server stopped")
                };
                let Ok(message) = serde_json::from_str::<Value>(&line) else {
                    continue;
                };
                if message["id"].as_i64() != Some(id) {
                    continue;
                }
                if truthy(message.get("error")) {
                    bail!("Codex App Server request failed");
                }
                return Ok(message.get("result").cloned().unwrap_or(Value::Null));
            }
        };
        tokio::time::timeout(REQUEST_TIMEOUT, response)
            .await
            .map_err(|_| anyhow!("Codex App Server request timed out"))?
    }

    fn close(mut self) {
        drop(self.stdin.take());
        #[cfg(unix)]
        if let Some(pid) = self.child.id() {
            let _ = crate::process::signal(pid, nix::sys::signal::Signal::SIGTERM);
        }
        #[cfg(not(unix))]
        let _ = self.child.start_kill();
        let mut child = self.child;
        tokio::spawn(async move { child.wait().await });
    }
}

/// `evaluateRateLimits`: the snapshot for `model` (else `codex`, else the default) decides.
fn evaluate(result: &Value, model: &str, checked_at: &Value) -> Value {
    let entries: Vec<(&String, &Value)> = result
        .get("rateLimitsByLimitId")
        .and_then(Value::as_object)
        .map(|snapshots| {
            snapshots
                .iter()
                .filter(|(_, snapshot)| snapshot.is_object() || snapshot.is_array())
                .collect()
        })
        .unwrap_or_default();
    let model = normalize(Some(&Value::String(model.to_owned())));
    let snapshot = entries
        .iter()
        .find(|(_, snapshot)| normalize(snapshot.get("limitName")) == model)
        .or_else(|| entries.iter().find(|(limit, _)| *limit == "codex"))
        .map(|(_, snapshot)| *snapshot)
        .or_else(|| result.get("rateLimits").filter(|limits| limits.is_object()));
    let Some(snapshot) = snapshot else {
        return json!({ "state": "unknown", "checkedAt": checked_at });
    };

    let field =
        |value: Option<&Value>, name: &str| value.and_then(|value| value.get(name)).cloned();
    let windows: Vec<&Value> = [snapshot.get("primary"), snapshot.get("secondary")]
        .into_iter()
        .flatten()
        .filter(|window| truthy(Some(window)))
        .collect();
    let credits = snapshot.get("credits");
    let credits_available = field(credits, "unlimited") == Some(json!(true))
        || field(credits, "hasCredits") == Some(json!(true));
    let exhausted: Vec<&Value> = windows
        .iter()
        .copied()
        .filter(|window| number(window.get("usedPercent")) >= 100.0)
        .collect();
    let individual = snapshot.get("individualLimit");
    let individually_exhausted = number(field(individual, "remainingPercent").as_ref()) <= 0.0;
    let reached = truthy(snapshot.get("rateLimitReachedType"))
        || snapshot.get("spendControlReached") == Some(&json!(true));
    let blocked =
        reached || individually_exhausted || (!exhausted.is_empty() && !credits_available);
    if !blocked {
        return json!({ "state": "available", "checkedAt": checked_at });
    }

    let mut candidates: Vec<f64> = exhausted
        .iter()
        .map(|window| number(window.get("resetsAt")))
        .collect();
    candidates.push(number(field(individual, "resetsAt").as_ref()));
    if reached {
        candidates.extend(windows.iter().map(|window| number(window.get("resetsAt"))));
    }
    let resets_at = candidates
        .into_iter()
        .filter(|candidate| candidate.is_finite())
        .fold(f64::NEG_INFINITY, f64::max);
    let mut status = Map::from_iter([
        ("state".to_owned(), json!("blocked")),
        ("checkedAt".to_owned(), checked_at.clone()),
    ]);
    if resets_at.is_finite() {
        status.insert("resetsAt".to_owned(), js_number(resets_at));
    }
    Value::Object(status)
}

fn normalize(value: Option<&Value>) -> String {
    value
        .and_then(Value::as_str)
        .map(|name| {
            name.to_lowercase()
                .chars()
                .filter(|character| character.is_ascii_lowercase() || character.is_ascii_digit())
                .collect()
        })
        .unwrap_or_default()
}
