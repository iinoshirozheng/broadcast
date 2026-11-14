//! The request gate in front of the router: launcher prefix, `assertTrustedNetworkRequest`,
//! embed CORS, launcher proofs and device-local routes, plus headers every reply carries.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::Arc;

use axum::extract::{ConnectInfo, Request, State};
use axum::http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode, Uri, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use broadcast_domain::ApiError;
use hmac::{Hmac, Mac};
use sha2::Sha256;
use url::Url;

use crate::App;
use crate::config::Launcher;
use crate::http::{Failure, header as text};

/// The packaged desktop shell's opaque origin, allowed to embed the board.
const EMBED_ORIGIN: &str = "app://-";
const PROOF: HeaderName = HeaderName::from_static("x-broadcast-proof");

/// What the gate learned about the caller, for handlers that answer differently per caller.
#[derive(Debug, Clone, Copy)]
pub struct Peer {
    /// Reached through a configured trusted HTTPS origin rather than the device network.
    pub configured: bool,
    pub address: Option<IpAddr>,
}

impl Peer {
    /// `isLoopbackAddress`: any 127/8 address.
    pub fn loopback(&self) -> bool {
        self.address.is_some_and(loopback)
    }

    /// `assertLoopbackRequest`.
    pub fn assert_local(&self) -> Result<(), ApiError> {
        assert_local(self.address)
    }
}

pub async fn guard(State(app): State<Arc<App>>, mut request: Request, next: Next) -> Response {
    let mut headers = HeaderMap::new();
    let mut response = match admit(&app, &mut request, &mut headers) {
        Ok(None) => next.run(request).await,
        Ok(Some(response)) => response,
        Err(error) => Failure::from(error).into_response(),
    };
    let reply = response.headers_mut();
    reply.extend(headers);
    reply.insert(header::X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
    reply.insert(header::REFERRER_POLICY, HeaderValue::from_static("no-referrer"));
    response
}

/// Returns a response to send instead of routing, or `None` to route the (rewritten) request.
fn admit(app: &App, request: &mut Request, headers: &mut HeaderMap) -> Result<Option<Response>, ApiError> {
    let launcher = app.config.launcher.as_ref();
    if let Some(launcher) = launcher
        && request.uri().path() != "/health"
        && let Some(response) = strip_prefix(launcher, request)?
    {
        return Ok(Some(response));
    }
    let origin = text(request.headers(), header::ORIGIN.as_str());
    let configured = check(app, request.headers(), origin.as_deref(), launcher.is_some())?;
    let embed = origin.as_deref().filter(|origin| *origin == EMBED_ORIGIN || (launcher.is_some() && *origin == "null"));
    if let Some(origin) = embed {
        let requested = text(request.headers(), header::ACCESS_CONTROL_REQUEST_HEADERS.as_str());
        embed_headers(headers, origin, requested.as_deref());
        if request.method() == Method::OPTIONS {
            return Ok(Some(StatusCode::NO_CONTENT.into_response()));
        }
    }
    if let Some(launcher) = launcher
        && origin.as_deref() == Some(EMBED_ORIGIN)
    {
        let proof = proof(launcher, request.headers())?;
        headers.insert(PROOF, HeaderValue::from_str(&proof).expect("hex is a header value"));
    }
    let peer = request.extensions().get::<ConnectInfo<SocketAddr>>().map(|info| info.0.ip());
    let path = request.uri().path();
    let development_contexts = path
        .strip_prefix("/api/projects/")
        .and_then(|rest| rest.strip_suffix("/development-contexts"))
        .is_some_and(|id| !id.is_empty() && !id.contains('/'));
    if configured && (path.starts_with("/api/local/") || path == "/api/device-workspaces" || development_contexts) {
        return Err(ApiError::new(409, "LOCAL_COMPANION_REQUIRED", "This capability requires a device-local Broadcast origin"));
    }
    if path == "/api/local/ai" || path.starts_with("/api/local/ai/") {
        if !peer.is_some_and(loopback) {
            return Err(ApiError::new(403, "LOCAL_AI_LOOPBACK_REQUIRED", "Local AI routes are only available from this device"));
        }
    } else if path.starts_with("/api/local/") {
        assert_local(peer)?;
    }
    request.extensions_mut().insert(Peer { configured, address: peer });
    Ok(None)
}

/// Launcher routes live under `/<token>`; `/<token>` itself redirects to `/<token>/`.
fn strip_prefix(launcher: &Launcher, request: &mut Request) -> Result<Option<Response>, ApiError> {
    let uri = request.uri();
    let prefix = format!("/{}", launcher.token);
    let query = uri.query().map(|query| format!("?{query}")).unwrap_or_default();
    if uri.path() == prefix {
        let location = HeaderValue::from_str(&format!("{prefix}/{query}")).expect("token and query are header-safe");
        return Ok(Some((StatusCode::MOVED_PERMANENTLY, [(header::LOCATION, location)]).into_response()));
    }
    let Some(rest) = uri.path().strip_prefix(&prefix).filter(|rest| rest.starts_with('/')) else {
        return Err(ApiError::new(404, "NOT_FOUND", "Route not found"));
    };
    let rewritten: Uri = format!("{rest}{query}").parse().map_err(|_| ApiError::new(404, "NOT_FOUND", "Route not found"))?;
    *request.uri_mut() = rewritten;
    Ok(None)
}

/// `x-broadcast-proof`: HMAC-SHA256 of the launcher's challenge, keyed by the instance secret.
pub fn proof(launcher: &Launcher, headers: &HeaderMap) -> Result<String, ApiError> {
    let challenge = text(headers, "x-broadcast-challenge")
        .filter(|value| (32..=128).contains(&value.len()) && value.bytes().all(|byte| byte.is_ascii_hexdigit()))
        .ok_or_else(|| ApiError::new(401, "INVALID_INSTANCE_CHALLENGE", "Launcher challenge is required"))?;
    let mut mac = Hmac::<Sha256>::new_from_slice(launcher.secret.as_bytes()).expect("HMAC accepts any key length");
    mac.update(challenge.as_bytes());
    Ok(mac.finalize().into_bytes().iter().map(|byte| format!("{byte:02x}")).collect())
}

/// `assertLoopbackRequest`: exactly this device's loopback address.
fn assert_local(peer: Option<IpAddr>) -> Result<(), ApiError> {
    let local = match peer {
        Some(IpAddr::V4(address)) => address == Ipv4Addr::LOCALHOST,
        Some(IpAddr::V6(address)) => {
            address == Ipv6Addr::LOCALHOST || address.to_ipv4_mapped() == Some(Ipv4Addr::LOCALHOST)
        }
        None => false,
    };
    if local { Ok(()) } else { Err(ApiError::new(403, "LOCAL_ONLY", "This endpoint is only available on this device")) }
}

fn loopback(address: IpAddr) -> bool {
    match address {
        IpAddr::V4(address) => address.is_loopback(),
        IpAddr::V6(address) => address.is_loopback() || address.to_ipv4_mapped().is_some_and(|v4| v4.is_loopback()),
    }
}

fn embed_headers(headers: &mut HeaderMap, origin: &str, requested: Option<&str>) {
    let allow_headers = requested
        .and_then(|value| HeaderValue::from_str(value).ok())
        .unwrap_or(HeaderValue::from_static("content-type"));
    headers.insert(header::ACCESS_CONTROL_ALLOW_ORIGIN, HeaderValue::from_str(origin).expect("embed origins are header-safe"));
    headers.insert(
        header::ACCESS_CONTROL_ALLOW_METHODS,
        HeaderValue::from_static("GET, HEAD, POST, PUT, PATCH, DELETE, OPTIONS"),
    );
    headers.insert(header::ACCESS_CONTROL_ALLOW_HEADERS, allow_headers);
    headers.insert(header::ACCESS_CONTROL_EXPOSE_HEADERS, HeaderValue::from_static("x-broadcast-proof"));
    headers.insert(HeaderName::from_static("access-control-allow-private-network"), HeaderValue::from_static("true"));
    headers.insert(header::VARY, HeaderValue::from_static("origin"));
}

/// `assertTrustedNetworkRequest`; true when a configured trusted origin admitted the request.
fn check(app: &App, headers: &HeaderMap, origin: Option<&str>, allow_opaque: bool) -> Result<bool, ApiError> {
    let invalid_host = || ApiError::new(403, "INVALID_HOST", "Request Host must be local, private, or explicitly trusted");
    let (hostname, https_origin) = parse_host(text(headers, header::HOST.as_str()).as_deref()).ok_or_else(invalid_host)?;
    let network_host = is_trusted_network_host(&hostname);
    let configured_host = !network_host && app.config.trusted_origins.contains(&https_origin);
    if !network_host && !configured_host {
        return Err(invalid_host());
    }
    let configured_origin = origin.is_some_and(|origin| app.config.trusted_origins.contains(origin));
    if let Some(origin) = origin.filter(|origin| !origin.is_empty())
        && !configured_origin
        && origin != EMBED_ORIGIN
        && !(allow_opaque && origin == "null")
    {
        let origin_host = Url::parse(origin).ok().map(|url| url.host_str().unwrap_or_default().to_owned());
        if !origin_host.is_some_and(|host| is_trusted_network_host(&host)) {
            return Err(ApiError::new(403, "INVALID_ORIGIN", "Request Origin must be local or private"));
        }
    }
    Ok(configured_host || configured_origin)
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
