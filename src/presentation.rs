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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn subagent_activity_reads_as_a_lowercase_phrase() {
        assert_eq!(
            subagent_activity(&SubagentActivity::Tool {
                name: "read_file".into(),
                detail: "src/main.rs".into()
            }),
            "read file src/main.rs"
        );
        assert_eq!(
            subagent_activity(&SubagentActivity::Waiting),
            "waiting for the model"
        );
        assert_eq!(subagent_activity(&SubagentActivity::Thinking), "thinking");
        assert_eq!(
            subagent_activity(&SubagentActivity::Preparing),
            "preparing actions"
        );
        assert_eq!(subagent_activity(&SubagentActivity::Writing), "writing");
    }

    #[test]
    fn prompt_progress_counts_in_thousands_from_a_thousand_up() {
        assert_eq!(
            subagent_activity(&SubagentActivity::Reading {
                processed: 999,
                total: 1000
            }),
            "reading 999 of 1.0k prompt tokens"
        );
        assert_eq!(
            subagent_activity(&SubagentActivity::Reading {
                processed: 12_345,
                total: 131_072
            }),
            "reading 12.3k of 131.1k prompt tokens"
        );
        assert_eq!(
            subagent_activity(&SubagentActivity::Reading {
                processed: 0,
                total: 0
            }),
            "reading 0 of 0 prompt tokens"
        );
    }

    #[test]
    fn compaction_fraction_is_clamped_and_rounded_to_a_percentage() {
        assert_eq!(
            subagent_activity(&SubagentActivity::Compacting { fraction: 0.456 }),
            "summarizing its context 46%"
        );
        assert_eq!(
            subagent_activity(&SubagentActivity::Compacting { fraction: -1.0 }),
            "summarizing its context 0%"
        );
        assert_eq!(
            subagent_activity(&SubagentActivity::Compacting { fraction: 7.0 }),
            "summarizing its context 100%"
        );
    }

    #[test]
    fn subagent_round_distinguishes_starting_rounds_and_reporting() {
        assert_eq!(subagent_round(0, false), "starting");
        assert_eq!(subagent_round(3, false), "round 3");
        assert_eq!(subagent_round(0, true), "writing its report");
        assert_eq!(subagent_round(9, true), "writing its report");
    }

    #[test]
    fn focus_line_names_the_step_and_what_is_paused() {
        assert_eq!(
            edit_focus_line(16, (9, 12), true),
            "16 actions on todo step 9 of 12 without a file change · search paused until it edits"
        );
        assert_eq!(
            edit_focus_line(32, (1, 1), false),
            "32 actions on todo step 1 of 1 without a file change · only edits, shell and plan updates allowed"
        );
    }

    #[test]
    fn nudge_line_reports_repeats_and_where_the_model_was_sent() {
        assert_eq!(
            nudge_line(12, Some((2, 5)), 0, true),
            "12 actions without new evidence or a file change · nudged back to todo step 2 of 5"
        );
        assert_eq!(
            nudge_line(12, None, 1, true),
            "12 actions without new evidence or a file change · 1 repeated read · no plan yet, nudged to write a todo list"
        );
        assert_eq!(
            nudge_line(24, None, 3, false),
            "24 actions without new evidence or a file change · 3 repeated reads · nudged to act on what it found"
        );
    }
}
