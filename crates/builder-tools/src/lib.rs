//! Typed tool registry, workspace boundaries, and bounded execution.
pub mod code_index;
pub mod git_history;
pub mod research;
pub mod semantic;
mod workspace;
use anyhow::Result;
use builder_core::protocol::ToolCall;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
pub use workspace::Workspace;

#[derive(Debug, Deserialize, Serialize)]
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
    TaskUpdate {
        next_action: String,
        questions: Vec<String>,
    },
    Shell {
        command: String,
        #[serde(default)]
        timeout_secs: Option<u64>,
    },
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Risk {
    Read,
    Write,
    Execute,
}

impl Action {
    pub fn from_call(call: &ToolCall) -> Result<Self> {
        Ok(serde_json::from_value(
            json!({"name": call.function.name, "arguments": serde_json::from_str::<Value>(&call.function.arguments)?}),
        )?)
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
            Self::WriteFile { .. } | Self::EditFile { .. } | Self::MemoryForget { .. } => {
                Risk::Write
            }
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
            Self::TaskUpdate { next_action, .. } => format!("task state · {next_action}"),
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
            Self::Shell { command, .. } => format!("shell · {command}"),
        }
    }
}

pub fn definitions() -> Vec<Value> {
    definitions_with_pipeline(&builder_core::config::PipelineSettings::default())
}
pub fn definitions_with_pipeline(settings: &builder_core::config::PipelineSettings) -> Vec<Value> {
    definitions_with_pipeline_and_phase(settings, None)
}

pub fn definitions_with_pipeline_and_phase(
    settings: &builder_core::config::PipelineSettings,
    phase: Option<&builder_core::research::Phase>,
) -> Vec<Value> {
    let mut tools = vec![
        schema(
            "list_files",
            "List workspace files, respecting ignore files. Optional glob.",
            json!({"glob":{"type":"string"}}),
            &[],
        ),
        schema(
            "read_file",
            "Read a UTF-8 file with line numbers: at most 500 lines and 12288 output bytes per call. Whole-file reads are allowed only up to 200 lines; for larger files provide start_line (end_line optional — omitting it reads the next chunk, omitting start_line reads the chunk ending at end_line). Ranges clamp to the file and the result notes any remaining lines; search for the relevant symbol first. Do not repeatedly read whole files or bypass limits with shell. Re-read only a needed range when exact text or freshness matters.",
            json!({"path":{"type":"string"},"start_line":{"type":"integer","minimum":1},"end_line":{"type":"integer","minimum":1}}),
            &["path"],
        ),
        schema(
            "search",
            "Search literal text in workspace files, respecting ignore files. At most 30 results and 8192 output bytes. Narrow glob to the relevant file; use returned line numbers for focused read_file calls.",
            json!({"query":{"type":"string"},"glob":{"type":"string"}}),
            &["query"],
        ),
        schema(
            "write_file",
            "Create or replace a UTF-8 file. Requires approval; existing files are backed up. Parent directory must exist.",
            json!({"path":{"type":"string"},"content":{"type":"string"}}),
            &["path", "content"],
        ),
        schema(
            "edit_file",
            "Replace exactly one occurrence of old text with new text. Requires approval; original is backed up.",
            json!({"path":{"type":"string"},"old":{"type":"string"},"new":{"type":"string"}}),
            &["path", "old", "new"],
        ),
        schema(
            "shell",
            "Execute a shell command in the workspace. Requires approval. Shell has the user's permissions, not a sandbox. Timeout up to 120 seconds.",
            json!({"command":{"type":"string"},"timeout_secs":{"type":"integer","minimum":1,"maximum":120}}),
            &["command"],
        ),
    ];
    if settings.enabled {
        tools.push(match phase {
            Some(phase) => crate::research::definition_with_phase(settings, phase),
            None => crate::research::definition_with_settings(settings),
        });
    }
    if settings.code_index {
        tools.push(schema(
            "code_search",
            "Search the current checkout's structural code index. Combines exact symbols and paths, FTS5 lexical ranking, symbol-reference graph expansion, and local semantic vectors when available. Results are source-hash validated, diversified, bounded navigation leads; read the returned current range before editing. If the index abstains, use a targeted literal search instead of repeating the same query.",
            json!({"query":{"type":"string","minLength":1,"maxLength":1000},"limit":{"type":"integer","minimum":1,"maximum":20}}),
            &["query"],
        ));
    }
    tools
}
fn schema(name: &str, description: &str, properties: Value, required: &[&str]) -> Value {
    json!({"type":"function","function":{"name":name,"description":description,"parameters":{"type":"object","properties":properties,"required":required,"additionalProperties":false}}})
}

/// One bounded, single-line description of a requested call, for status display.
/// Built from raw arguments so a malformed request still shows what was asked.
/// The text is untrusted data; callers sanitize and clip it for their terminal.
pub fn call_summary(name: &str, arguments: &str) -> String {
    let args: Value = serde_json::from_str(arguments).unwrap_or(Value::Null);
    let field = |key: &str| args[key].as_str().unwrap_or_default().trim();
    let summary = match name {
        "read_file" => match (
            field("path"),
            args["start_line"].as_u64(),
            args["end_line"].as_u64(),
        ) {
            (path, Some(start), Some(end)) => format!("{path}:{start}-{end}"),
            (path, _, _) => path.to_owned(),
        },
        "list_files" => match field("glob") {
            "" => "**/*".to_owned(),
            glob => glob.to_owned(),
        },
        "search" => match (field("query"), field("glob")) {
            (query, "") => query.to_owned(),
            (query, glob) => format!("{query} in {glob}"),
        },
        "code_search" => field("query").to_owned(),
        "write_file" | "edit_file" => field("path").to_owned(),
        "shell" => field("command").to_owned(),
        "memory_search" => field("query").to_owned(),
        "memory_get" | "memory_upsert" | "memory_forget" => field("key").to_owned(),
        "task_update" => field("next_action").to_owned(),
        "research" => {
            let request = &args["request"];
            let operation = request["operation"].as_str().unwrap_or("?");
            match request["criterion"]
                .as_str()
                .or_else(|| request["query"].as_str())
                .or_else(|| request["command"].as_str())
                .or_else(|| request["claim"].as_str())
                .or_else(|| request["question"].as_str())
                .or_else(|| request["key"].as_str())
                .or_else(|| request["outcome"].as_str())
            {
                Some(detail) => format!("{operation} · {}", detail.trim()),
                None => operation.to_owned(),
            }
        }
        _ => String::new(),
    };
    one_line(&summary, 240)
}

/// A short outcome note for a completed call: the reason a call failed, or the
/// shape of what a successful call returned. Never a claim the tool succeeded.
pub fn result_note(name: &str, result: &str) -> Option<String> {
    for prefix in ["ERROR:", "DENIED:"] {
        if let Some(reason) = result.strip_prefix(prefix) {
            return Some(one_line(reason, 160));
        }
    }
    let note = match name {
        // Shell reports its own exit status; a nonzero exit is a successful
        // execution with a failing command, and the distinction must be visible.
        // A clean exit needs no note; the nonzero case is the one a user must
        // not mistake for success just because the tool call itself worked.
        "shell" => match result
            .strip_prefix("exit: ")
            .and_then(|rest| rest.split('\n').next())
        {
            Some("0" | "exit status: 0" | "exit code: 0") | None => return None,
            Some(status) => format!(
                "exit {}",
                status
                    .strip_prefix("exit status: ")
                    .or_else(|| status.strip_prefix("exit code: "))
                    .unwrap_or(status)
            ),
        },
        "read_file" | "search" | "list_files" | "code_search" => {
            let lines = result.lines().filter(|line| !line.is_empty()).count();
            format!("{lines} lines")
        }
        _ => return None,
    };
    Some(note)
}

fn one_line(text: &str, max: usize) -> String {
    let mut out = String::with_capacity(text.len().min(max + 1));
    let mut space = true;
    for c in text.trim().chars() {
        if out.chars().count() >= max {
            out.push('…');
            break;
        }
        if c.is_whitespace() {
            if !space {
                out.push(' ');
                space = true;
            }
        } else {
            out.push(c);
            space = false;
        }
    }
    out.trim_end().to_owned()
}
