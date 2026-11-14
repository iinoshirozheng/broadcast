//! `jira-connection.json`: the Jira Data Center connection this device syncs assigned issues from.

use std::path::PathBuf;

use broadcast_domain::js;
use serde::Serialize;
use serde_json::Value;
use tokio::sync::Mutex;
use url::Url;

use crate::files;

const VERSION: u64 = 2;
const URL_RULES: &str = "Jira 地址必须使用 http 或 https，且不能包含账号、查询参数或片段";

#[derive(Debug)]
pub struct ConfigError {
    pub code: &'static str,
    pub message: String,
}

impl ConfigError {
    fn new(code: &'static str, message: &str) -> Self {
        Self { code, message: message.to_owned() }
    }

    fn invalid(message: &str) -> Self {
        Self::new("INVALID_JIRA_CONFIG", message)
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
        Self { code: "INVALID_JIRA_CONFIG", message: error.to_string() }
    }
}

/// A validated connection before the Jira instance and user are known.
#[derive(Debug, Clone)]
pub struct Candidate {
    pub base_url: String,
    pub username: String,
    pub password: String,
    pub projects: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct JiraConfig {
    version: u64,
    pub base_url: String,
    pub username: String,
    pub password: String,
    /// sha256 hex of the instance's application-link id.
    pub origin_id: String,
    pub display_name: String,
    pub projects: Vec<String>,
}

impl JiraConfig {
    pub fn new(candidate: Candidate, origin_id: String, display_name: &str) -> Self {
        Self {
            version: VERSION,
            base_url: candidate.base_url,
            username: candidate.username,
            password: candidate.password,
            origin_id,
            display_name: js::prefix(js::trim(display_name), 254).to_owned(),
            projects: candidate.projects,
        }
    }
}

/// `normalizeJiraUrl`: an http(s) URL without credentials, query or fragment, minus trailing slashes.
pub fn normalize_url(value: Option<&Value>) -> Result<String, ConfigError> {
    let rules = || ConfigError::new("INVALID_JIRA_URL", URL_RULES);
    let value = value.and_then(Value::as_str).filter(|value| !value.contains(['?', '#'])).ok_or_else(rules)?;
    let mut url = Url::parse(value).map_err(|_| ConfigError::new("INVALID_JIRA_URL", "Jira 地址无效"))?;
    if !matches!(url.scheme(), "http" | "https") || !url.username().is_empty() || url.password().is_some() {
        return Err(rules());
    }
    let path = url.path().trim_end_matches('/').to_owned();
    url.set_path(&path);
    let text = url.as_str();
    Ok(text.strip_suffix('/').unwrap_or(text).to_owned())
}

fn credentials(username: Option<&Value>, password: Option<&Value>) -> Result<(String, String), ConfigError> {
    let username = username
        .and_then(Value::as_str)
        .filter(|name| !js::trim(name).is_empty() && js::len(name) <= 254 && !name.contains(':'))
        .ok_or_else(|| {
            ConfigError::new("INVALID_JIRA_USERNAME", "Jira 用户名不能为空、不能包含冒号，且不能超过 254 个字符")
        })?;
    let password = password
        .and_then(Value::as_str)
        .filter(|password| !password.is_empty() && js::len(password) <= 4096)
        .ok_or_else(|| ConfigError::new("INVALID_JIRA_PASSWORD", "Jira 密码不能为空且不能超过 4096 个字符"))?;
    Ok((js::trim(username).to_owned(), password.to_owned()))
}

fn projects(value: Option<&Value>) -> Result<Vec<String>, ConfigError> {
    let Some(value) = value else { return Ok(Vec::new()) };
    let items = value
        .as_array()
        .filter(|items| items.len() <= 20)
        .ok_or_else(|| ConfigError::new("INVALID_JIRA_PROJECTS", "Jira 项目必须是最多 20 项的数组"))?;
    let mut projects: Vec<String> = Vec::new();
    for item in items {
        let project = item
            .as_str()
            .filter(|project| {
                !js::trim(project).is_empty() && js::len(project) <= 128 && !project.chars().any(|c| c <= '\u{1f}' || c == '\u{7f}')
            })
            .ok_or_else(|| {
                ConfigError::new(
                    "INVALID_JIRA_PROJECTS",
                    "Jira 项目名称或 Key 不能为空、不能包含控制字符，且不能超过 128 个字符",
                )
            })?;
        let project = js::trim(project).to_owned();
        if !projects.contains(&project) {
            projects.push(project);
        }
    }
    Ok(projects)
}

/// `validate`: the connection fields a user submits.
pub fn validate(base_url: Option<&Value>, username: Option<&Value>, password: Option<&Value>, projects_value: Option<&Value>) -> Result<Candidate, ConfigError> {
    let base_url = normalize_url(base_url)?;
    let (username, password) = credentials(username, password)?;
    Ok(Candidate { base_url, username, password, projects: projects(projects_value)? })
}

fn parse(value: &Value) -> Result<JiraConfig, ConfigError> {
    let object = value
        .as_object()
        .filter(|object| object.get("version").and_then(Value::as_u64) == Some(VERSION))
        .ok_or_else(|| ConfigError::invalid("Jira 配置文件无效"))?;
    const KEYS: [&str; 7] = ["version", "baseUrl", "username", "password", "originId", "displayName", "projects"];
    if object.keys().any(|key| !KEYS.contains(&key.as_str())) {
        return Err(ConfigError::invalid("Jira 配置文件包含未知字段"));
    }
    let (username, password) = credentials(object.get("username"), object.get("password"))?;
    let origin_id = object
        .get("originId")
        .and_then(Value::as_str)
        .filter(|id| id.len() == 64 && id.bytes().all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)))
        .ok_or_else(|| ConfigError::invalid("Jira 配置缺少稳定实例身份"))?;
    let display_name = object
        .get("displayName")
        .and_then(Value::as_str)
        .filter(|name| !js::trim(name).is_empty())
        .ok_or_else(|| ConfigError::invalid("Jira 配置缺少用户显示名称"))?;
    let candidate = Candidate {
        base_url: normalize_url(object.get("baseUrl"))?,
        username,
        password,
        projects: projects(object.get("projects"))?,
    };
    Ok(JiraConfig::new(candidate, origin_id.to_owned(), display_name))
}

/// Serializes reads and writes of the file.
pub struct ConfigStore {
    path: PathBuf,
    lock: Mutex<()>,
}

impl ConfigStore {
    pub fn new(path: PathBuf) -> Self {
        Self { path, lock: Mutex::new(()) }
    }

    pub async fn read(&self) -> Result<Option<JiraConfig>, ConfigError> {
        let _guard = self.lock.lock().await;
        files::read_json(&self.path).await?.as_ref().map(parse).transpose()
    }

    pub async fn save(&self, config: &JiraConfig) -> Result<(), ConfigError> {
        let _guard = self.lock.lock().await;
        files::write_private(&self.path, config, true).await?;
        Ok(())
    }
}
