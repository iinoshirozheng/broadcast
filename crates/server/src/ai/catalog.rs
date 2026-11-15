//! The AI chat catalog: the models, skills and slash commands a Codex
//! CLI offers for a workspace.

use std::process::Stdio;
use std::sync::LazyLock;
use std::time::Duration;

use anyhow::{Context, anyhow, bail};
use broadcast_domain::constants::AI_SANDBOX_MODES;
use broadcast_domain::js;
use serde::Serialize;
use serde_json::{Map, Value, json};
use tokio::io::{AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout};

use super::app_server::{self, AppServer};
use super::exec::{self, Line};

const CATALOG_TIMEOUT: Duration = Duration::from_secs(10);
const CATALOG_MAX_BUFFER: usize = 2 * 1024 * 1024;
const PLATFORM: &str = if cfg!(target_os = "macos") {
    "darwin"
} else if cfg!(windows) {
    "win32"
} else {
    std::env::consts::OS
};

static SLASH_COMMANDS: LazyLock<Option<Value>> = LazyLock::new(|| {
    serde_json::from_str(include_str!(
        "../../assets/codex-slash-commands-0.139.0.json"
    ))
    .ok()
});

#[derive(Clone, Serialize)]
pub struct ServiceTier {
    pub id: String,
    pub name: String,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Model {
    pub slug: String,
    pub display_name: String,
    pub description: String,
    pub default_reasoning_effort: String,
    pub supported_reasoning_efforts: Vec<String>,
    pub service_tiers: Vec<ServiceTier>,
}

#[derive(Clone, Serialize)]
pub struct Skill {
    pub id: String,
    pub label: String,
    pub description: String,
    pub path: String,
    pub scope: String,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SlashCommand {
    pub id: String,
    pub label: String,
    pub description: String,
    pub insert_text: String,
}

#[derive(Serialize)]
pub struct Catalog {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub available: Option<bool>,
    pub models: Vec<Model>,
    pub skills: Vec<Skill>,
    pub commands: Vec<SlashCommand>,
    pub sandboxes: Vec<&'static str>,
}

/// A trimmed non-empty string.
pub fn trimmed(value: Option<&Value>) -> Option<String> {
    let text = js::trim(value?.as_str()?);
    (!text.is_empty()).then(|| text.to_owned())
}

/// `/^[a-z][a-z0-9-]*$/`.
pub fn is_command_id(id: &str) -> bool {
    let mut chars = id.chars();
    chars.next().is_some_and(|first| first.is_ascii_lowercase())
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

/// The model fields of one catalog dialect: `debug models` (snake case) or `model/list`.
struct ModelKeys {
    slug: &'static str,
    display_name: &'static str,
    default_effort: &'static str,
    efforts: &'static str,
    effort: &'static str,
    tiers: &'static str,
}

fn sanitize(
    value: Option<&Value>,
    keys: &ModelKeys,
    visible: fn(&Map<String, Value>) -> bool,
) -> anyhow::Result<Vec<Model>> {
    let models = value
        .and_then(Value::as_array)
        .context("Codex returned an invalid model catalog")?;
    Ok(models
        .iter()
        .filter_map(Value::as_object)
        .filter(|model| visible(model))
        .filter_map(|model| {
            let slug = trimmed(model.get(keys.slug))?;
            let mut efforts: Vec<String> = Vec::new();
            for effort in model
                .get(keys.efforts)
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                if let Some(effort) = trimmed(effort.get(keys.effort))
                    && !efforts.contains(&effort)
                {
                    efforts.push(effort);
                }
            }
            let service_tiers = model
                .get(keys.tiers)
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(|tier| {
                    Some(ServiceTier {
                        id: trimmed(tier.get("id"))?,
                        name: trimmed(tier.get("name"))?,
                    })
                })
                .collect();
            Some(Model {
                display_name: trimmed(model.get(keys.display_name)).unwrap_or_else(|| slug.clone()),
                slug,
                description: model
                    .get("description")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned(),
                default_reasoning_effort: model
                    .get(keys.default_effort)
                    .and_then(Value::as_str)
                    .map(|effort| js::trim(effort).to_owned())
                    .unwrap_or_default(),
                supported_reasoning_efforts: efforts,
                service_tiers,
            })
        })
        .collect())
}

/// `sanitizeModels`: the `debug models` catalog, keeping listed models.
fn sanitize_models(value: Option<&Value>) -> anyhow::Result<Vec<Model>> {
    let keys = ModelKeys {
        slug: "slug",
        display_name: "display_name",
        default_effort: "default_reasoning_level",
        efforts: "supported_reasoning_levels",
        effort: "effort",
        tiers: "service_tiers",
    };
    sanitize(value, &keys, |model| {
        model
            .get("visibility")
            .is_none_or(|visibility| visibility == "list")
    })
}

/// `sanitizeAppServerModels`: the app-server `model/list` catalog, skipping hidden models.
fn sanitize_app_server_models(value: Option<&Value>) -> anyhow::Result<Vec<Model>> {
    let keys = ModelKeys {
        slug: "model",
        display_name: "displayName",
        default_effort: "defaultReasoningEffort",
        efforts: "supportedReasoningEfforts",
        effort: "reasoningEffort",
        tiers: "serviceTiers",
    };
    sanitize(value, &keys, |model| {
        model.get("hidden") != Some(&Value::Bool(true))
    })
}

/// The skills of `skills/list` entries.
pub fn entry_skills(entries: &[Value]) -> impl Iterator<Item = &Map<String, Value>> {
    entries
        .iter()
        .filter_map(|entry| entry.get("skills")?.as_array())
        .flatten()
        .filter_map(Value::as_object)
}

/// `interface.displayName`, trimmed.
pub fn display_name(skill: &Map<String, Value>) -> Option<String> {
    trimmed(
        skill
            .get("interface")
            .and_then(|interface| interface.get("displayName")),
    )
}

/// `sanitizeSkills`: enabled skills, unique by name and sorted by label.
fn sanitize_skills(entries: &[Value]) -> Vec<Skill> {
    let mut skills: Vec<Skill> = Vec::new();
    for skill in entry_skills(entries) {
        if skill.get("enabled") == Some(&Value::Bool(false)) {
            continue;
        }
        let Some(id) = trimmed(skill.get("name")) else {
            continue;
        };
        if skills.iter().any(|known| known.id == id) {
            continue;
        }
        let text = |key: &str| {
            skill
                .get(key)
                .and_then(Value::as_str)
                .map(|text| js::trim(text).to_owned())
                .unwrap_or_default()
        };
        let scope = skill
            .get("scope")
            .and_then(Value::as_str)
            .filter(|scope| ["user", "repo", "system", "admin"].contains(scope));
        skills.push(Skill {
            label: display_name(skill).unwrap_or_else(|| id.clone()),
            id,
            description: text("description"),
            path: text("path"),
            scope: scope.unwrap_or("user").to_owned(),
        });
    }
    skills.sort_by(|left, right| js::locale_compare(&left.label, &right.label));
    skills
}

/// `loadSlashCommands`: the bundled Codex slash command catalog for this platform.
pub fn slash_commands() -> anyhow::Result<Vec<SlashCommand>> {
    let commands = SLASH_COMMANDS
        .as_ref()
        .and_then(|catalog| catalog.get("commands")?.as_array())
        .context("Codex slash command catalog is invalid")?;
    let lists = |command: &Value, key: &str| {
        command
            .get(key)
            .and_then(Value::as_array)
            .map(|list| list.iter().any(|item| item == PLATFORM))
    };
    Ok(commands
        .iter()
        .filter_map(|command| {
            let id = command.get("id")?.as_str().filter(|id| is_command_id(id))?;
            let description = command.get("description")?.as_str()?;
            if command.get("debugOnly") == Some(&Value::Bool(true))
                || lists(command, "platforms") == Some(false)
                || lists(command, "excludedPlatforms") == Some(true)
            {
                return None;
            }
            let space = if command.get("supportsInlineArgs") == Some(&Value::Bool(true)) {
                " "
            } else {
                ""
            };
            Some(SlashCommand {
                id: id.to_owned(),
                label: format!("/{id}"),
                description: description.to_owned(),
                insert_text: format!("/{id}{space}"),
            })
        })
        .collect())
}

/// `unavailableAiCatalog`: the empty catalog reported when the Codex CLI is not installed.
pub fn unavailable() -> Catalog {
    Catalog {
        available: Some(false),
        models: Vec::new(),
        skills: Vec::new(),
        commands: Vec::new(),
        sandboxes: AI_SANDBOX_MODES.to_vec(),
    }
}

/// Whether a catalog failure means the Codex CLI (or the workspace it runs in) is missing.
pub fn is_missing_cli(error: &anyhow::Error) -> bool {
    error
        .downcast_ref::<std::io::Error>()
        .is_some_and(|error| error.kind() == std::io::ErrorKind::NotFound)
}

/// `discoverAiCatalog`: `codex debug models` and a one-off app-server skill listing.
pub async fn discover(executable: &str, workspace: &str) -> anyhow::Result<Catalog> {
    let models = async {
        let mut command = broadcast_codex::command(executable);
        command
            .args(["debug", "models"])
            .current_dir(workspace)
            .stdin(Stdio::null())
            .kill_on_drop(true);
        let output = tokio::time::timeout(CATALOG_TIMEOUT, command.output())
            .await
            .map_err(|_| anyhow!("Timed out while reading Codex models"))??;
        if output.stdout.len() > CATALOG_MAX_BUFFER {
            bail!("Codex model catalog exceeded the catalog size limit");
        }
        if !output.status.success() {
            bail!(
                "Codex could not list models ({})",
                app_server::exit_label(&output.status)
            );
        }
        let catalog: Value = serde_json::from_slice(&output.stdout)?;
        sanitize_models(catalog.get("models"))
    };
    let (models, entries) = tokio::try_join!(models, list_skills_once(executable, workspace))?;
    Ok(Catalog {
        available: None,
        models,
        skills: sanitize_skills(&entries),
        commands: slash_commands()?,
        sandboxes: AI_SANDBOX_MODES.to_vec(),
    })
}

/// `discoverAppServerAiCatalog`: the same catalog from a running app-server.
pub async fn discover_app_server(server: &AppServer, workspace: &str) -> anyhow::Result<Catalog> {
    let params = json!({ "cursor": null, "limit": 100, "includeHidden": false });
    let (models, entries) = tokio::try_join!(
        server.request("model/list", params),
        server.list_skills(workspace, false)
    )?;
    Ok(Catalog {
        available: None,
        models: sanitize_app_server_models(models.get("data"))?,
        skills: sanitize_skills(&entries),
        commands: slash_commands()?,
        sandboxes: AI_SANDBOX_MODES.to_vec(),
    })
}

/// `listSkills`: a short-lived `codex app-server --stdio` in the workspace that lists its skills.
async fn list_skills_once(executable: &str, workspace: &str) -> anyhow::Result<Vec<Value>> {
    let mut command = broadcast_codex::command(executable);
    command
        .args(["app-server", "--stdio"])
        .current_dir(workspace)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let mut child = command.spawn()?;
    let mut stdin = child
        .stdin
        .take()
        .context("app-server stdin is unavailable")?;
    let stdout = child
        .stdout
        .take()
        .context("app-server stdout is unavailable")?;
    let result = tokio::time::timeout(
        CATALOG_TIMEOUT,
        converse(&mut child, &mut stdin, stdout, workspace),
    )
    .await
    .unwrap_or_else(|_| Err(anyhow!("Timed out while reading Codex skills")));
    drop(stdin);
    app_server::signal(child.id(), false);
    tokio::spawn(async move {
        let _ = child.wait().await;
    });
    result
}

async fn converse(
    child: &mut Child,
    stdin: &mut ChildStdin,
    stdout: ChildStdout,
    workspace: &str,
) -> anyhow::Result<Vec<Value>> {
    async fn send(stdin: &mut ChildStdin, message: Value) -> std::io::Result<()> {
        stdin.write_all(format!("{message}\n").as_bytes()).await
    }
    let initialize = json!({
        "id": 1,
        "method": "initialize",
        "params": {
            "clientInfo": { "name": "broadcast", "version": "0.1.0" },
            "capabilities": { "experimentalApi": true },
        },
    });
    send(stdin, initialize).await?;
    let mut reader = BufReader::new(stdout);
    let mut line = Vec::new();
    loop {
        match exec::read_line(&mut reader, &mut line, CATALOG_MAX_BUFFER).await? {
            Line::Read => {}
            Line::TooLong => bail!("Codex skills response exceeded the catalog size limit"),
            Line::Eof => {
                let status = child.wait().await?;
                bail!(
                    "Codex app-server exited before listing skills ({})",
                    app_server::exit_label(&status)
                );
            }
        }
        let Ok(message) = serde_json::from_slice::<Value>(&line) else {
            continue;
        };
        let failed = message.get("error").is_some_and(js::truthy);
        match message.get("id").and_then(Value::as_f64) {
            Some(1.0) => {
                if failed {
                    bail!("Codex app-server rejected initialization");
                }
                send(stdin, json!({ "method": "initialized" })).await?;
                let list = json!({ "id": 2, "method": "skills/list", "params": { "cwds": [workspace], "forceReload": false } });
                send(stdin, list).await?;
            }
            Some(2.0) => {
                if failed {
                    bail!("Codex app-server could not list skills");
                }
                return Ok(message
                    .pointer("/result/data")
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default());
            }
            _ => {}
        }
    }
}
