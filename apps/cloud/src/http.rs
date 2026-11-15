//! Responses, request bodies and the attachment headers of the cloud API.

use broadcast_domain::{ApiError, js};
use serde_json::{Value, json};
use worker::{Headers, Request, Response};

use crate::error::{Failure, Result};

pub const JSON_BODY_LIMIT: usize = 1024 * 1024;
pub const JSON_BODY_TOO_LARGE: &str = "JSON body cannot exceed 1 MiB";
const ATTACHMENT_BODY_LIMIT: usize = 25 * 1024 * 1024;
const ATTACHMENT_TOO_LARGE: &str = "Attachment cannot exceed 25 MiB";

fn headers(entries: &[(&str, &str)]) -> Result<Headers> {
    let headers = Headers::new();
    for (name, value) in entries {
        headers.set(name, value)?;
    }
    Ok(headers)
}

pub fn json(status: u16, value: &Value) -> Result<Response> {
    json_with(status, value, &[])
}

fn json_with(status: u16, value: &Value, extra: &[(&str, &str)]) -> Result<Response> {
    let headers = headers(&[
        ("cache-control", "no-store"),
        ("content-type", "application/json; charset=utf-8"),
    ])?;
    for (name, value) in extra {
        headers.set(name, value)?;
    }
    Ok(Response::from_bytes(serde_json::to_vec(value)?)?
        .with_status(status)
        .with_headers(headers))
}

pub fn empty(status: u16) -> Result<Response> {
    Ok(Response::empty()?
        .with_status(status)
        .with_headers(headers(&[("cache-control", "no-store")])?))
}

pub fn method_not_allowed<T>(allowed: &[&str]) -> Result<T> {
    Err(
        ApiError::new(405, "METHOD_NOT_ALLOWED", "Method not allowed")
            .with_details(json!({ "allowed": allowed }))
            .into(),
    )
}

/// The JSON error of a failed request; an internal failure is logged and hidden.
pub fn failure_response(failure: Failure) -> Result<Response> {
    match failure {
        Failure::Api(error) => {
            let mut payload = json!({ "code": error.code, "message": error.message });
            let mut allow = None;
            if let Some(details) = error.details {
                if error.status == 405 {
                    allow = details
                        .get("allowed")
                        .and_then(Value::as_array)
                        .map(|allowed| {
                            allowed
                                .iter()
                                .filter_map(Value::as_str)
                                .collect::<Vec<_>>()
                                .join(", ")
                        });
                }
                payload["details"] = *details;
            }
            let extra = allow
                .as_deref()
                .map(|allow| vec![("allow", allow)])
                .unwrap_or_default();
            json_with(error.status, &json!({ "error": payload }), &extra)
        }
        Failure::Internal(message) => {
            worker::console_error!("{message}");
            json(
                500,
                &json!({ "error": { "code": "INTERNAL_ERROR", "message": "Internal server error" } }),
            )
        }
    }
}

/// A mutable copy of a response's headers: a fetched response's own headers are immutable.
fn header_copy(response: &Response) -> Result<Headers> {
    let headers = Headers::new();
    for (name, value) in response.headers().entries() {
        headers.append(&name, &value)?;
    }
    Ok(headers)
}

pub fn with_cookie(response: Response, cookie: &str) -> Result<Response> {
    let headers = header_copy(&response)?;
    headers.append("set-cookie", cookie)?;
    Ok(response.with_headers(headers))
}

/// Adds the security headers every response carries, except a WebSocket upgrade.
pub fn with_security_headers(response: Response) -> Result<Response> {
    if response.status_code() == 101 {
        return Ok(response);
    }
    let headers = header_copy(&response)?;
    headers.set("x-content-type-options", "nosniff")?;
    headers.set("referrer-policy", "no-referrer")?;
    Ok(response.with_headers(headers))
}

fn header(request: &Request, name: &str) -> Result<Option<String>> {
    Ok(request.headers().get(name)?)
}

fn declared_length(request: &Request) -> Result<f64> {
    Ok(header(request, "content-length")?.map_or(0.0, |length| js::string_to_number(&length)))
}

pub async fn read_json(request: &mut Request, limit: usize, too_large: &str) -> Result<Value> {
    let content_type = header(request, "content-type")?.unwrap_or_default();
    if !content_type.to_lowercase().starts_with("application/json") {
        return Err(ApiError::new(
            415,
            "UNSUPPORTED_MEDIA_TYPE",
            "Content-Type must be application/json",
        )
        .into());
    }
    if declared_length(request)? > limit as f64 {
        return Err(ApiError::new(413, "BODY_TOO_LARGE", too_large).into());
    }
    let text = request.text().await?;
    if text.len() > limit {
        return Err(ApiError::new(413, "BODY_TOO_LARGE", too_large).into());
    }
    serde_json::from_str(&text)
        .map_err(|_| ApiError::new(400, "INVALID_JSON", "Request body is not valid JSON").into())
}

pub async fn read_attachment(request: &mut Request) -> Result<Vec<u8>> {
    if declared_length(request)? > ATTACHMENT_BODY_LIMIT as f64 {
        return Err(ApiError::new(413, "BODY_TOO_LARGE", ATTACHMENT_TOO_LARGE).into());
    }
    let body = request.bytes().await?;
    if body.len() > ATTACHMENT_BODY_LIMIT {
        return Err(ApiError::new(413, "BODY_TOO_LARGE", ATTACHMENT_TOO_LARGE).into());
    }
    Ok(body)
}

pub struct AttachmentHeaders {
    pub filename: String,
    pub content_type: String,
    pub kind: String,
}

fn token(value: &str) -> bool {
    !value.is_empty()
        && value.bytes().all(|byte| {
            byte.is_ascii_digit() || byte.is_ascii_lowercase() || b"!#$%&'*+.^_`|~-".contains(&byte)
        })
}

pub fn attachment_headers(request: &Request) -> Result<AttachmentHeaders> {
    let invalid_filename =
        |message: &str| Failure::from(ApiError::new(400, "INVALID_FILENAME", message));
    let encoded = header(request, "x-broadcast-filename")?
        .ok_or_else(|| invalid_filename("X-Broadcast-Filename is required"))?;
    let filename = js::decode_uri_component(&encoded)
        .ok_or_else(|| invalid_filename("Attachment filename contains invalid encoding"))?;
    let filename = js::trim(&filename).to_owned();
    let length = js::len(&filename);
    if length == 0
        || length > 240
        || filename == "."
        || filename == ".."
        || filename
            .chars()
            .any(|c| c <= '\u{1f}' || c == '\u{7f}' || c == '/' || c == '\\')
    {
        return Err(invalid_filename("Attachment filename is invalid"));
    }
    let content_type = match header(request, "content-type")?.filter(|value| !value.is_empty()) {
        Some(raw) => js::trim(raw.split(';').next().unwrap_or_default()).to_lowercase(),
        None => "application/octet-stream".to_owned(),
    };
    let valid = content_type.len() <= 200
        && content_type
            .split_once('/')
            .is_some_and(|(kind, subtype)| token(kind) && token(subtype));
    if !valid {
        return Err(ApiError::new(
            415,
            "UNSUPPORTED_MEDIA_TYPE",
            "Attachment Content-Type is invalid",
        )
        .into());
    }
    let kind = header(request, "x-broadcast-attachment-kind")?.unwrap_or_default();
    if kind != "inline" && kind != "attachment" {
        return Err(ApiError::new(
            400,
            "INVALID_ATTACHMENT_KIND",
            "X-Broadcast-Attachment-Kind must be inline or attachment",
        )
        .into());
    }
    Ok(AttachmentHeaders {
        filename,
        content_type,
        kind,
    })
}
