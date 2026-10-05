use crate::{MAX_EDITS, SUBAGENT_DESCRIPTION_BYTES, SUBAGENT_PROMPT_BYTES, SUBAGENT_TOOL};
use builder_core::config::PipelineSettings;
use serde_json::{Value, json};

pub fn definitions() -> Vec<Value> {
    definitions_with_pipeline(&PipelineSettings::default())
}

pub fn definitions_with_pipeline(settings: &PipelineSettings) -> Vec<Value> {
    definitions_with_pipeline_and_phase(settings, None)
}

pub fn definitions_with_pipeline_and_phase(
    settings: &PipelineSettings,
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
            "Read a UTF-8 file with line numbers: at most 500 lines and 12288 output bytes per call. Files up to 200 lines are returned whole. A rangeless read of a larger file returns an outline of its declarations with line numbers plus its opening lines; choose the next range from that outline, code-index candidates, code_search, or file-scoped search, then pass start_line (end_line optional; omitting it reads the next chunk, omitting start_line reads the chunk ending at end_line). A range that exceeds a limit returns the part that fits and the exact start_line to continue with. Read many files or ranges in one batch; reads run in parallel. Do not page through whole files or bypass limits with shell. Re-read only a needed range when exact text or freshness matters.",
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
            "multi_edit",
            "Apply several exact-text replacements to one file in a single atomic write. Edits apply in order, each to the result of the previous one. Each old text must match exactly once unless replace_all is true. If any edit fails, none are applied. Prefer this over several edit_file calls on one file. Requires approval; the original is backed up.",
            json!({"path":{"type":"string"},"edits":{"type":"array","minItems":1,"maxItems":MAX_EDITS,"items":{"type":"object","properties":{"old":{"type":"string","minLength":1},"new":{"type":"string"},"replace_all":{"type":"boolean"}},"required":["old","new"],"additionalProperties":false}}}),
            &["path", "edits"],
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
    if settings.todos {
        tools.push(schema(
            builder_core::todo::TOOL,
            "Replace your ordered todo list; the user sees it as a board. For multi-step work, write the concrete steps in order as soon as you know which files to change, usually after a few reads; a detail still to confirm can be its own step. Then follow them instead of re-exploring. Keep exactly one item in_progress. When an item is done, mark it completed and set the next one in_progress in the same response as your next action. Rewrite the list when the user changes direction; an empty list clears it. Not needed for single-step or question-only requests. Writing the list is not progress by itself.",
            json!({"todos":{"type":"array","maxItems":builder_core::todo::MAX_ITEMS,"items":{"type":"object","properties":{"content":{"type":"string","minLength":1,"maxLength":builder_core::todo::MAX_ITEM_BYTES},"status":{"type":"string","enum":["pending","in_progress","completed"]}},"required":["content","status"],"additionalProperties":false}}}),
            &["todos"],
        ));
    }
    if settings.subagents {
        tools.push(schema(
            SUBAGENT_TOOL,
            "Delegate a self-contained, read-only investigation to a subagent with its own fresh context. It can list, search and read files (and use code_search) but cannot edit or run commands. Only its final report comes back to you, so use it to explore unfamiliar code, trace a flow across many files, or answer a question that would otherwise take many reads. Several subagent calls in one response run in parallel, so split independent questions. Write a complete prompt: the goal, what is already known, likely paths, and exactly what the report must contain (for example file paths with line numbers and the exact code to change). Do not delegate what one or two reads can answer.",
            json!({"description":{"type":"string","minLength":1,"maxLength":SUBAGENT_DESCRIPTION_BYTES,"description":"3–8 words shown to the user"},"prompt":{"type":"string","minLength":1,"maxLength":SUBAGENT_PROMPT_BYTES}}),
            &["description", "prompt"],
        ));
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

pub(crate) fn schema(name: &str, description: &str, properties: Value, required: &[&str]) -> Value {
    json!({"type":"function","function":{"name":name,"description":description,"parameters":{"type":"object","properties":properties,"required":required,"additionalProperties":false}}})
}
