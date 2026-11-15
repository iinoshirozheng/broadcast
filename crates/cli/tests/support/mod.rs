//! Serves a Broadcast server in-process on an ephemeral loopback port, and runs the `broadcast`
//! binary against it.

#![allow(dead_code)] // Each test crate uses its own subset.

use std::net::Ipv4Addr;
use std::path::Path;
use std::process::Command;

use broadcast_server::{Config, Server};
use serde_json::Value;
use tempfile::TempDir;
use tokio::net::TcpListener;

pub struct Board {
    pub url: String,
    dir: TempDir,
}

impl Board {
    pub async fn start() -> Self {
        let dir = tempfile::tempdir().expect("temporary directory");
        let config = Config {
            data_dir: dir.path().join("data"),
            static_dir: dir.path().join("static"),
            skill_path: dir.path().join("SKILL.md"),
            codex_home: dir.path().join("codex-home"),
            codex_executable: dir.path().join("codex").to_string_lossy().into_owned(),
            host: Ipv4Addr::LOCALHOST.into(),
            port: 0,
            trusted_origins: Default::default(),
            launcher: None,
            version: "test".into(),
        };
        std::fs::create_dir(&config.data_dir).expect("data directory");
        let listener = TcpListener::bind((config.host, 0)).await.expect("listen");
        let url = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
        let server = Server::open(config).expect("open the server");
        tokio::spawn(server.serve(listener, std::future::pending()));
        Self { url, dir }
    }

    pub fn dir(&self) -> &Path {
        self.dir.path()
    }

    /// POSTs `body` as JSON to `path`.
    pub async fn post(&self, path: &str, body: Value) {
        let response = reqwest::Client::builder()
            .no_proxy()
            .build()
            .unwrap()
            .post(format!("{}{path}", self.url))
            .header("content-type", "application/json")
            .body(body.to_string())
            .send()
            .await
            .expect("request");
        assert!(response.status().is_success(), "{path}: {}", response.status());
    }

    /// Runs `broadcast` against this board; the JSON it printed on stdout, or on stderr when it
    /// failed, with its exit code.
    pub async fn broadcast(&self, args: &[&str]) -> (i32, Value) {
        let mut command = Command::new(env!("CARGO_BIN_EXE_broadcast"));
        command
            .args(args)
            .env_clear()
            .env("BROADCAST_URL", &self.url)
            .env("CODEX_THREAD_ID", "test-thread");
        if let Some(path) = std::env::var_os("PATH") {
            command.env("PATH", path);
        }
        let output = tokio::task::spawn_blocking(move || command.output())
            .await
            .unwrap()
            .expect("run broadcast");
        let code = output.status.code().expect("exit code");
        let printed = if code == 0 {
            output.stdout
        } else {
            output.stderr
        };
        let text = String::from_utf8_lossy(&printed);
        let json = serde_json::from_str(&text).unwrap_or_else(|_| panic!("{code}: {text}"));
        (code, json)
    }
}
