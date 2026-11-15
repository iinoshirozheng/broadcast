//! Which Broadcast service a command talks to.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::Value;
use url::Url;

use crate::error::{CliError, Result, usage};

pub const DEFAULT_API_URL: &str = "http://127.0.0.1:47823";

/// The descriptor the source launcher writes in this checkout's `.data` directory. Packaged
/// wrappers point `BROADCAST_RUNTIME_FILE` at the app's own data directory instead.
const SOURCE_RUNTIME_FILE: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../.data/launcher-runtime.json"
);

pub struct Target {
    pub url: String,
    /// Under WSL the Windows launcher's loopback endpoint is reached through `curl.exe`.
    pub windows_transport: bool,
}

/// The process environment, with `--runtime-file` taking the place of `BROADCAST_RUNTIME_FILE`.
pub struct Env(HashMap<String, String>);

impl Env {
    pub fn new(runtime_file: Option<&str>) -> Self {
        let mut variables: HashMap<String, String> = std::env::vars_os()
            .map(|(name, value)| {
                (
                    name.to_string_lossy().into_owned(),
                    value.to_string_lossy().into_owned(),
                )
            })
            .collect();
        if let Some(runtime_file) = runtime_file {
            variables.insert("BROADCAST_RUNTIME_FILE".into(), runtime_file.to_owned());
        }
        Self(variables)
    }

    pub fn var(&self, name: &str) -> Option<&str> {
        self.0.get(name).map(String::as_str)
    }
}

struct Candidate {
    path: PathBuf,
    required: bool,
    windows_transport: bool,
}

pub fn broadcast_base_url(env: &Env) -> Result<Target> {
    if let Some(url) = env.var("BROADCAST_URL") {
        return Ok(Target {
            url: url.to_owned(),
            windows_transport: false,
        });
    }
    let wsl = env.var("WSL_DISTRO_NAME").is_some() || env.var("WSL_INTEROP").is_some();
    let candidates = if let Some(path) = env.var("BROADCAST_RUNTIME_FILE") {
        vec![Candidate {
            path: path.into(),
            required: true,
            windows_transport: false,
        }]
    } else if let Some(path) = env.var("BROADCAST_WSL_RUNTIME_FILE").filter(|_| wsl) {
        vec![Candidate {
            path: path.into(),
            required: true,
            windows_transport: true,
        }]
    } else {
        let windows = if wsl { wsl_runtime_file() } else { None };
        windows
            .map(|path| Candidate {
                path,
                required: false,
                windows_transport: true,
            })
            .into_iter()
            .chain([Candidate {
                path: SOURCE_RUNTIME_FILE.into(),
                required: false,
                windows_transport: false,
            }])
            .collect()
    };

    for candidate in candidates {
        let unreadable = |details: String| {
            CliError::new(
                "Cannot read the active Broadcast launcher endpoint",
                "SERVICE_UNAVAILABLE",
                3,
            )
            .details(details)
        };
        let text = match std::fs::read_to_string(&candidate.path) {
            Ok(text) => text,
            Err(error) if !candidate.required && error.kind() == std::io::ErrorKind::NotFound => {
                continue;
            }
            Err(error) => return Err(unreadable(error.to_string())),
        };
        let descriptor: Value =
            serde_json::from_str(&text).map_err(|error| unreadable(error.to_string()))?;
        let url = descriptor.get("url").and_then(Value::as_str);
        let Some(url) =
            url.filter(|_| descriptor.get("version").and_then(Value::as_f64) == Some(1.0))
        else {
            return Err(CliError::new(
                "The active Broadcast launcher endpoint is invalid",
                "INVALID_RESPONSE",
                4,
            ));
        };
        return Ok(Target {
            url: url.to_owned(),
            windows_transport: candidate.windows_transport,
        });
    }
    Ok(Target {
        url: DEFAULT_API_URL.to_owned(),
        windows_transport: false,
    })
}

/// `%APPDATA%\Broadcast\launcher-runtime.json` of the Windows user, as a WSL path.
fn wsl_runtime_file() -> Option<PathBuf> {
    let output = Command::new("cmd.exe")
        .args(["/d", "/u", "/s", "/c", "set APPDATA"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let units = output
        .stdout
        .as_chunks::<2>()
        .0
        .iter()
        .map(|pair| u16::from_le_bytes(*pair));
    let text: String = char::decode_utf16(units)
        .map(|c| c.unwrap_or(char::REPLACEMENT_CHARACTER))
        .collect();
    let app_data = text
        .split('\n')
        .map(|line| line.strip_suffix('\r').unwrap_or(line))
        .find_map(|line| {
            line.get(..8)
                .filter(|name| name.eq_ignore_ascii_case("APPDATA="))
                .map(|_| &line[8..])
        })?;
    let output = Command::new("wslpath")
        .args(["-u", app_data])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let path = String::from_utf8_lossy(&output.stdout);
    let path = broadcast_domain::js::trim(&path);
    (!path.is_empty()).then(|| {
        Path::new(path)
            .join("Broadcast")
            .join("launcher-runtime.json")
    })
}

/// Cloud and project-mapping commands talk to the local companion, which only listens on loopback.
pub fn companion_url(env: &Env) -> Result<Target> {
    let target = match env.var("BROADCAST_COMPANION_URL") {
        Some(url) => Target {
            url: url.to_owned(),
            windows_transport: false,
        },
        None => broadcast_base_url(env)?,
    };
    let url =
        Url::parse(&target.url).map_err(|_| usage("Local companion URL must be a valid URL"))?;
    let loopback = matches!(url.host_str(), Some("localhost" | "127.0.0.1" | "[::1]"));
    let path = url.path();
    let token = path.strip_prefix('/').unwrap_or(path);
    let token = token.strip_suffix('/').unwrap_or(token);
    let valid_path = path == "/"
        || ((16..=128).contains(&token.len())
            && token
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-'));
    let credentials =
        !url.username().is_empty() || url.password().is_some_and(|password| !password.is_empty());
    let extra = url.query().is_some_and(|query| !query.is_empty())
        || url.fragment().is_some_and(|hash| !hash.is_empty());
    if !loopback || !matches!(url.scheme(), "http" | "https") || credentials || !valid_path || extra
    {
        return Err(usage(
            "Local companion URL must be a loopback HTTP or HTTPS endpoint",
        ));
    }
    let url = url.to_string();
    Ok(Target {
        url: url.strip_suffix('/').unwrap_or(&url).to_owned(),
        windows_transport: target.windows_transport,
    })
}
