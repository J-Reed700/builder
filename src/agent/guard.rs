//! Durable tool-state interpretation and replay/liveness guards.

use builder_core::{
    protocol::{Message, Role, ToolCall},
    store::ToolOutcome,
};
use builder_tools::{Action, Risk};

pub(super) fn failed_tool_streak(
    history: &[Message],
    outcomes: &std::collections::HashMap<String, ToolOutcome>,
) -> usize {
    let mut failures = 0;
    for message in history {
        match message.role {
            Role::User => failures = 0,
            Role::Tool => {
                if message
                    .tool_call_id
                    .as_ref()
                    .and_then(|id| outcomes.get(id))
                    == Some(&ToolOutcome::Failed)
                {
                    failures += 1;
                } else if message
                    .tool_call_id
                    .as_ref()
                    .and_then(|id| outcomes.get(id))
                    == Some(&ToolOutcome::Changed)
                {
                    failures = 0;
                }
            }
            _ => {}
        }
    }
    failures
}

pub(super) fn no_progress_streak(
    history: &[Message],
    outcomes: &std::collections::HashMap<String, ToolOutcome>,
) -> usize {
    let mut calls = std::collections::HashMap::new();
    let mut seen_evidence = std::collections::HashSet::new();
    let mut count = 0;
    for message in history {
        if message.role == Role::User {
            count = 0;
            calls.clear();
            seen_evidence.clear();
        }
        for call in &message.tool_calls {
            calls.insert(call.id.as_str(), call);
        }
        let Some(id) = message.tool_call_id.as_deref() else {
            continue;
        };
        let Some(call) = calls.remove(id) else {
            continue;
        };
        if outcomes.get(id) == Some(&ToolOutcome::Changed) {
            count = 0;
            seen_evidence.clear();
        } else {
            let fresh_inspection = message
                .content
                .as_deref()
                .and_then(|result| fresh_inspection_evidence(call, result, outcomes.get(id)));
            if let Some(evidence) = fresh_inspection {
                if seen_evidence.insert(evidence) {
                    count = 0;
                } else {
                    count += 1;
                }
            } else {
                // Failed, denied, uncertain, and non-inspection calls consume
                // the liveness budget. Repeated evidence does too.
                count += 1;
            }
        }
    }
    count
}

type InspectionEvidence = (String, String);

fn fresh_inspection_evidence(
    call: &ToolCall,
    result: &str,
    outcome: Option<&ToolOutcome>,
) -> Option<InspectionEvidence> {
    if outcome != Some(&ToolOutcome::Succeeded) || result.trim().is_empty() {
        return None;
    }
    let action = Action::from_call(call).ok()?;
    if !matches!(
        action,
        Action::ListFiles { .. }
            | Action::ReadFile { .. }
            | Action::Search { .. }
            | Action::CodeSearch { .. }
            | Action::Subagent { .. }
    ) {
        return None;
    }
    Some((
        tool_signature(call),
        builder_core::memory::digest(result.as_bytes()),
    ))
}

/// Select the smallest useful research schema from typed, durable activity
/// after the latest user instruction. This does not inspect prompt wording,
/// endpoint identity, or model output prose.
pub(super) fn tool_phase(
    history: &[Message],
    outcomes: &std::collections::HashMap<String, ToolOutcome>,
) -> builder_core::research::Phase {
    use builder_core::research::{Phase, Request};
    let mut phase = Phase::Locate;
    let start = history
        .iter()
        .rposition(|message| message.role == Role::User)
        .map_or(0, |position| position + 1);
    for message in &history[start..] {
        for call in &message.tool_calls {
            let Some(outcome) = outcomes.get(&call.id) else {
                continue;
            };
            if *outcome == ToolOutcome::Changed {
                phase = Phase::Verify;
                continue;
            }
            if *outcome != ToolOutcome::Succeeded {
                continue;
            }
            let Ok(action) = Action::from_call(call) else {
                continue;
            };
            let candidate = match action {
                Action::ReadFile { .. }
                | Action::Search { .. }
                | Action::CodeSearch { .. }
                | Action::ListFiles { .. }
                | Action::Subagent { .. } => Some(Phase::Diagnose),
                Action::Research { request } => match request {
                    Request::CandidateApply { .. } | Request::Verify { .. } => Some(Phase::Verify),
                    Request::Hypothesis { .. } | Request::CandidateTest { .. } => {
                        Some(Phase::Implement)
                    }
                    Request::Observe { .. }
                    | Request::Symbols { .. }
                    | Request::Semantic { .. }
                    | Request::Analyze { .. } => Some(Phase::Diagnose),
                    _ => None,
                },
                _ => None,
            };
            if let Some(candidate) = candidate
                && phase_rank(&candidate) > phase_rank(&phase)
            {
                phase = candidate;
            }
        }
    }
    phase
}

fn phase_rank(phase: &builder_core::research::Phase) -> u8 {
    match phase {
        builder_core::research::Phase::Locate => 0,
        builder_core::research::Phase::Diagnose => 1,
        builder_core::research::Phase::Implement => 2,
        builder_core::research::Phase::Verify => 3,
    }
}

#[derive(Debug)]
pub(super) struct ToolRepetition {
    pub(super) name: String,
    pub(super) count: usize,
}

#[derive(Debug, Default)]
pub(super) struct ToolGuardState {
    pub(super) repetitions: std::collections::HashMap<String, ToolRepetition>,
    pub(super) blocked: std::collections::HashMap<String, ToolOutcome>,
}

#[derive(Debug)]
pub(super) struct GuardedCall {
    pub(super) signature: String,
    name: String,
    shell: bool,
}

pub(super) enum ToolGuardViolation<'a> {
    Blocked {
        call: &'a ToolCall,
        outcome: ToolOutcome,
    },
    Repeated {
        call: &'a ToolCall,
        prior_count: usize,
    },
}

pub(super) fn tool_signature(call: &ToolCall) -> String {
    let arguments = serde_json::from_str::<serde_json::Value>(&call.function.arguments)
        .and_then(|mut value| {
            // Tool schemas preserve declaration order for constrained decoding;
            // action identity must still ignore argument object ordering.
            value.sort_all_objects();
            serde_json::to_string(&value)
        })
        .unwrap_or_else(|_| call.function.arguments.clone());
    format!("{}\0{arguments}", call.function.name)
}

pub(super) fn invalid_tool_call_id(calls: &[ToolCall]) -> Option<&str> {
    let mut seen = std::collections::HashSet::new();
    calls.iter().find_map(|call| {
        (call.id.is_empty() || !seen.insert(call.id.as_str())).then_some(call.id.as_str())
    })
}

pub(super) fn structurally_valid_tool_call(call: &ToolCall) -> bool {
    call.kind == "function"
        && !call.function.name.is_empty()
        && serde_json::from_str::<serde_json::Value>(&call.function.arguments)
            .is_ok_and(|arguments| arguments.is_object())
}

pub(super) fn guarded_call(call: &ToolCall) -> Option<GuardedCall> {
    let action = Action::from_call(call).ok()?;
    if action.risk() == Risk::Read && !matches!(&action, Action::MemoryUpsert { .. }) {
        return None;
    }
    let signature = match &action {
        // A timeout changes only how long Builder waits, not the command whose
        // side effects may already have happened.
        Action::Shell { command, .. } => format!(
            "shell\0{}",
            serde_json::to_string(command).unwrap_or_else(|_| command.clone())
        ),
        _ => serde_json::to_string(&action).unwrap_or_else(|_| tool_signature(call)),
    };
    Some(GuardedCall {
        signature,
        name: call.function.name.clone(),
        shell: matches!(action, Action::Shell { .. }),
    })
}

pub(super) fn tool_guard_violation<'a>(
    calls: impl IntoIterator<Item = &'a ToolCall>,
    state: &ToolGuardState,
    identical_shell_calls: usize,
) -> Option<ToolGuardViolation<'a>> {
    let mut projected = state
        .repetitions
        .iter()
        .map(|(signature, repetition)| (signature.clone(), repetition.count))
        .collect::<std::collections::HashMap<_, _>>();
    for call in calls {
        let Some(guarded) = guarded_call(call) else {
            continue;
        };
        if let Some(outcome) = state.blocked.get(&guarded.signature) {
            return Some(ToolGuardViolation::Blocked {
                call,
                outcome: *outcome,
            });
        }
        if !guarded.shell {
            continue;
        }
        let count = projected.entry(guarded.signature).or_default();
        if *count >= identical_shell_calls {
            return Some(ToolGuardViolation::Repeated {
                call,
                prior_count: *count,
            });
        }
        *count += 1;
    }
    None
}

pub(super) fn tool_guard_state(
    history: &[Message],
    outcomes: &std::collections::HashMap<String, ToolOutcome>,
) -> ToolGuardState {
    let mut pending = std::collections::HashMap::new();
    let mut state = ToolGuardState::default();
    for message in history {
        if message.role == Role::User {
            pending.clear();
            state = ToolGuardState::default();
        }
        for call in &message.tool_calls {
            pending.insert(call.id.as_str(), guarded_call(call));
        }
        let Some(id) = message.tool_call_id.as_deref() else {
            continue;
        };
        let Some(Some(call)) = pending.remove(id) else {
            continue;
        };
        let outcome = outcomes.get(id).copied().unwrap_or(ToolOutcome::Unknown);
        if outcome == ToolOutcome::Changed {
            state.repetitions.clear();
        } else if call.shell {
            let repetition =
                state
                    .repetitions
                    .entry(call.signature.clone())
                    .or_insert(ToolRepetition {
                        name: call.name,
                        count: 0,
                    });
            repetition.count += 1;
        }
        if matches!(outcome, ToolOutcome::Denied | ToolOutcome::Uncertain) {
            state.blocked.insert(call.signature, outcome);
        }
    }
    state
}

pub(super) fn blocked_outcome_name(outcome: ToolOutcome) -> &'static str {
    match outcome {
        ToolOutcome::Denied => "denied",
        ToolOutcome::Uncertain => "uncertain",
        _ => "blocked",
    }
}
