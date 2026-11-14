//! Codex custom agents configured in TOML (`listConfiguredAgents`): `~/.codex` first, then the
//! `.codex` folders from the repository root down to the workspace, each overriding the last.

use std::io;
use std::path::{Path, PathBuf};

use broadcast_domain::js;

#[derive(Clone)]
pub struct Agent {
    pub stable_id: String,
    pub name: String,
    pub label: String,
    pub description: Option<String>,
    pub developer_instructions: Option<String>,
}

impl Agent {
    pub fn identity(&self) -> String {
        format!("agent\0{}", self.stable_id)
    }

    /// `mergeConfiguredAgent`: `self` wins, falling back to `low` for the texts it leaves unset.
    fn over(self, low: &Agent) -> Agent {
        Agent {
            description: self.description.or_else(|| low.description.clone()),
            developer_instructions: self.developer_instructions.or_else(|| low.developer_instructions.clone()),
            ..self
        }
    }

    fn named(name: &str, table: &toml::Table) -> Agent {
        Agent {
            stable_id: name.to_owned(),
            name: name.to_owned(),
            label: name.to_owned(),
            description: text(table, "description"),
            developer_instructions: text(table, "developer_instructions"),
        }
    }
}

/// `nonEmptyTomlString`.
fn text(table: &toml::Table, key: &str) -> Option<String> {
    let value = js::trim(table.get(key)?.as_str()?);
    (!value.is_empty()).then(|| value.to_owned())
}

fn parse(path: &Path) -> Option<toml::Table> {
    std::fs::read_to_string(path).ok()?.parse().ok()
}

fn absolute(path: &Path) -> PathBuf {
    std::path::absolute(path).unwrap_or_else(|_| path.to_owned())
}

/// `readConfiguredAgent`: an agent file, named by its `name`, `role_name` or the declaring role.
/// A file found by discovery (no role) must carry both texts.
fn read_agent(path: &Path, role: Option<&str>) -> Option<Agent> {
    let table = parse(path)?;
    let stable_id = text(&table, "name").or_else(|| text(&table, "role_name")).or_else(|| role.map(str::to_owned))?;
    let agent = Agent::named(&stable_id, &table);
    if stable_id.is_empty() || (role.is_none() && (agent.description.is_none() || agent.developer_instructions.is_none())) {
        return None;
    }
    Some(agent)
}

/// `collectTomlFiles`: every `.toml` file under `directory`; a missing directory is not available.
fn toml_files(directory: &Path) -> io::Result<(Vec<PathBuf>, bool)> {
    let mut files = Vec::new();
    let mut pending = vec![directory.to_owned()];
    let mut available = false;
    while let Some(current) = pending.pop() {
        let entries = match std::fs::read_dir(&current) {
            Ok(entries) => entries,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error),
        };
        available = true;
        for entry in entries {
            let entry = entry?;
            let kind = entry.file_type()?;
            if kind.is_dir() {
                pending.push(entry.path());
            } else if kind.is_file() && entry.file_name().to_string_lossy().ends_with(".toml") {
                files.push(entry.path());
            }
        }
    }
    files.sort_by_key(|path| path.to_string_lossy().into_owned());
    Ok((files, available))
}

/// `projectConfigFolders`: `.codex` folders from the enclosing repository root down to the workspace.
fn project_folders(workspace: Option<&str>) -> Vec<PathBuf> {
    let Some(workspace) = workspace.filter(|path| !js::trim(path).is_empty()) else { return Vec::new() };
    let mut folders = Vec::new();
    let mut current = Some(absolute(Path::new(workspace)));
    while let Some(directory) = current {
        folders.push(directory.join(".codex"));
        if std::fs::metadata(directory.join(".git")).is_ok() {
            break;
        }
        current = directory.parent().map(Path::to_owned);
    }
    folders.reverse();
    folders
}

/// `loadAgentLayer`: the roles declared in a `config.toml`, then the agent files discovered beside it.
fn load_layer(directory: &Path, config_file: &Path, agents_directory: &Path) -> io::Result<(Vec<Agent>, bool)> {
    let mut agents: Vec<Agent> = Vec::new();
    let mut declared = Vec::new();
    let config = parse(config_file);
    let mut available = config.is_some();
    let roles = config.as_ref().and_then(|config| config.get("agents")?.as_table());
    for (name, role) in roles.into_iter().flatten() {
        let Some(role) = role.as_table() else { continue };
        let mut agent = Agent::named(name, role);
        if let Some(file) = text(role, "config_file") {
            let file = absolute(&directory.join(file));
            let Some(file_agent) = read_agent(&file, Some(name)) else { continue };
            agent = file_agent.over(&agent);
            declared.push(file);
        }
        if !agents.iter().any(|known| known.stable_id == agent.stable_id) {
            agents.push(agent);
        }
    }
    let (files, discovered) = toml_files(agents_directory)?;
    available |= discovered;
    for file in files {
        if declared.contains(&absolute(&file)) {
            continue;
        }
        if let Some(agent) = read_agent(&file, None)
            && !agents.iter().any(|known| known.stable_id == agent.stable_id)
        {
            agents.push(agent);
        }
    }
    Ok((agents, available))
}

/// `listConfiguredAgents`: the effective agents with a description, sorted by label.
pub fn configured(codex_home: &Path, workspace: Option<&str>) -> io::Result<(Vec<Agent>, bool)> {
    let mut layers = vec![(codex_home.to_owned(), codex_home.join("agents"))];
    layers.extend(project_folders(workspace).into_iter().map(|folder| (folder.clone(), folder.join("agents"))));
    let mut effective: Vec<Agent> = Vec::new();
    let mut available = false;
    for (directory, agents_directory) in layers {
        let (agents, loaded) = load_layer(&directory, &directory.join("config.toml"), &agents_directory)?;
        available |= loaded;
        for agent in agents {
            let position = effective.iter().position(|known| known.stable_id == agent.stable_id);
            let merged = match position {
                Some(index) => agent.over(&effective[index]),
                None => agent,
            };
            if merged.stable_id.is_empty() || merged.description.is_none() {
                continue;
            }
            match position {
                Some(index) => effective[index] = merged,
                None => effective.push(merged),
            }
        }
    }
    effective.sort_by(|left, right| js::locale_compare(&left.label, &right.label));
    Ok((effective, available))
}
