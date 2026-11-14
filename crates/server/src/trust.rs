//! `assertTrustedNetworkRequest` plus the response headers every reply carries.

use std::net::{Ipv4Addr, Ipv6Addr};
use std::sync::Arc;

use axum::extract::{Request, State};
use axum::http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use broadcast_domain::ApiError;
use url::Url;

use crate::App;
use crate::http::{Failure, header as text};

/// The packaged desktop shell's opaque origin, allowed to embed the board.
const EMBED_ORIGIN: &str = "app://-";

pub async fn guard(State(app): State<Arc<App>>, request: Request, next: Next) -> Response {
    let origin = text(request.headers(), header::ORIGIN.as_str());
    let mut response = match check(&app, request.headers(), origin.as_deref()) {
        Err(error) => Failure::from(error).into_response(),
        Ok(()) if origin.as_deref() == Some(EMBED_ORIGIN) => {
            let preflight = request.method() == Method::OPTIONS;
            let requested = text(request.headers(), header::ACCESS_CONTROL_REQUEST_HEADERS.as_str());
            let mut response =
                if preflight { StatusCode::NO_CONTENT.into_response() } else { next.run(request).await };
            embed_headers(response.headers_mut(), requested.as_deref());
            response
        }
        Ok(()) => next.run(request).await,
    };
    let headers = response.headers_mut();
    headers.insert(header::X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
    headers.insert(header::REFERRER_POLICY, HeaderValue::from_static("no-referrer"));
    response
}

fn embed_headers(headers: &mut HeaderMap, requested: Option<&str>) {
    let allow_headers = requested
        .and_then(|value| HeaderValue::from_str(value).ok())
        .unwrap_or(HeaderValue::from_static("content-type"));
    headers.insert(header::ACCESS_CONTROL_ALLOW_ORIGIN, HeaderValue::from_static(EMBED_ORIGIN));
    headers.insert(
        header::ACCESS_CONTROL_ALLOW_METHODS,
        HeaderValue::from_static("GET, HEAD, POST, PUT, PATCH, DELETE, OPTIONS"),
    );
    headers.insert(header::ACCESS_CONTROL_ALLOW_HEADERS, allow_headers);
    headers.insert(header::ACCESS_CONTROL_EXPOSE_HEADERS, HeaderValue::from_static("x-broadcast-proof"));
    headers.insert(HeaderName::from_static("access-control-allow-private-network"), HeaderValue::from_static("true"));
    headers.insert(header::VARY, HeaderValue::from_static("origin"));
}

fn check(app: &App, headers: &HeaderMap, origin: Option<&str>) -> Result<(), ApiError> {
    let invalid_host = || ApiError::new(403, "INVALID_HOST", "Request Host must be local, private, or explicitly trusted");
    let (hostname, https_origin) = parse_host(text(headers, header::HOST.as_str()).as_deref()).ok_or_else(invalid_host)?;
    if !is_trusted_network_host(&hostname) && !app.config.trusted_origins.contains(&https_origin) {
        return Err(invalid_host());
    }
    let Some(origin) = origin.filter(|origin| !origin.is_empty()) else { return Ok(()) };
    if app.config.trusted_origins.contains(origin) || origin == EMBED_ORIGIN {
        return Ok(());
    }
    let origin_host = Url::parse(origin).ok().map(|url| url.host_str().unwrap_or_default().to_owned());
    if origin_host.is_some_and(|host| is_trusted_network_host(&host)) {
        return Ok(());
    }
    Err(ApiError::new(403, "INVALID_ORIGIN", "Request Origin must be local or private"))
}

/// `parseRequestHost`: the Host header read as an HTTPS authority.
fn parse_host(value: Option<&str>) -> Option<(String, String)> {
    let value = value.filter(|value| !value.is_empty() && *value == value.trim())?;
    let url = Url::parse(&format!("https://{value}")).ok()?;
    let exact = url.username().is_empty()
        && url.password().is_none()
        && url.path() == "/"
        && url.query().is_none_or(str::is_empty)
        && url.fragment().is_none_or(str::is_empty);
    let hostname = url.host_str().filter(|host| exact && !host.is_empty())?;
    Some((hostname.to_owned(), url.origin().ascii_serialization()))
}

/// Loopback, private-LAN and link-local hosts, plus `localhost` and `*.local`.
fn is_trusted_network_host(hostname: &str) -> bool {
    let lower = hostname.to_lowercase();
    let host = lower.strip_prefix('[').unwrap_or(&lower);
    let host = host.strip_suffix(']').unwrap_or(host);
    if host == "localhost" || host == "::1" || host.ends_with(".local") {
        return true;
    }
    if let Ok(address) = host.parse::<Ipv4Addr>() {
        let [first, second, ..] = address.octets();
        return matches!((first, second), (127 | 10, _) | (172, 16..=31) | (192, 168) | (169, 254));
    }
    host.parse::<Ipv6Addr>().is_ok()
        && (host.starts_with("fc") || host.starts_with("fd") || ["fe8", "fe9", "fea", "feb"].iter().any(|p| host.starts_with(p)))
}
