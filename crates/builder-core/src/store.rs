use crate::protocol::Message;
mod pages;
use crate::protocol::Role;
use anyhow::{Context, Result, ensure};
use fs2::FileExt;
pub use pages::{ChatSession, HistoryEntry, HistoryPage};
use rusqlite::{Connection, OptionalExtension, Transaction, params};
use serde::{Deserialize, Serialize};
use std::{
    fs::File,
    path::{Path, PathBuf},
};

pub struct Store {
    pub(crate) conn: Connection,
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

impl Drop for SessionGuard {
    fn drop(&mut self) {
        let _ = FileExt::unlock(&self.file);
    }
}

#[derive(Debug, Clone, Copy)]
pub enum AttemptOutcome {
    Complete,
    Failed,
}
impl AttemptOutcome {
    fn as_str(self) -> &'static str {
        match self {
            Self::Complete => "complete",
            Self::Failed => "failed",
        }
    }
}

/// Durable execution facts; legacy records have no inferred outcome.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolOutcome {
    #[default]
    Unknown,
    Succeeded,
    Changed,
    Failed,
    Denied,
    Uncertain,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolRunState {
    Unclaimed,
    Started,
    Finished,
}

impl Store {
    pub fn open(home: &Path) -> Result<Self> {
        crate::config::ensure_home(home)?;
        let mut options = File::options();
        options.create(true).write(true).truncate(false);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        // Seed with private permissions before SQLite creates its WAL files.
        options.open(home.join("builder.sqlite3"))?;
        let conn = Connection::open(home.join("builder.sqlite3"))?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL; PRAGMA foreign_keys=ON; BEGIN IMMEDIATE;")?;
        // Read the version under the write lock so simultaneous launches cannot
        // both attempt the same migration.
        let version: u32 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        ensure!(
            version <= 6,
            "Session database was created by a newer Builder version; upgrade Builder"
        );
        conn.execute_batch("CREATE TABLE IF NOT EXISTS sessions (
                id TEXT PRIMARY KEY, title TEXT NOT NULL, profile TEXT NOT NULL,
                workspace TEXT NOT NULL, updated_at TEXT NOT NULL);
            CREATE TABLE IF NOT EXISTS messages (
                seq INTEGER PRIMARY KEY AUTOINCREMENT, session_id TEXT NOT NULL REFERENCES sessions(id),
                body TEXT NOT NULL);
            CREATE INDEX IF NOT EXISTS messages_session ON messages(session_id, seq);
            CREATE TABLE IF NOT EXISTS attempts (
                id INTEGER PRIMARY KEY, session_id TEXT NOT NULL REFERENCES sessions(id),
                started_at TEXT NOT NULL, status TEXT NOT NULL, detail TEXT);
            CREATE TABLE IF NOT EXISTS tool_runs (
                session_id TEXT NOT NULL REFERENCES sessions(id), call_id TEXT NOT NULL,
                state TEXT NOT NULL CHECK(state IN ('started','finished')), result TEXT,
                PRIMARY KEY(session_id, call_id));")?;
        if version < 2 {
            conn.execute_batch(
                "ALTER TABLE messages ADD COLUMN active INTEGER NOT NULL DEFAULT 1;
                CREATE TABLE composer_drafts (
                    session_id TEXT PRIMARY KEY REFERENCES sessions(id), prompt TEXT NOT NULL);
                PRAGMA user_version=2;",
            )?;
        }
        if version < 3 {
            conn.execute_batch(
                "CREATE TABLE context_checkpoints (
                id INTEGER PRIMARY KEY, session_id TEXT NOT NULL REFERENCES sessions(id),
                through_seq INTEGER NOT NULL, context TEXT NOT NULL, created_at TEXT NOT NULL,
                active INTEGER NOT NULL DEFAULT 1);
                PRAGMA user_version=3;",
            )?;
        }
        if version < 4 {
            conn.execute_batch(crate::memory::SCHEMA)?;
        }
        if version < 5 {
            conn.execute_batch(
                "ALTER TABLE tool_runs ADD COLUMN outcome TEXT; PRAGMA user_version=5;",
            )?;
        }
        if version < 6 {
            conn.execute_batch(
                "CREATE TABLE IF NOT EXISTS chat_metadata (
                session_id TEXT PRIMARY KEY REFERENCES sessions(id),
                archived INTEGER NOT NULL DEFAULT 0 CHECK(archived IN (0,1)));
                PRAGMA user_version=6;",
            )?;
        }
        conn.execute_batch("COMMIT;")?;
        Ok(Self {
            conn,
            home: home.into(),
        })
    }
    pub fn create(
        &mut self,
        title: &str,
        profile: &str,
        workspace: &Path,
        system: &str,
    ) -> Result<String> {
        let id = uuid::Uuid::new_v4().to_string();
        let tx = self.conn.transaction()?;
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
        tx.commit()?;
        Ok(id)
    }
    pub fn lock(&self, id: &str) -> Result<SessionGuard> {
        // Validate before using an externally supplied ID in a filename.
        uuid::Uuid::parse_str(id).context("Invalid session ID")?;
        let file = File::options()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(self.home.join(format!("{id}.lock")))?;
        file.try_lock_exclusive()
            .context("This session is already open in another Builder process")?;
        Ok(SessionGuard { file })
    }
    pub fn resolve(&self, prefix: &str) -> Result<Session> {
        let matches: Vec<_> = self
            .sessions()?
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
    pub fn sessions(&self) -> Result<Vec<Session>> {
        let mut stmt = self.conn.prepare(
            "SELECT id,title,profile,workspace,updated_at FROM sessions ORDER BY updated_at DESC",
        )?;
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
    pub fn messages(&self, id: &str) -> Result<Vec<Message>> {
        let checkpoint: Option<(i64, String)> = self.conn.query_row(
            "SELECT through_seq,context FROM context_checkpoints WHERE session_id=?1 AND active=1 ORDER BY id DESC LIMIT 1",
            [id], |r| Ok((r.get(0)?, r.get(1)?))).optional()?;
        let Some((through, context)) = checkpoint else {
            return self.history_messages(id);
        };
        let mut messages: Vec<Message> = serde_json::from_str(&context)?;
        messages.extend(self.read_messages(id, &format!("AND active=1 AND seq>{through}"))?);
        Ok(messages)
    }
    pub fn history_messages(&self, id: &str) -> Result<Vec<Message>> {
        self.read_messages(id, "AND active=1")
    }
    /// Activate a complete checkpoint atomically; original rows and previous
    /// checkpoints remain durable. Never install a summary of stale context.
    pub fn checkpoint(
        &mut self,
        id: &str,
        expected: &[Message],
        context: &[Message],
    ) -> Result<()> {
        ensure!(
            self.messages(id)? == expected,
            "Conversation changed while compacting; history is intact"
        );
        let tx = self.conn.transaction()?;
        let through: i64 = tx.query_row(
            "SELECT MAX(seq) FROM messages WHERE session_id=?1",
            [id],
            |r| r.get(0),
        )?;
        tx.execute(
            "UPDATE context_checkpoints SET active=0 WHERE session_id=?1",
            [id],
        )?;
        tx.execute("INSERT INTO context_checkpoints(session_id,through_seq,context,created_at) VALUES (?1,?2,?3,?4)",
            params![id, through, serde_json::to_string(context)?, now()])?;
        touch(&tx, id)?;
        tx.commit()?;
        Ok(())
    }
    pub fn archived_messages(&self, id: &str) -> Result<Vec<Message>> {
        self.read_messages(id, "AND (active=0 OR seq<=(SELECT MAX(through_seq) FROM context_checkpoints WHERE session_id=messages.session_id))")
    }
    /// Call IDs stay reserved even when their turn has been rewound.
    pub fn used_call_ids(&self, id: &str) -> Result<std::collections::HashSet<String>> {
        let mut stmt = self.conn.prepare(
            "SELECT json_extract(call.value, '$.id') FROM messages AS message,
             json_each(message.body, '$.tool_calls') AS call WHERE message.session_id=?1",
        )?;
        let rows = stmt.query_map([id], |r| r.get::<_, String>(0))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// IDs with more than one durable tool-call occurrence, including calls in
    /// rewound turns. A resumed batch must never borrow another call's result.
    pub fn reused_call_ids(&self, id: &str) -> Result<std::collections::HashSet<String>> {
        let mut stmt = self.conn.prepare(
            "SELECT json_extract(call.value, '$.id') FROM messages AS message,
             json_each(message.body, '$.tool_calls') AS call WHERE message.session_id=?1
             GROUP BY json_extract(call.value, '$.id') HAVING COUNT(*) > 1",
        )?;
        let rows = stmt.query_map([id], |r| r.get::<_, String>(0))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    fn read_messages(&self, id: &str, filter: &str) -> Result<Vec<Message>> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT body FROM messages WHERE session_id=?1 {filter} ORDER BY seq"
        ))?;
        let rows = stmt.query_map([id], |r| r.get::<_, String>(0))?;
        rows.map(|r| Ok(serde_json::from_str(&r?)?)).collect()
    }
    /// Close pending calls without executing them, and optionally commit a new
    /// instruction in the same transaction. Claimed calls remain uncertain.
    pub fn interrupt_turn(&mut self, id: &str, next: Option<&str>) -> Result<usize> {
        if let Some(prompt) = next {
            ensure!(!prompt.trim().is_empty(), "Prompt is empty");
        }
        let messages = self.messages(id)?;
        let tx = self.conn.transaction()?;
        let uncertain = close_pending(&tx, id, &messages)?;
        if let Some(prompt) = next {
            insert_message(&tx, id, &Message::text(Role::User, prompt))?;
            tx.execute("DELETE FROM composer_drafts WHERE session_id=?1", [id])?;
        }
        touch(&tx, id)?;
        tx.commit()?;
        Ok(uncertain)
    }
    /// Archive the last user turn, retaining the original prompt as a durable
    /// composer draft. Workspace side effects and tool claims are never undone.
    pub fn rewind(&mut self, id: &str) -> Result<(String, usize)> {
        let messages = self.history_messages(id)?;
        let tx = self.conn.transaction()?;
        let (seq, body): (i64, String) = tx
            .query_row(
                "SELECT seq,body FROM messages WHERE session_id=?1 AND active=1
             AND json_extract(body, '$.role')='user' ORDER BY seq DESC LIMIT 1",
                [id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?
            .context("No previous user message to rewind")?;
        let prompt = serde_json::from_str::<Message>(&body)?
            .content
            .unwrap_or_default();
        let uncertain = close_pending(&tx, id, &messages)?;
        tx.execute(
            "UPDATE context_checkpoints SET active=0 WHERE session_id=?1",
            [id],
        )?;
        tx.execute(
            "UPDATE messages SET active=0 WHERE session_id=?1 AND seq>=?2 AND active=1",
            params![id, seq],
        )?;
        // Keep the current filesystem state explicit when removing tool context.
        if messages
            .iter()
            .rev()
            .take_while(|m| m.role != Role::User)
            .any(|m| !m.tool_calls.is_empty())
        {
            insert_message(
                &tx,
                id,
                &Message::text(
                    Role::System,
                    "The user rewound the last conversation turn. Its transcript is archived. Workspace changes from that turn remain; inspect current files before editing or repeating actions. Any interrupted execution may have had side effects.",
                ),
            )?;
        }
        tx.execute(
            "INSERT INTO composer_drafts(session_id,prompt) VALUES (?1,?2)
            ON CONFLICT(session_id) DO UPDATE SET prompt=excluded.prompt",
            params![id, prompt],
        )?;
        touch(&tx, id)?;
        tx.commit()?;
        Ok((prompt, uncertain))
    }
    pub fn composer_draft(&self, id: &str) -> Result<Option<String>> {
        Ok(self
            .conn
            .query_row(
                "SELECT prompt FROM composer_drafts WHERE session_id=?1",
                [id],
                |r| r.get(0),
            )
            .optional()?)
    }
    /// Mark a dropped model future in the audit without changing retry context.
    pub fn interrupt_attempts(&self, id: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE attempts SET status='failed',detail='Interrupted by user'
            WHERE session_id=?1 AND status='running'",
            [id],
        )?;
        Ok(())
    }
    pub fn append(&mut self, id: &str, message: &Message) -> Result<()> {
        let tx = self.conn.transaction()?;
        tx.execute(
            "INSERT INTO messages(session_id,body) VALUES (?1,?2)",
            params![id, serde_json::to_string(message)?],
        )?;
        tx.execute(
            "UPDATE sessions SET updated_at=?2 WHERE id=?1",
            params![id, now()],
        )?;
        tx.commit()?;
        Ok(())
    }
    pub fn begin_attempt(&self, id: &str) -> Result<i64> {
        self.conn.execute(
            "INSERT INTO attempts(session_id,started_at,status) VALUES (?1,?2,'running')",
            params![id, now()],
        )?;
        Ok(self.conn.last_insert_rowid())
    }
    pub fn finish_attempt(&self, attempt: i64, status: AttemptOutcome, detail: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE attempts SET status=?2,detail=?3 WHERE id=?1",
            params![attempt, status.as_str(), detail],
        )?;
        Ok(())
    }
    /// Claim before executing. An existing claim is never executed automatically.
    pub fn claim_tool(&self, session: &str, call: &str) -> Result<bool> {
        Ok(self.conn.execute(
            "INSERT OR IGNORE INTO tool_runs(session_id,call_id,state) VALUES (?1,?2,'started')",
            params![session, call],
        )? == 1)
    }
    pub fn tool_run_state(&self, session: &str, call: &str) -> Result<ToolRunState> {
        let state = self
            .conn
            .query_row(
                "SELECT state FROM tool_runs WHERE session_id=?1 AND call_id=?2",
                params![session, call],
                |row| row.get::<_, String>(0),
            )
            .optional()?;
        match state.as_deref() {
            None => Ok(ToolRunState::Unclaimed),
            Some("started") => Ok(ToolRunState::Started),
            Some("finished") => Ok(ToolRunState::Finished),
            Some(state) => anyhow::bail!("Unknown durable tool state: {state}"),
        }
    }
    pub fn tool_result(&self, session: &str, call: &str) -> Result<Option<String>> {
        let mut stmt = self
            .conn
            .prepare("SELECT result FROM tool_runs WHERE session_id=?1 AND call_id=?2")?;
        let mut rows = stmt.query(params![session, call])?;
        Ok(match rows.next()? {
            Some(r) => r.get(0)?,
            None => None,
        })
    }
    /// Restore a result omitted by a malformed legacy context projection.
    /// This never claims or executes the tool. A finished run missing its
    /// atomic result is conservatively repaired as uncertain.
    pub fn restore_finished_tool_message(&mut self, session: &str, call: &str) -> Result<bool> {
        let tx = self.conn.transaction()?;
        let (state, result, outcome): (String, Option<String>, Option<String>) = tx
            .query_row(
                "SELECT state,result,outcome FROM tool_runs WHERE session_id=?1 AND call_id=?2",
                params![session, call],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .context("Tool run is missing")?;
        ensure!(state == "finished", "Tool run is not finished");
        let was_uncertain = result.is_none()
            || outcome
                .as_deref()
                .map(serde_json::from_str::<ToolOutcome>)
                .transpose()?
                == Some(ToolOutcome::Uncertain);
        let (result, uncertain) = match result {
            Some(result) => (result, None),
            None => (
                "ERROR: Execution uncertain after interruption. This tool will NOT be rerun automatically. Ask the user to inspect any side effects before issuing further mutations.".into(),
                Some(serde_json::to_string(&ToolOutcome::Uncertain)?),
            ),
        };
        tx.execute(
            "UPDATE tool_runs SET result=?3,outcome=COALESCE(?4,outcome)
             WHERE session_id=?1 AND call_id=?2 AND state='finished'",
            params![session, call, result, uncertain],
        )?;
        insert_message(&tx, session, &Message::tool(call, result))?;
        touch(&tx, session)?;
        tx.commit()?;
        Ok(was_uncertain)
    }
    /// Result and conversation message become durable in the same transaction.
    pub fn complete_tool(&mut self, session: &str, call: &str, result: &str) -> Result<()> {
        self.complete_tool_with_outcome(session, call, result, ToolOutcome::Unknown)
    }
    pub fn tool_outcomes(
        &self,
        session: &str,
    ) -> Result<std::collections::HashMap<String, ToolOutcome>> {
        let mut stmt = self.conn.prepare(
            "SELECT call_id,outcome FROM tool_runs WHERE session_id=?1 AND outcome IS NOT NULL",
        )?;
        let rows = stmt.query_map([session], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?;
        rows.map(|row| {
            let (id, outcome) = row?;
            Ok((id, serde_json::from_str(&outcome)?))
        })
        .collect()
    }
    pub fn complete_tool_with_outcome(
        &mut self,
        session: &str,
        call: &str,
        result: &str,
        outcome: ToolOutcome,
    ) -> Result<()> {
        let tx = self.conn.transaction()?;
        let state: String = tx
            .query_row(
                "SELECT state FROM tool_runs WHERE session_id=?1 AND call_id=?2",
                params![session, call],
                |r| r.get(0),
            )
            .context("Tool must be claimed before completion")?;
        if state == "finished" {
            return Ok(());
        }
        tx.execute(
            "UPDATE tool_runs SET state='finished',result=?3,outcome=?4 WHERE session_id=?1 AND call_id=?2 AND state='started'",
            params![session, call, result, serde_json::to_string(&outcome)?],
        )?;
        tx.execute(
            "INSERT INTO messages(session_id,body) VALUES (?1,?2)",
            params![
                session,
                serde_json::to_string(&Message::tool(call, result.into()))?
            ],
        )?;
        tx.commit()?;
        Ok(())
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
fn close_pending(tx: &Transaction<'_>, id: &str, messages: &[Message]) -> Result<usize> {
    let pending = messages
        .last()
        .is_some_and(|m| m.role == Role::User || m.role == Role::Tool || !m.tool_calls.is_empty());
    let mut uncertain = 0;
    if pending {
        let completed: std::collections::HashSet<_> = messages
            .iter()
            .filter_map(|m| m.tool_call_id.as_deref())
            .collect();
        for call in messages.iter().flat_map(|m| &m.tool_calls) {
            if completed.contains(call.id.as_str()) {
                continue;
            }
            let run: Option<(String, Option<String>)> = tx
                .query_row(
                    "SELECT state,result FROM tool_runs WHERE session_id=?1 AND call_id=?2",
                    params![id, call.id],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .optional()?;
            let (result, outcome) = match run {
                None => (
                    "CANCELLED: User stopped this turn before this tool ran. Do not execute the cancelled request.".to_owned(),
                    None,
                ),
                Some((state, Some(result))) if state == "finished" => (result, None),
                Some(_) => {
                    uncertain += 1;
                    (
                        "ERROR: Execution uncertain after interruption. Do not rerun this tool; inspect any side effects before further mutations.".to_owned(),
                        Some(serde_json::to_string(&ToolOutcome::Uncertain)?),
                    )
                }
            };
            tx.execute("INSERT INTO tool_runs(session_id,call_id,state,result,outcome) VALUES (?1,?2,'finished',?3,?4)
                ON CONFLICT(session_id,call_id) DO UPDATE SET state='finished',result=excluded.result,
                outcome=COALESCE(excluded.outcome,tool_runs.outcome)",
                params![id, call.id, result, outcome])?;
            insert_message(tx, id, &Message::tool(&call.id, result))?;
        }
        insert_message(
            tx,
            id,
            &Message::text(
                Role::Assistant,
                "[Response cancelled by the user. Completed tool results remain valid; follow the user's next instruction.]",
            ),
        )?;
    }
    tx.execute(
        "UPDATE attempts SET status='failed',detail='Turn stopped by user'
        WHERE session_id=?1 AND status='running'",
        [id],
    )?;
    Ok(uncertain)
}
