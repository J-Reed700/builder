//! Session creation, lookup, and lifetime ownership.
use super::{Session, SessionGuard, Store, now, private_lock_options};
use crate::protocol::{Message, Role};
use anyhow::{Context, Result, ensure};
use fs2::FileExt;
use rusqlite::{Transaction, params};
use std::path::{Path, PathBuf};

pub(super) fn insert_session(
    tx: &Transaction<'_>,
    title: &str,
    profile: &str,
    workspace: &Path,
    system: &str,
) -> Result<String> {
    let id = uuid::Uuid::new_v4().to_string();
    tx.execute(
        "INSERT INTO sessions VALUES (?1,?2,?3,?4,?5)",
        params![
            id,
            title.chars().take(80).collect::<String>(),
            profile,
            workspace.to_string_lossy(),
            now()
        ],
    )?;
    tx.execute(
        "INSERT INTO messages(session_id,body) VALUES (?1,?2)",
        params![
            id,
            serde_json::to_string(&Message::text(Role::System, system))?
        ],
    )?;
    Ok(id)
}

impl Store {
    pub fn create(
        &mut self,
        title: &str,
        profile: &str,
        workspace: &Path,
        system: &str,
    ) -> Result<String> {
        let workspace = workspace
            .canonicalize()
            .context("Session workspace does not exist")?;
        ensure!(workspace.is_dir(), "Session workspace must be a directory");
        let tx = self.journal_transaction()?;
        let id = insert_session(&tx, title, profile, &workspace, system)?;
        tx.commit()?;
        Ok(id)
    }
    pub fn lock(&self, id: &str) -> Result<SessionGuard> {
        // Validate before using an externally supplied ID in a filename.
        uuid::Uuid::parse_str(id).context("Invalid session ID")?;
        let file = private_lock_options().open(self.home.join(format!("{id}.lock")))?;
        file.try_lock_exclusive()
            .context("This session is already open in another Builder process")?;
        Ok(SessionGuard { file })
    }
    pub fn resolve(&self, prefix: &str) -> Result<Session> {
        let matches: Vec<_> = self
            .sessions_where("")?
            .into_iter()
            .filter(|s| s.id.starts_with(prefix))
            .collect();
        ensure!(
            matches.len() == 1,
            "Session prefix must match exactly one session (matched {})",
            matches.len()
        );
        Ok(matches.into_iter().next().unwrap())
    }
    /// User conversations, newest first. Subagent child sessions are
    /// excluded; `resolve` still finds them by ID for export and inspection.
    pub fn sessions(&self) -> Result<Vec<Session>> {
        self.sessions_where("WHERE id NOT IN (SELECT session_id FROM subagent_sessions)")
    }
    fn sessions_where(&self, filter: &str) -> Result<Vec<Session>> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT id,title,profile,workspace,updated_at FROM sessions {filter} ORDER BY updated_at DESC"
        ))?;
        let rows = stmt.query_map([], |r| {
            Ok(Session {
                id: r.get(0)?,
                title: r.get(1)?,
                profile: r.get(2)?,
                workspace: PathBuf::from(r.get::<_, String>(3)?),
                updated_at: r.get(4)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }
}
