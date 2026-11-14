//! Response helpers and request parsing (`sendJson`, `readJson`, error replies).

use axum::body::to_bytes;
use axum::extract::Request;
use axum::http::{HeaderMap, HeaderValue, StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};
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
    headers.insert(header::CONTENT_TYPE, HeaderValue::from_static("application/json; charset=utf-8"));
    response
}

pub fn empty(status: StatusCode) -> Response {
    let mut response = status.into_response();
    response.headers_mut().insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

pub fn method_not_allowed(allowed: &[&str]) -> Reply {
    let allowed = allowed.join(", ");
    let mut response =
        json(405, &json!({ "error": { "code": "METHOD_NOT_ALLOWED", "message": format!("Allowed methods: {allowed}") } }));
    response.headers_mut().insert(header::ALLOW, HeaderValue::from_str(&allowed).map_err(Error::from)?);
    Ok(response)
}

/// A header value decoded as latin1, like Node's `request.headers`.
pub fn header(headers: &HeaderMap, name: &str) -> Option<String> {
    headers.get(name).map(|value| value.as_bytes().iter().map(|&byte| char::from(byte)).collect())
}

pub fn query(uri: &Uri) -> Vec<(String, String)> {
    form_urlencoded::parse(uri.query().unwrap_or_default().as_bytes()).into_owned().collect()
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

/// `readJson`: `application/json`, at most 1 MiB, non-empty, valid JSON.
pub async fn read_json(request: Request) -> Result<Value> {
    let content_type = header(request.headers(), header::CONTENT_TYPE.as_str()).unwrap_or_default();
    let media_type = content_type.split(';').next().unwrap_or_default().trim().to_lowercase();
    if media_type != "application/json" {
        return Err(ApiError::new(415, "UNSUPPORTED_MEDIA_TYPE", "Content-Type must be application/json").into());
    }
    let too_large = || ApiError::new(413, "BODY_TOO_LARGE", "Request body cannot exceed 1 MiB");
    let declared = header(request.headers(), header::CONTENT_LENGTH.as_str())
        .and_then(|length| length.trim().parse::<f64>().ok())
        .unwrap_or(0.0);
    if declared > JSON_BODY_LIMIT as f64 {
        return Err(too_large().into());
    }
    let body = to_bytes(request.into_body(), JSON_BODY_LIMIT).await.map_err(|_| too_large())?;
    if body.is_empty() {
        return Err(ApiError::new(400, "INVALID_JSON", "Request body cannot be empty").into());
    }
    serde_json::from_str(&String::from_utf8_lossy(&body))
        .map_err(|_| ApiError::new(400, "INVALID_JSON", "Request body must contain valid JSON").into())
}

pub fn api_error(error: &ApiError) -> Response {
    let mut payload = json!({ "code": error.code, "message": error.message });
    if let Some(details) = &error.details {
        payload["details"] = details.clone();
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
                json(500, &json!({ "error": { "code": "INTERNAL_ERROR", "message": "Internal server error" } }))
            }
        }
    }
}
