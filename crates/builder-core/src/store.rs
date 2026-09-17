use crate::protocol::Message;
mod pages;
mod subagents;
use crate::protocol::Role;
use anyhow::{Context, Result, ensure};
use fs2::FileExt;
pub use pages::{ChatSession, HistoryEntry, HistoryPage};
use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    fs::File,
    path::{Path, PathBuf},
    time::Duration,
};

const JOURNAL_BUSY_TIMEOUT: Duration = Duration::from_secs(5);
pub const INTERRUPTED_SIDE_EFFECT_FREE: &str = "ERROR: Interrupted before this result was saved. The call has no side effects; repeat it if the result is still needed.";
const INDEX_OPEN_BUSY_TIMEOUT: Duration = Duration::from_millis(250);

pub struct Store {
    pub(crate) conn: Connection,
    /// Derived repository indexes live outside the authoritative conversation
    /// journal. A bulk index write must never take the writer lock needed to
    /// save a user message, tool claim, or tool result.
    pub(crate) index_conn: Option<Connection>,
    index_error: Option<String>,
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

/// Serializes rebuildable index maintenance for one checkout across Builder
/// processes. Losing the process releases the advisory lock automatically.
pub struct CodeIndexGuard {
    file: File,
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

fn insert_session(
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

fn private_database_options() -> std::fs::OpenOptions {
    let mut options = File::options();
    options.create_new(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options
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

fn open_index(home: &Path) -> Result<Connection> {
    let path = home.join("builder-index.sqlite3");
    let options = private_database_options();
    match options.open(&path) {
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error.into()),
    }
    let mut conn = Connection::open(&path)?;
    conn.busy_timeout(INDEX_OPEN_BUSY_TIMEOUT)?;
    conn.execute_batch("PRAGMA synchronous=FULL; PRAGMA foreign_keys=ON;")?;
    let mut version: u32 = conn.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    ensure!(
        version <= 1,
        "Code index database was created by a newer Builder version; upgrade Builder"
    );
    if version == 0 {
        // journal_mode cannot change inside a transaction. It is established
        // once before any index rows exist; later opens only read the version
        // and therefore do not contend with an active background writer.
        conn.execute_batch("PRAGMA journal_mode=WAL;")?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        version = tx.query_row("PRAGMA user_version", [], |row| row.get(0))?;
        ensure!(
            version <= 1,
            "Code index database was created by a newer Builder version; upgrade Builder"
        );
        if version == 0 {
            tx.execute_batch(crate::code_index::SCHEMA)?;
            tx.execute_batch(crate::code_index::TELEMETRY_SCHEMA)?;
            tx.execute_batch(crate::code_index::HISTORY_SCHEMA)?;
            tx.execute_batch(crate::code_index::GRAPH_SCHEMA)?;
            tx.execute_batch("PRAGMA user_version=1;")?;
        }
        tx.commit()?;
    }
    let journal_mode: String =
        conn.query_row("PRAGMA journal_mode", [], |row| row.get::<_, String>(0))?;
    ensure!(
        journal_mode.eq_ignore_ascii_case("wal"),
        "Code index database is not in WAL mode"
    );
    Ok(conn)
}

impl Store {
    pub fn open(home: &Path) -> Result<Self> {
        crate::config::ensure_home(home)?;
        let path = home.join("builder.sqlite3");
        let options = private_database_options();
        // Seed a new database with private permissions; SQLite copies them to
        // its WAL and shm files. Never open an existing database here: POSIX
        // drops every advisory lock this process holds on a file whenever any
        // descriptor for it is closed, so opening and closing the file beside
        // a live SQLite connection released that connection's WAL-mode shared
        // lock. A second Builder process could then checkpoint and truncate the
        // WAL underneath it, and the next read failed with SQLITE_IOERR_SHORT_READ
        // ("disk I/O error: Error code 522 ... file truncated?").
        match options.open(&path) {
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error.into()),
        }
        let mut conn = Connection::open(&path)?;
        conn.busy_timeout(JOURNAL_BUSY_TIMEOUT)?;
        conn.execute_batch("PRAGMA synchronous=FULL; PRAGMA foreign_keys=ON;")?;
        let mut version: u32 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        ensure!(
            version <= 13,
            "Session database was created by a newer Builder version; upgrade Builder"
        );
        if version < 13 {
            // WAL mode is persistent. Only migrations may change it or acquire
            // a write lock; opening a current journal is a read-only fast path.
            conn.execute_batch("PRAGMA journal_mode=WAL;")?;
            let tx = conn
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .context("Authoritative journal is busy while applying a schema migration")?;
            // A concurrent process may have completed the migration while this
            // connection waited for the writer lock.
            version = tx.query_row("PRAGMA user_version", [], |r| r.get(0))?;
            ensure!(
                version <= 13,
                "Session database was created by a newer Builder version; upgrade Builder"
            );
            tx.execute_batch("CREATE TABLE IF NOT EXISTS sessions (
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
                tx.execute_batch(
                    "ALTER TABLE messages ADD COLUMN active INTEGER NOT NULL DEFAULT 1;
                CREATE TABLE composer_drafts (
                    session_id TEXT PRIMARY KEY REFERENCES sessions(id), prompt TEXT NOT NULL);
                PRAGMA user_version=2;",
                )?;
            }
            if version < 3 {
                tx.execute_batch(
                    "CREATE TABLE context_checkpoints (
                id INTEGER PRIMARY KEY, session_id TEXT NOT NULL REFERENCES sessions(id),
                through_seq INTEGER NOT NULL, context TEXT NOT NULL, created_at TEXT NOT NULL,
                active INTEGER NOT NULL DEFAULT 1);
                PRAGMA user_version=3;",
                )?;
            }
            if version < 4 {
                tx.execute_batch(crate::memory::SCHEMA)?;
            }
            if version < 5 {
                tx.execute_batch(
                    "ALTER TABLE tool_runs ADD COLUMN outcome TEXT; PRAGMA user_version=5;",
                )?;
            }
            if version < 6 {
                tx.execute_batch(
                    "CREATE TABLE IF NOT EXISTS chat_metadata (
                session_id TEXT PRIMARY KEY REFERENCES sessions(id),
                archived INTEGER NOT NULL DEFAULT 0 CHECK(archived IN (0,1)));
                PRAGMA user_version=6;",
                )?;
            }
            if version < 7 {
                tx.execute_batch("PRAGMA user_version=7;")?;
            }
            if version < 8 {
                tx.execute_batch("PRAGMA user_version=8;")?;
            }
            if version < 9 {
                tx.execute_batch("PRAGMA user_version=9;")?;
            }
            if version < 10 {
                tx.execute_batch("PRAGMA user_version=10;")?;
            }
            if version < 11 {
                tx.execute_batch(
                    "CREATE TABLE IF NOT EXISTS provider_conformance (
                 fingerprint TEXT PRIMARY KEY, profile TEXT NOT NULL, endpoint TEXT NOT NULL,
                 model TEXT NOT NULL, checked_at TEXT NOT NULL, report TEXT NOT NULL);
                 PRAGMA user_version=11;",
                )?;
            }
            if version < 12 {
                // Code indexes are derived and rebuildable. Schema 12 moves all
                // new index writes to builder-index.sqlite3 so their bulk
                // transactions cannot block the authoritative journal. Legacy
                // index tables are deliberately retained until explicit cleanup;
                // migration never risks transcript availability on a large DROP.
                tx.execute_batch("PRAGMA user_version=12;")?;
            }
            if version < 13 {
                // Side-effect-free claims (reads, subagents) may be closed as
                // retryable failures after an interruption instead of uncertain.
                tx.execute_batch(subagents::SCHEMA)?;
                let migrated: bool = tx.query_row(
                    "SELECT EXISTS(SELECT 1 FROM pragma_table_info('tool_runs') WHERE name='side_effect_free')",
                    [],
                    |r| r.get(0),
                )?;
                if !migrated {
                    tx.execute_batch(
                        "ALTER TABLE tool_runs ADD COLUMN side_effect_free INTEGER NOT NULL DEFAULT 0;",
                    )?;
                }
                tx.execute_batch("PRAGMA user_version=13;")?;
            }
            tx.commit()?;
        }
        let journal_mode: String =
            conn.query_row("PRAGMA journal_mode", [], |row| row.get::<_, String>(0))?;
        ensure!(
            journal_mode.eq_ignore_ascii_case("wal"),
            "Authoritative journal is not in WAL mode"
        );
        // A rebuildable sidecar must never make the authoritative journal
        // unavailable. Automatic code context degrades with an explicit notice;
        // a later Store open retries initialization.
        let (index_conn, index_error) = match open_index(home) {
            Ok(connection) => (Some(connection), None),
            Err(error) => (None, Some(format!("{error:#}"))),
        };
        Ok(Self {
            conn,
            index_conn,
            index_error,
            home: home.into(),
        })
    }

    pub(crate) fn index(&self) -> Result<&Connection> {
        self.index_conn.as_ref().with_context(|| {
            format!(
                "Derived code index unavailable: {}",
                self.index_error.as_deref().unwrap_or("unknown error")
            )
        })
    }

    pub(crate) fn index_mut(&mut self) -> Result<&mut Connection> {
        self.index_conn.as_mut().with_context(|| {
            format!(
                "Derived code index unavailable: {}",
                self.index_error.as_deref().unwrap_or("unknown error")
            )
        })
    }

    /// Acquire the journal writer at the transaction boundary. SQLite's
    /// default DEFERRED mode can create a read snapshot and then fail an
    /// upgrade immediately with SQLITE_BUSY_SNAPSHOT; IMMEDIATE makes writer
    /// contention happen before any transaction work is performed.
    pub(crate) fn journal_transaction(&mut self) -> Result<Transaction<'_>> {
        self.conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .context("Authoritative journal remained busy before a durable write")
    }

    pub fn try_code_index_lock(&self, scope: &str) -> Result<Option<CodeIndexGuard>> {
        ensure!(
            !scope.is_empty() && scope.len() <= 16 * 1024,
            "Invalid code index lock scope"
        );
        let identity = format!("{:x}", Sha256::digest(scope.as_bytes()));
        let file = private_lock_options()
            .open(self.home.join(format!("builder-index-{identity}.lock")))?;
        match file.try_lock_exclusive() {
            Ok(()) => Ok(Some(CodeIndexGuard { file })),
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => Ok(None),
            Err(error) => Err(error).context("Could not acquire code index maintenance lock"),
        }
    }
    pub fn save_provider_conformance(
        &mut self,
        fingerprint: &str,
        profile: &str,
        endpoint: &str,
        model: &str,
        report: &serde_json::Value,
    ) -> Result<()> {
        ensure!(
            !fingerprint.is_empty()
                && fingerprint.len() <= 256
                && !profile.is_empty()
                && profile.len() <= 256
                && endpoint.len() <= 2048
                && !model.is_empty()
                && model.len() <= 1024,
            "Invalid provider conformance identity"
        );
        let report = serde_json::to_string(report)?;
        ensure!(
            report.len() <= 16 * 1024,
            "Provider conformance report exceeds storage bound"
        );
        self.conn.execute(
            "INSERT INTO provider_conformance(fingerprint,profile,endpoint,model,checked_at,report)
             VALUES(?1,?2,?3,?4,?5,?6) ON CONFLICT(fingerprint) DO UPDATE SET
             profile=excluded.profile,endpoint=excluded.endpoint,model=excluded.model,
             checked_at=excluded.checked_at,report=excluded.report",
            params![
                fingerprint,
                profile,
                endpoint,
                model,
                chrono::Utc::now().to_rfc3339(),
                report
            ],
        )?;
        Ok(())
    }
    /// A second connection to the same journal, for work that runs beside
    /// this one, such as a subagent's child session.
    pub fn reopen(&self) -> Result<Self> {
        Self::open(&self.home)
    }
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
        let tx = self.journal_transaction()?;
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
        let tx = self.journal_transaction()?;
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
        let tx = self.journal_transaction()?;
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
        let tx = self.journal_transaction()?;
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
        self.claim(session, call, false)
    }
    /// Claim a call the executor guarantees has no side effects. If it is
    /// interrupted, it closes as a retryable failure rather than uncertain.
    pub fn claim_side_effect_free_tool(&self, session: &str, call: &str) -> Result<bool> {
        self.claim(session, call, true)
    }
    fn claim(&self, session: &str, call: &str, side_effect_free: bool) -> Result<bool> {
        Ok(self.conn.execute(
            "INSERT OR IGNORE INTO tool_runs(session_id,call_id,state,side_effect_free) VALUES (?1,?2,'started',?3)",
            params![session, call, side_effect_free],
        )? == 1)
    }
    /// Close a side-effect-free claim that never recorded a result. Returns
    /// false, changing nothing, for any other claim state.
    pub fn close_interrupted_side_effect_free_tool(
        &mut self,
        session: &str,
        call: &str,
    ) -> Result<bool> {
        let free: bool = self
            .conn
            .query_row(
                "SELECT side_effect_free FROM tool_runs WHERE session_id=?1 AND call_id=?2 AND state='started'",
                params![session, call],
                |r| r.get(0),
            )
            .optional()?
            .unwrap_or(false);
        if free {
            self.complete_tool_with_outcome(
                session,
                call,
                INTERRUPTED_SIDE_EFFECT_FREE,
                ToolOutcome::Failed,
            )?;
        }
        Ok(free)
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
        let tx = self.journal_transaction()?;
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
        let tx = self.journal_transaction()?;
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
            let run: Option<(String, Option<String>, bool)> = tx
                .query_row(
                    "SELECT state,result,side_effect_free FROM tool_runs WHERE session_id=?1 AND call_id=?2",
                    params![id, call.id],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
                )
                .optional()?;
            let (result, outcome) = match run {
                None => (
                    "CANCELLED: User stopped this turn before this tool ran. Do not execute the cancelled request.".to_owned(),
                    None,
                ),
                Some((state, Some(result), _)) if state == "finished" => (result, None),
                Some((_, _, true)) => (
                    INTERRUPTED_SIDE_EFFECT_FREE.to_owned(),
                    Some(serde_json::to_string(&ToolOutcome::Failed)?),
                ),
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
