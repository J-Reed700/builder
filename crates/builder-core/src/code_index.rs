//! Checkout-scoped index values. Indexed text is navigation evidence;
//! callers must validate file hashes against current source before use.
use serde::{Deserialize, Serialize};

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
    pub(crate) fn as_str(self) -> &'static str {
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
