//! Response helpers and request parsing (`sendJson`, `readJson`, error replies).

use axum::body::{Bytes, to_bytes};
use axum::extract::Request;
use axum::http::{HeaderMap, HeaderValue, StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};
use broadcast_domain::js::{self, decode_uri_component};
use broadcast_domain::{ApiError, Error, Result};
use serde_json::{Value, json};

const JSON_BODY_LIMIT: usize = 1024 * 1024;

/// A handler result; errors render as `{ error: { code, message, details? } }`.
pub type Reply = std::result::Result<Response, Failure>;

pub fn json(status: u16, value: &Value) -> Response {
    let status = StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    let mut response = (status, value.to_string()).into_response();
    let headers = response.headers_mut();
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json; charset=utf-8"),
    );
    response
}

pub fn empty(status: StatusCode) -> Response {
    let mut response = status.into_response();
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

pub fn method_not_allowed(allowed: &[&str]) -> Reply {
    let allowed = allowed.join(", ");
    let mut response = json(
        405,
        &json!({ "error": { "code": "METHOD_NOT_ALLOWED", "message": format!("Allowed methods: {allowed}") } }),
    );
    response.headers_mut().insert(
        header::ALLOW,
        HeaderValue::from_str(&allowed).map_err(Error::from)?,
    );
    Ok(response)
}

/// A header value decoded as latin1, like Node's `request.headers`.
pub fn header(headers: &HeaderMap, name: &str) -> Option<String> {
    headers.get(name).map(|value| {
        value
            .as_bytes()
            .iter()
            .map(|&byte| char::from(byte))
            .collect()
    })
}

pub fn query(uri: &Uri) -> Vec<(String, String)> {
    form_urlencoded::parse(uri.query().unwrap_or_default().as_bytes())
        .into_owned()
        .collect()
}

/// The `n`th `/`-separated path segment (segment 0 is the empty string before the first `/`).
pub fn segment(uri: &Uri, n: usize) -> &str {
    uri.path().split('/').nth(n).unwrap_or_default()
}

pub fn reject_query(uri: &Uri, message: &str) -> Result<()> {
    if query(uri).is_empty() {
        return Ok(());
    }
    Err(ApiError::new(400, "UNKNOWN_QUERY_PARAMETER", message).into())
}

/// `assertAllowedQuery`: every key allowed and given once.
pub fn assert_allowed_query(
    query: &[(String, String)],
    allowed: &[&str],
    label: &str,
) -> Result<()> {
    for (key, _) in query {
        if !allowed.contains(&key.as_str()) {
            return Err(ApiError::new(
                400,
                "UNKNOWN_QUERY_PARAMETER",
                format!("{label} does not accept query parameter '{key}'"),
            )
            .into());
        }
        if query.iter().filter(|(other, _)| other == key).count() != 1 {
            return Err(ApiError::new(
                400,
                "INVALID_QUERY_PARAMETER",
                format!("Query parameter '{key}' cannot be repeated"),
            )
            .into());
        }
    }
    Ok(())
}

/// An `?after=<revision>` cursor: the raw value and its revision.
pub struct Cursor {
    pub value: String,
    pub revision: i64,
}

/// `parseAfterCursor`.
pub fn after_cursor(uri: &Uri, label: &str) -> Result<Option<Cursor>> {
    let query = query(uri);
    assert_allowed_query(&query, &["after"], label)?;
    let Some((_, value)) = query.into_iter().find(|(key, _)| key == "after") else {
        return Ok(None);
    };
    let revision = (!value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit()))
        .then(|| value.parse::<i64>().ok())
        .flatten()
        .filter(|revision| *revision <= MAX_SAFE_INTEGER);
    let Some(revision) = revision else {
        return Err(ApiError::new(
            400,
            "INVALID_CURSOR",
            "Cursor must be a non-negative integer revision",
        )
        .into());
    };
    Ok(Some(Cursor { value, revision }))
}

/// `nextCursor`: the highest revision returned, or the cursor the client sent.
pub fn next_cursor(revisions: impl Iterator<Item = i64>, after: Option<&Cursor>) -> String {
    match revisions.max() {
        Some(revision) => revision.max(0).to_string(),
        None => after.map_or_else(|| "0".to_owned(), |cursor| cursor.value.clone()),
    }
}

const MAX_SAFE_INTEGER: i64 = 9_007_199_254_740_991;

/// `decodeURIComponent` of path segment `n`, 1..=128 UTF-16 units long.
pub fn path_id(uri: &Uri, n: usize, label: &str) -> Result<String> {
    let id = decode_uri_component(segment(uri, n)).ok_or_else(|| {
        ApiError::new(
            400,
            "INVALID_PATH",
            format!("{label} id contains invalid encoding"),
        )
    })?;
    if id.is_empty() || js::len(&id) > 128 {
        return Err(ApiError::new(400, "INVALID_PATH", format!("{label} id is invalid")).into());
    }
    Ok(id)
}

/// `readBody`: the whole body, rejecting anything over `limit` bytes.
pub async fn read_body(request: Request, limit: usize, too_large: &str) -> Result<Bytes> {
    let error = || ApiError::new(413, "BODY_TOO_LARGE", too_large);
    let declared = header(request.headers(), header::CONTENT_LENGTH.as_str())
        .and_then(|length| length.trim().parse::<f64>().ok())
        .unwrap_or(0.0);
    if declared > limit as f64 {
        return Err(error().into());
    }
    to_bytes(request.into_body(), limit)
        .await
        .map_err(|_| error().into())
}

/// `assertEmptyRequestBody`: `route` (e.g. `POST /api/...`) takes no body.
pub async fn assert_empty_body(request: Request, route: &str) -> Result<()> {
    if read_body(request, JSON_BODY_LIMIT, "Request body cannot exceed 1 MiB")
        .await?
        .is_empty()
    {
        return Ok(());
    }
    Err(ApiError::new(
        400,
        "INVALID_BODY",
        format!("{route} does not accept a request body"),
    )
    .into())
}

/// `readJson`: `application/json`, at most 1 MiB, non-empty, valid JSON.
pub async fn read_json(request: Request) -> Result<Value> {
    read_json_limit(request, JSON_BODY_LIMIT, "Request body cannot exceed 1 MiB").await
}

pub async fn read_json_limit(request: Request, limit: usize, too_large: &str) -> Result<Value> {
    let content_type = header(request.headers(), header::CONTENT_TYPE.as_str()).unwrap_or_default();
    let media_type = content_type
        .split(';')
        .next()
        .unwrap_or_default()
        .trim()
        .to_lowercase();
    if media_type != "application/json" {
        return Err(ApiError::new(
            415,
            "UNSUPPORTED_MEDIA_TYPE",
            "Content-Type must be application/json",
        )
        .into());
    }
    let body = read_body(request, limit, too_large).await?;
    if body.is_empty() {
        return Err(ApiError::new(400, "INVALID_JSON", "Request body cannot be empty").into());
    }
    serde_json::from_str(&String::from_utf8_lossy(&body)).map_err(|_| {
        ApiError::new(400, "INVALID_JSON", "Request body must contain valid JSON").into()
    })
}

/// `parseAttachmentHeaders`: the upload's filename, media type and kind.
pub struct AttachmentHeaders {
    pub filename: String,
    pub content_type: String,
    pub kind: &'static str,
}

pub fn attachment_headers(headers: &HeaderMap) -> Result<AttachmentHeaders> {
    let invalid = |message: &str| ApiError::new(400, "INVALID_FILENAME", message);
    let encoded = header(headers, "x-broadcast-filename")
        .ok_or_else(|| invalid("X-Broadcast-Filename is required"))?;
    let filename = decode_uri_component(&encoded)
        .ok_or_else(|| invalid("Attachment filename contains invalid encoding"))?;
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
        return Err(invalid("Attachment filename is invalid").into());
    }
    let content_type = header(headers, header::CONTENT_TYPE.as_str()).map_or_else(
        || "application/octet-stream".to_owned(),
        |raw| js::trim(raw.split(';').next().unwrap_or_default()).to_lowercase(),
    );
    if !is_media_type(&content_type) {
        return Err(ApiError::new(
            415,
            "UNSUPPORTED_MEDIA_TYPE",
            "Attachment Content-Type is invalid",
        )
        .into());
    }
    let kind = match header(headers, "x-broadcast-attachment-kind").as_deref() {
        Some("inline") => "inline",
        Some("attachment") => "attachment",
        _ => {
            return Err(ApiError::new(
                400,
                "INVALID_ATTACHMENT_KIND",
                "X-Broadcast-Attachment-Kind must be inline or attachment",
            )
            .into());
        }
    };
    Ok(AttachmentHeaders {
        filename,
        content_type,
        kind,
    })
}

/// `type/subtype` made of RFC 7230 token characters, at most 200 long.
fn is_media_type(value: &str) -> bool {
    let token = |part: &str| {
        !part.is_empty()
            && part.bytes().all(|byte| {
                byte.is_ascii_digit()
                    || byte.is_ascii_lowercase()
                    || b"!#$%&'*+.^_`|~-".contains(&byte)
            })
    };
    value.len() <= 200
        && value
            .split_once('/')
            .is_some_and(|(kind, subtype)| token(kind) && token(subtype))
}

pub fn api_error(error: &ApiError) -> Response {
    let mut payload = json!({ "code": error.code, "message": error.message });
    if let Some(details) = &error.details {
        payload["details"] = Value::clone(details);
    }
    json(error.status, &json!({ "error": payload }))
}

/// Wraps the domain error so handlers can use `?` and still return a JSON reply.
#[derive(Debug)]
pub struct Failure(Error);

impl From<Error> for Failure {
    fn from(error: Error) -> Self {
        Self(error)
    }
}

impl From<ApiError> for Failure {
    fn from(error: ApiError) -> Self {
        Self(Error::Api(error))
    }
}

impl IntoResponse for Failure {
    fn into_response(self) -> Response {
        match self.0 {
            Error::Api(error) => api_error(&error),
            Error::Internal(error) => {
                eprintln!("{error:?}");
                json(
                    500,
                    &json!({ "error": { "code": "INTERNAL_ERROR", "message": "Internal server error" } }),
                )
            }
        }
    }
}
