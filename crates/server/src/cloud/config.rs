//! `cloud-companion.json`: the cloud board this device mirrors and its per-project workspaces.

use std::path::{Path, PathBuf};

use broadcast_domain::js;
use serde::Serialize;
use serde_json::{Map, Value};
use tokio::sync::Mutex;
use url::Url;

use crate::files;

const VERSION: u64 = 1;

#[derive(Debug)]
pub struct ConfigError {
    pub code: &'static str,
    pub message: String,
}

impl ConfigError {
    fn new(code: &'static str, message: &str) -> Self {
        Self { code, message: message.to_owned() }
    }

    fn invalid() -> Self {
        Self::new("INVALID_CLOUD_CONFIG", "Cloud companion configuration is invalid")
    }
}

impl std::fmt::Display for ConfigError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}: {}", self.code, self.message)
    }
}

impl std::error::Error for ConfigError {}

impl From<std::io::Error> for ConfigError {
    fn from(error: std::io::Error) -> Self {
        Self { code: "INVALID_CLOUD_CONFIG", message: error.to_string() }
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CloudConfig {
    version: u64,
    pub remote_url: Option<String>,
    pub actor_name: Option<String>,
    pub shared_key: Option<String>,
    /// Project id → absolute workspace path on this device.
    pub project_mappings: Map<String, Value>,
}

impl Default for CloudConfig {
    fn default() -> Self {
        Self { version: VERSION, remote_url: None, actor_name: None, shared_key: None, project_mappings: Map::new() }
    }
}

impl CloudConfig {
    pub fn workspace(&self, project_id: &str) -> Option<&str> {
        self.project_mappings.get(project_id).and_then(Value::as_str)
    }
}

/// `normalizeCloudUrl`: an HTTPS origin, or loopback HTTP for development.
pub fn normalize_url(value: Option<&Value>) -> Result<String, ConfigError> {
    let url = value
        .and_then(Value::as_str)
        .and_then(|value| Url::parse(value).ok())
        .ok_or_else(|| ConfigError::new("INVALID_CLOUD_URL", "Cloud broadcast URL must be a valid URL"))?;
    let loopback = matches!(url.host_str(), Some("localhost" | "127.0.0.1" | "[::1]"));
    let scheme_ok = url.scheme() == "https" || (url.scheme() == "http" && loopback);
    let exact = url.username().is_empty()
        && url.password().is_none()
        && url.path() == "/"
        && url.query().is_none_or(str::is_empty)
        && url.fragment().is_none_or(str::is_empty);
    if !scheme_ok || !exact {
        return Err(ConfigError::new(
            "INVALID_CLOUD_URL",
            "Cloud broadcast URL must be an HTTPS origin (loopback HTTP is allowed for development)",
        ));
    }
    Ok(url.origin().ascii_serialization())
}

fn credentials(actor_name: Option<&Value>, shared_key: Option<&Value>) -> Result<(String, String), ConfigError> {
    let actor = actor_name
        .and_then(Value::as_str)
        .filter(|name| !js::trim(name).is_empty() && js::len(name) <= 120 && !name.contains(':'))
        .ok_or_else(|| {
            ConfigError::new("INVALID_CLOUD_ACTOR", "Cloud actor name must be 1 to 120 characters and cannot contain ':'")
        })?;
    let key = shared_key
        .and_then(Value::as_str)
        .filter(|key| !key.is_empty() && js::len(key) <= 4096)
        .ok_or_else(|| ConfigError::new("INVALID_CLOUD_KEY", "Cloud shared key must be 1 to 4096 characters"))?;
    Ok((js::trim(actor).to_owned(), key.to_owned()))
}

fn parse(value: &Value) -> Result<CloudConfig, ConfigError> {
    let object = value.as_object().ok_or_else(ConfigError::invalid)?;
    const KEYS: [&str; 5] = ["version", "remoteUrl", "actorName", "sharedKey", "projectMappings"];
    if object.get("version").and_then(Value::as_u64) != Some(VERSION) || object.keys().any(|key| !KEYS.contains(&key.as_str())) {
        return Err(ConfigError::invalid());
    }
    let mappings = object.get("projectMappings").and_then(Value::as_object).ok_or_else(ConfigError::invalid)?;
    let valid = |(id, path): (&String, &Value)| !id.is_empty() && path.as_str().is_some_and(|path| Path::new(path).is_absolute());
    if !mappings.iter().all(valid) {
        return Err(ConfigError::new("INVALID_CLOUD_CONFIG", "Cloud project mappings are invalid"));
    }
    let project_mappings = mappings.clone();
    let null = |key: &str| object.get(key).is_some_and(Value::is_null);
    if null("remoteUrl") && null("actorName") && null("sharedKey") {
        return Ok(CloudConfig { project_mappings, ..CloudConfig::default() });
    }
    let (actor_name, shared_key) = credentials(object.get("actorName"), object.get("sharedKey"))?;
    Ok(CloudConfig {
        version: VERSION,
        remote_url: Some(normalize_url(object.get("remoteUrl"))?),
        actor_name: Some(actor_name),
        shared_key: Some(shared_key),
        project_mappings,
    })
}

/// Serializes every read-modify-write of the file.
pub struct ConfigStore {
    path: PathBuf,
    lock: Mutex<()>,
}

impl ConfigStore {
    pub fn new(path: PathBuf) -> Self {
        Self { path, lock: Mutex::new(()) }
    }

    async fn load(&self) -> Result<CloudConfig, ConfigError> {
        match files::read_json(&self.path).await? {
            Some(value) => parse(&value),
            None => Ok(CloudConfig::default()),
        }
    }

    pub async fn read(&self) -> Result<CloudConfig, ConfigError> {
        let _guard = self.lock.lock().await;
        self.load().await
    }

    async fn update(&self, change: impl FnOnce(&mut CloudConfig)) -> Result<CloudConfig, ConfigError> {
        let _guard = self.lock.lock().await;
        let mut config = self.load().await?;
        change(&mut config);
        files::write_private(&self.path, &config, true).await?;
        Ok(config)
    }

    pub async fn configure(&self, remote_url: Option<&Value>, actor_name: Option<&Value>, shared_key: Option<&Value>) -> Result<CloudConfig, ConfigError> {
        let url = normalize_url(remote_url)?;
        let (actor, key) = credentials(actor_name, shared_key)?;
        self.update(|config| {
            config.remote_url = Some(url);
            config.actor_name = Some(actor);
            config.shared_key = Some(key);
        })
        .await
    }

    pub async fn clear(&self) -> Result<CloudConfig, ConfigError> {
        self.update(|config| {
            config.remote_url = None;
            config.actor_name = None;
            config.shared_key = None;
        })
        .await
    }

    pub async fn set_project_workspace(&self, project_id: &str, workspace: &str) -> Result<CloudConfig, ConfigError> {
        self.update(|config| {
            config.project_mappings.insert(project_id.to_owned(), Value::String(workspace.to_owned()));
        })
        .await
    }
}
