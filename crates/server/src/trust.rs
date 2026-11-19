//! The request gate in front of the router: `assertTrustedNetworkRequest`, device-local routes,
//! and the headers every reply carries.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::Arc;

use axum::extract::{ConnectInfo, Request, State};
use axum::http::{HeaderMap, HeaderValue, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use broadcast_domain::ApiError;
use url::Url;

use crate::App;
use crate::http::{Failure, header as text};

pub async fn guard(State(app): State<Arc<App>>, request: Request, next: Next) -> Response {
    let mut response = match admit(&app, &request) {
        Ok(()) => next.run(request).await,
        Err(error) => Failure::from(error).into_response(),
    };
    let reply = response.headers_mut();
    reply.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    reply.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("no-referrer"),
    );
    response
}

fn admit(app: &App, request: &Request) -> Result<(), ApiError> {
    let origin = text(request.headers(), header::ORIGIN.as_str());
    check(app, request.headers(), origin.as_deref())?;
    if request.uri().path().starts_with("/api/local/") {
        let peer = request
            .extensions()
            .get::<ConnectInfo<SocketAddr>>()
            .map(|info| info.0.ip());
        assert_local(peer)?;
    }
    Ok(())
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
    if local {
        Ok(())
    } else {
        Err(ApiError::new(
            403,
            "LOCAL_ONLY",
            "This endpoint is only available on this device",
        ))
    }
}

/// `assertTrustedNetworkRequest`: local, private or configured trusted hosts and origins.
fn check(app: &App, headers: &HeaderMap, origin: Option<&str>) -> Result<(), ApiError> {
    let invalid_host = || {
        ApiError::new(
            403,
            "INVALID_HOST",
            "Request Host must be local, private, or explicitly trusted",
        )
    };
    let (hostname, https_origin) =
        parse_host(text(headers, header::HOST.as_str()).as_deref()).ok_or_else(invalid_host)?;
    let network_host = is_trusted_network_host(&hostname);
    let configured_host = !network_host && app.config.trusted_origins.contains(&https_origin);
    if !network_host && !configured_host {
        return Err(invalid_host());
    }
    let configured_origin =
        origin.is_some_and(|origin| app.config.trusted_origins.contains(origin));
    if let Some(origin) = origin.filter(|origin| !origin.is_empty())
        && !configured_origin
    {
        let origin_host = Url::parse(origin)
            .ok()
            .map(|url| url.host_str().unwrap_or_default().to_owned());
        if !origin_host.is_some_and(|host| is_trusted_network_host(&host)) {
            return Err(ApiError::new(
                403,
                "INVALID_ORIGIN",
                "Request Origin must be local or private",
            ));
        }
    }
    Ok(())
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
        return matches!(
            (first, second),
            (127 | 10, _) | (172, 16..=31) | (192, 168) | (169, 254)
        );
    }
    host.parse::<Ipv6Addr>().is_ok()
        && (host.starts_with("fc")
            || host.starts_with("fd")
            || ["fe8", "fe9", "fea", "feb"]
                .iter()
                .any(|p| host.starts_with(p)))
}
