//! `/api/local/jira-connection`: the Jira connection form and manual sync.

use std::sync::Arc;

use axum::extract::{Request, State};
use broadcast_domain::constants::JIRA_PROJECT_ID;
use broadcast_domain::fields::{self, StringRules};
use broadcast_domain::{ApiError, Error, js};
use serde_json::json;

use crate::App;
use crate::http::{Reply, assert_empty_body, json, method_not_allowed, read_json, reject_query};
use crate::jira;

pub async fn connection(State(app): State<Arc<App>>, request: Request) -> Reply {
    reject_query(request.uri(), "Jira 连接接口不接受查询参数")?;
    match request.method().as_str() {
        "GET" => Ok(json(200, &json!({ "connection": jira::status(&app).await? }))),
        "PUT" => {
            if app.cloud.read().await.map_err(Error::from)?.remote_url.is_some() {
                let message = "Jira 连接当前仅支持本地数据模式，请先退出云端协作模式";
                return Err(ApiError::new(409, "JIRA_LOCAL_MODE_REQUIRED", message).into());
            }
            let body = read_json(request).await?;
            let object = fields::object(&body, &["baseUrl", "username", "password", "projects"])?;
            let base_url = fields::required_string(object.get("baseUrl"), "baseUrl", 2048)?;
            let username = fields::string_field(
                Some(object.get("username").filter(|value| !value.is_null()).unwrap_or(&json!(""))),
                "username",
                StringRules { max_length: Some(254), ..StringRules::default() },
            )?
            .flatten()
            .unwrap_or_default();
            let password = match object.get("password").filter(|value| !value.is_null()) {
                None => String::new(),
                Some(value) => {
                    let password = value.as_str().ok_or_else(|| ApiError::invalid_field("'password' must be a string"))?;
                    if js::len(password) > 4096 {
                        return Err(ApiError::invalid_field("'password' cannot exceed 4096 characters").into());
                    }
                    password.to_owned()
                }
            };
            let input = jira::Input { base_url, username, password, projects: object.get("projects").cloned() };
            let connection = jira::configure(&app, input).await?;
            emit_labels(&app).await?;
            Ok(json(200, &json!({ "connection": connection })))
        }
        _ => method_not_allowed(&["GET", "PUT"]),
    }
}

pub async fn sync(State(app): State<Arc<App>>, request: Request) -> Reply {
    if request.method() != "POST" {
        return method_not_allowed(&["POST"]);
    }
    reject_query(request.uri(), "Jira 同步接口不接受查询参数")?;
    assert_empty_body(request, "POST /api/local/jira-connection/sync").await?;
    let connection = jira::sync(&app, true).await?;
    emit_labels(&app).await?;
    Ok(json(200, &json!({ "connection": connection })))
}

async fn emit_labels(app: &Arc<App>) -> broadcast_domain::Result<()> {
    let project = app.store(|store| store.get_project(JIRA_PROJECT_ID)).await?;
    app.events.emit("project.labels.updated", json!({ "project": project }));
    Ok(())
}
