//! `EventHub`: board changes pushed to every client over Server-Sent Events.

use std::convert::Infallible;
use std::time::Duration;

use axum::http::{HeaderName, HeaderValue, header};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use serde_json::{Map, Value};
use tokio::sync::broadcast;
use tokio_stream::{Stream, StreamExt};
use tokio_stream::wrappers::BroadcastStream;

/// `None` ends every stream on shutdown.
type Message = Option<(&'static str, String)>;

pub struct EventHub {
    sender: broadcast::Sender<Message>,
}

impl EventHub {
    pub fn new() -> Self {
        Self { sender: broadcast::channel(1024).0 }
    }

    /// Sends `{ type, projectId?, taskId?, ...value, at }` as event `kind`.
    pub fn emit(&self, kind: &'static str, value: Value) {
        let pick = |pointers: [&str; 3]| {
            pointers.into_iter().find_map(|pointer| value.pointer(pointer).filter(|found| !found.is_null()).cloned())
        };
        let project_id = pick(["/projectId", "/project/id", "/task/projectId"]);
        let task_id = pick(["/task/id", "/comment/taskId", "/attachment/taskId"]);

        let mut event = Map::new();
        event.insert("type".into(), kind.into());
        if let Some(project_id) = project_id {
            event.insert("projectId".into(), project_id);
        }
        if let Some(task_id) = task_id {
            event.insert("taskId".into(), task_id);
        }
        if let Value::Object(fields) = value {
            event.extend(fields);
        }
        let at = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string();
        event.insert("at".into(), at.into());
        // No subscribers is not an error.
        let _ = self.sender.send(Some((kind, Value::Object(event).to_string())));
    }

    pub fn close(&self) {
        let _ = self.sender.send(None);
    }

    pub fn connect(&self) -> Response {
        let changes = BroadcastStream::new(self.sender.subscribe())
            .filter_map(Result::ok)
            .take_while(Option::is_some)
            .filter_map(|message| message)
            .map(|(kind, data)| Event::default().event(kind).data(data));
        sse(tokio_stream::once(Event::default().comment("connected")).chain(changes))
    }
}

/// An event stream with a 20 s keep-alive comment and the JS server's headers.
pub fn sse(events: impl Stream<Item = Event> + Send + 'static) -> Response {
    let keep_alive = KeepAlive::new().interval(Duration::from_secs(20)).text("keep-alive");
    let mut response = Sse::new(events.map(Ok::<_, Infallible>)).keep_alive(keep_alive).into_response();
    let headers = response.headers_mut();
    headers.insert(header::CONTENT_TYPE, HeaderValue::from_static("text/event-stream; charset=utf-8"));
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache, no-transform"));
    headers.insert(HeaderName::from_static("x-accel-buffering"), HeaderValue::from_static("no"));
    response
}
