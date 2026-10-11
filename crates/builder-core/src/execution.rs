//! Durable execution facts. Legacy records never infer outcomes from prose.
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy)]
pub enum AttemptOutcome {
    Complete,
    Failed,
}
impl AttemptOutcome {
    pub(crate) fn as_str(self) -> &'static str {
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
