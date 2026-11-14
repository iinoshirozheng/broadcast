//! Jira issues as Broadcast tasks: status, priority and actor mapping, and the assigned-issues JQL.

use broadcast_domain::js;
use broadcast_store::{JiraActor, JiraTask};
use serde_json::Value;
use sha2::{Digest, Sha256};

use super::config::JiraConfig;

pub const FIELDS: [&str; 10] =
    ["summary", "description", "status", "priority", "labels", "duedate", "assignee", "reporter", "created", "updated"];

fn quote(value: &str) -> String {
    format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\""))
}

/// `buildJiraJql`: open issues assigned to the connected user, plus those updated in the last 30 days.
pub fn jql(projects: &[String]) -> String {
    let filter = if projects.is_empty() {
        String::new()
    } else {
        format!(" AND project in ({})", projects.iter().map(|project| quote(project)).collect::<Vec<_>>().join(", "))
    };
    format!("assignee = currentUser(){filter} AND (statusCategory != Done OR updated >= -30d) ORDER BY updated DESC")
}

/// `String(value)` for the JSON values Jira returns; `None` for null or missing.
pub fn text(value: &Value) -> Option<String> {
    match value {
        Value::Null => None,
        Value::String(text) => Some(text.clone()),
        Value::Number(number) => Some(js::number(number.as_f64().unwrap_or_default()).to_string()),
        Value::Bool(value) => Some(value.to_string()),
        Value::Array(_) | Value::Object(_) => Some("[object Object]".to_owned()),
    }
}

/// `limitedString`: trimmed, falling back when empty, cut to `max` UTF-16 units.
fn limited(value: &Value, fallback: &str, max: usize) -> String {
    let result = text(value).unwrap_or_else(|| fallback.to_owned());
    let result = js::trim(&result);
    js::prefix(if result.is_empty() { fallback } else { result }, max).to_owned()
}

fn includes_any(value: &Value, terms: &[&str]) -> bool {
    let name = text(value).unwrap_or_default().to_lowercase();
    terms.iter().any(|term| name.contains(term))
}

/// `taskStatusFromJira`: the status category decides, refined by the status name.
pub fn status(status: &Value) -> &'static str {
    let name = &status["name"];
    match status["statusCategory"]["key"].as_str() {
        Some("done") if includes_any(name, &["cancel", "reject", "取消", "拒绝"]) => "canceled",
        Some("done") => "done",
        Some("new") if includes_any(name, &["backlog", "待立项", "需求池"]) => "backlog",
        Some("new") => "todo",
        _ if includes_any(name, &["review", "verify", "test", "验收", "评审", "测试"]) => "in_review",
        _ if includes_any(name, &["block", "hold", "阻塞", "挂起"]) => "blocked",
        _ => "in_progress",
    }
}

/// `taskPriorityFromJira`.
pub fn priority(priority: &Value) -> &'static str {
    let name = &priority["name"];
    if includes_any(name, &["highest", "critical", "blocker", "urgent", "紧急", "最高"]) {
        "urgent"
    } else if includes_any(name, &["high", "major", "高"]) {
        "high"
    } else if includes_any(name, &["medium", "normal", "中"]) {
        "medium"
    } else if includes_any(name, &["low", "minor", "trivial", "低"]) {
        "low"
    } else {
        "none"
    }
}

fn first_present<'a>(user: &'a Value, keys: &[&str]) -> &'a Value {
    keys.iter().map(|key| &user[*key]).find(|value| !value.is_null()).unwrap_or(&Value::Null)
}

fn actor(user: &Value, fallback: &str) -> JiraActor {
    let avatars = &user["avatarUrls"];
    JiraActor {
        id: format!("jira:{}", limited(first_present(user, &["key", "name", "accountId"]), fallback, 240)),
        name: limited(first_present(user, &["displayName", "name"]), fallback, 120),
        avatar_url: text(first_present(avatars, &["48x48", "32x32"])),
    }
}

/// `jiraOriginId`: sha256 hex of the application-link id, stable across URL changes.
pub fn origin_id(manifest: &Value) -> Option<String> {
    let id = manifest["id"].as_str().map(js::trim).filter(|id| !id.is_empty())?;
    Some(Sha256::digest(id.as_bytes()).iter().map(|byte| format!("{byte:02x}")).collect())
}

/// `normalizeIssue`: `index` orders tasks as Jira returned them.
pub fn normalize(issue: &Value, config: &JiraConfig, index: usize) -> JiraTask {
    let fields = &issue["fields"];
    let external_id = text(&issue["id"]).unwrap_or_else(|| "undefined".to_owned());
    let external_key = limited(&issue["key"], "JIRA", 128);
    let id = format!("JIRA:{}:{external_id}", config.origin_id.to_uppercase());
    let mut labels: Vec<String> = Vec::new();
    for label in fields["labels"].as_array().into_iter().flatten().filter_map(Value::as_str) {
        let label = js::prefix(js::trim(label), 64);
        if !label.is_empty() && !labels.iter().any(|existing| existing == label) {
            labels.push(label.to_owned());
        }
    }
    labels.truncate(20);
    let string = |key: &str| fields[key].as_str().map(str::to_owned);
    JiraTask {
        title: limited(&fields["summary"], &external_key, 240),
        description: fields["description"].as_str().map(|text| js::prefix(text, 100_000).to_owned()).unwrap_or_default(),
        status: status(&fields["status"]),
        priority: priority(&fields["priority"]),
        labels,
        sort_order: ((index + 1) * 1024) as f64,
        creator: actor(&fields["reporter"], &config.display_name),
        assignee: actor(&fields["assignee"], &config.display_name),
        due_date: string("duedate"),
        external_origin: config.origin_id.clone(),
        external_url: format!("{}/browse/{}", config.base_url, js::encode_uri_component(&external_key)),
        external_id,
        external_key,
        created_at: string("created").unwrap_or_else(broadcast_store::now),
        updated_at: string("updated").unwrap_or_else(broadcast_store::now),
        id,
    }
}
