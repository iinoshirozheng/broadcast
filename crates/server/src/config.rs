//! `resolveServerOptions`, `resolvePort` and `resolveHost`.

use std::collections::HashSet;
use std::net::{IpAddr, Ipv4Addr};
use std::path::{PathBuf, absolute};

use anyhow::{Context, bail};
use url::Url;

const TRUSTED_ORIGINS_ENV: &str = "BROADCAST_TRUSTED_ORIGINS";

#[derive(Debug, Clone)]
pub struct Config {
    pub data_dir: PathBuf,
    pub static_dir: PathBuf,
    pub skill_path: PathBuf,
    pub codex_home: PathBuf,
    pub host: IpAddr,
    pub port: u16,
    pub trusted_origins: HashSet<String>,
    /// Set when the desktop launcher owns this server.
    pub launcher: Option<Launcher>,
    pub version: String,
}

/// Launcher mode: every route except `/health` lives under `/<token>`, and the
/// launcher verifies the server by HMAC proofs keyed with `secret`.
#[derive(Debug, Clone)]
pub struct Launcher {
    pub token: String,
    pub secret: String,
}

impl Config {
    /// Reads `BROADCAST_*` variables; paths default to the working directory.
    pub fn from_env() -> anyhow::Result<Self> {
        let variable = |name: &str| std::env::var(name).ok();
        let path = |name: &str, default: &str| -> anyhow::Result<PathBuf> {
            let value = variable(name).filter(|value| !value.is_empty()).unwrap_or(default.into());
            absolute(&value).with_context(|| format!("cannot resolve {value}"))
        };
        let codex_home = match variable("CODEX_HOME").filter(|value| !value.is_empty()) {
            Some(home) => PathBuf::from(home),
            None => std::env::home_dir().context("cannot resolve the home directory")?.join(".codex"),
        };
        Ok(Self {
            data_dir: path("BROADCAST_DATA_DIR", ".data")?,
            static_dir: path("BROADCAST_STATIC_DIR", "dist/web")?,
            skill_path: path("BROADCAST_SKILL_PATH", "skills/manage-broadcast/SKILL.md")?,
            codex_home,
            host: host(variable("BROADCAST_HOST").as_deref().unwrap_or("0.0.0.0"))?,
            port: port(variable("BROADCAST_PORT").as_deref().unwrap_or("47823"))?,
            trusted_origins: trusted_origins(variable(TRUSTED_ORIGINS_ENV).as_deref())?,
            launcher: launcher(
                variable("BROADCAST_INSTANCE_TOKEN").unwrap_or_default().trim(),
                variable("BROADCAST_INSTANCE_SECRET").unwrap_or_default().trim(),
            )?,
            version: variable("BROADCAST_VERSION").unwrap_or("development".into()).trim().to_owned(),
        })
    }
}

fn launcher(token: &str, secret: &str) -> anyhow::Result<Option<Launcher>> {
    if token.is_empty() {
        return Ok(None);
    }
    let identifier = |value: &str, hex_only: bool, range: std::ops::RangeInclusive<usize>| {
        range.contains(&value.len())
            && value.bytes().all(|byte| {
                byte == b'-' || if hex_only { byte.is_ascii_hexdigit() } else { byte.is_ascii_alphanumeric() }
            })
    };
    if !identifier(token, false, 16..=128) {
        bail!("BROADCAST_INSTANCE_TOKEN must be an identifier");
    }
    if !identifier(secret, true, 32..=128) {
        bail!("BROADCAST_INSTANCE_SECRET must be set in launcher mode");
    }
    Ok(Some(Launcher { token: token.to_owned(), secret: secret.to_owned() }))
}

/// `resolvePort`; 0 binds an ephemeral port.
fn port(value: &str) -> anyhow::Result<u16> {
    value.trim().parse().context("BROADCAST_PORT must be an integer between 0 and 65535")
}

fn host(value: &str) -> anyhow::Result<IpAddr> {
    match value.trim() {
        "127.0.0.1" => Ok(Ipv4Addr::LOCALHOST.into()),
        "0.0.0.0" => Ok(Ipv4Addr::UNSPECIFIED.into()),
        _ => bail!("BROADCAST_HOST must be 127.0.0.1 or 0.0.0.0"),
    }
}

/// `parseTrustedOrigins`: exact HTTPS origins.
fn trusted_origins(value: Option<&str>) -> anyhow::Result<HashSet<String>> {
    let mut origins = HashSet::new();
    let Some(value) = value else { return Ok(origins) };
    let configured = value.trim();
    if configured.is_empty() {
        bail!("{TRUSTED_ORIGINS_ENV} must not be empty when configured");
    }
    for origin in configured.split(',').map(str::trim) {
        if origin.is_empty() || origin.contains('*') {
            bail!("{TRUSTED_ORIGINS_ENV} must be a comma-separated list of exact HTTPS origins");
        }
        let Ok(url) = Url::parse(origin) else {
            bail!("{TRUSTED_ORIGINS_ENV} must contain valid HTTPS origins");
        };
        let exact = url.scheme() == "https"
            && url.username().is_empty()
            && url.password().is_none()
            && url.path() == "/"
            && url.query().is_none_or(str::is_empty)
            && url.fragment().is_none_or(str::is_empty);
        if !exact {
            bail!(
                "{TRUSTED_ORIGINS_ENV} must contain exact HTTPS origins without paths, queries, fragments, or credentials"
            );
        }
        if !origins.insert(url.origin().ascii_serialization()) {
            bail!("{TRUSTED_ORIGINS_ENV} must not contain duplicate origins");
        }
    }
    Ok(origins)
}
