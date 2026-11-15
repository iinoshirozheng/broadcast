//! Jira Data Center integration: issues assigned to the connected user mirror into the
//! `jira-my-tasks` project, and edits there write through to Jira.

pub mod config;
mod issue;

use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use broadcast_domain::js::encode_uri_component;
use broadcast_domain::records::{Task, TaskChanges};
use broadcast_domain::{ApiError, Error, Result};
use reqwest::{Method, header};
use serde_json::{Map, Value, json};

use crate::App;
use config::{Candidate, ConfigError, ConfigStore, JiraConfig};

const SYNC_INTERVAL: Duration = Duration::from_secs(60);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(20);

pub struct Jira {
    config: ConfigStore,
    client: reqwest::Client,
    /// Runs configure, sync and reconcile one at a time.
    operation: tokio::sync::Mutex<()>,
    last_synced: Mutex<Option<(String, Instant)>>,
}

impl Jira {
    pub fn new(path: std::path::PathBuf) -> reqwest::Result<Self> {
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .no_proxy()
            .timeout(REQUEST_TIMEOUT)
            .build()?;
        Ok(Self {
            config: ConfigStore::new(path),
            client,
            operation: tokio::sync::Mutex::new(()),
            last_synced: Mutex::new(None),
        })
    }

    fn last_synced(&self) -> Option<(String, Instant)> {
        self.last_synced
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    fn mark_synced(&self) -> String {
        let at = broadcast_store::now();
        *self
            .last_synced
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some((at.clone(), Instant::now()));
        at
    }

    async fn read(&self) -> Result<Option<JiraConfig>> {
        self.config.read().await.map_err(Error::from)
    }
}

/// `safeConfig`: the connection without its credentials.
fn safe(config: Option<&JiraConfig>, last_synced_at: Option<String>) -> Value {
    let project_id = broadcast_domain::constants::JIRA_PROJECT_ID;
    match config {
        Some(config) => json!({
            "configured": true,
            "baseUrl": config.base_url,
            "username": null,
            "displayName": config.display_name,
            "projects": config.projects,
            "projectId": project_id,
            "lastSyncedAt": last_synced_at,
            "insecureHttp": config.base_url.starts_with("http:"),
        }),
        None => json!({
            "configured": false,
            "baseUrl": null,
            "username": null,
            "displayName": null,
            "projects": [],
            "projectId": project_id,
            "lastSyncedAt": null,
            "insecureHttp": false,
        }),
    }
}

/// The address and Basic credentials of one Jira instance.
struct Site<'a> {
    base_url: &'a str,
    username: &'a str,
    password: &'a str,
}

impl Candidate {
    fn site(&self) -> Site<'_> {
        Site {
            base_url: &self.base_url,
            username: &self.username,
            password: &self.password,
        }
    }
}

impl JiraConfig {
    fn site(&self) -> Site<'_> {
        Site {
            base_url: &self.base_url,
            username: &self.username,
            password: &self.password,
        }
    }
}

/// One Jira REST call; redirects and HTTP errors become Broadcast errors.
async fn request(
    app: &App,
    site: &Site<'_>,
    method: Method,
    path: &str,
    body: Option<Value>,
) -> Result<Value> {
    let credentials = STANDARD.encode(format!("{}:{}", site.username, site.password));
    let mut builder = app
        .jira
        .client
        .request(method, format!("{}{path}", site.base_url))
        .header(header::ACCEPT, "application/json")
        .header(header::AUTHORIZATION, format!("Basic {credentials}"));
    if let Some(body) = body {
        builder = builder
            .header(header::CONTENT_TYPE, "application/json")
            .body(body.to_string());
    }
    let response = builder.send().await.map_err(|error| {
        if error.is_timeout() {
            ApiError::new(502, "JIRA_TIMEOUT", "连接 Jira 超时")
        } else {
            ApiError::new(
                502,
                "JIRA_UNAVAILABLE",
                "无法连接 Jira，请检查地址和内网连接",
            )
        }
    })?;
    let status = response.status().as_u16();
    if status == 401 || status == 403 {
        return Err(ApiError::new(
            401,
            "JIRA_AUTH_FAILED",
            "Jira 登录失败，请检查用户名、密码、Basic Auth 或 CAPTCHA 状态",
        )
        .into());
    }
    if (300..400).contains(&status) {
        return Err(ApiError::new(
            400,
            "JIRA_REDIRECT",
            "Jira 地址发生重定向，请填写最终访问地址",
        )
        .into());
    }
    if !response.status().is_success() {
        let code = if status >= 500 { 502 } else { 409 };
        return Err(ApiError::new(
            code,
            "JIRA_REQUEST_FAILED",
            format!("Jira 请求失败（HTTP {status}）"),
        )
        .into());
    }
    if status == 204 {
        return Ok(Value::Null);
    }
    let invalid = || ApiError::new(502, "INVALID_JIRA_RESPONSE", "Jira 返回了无效的 JSON 数据");
    let bytes = response.bytes().await.map_err(|_| invalid())?;
    Ok(serde_json::from_slice(&bytes).map_err(|_| invalid())?)
}

/// `fetchAssignedIssues`: Jira Cloud pages by token, Data Center by offset.
async fn assigned_issues(app: &App, site: &Site<'_>, projects: &[String]) -> Result<Vec<Value>> {
    let jql = issue::jql(projects);
    let cloud = url::Url::parse(site.base_url)
        .ok()
        .and_then(|url| url.host_str().map(|host| host.ends_with(".atlassian.net")));
    let mut issues = Vec::new();
    if cloud == Some(true) {
        let mut next_page_token: Option<String> = None;
        loop {
            let mut body = json!({ "jql": jql, "maxResults": 100, "fields": issue::FIELDS });
            if let Some(token) = &next_page_token {
                body["nextPageToken"] = json!(token);
            }
            let page = request(
                app,
                site,
                Method::POST,
                "/rest/api/2/search/jql",
                Some(body),
            )
            .await?;
            issues.extend(page["issues"].as_array().into_iter().flatten().cloned());
            if page["isLast"] == Value::Bool(true) {
                return Ok(issues);
            }
            let token = page["nextPageToken"]
                .as_str()
                .filter(|token| !broadcast_domain::js::trim(token).is_empty());
            let Some(token) = token else {
                return Err(ApiError::new(
                    502,
                    "INVALID_JIRA_RESPONSE",
                    "Jira 搜索结果缺少下一页标记",
                )
                .into());
            };
            next_page_token = Some(token.to_owned());
        }
    }
    loop {
        let body = json!({ "jql": jql, "startAt": issues.len(), "maxResults": 100, "fields": issue::FIELDS });
        let page = request(app, site, Method::POST, "/rest/api/2/search", Some(body)).await?;
        let page_issues = page["issues"].as_array().cloned().unwrap_or_default();
        let empty = page_issues.is_empty();
        issues.extend(page_issues);
        let total = match &page["total"] {
            Value::Null => 0.0,
            Value::Number(number) => number.as_f64().unwrap_or(f64::NAN),
            Value::String(text) => broadcast_domain::js::trim(text).parse().unwrap_or(f64::NAN),
            _ => f64::NAN,
        };
        // `startAt >= Number(total)` is false for NaN, so a malformed total keeps paging until a page is empty.
        if empty || issues.len() as f64 >= total {
            return Ok(issues);
        }
    }
}

async fn fetch_origin(app: &App, site: &Site<'_>) -> Result<String> {
    let manifest = request(app, site, Method::GET, "/rest/applinks/1.0/manifest", None).await?;
    issue::origin_id(&manifest).ok_or_else(|| {
        ApiError::new(502, "INVALID_JIRA_ORIGIN", "Jira 未返回稳定的实例身份").into()
    })
}

async fn assert_live_origin(app: &App, config: &JiraConfig) -> Result<()> {
    if fetch_origin(app, &config.site()).await? != config.origin_id {
        return Err(ApiError::new(
            409,
            "JIRA_ORIGIN_MISMATCH",
            "当前 Jira 地址指向了另一个实例，请重新连接后再操作",
        )
        .into());
    }
    Ok(())
}

/// Mirrors `issues` into the Jira project.
async fn store_issues(
    app: &Arc<App>,
    config: &JiraConfig,
    issues: &[Value],
    archive_missing: bool,
) -> Result<()> {
    let tasks: Vec<_> = issues
        .iter()
        .enumerate()
        .map(|(index, issue)| issue::normalize(issue, config, index))
        .collect();
    let project_name = format!("Jira · {}", config.display_name);
    app.store(move |store| store.sync_jira_tasks(&tasks, &project_name, archive_missing))
        .await
}

async fn sync_with(app: &Arc<App>, config: &JiraConfig, archive_missing: bool) -> Result<Value> {
    assert_live_origin(app, config).await?;
    let issues = assigned_issues(app, &config.site(), &config.projects).await?;
    store_issues(app, config, &issues, archive_missing).await?;
    Ok(safe(Some(config), Some(app.jira.mark_synced())))
}

pub async fn status(app: &App) -> Result<Value> {
    Ok(safe(
        app.jira.read().await?.as_ref(),
        app.jira.last_synced().map(|(at, _)| at),
    ))
}

/// `sync`: at most once a minute unless forced.
pub async fn sync(app: &Arc<App>, force: bool) -> Result<Value> {
    let _operation = app.jira.operation.lock().await;
    let Some(config) = app.jira.read().await? else {
        return Ok(safe(None, None));
    };
    if !force
        && let Some((at, synced)) = app.jira.last_synced()
        && synced.elapsed() < SYNC_INTERVAL
    {
        return Ok(safe(Some(&config), Some(at)));
    }
    sync_with(app, &config, true).await
}

/// The submitted connection form; an empty username or password keeps the saved one.
pub struct Input {
    pub base_url: String,
    pub username: String,
    pub password: String,
    pub projects: Option<Value>,
}

fn rejected(error: ConfigError) -> Error {
    ApiError::new(400, error.code, error.message).into()
}

/// `configure`: verifies the instance, user and issues before saving the connection.
pub async fn configure(app: &Arc<App>, input: Input) -> Result<Value> {
    let _operation = app.jira.operation.lock().await;
    let current = app.jira.config.read().await.map_err(rejected)?;
    let keep = |given: &str, saved: Option<&str>| {
        Some(given)
            .filter(|given| !given.is_empty())
            .or(saved)
            .map(|value| Value::String(value.to_owned()))
    };
    let username = keep(
        &input.username,
        current.as_ref().map(|config| config.username.as_str()),
    );
    let password = keep(
        &input.password,
        current.as_ref().map(|config| config.password.as_str()),
    );
    let base_url = Value::String(input.base_url);
    let candidate = config::validate(
        Some(&base_url),
        username.as_ref(),
        password.as_ref(),
        input.projects.as_ref(),
    )
    .map_err(rejected)?;
    let same_login = current.as_ref().is_some_and(|current| {
        current.base_url == candidate.base_url && current.username == candidate.username
    });
    if input.password.is_empty() && !same_login {
        return Err(ApiError::new(
            400,
            "JIRA_PASSWORD_REQUIRED",
            "修改 Jira 地址或用户名时必须重新输入密码",
        )
        .into());
    }
    let origin_id = fetch_origin(app, &candidate.site()).await?;
    let myself = request(
        app,
        &candidate.site(),
        Method::GET,
        "/rest/api/2/myself",
        None,
    )
    .await?;
    let display_name = [&myself["displayName"], &myself["name"]]
        .into_iter()
        .find(|name| !name.is_null())
        .and_then(issue::text)
        .unwrap_or_else(|| candidate.username.clone());
    let config = JiraConfig::new(candidate, origin_id, &display_name);
    let issues = assigned_issues(app, &config.site(), &config.projects).await?;
    store_issues(app, &config, &issues, true).await?;
    app.jira.config.save(&config).await.map_err(rejected)?;
    Ok(safe(Some(&config), Some(app.jira.mark_synced())))
}

/// `reconcile`: re-reads Jira after a write-through, without archiving.
pub async fn reconcile(app: &Arc<App>) -> Result<Value> {
    let _operation = app.jira.operation.lock().await;
    let Some(config) = app.jira.read().await? else {
        return Err(ApiError::new(409, "JIRA_NOT_CONFIGURED", "Jira 尚未完成稳定身份配置").into());
    };
    sync_with(app, &config, false).await
}

/// The saved connection, provided `task` came from it and it still points at the same instance.
async fn connection_for(app: &App, task: &Task) -> Result<(JiraConfig, String)> {
    let Some(config) = app.jira.read().await? else {
        return Err(ApiError::new(409, "JIRA_NOT_CONFIGURED", "Jira 尚未配置").into());
    };
    let key = task
        .external_key
        .clone()
        .filter(|_| task.external_origin.as_deref() == Some(config.origin_id.as_str()));
    let Some(key) = key else {
        return Err(ApiError::new(
            409,
            "JIRA_ORIGIN_MISMATCH",
            "此任务不属于当前 Jira 连接，请重新同步后再操作",
        )
        .into());
    };
    assert_live_origin(app, &config).await?;
    Ok((config, key))
}

/// `resolveTransition`: exactly one workflow transition must reach `target`.
async fn transition(app: &App, config: &JiraConfig, key: &str, target: &str) -> Result<String> {
    let path = format!(
        "/rest/api/2/issue/{}/transitions?expand=transitions.fields",
        encode_uri_component(key)
    );
    let payload = request(app, &config.site(), Method::GET, &path, None).await?;
    let transitions = payload["transitions"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    let id =
        |candidate: &Value| issue::text(&candidate["id"]).unwrap_or_else(|| "undefined".to_owned());
    let available: Vec<Value> = transitions
        .iter()
        .map(|candidate| {
            let name = [&candidate["name"], &candidate["to"]["name"]].into_iter().find(|name| !name.is_null()).and_then(issue::text);
            json!({ "id": id(candidate), "name": name.unwrap_or_default(), "broadcastStatus": issue::status(&candidate["to"]) })
        })
        .collect();
    let matches: Vec<&Value> = transitions
        .iter()
        .filter(|candidate| issue::status(&candidate["to"]) == target)
        .collect();
    match matches.as_slice() {
        [only] => Ok(id(only)),
        [] => Err(ApiError::new(
            409,
            "JIRA_TRANSITION_UNAVAILABLE",
            format!("Jira 当前工作流不能将 {key} 移到目标状态"),
        )
        .with_details(json!({ "availableStatuses": available }))
        .into()),
        _ => {
            let available: Vec<Value> = available
                .into_iter()
                .filter(|status| status["broadcastStatus"] == target)
                .collect();
            Err(ApiError::new(
                409,
                "JIRA_TRANSITION_AMBIGUOUS",
                format!("Jira 有多个工作流操作可将 {key} 移到目标状态，请在 Jira 中选择"),
            )
            .with_details(json!({ "availableStatuses": available }))
            .into())
        }
    }
}

async fn apply_transition(app: &App, config: &JiraConfig, key: &str, id: String) -> Result<()> {
    let path = format!(
        "/rest/api/2/issue/{}/transitions",
        encode_uri_component(key)
    );
    request(
        app,
        &config.site(),
        Method::POST,
        &path,
        Some(json!({ "transition": { "id": id } })),
    )
    .await?;
    Ok(())
}

/// `resolveJiraPriority`: the first Jira priority that maps to `target`; `none` clears it.
async fn priority(app: &App, config: &JiraConfig, target: &str) -> Result<Value> {
    if target == "none" {
        return Ok(Value::Null);
    }
    let priorities = request(
        app,
        &config.site(),
        Method::GET,
        "/rest/api/2/priority",
        None,
    )
    .await?;
    let found = priorities
        .as_array()
        .into_iter()
        .flatten()
        .find(|candidate| issue::priority(candidate) == target);
    let Some(found) = found else {
        return Err(ApiError::new(
            409,
            "JIRA_PRIORITY_UNAVAILABLE",
            "Jira 中没有可映射到该优先级的选项",
        )
        .into());
    };
    Ok(json!({ "id": issue::text(&found["id"]).unwrap_or_else(|| "undefined".to_owned()) }))
}

/// `updateTask`: writes the changed fields, or one status transition, to Jira; `true` when Jira changed.
pub async fn update_task(app: &App, task: &Task, changes: &TaskChanges) -> Result<bool> {
    let (config, key) = connection_for(app, task).await?;
    let status_changed = changes.status.filter(|status| *status != task.status);
    let priority_changed = changes
        .priority
        .filter(|priority| *priority != task.priority);
    let mut fields = Map::new();
    if let Some(title) = changes.title.as_ref().filter(|title| **title != task.title) {
        fields.insert("summary".to_owned(), json!(title));
    }
    if let Some(description) = changes
        .description
        .as_ref()
        .filter(|description| **description != task.description)
    {
        fields.insert("description".to_owned(), json!(description));
    }
    if let Some(labels) = changes
        .labels
        .as_ref()
        .filter(|labels| **labels != task.labels)
    {
        fields.insert("labels".to_owned(), json!(labels));
    }
    if let Some(due_date) = changes
        .due_date
        .as_ref()
        .filter(|due_date| **due_date != task.due_date)
    {
        fields.insert("duedate".to_owned(), json!(due_date));
    }
    let fields_changed = !fields.is_empty() || priority_changed.is_some();
    if status_changed.is_some() && fields_changed {
        return Err(ApiError::new(
            409,
            "JIRA_MULTI_STEP_UPDATE_UNAVAILABLE",
            "请分开修改 Jira 状态和其他字段",
        )
        .into());
    }
    if let Some(target) = priority_changed {
        fields.insert("priority".to_owned(), priority(app, &config, target).await?);
    }
    if let Some(target) = status_changed {
        let id = transition(app, &config, &key, target).await?;
        apply_transition(app, &config, &key, id).await?;
        return Ok(true);
    }
    if fields_changed {
        let path = format!("/rest/api/2/issue/{}", encode_uri_component(&key));
        request(
            app,
            &config.site(),
            Method::PUT,
            &path,
            Some(json!({ "fields": fields })),
        )
        .await?;
        return Ok(true);
    }
    Ok(false)
}

/// `moveTask`: a board move is a single Jira transition.
pub async fn move_task(app: &App, task: &Task, status: &str) -> Result<()> {
    if status == task.status {
        return Ok(());
    }
    let (config, key) = connection_for(app, task).await?;
    let id = transition(app, &config, &key, status).await?;
    apply_transition(app, &config, &key, id).await
}
