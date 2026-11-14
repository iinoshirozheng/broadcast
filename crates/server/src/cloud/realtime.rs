//! `/api/events` WebSocket in cloud mode: the cloud board's revision stream, relayed to this device's
//! loopback clients with this device's credentials.

use std::sync::Arc;

use axum::extract::ws::{self, WebSocket, WebSocketUpgrade};
use axum::extract::{FromRequest, Request};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use futures_util::{SinkExt, StreamExt};
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::protocol::CloseFrame;
use tokio_tungstenite::tungstenite::{self, Message};
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

use crate::App;
use crate::cloud::proxy::Remote;
use crate::http::query;
use crate::trust::Peer;

type Upstream = WebSocketStream<MaybeTlsStream<TcpStream>>;

pub fn is_upgrade(request: &Request) -> bool {
    request.headers().get(header::UPGRADE).is_some_and(|value| value.as_bytes().eq_ignore_ascii_case(b"websocket"))
}

/// Opens the cloud stream first, so a failure is reported as an HTTP status rather than a closed socket.
pub async fn relay(app: Arc<App>, request: Request) -> Response {
    if request.uri().path() != "/api/events" || !query(request.uri()).is_empty() {
        return reject(StatusCode::NOT_FOUND, "Not Found");
    }
    let peer = *request.extensions().get::<Peer>().expect("the trust gate identifies the peer");
    let upstream = match connect(&app, peer).await {
        Ok(upstream) => upstream,
        Err(status) => return reject(status, "WebSocket connection failed"),
    };
    match WebSocketUpgrade::from_request(request, &()).await {
        Ok(upgrade) => upgrade.on_upgrade(move |local| pump(local, upstream)),
        Err(rejection) => rejection.into_response(),
    }
}

fn reject(status: StatusCode, message: &str) -> Response {
    (status, [(header::CONTENT_TYPE, "text/plain; charset=utf-8")], format!("{message}\n")).into_response()
}

async fn connect(app: &App, peer: Peer) -> Result<Upstream, StatusCode> {
    let status = |code: u16| StatusCode::from_u16(code).unwrap_or(StatusCode::BAD_GATEWAY);
    peer.assert_local().map_err(|error| status(error.status))?;
    let config = app.cloud.read().await.map_err(|_| StatusCode::BAD_GATEWAY)?;
    let remote = Remote::of(&config).map_err(|error| status(error.status))?;
    let scheme = if remote.origin.starts_with("https:") { "wss" } else { "ws" };
    let (_, authority) = remote.origin.split_once("://").expect("origins carry a scheme");
    let mut target = format!("{scheme}://{authority}/api/events").into_client_request().map_err(|_| StatusCode::BAD_GATEWAY)?;
    target.headers_mut().insert(header::AUTHORIZATION, remote.authorization);
    let (upstream, _) = tokio_tungstenite::connect_async(target).await.map_err(|_| StatusCode::BAD_GATEWAY)?;
    Ok(upstream)
}

/// Cloud messages go down; a client message closes the client with 1008. Either side's close ends both.
async fn pump(mut local: WebSocket, upstream: Upstream) {
    let (mut upstream_tx, mut upstream_rx) = upstream.split();
    loop {
        tokio::select! {
            message = upstream_rx.next() => match message {
                Some(Ok(Message::Text(text))) => {
                    if local.send(ws::Message::Text(text.as_str().into())).await.is_err() {
                        break;
                    }
                }
                Some(Ok(Message::Binary(bytes))) => {
                    if local.send(ws::Message::Binary(bytes)).await.is_err() {
                        break;
                    }
                }
                Some(Ok(Message::Close(frame))) => {
                    if let Some((code, reason)) = frame.map(|frame| (u16::from(frame.code), frame.reason)).filter(|(code, _)| closable(*code)) {
                        let _ = local.send(ws::Message::Close(Some(ws::CloseFrame { code, reason: reason.as_str().into() }))).await;
                    }
                    break;
                }
                Some(Ok(_)) => {}
                Some(Err(_)) => {
                    let failed = ws::CloseFrame { code: 1011, reason: "Cloud realtime connection failed".into() };
                    let _ = local.send(ws::Message::Close(Some(failed))).await;
                    break;
                }
                None => break,
            },
            message = local.recv() => match message {
                Some(Ok(ws::Message::Text(_) | ws::Message::Binary(_))) => {
                    let reason = "Client messages are not supported";
                    let _ = local.send(ws::Message::Close(Some(ws::CloseFrame { code: 1008, reason: reason.into() }))).await;
                    let _ = upstream_tx.send(Message::Close(Some(CloseFrame { code: 1008.into(), reason: reason.into() }))).await;
                    break;
                }
                Some(Ok(ws::Message::Close(frame))) => {
                    // Completes the client's close handshake with the reply queued on receipt.
                    let _ = local.flush().await;
                    if let Some(frame) = frame.filter(|frame| closable(frame.code)) {
                        let reason = tungstenite::Utf8Bytes::from(frame.reason.as_str().to_owned());
                        let _ = upstream_tx.send(Message::Close(Some(CloseFrame { code: frame.code.into(), reason }))).await;
                    }
                    break;
                }
                Some(Ok(_)) => {}
                Some(Err(_)) | None => break,
            },
        }
    }
}

/// `closeOrTerminateWebSocket`: codes a peer may send in a close frame; others drop the connection.
fn closable(code: u16) -> bool {
    code >= 1000 && ![1004, 1005, 1006, 1015].contains(&code)
}
