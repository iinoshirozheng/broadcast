//! SQLite persistence for the Broadcast board.
//!
//! One connection behind a mutex: SQLite serializes writers anyway, and every
//! mutation runs in a `BEGIN IMMEDIATE` transaction.

mod ai_chat;
mod attachments;
mod comments;
mod jira;
mod projects;
mod readme;
mod relations;
mod rows;
mod tasks;

use std::path::Path;
use std::sync::{Mutex, MutexGuard, PoisonError};

use broadcast_domain::Result;
use rusqlite::Connection;
use rusqlite_migration::{M, Migrations};

pub use ai_chat::{AiChatRunOutcome, AiChatThreadChanges, NewAiChatEvent, NewAiChatThread};
pub use attachments::NewAttachment;
pub use jira::{JiraActor, JiraTask};
pub use tasks::DeletedTask;

const MIGRATION_LIST: &[M<'_>] = &[M::up(include_str!(
    "../../../db/migrations/0001_initial.sql"
))];
const MIGRATIONS: Migrations<'_> = Migrations::from_slice(MIGRATION_LIST);

pub struct Store {
    connection: Mutex<Connection>,
}

impl Store {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut connection = Connection::open(path)?;
        connection.execute_batch(
            "PRAGMA foreign_keys = ON; PRAGMA journal_mode = WAL; PRAGMA busy_timeout = 5000;",
        )?;
        MIGRATIONS.to_latest(&mut connection)?;
        ai_chat::interrupt_abandoned_runs(&mut connection)?;
        Ok(Self {
            connection: Mutex::new(connection),
        })
    }

    fn connection(&self) -> MutexGuard<'_, Connection> {
        self.connection
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }
}

/// `new Date().toISOString()`.
pub fn now() -> String {
    chrono::Utc::now()
        .format("%Y-%m-%dT%H:%M:%S%.3fZ")
        .to_string()
}

/// `#nextCommentAttachmentRevision`: the shared cursor for comments and attachments.
fn next_revision(connection: &Connection) -> Result<i64> {
    Ok(connection.query_row(
        "UPDATE comment_attachment_revision SET value = value + 1 WHERE id = 1 RETURNING value",
        [],
        |row| row.get(0),
    )?)
}
