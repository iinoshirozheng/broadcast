//! Basic credentials checked against the shared secret, and the signed session cookie they mint.

use base64::Engine;
use base64::alphabet;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::engine::{DecodePaddingMode, GeneralPurpose, GeneralPurposeConfig};
use broadcast_domain::fields::{AssigneeTarget, required_string};
use broadcast_domain::{ApiError, js};
use hmac::{Hmac, Mac};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use worker::{Env, Request, Response};

use crate::error::Result;
use crate::http;

const SESSION_COOKIE_NAME: &str = "__Host-broadcast_session";
const SESSION_MAX_AGE_SECONDS: i64 = 24 * 60 * 60;

/// The signed-in user, or the Codex agent acting for them.
pub struct Actor {
    pub kind: &'static str,
    pub id: String,
    pub name: String,
    pub username: String,
}

impl Actor {
    /// The actor record with `username`, as the JavaScript worker stores and compares it.
    pub fn record(&self) -> Value {
        json!({ "type": self.kind, "id": self.id, "name": self.name, "avatarUrl": null, "username": self.username })
    }
}

pub struct Session {
    pub actor: Actor,
    /// Set when Basic credentials signed the user in.
    pub cookie: Option<String>,
}

fn user_id(username: &str) -> String {
    format!("basic:{}", js::encode_uri_component(&username.to_lowercase()))
}

fn codex_agent(username: &str) -> (String, String) {
    (format!("{}:codex-agent", user_id(username)), format!("Codex Agent ({username})"))
}

/// `resolveAssignee`: the actor itself, or the actor's Codex agent.
pub fn resolve_assignee(target: Option<AssigneeTarget>, actor: &Actor) -> Value {
    match target {
        None | Some(AssigneeTarget::CurrentUser) => actor.record(),
        Some(_) => {
            let (id, name) = codex_agent(&actor.username);
            json!({ "type": "agent", "id": id, "name": name, "avatarUrl": null })
        }
    }
}

/// `atob`: forgiving base64 that ignores ASCII whitespace and missing padding.
fn atob(value: &str) -> Option<Vec<u8>> {
    let engine = GeneralPurpose::new(
        &alphabet::STANDARD,
        GeneralPurposeConfig::new()
            .with_decode_padding_mode(DecodePaddingMode::Indifferent)
            .with_decode_allow_trailing_bits(true),
    );
    let compact: String = value.chars().filter(|c| !matches!(c, ' ' | '\t' | '\n' | '\x0c' | '\r')).collect();
    engine.decode(compact).ok()
}

/// `new TextDecoder("utf-8", { fatal: true }).decode`.
fn utf8(bytes: Vec<u8>) -> Option<String> {
    let text = String::from_utf8(bytes).ok()?;
    Some(text.strip_prefix('\u{feff}').map(str::to_owned).unwrap_or(text))
}

fn basic_credentials(header: Option<&str>) -> Option<(String, String)> {
    let encoded = header?.strip_prefix("Basic ")?;
    let value = utf8(atob(js::trim(encoded))?)?;
    let separator = value.find(':').filter(|&index| index >= 1)?;
    Some((value[..separator].to_owned(), value[separator + 1..].to_owned()))
}

fn signer(secret: &str) -> Hmac<Sha256> {
    Hmac::<Sha256>::new_from_slice(secret.as_bytes()).expect("HMAC accepts keys of any length")
}

fn session_cookie(username: &str, secret: &str) -> String {
    let expires_at = worker::js_sys::Date::now() as i64 + SESSION_MAX_AGE_SECONDS * 1000;
    let payload = URL_SAFE_NO_PAD.encode(json!({ "username": username, "expiresAt": expires_at }).to_string());
    let mut mac = signer(secret);
    mac.update(payload.as_bytes());
    let signature = URL_SAFE_NO_PAD.encode(mac.finalize().into_bytes());
    format!("{SESSION_COOKIE_NAME}={payload}.{signature}; Path=/; Max-Age={SESSION_MAX_AGE_SECONDS}; HttpOnly; Secure; SameSite=Strict")
}

fn read_cookie(header: &str, name: &str) -> Option<String> {
    header.split(';').find_map(|part| {
        let separator = part.find('=').filter(|&index| index >= 1)?;
        (js::trim(&part[..separator]) == name).then(|| js::trim(&part[separator + 1..]).to_owned())
    })
}

fn decode_base64_url(value: &str) -> Option<Vec<u8>> {
    let normalized = value.replace('-', "+").replace('_', "/");
    let padded = format!("{normalized}{}", "=".repeat((4 - normalized.len() % 4) % 4));
    atob(&padded)
}

fn session_username(cookie: Option<&str>, secret: &str) -> Option<String> {
    let token = read_cookie(cookie.unwrap_or_default(), SESSION_COOKIE_NAME).filter(|token| !token.is_empty())?;
    let parts: Vec<&str> = token.split('.').collect();
    let [payload, signature] = parts.as_slice() else { return None };
    let mut mac = signer(secret);
    mac.update(payload.as_bytes());
    mac.verify_slice(&decode_base64_url(signature)?).ok()?;
    let payload: Value = serde_json::from_str(&utf8(decode_base64_url(payload)?)?).ok()?;
    let expires_at = js::safe_integer(payload.get("expiresAt"))?;
    if expires_at as f64 <= worker::js_sys::Date::now() {
        return None;
    }
    required_string(payload.get("username"), "session username", 120).ok()
}

/// `authenticate`: `None` when neither the credentials nor the session cookie are valid.
pub fn authenticate(request: &Request, env: &Env) -> Result<Option<Session>> {
    let secret = env.var("BROADCAST_SHARED_SECRET").map(|secret| secret.to_string()).unwrap_or_default();
    if secret.is_empty() {
        return Err(ApiError::new(500, "SERVER_MISCONFIGURED", "BROADCAST_SHARED_SECRET is not configured").into());
    }
    let headers = request.headers();
    let (username, cookie) = match basic_credentials(headers.get("authorization")?.as_deref()) {
        Some((username, password)) => {
            let provided = Sha256::digest(password.as_bytes());
            let configured = Sha256::digest(secret.as_bytes());
            if provided.iter().zip(configured.iter()).fold(0, |difference, (a, b)| difference | (a ^ b)) != 0 {
                return Ok(None);
            }
            let username = required_string(Some(&Value::String(username)), "Basic username", 120)?;
            let cookie = session_cookie(&username, &secret);
            (username, Some(cookie))
        }
        None => match session_username(headers.get("cookie")?.as_deref(), &secret) {
            Some(username) => (username, None),
            None => return Ok(None),
        },
    };
    let actor = if headers.get("x-broadcast-client")?.as_deref() == Some("broadcast") {
        let (id, name) = codex_agent(&username);
        Actor { kind: "agent", id, name, username }
    } else {
        Actor { kind: "user", id: user_id(&username), name: username.clone(), username }
    };
    Ok(Some(Session { actor, cookie }))
}

pub fn unauthorized() -> Result<Response> {
    let mut response = http::json(
        401,
        &json!({ "error": { "code": "UNAUTHORIZED", "message": "Valid Basic credentials are required" } }),
    )?;
    response.headers_mut().set("www-authenticate", r#"Basic realm="Broadcast", charset="UTF-8""#)?;
    Ok(response)
}
