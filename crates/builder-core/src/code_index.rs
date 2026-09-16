//! Durable, checkout-scoped code index generations. Indexed text is a navigation
//! hint; callers must validate `file_hash` against the workspace before use.
use crate::{memory::validate_vector, store::Store};
use anyhow::{Result, ensure};
use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize};

pub(crate) const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS code_index_state (
 scope TEXT PRIMARY KEY, generation INTEGER NOT NULL, snapshot_hash TEXT NOT NULL,
 status TEXT NOT NULL, completed_at TEXT NOT NULL, files INTEGER NOT NULL,
 chunks INTEGER NOT NULL, source_bytes INTEGER NOT NULL, skipped INTEGER NOT NULL);
CREATE TABLE IF NOT EXISTS code_index_files (
 scope TEXT NOT NULL, path TEXT NOT NULL, generation INTEGER NOT NULL,
 hash TEXT NOT NULL, source_bytes INTEGER NOT NULL, language TEXT NOT NULL,
 PRIMARY KEY(scope,path));
CREATE TABLE IF NOT EXISTS code_index_chunks (
 scope TEXT NOT NULL, id TEXT NOT NULL, path TEXT NOT NULL, generation INTEGER NOT NULL,
 content_hash TEXT NOT NULL, body TEXT NOT NULL, PRIMARY KEY(scope,id));
CREATE INDEX IF NOT EXISTS code_index_chunks_path ON code_index_chunks(scope,path);
CREATE INDEX IF NOT EXISTS code_index_chunks_content ON code_index_chunks(content_hash);
CREATE VIRTUAL TABLE IF NOT EXISTS code_index_fts USING fts5(
 scope UNINDEXED,id UNINDEXED,path,symbols,references,content);
CREATE TABLE IF NOT EXISTS code_index_vectors (
 content_hash TEXT NOT NULL, fingerprint TEXT NOT NULL, vector BLOB NOT NULL,
 PRIMARY KEY(content_hash,fingerprint));";

pub(crate) const TELEMETRY_SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS code_index_queries (
 id INTEGER PRIMARY KEY AUTOINCREMENT, scope TEXT NOT NULL, session TEXT NOT NULL,
 source TEXT NOT NULL CHECK(source IN ('tool','automatic')), query TEXT NOT NULL,
 created_at TEXT NOT NULL, elapsed_ms INTEGER NOT NULL, candidates INTEGER NOT NULL,
 returned INTEGER NOT NULL, stale_suppressed INTEGER NOT NULL,
 semantic INTEGER NOT NULL CHECK(semantic IN (0,1)), coverage_indexed INTEGER,
 coverage_total INTEGER, result_paths TEXT NOT NULL);
CREATE INDEX IF NOT EXISTS code_index_queries_scope
 ON code_index_queries(scope,id DESC);";

pub(crate) const HISTORY_SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS code_history_state (
 scope TEXT PRIMARY KEY, head TEXT NOT NULL, snapshot_hash TEXT NOT NULL,
 indexed_at TEXT NOT NULL, commits INTEGER NOT NULL);
CREATE TABLE IF NOT EXISTS code_history_entries (
 scope TEXT NOT NULL, revision TEXT NOT NULL, body TEXT NOT NULL,
 PRIMARY KEY(scope,revision));
CREATE VIRTUAL TABLE IF NOT EXISTS code_history_fts USING fts5(
 scope UNINDEXED,revision UNINDEXED,subject,paths);";

pub(crate) const GRAPH_SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS code_index_symbols (
 scope TEXT NOT NULL, normalized TEXT NOT NULL, symbol TEXT NOT NULL,
 chunk_id TEXT NOT NULL, path TEXT NOT NULL,
 role TEXT NOT NULL CHECK(role IN ('declaration','reference')),
 PRIMARY KEY(scope,normalized,chunk_id,role));
CREATE INDEX IF NOT EXISTS code_index_symbols_chunk
 ON code_index_symbols(scope,chunk_id);
CREATE INDEX IF NOT EXISTS code_index_symbols_path
 ON code_index_symbols(scope,path);
UPDATE code_index_state SET status='stale';";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CodeIndexFile {
    pub path: String,
    pub hash: String,
    pub source_bytes: usize,
    pub language: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CodeChunk {
    pub id: String,
    pub path: String,
    pub file_hash: String,
    pub content_hash: String,
    pub language: String,
    pub kind: String,
    pub start_line: usize,
    pub end_line: usize,
    pub symbols: Vec<String>,
    pub references: Vec<String>,
    pub content: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CodeIndexSnapshot {
    pub snapshot_hash: String,
    pub files: Vec<CodeIndexFile>,
    pub chunks: Vec<CodeChunk>,
    pub source_bytes: usize,
    pub skipped: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CodeIndexStatus {
    pub generation: i64,
    pub snapshot_hash: String,
    pub status: String,
    pub completed_at: String,
    pub files: usize,
    pub chunks: usize,
    pub source_bytes: usize,
    pub skipped: usize,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CodeQuerySource {
    Tool,
    Automatic,
}

impl CodeQuerySource {
    fn as_str(self) -> &'static str {
        match self {
            Self::Tool => "tool",
            Self::Automatic => "automatic",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CodeQueryTelemetry {
    pub session: String,
    pub source: CodeQuerySource,
    pub query: String,
    pub elapsed_ms: u64,
    pub candidates: usize,
    pub returned: usize,
    pub stale_suppressed: usize,
    pub semantic: bool,
    pub coverage: Option<(usize, usize)>,
    pub result_paths: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CodeQuerySummary {
    pub queries: usize,
    pub abstentions: usize,
    pub stale_suppressions: usize,
    pub average_elapsed_ms: u64,
    pub last_query_at: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CodeHistoryEntry {
    pub revision: String,
    pub unix_time: i64,
    pub subject: String,
    pub paths: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CodeHistorySnapshot {
    pub head: String,
    pub snapshot_hash: String,
    pub entries: Vec<CodeHistoryEntry>,
}

impl Store {
    pub fn code_index_status(&self, scope: &str) -> Result<Option<CodeIndexStatus>> {
        Ok(self.index()?.query_row(
            "SELECT generation,snapshot_hash,status,completed_at,files,chunks,source_bytes,skipped
             FROM code_index_state WHERE scope=?1",
            [scope],
            |row| Ok(CodeIndexStatus {
                generation: row.get(0)?, snapshot_hash: row.get(1)?, status: row.get(2)?,
                completed_at: row.get(3)?, files: row.get(4)?, chunks: row.get(5)?,
                source_bytes: row.get(6)?, skipped: row.get(7)?,
            }),
        ).optional()?)
    }

    /// Atomically publish a complete generation. The prior generation remains
    /// visible if validation, serialization, or any SQLite write fails.
    pub fn code_index_replace(
        &mut self,
        scope: &str,
        snapshot: &CodeIndexSnapshot,
    ) -> Result<bool> {
        ensure!(!scope.is_empty(), "Code index scope cannot be empty");
        ensure!(
            !snapshot.snapshot_hash.is_empty(),
            "Code index snapshot hash is missing"
        );
        if self.code_index_status(scope)?.is_some_and(|state| {
            state.status == "ready" && state.snapshot_hash == snapshot.snapshot_hash
        }) {
            return Ok(false);
        }
        ensure!(
            snapshot.files.len() <= 50_000,
            "Code index file count exceeds storage bound"
        );
        ensure!(
            snapshot.chunks.len() <= 100_000,
            "Code index chunk count exceeds storage bound"
        );
        let file_paths = snapshot
            .files
            .iter()
            .map(|file| file.path.as_str())
            .collect::<std::collections::HashSet<_>>();
        for chunk in &snapshot.chunks {
            ensure!(
                file_paths.contains(chunk.path.as_str()),
                "Code chunk references a missing file"
            );
            ensure!(
                chunk.start_line > 0 && chunk.end_line >= chunk.start_line,
                "Invalid code chunk line range"
            );
            ensure!(
                !chunk.content.is_empty() && chunk.content.len() <= 8192,
                "Invalid code chunk size"
            );
        }
        let tx = self
            .index_mut()?
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        tx.execute("DELETE FROM code_index_symbols WHERE scope=?1", [scope])?;
        let generation: i64 = tx.query_row(
            "SELECT COALESCE(MAX(generation),0)+1 FROM code_index_state WHERE scope=?1",
            [scope],
            |row| row.get(0),
        )?;
        tx.execute("DELETE FROM code_index_fts WHERE scope=?1", [scope])?;
        tx.execute("DELETE FROM code_index_chunks WHERE scope=?1", [scope])?;
        tx.execute("DELETE FROM code_index_files WHERE scope=?1", [scope])?;
        for file in &snapshot.files {
            tx.execute(
                "INSERT INTO code_index_files(scope,path,generation,hash,source_bytes,language)
                 VALUES(?1,?2,?3,?4,?5,?6)",
                params![
                    scope,
                    file.path,
                    generation,
                    file.hash,
                    file.source_bytes,
                    file.language
                ],
            )?;
        }
        for chunk in &snapshot.chunks {
            tx.execute(
                "INSERT INTO code_index_chunks(scope,id,path,generation,content_hash,body)
                 VALUES(?1,?2,?3,?4,?5,?6)",
                params![
                    scope,
                    chunk.id,
                    chunk.path,
                    generation,
                    chunk.content_hash,
                    serde_json::to_string(chunk)?
                ],
            )?;
            tx.execute(
                "INSERT INTO code_index_fts(scope,id,path,symbols,\"references\",content)
                 VALUES(?1,?2,?3,?4,?5,?6)",
                params![
                    scope,
                    chunk.id,
                    chunk.path,
                    chunk.symbols.join(" "),
                    chunk.references.join(" "),
                    chunk.content
                ],
            )?;
            for (role, values) in [
                ("declaration", &chunk.symbols),
                ("reference", &chunk.references),
            ] {
                for symbol in values {
                    tx.execute(
                        "INSERT OR IGNORE INTO code_index_symbols(scope,normalized,symbol,chunk_id,path,role)
                         VALUES(?1,?2,?3,?4,?5,?6)",
                        params![
                            scope,
                            symbol.to_ascii_lowercase(),
                            symbol,
                            chunk.id,
                            chunk.path,
                            role
                        ],
                    )?;
                }
            }
        }
        tx.execute(
            "INSERT INTO code_index_state(scope,generation,snapshot_hash,status,completed_at,files,chunks,source_bytes,skipped)
             VALUES(?1,?2,?3,'ready',?4,?5,?6,?7,?8)
             ON CONFLICT(scope) DO UPDATE SET generation=excluded.generation,snapshot_hash=excluded.snapshot_hash,
             status=excluded.status,completed_at=excluded.completed_at,files=excluded.files,chunks=excluded.chunks,
             source_bytes=excluded.source_bytes,skipped=excluded.skipped",
            params![scope,generation,snapshot.snapshot_hash,chrono::Utc::now().to_rfc3339(),snapshot.files.len(),snapshot.chunks.len(),snapshot.source_bytes,snapshot.skipped],
        )?;
        // Content-addressed vectors can survive generations, but not forever.
        tx.execute("DELETE FROM code_index_vectors WHERE NOT EXISTS (
            SELECT 1 FROM code_index_chunks c WHERE c.content_hash=code_index_vectors.content_hash)", [])?;
        tx.commit()?;
        Ok(true)
    }

    pub fn code_index_lexical(
        &self,
        scope: &str,
        expression: &str,
        limit: usize,
    ) -> Result<Vec<(CodeChunk, f64)>> {
        ensure!(!expression.is_empty(), "Code index query is empty");
        // Paths and extracted references repeat across passages; give source
        // text and declarations greater weight than duplicated metadata.
        let index = self.index()?;
        let mut stmt = index.prepare(
            "SELECT c.body,bm25(code_index_fts,0,0,0.1,1,0.1,1) FROM code_index_fts
             JOIN code_index_chunks c ON c.scope=code_index_fts.scope AND c.id=code_index_fts.id
             WHERE code_index_fts.scope=?1 AND code_index_fts MATCH ?2
             ORDER BY bm25(code_index_fts,0,0,0.1,1,0.1,1),c.path,c.id LIMIT ?3",
        )?;
        let rows = stmt.query_map(params![scope, expression, limit.min(500)], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, f64>(1)?))
        })?;
        rows.map(|row| {
            let (body, score) = row?;
            Ok((serde_json::from_str(&body)?, score))
        })
        .collect()
    }

    pub fn code_index_pending_vectors(
        &self,
        scope: &str,
        fingerprint: &str,
        limit: usize,
    ) -> Result<Vec<CodeChunk>> {
        let index = self.index()?;
        let mut stmt = index.prepare(
            "SELECT MIN(c.body) FROM code_index_chunks c
             LEFT JOIN code_index_vectors v ON v.content_hash=c.content_hash AND v.fingerprint=?2
             WHERE c.scope=?1 AND v.content_hash IS NULL GROUP BY c.content_hash
             ORDER BY c.content_hash LIMIT ?3",
        )?;
        let rows = stmt.query_map(params![scope, fingerprint, limit.min(128)], |row| {
            row.get::<_, String>(0)
        })?;
        rows.map(|row| Ok(serde_json::from_str(&row?)?)).collect()
    }

    pub fn code_index_set_vector(
        &mut self,
        content_hash: &str,
        fingerprint: &str,
        vector: &[f32],
    ) -> Result<()> {
        validate_vector(vector)?;
        ensure!(
            self.index()?.query_row(
                "SELECT EXISTS(SELECT 1 FROM code_index_chunks WHERE content_hash=?1)",
                [content_hash],
                |row| row.get::<_, bool>(0),
            )?,
            "Code chunk changed before its embedding completed"
        );
        let bytes = vector
            .iter()
            .flat_map(|value| value.to_le_bytes())
            .collect::<Vec<_>>();
        self.index()?.execute(
            "INSERT OR REPLACE INTO code_index_vectors(content_hash,fingerprint,vector) VALUES(?1,?2,?3)",
            params![content_hash,fingerprint,bytes],
        )?;
        Ok(())
    }

    pub fn code_index_set_vectors(
        &mut self,
        fingerprint: &str,
        vectors: &[(String, Vec<f32>)],
    ) -> Result<()> {
        ensure!(
            !vectors.is_empty() && vectors.len() <= 128,
            "Invalid code vector batch"
        );
        for (_, vector) in vectors {
            validate_vector(vector)?;
        }
        let tx = self
            .index_mut()?
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        for (content_hash, vector) in vectors {
            ensure!(
                tx.query_row(
                    "SELECT EXISTS(SELECT 1 FROM code_index_chunks WHERE content_hash=?1)",
                    [content_hash],
                    |row| row.get::<_, bool>(0),
                )?,
                "Code generation changed before its embedding batch completed"
            );
            let bytes = vector
                .iter()
                .flat_map(|value| value.to_le_bytes())
                .collect::<Vec<_>>();
            tx.execute(
                "INSERT OR REPLACE INTO code_index_vectors(content_hash,fingerprint,vector) VALUES(?1,?2,?3)",
                params![content_hash,fingerprint,bytes],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    pub fn code_index_dense(
        &self,
        scope: &str,
        fingerprint: &str,
        limit: usize,
    ) -> Result<Vec<(CodeChunk, Vec<f32>)>> {
        let index = self.index()?;
        let mut stmt = index.prepare(
            "SELECT c.body,v.vector FROM code_index_chunks c JOIN code_index_vectors v
             ON v.content_hash=c.content_hash WHERE c.scope=?1 AND v.fingerprint=?2
             ORDER BY c.path,c.id LIMIT ?3",
        )?;
        let rows = stmt.query_map(params![scope, fingerprint, limit.min(100_000)], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, Vec<u8>>(1)?))
        })?;
        rows.map(|row| {
            let (body, bytes) = row?;
            ensure!(
                bytes.len() % 4 == 0 && bytes.len() <= 32768,
                "Invalid stored code vector"
            );
            let vector = bytes
                .chunks_exact(4)
                .map(|part| f32::from_le_bytes(part.try_into().expect("four bytes")))
                .collect::<Vec<_>>();
            validate_vector(&vector)?;
            Ok((serde_json::from_str(&body)?, vector))
        })
        .collect()
    }

    pub fn code_index_vector_coverage(
        &self,
        scope: &str,
        fingerprint: &str,
    ) -> Result<(usize, usize)> {
        Ok(self.index()?.query_row(
            "SELECT COUNT(DISTINCT CASE WHEN v.content_hash IS NOT NULL THEN c.content_hash END),
                    COUNT(DISTINCT c.content_hash)
             FROM code_index_chunks c LEFT JOIN code_index_vectors v
             ON v.content_hash=c.content_hash AND v.fingerprint=?2 WHERE c.scope=?1",
            params![scope, fingerprint],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?)
    }

    pub fn code_index_remove_path(&mut self, scope: &str, path: &str) -> Result<()> {
        let tx = self
            .index_mut()?
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        tx.execute(
            "DELETE FROM code_index_symbols WHERE scope=?1 AND path=?2",
            params![scope, path],
        )?;
        tx.execute(
            "DELETE FROM code_index_fts WHERE scope=?1 AND id IN (
            SELECT id FROM code_index_chunks WHERE scope=?1 AND path=?2)",
            params![scope, path],
        )?;
        tx.execute(
            "DELETE FROM code_index_chunks WHERE scope=?1 AND path=?2",
            params![scope, path],
        )?;
        tx.execute(
            "DELETE FROM code_index_files WHERE scope=?1 AND path=?2",
            params![scope, path],
        )?;
        tx.execute(
            "UPDATE code_index_state SET status='stale' WHERE scope=?1",
            [scope],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Record a failed complete scan without deleting or replacing the last
    /// queryable generation.
    pub fn code_index_record_failure(&mut self, scope: &str, detail: &str) -> Result<()> {
        let mut detail = detail.replace(['\r', '\n'], " ");
        if detail.len() > 500 {
            detail.truncate(500);
        }
        let status = format!("refresh_failed: {detail}");
        self.index()?.execute(
            "INSERT INTO code_index_state(scope,generation,snapshot_hash,status,completed_at,files,chunks,source_bytes,skipped)
             VALUES(?1,0,'',?2,?3,0,0,0,0)
             ON CONFLICT(scope) DO UPDATE SET status=excluded.status,completed_at=excluded.completed_at",
            params![scope,status,chrono::Utc::now().to_rfc3339()],
        )?;
        Ok(())
    }

    /// Persist bounded retrieval diagnostics. This records navigation metadata,
    /// not source excerpts, prompts, model output, or credentials.
    pub fn code_index_record_query(
        &mut self,
        scope: &str,
        telemetry: &CodeQueryTelemetry,
    ) -> Result<i64> {
        ensure!(!scope.is_empty(), "Code query scope cannot be empty");
        ensure!(
            !telemetry.query.is_empty() && telemetry.query.len() <= 1000,
            "Code query telemetry exceeds query bound"
        );
        ensure!(
            telemetry.result_paths.len() <= 20
                && telemetry
                    .result_paths
                    .iter()
                    .all(|path| !path.is_empty() && path.len() <= 4096),
            "Code query telemetry exceeds result bound"
        );
        let tx = self
            .index_mut()?
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        tx.execute(
            "INSERT INTO code_index_queries(scope,session,source,query,created_at,elapsed_ms,
             candidates,returned,stale_suppressed,semantic,coverage_indexed,coverage_total,result_paths)
             VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13)",
            params![
                scope,
                telemetry.session,
                telemetry.source.as_str(),
                telemetry.query,
                chrono::Utc::now().to_rfc3339(),
                telemetry.elapsed_ms,
                telemetry.candidates,
                telemetry.returned,
                telemetry.stale_suppressed,
                telemetry.semantic,
                telemetry.coverage.map(|value| value.0),
                telemetry.coverage.map(|value| value.1),
                serde_json::to_string(&telemetry.result_paths)?,
            ],
        )?;
        let id = tx.last_insert_rowid();
        tx.execute(
            "DELETE FROM code_index_queries WHERE scope=?1 AND id NOT IN (
             SELECT id FROM code_index_queries WHERE scope=?1 ORDER BY id DESC LIMIT 2000)",
            [scope],
        )?;
        tx.commit()?;
        Ok(id)
    }

    pub fn code_index_query_summary(&self, scope: &str) -> Result<CodeQuerySummary> {
        self.index()?
            .query_row(
                "SELECT COUNT(*),COALESCE(SUM(returned=0),0),COALESCE(SUM(stale_suppressed),0),
             CAST(COALESCE(ROUND(AVG(elapsed_ms)),0) AS INTEGER),MAX(created_at)
             FROM code_index_queries WHERE scope=?1",
                [scope],
                |row| {
                    Ok(CodeQuerySummary {
                        queries: row.get(0)?,
                        abstentions: row.get(1)?,
                        stale_suppressions: row.get(2)?,
                        average_elapsed_ms: row.get(3)?,
                        last_query_at: row.get(4)?,
                    })
                },
            )
            .map_err(Into::into)
    }

    pub fn code_history_replace(
        &mut self,
        scope: &str,
        snapshot: &CodeHistorySnapshot,
    ) -> Result<bool> {
        ensure!(!scope.is_empty(), "Code history scope cannot be empty");
        ensure!(
            !snapshot.head.is_empty() && !snapshot.snapshot_hash.is_empty(),
            "Code history snapshot identity is missing"
        );
        ensure!(
            snapshot.entries.len() <= 5000,
            "Code history exceeds storage bound"
        );
        if self
            .index()?
            .query_row(
                "SELECT snapshot_hash FROM code_history_state WHERE scope=?1",
                [scope],
                |row| row.get::<_, String>(0),
            )
            .optional()?
            .is_some_and(|hash| hash == snapshot.snapshot_hash)
        {
            return Ok(false);
        }
        for entry in &snapshot.entries {
            ensure!(
                !entry.revision.is_empty()
                    && entry.revision.len() <= 128
                    && entry.subject.len() <= 1000
                    && entry.paths.len() <= 512
                    && entry
                        .paths
                        .iter()
                        .all(|path| !path.is_empty() && path.len() <= 4096),
                "Invalid code history entry"
            );
        }
        let tx = self
            .index_mut()?
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        tx.execute("DELETE FROM code_history_fts WHERE scope=?1", [scope])?;
        tx.execute("DELETE FROM code_history_entries WHERE scope=?1", [scope])?;
        for entry in &snapshot.entries {
            tx.execute(
                "INSERT INTO code_history_entries(scope,revision,body) VALUES(?1,?2,?3)",
                params![scope, entry.revision, serde_json::to_string(entry)?],
            )?;
            tx.execute(
                "INSERT INTO code_history_fts(scope,revision,subject,paths) VALUES(?1,?2,?3,?4)",
                params![scope, entry.revision, entry.subject, entry.paths.join(" ")],
            )?;
        }
        tx.execute(
            "INSERT INTO code_history_state(scope,head,snapshot_hash,indexed_at,commits)
             VALUES(?1,?2,?3,?4,?5) ON CONFLICT(scope) DO UPDATE SET
             head=excluded.head,snapshot_hash=excluded.snapshot_hash,
             indexed_at=excluded.indexed_at,commits=excluded.commits",
            params![
                scope,
                snapshot.head,
                snapshot.snapshot_hash,
                chrono::Utc::now().to_rfc3339(),
                snapshot.entries.len()
            ],
        )?;
        tx.commit()?;
        Ok(true)
    }

    pub fn code_history_available(&self, scope: &str) -> Result<bool> {
        Ok(self.index()?.query_row(
            "SELECT EXISTS(SELECT 1 FROM code_history_state WHERE scope=?1)",
            [scope],
            |row| row.get(0),
        )?)
    }

    pub fn code_history_lexical(
        &self,
        scope: &str,
        expression: &str,
        limit: usize,
    ) -> Result<Vec<(CodeHistoryEntry, f64)>> {
        ensure!(!expression.is_empty(), "Code history query is empty");
        let index = self.index()?;
        let mut statement = index.prepare(
            "SELECT e.body,bm25(code_history_fts) FROM code_history_fts
             JOIN code_history_entries e ON e.scope=code_history_fts.scope
             AND e.revision=code_history_fts.revision
             WHERE code_history_fts.scope=?1 AND code_history_fts MATCH ?2
             ORDER BY bm25(code_history_fts),e.revision LIMIT ?3",
        )?;
        let rows = statement.query_map(params![scope, expression, limit.min(100)], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, f64>(1)?))
        })?;
        rows.map(|row| {
            let (body, score) = row?;
            Ok((serde_json::from_str(&body)?, score))
        })
        .collect()
    }

    pub fn code_index_chunks_for_paths(
        &self,
        scope: &str,
        paths: &[String],
        chunks_per_path: usize,
    ) -> Result<Vec<CodeChunk>> {
        ensure!(paths.len() <= 100, "Too many code history paths");
        let index = self.index()?;
        let mut statement = index.prepare(
            "SELECT body FROM code_index_chunks WHERE scope=?1 AND path=?2
             ORDER BY CAST(json_extract(body,'$.start_line') AS INTEGER) LIMIT ?3",
        )?;
        let mut chunks = Vec::new();
        for path in paths {
            let rows = statement
                .query_map(params![scope, path, chunks_per_path.clamp(1, 4)], |row| {
                    row.get::<_, String>(0)
                })?;
            for row in rows {
                chunks.push(serde_json::from_str(&row?)?);
            }
        }
        Ok(chunks)
    }

    pub fn code_index_symbol_chunks(
        &self,
        scope: &str,
        symbols: &[String],
        limit: usize,
    ) -> Result<Vec<(CodeChunk, String, String)>> {
        ensure!(symbols.len() <= 32, "Too many exact code symbols");
        let limit = limit.clamp(1, 500);
        let index = self.index()?;
        let mut statement = index.prepare(
            "SELECT c.body,s.symbol,s.role FROM code_index_symbols s
             JOIN code_index_chunks c ON c.scope=s.scope AND c.id=s.chunk_id
             WHERE s.scope=?1 AND s.normalized=?2
             ORDER BY s.role='declaration' DESC,s.path,c.id,s.role LIMIT ?3",
        )?;
        let mut seen = std::collections::HashSet::new();
        let mut chunks = Vec::new();
        for symbol in symbols {
            let rows =
                statement.query_map(params![scope, symbol.to_ascii_lowercase(), limit], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                    ))
                })?;
            for row in rows {
                let (body, symbol, role) = row?;
                let chunk: CodeChunk = serde_json::from_str(&body)?;
                if seen.insert((chunk.id.clone(), role.clone())) {
                    chunks.push((chunk, symbol, role));
                    if chunks.len() == limit {
                        return Ok(chunks);
                    }
                }
            }
        }
        Ok(chunks)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snapshot(text: &str) -> CodeIndexSnapshot {
        let hash = crate::memory::digest(text.as_bytes());
        CodeIndexSnapshot {
            snapshot_hash: hash.clone(),
            files: vec![CodeIndexFile {
                path: "arena.rs".into(),
                hash: hash.clone(),
                source_bytes: text.len(),
                language: "rust".into(),
            }],
            chunks: vec![CodeChunk {
                id: hash.clone(),
                path: "arena.rs".into(),
                file_hash: hash.clone(),
                content_hash: hash,
                language: "rust".into(),
                kind: "declaration".into(),
                start_line: 1,
                end_line: 1,
                symbols: vec!["shield_drop".into()],
                references: vec!["shield".into(), "drop".into()],
                content: text.into(),
            }],
            source_bytes: text.len(),
            skipped: 0,
        }
    }

    #[test]
    fn complete_generation_is_searchable_and_failed_replacement_preserves_it() {
        let home = tempfile::tempdir().unwrap();
        let mut store = Store::open(home.path()).unwrap();
        let valid = snapshot("fn shield_drop() {}");
        assert!(store.code_index_replace("repo", &valid).unwrap());
        assert_eq!(
            store.code_index_lexical("repo", "\"shield\"", 10).unwrap()[0]
                .0
                .path,
            "arena.rs"
        );
        let exact = store
            .code_index_symbol_chunks("repo", &["shield_drop".into()], 10)
            .unwrap();
        assert_eq!(exact[0].1, "shield_drop");
        assert_eq!(exact[0].2, "declaration");
        let mut invalid = snapshot("fn boss_collision() {}");
        invalid.files.clear();
        assert!(store.code_index_replace("repo", &invalid).is_err());
        assert_eq!(
            store.code_index_status("repo").unwrap().unwrap().generation,
            1
        );
        assert_eq!(
            store.code_index_lexical("repo", "\"shield\"", 10).unwrap()[0]
                .0
                .content,
            "fn shield_drop() {}"
        );
        store
            .code_index_record_failure("repo", "bounded scan failed\nsecret second line")
            .unwrap();
        let state = store.code_index_status("repo").unwrap().unwrap();
        assert_eq!(state.generation, 1);
        assert!(state.status.starts_with("refresh_failed:"));
        assert!(!state.status.contains('\n'));
    }

    #[test]
    fn query_telemetry_is_bounded_and_aggregated_without_source_text() {
        let home = tempfile::tempdir().unwrap();
        let mut store = Store::open(home.path()).unwrap();
        let id = store
            .code_index_record_query(
                "repo",
                &CodeQueryTelemetry {
                    session: "session".into(),
                    source: CodeQuerySource::Tool,
                    query: "shield drop".into(),
                    elapsed_ms: 42,
                    candidates: 5,
                    returned: 0,
                    stale_suppressed: 1,
                    semantic: true,
                    coverage: Some((4, 10)),
                    result_paths: vec![],
                },
            )
            .unwrap();
        assert!(id > 0);
        assert_eq!(
            store.code_index_query_summary("repo").unwrap(),
            CodeQuerySummary {
                queries: 1,
                abstentions: 1,
                stale_suppressions: 1,
                average_elapsed_ms: 42,
                last_query_at: store
                    .code_index_query_summary("repo")
                    .unwrap()
                    .last_query_at,
            }
        );
    }

    #[test]
    fn history_replacement_is_atomic_searchable_and_content_addressed() {
        let home = tempfile::tempdir().unwrap();
        let mut store = Store::open(home.path()).unwrap();
        let snapshot = CodeHistorySnapshot {
            head: "abc".into(),
            snapshot_hash: "history-one".into(),
            entries: vec![CodeHistoryEntry {
                revision: "abc".into(),
                unix_time: 100,
                subject: "Reduce arena shield drops".into(),
                paths: vec!["src/arena.rs".into()],
            }],
        };
        assert!(store.code_history_replace("repo", &snapshot).unwrap());
        assert!(!store.code_history_replace("repo", &snapshot).unwrap());
        let hits = store
            .code_history_lexical("repo", "\"shield\"", 10)
            .unwrap();
        assert_eq!(hits[0].0.paths, ["src/arena.rs"]);
        let invalid = CodeHistorySnapshot {
            entries: vec![CodeHistoryEntry {
                revision: String::new(),
                ..snapshot.entries[0].clone()
            }],
            snapshot_hash: "history-two".into(),
            ..snapshot
        };
        assert!(store.code_history_replace("repo", &invalid).is_err());
        assert_eq!(
            store
                .code_history_lexical("repo", "\"shield\"", 10)
                .unwrap()
                .len(),
            1
        );
    }
}
