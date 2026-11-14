//! Codex desktop state on this device (`~/.codex`) and the git contexts of its workspaces.

pub mod progress;

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::{Map, Value, json};
use tokio::process::Command;

const GIT_TIMEOUT: Duration = Duration::from_secs(4);

pub fn state_path(codex_home: &Path) -> PathBuf {
    codex_home.join(".codex-global-state.json")
}

pub fn processes_path(codex_home: &Path) -> PathBuf {
    codex_home.join("process_manager").join("chat_processes.json")
}

/// A command whose environment drops the launcher's `BROADCAST_*` variables.
pub fn command(program: &str) -> Command {
    let mut command = Command::new(program);
    command.env_clear().envs(std::env::vars_os().filter(|(name, _)| {
        !name.to_str().is_some_and(|name| name.starts_with("BROADCAST_"))
    }));
    command
}

async fn read_json(path: &Path) -> Option<Value> {
    serde_json::from_slice(&tokio::fs::read(path).await.ok()?).ok()
}

/// `codexProjectRoot`: the first non-blank root of a Codex local project.
fn project_root(state: &Value, project_id: Option<&str>) -> Option<String> {
    let root = state.get("local-projects")?.get(project_id.filter(|id| !id.is_empty())?)?.get("rootPaths")?.get(0)?;
    root.as_str().filter(|root| !root.trim().is_empty()).map(str::to_owned)
}

/// `readCodexProjectWorkspaces`: Codex project id → workspace root.
pub async fn workspaces(codex_home: &Path) -> Map<String, Value> {
    let Some(state) = read_json(&state_path(codex_home)).await else { return Map::new() };
    let Some(projects) = state.get("local-projects").and_then(Value::as_object) else { return Map::new() };
    projects
        .keys()
        .filter_map(|id| project_root(&state, Some(id)).map(|root| (id.clone(), Value::String(root))))
        .collect()
}

/// `latestThreadCwd`: the most recently updated process entry for a Codex thread.
fn latest_thread_cwd(value: &Value, thread_id: &str) -> Option<String> {
    let mut best: Option<(f64, &str)> = None;
    let mut stack = vec![value];
    while let Some(candidate) = stack.pop() {
        match candidate {
            Value::Array(items) => stack.extend(items),
            Value::Object(object) => {
                let cwd = object.get("cwd").and_then(Value::as_str).filter(|cwd| !cwd.trim().is_empty());
                if let Some(cwd) = cwd
                    && object.get("conversationId").and_then(Value::as_str) == Some(thread_id)
                {
                    let updated = object.get("updatedAtMs").and_then(Value::as_f64).unwrap_or(0.0);
                    if best.is_none_or(|(latest, _)| updated > latest) {
                        best = Some((updated, cwd));
                    }
                }
                stack.extend(object.values());
            }
            _ => {}
        }
    }
    best.map(|(_, cwd)| cwd.to_owned())
}

/// `resolveProjectWorkspace`: Codex project roots first, then the board project, then the thread's cwd.
pub async fn resolve_workspace(
    codex_home: &Path,
    project_id: &str,
    project_workspace: Option<&str>,
    codex_project_id: Option<&str>,
    thread_id: Option<&str>,
) -> Option<String> {
    if let Some(state) = read_json(&state_path(codex_home)).await {
        let assignment = thread_id.and_then(|id| state.get("thread-project-assignments")?.get(id));
        let root = project_root(&state, Some(project_id))
            .or_else(|| project_root(&state, codex_project_id))
            .or_else(|| project_root(&state, assignment?.get("projectId")?.as_str()))
            .or_else(|| assignment?.get("cwd")?.as_str().map(str::to_owned))
            .filter(|root| !root.is_empty());
        if root.is_some() {
            return root;
        }
    }
    if let Some(workspace) = project_workspace.filter(|path| !path.is_empty()) {
        return Some(workspace.to_owned());
    }
    let thread_id = thread_id.filter(|id| !id.is_empty())?;
    latest_thread_cwd(&read_json(&processes_path(codex_home)).await?, thread_id)
}

async fn git(args: &[&str]) -> Option<String> {
    let mut command = command("git");
    command.args(args).kill_on_drop(true);
    let output = tokio::time::timeout(GIT_TIMEOUT, command.output()).await.ok()?.ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8(output.stdout).ok()
}

/// `scanDevelopmentContexts`: the workspace's git root, local branches and live worktrees.
pub async fn development_contexts(workspace: Option<&str>) -> Value {
    let Some(workspace) = workspace.filter(|path| !path.is_empty()) else {
        return json!({ "workspacePath": null, "contexts": [] });
    };
    scan(workspace).await.unwrap_or_else(|| json!({ "workspacePath": workspace, "contexts": [] }))
}

async fn scan(workspace: &str) -> Option<Value> {
    let root = git(&["-C", workspace, "rev-parse", "--show-toplevel"]).await?.trim().to_owned();
    let branch_args = ["-C", &root, "for-each-ref", "--format=%(refname:short)", "refs/heads"];
    let worktree_args = ["-C", &root, "worktree", "list", "--porcelain"];
    let (branches, worktrees) = tokio::join!(git(&branch_args), git(&worktree_args));
    let (branches, worktrees) = (branches?, worktrees?);
    let mut contexts: Vec<Value> = branches
        .lines()
        .map(str::trim)
        .filter(|branch| !branch.is_empty())
        .map(|branch| json!({ "type": "branch", "branch": branch }))
        .collect();
    contexts.extend(parse_worktrees(&worktrees).await?);
    Some(json!({ "workspacePath": root, "contexts": contexts }))
}

/// `git worktree list --porcelain` blocks, skipping prunable and vanished worktrees.
async fn parse_worktrees(output: &str) -> Option<Vec<Value>> {
    let mut contexts = Vec::new();
    let mut blocks = Vec::new();
    let mut block = Vec::new();
    for line in output.trim().split('\n') {
        if line.trim().is_empty() {
            blocks.push(std::mem::take(&mut block));
        } else {
            block.push(line);
        }
    }
    blocks.push(block);
    for block in blocks {
        let path = block.iter().rev().find_map(|line| line.strip_prefix("worktree ")).unwrap_or_default();
        let branch = block.iter().rev().find_map(|line| line.strip_prefix("branch refs/heads/"));
        if path.is_empty() || block.iter().any(|line| line.starts_with("prunable")) {
            continue;
        }
        match tokio::fs::metadata(path).await {
            Ok(_) => contexts.push(json!({ "type": "worktree", "path": path, "branch": branch })),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => return None,
        }
    }
    Some(contexts)
}
