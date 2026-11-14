//! `client-storage.json`: web client preferences kept by the device rather than the browser.

use std::path::PathBuf;

use broadcast_domain::{Result, fields};
use serde_json::{Map, Value};
use tokio::sync::Mutex;

use crate::files;

/// Board display settings are shared with the cloud board in cloud mode and announced to other tabs.
pub const BOARD_DISPLAY_PREFIX: &str = "broadcast.project-board-display-settings.v3.";

pub struct Update {
    pub key: String,
    /// `None` removes the entry.
    pub value: Option<String>,
}

/// `parseClientStorageUpdate`.
pub fn parse_update(body: &Value) -> Result<Update> {
    let body = fields::object(body, &["key", "value"])?;
    let key = fields::required_string(body.get("key"), "key", 512)?;
    let value = fields::nullable_string(body.get("value"), "value", 100_000)?.flatten();
    Ok(Update { key, value })
}

pub struct ClientStorage {
    path: PathBuf,
    lock: Mutex<()>,
}

impl ClientStorage {
    pub fn new(path: PathBuf) -> Self {
        Self { path, lock: Mutex::new(()) }
    }

    async fn load(&self) -> Result<Map<String, Value>> {
        Ok(match files::read_json(&self.path).await? {
            Some(Value::Object(entries)) => entries,
            _ => Map::new(),
        })
    }

    pub async fn entries(&self) -> Result<Map<String, Value>> {
        let _guard = self.lock.lock().await;
        self.load().await
    }

    pub async fn update(&self, update: Update) -> Result<()> {
        let _guard = self.lock.lock().await;
        let mut entries = self.load().await?;
        match update.value {
            Some(value) => entries.insert(update.key, Value::String(value)),
            None => entries.shift_remove(&update.key),
        };
        Ok(files::write_private(&self.path, &entries, false).await?)
    }
}
