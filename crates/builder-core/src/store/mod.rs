//! Concrete SQLite repository. Connections and transaction boundaries are private
//! to this module; domain models never issue SQL. Public methods remain on Store.
mod code_index;
mod database;
mod execution;
mod memory;
mod messages;
mod migrations;
mod pages;
mod provider;
mod recovery;
mod research;
mod schedules;
mod sessions;
mod subagents;
mod todos;

use crate::protocol::Message;
use anyhow::{Context, Result};
use fs2::FileExt;
use rusqlite::{Connection, Transaction, TransactionBehavior, params};
use serde::Serialize;
use std::{cell::RefCell, collections::HashMap, fs::File, path::PathBuf};

pub use crate::execution::{AttemptOutcome, ToolOutcome, ToolRunState};
pub use pages::{ChatSession, HistoryEntry, HistoryPage};
pub use schedules::SchedulerGuard;
use sessions::insert_session;

pub const INTERRUPTED_SIDE_EFFECT_FREE: &str = "ERROR: Interrupted before this result was saved. The call has no side effects; repeat it if the result is still needed.";
pub struct Store {
    conn: Connection,
    /// Derived repository indexes live outside the authoritative conversation
    /// journal, one database per checkout, opened on first use. A bulk index
    /// write can never take the writer lock needed to save a user message,
    /// tool claim, or tool result, nor block another checkout's index.
    indexes: RefCell<HashMap<String, Connection>>,
    home: PathBuf,
}

#[derive(Debug, Serialize)]
pub struct Session {
    pub id: String,
    pub title: String,
    pub profile: String,
    pub workspace: PathBuf,
    pub updated_at: String,
}

/// Held for the entire agent lifetime; prevents two processes driving one session.
pub struct SessionGuard {
    file: File,
}

/// The sole-writer capability for one checkout's index across Builder
/// processes. Every index mutation requires it, so SQLite never sees two
/// writers on one index database and readers never wait on a writer (WAL).
/// Losing the process releases the advisory lock automatically.
pub struct CodeIndexGuard {
    file: File,
    scope: String,
}

impl CodeIndexGuard {
    pub fn scope(&self) -> &str {
        &self.scope
    }
}

impl Drop for SessionGuard {
    fn drop(&mut self) {
        let _ = FileExt::unlock(&self.file);
    }
}

impl Drop for CodeIndexGuard {
    fn drop(&mut self) {
        let _ = FileExt::unlock(&self.file);
    }
}

fn private_lock_options() -> std::fs::OpenOptions {
    let mut options = File::options();
    options.create(true).truncate(false).read(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options
}

impl Store {
    /// Acquire the journal writer at the transaction boundary. SQLite's
    /// default DEFERRED mode can create a read snapshot and then fail an
    /// upgrade immediately with SQLITE_BUSY_SNAPSHOT; IMMEDIATE makes writer
    /// contention happen before any transaction work is performed.
    fn journal_transaction(&mut self) -> Result<Transaction<'_>> {
        self.conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .context("Authoritative journal remained busy before a durable write")
    }

    /// A second connection to the same journal, for work that runs beside
    /// this one, such as a subagent's child session.
    pub fn reopen(&self) -> Result<Self> {
        Self::open(&self.home)
    }
}

fn now() -> String {
    chrono::Utc::now().to_rfc3339()
}

fn insert_message(tx: &Transaction<'_>, id: &str, message: &Message) -> Result<()> {
    tx.execute(
        "INSERT INTO messages(session_id,body) VALUES (?1,?2)",
        params![id, serde_json::to_string(message)?],
    )?;
    Ok(())
}
fn touch(tx: &Transaction<'_>, id: &str) -> Result<()> {
    tx.execute(
        "UPDATE sessions SET updated_at=?2 WHERE id=?1",
        params![id, now()],
    )?;
    Ok(())
}
