use anyhow::Result;
use builder_core::protocol::ToolCall;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(tag = "name", content = "arguments", rename_all = "snake_case")]
pub enum Action {
    Research {
        request: builder_core::research::Request,
    },
    ListFiles {
        #[serde(default)]
        glob: Option<String>,
    },
    ReadFile {
        path: String,
        #[serde(default)]
        start_line: Option<usize>,
        #[serde(default)]
        end_line: Option<usize>,
    },
    Search {
        query: String,
        #[serde(default)]
        glob: Option<String>,
    },
    CodeSearch {
        query: String,
        #[serde(default)]
        limit: Option<usize>,
    },
    WriteFile {
        path: String,
        content: String,
    },
    EditFile {
        path: String,
        old: String,
        new: String,
    },
    MultiEdit {
        path: String,
        edits: Vec<Replacement>,
    },
    MemorySearch {
        query: String,
    },
    MemoryGet {
        key: String,
        #[serde(default)]
        revision: Option<i64>,
    },
    MemoryUpsert {
        key: String,
        text: String,
        expected_revision: i64,
        evidence_call_ids: Vec<String>,
    },
    MemoryForget {
        key: String,
        expected_revision: i64,
    },
    TodoWrite {
        todos: builder_core::todo::List,
    },
    Subagent {
        description: String,
        prompt: String,
    },
    Shell {
        command: String,
        #[serde(default)]
        timeout_secs: Option<u64>,
    },
}

/// One exact-text replacement within a `multi_edit`.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Replacement {
    pub old: String,
    pub new: String,
    #[serde(default)]
    pub replace_all: bool,
}

pub const MAX_EDITS: usize = 50;
pub const SUBAGENT_TOOL: &str = "subagent";
pub const SUBAGENT_DESCRIPTION_BYTES: usize = 80;
pub const SUBAGENT_PROMPT_BYTES: usize = 16 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Risk {
    Read,
    Write,
    Execute,
}

impl Action {
    pub fn from_call(call: &ToolCall) -> Result<Self> {
        Ok(serde_json::from_value(json!({
            "name": call.function.name,
            "arguments": serde_json::from_str::<Value>(&call.function.arguments)?
        }))?)
    }

    /// Reads the workspace without changing it: safe to run beside other
    /// inspections and to close as retryable if interrupted.
    pub fn is_inspection(&self) -> bool {
        matches!(
            self,
            Self::ListFiles { .. } | Self::ReadFile { .. } | Self::Search { .. }
        )
    }

    pub fn risk(&self) -> Risk {
        match self {
            Self::Research { request } => match request {
                builder_core::research::Request::Semantic { .. }
                | builder_core::research::Request::Verify { .. }
                | builder_core::research::Request::CandidateTest { .. } => Risk::Execute,
                builder_core::research::Request::CandidateApply { .. }
                | builder_core::research::Request::Retire { .. } => Risk::Write,
                _ => Risk::Read,
            },
            Self::WriteFile { .. }
            | Self::EditFile { .. }
            | Self::MultiEdit { .. }
            | Self::MemoryForget { .. } => Risk::Write,
            Self::Shell { .. } => Risk::Execute,
            _ => Risk::Read,
        }
    }

    pub fn description(&self) -> String {
        match self {
            Self::Research { request } => format!(
                "research · {}",
                serde_json::to_string(request).unwrap_or_default()
            ),
            Self::MemorySearch { query } => format!("recall · {query}"),
            Self::MemoryGet { key, .. } => format!("memory · {key}"),
            Self::MemoryUpsert { key, .. } => format!("remember finding · {key}"),
            Self::MemoryForget { key, .. } => format!("forget memory · {key}"),
            Self::TodoWrite { todos } => format!("todo list\n{}", todos.checklist()),
            Self::Subagent {
                description,
                prompt,
            } => format!("subagent · {description}\n{prompt}"),
            Self::ListFiles { glob } => {
                format!("list files · {}", glob.as_deref().unwrap_or("**/*"))
            }
            Self::ReadFile { path, .. } => format!("read · {path}"),
            Self::Search { query, .. } => format!("search · {query}"),
            Self::CodeSearch { query, .. } => format!("code index · {query}"),
            Self::WriteFile { path, content } => {
                format!("write · {path} ({} bytes)\n{content}", content.len())
            }
            Self::EditFile { path, old, new } => {
                format!("edit · {path}\n--- old\n{old}\n+++ new\n{new}")
            }
            Self::MultiEdit { path, edits } => {
                let mut text = format!("edit · {path} ({} edits)", edits.len());
                for (index, edit) in edits.iter().enumerate() {
                    let all = if edit.replace_all {
                        " (every match)"
                    } else {
                        ""
                    };
                    text.push_str(&format!(
                        "\n[{}]{all}\n--- old\n{}\n+++ new\n{}",
                        index + 1,
                        edit.old,
                        edit.new
                    ));
                }
                text
            }
            Self::Shell { command, .. } => format!("shell · {command}"),
        }
    }
}
