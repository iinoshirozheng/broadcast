//! Which Broadcast service a command talks to.

pub const DEFAULT_API_URL: &str = "http://127.0.0.1:47823";

/// `BROADCAST_URL`, or the local development server.
pub fn base_url() -> String {
    std::env::var("BROADCAST_URL").unwrap_or_else(|_| DEFAULT_API_URL.to_owned())
}
