//! Where a conversation runs (`resolveAiChatContext`, `resolveAiWorkspace`): the project, its
//! workspace on this device or a remote Codex host, and the task it is about.

use std::path::Path;
use std::sync::Arc;

use broadcast_domain::constants::DEFAULT_PROJECT_ID;
use broadcast_domain::{ApiError, Error, Result, js};
use serde_json::Value;

use crate::{App, codex};

/// A Codex project on a remote host that runs a conversation.
#[derive(Clone)]
pub struct RemoteTarget {
    pub codex_project_id: String,
    pub codex_host_id: String,
    pub workspace_path: String,
}

pub struct Issue {
    pub id: String,
    pub identifier: String,
}

pub struct Resolved {
    pub workspace_path: String,
    pub add_directories: Vec<String>,
    pub project_id: String,
    pub project_name: String,
    pub issue: Option<Issue>,
    pub remote: Option<RemoteTarget>,
}

/// `existingDirectory`: an absolute path resolved to the real directory it names.
pub async fn existing_directory(value: &str) -> Option<String> {
    let value = js::trim(value);
    if !Path::new(value).is_absolute() {
        return None;
    }
    let resolved = tokio::fs::canonicalize(value).await.ok()?;
    tokio::fs::metadata(&resolved)
        .await
        .ok()?
        .is_dir()
        .then(|| resolved.to_string_lossy().into_owned())
}

/// `loadDeviceWorkspaces`: Codex project roots, then board projects' own workspaces.
async fn device_workspaces(app: &Arc<App>) -> Result<Vec<(String, String)>> {
    let mut workspaces: Vec<(String, String)> = Vec::new();
    let state: Option<Value> = tokio::fs::read(codex::state_path(&app.config.codex_home))
        .await
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok());
    let projects = state
        .as_ref()
        .and_then(|state| state.get("local-projects")?.as_object());
    for (project_id, project) in projects.into_iter().flatten() {
        for root in project
            .get("rootPaths")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            if let Some(workspace) = existing_directory(root.as_str().unwrap_or_default()).await {
                workspaces.push((project_id.clone(), workspace));
                break;
            }
        }
    }
    for project in app.store(|store| store.list_projects()).await? {
        if workspaces.iter().any(|(id, _)| *id == project.id) {
            continue;
        }
        if let Some(workspace) =
            existing_directory(project.workspace_path.as_deref().unwrap_or_default()).await
        {
            workspaces.push((project.id, workspace));
        }
    }
    Ok(workspaces)
}

/// `loadMappedWorkspaces`: the cloud project mappings that name a directory on this device.
async fn mapped_workspaces(mappings: &serde_json::Map<String, Value>) -> Vec<(String, String)> {
    let mut workspaces = Vec::new();
    for (project_id, path) in mappings {
        if let Some(workspace) = existing_directory(path.as_str().unwrap_or_default()).await {
            workspaces.push((project_id.clone(), workspace));
        }
    }
    workspaces
}

fn project_not_found(project_id: &str) -> Error {
    ApiError::new(
        404,
        "PROJECT_NOT_FOUND",
        format!("Project '{project_id}' does not exist"),
    )
    .into()
}

/// `resolvedWorkspace`: the project's workspace plus every other known workspace as extra roots.
fn resolved_workspace(
    project_id: &str,
    project_name: String,
    workspaces: &[(String, String)],
) -> Result<Resolved> {
    let Some((_, workspace_path)) = workspaces.iter().find(|(id, _)| id == project_id) else {
        let message = format!("Project '{project_id}' has no available device workspace");
        return Err(ApiError::new(409, "PROJECT_WORKSPACE_UNAVAILABLE", message).into());
    };
    let mut add_directories: Vec<String> = Vec::new();
    for (_, workspace) in workspaces {
        if workspace != workspace_path && !add_directories.contains(workspace) {
            add_directories.push(workspace.clone());
        }
    }
    Ok(Resolved {
        workspace_path: workspace_path.clone(),
        add_directories,
        project_id: project_id.to_owned(),
        project_name,
        issue: None,
        remote: None,
    })
}

fn inactive_issue(project_id: &str, issue_id: &str) -> Error {
    let message = format!("Task '{issue_id}' is not an active task in project '{project_id}'");
    ApiError::new(404, "AI_CHAT_ISSUE_NOT_FOUND", message).into()
}

async fn local_issue(
    app: &Arc<App>,
    project_id: &str,
    issue_id: Option<&str>,
) -> Result<Option<Issue>> {
    let Some(issue_id) = issue_id else {
        return Ok(None);
    };
    let id = issue_id.to_owned();
    match app.store(move |store| store.get_task(&id)).await? {
        Some(task) if task.project_id == project_id && task.archived_at.is_none() => {
            Ok(Some(Issue {
                id: task.id,
                identifier: task.identifier,
            }))
        }
        _ => Err(inactive_issue(project_id, issue_id)),
    }
}

fn remote_resolved(
    project_id: &str,
    project_name: String,
    issue: Option<Issue>,
    target: &RemoteTarget,
) -> Resolved {
    Resolved {
        workspace_path: target.workspace_path.clone(),
        add_directories: Vec::new(),
        project_id: project_id.to_owned(),
        project_name,
        issue,
        remote: Some(target.clone()),
    }
}

/// `resolveAiChatContext`: local board projects resolve against this device's workspaces (the
/// default project falls back to the server's directory); cloud projects against the mappings.
pub async fn resolve(
    app: &Arc<App>,
    project_id: &str,
    issue_id: Option<&str>,
    remote: Option<&RemoteTarget>,
) -> Result<Resolved> {
    let config = app.cloud.read().await.map_err(Error::from)?;
    if config.remote_url.is_none() {
        let id = project_id.to_owned();
        let project = app.store(move |store| store.get_project(&id)).await?;
        if let Some(target) = remote {
            let project = project.ok_or_else(|| project_not_found(project_id))?;
            let issue = local_issue(app, project_id, issue_id).await?;
            return Ok(remote_resolved(project_id, project.name, issue, target));
        }
        let project = project.ok_or_else(|| project_not_found(project_id))?;
        let workspaces = device_workspaces(app).await?;
        let mut resolved = match resolved_workspace(project_id, project.name.clone(), &workspaces) {
            Err(Error::Api(error))
                if error.code == "PROJECT_WORKSPACE_UNAVAILABLE"
                    && project_id == DEFAULT_PROJECT_ID =>
            {
                Resolved {
                    workspace_path: std::env::current_dir()?.to_string_lossy().into_owned(),
                    add_directories: Vec::new(),
                    project_id: project.id,
                    project_name: project.name,
                    issue: None,
                    remote: None,
                }
            }
            other => other?,
        };
        resolved.issue = local_issue(app, project_id, issue_id).await?;
        return Ok(resolved);
    }

    let projects = app
        .cloud_proxy
        .read_json(app, &config, "/api/projects")
        .await?;
    let project = projects
        .get("projects")
        .and_then(Value::as_array)
        .and_then(|projects| {
            projects
                .iter()
                .find(|project| project.get("id").and_then(Value::as_str) == Some(project_id))
        })
        .ok_or_else(|| project_not_found(project_id))?;
    let project_name = project
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    let issue = match issue_id {
        None => None,
        Some(issue_id) => {
            let path = format!("/api/tasks/{}", js::encode_uri_component(issue_id));
            let payload = app.cloud_proxy.read_json(app, &config, &path).await?;
            let task = payload.get("task").filter(|task| {
                task.get("projectId").and_then(Value::as_str) == Some(project_id)
                    && task.get("archivedAt").is_none_or(Value::is_null)
            });
            let task = task.ok_or_else(|| inactive_issue(project_id, issue_id))?;
            let text = |key: &str| {
                task.get(key)
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned()
            };
            Some(Issue {
                id: text("id"),
                identifier: text("identifier"),
            })
        }
    };
    if let Some(target) = remote {
        return Ok(remote_resolved(project_id, project_name, issue, target));
    }
    let workspaces = mapped_workspaces(&config.project_mappings).await;
    let mut resolved = resolved_workspace(project_id, project_name, &workspaces)?;
    resolved.issue = issue;
    Ok(resolved)
}
