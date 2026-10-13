//! Plain activity descriptions shared by terminal and remote adapters.
//! No terminal, HTTP, or runtime state belongs here.
use crate::agent::SubagentActivity;

/// What a subagent is doing, as a lowercase phrase.
pub fn subagent_activity(activity: &SubagentActivity) -> String {
    match activity {
        SubagentActivity::Tool { name, detail } => format!("{} {detail}", name.replace('_', " ")),
        SubagentActivity::Waiting => "waiting for the model".into(),
        SubagentActivity::Reading { processed, total } => format!(
            "reading {} of {} prompt tokens",
            count(*processed),
            count(*total)
        ),
        SubagentActivity::Thinking => "thinking".into(),
        SubagentActivity::Preparing => "preparing actions".into(),
        SubagentActivity::Writing => "writing".into(),
        SubagentActivity::Compacting { fraction } => format!(
            "summarizing its context {}%",
            (fraction.clamp(0.0, 1.0) * 100.0).round()
        ),
    }
}

/// How far a subagent has got: its round, or its report once tools are done.
pub fn subagent_round(round: usize, reporting: bool) -> String {
    if reporting {
        "writing its report".into()
    } else if round == 0 {
        "starting".into()
    } else {
        format!("round {round}")
    }
}

fn count(value: u64) -> String {
    if value < 1000 {
        value.to_string()
    } else {
        format!("{:.1}k", value as f64 / 1000.0)
    }
}

/// The user-facing record of a progress nudge. Shared with remote clients.
pub fn edit_focus_line(calls: usize, step: (usize, usize), reads: bool) -> String {
    let (number, total) = step;
    let paused = if reads {
        "search paused until it edits"
    } else {
        "only edits, shell and plan updates allowed"
    };
    format!("{calls} actions on todo step {number} of {total} without a file change · {paused}")
}

pub fn nudge_line(
    calls: usize,
    step: Option<(usize, usize)>,
    repeated_reads: usize,
    planning: bool,
) -> String {
    let mut line = format!("{calls} actions without new evidence or a file change");
    if repeated_reads > 0 {
        line.push_str(&format!(
            " · {repeated_reads} repeated read{}",
            if repeated_reads == 1 { "" } else { "s" }
        ));
    }
    match step {
        Some((number, total)) => {
            line.push_str(&format!(" · nudged back to todo step {number} of {total}"))
        }
        None if planning => line.push_str(" · no plan yet, nudged to write a todo list"),
        None => line.push_str(" · nudged to act on what it found"),
    }
    line
}
