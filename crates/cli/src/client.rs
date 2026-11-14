//! Requests to the Broadcast service and how its replies map to CLI results and errors.

use std::collections::BTreeMap;
use std::process::Stdio;

use serde_json::{Map, Value};
use tokio::io::AsyncWriteExt;
use url::Url;

use crate::error::{CliError, Result, usage};
use crate::target::Target;

pub struct Reply {
    pub status: u16,
    pub content_type: Option<String>,
    pub content_length: Option<String>,
    pub body: Vec<u8>,
}

pub struct Api {
    base: String,
    /// `None` sends through `curl.exe` on the Windows side of WSL.
    http: Option<reqwest::Client>,
}

impl Api {
    pub fn new(target: Target) -> Result<Self> {
        let base = normalize_base_url(&target.url)?;
        let http = if target.windows_transport {
            None
        } else {
            let client = reqwest::Client::builder().no_proxy().build();
            Some(client.map_err(|error| CliError::new("HTTP client is not available", "CLIENT_UNAVAILABLE", 3).details(error))?)
        };
        Ok(Self { base, http })
    }

    async fn send(&self, method: &str, pathname: &str, headers: &[(&str, &str)], body: Option<Vec<u8>>) -> Result<Reply> {
        let mut all = BTreeMap::from([("accept", "application/json"), ("x-broadcast-client", "broadcast")]);
        all.extend(headers.iter().copied());
        let reply = match self.resolve(pathname) {
            Ok(url) => match &self.http {
                Some(http) => fetch(http, method, url, &all, body).await,
                None => fetch_through_windows(method, url, &all, body).await,
            },
            Err(error) => Err(error),
        };
        let reply = reply.map_err(|details| {
            CliError::new(format!("Cannot reach broadcast service at {}", self.base), "SERVICE_UNAVAILABLE", 3).details(details)
        })?;
        if (200..300).contains(&reply.status) {
            return Ok(reply);
        }
        let payload = read_payload(&reply.body)?;
        Err(api_error(&payload, reply.status))
    }

    fn resolve(&self, pathname: &str) -> std::result::Result<Url, String> {
        let base = Url::parse(&format!("{}/", self.base)).map_err(|error| error.to_string())?;
        base.join(pathname.strip_prefix('/').unwrap_or(pathname)).map_err(|error| error.to_string())
    }

    pub async fn request(&self, method: &str, pathname: &str, body: Option<Value>) -> Result<Map<String, Value>> {
        let headers: &[(&str, &str)] = if body.is_some() { &[("content-type", "application/json")] } else { &[] };
        let reply = self.send(method, pathname, headers, body.map(|body| body.to_string().into_bytes())).await?;
        json_object(&reply.body)
    }

    pub async fn download(&self, pathname: &str) -> Result<Reply> {
        self.send("GET", pathname, &[("accept", "*/*")], None).await
    }

    pub async fn upload(&self, pathname: &str, body: Vec<u8>, content_type: &str, filename: &str, kind: &str) -> Result<Map<String, Value>> {
        let filename = broadcast_domain::js::encode_uri_component(filename);
        let headers =
            [("content-type", content_type), ("x-broadcast-filename", filename.as_str()), ("x-broadcast-attachment-kind", kind)];
        let reply = self.send("POST", pathname, &headers, Some(body)).await?;
        json_object(&reply.body)
    }
}

fn normalize_base_url(raw: &str) -> Result<String> {
    let mut url = Url::parse(raw).map_err(|_| usage("BROADCAST_URL must be a valid URL"))?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err(usage("BROADCAST_URL must use http or https"));
    }
    let path = url.path().strip_suffix('/').unwrap_or(url.path()).to_owned();
    url.set_path(&path);
    url.set_query(None);
    url.set_fragment(None);
    let url = url.to_string();
    Ok(url.strip_suffix('/').unwrap_or(&url).to_owned())
}

async fn fetch(
    http: &reqwest::Client,
    method: &str,
    url: Url,
    headers: &BTreeMap<&str, &str>,
    body: Option<Vec<u8>>,
) -> std::result::Result<Reply, String> {
    let method = reqwest::Method::from_bytes(method.as_bytes()).map_err(|error| error.to_string())?;
    let mut request = http.request(method, url);
    for (name, value) in headers {
        request = request.header(*name, *value);
    }
    if let Some(body) = body {
        request = request.body(body);
    }
    let response = request.send().await.map_err(|error| error_chain(&error))?;
    let header = |name| response.headers().get(name).and_then(|value| value.to_str().ok()).map(str::to_owned);
    let (content_type, content_length) = (header("content-type"), header("content-length"));
    let status = response.status().as_u16();
    let body = response.bytes().await.map_err(|error| error_chain(&error))?.to_vec();
    Ok(Reply { status, content_type, content_length, body })
}

fn error_chain(error: &dyn std::error::Error) -> String {
    let mut message = error.to_string();
    let mut source = error.source();
    while let Some(cause) = source {
        message = format!("{message}: {cause}");
        source = cause.source();
    }
    message
}

/// WSL cannot reach the Windows loopback directly, so the request runs through `curl.exe`, which
/// prints the status, content type and size after a marker on stderr.
async fn fetch_through_windows(
    method: &str,
    url: Url,
    headers: &BTreeMap<&str, &str>,
    body: Option<Vec<u8>>,
) -> std::result::Result<Reply, String> {
    const MARKER: &str = "__BROADCAST_CURL_RESPONSE__";
    let mut args = ["--disable", "--noproxy", "*", "--silent", "--show-error", "--request", method].map(str::to_owned).to_vec();
    for (name, value) in headers {
        args.extend(["--header".to_owned(), format!("{name}: {value}")]);
    }
    if body.is_some() {
        args.extend(["--data-binary".to_owned(), "@-".to_owned()]);
    }
    args.extend([
        "--write-out".to_owned(),
        format!("%{{stderr}}{MARKER}%{{http_code}}\t%{{content_type}}\t%{{size_download}}"),
        "--url".to_owned(),
        url.to_string(),
    ]);
    let mut child = tokio::process::Command::new("curl.exe")
        .args(&args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| error.to_string())?;
    let mut stdin = child.stdin.take().ok_or("curl.exe stdin is not available")?;
    let writer = tokio::spawn(async move {
        if let Some(body) = body {
            stdin.write_all(&body).await?;
        }
        stdin.shutdown().await
    });
    let output = child.wait_with_output().await.map_err(|error| error.to_string())?;
    writer.await.map_err(|error| error.to_string())?.map_err(|error| error.to_string())?;
    let error_text = String::from_utf8_lossy(&output.stderr);
    if !output.status.success() {
        let trimmed = broadcast_domain::js::trim(&error_text);
        let code = output.status.code().map_or_else(|| "null".to_owned(), |code| code.to_string());
        return Err(if trimmed.is_empty() { format!("curl.exe exited with {code}") } else { trimmed.to_owned() });
    }
    let Some(index) = error_text.rfind(MARKER) else {
        return Err("curl.exe did not return HTTP response metadata".to_owned());
    };
    let mut metadata = error_text[index + MARKER.len()..].split('\t');
    let status = broadcast_domain::js::string_to_number(metadata.next().unwrap_or(""));
    if status.trunc() != status || !(100.0..=599.0).contains(&status) {
        return Err("curl.exe returned invalid HTTP response metadata".to_owned());
    }
    let present = |value: Option<&str>| value.filter(|value| !value.is_empty()).map(str::to_owned);
    let (content_type, content_length) = (present(metadata.next()), present(metadata.next()));
    Ok(Reply { status: status as u16, content_type, content_length, body: output.stdout })
}

/// An empty body reads as `{}`.
fn read_payload(body: &[u8]) -> Result<Value> {
    let text = String::from_utf8_lossy(body);
    let text = text.strip_prefix('\u{FEFF}').unwrap_or(&text);
    if text.is_empty() {
        return Ok(Value::Object(Map::new()));
    }
    serde_json::from_str(text)
        .map_err(|_| CliError::new("Broadcast service returned invalid JSON", "INVALID_RESPONSE", 4))
}

fn json_object(body: &[u8]) -> Result<Map<String, Value>> {
    match read_payload(body)? {
        Value::Object(object) => Ok(object),
        _ => Err(CliError::new("Broadcast service returned an invalid JSON response", "INVALID_RESPONSE", 4)),
    }
}

fn api_error(payload: &Value, status: u16) -> CliError {
    let fallback_code = || Value::String(format!("HTTP_{status}"));
    let fallback_message = || format!("Broadcast service returned HTTP {status}");
    let present = |value: Option<&Value>| value.filter(|value| !value.is_null()).cloned();
    let (code, message, details) = match payload.get("error") {
        Some(error @ (Value::Object(_) | Value::Array(_))) => (
            present(error.get("code")).unwrap_or_else(fallback_code),
            present(error.get("message")).map_or_else(fallback_message, |message| js_string(&message)),
            error.get("details").cloned(),
        ),
        error => (
            present(payload.get("code")).unwrap_or_else(fallback_code),
            match (present(payload.get("message")), error) {
                (Some(message), _) => js_string(&message),
                (None, Some(Value::String(error))) => error.clone(),
                (None, _) => fallback_message(),
            },
            payload.get("details").cloned(),
        ),
    };
    CliError::api(message, code, if status == 409 { 5 } else { 4 }, details)
}

/// `String(value)` for the JSON values an error message can hold.
pub fn js_string(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Array(items) => items.iter().map(|item| if item.is_null() { String::new() } else { js_string(item) }).collect::<Vec<_>>().join(","),
        Value::Object(_) => "[object Object]".to_owned(),
        other => other.to_string(),
    }
}
