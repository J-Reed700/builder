//! Versioned derived memory. Original conversation/tool records remain authoritative.
use crate::{
    protocol::{Message, ToolCall},
    store::Store,
};
use anyhow::{Result, ensure};
use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub(crate) const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS memories (
 scope TEXT NOT NULL, key TEXT NOT NULL, revision INTEGER NOT NULL,
 forgotten INTEGER NOT NULL DEFAULT 0, PRIMARY KEY(scope,key));
CREATE TABLE IF NOT EXISTS memory_revisions (
 scope TEXT NOT NULL, key TEXT NOT NULL, revision INTEGER NOT NULL,
 body TEXT NOT NULL, PRIMARY KEY(scope,key,revision));
CREATE VIRTUAL TABLE IF NOT EXISTS memory_fts USING fts5(scope UNINDEXED,key,text);
CREATE TABLE IF NOT EXISTS memory_vectors (
 scope TEXT NOT NULL,key TEXT NOT NULL,revision INTEGER NOT NULL,
 fingerprint TEXT NOT NULL, vector BLOB NOT NULL,
 PRIMARY KEY(scope,key,revision,fingerprint));
CREATE TABLE IF NOT EXISTS memory_tasks (
 session TEXT PRIMARY KEY REFERENCES sessions(id), through_seq INTEGER NOT NULL,
 body TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS memory_extraction (
 session TEXT PRIMARY KEY REFERENCES sessions(id), through_seq INTEGER NOT NULL,
 status TEXT NOT NULL);
PRAGMA user_version=4;";

pub fn digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Evidence {
    pub session: String,
    pub seq: i64,
    pub call_id: String,
    pub path: String,
    pub hash: String,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MemoryKind {
    Finding,
    Preference,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Memory {
    pub key: String,
    pub revision: i64,
    pub kind: MemoryKind,
    pub text: String,
    pub evidence: Vec<Evidence>,
    pub origin_session: String,
    pub origin_seq: i64,
    pub created_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskState {
    pub next_action: String,
    pub questions: Vec<String>,
    pub source_seq: i64,
}

impl Store {
    /// Select source reads independently of intervening shell results and prose.
    /// Both halves of the tool exchange must still be active and durable.
    pub fn memory_source_reads(
        &self,
        session: &str,
        after: i64,
        call_id: Option<&str>,
        limit: usize,
    ) -> Result<Vec<(i64, Message, ToolCall)>> {
        let mut stmt = self.conn.prepare(
            "SELECT r.seq,r.body,c.value FROM messages r
             JOIN tool_runs t ON t.session_id=r.session_id
                AND t.call_id=json_extract(r.body,'$.tool_call_id') AND t.state='finished'
             JOIN messages a ON a.session_id=r.session_id AND a.active=1 AND a.seq<r.seq
             JOIN json_each(a.body,'$.tool_calls') c
                ON json_extract(c.value,'$.id')=t.call_id
             WHERE r.session_id=?1 AND r.active=1 AND r.seq>?2
                AND json_extract(r.body,'$.role')='tool'
                AND json_extract(c.value,'$.function.name')='read_file'
                AND (?3 IS NULL OR t.call_id=?3)
             ORDER BY r.seq DESC LIMIT ?4",
        )?;
        let rows = stmt.query_map(params![session, after, call_id, limit.min(64)], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
            ))
        })?;
        let mut reads = rows
            .map(|row| {
                let (seq, result, call) = row?;
                Ok((
                    seq,
                    serde_json::from_str(&result)?,
                    serde_json::from_str(&call)?,
                ))
            })
            .collect::<Result<Vec<_>>>()?;
        reads.reverse();
        Ok(reads)
    }
    pub fn memory_latest_user(&self, session: &str) -> Result<Option<(i64, Message)>> {
        let row: Option<(i64, String)> = self.conn.query_row("SELECT seq,body FROM messages WHERE session_id=?1 AND active=1 AND json_extract(body,'$.role')='user' ORDER BY seq DESC LIMIT 1", [session], |r| Ok((r.get(0)?,r.get(1)?))).optional()?;
        row.map(|(seq, body)| Ok((seq, serde_json::from_str(&body)?)))
            .transpose()
    }
    pub fn memory_events(
        &self,
        session: &str,
        after: i64,
        limit: usize,
    ) -> Result<Vec<(i64, Message)>> {
        let mut stmt=self.conn.prepare("SELECT seq,body FROM messages WHERE session_id=?1 AND active=1 AND seq>?2 ORDER BY seq DESC LIMIT ?3")?;
        let rows = stmt.query_map(params![session, after, limit.min(64)], |r| {
            Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?))
        })?;
        let mut result = rows
            .map(|r| {
                let (seq, body) = r?;
                Ok((seq, serde_json::from_str(&body)?))
            })
            .collect::<Result<Vec<_>>>()?;
        result.reverse();
        Ok(result)
    }
    pub fn memory_event_active(&self, session: &str, seq: i64) -> Result<bool> {
        Ok(self.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM messages WHERE session_id=?1 AND seq=?2 AND active=1)",
            params![session, seq],
            |r| r.get(0),
        )?)
    }
    pub fn memory_latest_seq(&self, session: &str) -> Result<i64> {
        Ok(self.conn.query_row(
            "SELECT COALESCE(MAX(seq),0) FROM messages WHERE session_id=?1 AND active=1",
            [session],
            |r| r.get(0),
        )?)
    }
    pub fn memory_get(
        &self,
        scope: &str,
        key: &str,
        revision: Option<i64>,
    ) -> Result<Option<Memory>> {
        let body:Option<String>=self.conn.query_row("SELECT r.body FROM memory_revisions r JOIN memories m ON r.scope=m.scope AND r.key=m.key WHERE m.scope=?1 AND m.key=?2 AND m.forgotten=0 AND r.revision=COALESCE(?3,m.revision)",params![scope,key,revision],|r|r.get(0)).optional()?;
        body.map(|s| Ok(serde_json::from_str(&s)?)).transpose()
    }
    pub fn memory_list(&self, scope: &str) -> Result<Vec<Memory>> {
        let mut stmt=self.conn.prepare("SELECT r.body FROM memories m JOIN memory_revisions r ON r.scope=m.scope AND r.key=m.key AND r.revision=m.revision WHERE m.scope=?1 AND m.forgotten=0 ORDER BY m.rowid DESC LIMIT 10000")?;
        let rows = stmt.query_map([scope], |r| r.get::<_, String>(0))?;
        rows.map(|r| Ok(serde_json::from_str(&r?)?)).collect()
    }
    pub fn memory_put(&mut self, scope: &str, expected: i64, mut memory: Memory) -> Result<Memory> {
        ensure!(
            !memory.key.is_empty()
                && memory.key.len() <= 100
                && memory.text.len() <= 1600
                && !memory.text.trim().is_empty(),
            "Memory key/text exceeds limits or is empty"
        );
        ensure!(
            memory.evidence.len() <= 8,
            "At most eight evidence references"
        );
        ensure!(
            (scope == "@user"
                && memory.kind == MemoryKind::Preference
                && memory.origin_session.is_empty())
                || self.memory_event_active(&memory.origin_session, memory.origin_seq)?,
            "Memory origin is missing or rewound"
        );
        ensure!(
            (scope == "@user") == (memory.kind == MemoryKind::Preference),
            "Preference scope mismatch"
        );
        ensure!(expected < 1000, "Memory revision limit reached");
        for evidence in &memory.evidence {
            ensure!(
                self.memory_event_active(&evidence.session, evidence.seq)?,
                "Memory evidence is missing or rewound"
            );
        }
        let tx = self.journal_transaction()?;
        let existing: Option<(i64, bool)> = tx
            .query_row(
                "SELECT revision,forgotten FROM memories WHERE scope=?1 AND key=?2",
                params![scope, memory.key],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        ensure!(
            existing.is_none_or(|(_, forgotten)| !forgotten),
            "Memory was forgotten; use a new key for an explicit new finding"
        );
        ensure!(
            existing.map_or(0, |x| x.0) == expected,
            "Memory revision conflict; retrieve the current revision before updating"
        );
        if existing.is_none() {
            let count: i64 = tx.query_row(
                "SELECT COUNT(*) FROM memories WHERE scope=?1",
                [scope],
                |r| r.get(0),
            )?;
            ensure!(
                count < 10000,
                "Memory scope is at its 10000-record capacity"
            );
        }
        memory.revision = expected + 1;
        memory.created_at = chrono::Utc::now().to_rfc3339();
        tx.execute(
            "INSERT INTO memory_revisions VALUES(?1,?2,?3,?4)",
            params![
                scope,
                memory.key,
                memory.revision,
                serde_json::to_string(&memory)?
            ],
        )?;
        tx.execute("INSERT INTO memories(scope,key,revision) VALUES(?1,?2,?3) ON CONFLICT(scope,key) DO UPDATE SET revision=excluded.revision",params![scope,memory.key,memory.revision])?;
        tx.execute(
            "DELETE FROM memory_fts WHERE scope=?1 AND key=?2",
            params![scope, memory.key],
        )?;
        tx.execute(
            "INSERT INTO memory_fts(scope,key,text) VALUES(?1,?2,?3)",
            params![scope, memory.key, memory.text],
        )?;
        tx.commit()?;
        Ok(memory)
    }
    pub fn memory_forget(&mut self, scope: &str, key: &str, expected: i64) -> Result<()> {
        let tx = self.journal_transaction()?;
        ensure!(tx.execute("UPDATE memories SET forgotten=1 WHERE scope=?1 AND key=?2 AND revision=?3 AND forgotten=0",params![scope,key,expected])?==1,"Memory revision conflict or missing memory");
        tx.execute(
            "DELETE FROM memory_fts WHERE scope=?1 AND key=?2",
            params![scope, key],
        )?;
        tx.execute(
            "DELETE FROM memory_vectors WHERE scope=?1 AND key=?2",
            params![scope, key],
        )?;
        tx.commit()?;
        Ok(())
    }
    pub fn memory_keywords(&self, scope: &str, query: &str) -> Result<Vec<String>> {
        let words = query
            .split(|c: char| !c.is_alphanumeric() && c != '_')
            .filter(|s| !s.is_empty())
            .take(16)
            .map(|s| format!("\"{s}\""))
            .collect::<Vec<_>>()
            .join(" OR ");
        if words.is_empty() {
            return Ok(vec![]);
        }
        let mut stmt=self.conn.prepare("SELECT key FROM memory_fts WHERE scope=?1 AND memory_fts MATCH ?2 ORDER BY bm25(memory_fts) LIMIT 40")?;
        Ok(stmt
            .query_map(params![scope, words], |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?)
    }
    pub fn memory_vector(
        &self,
        scope: &str,
        key: &str,
        revision: i64,
        fingerprint: &str,
    ) -> Result<Option<Vec<f32>>> {
        let bytes:Option<Vec<u8>>=self.conn.query_row("SELECT vector FROM memory_vectors WHERE scope=?1 AND key=?2 AND revision=?3 AND fingerprint=?4",params![scope,key,revision,fingerprint],|r|r.get(0)).optional()?;
        bytes
            .map(|b| {
                ensure!(
                    b.len() % 4 == 0 && b.len() <= 32768,
                    "Invalid stored vector"
                );
                let v = b
                    .chunks_exact(4)
                    .map(|x| f32::from_le_bytes(x.try_into().unwrap()))
                    .collect::<Vec<_>>();
                validate_vector(&v)?;
                Ok(v)
            })
            .transpose()
    }
    pub fn memory_set_vector(
        &mut self,
        scope: &str,
        key: &str,
        revision: i64,
        fingerprint: &str,
        vector: &[f32],
    ) -> Result<()> {
        validate_vector(vector)?;
        let bytes = vector
            .iter()
            .flat_map(|f| f.to_le_bytes())
            .collect::<Vec<_>>();
        ensure!(self.conn.execute("INSERT OR REPLACE INTO memory_vectors SELECT scope,key,revision,?4,?5 FROM memories WHERE scope=?1 AND key=?2 AND revision=?3 AND forgotten=0",params![scope,key,revision,fingerprint,bytes])?==1,"Embedding target revision changed or was forgotten");
        Ok(())
    }
    pub fn memory_save_task(&mut self, session: &str, state: &TaskState) -> Result<()> {
        ensure!(
            state.next_action.len() <= 1200
                && state.questions.len() <= 8
                && state.questions.iter().all(|q| q.len() <= 400),
            "Task state exceeds bounds"
        );
        ensure!(
            self.memory_event_active(session, state.source_seq)?,
            "Task origin no longer active"
        );
        self.conn.execute("INSERT INTO memory_tasks VALUES(?1,?2,?3) ON CONFLICT(session) DO UPDATE SET through_seq=excluded.through_seq,body=excluded.body",params![session,state.source_seq,serde_json::to_string(state)?])?;
        Ok(())
    }
    pub fn memory_task(&self, session: &str) -> Result<Option<TaskState>> {
        let body:Option<String>=self.conn.query_row("SELECT t.body FROM memory_tasks t JOIN messages m ON m.session_id=t.session AND m.seq=t.through_seq WHERE t.session=?1 AND m.active=1",[session],|r|r.get(0)).optional()?;
        body.map(|s| Ok(serde_json::from_str(&s)?)).transpose()
    }
    pub fn memory_extraction_cursor(&self, session: &str) -> Result<i64> {
        // Only completed batches are considered. Rows left as 'attempted' by
        // older versions recorded a failed generation and are eligible again.
        Ok(self
            .conn
            .query_row(
                "SELECT through_seq FROM memory_extraction WHERE session=?1 AND status='done'",
                [session],
                |r| r.get(0),
            )
            .optional()?
            .unwrap_or(0))
    }
    pub fn memory_extraction_done(&mut self, session: &str, seq: i64) -> Result<()> {
        self.conn.execute("INSERT INTO memory_extraction VALUES(?1,?2,'done') ON CONFLICT(session) DO UPDATE SET through_seq=excluded.through_seq,status='done'",params![session,seq])?;
        Ok(())
    }
}

pub fn validate_vector(vector: &[f32]) -> Result<()> {
    ensure!(
        !vector.is_empty() && vector.len() <= 8192 && vector.iter().all(|f| f.is_finite()),
        "Invalid embedding dimensions or values"
    );
    ensure!(
        vector.iter().map(|x| f64::from(*x).powi(2)).sum::<f64>() > 0.0,
        "Embedding has zero norm"
    );
    Ok(())
}
pub fn cosine(a: &[f32], b: &[f32]) -> Result<f64> {
    validate_vector(a)?;
    validate_vector(b)?;
    ensure!(a.len() == b.len(), "Embedding dimension mismatch");
    let dot = a
        .iter()
        .zip(b)
        .map(|(x, y)| f64::from(*x) * f64::from(*y))
        .sum::<f64>();
    let norm = |v: &[f32]| v.iter().map(|x| f64::from(*x).powi(2)).sum::<f64>().sqrt();
    Ok(dot / (norm(a) * norm(b)))
}
