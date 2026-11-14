//! Plan progress and running state of Codex threads, read from the tail of their session logs.

use std::collections::HashMap;
use std::io::{ErrorKind, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant, SystemTime};

use regex::Regex;
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncSeekExt};

/// Only the newest 16 MiB of a session is read.
const TAIL_BYTES: u64 = 16 * 1024 * 1024;
/// How long a thread without a session file stays "not found" before searching again.
const MISS_TTL: Duration = Duration::from_secs(5);

static PLAN_STATUS: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"["']?status["']?\s*:\s*["'](completed|in_progress|pending)["']"#).expect("valid pattern")
});

struct Search {
    path: Option<PathBuf>,
    checked: Instant,
}

struct Cached {
    size: u64,
    modified: SystemTime,
    state: Value,
}

pub struct Sessions {
    directory: PathBuf,
    searches: Mutex<HashMap<String, Search>>,
    states: Mutex<HashMap<PathBuf, Cached>>,
}

impl Sessions {
    pub fn new(codex_home: &Path) -> Self {
        Self { directory: codex_home.join("sessions"), searches: Mutex::default(), states: Mutex::default() }
    }

    /// `findCodexSession`: the `*-<thread>.jsonl` file anywhere under `sessions/`.
    async fn find(&self, thread_id: &str) -> std::io::Result<Option<PathBuf>> {
        if let Some(search) = self.searches.lock().expect("lock").get(thread_id)
            && (search.path.is_some() || search.checked.elapsed() < MISS_TTL)
        {
            return Ok(search.path.clone());
        }
        let suffix = format!("-{thread_id}.jsonl");
        let mut directories = vec![self.directory.clone()];
        let mut found = None;
        'walk: while let Some(directory) = directories.pop() {
            let mut entries = match tokio::fs::read_dir(&directory).await {
                Ok(entries) => entries,
                Err(error) if error.kind() == ErrorKind::NotFound => continue,
                Err(error) => return Err(error),
            };
            while let Some(entry) = entries.next_entry().await? {
                let kind = entry.file_type().await?;
                if kind.is_dir() {
                    directories.push(entry.path());
                } else if kind.is_file() && entry.file_name().to_string_lossy().ends_with(&suffix) {
                    found = Some(entry.path());
                    break 'walk;
                }
            }
        }
        let search = Search { path: found.clone(), checked: Instant::now() };
        self.searches.lock().expect("lock").insert(thread_id.to_owned(), search);
        Ok(found)
    }

    /// `readCodexSessionState`: `{ completed, total, running }`, or null without a session.
    pub async fn state(&self, thread_id: &str) -> std::io::Result<Value> {
        let Some(path) = self.find(thread_id).await? else { return Ok(Value::Null) };
        let metadata = tokio::fs::metadata(&path).await?;
        let (size, modified) = (metadata.len(), metadata.modified()?);
        if let Some(cached) = self.states.lock().expect("lock").get(&path)
            && cached.size == size
            && cached.modified == modified
        {
            return Ok(cached.state.clone());
        }
        let length = size.min(TAIL_BYTES);
        let mut buffer = vec![0; usize::try_from(length).expect("tail fits in memory")];
        let mut file = tokio::fs::File::open(&path).await?;
        file.seek(SeekFrom::Start(size - length)).await?;
        file.read_exact(&mut buffer).await?;
        let text = String::from_utf8_lossy(&buffer);
        let skip = usize::from(length < size);
        let records: Vec<Value> = text.split('\n').skip(skip).filter_map(|line| serde_json::from_str(line).ok()).collect();
        let state = summarize(&records);
        self.states.lock().expect("lock").insert(path, Cached { size, modified, state: state.clone() });
        Ok(state)
    }
}

fn summarize(records: &[Value]) -> Value {
    let mut running: Option<&str> = None;
    for record in records {
        let payload = &record["payload"];
        let Some(turn) = payload["turn_id"].as_str().filter(|_| record["type"] == "event_msg") else { continue };
        match payload["type"].as_str() {
            Some("task_started") => running = Some(turn),
            Some("task_complete" | "turn_aborted") if running == Some(turn) => running = None,
            _ => {}
        }
    }
    let progress = records.iter().rev().find_map(|record| {
        let payload = &record["payload"];
        let input = payload["input"].as_str().filter(|_| payload["type"] == "custom_tool_call")?;
        let statuses = plan_statuses(payload["name"].as_str(), input);
        (!statuses.is_empty()).then(|| (statuses.iter().filter(|status| **status == "completed").count(), statuses.len()))
    });
    json!({
        "completed": progress.map(|(completed, _)| completed),
        "total": progress.map(|(_, total)| total),
        "running": running.is_some(),
    })
}

/// Statuses of the latest plan update in a tool call: `update_plan` JSON, or a `tools.update_plan(` exec script.
fn plan_statuses(name: Option<&str>, input: &str) -> Vec<Value> {
    match name {
        Some("update_plan") => serde_json::from_str::<Value>(input)
            .ok()
            .and_then(|input| input.get("plan")?.as_array().cloned())
            .map(|plan| plan.iter().map(|item| item["status"].clone()).filter(truthy).collect())
            .unwrap_or_default(),
        Some("exec") => input
            .rfind("tools.update_plan(")
            .map(|start| PLAN_STATUS.captures_iter(&input[start..]).map(|capture| json!(&capture[1])).collect())
            .unwrap_or_default(),
        _ => Vec::new(),
    }
}

fn truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(value) => *value,
        Value::Number(number) => number.as_f64().is_some_and(|number| number != 0.0),
        Value::String(text) => !text.is_empty(),
        Value::Array(_) | Value::Object(_) => true,
    }
}

/// A requested thread id without its `local:` or `cloud:` prefix.
pub fn strip_thread_prefix(value: &str) -> &str {
    let value = broadcast_domain::js::trim(value);
    ["local:", "cloud:"]
        .iter()
        .find_map(|prefix| value.get(..prefix.len()).filter(|head| head.eq_ignore_ascii_case(prefix)).map(|_| &value[prefix.len()..]))
        .unwrap_or(value)
}

/// A Codex thread id is a hyphenated UUID.
pub fn is_thread_id(id: &str) -> bool {
    id.len() == 36
        && id.char_indices().all(|(index, c)| if [8, 13, 18, 23].contains(&index) { c == '-' } else { c.is_ascii_hexdigit() })
}
