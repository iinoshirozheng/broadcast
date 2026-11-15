//! The realtime hub: one Durable Object that fans revisions out to WebSocket clients and keeps
//! the shared board display settings and the agent runners' last heartbeats.

use broadcast_domain::js;
use serde_json::{Map, Value, json};
use worker::{
    DurableObject, Env, Error, ListOptions, Method, Request, RequestInit, Response, Result, State,
    Stub, WebSocket, WebSocketIncomingMessage, WebSocketPair, durable_object, wasm_bindgen,
    wasm_bindgen::JsValue,
};

use crate::http;

pub const DISPLAY_SETTINGS_PREFIX: &str = "broadcast.project-board-display-settings.v3.";
const RUNNER_PREFIX: &str = "runner:";
/// Runners beat every 15 seconds; three missed beats mark the agent offline.
const RUNNER_PRESENCE_MS: f64 = 45_000.0;

fn failed<T>(error: crate::error::Failure) -> Result<T> {
    Err(match error {
        crate::error::Failure::Internal(message) => Error::RustError(message),
        crate::error::Failure::Api(error) => Error::RustError(error.message),
    })
}

fn respond(response: crate::error::Result<Response>) -> Result<Response> {
    response.or_else(failed)
}

fn error(status: u16, code: &str, message: &str) -> crate::error::Result<Response> {
    http::json(
        status,
        &json!({ "error": { "code": code, "message": message } }),
    )
}

#[durable_object]
pub struct RealtimeHub {
    state: State,
}

impl RealtimeHub {
    async fn client_storage(&self, mut request: Request) -> crate::error::Result<Response> {
        let storage = self.state.storage();
        match request.method() {
            Method::Get => {
                let stored = storage
                    .list_with_options(ListOptions::new().prefix(DISPLAY_SETTINGS_PREFIX))
                    .await?;
                let mut entries = Map::new();
                for entry in stored.entries() {
                    let entry = worker::js_sys::Array::from(&entry.map_err(Error::from)?);
                    let value: Value =
                        serde_wasm_bindgen::from_value(entry.get(1)).map_err(Error::from)?;
                    entries.insert(entry.get(0).as_string().unwrap_or_default(), value);
                }
                http::json(200, &json!({ "entries": entries }))
            }
            Method::Patch => {
                let update: Value = request.json().await?;
                let key = update["key"].as_str().unwrap_or_default();
                match update["value"].as_str() {
                    Some(value) => storage.put(key, value).await?,
                    None => {
                        storage.delete(key).await?;
                    }
                }
                http::empty(204)
            }
            _ => {
                let mut response = error(405, "METHOD_NOT_ALLOWED", "Method not allowed")?;
                response.headers_mut().set("allow", "GET, PATCH")?;
                Ok(response)
            }
        }
    }

    /// PUT records a runner's heartbeat; GET lists the runners heard from within the window.
    async fn runners(&self, mut request: Request) -> crate::error::Result<Response> {
        let storage = self.state.storage();
        match request.method() {
            Method::Get => {
                let stored = storage
                    .list_with_options(ListOptions::new().prefix(RUNNER_PREFIX))
                    .await?;
                let now = worker::js_sys::Date::now();
                let mut runners = Vec::new();
                for entry in stored.entries() {
                    let entry = worker::js_sys::Array::from(&entry.map_err(Error::from)?);
                    let runner: Value =
                        serde_json::from_str(&entry.get(1).as_string().unwrap_or_default())
                            .unwrap_or(Value::Null);
                    let seen = worker::js_sys::Date::parse(
                        runner["lastSeenAt"].as_str().unwrap_or_default(),
                    );
                    if now - seen < RUNNER_PRESENCE_MS {
                        runners.push(runner);
                    }
                }
                http::json(200, &json!({ "runners": runners }))
            }
            Method::Put => {
                let runner: Value = request.json().await?;
                let key = format!(
                    "{RUNNER_PREFIX}{}",
                    runner["agentId"].as_str().unwrap_or_default()
                );
                storage.put(&key, runner.to_string()).await?;
                http::empty(204)
            }
            _ => {
                let mut response = error(405, "METHOD_NOT_ALLOWED", "Method not allowed")?;
                response.headers_mut().set("allow", "GET, PUT")?;
                Ok(response)
            }
        }
    }

    async fn broadcast(&self, mut request: Request) -> crate::error::Result<Response> {
        let payload: Value = request.json().await?;
        let Some(revision) =
            js::safe_integer(payload.get("revision")).filter(|&revision| revision >= 0)
        else {
            return error(400, "INVALID_REVISION", "revision must be non-negative");
        };
        let message = json!({ "type": "revision", "revision": revision }).to_string();
        for socket in self.state.get_websockets() {
            // The runtime delivers the close or error event of a stale socket.
            let _ = socket.send_with_str(&message);
        }
        http::empty(204)
    }
}

impl DurableObject for RealtimeHub {
    fn new(state: State, _env: Env) -> Self {
        Self { state }
    }

    async fn fetch(&self, request: Request) -> Result<Response> {
        let path = request.path();
        match path.as_str() {
            "/connect" => {
                let upgrade = request
                    .headers()
                    .get("upgrade")?
                    .map(|value| value.to_lowercase());
                if upgrade.as_deref() != Some("websocket") {
                    let mut response = respond(error(
                        426,
                        "WEBSOCKET_REQUIRED",
                        "A WebSocket upgrade is required",
                    ))?;
                    response.headers_mut().set("upgrade", "websocket")?;
                    return Ok(response);
                }
                let pair = WebSocketPair::new()?;
                self.state.accept_web_socket(&pair.server);
                Response::from_websocket(pair.client)
            }
            "/client-storage" => respond(self.client_storage(request).await),
            "/runners" => respond(self.runners(request).await),
            "/broadcast" if request.method() == Method::Post => {
                respond(self.broadcast(request).await)
            }
            _ => respond(error(404, "NOT_FOUND", "Resource not found")),
        }
    }

    async fn websocket_message(
        &self,
        socket: WebSocket,
        _message: WebSocketIncomingMessage,
    ) -> Result<()> {
        socket.close(Some(1008), Some("Client messages are not supported"))
    }

    async fn websocket_close(
        &self,
        _socket: WebSocket,
        _code: usize,
        _reason: String,
        _clean: bool,
    ) -> Result<()> {
        Ok(())
    }

    async fn websocket_error(&self, _socket: WebSocket, _error: Error) -> Result<()> {
        Ok(())
    }
}

fn stub(env: &Env) -> Result<Stub> {
    env.durable_object("REALTIME_HUB")?.get_by_name("global")
}

/// A request to the hub at `https://realtime.internal{path}`.
pub async fn send(env: &Env, method: Method, path: &str, body: Option<&Value>) -> Result<Response> {
    let mut init = RequestInit::new();
    init.with_method(method);
    if let Some(body) = body {
        let headers = worker::Headers::new();
        headers.set("content-type", "application/json")?;
        init.with_headers(headers)
            .with_body(Some(JsValue::from_str(&body.to_string())));
    }
    let request = Request::new_with_init(&format!("https://realtime.internal{path}"), &init)?;
    stub(env)?.fetch_with_request(request).await
}

/// Forwards a client's WebSocket upgrade to the hub.
pub async fn connect(env: &Env, request: &Request) -> Result<Response> {
    let mut init = RequestInit::new();
    init.with_headers(request.headers().clone());
    let request = Request::new_with_init("https://realtime.internal/connect", &init)?;
    stub(env)?.fetch_with_request(request).await
}
