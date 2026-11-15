//! `/api/client-storage`: web client preferences kept on this device. In cloud mode the board
//! display settings are shared through the cloud board instead.

use std::sync::Arc;

use axum::Extension;
use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{StatusCode, header};
use broadcast_domain::Error;
use serde_json::json;

use crate::App;
use crate::client_storage::{BOARD_DISPLAY_PREFIX, parse_update};
use crate::http::{Reply, empty, json, method_not_allowed, read_json};
use crate::trust::Peer;

pub async fn client_storage(
    State(app): State<Arc<App>>,
    Extension(peer): Extension<Peer>,
    request: Request,
) -> Reply {
    match request.method().as_str() {
        "GET" => {
            let mut entries = app.client_storage.entries().await?;
            let config = app.cloud.read().await.map_err(Error::from)?;
            if config.remote_url.is_some() {
                peer.assert_local()?;
                let shared = app
                    .cloud_proxy
                    .read_json(&app, &config, "/api/client-storage")
                    .await?;
                entries.retain(|key, _| !key.starts_with(BOARD_DISPLAY_PREFIX));
                if let Some(shared) = shared["entries"].as_object() {
                    entries.extend(
                        shared
                            .iter()
                            .filter(|(key, _)| key.starts_with(BOARD_DISPLAY_PREFIX))
                            .map(|(key, value)| (key.clone(), value.clone())),
                    );
                }
            }
            Ok(json(200, &json!({ "entries": entries })))
        }
        "PATCH" => {
            let update = parse_update(&read_json(request).await?)?;
            let shared = update.key.starts_with(BOARD_DISPLAY_PREFIX);
            let config = app.cloud.read().await.map_err(Error::from)?;
            if config.remote_url.is_some() && shared {
                peer.assert_local()?;
                let body = serde_json::to_vec(&json!({ "key": update.key, "value": update.value }))
                    .map_err(Error::from)?;
                let request = Request::patch("/api/client-storage")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(body))
                    .expect("valid request");
                return Ok(app.cloud_proxy.forward(&app, &config, request).await?);
            }
            let key = update.key.clone();
            app.client_storage.update(update).await?;
            if shared {
                app.events
                    .emit("client-storage.updated", json!({ "key": key }));
            }
            Ok(empty(StatusCode::NO_CONTENT))
        }
        _ => method_not_allowed(&["GET", "PATCH"]),
    }
}
