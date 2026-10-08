//! Transport-neutral runtime events consumed by application adapters.
use builder_provider::Event;

#[derive(Debug)]
pub enum AgentEvent {
    Model(Event),
    /// Why an automatic compaction was triggered. The terms are reported
    /// separately because the schema and output reservations, not the
    /// conversation alone, are usually what crosses the threshold.
    AutoCompact {
        messages: usize,
        overhead: usize,
        reserved: usize,
        threshold: usize,
        context_tokens: usize,
    },
    Compacting {
        before: usize,
        context_tokens: usize,
    },
    Compacted {
        before: usize,
        after: usize,
        context_tokens: usize,
    },
    /// Estimated summarization progress across all fragments, 0.0 to 1.0.
    CompactionProgress {
        fraction: f64,
        fragment: usize,
        stage: SummaryStage,
    },
    OutputRecovery {
        budget: usize,
    },
    SummaryRecovery {
        size: usize,
        limit: usize,
    },
    /// Consecutive tool failures triggered corrective guidance.
    ExplorationRecovery {
        calls: usize,
    },
    /// The model reached the progress check without new evidence or a file
    /// change and was nudged to commit. Repeats at each further multiple.
    ProgressNudge {
        calls: usize,
        /// Current todo item and item count, when a list is active.
        step: Option<(usize, usize)>,
        /// Reads of a file that was already read since the last file change.
        repeated_reads: usize,
        /// The nudge recommended writing a todo list.
        planning: bool,
    },
    MemoryNotice(String),
    /// The runtime told the model it is repeating itself. The user sees the
    /// same warning, so a developing loop is visible before the guard stops it.
    RepetitionNotice {
        name: String,
        count: usize,
        limit: usize,
    },
    ToolStarted {
        name: String,
        detail: String,
    },
    ToolFinished {
        name: String,
        detail: String,
        note: Option<String>,
        failed: bool,
    },
    /// A todo list was recorded. Emitted after its `ToolFinished`.
    TodosUpdated(builder_core::todo::List),
    /// A running subagent completed one of its own tool calls.
    SubagentProgress {
        call_id: String,
        description: String,
        actions: usize,
        activity: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum SummaryStage {
    /// Request sent; the server has not reported anything yet.
    Waiting,
    /// The server is processing the fragment's prompt.
    Reading { processed: u64, total: u64 },
    /// The model is producing the handoff. Hidden reasoning is counted apart
    /// from handoff text, and `expected` is the size the estimate runs against.
    Writing {
        summary_bytes: usize,
        reasoning_bytes: usize,
        expected: usize,
    },
}
