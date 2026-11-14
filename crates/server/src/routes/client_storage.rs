//! `/api/client-storage`: web client preferences kept on this device.

use std::sync::Arc;

use axum::extract::{Request, State};
use axum::http::StatusCode;
use serde_json::json;

use crate::App;
use crate::client_storage::{BOARD_DISPLAY_PREFIX, parse_update};
use crate::http::{Reply, empty, json, method_not_allowed, read_json};

pub async fn client_storage(State(app): State<Arc<App>>, request: Request) -> Reply {
    match request.method().as_str() {
        "GET" => Ok(json(200, &json!({ "entries": app.client_storage.entries().await? }))),
        "PATCH" => {
            let update = parse_update(&read_json(request).await?)?;
            let key = update.key.clone();
            app.client_storage.update(update).await?;
            if key.starts_with(BOARD_DISPLAY_PREFIX) {
                app.events.emit("client-storage.updated", json!({ "key": key }));
            }
            Ok(empty(StatusCode::NO_CONTENT))
        }
        _ => method_not_allowed(&["GET", "PATCH"]),
    }
}
