//! Cloud companion rules shared by the server and the CLI.

use url::Url;

/// `normalizeCloudUrl`: an HTTPS origin, or loopback HTTP for development. The error is the
/// `INVALID_CLOUD_URL` message.
pub fn origin(value: &str) -> Result<String, &'static str> {
    let url = Url::parse(value).map_err(|_| "Cloud broadcast URL must be a valid URL")?;
    let loopback = matches!(url.host_str(), Some("localhost" | "127.0.0.1" | "[::1]"));
    let scheme_ok = url.scheme() == "https" || (url.scheme() == "http" && loopback);
    let exact = url.username().is_empty()
        && url.password().is_none()
        && url.path() == "/"
        && url.query().is_none_or(str::is_empty)
        && url.fragment().is_none_or(str::is_empty);
    if !scheme_ok || !exact {
        return Err(
            "Cloud broadcast URL must be an HTTPS origin (loopback HTTP is allowed for development)",
        );
    }
    Ok(url.origin().ascii_serialization())
}
