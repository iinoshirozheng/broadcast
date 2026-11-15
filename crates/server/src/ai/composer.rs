//! Composer references (`ComposerCatalog`, `web/src/composerReference.mjs`): the Skills, agents
//! and slash actions a composer offers, with per-workspace candidate refs that a turn resolves.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use broadcast_domain::{ApiError, Result, js};
use serde_json::{Value, json};
use unicode_normalization::UnicodeNormalization;

use super::agents::{self, Agent};
use super::app_server::AppServer;
use super::catalog::{self, display_name, entry_skills, trimmed};

pub const CONTRACT_VERSION: &str = "composer.v1";
const REFERENCE_FORMAT: &str = "broadcast.composer-reference.v1";
const REFERENCE_PREFIX: &str = "broadcast://composer-reference/v1";
const GLOBAL_WORKSPACE: &str = "__global__";

/// `VERIFIED_SLASH_ACTIONS`: command, label, description, client handler.
const SLASH_ACTIONS: [(&str, &str, &str, &str); 4] = [
    ("/new", "New conversation", "Start a new conversation", "new-conversation"),
    ("/model", "Model", "Choose the model", "open-model-menu"),
    ("/reasoning", "Reasoning", "Choose the reasoning effort", "open-reasoning-menu"),
    ("/compact", "Compact", "Compact this conversation's context", "compact-conversation"),
];

pub const COMPACT_HANDLER: &str = "compact-conversation";

/// `encodeComposerReferenceKey`: unpadded base64url of the UTF-8 stable id.
pub fn encode_key(stable_id: &str) -> String {
    URL_SAFE_NO_PAD.encode(stable_id)
}

/// `decodeComposerReferenceKey`: a canonical unpadded base64url key of a non-empty UTF-8 id.
pub fn decode_key(key: &str) -> Option<String> {
    if key.is_empty() || !key.bytes().all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-') {
        return None;
    }
    let decoded = String::from_utf8(URL_SAFE_NO_PAD.decode(key).ok()?).ok()?;
    (!decoded.is_empty() && encode_key(&decoded) == key).then_some(decoded)
}

pub fn nfc(value: &str) -> String {
    value.nfc().collect()
}

/// `composerReferencePersistence`: the Markdown marker that persists a reference in board text.
pub fn persistence(kind: &str, stable_id: &str, label: &str) -> Value {
    let stable_id = if kind == "skill" { nfc(stable_id) } else { stable_id.to_owned() };
    let key = encode_key(&stable_id);
    let label = label.replace('\\', "\\\\").replace('[', "\\[").replace(']', "\\]");
    json!({
        "format": REFERENCE_FORMAT,
        "kind": kind,
        "referenceKey": key,
        "markdown": format!("[{label}]({REFERENCE_PREFIX}/{kind}/{key})"),
    })
}

#[derive(Clone)]
pub struct ComposerSkill {
    pub identity: String,
    pub stable_id: String,
    pub name: String,
    pub path: String,
    pub label: String,
    pub description: Option<String>,
}

/// `sanitizeComposerSkills`: enabled Skills with an absolute path, unique by name and path.
fn sanitize_skills(entries: &[Value]) -> Vec<ComposerSkill> {
    let mut skills: Vec<ComposerSkill> = Vec::new();
    for skill in entry_skills(entries) {
        if skill.get("enabled") != Some(&Value::Bool(true)) {
            continue;
        }
        let Some(name) = trimmed(skill.get("name")) else { continue };
        let Some(path) = skill.get("path").and_then(Value::as_str).map(js::trim).filter(|path| Path::new(path).is_absolute()) else {
            continue;
        };
        let identity = format!("{name}\0{path}");
        if skills.iter().any(|known| known.identity == identity) {
            continue;
        }
        skills.push(ComposerSkill {
            identity,
            stable_id: nfc(&name),
            label: display_name(skill).unwrap_or_else(|| name.clone()),
            path: path.to_owned(),
            description: trimmed(skill.get("description")),
            name,
        });
    }
    skills
}

/// A resolved composer reference.
pub enum Reference {
    Skill(ComposerSkill),
    Agent(Agent),
}

/// A `POST /composer/rebind` document node.
pub enum RebindNode {
    Text(String),
    Unsupported { reason_code: &'static str },
    Persisted { kind: &'static str, stable_id: String },
}

/// A composer turn document node.
pub enum DocumentNode {
    Text(String),
    Reference { kind: &'static str, candidate_ref: String, label: String },
    Other(String),
}

impl DocumentNode {
    pub fn is_reference(&self) -> bool {
        matches!(self, Self::Reference { .. })
    }
}

/// One workspace's accepted catalog: a revision and the candidate ref of every item.
struct State {
    revision: String,
    signature: String,
    skills: Vec<ComposerSkill>,
    agents: Vec<Agent>,
    refs: HashMap<String, String>,
}

impl State {
    fn reference(&self, identity: &str) -> &str {
        self.refs.get(identity).map_or("", String::as_str)
    }
}

fn signature(skills: &[ComposerSkill], agents: &[Agent]) -> String {
    let skills = skills.iter().map(|skill| {
        json!({ "identity": skill.identity, "stableId": skill.stable_id, "label": skill.label, "description": skill.description })
    });
    let agents = agents.iter().map(|agent| {
        json!({
            "identity": agent.identity(),
            "stableId": agent.stable_id,
            "label": agent.label,
            "description": agent.description,
            "developerInstructions": agent.developer_instructions,
        })
    });
    Value::Array(skills.chain(agents).collect()).to_string()
}

/// `composerMatchScore`: prefix matches rank by length, then substring, then description matches.
fn match_score(query: &str, primaries: &[&str], description: &str) -> i64 {
    if query.is_empty() {
        return 0;
    }
    let values: Vec<String> = primaries.iter().map(|value| value.to_lowercase()).collect();
    if let Some(length) = values.iter().filter(|value| value.starts_with(query)).map(|value| js::len(value)).min() {
        return 1000 - i64::try_from(length).unwrap_or(i64::MAX);
    }
    if values.iter().any(|value| value.contains(query)) {
        return 500;
    }
    if description.to_lowercase().contains(query) { 100 } else { -1 }
}

fn source(kind: &str, available: bool) -> Value {
    if available {
        json!({ "kind": kind, "state": "available", "reasonCode": null })
    } else {
        json!({ "kind": kind, "state": "unavailable", "reasonCode": "SOURCE_UNAVAILABLE" })
    }
}

/// `composerSources`.
fn sources(skills: bool, agents: bool) -> Value {
    json!([
        source("skills", skills),
        source("agents", agents),
        source("slash", true),
        { "kind": "apps", "state": "unsupported", "reasonCode": "INVOCATION_NAME_UNAVAILABLE" },
        { "kind": "files", "state": "unsupported", "reasonCode": "ENCODER_UNSUPPORTED" },
        { "kind": "plugins", "state": "unsupported", "reasonCode": "EXPERIMENTAL_SOURCE_NOT_ALLOWED" },
        { "kind": "customPrompts", "state": "unsupported", "reasonCode": "NO_STABLE_CATALOG" },
    ])
}

/// `referenceUnavailable`.
fn unavailable(node_index: usize, reason_code: &str) -> ApiError {
    ApiError::new(409, "COMPOSER_REFERENCE_UNAVAILABLE", "A selected composer reference is no longer available")
        .with_details(json!({ "nodeIndex": node_index, "reasonCode": reason_code }))
}

fn sort_scored(mut scored: Vec<(i64, u8, usize, Value)>) -> Value {
    scored.sort_by(|left, right| right.0.cmp(&left.0).then(left.1.cmp(&right.1)).then(left.2.cmp(&right.2)));
    Value::Array(scored.into_iter().map(|(.., candidate)| candidate).collect())
}

/// `composerCandidatesForSurface`: board surfaces insert slash commands as text instead of
/// dispatching client actions; `/` there lists the Codex slash commands.
pub fn for_surface(mut response: Value, surface: &str, trigger: &str, query: &str) -> Value {
    if surface == "ai-chat" {
        return response;
    }
    if trigger != "/" {
        if let Some(candidates) = response.get_mut("candidates").and_then(Value::as_array_mut) {
            for candidate in candidates.iter_mut().filter(|candidate| candidate["kind"] == "slashAction") {
                let text = candidate["insertionText"].clone();
                if let Some(candidate) = candidate.as_object_mut() {
                    candidate.remove("dispatch");
                    candidate.insert("selection".to_owned(), json!({ "type": "insertText", "text": text }));
                }
            }
        }
        return response;
    }
    let Ok(commands) = catalog::slash_commands() else {
        response["candidates"] = json!([]);
        if let Some(sources) = response.get_mut("sources").and_then(Value::as_array_mut) {
            for source in sources.iter_mut().filter(|source| source["kind"] == "slash") {
                *source = json!({ "kind": "slash", "state": "unavailable", "reasonCode": "SOURCE_UNAVAILABLE" });
            }
        }
        return response;
    };
    let query = query.to_lowercase();
    let mut unique: Vec<catalog::SlashCommand> = Vec::new();
    for command in commands {
        if !unique.iter().any(|known| known.id == command.id) {
            unique.push(command);
        }
    }
    let scored = unique
        .iter()
        .enumerate()
        .filter_map(|(item_order, command)| {
            let label = command.label.strip_prefix('/').unwrap_or(&command.label);
            let score = match_score(&query, &[&command.id, label], &command.description);
            (score >= 0).then(|| {
                let candidate = json!({
                    "kind": "slashAction",
                    "candidateRef": format!("slash:insert:{}", command.id),
                    "trigger": "/",
                    "label": command.label,
                    "description": command.description,
                    "group": "Commands",
                    "groupOrder": 0,
                    "itemOrder": item_order,
                    "selectable": true,
                    "command": format!("/{}", command.id),
                    "insertionText": command.insert_text,
                    "selection": { "type": "insertText", "text": command.insert_text },
                });
                (score, 0, item_order, candidate)
            })
        })
        .collect();
    response["candidates"] = sort_scored(scored);
    response
}

/// `ComposerCatalog`: one app-server's Skills plus, for this device, its configured agents.
pub struct ComposerCatalog {
    server: AppServer,
    /// The Codex home whose agents this catalog offers; remote catalogs have none.
    codex_home: Option<PathBuf>,
    workspaces: Mutex<HashMap<String, Arc<State>>>,
}

impl ComposerCatalog {
    pub fn new(server: AppServer, codex_home: Option<PathBuf>) -> Self {
        Self { server, codex_home, workspaces: Mutex::new(HashMap::new()) }
    }

    /// Drops every accepted catalog, so the next request issues fresh refs (`skills/changed`).
    pub fn invalidate(&self) {
        self.workspaces.lock().unwrap_or_else(|poison| poison.into_inner()).clear();
    }

    async fn agents(&self, workspace: Option<&str>) -> Result<(Vec<Agent>, bool)> {
        let Some(home) = self.codex_home.clone() else { return Ok((Vec::new(), false)) };
        let workspace = workspace.map(str::to_owned);
        Ok(tokio::task::spawn_blocking(move || agents::configured(&home, workspace.as_deref())).await??)
    }

    async fn skills(&self, workspace: &str, force_reload: bool) -> Option<Vec<Value>> {
        self.server.list_skills(workspace, force_reload).await.ok()
    }

    /// `#acceptCatalog`: keeps the current refs while the catalog is unchanged.
    fn accept(&self, key: &str, skills: Vec<ComposerSkill>, agents: Vec<Agent>) -> Arc<State> {
        let signature = signature(&skills, &agents);
        let mut workspaces = self.workspaces.lock().unwrap_or_else(|poison| poison.into_inner());
        if let Some(current) = workspaces.get(key).filter(|current| current.signature == signature) {
            return Arc::clone(current);
        }
        let identities = skills.iter().map(|skill| skill.identity.clone()).chain(agents.iter().map(Agent::identity));
        let refs = identities.map(|identity| (identity, uuid::Uuid::new_v4().to_string())).collect();
        let state = Arc::new(State { revision: uuid::Uuid::new_v4().to_string(), signature, skills, agents, refs });
        workspaces.insert(key.to_owned(), Arc::clone(&state));
        state
    }

    fn revision(&self, key: &str) -> Option<String> {
        self.workspaces.lock().unwrap_or_else(|poison| poison.into_inner()).get(key).map(|state| state.revision.clone())
    }

    /// `candidates`: the composer menu for a `@` or `/` trigger.
    pub async fn candidates(&self, workspace: Option<&str>, trigger: &str, query: &str) -> Result<Value> {
        let entries = match workspace {
            Some(workspace) => self.skills(workspace, false).await,
            None => None,
        };
        let skills_available = entries.is_some();
        let (agents, agents_available) = self.agents(workspace).await?;
        let state = self.accept(workspace.unwrap_or(GLOBAL_WORKSPACE), sanitize_skills(&entries.unwrap_or_default()), agents);
        let query = query.to_lowercase();
        let mut scored = Vec::new();
        if trigger == "@" {
            for (item_order, skill) in state.skills.iter().enumerate() {
                if state.skills.iter().filter(|other| other.stable_id == skill.stable_id).count() != 1 {
                    continue;
                }
                let score = match_score(&query, &[&skill.label, &skill.name], skill.description.as_deref().unwrap_or(""));
                if score < 0 {
                    continue;
                }
                let candidate = json!({
                    "kind": "skill",
                    "candidateRef": state.reference(&skill.identity),
                    "trigger": trigger,
                    "label": skill.label,
                    "description": skill.description,
                    "group": "Skills",
                    "groupOrder": 0,
                    "itemOrder": item_order,
                    "selectable": true,
                    "persistence": persistence("skill", &skill.stable_id, &skill.label),
                });
                scored.push((score, 0, item_order, candidate));
            }
            for (item_order, agent) in state.agents.iter().enumerate() {
                let score = match_score(&query, &[&agent.label, &agent.stable_id], agent.description.as_deref().unwrap_or(""));
                if score < 0 {
                    continue;
                }
                let candidate = json!({
                    "kind": "agent",
                    "candidateRef": state.reference(&agent.identity()),
                    "trigger": trigger,
                    "label": agent.label,
                    "description": agent.description,
                    "group": "Agents",
                    "groupOrder": 1,
                    "itemOrder": item_order,
                    "selectable": true,
                    "insertionText": format!("@{}", agent.name),
                    "persistence": persistence("agent", &agent.stable_id, &agent.label),
                });
                scored.push((score, 1, item_order, candidate));
            }
        }
        if trigger == "/" {
            for (item_order, (command, label, description, handler)) in SLASH_ACTIONS.into_iter().enumerate() {
                let score = match_score(&query, &[&command[1..], label], description);
                if score < 0 {
                    continue;
                }
                let candidate = json!({
                    "kind": "slashAction",
                    "candidateRef": format!("slash:{handler}"),
                    "trigger": trigger,
                    "label": label,
                    "description": description,
                    "group": "Commands",
                    "groupOrder": 0,
                    "itemOrder": item_order,
                    "selectable": true,
                    "command": command,
                    "insertionText": command,
                    "dispatch": { "type": "client", "handlerId": handler },
                });
                scored.push((score, 0, item_order, candidate));
            }
        }
        Ok(json!({
            "contractVersion": CONTRACT_VERSION,
            "revision": state.revision,
            "candidates": sort_scored(scored),
            "sources": sources(skills_available, agents_available),
        }))
    }

    /// `rebindPersistedReferences`: maps persisted Markdown markers back to current candidate refs.
    pub async fn rebind(&self, workspace: &str, nodes: Vec<RebindNode>) -> Result<Value> {
        let entries = self.skills(workspace, true).await;
        let skills_available = entries.is_some();
        let (agents, agents_available) = self.agents(Some(workspace)).await?;
        let state = self.accept(workspace, sanitize_skills(&entries.unwrap_or_default()), agents);
        let mut rebound = Vec::new();
        let mut bindings = Vec::new();
        let mut ready = true;
        for (node_index, node) in nodes.into_iter().enumerate() {
            let (kind, stable_id) = match node {
                RebindNode::Text(text) => {
                    rebound.push(json!({ "type": "text", "text": text }));
                    continue;
                }
                RebindNode::Unsupported { reason_code } => {
                    ready = false;
                    bindings.push(json!({ "nodeIndex": node_index, "status": "unavailable", "referenceKind": "unsupported", "reasonCode": reason_code }));
                    continue;
                }
                RebindNode::Persisted { kind, stable_id } => (kind, stable_id),
            };
            let matches: Vec<(&str, &str)> = if kind == "skill" {
                let skills = state.skills.iter().filter(|skill| skill.stable_id == stable_id);
                skills.map(|skill| (state.reference(&skill.identity), skill.label.as_str())).collect()
            } else {
                let agents = state.agents.iter().filter(|agent| agent.stable_id == stable_id);
                agents.map(|agent| (state.reference(&agent.identity()), agent.label.as_str())).collect()
            };
            let source_available = if kind == "skill" { skills_available } else { agents_available };
            let reason_code = if !source_available {
                Some("SOURCE_UNAVAILABLE")
            } else if matches.is_empty() {
                Some("REFERENCE_NOT_FOUND")
            } else if matches.len() > 1 {
                Some("REFERENCE_AMBIGUOUS")
            } else {
                None
            };
            if let Some(reason_code) = reason_code {
                ready = false;
                bindings.push(json!({ "nodeIndex": node_index, "status": "unavailable", "referenceKind": kind, "reasonCode": reason_code }));
                continue;
            }
            let (candidate_ref, label) = matches[0];
            bindings.push(json!({ "nodeIndex": node_index, "status": "resolved", "referenceKind": kind, "label": label }));
            rebound.push(json!({ "type": kind, "candidateRef": candidate_ref, "label": label }));
        }
        let mut response = json!({ "contractVersion": CONTRACT_VERSION, "ready": ready, "revision": state.revision });
        if ready {
            response["document"] = json!({ "version": 1, "nodes": rebound });
        }
        response["bindings"] = Value::Array(bindings);
        response["sources"] = sources(skills_available, agents_available);
        response["diagnostics"] = json!([]);
        Ok(response)
    }

    /// `resolveReferences`: the Skill or agent behind each reference node, from the revision the
    /// composer saw; any catalog change since then makes the selection unavailable.
    pub async fn resolve(&self, workspace: &str, revision: &str, nodes: &[DocumentNode]) -> Result<Vec<Option<Reference>>> {
        let first = nodes.iter().position(DocumentNode::is_reference).unwrap_or(0);
        if self.revision(workspace).as_deref() != Some(revision) {
            return Err(unavailable(first, "SOURCE_UNAVAILABLE").into());
        }
        let Some(entries) = self.skills(workspace, true).await else { return Err(unavailable(first, "SOURCE_UNAVAILABLE").into()) };
        let (agents, _) = self.agents(Some(workspace)).await?;
        let state = self.accept(workspace, sanitize_skills(&entries), agents);
        if state.revision != revision {
            return Err(unavailable(first, "SOURCE_UNAVAILABLE").into());
        }
        let mut references = Vec::with_capacity(nodes.len());
        for (node_index, node) in nodes.iter().enumerate() {
            let DocumentNode::Reference { kind, candidate_ref, .. } = node else {
                references.push(None);
                continue;
            };
            let reference = if *kind == "skill" {
                state.skills.iter().find(|skill| state.reference(&skill.identity) == candidate_ref).cloned().map(Reference::Skill)
            } else {
                state.agents.iter().find(|agent| state.reference(&agent.identity()) == candidate_ref).cloned().map(Reference::Agent)
            };
            references.push(Some(reference.ok_or_else(|| unavailable(node_index, "SOURCE_UNAVAILABLE"))?));
        }
        Ok(references)
    }
}
