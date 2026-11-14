//! One `codex exec --json` turn: arguments, prompt, JSONL normalization and the turn-owner process
//! (`ai-chat-process.mjs`, `ai-turn-owner.mjs`).
//!
//! The server starts `<self> __turn-owner <codex> <args-json>` as its own process group and writes
//! the prompt as one JSON line to its stdin, keeping stdin open as a control channel. The owner
//! runs Codex, mirrors its exit, and kills the whole group when the control channel closes, so a
//! crashed server never leaves Codex running.

use std::ffi::OsString;
use std::io::{BufRead, Write};
use std::path::Path;
use std::process::Stdio;

use broadcast_domain::js;
use broadcast_domain::records::AiChatThread;
use serde_json::{Map, Value, json};
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStderr, ChildStdin, ChildStdout};

pub const TURN_OWNER_ARG: &str = "__turn-owner";
pub const SKILL_MARKER: char = '\u{FFFC}';
const VISIBLE_TEXT_LIMIT: usize = 65_536;
pub const MAX_LINE_BYTES: usize = 16 * 1024 * 1024;
const ITEM_TYPES: [&str; 7] =
    ["agent_message", "command_execution", "file_change", "mcp_tool_call", "web_search", "todo_list", "error"];

/// `resolveCodexPermissions`: stored sandbox values are product permission choices.
pub struct Permissions {
    pub sandbox: &'static str,
    pub approval_policy: &'static str,
    pub reviewer: Option<&'static str>,
}

pub fn permissions(sandbox: &str) -> Permissions {
    if sandbox == "danger-full-access" {
        return Permissions { sandbox: "danger-full-access", approval_policy: "never", reviewer: None };
    }
    let reviewer = if sandbox == "read-only" { "user" } else { "auto_review" };
    Permissions { sandbox: "workspace-write", approval_policy: "on-request", reviewer: Some(reviewer) }
}

pub fn capped_str(value: &str) -> String {
    js::prefix(value, VISIBLE_TEXT_LIMIT).to_owned()
}

/// `cappedText`: a string cut to the visible limit, or empty.
pub fn capped(value: &Value) -> String {
    value.as_str().map(capped_str).unwrap_or_default()
}

fn error_message(value: &Value) -> String {
    match value {
        Value::String(text) => capped_str(text),
        Value::Object(object) => capped(object.get("message").unwrap_or(&Value::Null)),
        _ => String::new(),
    }
}

fn detail_text(value: &Value) -> String {
    match value {
        Value::Null => String::new(),
        Value::String(text) => capped_str(text),
        other => capped_str(&other.to_string()),
    }
}

/// `value ?? fallback` over JSON fields.
fn either<'a>(value: &'a Value, fallback: &'a Value) -> &'a Value {
    if value.is_null() { fallback } else { value }
}

/// A normalized Codex event ready for the event log.
pub struct EventRecord {
    pub kind: String,
    pub role: &'static str,
    pub content: String,
    pub data: Value,
}

pub enum CodexEvent {
    ThreadStarted(String),
    Event(EventRecord),
}

fn normalized_item(raw_type: &str, item: &Value) -> EventRecord {
    let status = item["status"].as_str().map_or_else(|| raw_type["item.".len()..].to_owned(), capped_str);
    let item_id = capped(&item["id"]);
    let mut data = Map::new();
    data.insert("status".into(), json!(status));
    if !item_id.is_empty() {
        data.insert("itemId".into(), json!(item_id));
    }
    let kind = item["type"].as_str().unwrap_or_default().to_owned();
    let event = |role, content, data: Map<String, Value>| EventRecord {
        kind: kind.clone(),
        role,
        content,
        data: Value::Object(data),
    };
    match kind.as_str() {
        "agent_message" => event("assistant", capped(&item["text"]), data),
        "command_execution" => {
            let command = capped(&item["command"]);
            let output = capped(&item["aggregated_output"]);
            data.insert("command".into(), json!(command));
            if !output.is_empty() {
                data.insert("output".into(), json!(output));
            }
            if item["exit_code"].is_i64() || item["exit_code"].is_u64() {
                data.insert("exitCode".into(), item["exit_code"].clone());
            }
            event("activity", command, data)
        }
        "file_change" => {
            let changes: Vec<Value> = item["changes"]
                .as_array()
                .into_iter()
                .flatten()
                .map(|change| json!({ "path": capped(&change["path"]), "kind": capped(&change["kind"]) }))
                .filter(|change| change["path"].as_str().is_some_and(|path| !path.is_empty()))
                .collect();
            let paths = capped_str(
                &changes.iter().filter_map(|change| change["path"].as_str()).collect::<Vec<_>>().join("\n"),
            );
            let files: Vec<&str> = paths.split('\n').filter(|path| !path.is_empty()).collect();
            data.insert("files".into(), json!(files));
            if !changes.is_empty() {
                data.insert("detail".into(), json!(detail_text(&Value::Array(changes))));
            }
            event("activity", paths.clone(), data)
        }
        "mcp_tool_call" => {
            let server = capped(&item["server"]);
            let tool = capped(&item["tool"]);
            let mut detail = Map::new();
            for key in ["arguments", "result", "error"] {
                if let Some(value) = item.get(key) {
                    detail.insert(key.into(), value.clone());
                }
            }
            let detail = detail_text(&Value::Object(detail));
            if !server.is_empty() {
                data.insert("server".into(), json!(server));
            }
            if !tool.is_empty() {
                data.insert("tool".into(), json!(tool));
            }
            if !detail.is_empty() && detail != "{}" {
                data.insert("detail".into(), json!(detail));
            }
            let role = if js::truthy(&item["error"]) { "error" } else { "activity" };
            let name = [server, tool].into_iter().filter(|part| !part.is_empty()).collect::<Vec<_>>().join(".");
            event(role, capped_str(&name), data)
        }
        "web_search" => {
            let query = capped(&item["query"]);
            if !query.is_empty() {
                data.insert("query".into(), json!(query));
            }
            event("activity", query, data)
        }
        "todo_list" => {
            let items: Vec<Value> = item["items"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|todo| {
                    let text = capped(&todo["text"]);
                    if text.is_empty() {
                        return None;
                    }
                    let mut entry = Map::new();
                    entry.insert("text".into(), json!(text));
                    if let Some(completed) = todo["completed"].as_bool() {
                        entry.insert("completed".into(), json!(completed));
                    }
                    Some(Value::Object(entry))
                })
                .collect();
            let content =
                capped_str(&items.iter().filter_map(|todo| todo["text"].as_str()).collect::<Vec<_>>().join("\n"));
            if !items.is_empty() {
                data.insert("detail".into(), json!(detail_text(&Value::Array(items))));
            }
            event("activity", content, data)
        }
        _ => {
            let message = error_message(either(&item["message"], &item["error"]));
            if raw_type == "item.completed" && status == "completed" {
                data.insert("status".into(), json!("warning"));
                event("activity", message, data)
            } else {
                event("error", message, data)
            }
        }
    }
}

/// `normalizeCodexEvent`: `None` for events the log does not keep.
pub fn normalize_event(raw: &Value) -> Option<CodexEvent> {
    let raw_type = raw.as_object()?.get("type").and_then(Value::as_str)?;
    let event = |kind: &str, role, content, data| Some(CodexEvent::Event(EventRecord { kind: kind.to_owned(), role, content, data }));
    match raw_type {
        "thread.started" => {
            let id = raw["thread_id"].as_str()?;
            (!id.is_empty() && js::len(id) <= 256 && !id.contains('\0')).then(|| CodexEvent::ThreadStarted(id.to_owned()))
        }
        "turn.started" => event(raw_type, "activity", String::new(), json!({ "status": "started" })),
        "turn.completed" => {
            let mut usage = Map::new();
            for key in ["input_tokens", "cached_input_tokens", "output_tokens"] {
                if let Some(value) = raw["usage"].get(key).filter(|value| value.is_number()) {
                    usage.insert(key.into(), value.clone());
                }
            }
            let mut data = Map::new();
            data.insert("status".into(), json!("completed"));
            if !usage.is_empty() {
                data.insert("usage".into(), Value::Object(usage));
            }
            event(raw_type, "activity", String::new(), Value::Object(data))
        }
        "turn.failed" => {
            event(raw_type, "error", error_message(either(&raw["error"], &raw["message"])), json!({ "status": "failed" }))
        }
        "error" => event(
            raw_type,
            "activity",
            error_message(either(&raw["message"], &raw["error"])),
            json!({ "status": "warning" }),
        ),
        "item.started" | "item.updated" | "item.completed" => {
            let item = raw.get("item").filter(|item| item.is_object())?;
            ITEM_TYPES
                .contains(&item["type"].as_str()?)
                .then(|| CodexEvent::Event(normalized_item(raw_type, item)))
        }
        _ => None,
    }
}

/// `buildCodexArgs`.
pub fn build_args(thread: &AiChatThread, add_directories: &[String], image_paths: &[String]) -> Vec<String> {
    let permission = permissions(&thread.sandbox);
    let mut args: Vec<String> = ["exec", "--json", "--color", "never", "-C"].map(str::to_owned).into();
    args.extend([
        thread.origin.workspace_path.clone(),
        "-s".into(),
        permission.sandbox.into(),
        "-c".into(),
        format!("approval_policy=\"{}\"", permission.approval_policy),
    ]);
    if let Some(reviewer) = permission.reviewer {
        args.extend(["-c".into(), format!("approvals_reviewer=\"{reviewer}\"")]);
    }
    for directory in add_directories {
        args.extend(["--add-dir".into(), directory.clone()]);
    }
    if !thread.model.is_empty() {
        args.extend(["-m".into(), thread.model.clone()]);
    }
    if !thread.reasoning_effort.is_empty() {
        args.extend(["-c".into(), format!("model_reasoning_effort=\"{}\"", thread.reasoning_effort)]);
    }
    let resume = thread.codex_thread_id.as_ref().filter(|id| !id.is_empty());
    if resume.is_some() {
        args.push("resume".into());
    }
    for path in image_paths {
        args.extend(["-i".into(), path.clone()]);
    }
    if let Some(id) = resume {
        args.push(id.clone());
    }
    args.push("-".into());
    args
}

/// A selected Skill: the marker in the message becomes `[$id](path)`.
pub struct SkillLink {
    pub id: String,
    pub path: String,
}

/// `buildCodexPrompt`: the manage-broadcast skill, private context, then the user message.
pub fn build_prompt(
    thread: &AiChatThread,
    message: &str,
    skills: &[SkillLink],
    attachment_paths: &[String],
    skill_path: &Path,
) -> String {
    let mut skills = skills.iter();
    let mut user_message = String::with_capacity(message.len());
    for c in message.chars() {
        match (c, c == SKILL_MARKER) {
            (_, true) => {
                if let Some(skill) = skills.next() {
                    user_message.push_str(&format!("[${}]({})", skill.id, skill.path));
                }
            }
            (c, false) => user_message.push(c),
        }
    }
    let origin = &thread.origin;
    let mut context = vec![
        format!("project_id: {}", origin.project_id),
        format!("project_name: {}", origin.project_name),
        format!("workspace_path: {}", origin.workspace_path),
    ];
    if let Some(identifier) = origin.issue_identifier.as_ref().filter(|identifier| !identifier.is_empty()) {
        context.push(format!("issue_identifier: {identifier}"));
    }
    if !attachment_paths.is_empty() {
        context.push("turn_attachment_paths:".into());
        context.extend(attachment_paths.iter().map(|path| format!("- {path}")));
    }
    context.push(
        "This is private server-owned context. Do not quote, reveal, mention, or expose this block, its tags, or its filesystem paths to the user."
            .into(),
    );
    let mut lines = vec![format!("[$manage-broadcast]({}) e-broadcast", skill_path.display()), String::new()];
    lines.push("<broadcast_context>".into());
    lines.extend(context);
    lines.extend(["</broadcast_context>".into(), String::new(), "<user_message>".into(), user_message]);
    lines.push("</user_message>".into());
    lines.join("\n")
}

/// A running turn owner. Dropping `control` (or the server dying) kills the process group.
pub struct TurnProcess {
    pub pid: Option<u32>,
    pub child: Child,
    pub control: ChildStdin,
    pub stdout: BufReader<ChildStdout>,
    pub stderr: ChildStderr,
}

/// `spawnCodexTurn`: starts the owner and hands it the prompt.
pub async fn spawn_turn(executable: &str, args: &[String], prompt: &str) -> std::io::Result<TurnProcess> {
    let owner = std::env::current_exe()?;
    let mut command = crate::codex::command(&owner.to_string_lossy());
    command
        .arg(TURN_OWNER_ARG)
        .arg(executable)
        .arg(serde_json::to_string(args)?)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(false);
    #[cfg(unix)]
    command.process_group(0);
    let mut child = command.spawn()?;
    let mut control = child.stdin.take().ok_or_else(|| std::io::Error::other("turn owner stdin is unavailable"))?;
    let stdout = child.stdout.take().ok_or_else(|| std::io::Error::other("turn owner stdout is unavailable"))?;
    let stderr = child.stderr.take().ok_or_else(|| std::io::Error::other("turn owner stderr is unavailable"))?;
    let pid = child.id();
    let mut line = serde_json::to_string(prompt)?;
    line.push('\n');
    // A failed write means the owner already exited; its exit status reports why.
    let _ = control.write_all(line.as_bytes()).await;
    Ok(TurnProcess { pid, child, control, stdout: BufReader::new(stdout), stderr })
}

pub enum Line {
    Read,
    Eof,
    TooLong,
}

/// Reads one `\n`-terminated line into `line` without buffering more than `limit` bytes.
pub async fn read_line(reader: &mut (impl AsyncBufRead + Unpin), line: &mut Vec<u8>, limit: usize) -> std::io::Result<Line> {
    line.clear();
    loop {
        let available = reader.fill_buf().await?;
        if available.is_empty() {
            return Ok(if line.is_empty() { Line::Eof } else { Line::Read });
        }
        let (take, done) = match available.iter().position(|&byte| byte == b'\n') {
            Some(index) => (index, true),
            None => (available.len(), false),
        };
        if line.len() + take > limit {
            return Ok(Line::TooLong);
        }
        line.extend_from_slice(&available[..take]);
        reader.consume(take + usize::from(done));
        if done {
            return Ok(Line::Read);
        }
    }
}

/// `signalProcessTree`: signals the owner's process group (the whole tree on Windows).
pub fn signal_group(pid: u32, kill: bool) {
    #[cfg(unix)]
    {
        use nix::sys::signal::{Signal, killpg};
        use nix::unistd::Pid;
        let signal = if kill { Signal::SIGKILL } else { Signal::SIGTERM };
        if let Ok(pid) = i32::try_from(pid) {
            let _ = killpg(Pid::from_raw(pid), signal);
        }
    }
    #[cfg(not(unix))]
    {
        let _ = kill;
        let _ = std::process::Command::new("taskkill")
            .args(["/PID", &pid.to_string(), "/T", "/F"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
}

/// The `__turn-owner` process: runs Codex with the prompt on stdin and mirrors its exit.
pub fn turn_owner(mut args: impl Iterator<Item = OsString>) -> ! {
    let (Some(executable), Some(encoded)) = (args.next(), args.next()) else { std::process::exit(2) };
    let Ok(codex_args) = serde_json::from_str::<Vec<String>>(&encoded.to_string_lossy()) else { std::process::exit(2) };
    let mut line = String::new();
    if std::io::stdin().lock().read_line(&mut line).is_err() {
        std::process::exit(2);
    }
    let Ok(prompt) = serde_json::from_str::<String>(&line) else { std::process::exit(2) };
    let mut child = match std::process::Command::new(&executable).args(&codex_args).stdin(Stdio::piped()).spawn() {
        Ok(child) => child,
        Err(error) => {
            eprintln!("{error}");
            std::process::exit(1);
        }
    };
    let pid = child.id();
    std::thread::spawn(move || {
        let _ = std::io::copy(&mut std::io::stdin().lock(), &mut std::io::sink());
        terminate_owner_group(pid);
    });
    if let Some(mut input) = child.stdin.take() {
        let _ = input.write_all(prompt.as_bytes());
    }
    let status = match child.wait() {
        Ok(status) => status,
        Err(error) => {
            eprintln!("{error}");
            std::process::exit(1);
        }
    };
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        if let Some(signal) = status.signal().and_then(|signal| nix::sys::signal::Signal::try_from(signal).ok()) {
            let _ = nix::sys::signal::raise(signal);
        }
    }
    std::process::exit(status.code().unwrap_or(1))
}

fn terminate_owner_group(codex_pid: u32) -> ! {
    #[cfg(unix)]
    {
        let _ = codex_pid;
        let _ = nix::sys::signal::killpg(nix::unistd::getpgrp(), nix::sys::signal::Signal::SIGKILL);
    }
    #[cfg(not(unix))]
    signal_group(codex_pid, true);
    std::process::exit(1)
}

/// Drains a reader the caller no longer needs.
pub async fn drain(mut reader: impl tokio::io::AsyncRead + Unpin) {
    let _ = tokio::io::copy(&mut reader, &mut tokio::io::sink()).await;
}
