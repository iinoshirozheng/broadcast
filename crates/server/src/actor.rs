//! `actorFromRequest` and `resolveAssignee`.

use axum::http::HeaderMap;
use broadcast_domain::fields::{AssigneeTarget, required_string};
use broadcast_domain::js::decode_uri_component;
use broadcast_domain::records::Actor;
use broadcast_domain::{ApiError, Result};
use serde_json::Value;
use url::Url;

use crate::http::header;

fn invalid(message: &str) -> ApiError {
    ApiError::new(400, "INVALID_ACTOR", message)
}

/// The acting agent (`X-Broadcast-Client`) or user (`X-Broadcast-User-*`); local user by default.
pub fn from_headers(headers: &HeaderMap) -> Result<Actor, ApiError> {
    match header(headers, "x-broadcast-client").as_deref() {
        Some("broadcast") => return Ok(Actor::codex_agent()),
        Some("claude-runner") => return Ok(Actor::claude_agent()),
        Some("codex-runner") => return Ok(Actor::codex_agent()),
        _ => {}
    }
    let raw_id = header(headers, "x-broadcast-user-id");
    let raw_name = header(headers, "x-broadcast-user-name");
    let raw_avatar = header(headers, "x-broadcast-user-avatar");
    if raw_id.is_none() && raw_name.is_none() && raw_avatar.is_none() {
        return Ok(Actor::local_user());
    }
    let (Some(raw_id), Some(raw_name)) = (raw_id, raw_name) else {
        return Err(invalid("User identity requires both an ID and name"));
    };

    let id = required_string(Some(&Value::String(raw_id)), "X-Broadcast-User-Id", 96)?;
    if !is_user_id(&id) {
        return Err(invalid("User ID contains unsupported characters"));
    }
    let name = decode_uri_component(&raw_name)
        .ok_or_else(|| invalid("User name is not valid URL-encoded text"))?;
    let name = required_string(Some(&Value::String(name)), "X-Broadcast-User-Name", 120)?;
    let avatar_url = raw_avatar.map(avatar).transpose()?;
    Ok(Actor {
        kind: "user".into(),
        id,
        name,
        avatar_url,
    })
}

/// `^[a-z0-9](?:[a-z0-9._-]*[a-z0-9])?$`
fn is_user_id(id: &str) -> bool {
    let edge = |c: u8| c.is_ascii_lowercase() || c.is_ascii_digit();
    let bytes = id.as_bytes();
    bytes.first().is_some_and(|&c| edge(c))
        && bytes.last().is_some_and(|&c| edge(c))
        && bytes
            .iter()
            .all(|&c| edge(c) || matches!(c, b'.' | b'_' | b'-'))
}

fn avatar(raw: String) -> Result<String, ApiError> {
    let value = required_string(Some(&Value::String(raw)), "X-Broadcast-User-Avatar", 2048)?;
    let url = Url::parse(&value).map_err(|_| invalid("User avatar URL is invalid"))?;
    if !matches!(url.scheme(), "http" | "https") && !is_webp_data_url(&value) {
        return Err(invalid(
            "User avatar URL must use HTTP, HTTPS or a base64 WebP image",
        ));
    }
    Ok(url.to_string())
}

/// `^data:image/webp;base64,[A-Za-z0-9+/]+={0,2}$`
fn is_webp_data_url(value: &str) -> bool {
    let Some(data) = value.strip_prefix("data:image/webp;base64,") else {
        return false;
    };
    let body = data.trim_end_matches('=');
    data.len() - body.len() <= 2
        && !body.is_empty()
        && body
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || c == b'+' || c == b'/')
}

pub fn resolve_assignee(target: Option<AssigneeTarget>, actor: &Actor) -> Result<Actor, ApiError> {
    match target {
        None => Ok(actor.clone()),
        Some(AssigneeTarget::CodexAgent) => Ok(Actor::codex_agent()),
        Some(AssigneeTarget::ClaudeAgent) => Ok(Actor::claude_agent()),
        Some(AssigneeTarget::CurrentUser) if actor.kind != "user" => Err(ApiError::invalid_field(
            "'current-user' requires a user request identity",
        )),
        Some(AssigneeTarget::CurrentUser) => Ok(actor.clone()),
    }
}
