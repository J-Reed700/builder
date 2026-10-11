//! Journal schema upgrades run in one writer-reserved transaction.
use super::{memory, schedules, subagents};
use anyhow::{Context, Result, ensure};
use rusqlite::{Connection, TransactionBehavior};

pub(super) fn migrate(conn: &mut Connection) -> Result<()> {
    let mut version: u32 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    ensure!(
        version <= 15,
        "Session database was created by a newer Builder version; upgrade Builder"
    );
    if version < 15 {
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
            version <= 15,
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
            tx.execute_batch(memory::SCHEMA)?;
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
        if version < 14 {
            tx.execute_batch(schedules::SCHEMA)?;
        }
        if version < 15 {
            tx.execute_batch(schedules::OUTCOME_SCHEMA)?;
        }
        tx.commit()?;
    }
    Ok(())
}
