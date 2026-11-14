//! `/api/local/*`: capabilities only the device-local server offers.

use std::sync::Arc;

use axum::extract::{Request, State};
use serde_json::json;

use crate::App;
use crate::http::{Reply, json, method_not_allowed, read_json};

pub async fn host_runtime(State(app): State<Arc<App>>, request: Request) -> Reply {
    match request.method().as_str() {
        "GET" => Ok(json(200, &json!({ "runtime": app.host.current() }))),
        "PUT" => {
            let runtime = app.host.report(&read_json(request).await?)?;
            Ok(json(200, &json!({ "runtime": runtime })))
        }
        _ => method_not_allowed(&["GET", "PUT"]),
    }
}
