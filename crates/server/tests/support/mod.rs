//! Runs the `broadcast-server` binary on an ephemeral loopback port with its data, and its Codex
//! home, in a temporary directory.

#![allow(dead_code)] // Each test crate uses its own subset.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};

use axum::Router;
use axum::http::Uri;
use reqwest::{Client, Method, RequestBuilder, Response};
use serde_json::{Value, json};
use tempfile::TempDir;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::oneshot;

pub struct Server {
    // Declared before `dir` so the process is killed before its data is removed.
    child: Child,
    pub port: u16,
    pub client: Client,
    dir: TempDir,
}

pub struct Builder {
    command: Command,
    dir: TempDir,
}

impl Server {
    /// A server with its data in a fresh temporary directory, listening on 127.0.0.1.
    pub fn builder() -> Builder {
        let dir = tempfile::tempdir().expect("temporary directory");
        let mut command = Command::new(env!("CARGO_BIN_EXE_broadcast-server"));
        command
            .env("BROADCAST_DATA_DIR", dir.path())
            .env("BROADCAST_HOST", "127.0.0.1")
            .env("BROADCAST_PORT", "0")
            .env("CODEX_HOME", dir.path().join("codex-home"));
        Builder { command, dir }
    }

    pub async fn start() -> Self {
        Self::builder().start().await
    }

    pub fn dir(&self) -> &Path {
        self.dir.path()
    }

    /// See [`Builder::codex_home`].
    pub fn codex_home(&self) -> PathBuf {
        self.dir().join("codex-home")
    }

    pub fn url(&self, path: &str) -> String {
        format!("http://127.0.0.1:{}{path}", self.port)
    }

    /// The server's SQLite database.
    pub fn database(&self) -> rusqlite::Connection {
        rusqlite::Connection::open(self.dir().join("broadcast.sqlite")).expect("open the database")
    }

    pub fn request(&self, method: Method, path: &str) -> RequestBuilder {
        self.client.request(method, self.url(path))
    }

    pub async fn get(&self, path: &str) -> (u16, Value) {
        json(self.request(Method::GET, path).send().await.expect("request")).await
    }

    /// Sends `body` as JSON.
    pub async fn send(&self, method: Method, path: &str, body: Value) -> (u16, Value) {
        self.send_with(method, path, &[], body).await
    }

    /// Sends `body` as JSON with the extra `headers`.
    pub async fn send_with(
        &self,
        method: Method,
        path: &str,
        headers: &[(&str, &str)],
        body: Value,
    ) -> (u16, Value) {
        let mut request = with_json(self.request(method, path), &body);
        for (name, value) in headers {
            request = request.header(*name, *value);
        }
        json(request.send().await.expect("request")).await
    }

    /// POSTs `body` to `path` as the attachment `filename`.
    pub async fn upload(
        &self,
        path: &str,
        content_type: &str,
        filename: &str,
        body: &str,
    ) -> (u16, Value) {
        let filename: String = form_urlencoded::byte_serialize(filename.as_bytes()).collect();
        let request = self
            .request(Method::POST, path)
            .header("content-type", content_type)
            .header("x-broadcast-filename", filename)
            .header("x-broadcast-attachment-kind", "attachment")
            .body(body.to_owned());
        json(request.send().await.expect("request")).await
    }

    /// Where the server stores the attachment `id`.
    pub fn attachment_file(&self, id: &Value) -> PathBuf {
        self.dir().join("attachments").join(id.as_str().expect("attachment id"))
    }
}

impl Builder {
    pub fn dir(&self) -> &Path {
        self.dir.path()
    }

    /// The Codex home the server reads: `.codex-global-state.json` and
    /// `process_manager/chat_processes.json` belong here.
    pub fn codex_home(&self) -> PathBuf {
        self.dir.path().join("codex-home")
    }

    /// Configures cloud mode against `remote_url` as Alice with the shared key
    /// `two-person-shared-key`.
    pub fn cloud(self, remote_url: &str) -> Self {
        write_cloud_config(
            self.dir(),
            json!({
                "version": 1,
                "remoteUrl": remote_url,
                "actorName": "Alice",
                "sharedKey": "two-person-shared-key",
                "projectMappings": {},
            }),
        );
        self
    }

    pub fn env(mut self, name: &str, value: impl AsRef<std::ffi::OsStr>) -> Self {
        self.command.env(name, value);
        self
    }

    pub async fn start(mut self) -> Server {
        let mut child = self
            .command
            .stdout(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .expect("start broadcast-server");
        let mut lines = BufReader::new(child.stdout.take().expect("stdout")).lines();
        let (ready, port) = oneshot::channel();
        // Keeps reading so the server never writes to a closed pipe.
        tokio::spawn(async move {
            let mut ready = Some(ready);
            while let Ok(Some(line)) = lines.next_line().await {
                if let Some(port) = line.strip_prefix("Broadcast listening on http://127.0.0.1:")
                    && let Some(ready) = ready.take()
                {
                    let _ = ready.send(port.parse::<u16>().expect("port"));
                }
            }
        });
        let port = port.await.expect("broadcast-server exited during startup");
        let client = Client::builder().no_proxy().build().expect("HTTP client");
        Server {
            child,
            port,
            client,
            dir: self.dir,
        }
    }
}

/// Writes the cloud settings the server reads from its data directory `dir`.
pub fn write_cloud_config(dir: &Path, config: Value) {
    std::fs::write(dir.join("cloud-companion.json"), config.to_string()).expect("cloud settings");
}

/// A loopback HTTP server standing in for the cloud board.
pub struct Upstream {
    pub url: String,
    /// The path and query of every request it answered, in order.
    pub requests: Arc<Mutex<Vec<String>>>,
}

impl Upstream {
    /// Answers every request with `reply` as JSON.
    pub async fn json(reply: Value) -> Self {
        let requests = Arc::new(Mutex::new(Vec::new()));
        let recorded = Arc::clone(&requests);
        let router = Router::new().fallback(move |uri: Uri| {
            recorded.lock().unwrap().push(uri.to_string());
            let reply = reply.clone();
            async move { axum::Json(reply) }
        });
        Self {
            url: serve(router).await,
            requests,
        }
    }

    pub fn calls(&self) -> Vec<String> {
        self.requests.lock().unwrap().clone()
    }
}

/// Serves `router` on an ephemeral loopback port and returns its origin.
pub async fn serve(router: Router) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("listen");
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, router).await });
    url
}

/// The first non-loopback IPv4 address of this machine.
pub fn lan_address() -> Option<std::net::Ipv4Addr> {
    if_addrs::get_if_addrs()
        .ok()?
        .into_iter()
        .find_map(|interface| match interface.ip() {
            std::net::IpAddr::V4(address) if !interface.is_loopback() => Some(address),
            _ => None,
        })
}

pub fn with_json(request: RequestBuilder, body: &Value) -> RequestBuilder {
    request
        .header("content-type", "application/json")
        .body(body.to_string())
}

/// The status and the JSON body of `response`; an empty body is `null`.
pub async fn json(response: Response) -> (u16, Value) {
    let status = response.status().as_u16();
    let text = response.text().await.expect("response body");
    let body = if text.is_empty() {
        Value::Null
    } else {
        serde_json::from_str(&text).unwrap_or_else(|_| panic!("{status} {text}"))
    };
    (status, body)
}
