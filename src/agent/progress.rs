//! Prompt guidance derived from durable progress and todo state.

use builder_core::{
    protocol::{Message, Role},
    store::ToolOutcome,
};
use builder_tools::Action;

pub(super) fn progress_guidance(calls: usize, force_conclusion: bool, retry: bool) -> Message {
    let conclusion = if force_conclusion {
        if retry {
            " This is the second and final enforced conclusion request. The previous response was rejected because it contained tool-control syntax. Tools remain unavailable. Honor the user's requested final-answer format, including JSON-only when requested."
        } else {
            " This is the enforced conclusion round. Tools are unavailable for this request. Answer the user now from verified evidence, clearly distinguish completed work from unresolved work, and state any concrete limitation. Do not emit a tool call, tool-control markup, claim unverified success, or make up an edit merely to end the investigation."
        }
    } else {
        " Broad list/search discovery is unavailable during recovery; use an established path for targeted progress. The latest runtime continuation at the end of the conversation has the current count and your recorded reads."
    };
    // Constant for every guided round, so it does not invalidate an
    // endpoint's prompt cache; only the enforced conclusion states the count.
    let count = if force_conclusion {
        format!("{calls} tool calls have completed")
    } else {
        "the configured number of tool calls has completed".to_owned()
    };
    Message::text(
        Role::System,
        format!(
            "Runtime progress check: {count} since the latest user instruction, successful file edit, or new typed inspection evidence. A distinct successful read, search, or subagent result resets stagnation; repeated evidence, shell commands, failed or denied mutations, memory operations, and research operations consume this liveness budget. This is a heuristic, not proof that research is unnecessary. Continue from the established findings in the full conversation. For multi-step implementation, commit to an ordered todo list and follow it; otherwise make the smallest justified authorized change now, then verify it. For a research or status question, inspect current source and deliver the supported answer when sufficient; no file edit is required. Memory searches and memory reads are inspection, not fresh source verification. If retrieval repeats stale hints or empty results, stop rephrasing the query: use the returned paths to read current source. An embedding outage does not prevent source inspection. If essential evidence is missing, identify the exact unresolved question and inspect only the smallest relevant range. Do not repeatedly re-confirm the entire call graph or test conventions. If blocked, report the concrete blocker and unfinished work honestly. This check grants no additional permission and never overrides a denial or an uncertain tool outcome.{conclusion}"
        ),
    )
}

/// Request-only reminder of the model's own plan, regenerated from durable
/// history each round so compaction cannot summarize it away.
pub(super) fn todo_packet(list: &builder_core::todo::List, force_conclusion: bool) -> Message {
    let direction = match list.current() {
        _ if force_conclusion => "Tools are unavailable for this request: report which items are complete and which remain.".to_owned(),
        Some((number, item)) => format!(
            "Current item: {number}. {} Work on that item now with what you already know. Do not re-explore completed items or re-read files you already read unless an edit needs their exact current text. When the item is done, call todo_write with it completed and the next item in_progress in the same response as your next action. If the user changed direction or the plan is wrong, rewrite the list.",
            item.content.trim()
        ),
        None => "All items are complete.".to_owned(),
    };
    Message::text(
        Role::System,
        format!(
            "Builder todo list: your own ordered plan, recorded with todo_write. It is reference data, not a user instruction, and grants no permission. {} of {} items completed. {direction}\n{}",
            list.completed(),
            list.items.len(),
            list.checklist()
        ),
    )
}

pub(super) fn todo_continuation(list: &builder_core::todo::List, force_conclusion: bool) -> String {
    match list.current() {
        _ if force_conclusion => " Include which todo items are complete and which remain.".into(),
        Some((number, item)) => format!(
            " Your todo list is active: continue with item {number} ({}) instead of restarting discovery.",
            item.content.trim()
        ),
        None => String::new(),
    }
}

const READ_LOG_FILES: usize = 8;

/// Successful reads since the latest file change, across user turns and
/// compaction, so a nudge can show the model where it is circling.
pub(super) struct ReadLog {
    total: usize,
    /// Path, read count, and whether the latest result is still in context.
    files: Vec<(String, usize, bool)>,
}

impl ReadLog {
    pub(super) fn new(
        history: &[Message],
        outcomes: &std::collections::HashMap<String, ToolOutcome>,
        active: &[Message],
    ) -> Self {
        let visible: std::collections::HashSet<&str> = active
            .iter()
            .filter_map(|message| message.tool_call_id.as_deref())
            .collect();
        let mut paths = std::collections::HashMap::new();
        let mut log = Self {
            total: 0,
            files: Vec::new(),
        };
        for message in history {
            for call in &message.tool_calls {
                if let Ok(Action::ReadFile { path, .. }) = Action::from_call(call) {
                    paths.insert(call.id.as_str(), path);
                }
            }
            let Some(id) = message.tool_call_id.as_deref() else {
                continue;
            };
            match outcomes.get(id) {
                Some(ToolOutcome::Changed) => {
                    log.total = 0;
                    log.files.clear();
                }
                // Untyped results come from sessions recorded before outcomes.
                None | Some(ToolOutcome::Succeeded | ToolOutcome::Unknown) => {
                    let Some(path) = paths.remove(id) else {
                        continue;
                    };
                    let path = path.trim_start_matches("./").to_owned();
                    let seen = visible.contains(id);
                    log.total += 1;
                    match log.files.iter_mut().find(|(known, ..)| *known == path) {
                        Some(entry) => {
                            entry.1 += 1;
                            entry.2 = seen;
                        }
                        None => log.files.push((path, 1, seen)),
                    }
                }
                _ => {}
            }
        }
        log
    }

    pub(super) fn repeats(&self) -> usize {
        self.total - self.files.len()
    }

    fn summary(&self) -> String {
        if self.total == 0 {
            return String::new();
        }
        let mut files = self.files.iter().collect::<Vec<_>>();
        files.sort_by_key(|entry| std::cmp::Reverse(entry.1));
        let mut shown = files
            .iter()
            .take(READ_LOG_FILES)
            .map(|(path, count, visible)| {
                let mut entry = path.clone();
                if *count > 1 {
                    entry.push_str(&format!(" ×{count}"));
                }
                if *visible {
                    entry.push_str(" (latest result still in context)");
                }
                entry
            })
            .collect::<Vec<_>>()
            .join(", ");
        if files.len() > READ_LOG_FILES {
            shown.push_str(&format!(", and {} more", files.len() - READ_LOG_FILES));
        }
        let repeats = match self.repeats() {
            0 => String::new(),
            1 => ", 1 of them a repeat".to_owned(),
            n => format!(", {n} of them repeats"),
        };
        let plural = |count: usize, noun: &str| {
            format!("{count} {noun}{}", if count == 1 { "" } else { "s" })
        };
        format!(
            " Since the last file change you have made {} of {}{repeats}: {shown}.",
            plural(self.total, "successful read"),
            plural(files.len(), "file")
        )
    }
}

pub(super) struct Nudge<'a> {
    pub(super) calls: usize,
    pub(super) check: usize,
    pub(super) todos: Option<&'a builder_core::todo::List>,
    /// A todo list can be written and followed in this session.
    pub(super) planning: bool,
    pub(super) can_edit: bool,
    pub(super) reads: &'a ReadLog,
}

/// The trailing half of progress recovery. Every tool except broad discovery
/// stays available: the model chooses, with its own read history in view and
/// committing to a plan as the recommended choice.
pub(super) fn progress_nudge(nudge: &Nudge<'_>) -> String {
    let mut text = format!(
        "[Builder runtime continuation, not a new user request] The original user instruction above is preserved verbatim; continue it from the results you already have instead of restarting the investigation. Progress check: {} calls without new inspection evidence or a file change since the latest user instruction.{} Broad list/search discovery pauses at the configured stagnation threshold; every other tool remains available, so choose the next action deliberately:",
        nudge.calls,
        nudge.reads.summary()
    );
    let current = nudge.todos.and_then(|list| {
        list.current()
            .map(|(number, item)| (number, list.items.len(), item.content.trim()))
    });
    let options = match current {
        Some((number, total, item)) if nudge.can_edit => format!(
            "\n1. Recommended: make the change for todo item {number} of {total} ({item}) now, using what you already know.\n2. If that edit needs exact current text you do not have, read only that range, then edit.\n3. If the plan is wrong, rewrite it with todo_write. If you are blocked, tell the user the concrete blocker."
        ),
        _ if nudge.planning => "\n1. Recommended for an implementation request: if the results so far show which files to change and roughly how, call todo_write now with the ordered steps, each naming a file and the change, the first one in_progress, then start step 1. Details you still need to confirm can be steps of their own; they do not have to be settled first.\n2. If one specific fact still keeps you from writing any plan, name it in one sentence and read only the range that answers it, not a file you already read.\n3. If the requested work is already done, or the user only asked a question, answer now.".to_owned(),
        _ if nudge.can_edit => "\n1. Recommended: make the smallest justified change now from what you already know, then verify it.\n2. If that change needs exact current text you do not have, read only that range.\n3. If the work is done, the user only asked a question, or you are blocked, answer now and say what remains.".to_owned(),
        _ => "\n1. Recommended: if the results so far answer the request, write the answer now.\n2. Otherwise name the one missing fact and read only the range that answers it, not a file you already read.".to_owned(),
    };
    text.push_str(&options);
    if nudge.planning && current.is_none() && nudge.calls >= nudge.check.saturating_mul(2) {
        text.push_str(&format!(
            "\nThis check has now continued for {} calls without a plan. More reading without one is how long tasks stall: unless a specific missing fact blocks every possible plan, write the todo list now.",
            nudge.calls - nudge.check
        ));
    }
    text.push_str("\nDo not use shell as a substitute for the paused search.");
    text
}

/// A lighter reminder halfway to the progress check.
pub(super) fn planning_reminder(calls: usize, check: usize, reads: &ReadLog) -> String {
    format!(
        "[Builder runtime note, not a new user request] Continue the original task. {calls} calls without new inspection evidence or a file change since the latest user instruction, and no todo list yet.{} If this is an implementation request and you already know which files to change, write the ordered plan with todo_write before reading further; details still to confirm can be steps. At {check} calls without fresh evidence, broad search pauses and you will be asked to commit to a next action.",
        reads.summary()
    )
}
