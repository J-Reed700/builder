//! Protocol-independent checks on a decoded assistant reply. Every wire
//! protocol decodes into [`Message`] and must pass [`validate`] before the
//! reply can enter the durable conversation or dispatch a tool.
use anyhow::{Context, Result, ensure};
use builder_core::protocol::{Message, Role};
use serde_json::Value;

#[derive(Debug)]
pub(crate) enum MissingResponse {
    Empty,
    ReasoningOnly,
}
impl std::fmt::Display for MissingResponse {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let reason = match self {
            Self::Empty => "Empty model response: endpoint finished without an answer or tool call",
            Self::ReasoningOnly => "Model returned only reasoning, without an answer or tool call",
        };
        write!(f, "{reason}. No response was committed")
    }
}
impl std::error::Error for MissingResponse {}

pub(crate) fn validate(message: &Message, thinking: bool) -> Result<()> {
    ensure!(message.role == Role::Assistant, "Expected assistant role");
    ensure!(
        message
            .content
            .as_ref()
            .is_some_and(|s| !s.trim().is_empty())
            || !message.tool_calls.is_empty(),
        if thinking {
            MissingResponse::ReasoningOnly
        } else {
            MissingResponse::Empty
        }
    );
    let mut ids = std::collections::HashSet::new();
    for call in &message.tool_calls {
        ensure!(
            !call.id.is_empty() && ids.insert(&call.id),
            "Missing or duplicate tool call ID"
        );
        ensure!(
            call.kind == "function" && !call.function.name.is_empty(),
            "Invalid tool call"
        );
        let args: Value =
            serde_json::from_str(&call.function.arguments).context("Incomplete tool arguments")?;
        ensure!(args.is_object(), "Tool arguments must be an object");
    }
    Ok(())
}
