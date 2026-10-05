//! Completion is derived from durable execution facts, never assistant prose.
use anyhow::Result;
use builder_core::{
    protocol::Role,
    store::{Store, ToolOutcome},
};
use builder_tools::Workspace;
use serde::Serialize;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    Verified,
    Unverified,
    Failed,
    Blocked,
    Pending,
}

impl Outcome {
    pub fn requires_attention(self) -> bool {
        matches!(self, Self::Failed | Self::Blocked | Self::Pending)
    }
}

pub fn assess(
    store: &Store,
    session: &str,
    workspace: &Workspace,
    settings: &builder_core::config::PipelineSettings,
) -> Result<Outcome> {
    let history = store.messages(session)?;
    if crate::agent::pending(&history) {
        return Ok(Outcome::Pending);
    }
    // Full history survives automatic compaction; old turns cannot poison a
    // new instruction. Successful retries clear only their matching failure.
    let history = store.history_messages(session)?;
    let outcomes = store.tool_outcomes(session)?;
    let start = history
        .iter()
        .rposition(|m| m.role == Role::User)
        .map_or(0, |i| i + 1);
    let mut latest = std::collections::HashMap::new();
    let mut blocked = false;
    let mut last_finish = None;
    let results: std::collections::HashMap<_, _> = history[start..]
        .iter()
        .filter_map(|message| {
            message
                .tool_call_id
                .as_deref()
                .map(|id| (id, message.content.as_deref().unwrap_or_default()))
        })
        .collect();
    for (position, message) in history[start..].iter().enumerate() {
        for call in &message.tool_calls {
            if let Some(outcome) = outcomes.get(&call.id) {
                let mut outcome = *outcome;
                blocked |= matches!(outcome, ToolOutcome::Denied | ToolOutcome::Uncertain);
                let mut arguments =
                    serde_json::from_str::<serde_json::Value>(&call.function.arguments)
                        .unwrap_or(serde_json::Value::Null);
                if let Ok(builder_tools::Action::Research { request }) =
                    builder_tools::Action::from_call(call)
                    && outcome == ToolOutcome::Succeeded
                {
                    if matches!(request, builder_core::research::Request::Finish { .. }) {
                        last_finish = Some(position);
                    }
                    if let Some(result) = results
                        .get(call.id.as_str())
                        .and_then(|text| serde_json::from_str::<serde_json::Value>(text).ok())
                        && (result["passed"] == false || result["verdict"] == "needs_work")
                    {
                        outcome = ToolOutcome::Failed;
                    }
                }
                if call.function.name == "shell"
                    && let Some(object) = arguments.as_object_mut()
                {
                    object.remove("timeout_secs");
                }
                arguments.sort_all_objects();
                latest.insert(
                    (call.function.name.clone(), arguments.to_string()),
                    (outcome, position),
                );
            }
        }
    }
    if blocked {
        return Ok(Outcome::Blocked);
    }
    let research = crate::research::status_with_settings(store, session, workspace, settings)?;
    if research["finish_valid"] == true && research["outcome"] == "blocked" {
        return Ok(Outcome::Blocked);
    }
    let verified = research["finish_valid"] == true && research["outcome"] == "verified";
    if latest.values().any(|(outcome, position)| {
        *outcome == ToolOutcome::Failed
            && !(verified && last_finish.is_some_and(|finish| finish > *position))
    }) {
        return Ok(Outcome::Failed);
    }
    if verified {
        return Ok(Outcome::Verified);
    }
    // A final answer (including "done") proves only that generation ended.
    Ok(Outcome::Unverified)
}
