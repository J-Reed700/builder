//! Child sessions for delegated research. A child is an ordinary durable
//! session linked to the tool call that started it, so its transcript has the
//! same crash and audit guarantees. It stays out of session lists so it never
//! competes with the user's own conversations.
use super::{Store, insert_session};
use anyhow::{Context, Result, ensure};
use rusqlite::{OptionalExtension, params};
use std::path::PathBuf;

pub(super) const SCHEMA: &str = "CREATE TABLE IF NOT EXISTS subagent_sessions (
    session_id TEXT PRIMARY KEY REFERENCES sessions(id),
    parent_id TEXT NOT NULL REFERENCES sessions(id),
    call_id TEXT NOT NULL,
    UNIQUE(parent_id, call_id));";

impl Store {
    /// Create the child session for one parent tool call. A child inherits
    /// the parent's profile and workspace and cannot delegate further.
    pub fn create_subagent(
        &mut self,
        parent: &str,
        call_id: &str,
        title: &str,
        system: &str,
    ) -> Result<String> {
        ensure!(!call_id.is_empty(), "Subagent call ID is empty");
        let tx = self.journal_transaction()?;
        let (profile, workspace): (String, String) = tx
            .query_row(
                "SELECT profile,workspace FROM sessions WHERE id=?1",
                [parent],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?
            .context("Parent session not found")?;
        let nested: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM subagent_sessions WHERE session_id=?1)",
            [parent],
            |r| r.get(0),
        )?;
        ensure!(!nested, "A subagent cannot start another subagent");
        let id = insert_session(&tx, title, &profile, &PathBuf::from(workspace), system)?;
        tx.execute(
            "INSERT INTO subagent_sessions VALUES (?1,?2,?3)",
            params![id, parent, call_id],
        )
        .context("This tool call already started a subagent")?;
        tx.commit()?;
        Ok(id)
    }

    pub fn is_subagent(&self, session: &str) -> Result<bool> {
        Ok(self.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM subagent_sessions WHERE session_id=?1)",
            [session],
            |r| r.get(0),
        )?)
    }
}
