//! Research requests and source evidence, independent of persistence.
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Selector {
    File,
    JsonPointer {
        pointer: String,
    },
    /// Exact unique source text; changes elsewhere need not invalidate this anchor.
    Anchor {
        text: String,
    },
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Artifact {
    pub path: String,
    pub selector: Selector,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Observation {
    pub artifact: Artifact,
    pub hash: String,
    pub file_hash: String,
    pub excerpt: String,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    Locate,
    Diagnose,
    Implement,
    Verify,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Completion {
    Verified,
    Unverified,
    Blocked,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SymbolFeature {
    Definition,
    References,
    Diagnostics,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Patch {
    pub path: String,
    pub source_hash: String,
    pub old: String,
    pub new: String,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "operation", rename_all = "snake_case", deny_unknown_fields)]
pub enum Request {
    Plan {
        criteria: Vec<String>,
    },
    Review {
        verification_ids: Vec<String>,
    },
    Finish {
        outcome: Completion,
        explanation: String,
    },
    Semantic {
        server: String,
        args: Vec<String>,
        path: String,
        line: usize,
        character: usize,
        feature: SymbolFeature,
    },
    Observe {
        artifacts: Vec<Artifact>,
    },
    Symbols {
        query: String,
        glob: Option<String>,
    },
    Hypothesis {
        claim: String,
        falsification: String,
        evidence_ids: Vec<String>,
    },
    Verify {
        hypothesis_id: Option<String>,
        criterion: String,
        command: String,
        dependencies: Vec<Artifact>,
        timeout_secs: Option<u64>,
    },
    CandidateTest {
        hypothesis_id: String,
        criterion: String,
        patches: Vec<Patch>,
        command: String,
        timeout_secs: Option<u64>,
    },
    CandidateApply {
        candidate_id: String,
    },
    Learn {
        key: String,
        phase: Phase,
        procedure: String,
        applicability: String,
        verification_ids: Vec<String>,
        supersedes: Option<String>,
    },
    Retire {
        key: String,
        reason: String,
    },
    Recall {
        query: String,
        phase: Phase,
    },
    HistorySearch {
        query: String,
        before_seq: Option<i64>,
        include_archived: bool,
    },
    HistoryRead {
        seq: i64,
        offset: usize,
    },
    Analyze {
        question: String,
        artifacts: Vec<Artifact>,
    },
    Status,
}
#[derive(Debug, Clone, Serialize)]
pub struct Record {
    pub seq: i64,
    pub session: String,
    pub call_id: String,
    pub request: Request,
    pub result: serde_json::Value,
}
impl Request {
    pub fn operation(&self) -> &'static str {
        match self {
            Self::Plan { .. } => "plan",
            Self::Review { .. } => "review",
            Self::Finish { .. } => "finish",
            Self::Semantic { .. } => "semantic",
            Self::Observe { .. } => "observe",
            Self::Symbols { .. } => "symbols",
            Self::Hypothesis { .. } => "hypothesis",
            Self::Verify { .. } => "verify",
            Self::CandidateTest { .. } => "candidate_test",
            Self::CandidateApply { .. } => "candidate_apply",
            Self::Learn { .. } => "learn",
            Self::Retire { .. } => "retire",
            Self::Recall { .. } => "recall",
            Self::HistorySearch { .. } => "history_search",
            Self::HistoryRead { .. } => "history_read",
            Self::Analyze { .. } => "analyze",
            Self::Status => "status",
        }
    }
}
