//! Typed tool registry, workspace boundaries, and bounded execution.

mod action;
mod display;
mod registry;
mod workspace;

pub mod code_index;
pub mod git_history;
pub mod research;
pub mod semantic;

pub use action::{
    Action, MAX_EDITS, Replacement, Risk, SUBAGENT_DESCRIPTION_BYTES, SUBAGENT_PROMPT_BYTES,
    SUBAGENT_TOOL,
};
pub use display::{call_summary, result_note};
pub(crate) use registry::schema;
pub use registry::{definitions, definitions_with_pipeline, definitions_with_pipeline_and_phase};
pub use workspace::{ShellFailure, ShellFailureKind, Workspace};
