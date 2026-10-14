use super::guard::*;
use super::*;
use builder_core::{
    protocol::{Function, ToolCall},
    store::ToolOutcome,
};

#[test]
fn subagent_capacity_respects_server_and_local_limits() {
    let mut profile = Profile::default();
    assert_eq!(subagent_slots(&profile, Some(4)), 4);
    assert_eq!(subagent_slots(&profile, Some(1)), 1);
    assert_eq!(subagent_slots(&profile, None), 3);
    assert_eq!(subagent_slots(&profile, Some(32)), 8);
    profile.pipeline.subagent_parallel = 2;
    assert_eq!(subagent_slots(&profile, Some(4)), 2);
    assert_eq!(subagent_slots(&profile, None), 2);
    assert_eq!(subagent_slots(&profile, Some(1)), 1);
    profile.pipeline.parallel_tools = 1;
    assert_eq!(subagent_slots(&profile, Some(4)), 1);
}

#[test]
fn context_estimate_excludes_transcript_only_reasoning() {
    let plain = Message::text(Role::Assistant, "answer");
    let mut thinking = plain.clone();
    thinking.reasoning = Some("private reasoning ".repeat(10_000));
    assert_eq!(estimate_tokens(&[plain]), estimate_tokens(&[thinking]));
}

#[test]
fn history_estimate_counts_reasoning_only_when_it_is_sent() {
    let plain = Message::text(Role::Assistant, "answer");
    let mut thinking = plain.clone();
    thinking.reasoning = Some("private reasoning ".repeat(10_000));
    let history = [Message::text(Role::User, "go"), thinking];
    let mut profile = Profile::default();
    assert_eq!(
        estimate_history_tokens(&history, &profile),
        estimate_tokens(&history),
        "stripped by default, so it costs nothing"
    );
    profile.reasoning_history = builder_core::config::ReasoningHistory::Send;
    assert!(
        estimate_history_tokens(&history, &profile) >= estimate_tokens(&history) + 90_000,
        "sent back, it is part of the request and of the compaction threshold"
    );
}

#[test]
fn tool_phase_uses_typed_actions_and_outcomes_and_resets_on_user_input() {
    let mut call = Message::text(Role::Assistant, "model prose saying verify is ignored");
    call.tool_calls.push(ToolCall {
        id: "search".into(),
        kind: "function".into(),
        function: Function {
            name: "code_search".into(),
            arguments: serde_json::json!({"query":"shield"}).to_string(),
        },
    });
    let mut history = vec![Message::text(Role::User, "request"), call];
    let mut outcomes = std::collections::HashMap::from([("search".into(), ToolOutcome::Succeeded)]);
    assert_eq!(
        tool_phase(&history, &outcomes),
        builder_core::research::Phase::Diagnose
    );

    let mut edit = Message::text(Role::Assistant, "");
    edit.tool_calls.push(ToolCall {
        id: "edit".into(),
        kind: "function".into(),
        function: Function {
            name: "edit_file".into(),
            arguments: serde_json::json!({"path":"arena.rs","old":"a","new":"b"}).to_string(),
        },
    });
    history.push(edit);
    outcomes.insert("edit".into(), ToolOutcome::Changed);
    assert_eq!(
        tool_phase(&history, &outcomes),
        builder_core::research::Phase::Verify
    );
    history.push(Message::text(Role::User, "new request"));
    assert_eq!(
        tool_phase(&history, &outcomes),
        builder_core::research::Phase::Locate
    );
}

#[test]
fn failure_recovery_uses_outcomes_not_model_or_tool_wording() {
    let mut history = vec![Message::text(Role::User, "status")];
    let mut outcomes = std::collections::HashMap::new();
    for id in ["one", "two", "three"] {
        history.push(Message::tool(id, "Succeeded, everything is fine".into()));
        outcomes.insert(id.into(), ToolOutcome::Failed);
    }
    assert_eq!(failed_tool_streak(&history, &outcomes), 3);
    history.push(Message::tool(
        "source",
        "ERROR: an example from the source file".into(),
    ));
    outcomes.insert("source".into(), ToolOutcome::Succeeded);
    assert_eq!(failed_tool_streak(&history, &outcomes), 3);
    history.push(Message::tool("edit", "changed".into()));
    outcomes.insert("edit".into(), ToolOutcome::Changed);
    assert_eq!(failed_tool_streak(&history, &outcomes), 0);
    history.push(Message::tool("four", "failed".into()));
    outcomes.insert("four".into(), ToolOutcome::Failed);
    assert_eq!(failed_tool_streak(&history, &outcomes), 1);
    history.push(Message::text(Role::User, "new task"));
    assert_eq!(failed_tool_streak(&history, &outcomes), 0);
}

#[test]
fn no_progress_streak_counts_every_tool_and_resets_only_for_change_or_instruction() {
    let mut history = vec![Message::text(Role::User, "inspect")];
    let mut outcomes = std::collections::HashMap::new();
    let append_result = |history: &mut Vec<Message>, name: &str, result: &str| {
        let id = history.len().to_string();
        let mut message = Message::text(Role::Assistant, "");
        message.tool_calls.push(ToolCall {
                id: id.clone(),
                kind: "function".into(),
                function: Function {
                    name: name.into(),
                    arguments: serde_json::json!({"path":"rate.ts", "query":"rate", "content":"new", "old":"old", "new":"new", "command":"echo rate"}).to_string(),
                },
            });
        history.push(message);
        history.push(Message::tool(&id, result.into()));
        id
    };
    for _ in 0..12 {
        append_result(&mut history, "read_file", "ERROR: range too large");
    }
    for (name, result) in [
        ("edit_file", "DENIED: not authorized"),
        ("write_file", "ERROR: Execution uncertain"),
        ("write_file", "UNCHANGED: already matches"),
        ("shell", "file contents"),
    ] {
        append_result(&mut history, name, result);
    }
    assert_eq!(no_progress_streak(&history, &outcomes), 16);
    let repetitions = tool_guard_state(&history, &outcomes).repetitions;
    assert_eq!(
        repetitions
            .get(
                &guarded_call(
                    &history
                        .iter()
                        .rev()
                        .find(|message| !message.tool_calls.is_empty())
                        .unwrap()
                        .tool_calls[0],
                )
                .unwrap()
                .signature,
            )
            .unwrap()
            .count,
        1
    );
    for _ in 0..3 {
        append_result(&mut history, "shell", "same result");
    }
    let repeated_shell = history
        .iter()
        .rev()
        .find(|message| !message.tool_calls.is_empty())
        .unwrap();
    assert_eq!(
        tool_guard_state(&history, &outcomes).repetitions[&guarded_call(
            &repeated_shell.tool_calls[0]
        )
        .unwrap()
        .signature]
            .count,
        4
    );
    let changed = append_result(
        &mut history,
        "edit_file",
        "ERROR: this is arbitrary tool text, not status",
    );
    outcomes.insert(changed, ToolOutcome::Changed);
    assert_eq!(no_progress_streak(&history, &outcomes), 0);
    assert!(tool_guard_state(&history, &outcomes).repetitions.is_empty());
    append_result(&mut history, "search", "one match");
    assert_eq!(no_progress_streak(&history, &outcomes), 1);
    history.push(Message::text(Role::User, "new instruction"));
    assert_eq!(no_progress_streak(&history, &outcomes), 0);
    assert!(tool_guard_state(&history, &outcomes).repetitions.is_empty());
}

#[test]
fn unique_successful_inspection_evidence_resets_stagnation() {
    let mut history = vec![Message::text(Role::User, "inspect")];
    let mut outcomes = std::collections::HashMap::new();
    let append = |history: &mut Vec<Message>,
                  outcomes: &mut std::collections::HashMap<String, ToolOutcome>,
                  id: &str,
                  path: &str,
                  result: &str,
                  outcome| {
        let mut assistant = Message::text(Role::Assistant, "");
        assistant.tool_calls.push(ToolCall {
            id: id.into(),
            kind: "function".into(),
            function: Function {
                name: "read_file".into(),
                arguments: serde_json::json!({"path":path}).to_string(),
            },
        });
        history.push(assistant);
        history.push(Message::tool(id, result.into()));
        outcomes.insert(id.into(), outcome);
    };

    for index in 0..12 {
        append(
            &mut history,
            &mut outcomes,
            &format!("read-{index}"),
            &format!("file-{index}.rs"),
            &format!("source {index}"),
            ToolOutcome::Succeeded,
        );
    }
    assert_eq!(no_progress_streak(&history, &outcomes), 0);

    append(
        &mut history,
        &mut outcomes,
        "repeat",
        "file-11.rs",
        "source 11",
        ToolOutcome::Succeeded,
    );
    assert_eq!(no_progress_streak(&history, &outcomes), 1);

    append(
        &mut history,
        &mut outcomes,
        "new-result",
        "file-11.rs",
        "updated source 11",
        ToolOutcome::Succeeded,
    );
    assert_eq!(no_progress_streak(&history, &outcomes), 0);

    append(
        &mut history,
        &mut outcomes,
        "failed",
        "file-12.rs",
        "ERROR: missing",
        ToolOutcome::Failed,
    );
    assert_eq!(no_progress_streak(&history, &outcomes), 1);

    append(
        &mut history,
        &mut outcomes,
        "empty-success",
        "another-file.rs",
        "   ",
        ToolOutcome::Succeeded,
    );
    assert_eq!(no_progress_streak(&history, &outcomes), 2);
}

#[test]
fn tool_signature_normalizes_json_object_order_but_not_action_changes() {
    let call = |arguments: &str| ToolCall {
        id: "ignored".into(),
        kind: "function".into(),
        function: Function {
            name: "shell".into(),
            arguments: arguments.into(),
        },
    };
    assert_eq!(
        tool_signature(&call(r#"{"command":"printf stable","timeout_secs":5}"#)),
        tool_signature(&call(r#"{"timeout_secs":5,"command":"printf stable"}"#))
    );
    assert_eq!(
        tool_signature(&call(
            r#"{"request":{"operation":"history_read","seq":2,"offset":0}}"#
        )),
        tool_signature(&call(
            r#"{"request":{"offset":0,"seq":2,"operation":"history_read"}}"#
        ))
    );
    assert_ne!(
        tool_signature(&call(r#"{"command":"printf stable","timeout_secs":5}"#)),
        tool_signature(&call(r#"{"command":"printf changed","timeout_secs":5}"#))
    );
}

#[test]
fn denied_execution_guard_survives_unrelated_change_until_new_instruction() {
    let call = |id: &str, name: &str, arguments: serde_json::Value| ToolCall {
        id: id.into(),
        kind: "function".into(),
        function: Function {
            name: name.into(),
            arguments: arguments.to_string(),
        },
    };
    let denied = call("denied", "shell", serde_json::json!({"command":"make"}));
    let changed = call(
        "changed",
        "edit_file",
        serde_json::json!({"path":"a", "old":"x", "new":"y"}),
    );
    let replay = call("replay", "shell", serde_json::json!({"command":"make"}));
    let mut denied_message = Message::text(Role::Assistant, "");
    denied_message.tool_calls.push(denied);
    let mut changed_message = Message::text(Role::Assistant, "");
    changed_message.tool_calls.push(changed);
    let mut history = vec![
        Message::text(Role::User, "work"),
        denied_message,
        Message::tool("denied", "DENIED".into()),
        changed_message,
        Message::tool("changed", "edited".into()),
    ];
    let outcomes = std::collections::HashMap::from([
        ("denied".into(), ToolOutcome::Denied),
        ("changed".into(), ToolOutcome::Changed),
    ]);
    let state = tool_guard_state(&history, &outcomes);
    assert!(state.repetitions.is_empty());
    assert!(matches!(
        tool_guard_violation([&replay], &state, 3),
        Some(ToolGuardViolation::Blocked {
            outcome: ToolOutcome::Denied,
            ..
        })
    ));
    history.push(Message::text(Role::User, "run it now"));
    let state = tool_guard_state(&history, &outcomes);
    assert!(state.blocked.is_empty());
    assert!(tool_guard_violation([&replay], &state, 3).is_none());
}

#[test]
fn uncertain_internal_memory_mutations_cannot_replay_under_new_ids() {
    let call = |id: &str| ToolCall {
        id: id.into(),
        kind: "function".into(),
        function: Function {
            name: "memory_upsert".into(),
            arguments: serde_json::json!({
                "key":"finding",
                "text":"source-backed note",
                "expected_revision":0,
                "evidence_call_ids":[]
            })
            .to_string(),
        },
    };
    let replay = call("replacement");
    let mut assistant = Message::text(Role::Assistant, "");
    assistant.tool_calls.push(call("uncertain"));
    let history = vec![
        Message::text(Role::User, "work"),
        assistant,
        Message::tool("uncertain", "ERROR: outcome unknown".into()),
    ];
    let outcomes = std::collections::HashMap::from([("uncertain".into(), ToolOutcome::Uncertain)]);
    let state = tool_guard_state(&history, &outcomes);
    assert!(matches!(
        tool_guard_violation([&replay], &state, 3),
        Some(ToolGuardViolation::Blocked {
            outcome: ToolOutcome::Uncertain,
            ..
        })
    ));
}

// ---------------------------------------------------------------------------
// Step focus, read log and the prompts the guards produce.
// ---------------------------------------------------------------------------

use super::progress::{
    Focus, FocusStage, Nudge, ReadLog, edit_focus, planning_reminder, progress_guidance,
    progress_nudge, todo_continuation, todo_packet,
};
use builder_core::research::Phase;
use builder_core::todo::{Item, List, Status};
use serde_json::{Value, json};
use std::collections::HashMap;

type Outcomes = HashMap<String, ToolOutcome>;

fn typed_call(id: &str, name: &str, arguments: Value) -> ToolCall {
    ToolCall {
        id: id.into(),
        kind: "function".into(),
        function: Function {
            name: name.into(),
            arguments: arguments.to_string(),
        },
    }
}

/// Records one call and its result, as the journal would.
fn exchange(
    history: &mut Vec<Message>,
    outcomes: &mut Outcomes,
    id: &str,
    name: &str,
    arguments: Value,
    result: &str,
    outcome: Option<ToolOutcome>,
) {
    let mut assistant = Message::text(Role::Assistant, "");
    assistant.tool_calls.push(typed_call(id, name, arguments));
    history.push(assistant);
    history.push(Message::tool(id, result.into()));
    if let Some(outcome) = outcome {
        outcomes.insert(id.into(), outcome);
    }
}

fn todo_arguments(items: &[(&str, &str)]) -> Value {
    json!({
        "todos": items
            .iter()
            .map(|(content, status)| json!({"content": content, "status": status}))
            .collect::<Vec<_>>()
    })
}

fn todo_list(items: &[(&str, Status)]) -> List {
    List {
        items: items
            .iter()
            .map(|(content, status)| Item {
                content: (*content).into(),
                status: *status,
            })
            .collect(),
    }
}

fn read(path: &str) -> Value {
    json!({ "path": path })
}

#[test]
fn step_focus_counts_calls_on_the_current_item_until_the_plan_or_a_file_moves() {
    let mut history = vec![Message::text(Role::User, "refactor the parser")];
    let mut outcomes = Outcomes::new();
    let ok = Some(ToolOutcome::Succeeded);
    let focus = |history: &[Message], outcomes: &Outcomes| step_focus(history, outcomes);
    let calls = |history: &[Message], outcomes: &Outcomes| focus(history, outcomes).calls;

    exchange(
        &mut history,
        &mut outcomes,
        "r1",
        "read_file",
        read("a.rs"),
        "fn a",
        ok,
    );
    exchange(
        &mut history,
        &mut outcomes,
        "r2",
        "read_file",
        read("b.rs"),
        "fn b",
        ok,
    );
    assert_eq!(
        calls(&history, &outcomes),
        2,
        "without a plan every call counts"
    );

    let first_plan = todo_arguments(&[("edit a", "in_progress"), ("edit b", "pending")]);
    exchange(
        &mut history,
        &mut outcomes,
        "p1",
        "todo_write",
        first_plan.clone(),
        "saved",
        ok,
    );
    assert_eq!(
        calls(&history, &outcomes),
        0,
        "the first plan starts the count"
    );

    exchange(
        &mut history,
        &mut outcomes,
        "r3",
        "read_file",
        read("c.rs"),
        "fn c",
        ok,
    );
    exchange(
        &mut history,
        &mut outcomes,
        "s1",
        "search",
        json!({"query":"a"}),
        "hit",
        ok,
    );
    exchange(
        &mut history,
        &mut outcomes,
        "sh1",
        "shell",
        json!({"command":"ls"}),
        "a.rs",
        ok,
    );
    assert_eq!(calls(&history, &outcomes), 3);

    history.push(Message::text(Role::User, "continue"));
    assert_eq!(
        calls(&history, &outcomes),
        3,
        "continue asks for the same work"
    );
    exchange(
        &mut history,
        &mut outcomes,
        "r4",
        "read_file",
        read("a.rs"),
        "fn a",
        ok,
    );
    assert_eq!(calls(&history, &outcomes), 4);

    exchange(
        &mut history,
        &mut outcomes,
        "p2",
        "todo_write",
        first_plan.clone(),
        "saved",
        ok,
    );
    assert_eq!(
        calls(&history, &outcomes),
        5,
        "rewriting the same plan is not progress"
    );
    let invalid = todo_arguments(&[("edit a", "in_progress"), ("edit b", "in_progress")]);
    exchange(
        &mut history,
        &mut outcomes,
        "p3",
        "todo_write",
        invalid,
        "saved",
        ok,
    );
    assert_eq!(
        calls(&history, &outcomes),
        6,
        "an invalid list is not a plan"
    );
    let advanced = todo_arguments(&[("edit a", "completed"), ("edit b", "in_progress")]);
    exchange(
        &mut history,
        &mut outcomes,
        "p4",
        "todo_write",
        advanced.clone(),
        "ERROR",
        Some(ToolOutcome::Failed),
    );
    assert_eq!(
        focus(&history, &outcomes),
        StepFocus {
            calls: 7,
            edit_failed: false
        },
        "a failed plan write moves nothing and is not a failed edit"
    );

    exchange(
        &mut history,
        &mut outcomes,
        "p5",
        "todo_write",
        advanced,
        "saved",
        ok,
    );
    assert_eq!(
        calls(&history, &outcomes),
        0,
        "completing an item restarts the count"
    );
    exchange(
        &mut history,
        &mut outcomes,
        "r5",
        "read_file",
        read("d.rs"),
        "fn d",
        ok,
    );
    assert_eq!(calls(&history, &outcomes), 1);

    let redirected = todo_arguments(&[
        ("edit a", "completed"),
        ("edit c", "in_progress"),
        ("edit b", "pending"),
    ]);
    exchange(
        &mut history,
        &mut outcomes,
        "p6",
        "todo_write",
        redirected,
        "saved",
        ok,
    );
    assert_eq!(
        calls(&history, &outcomes),
        0,
        "a new current item restarts the count"
    );
    exchange(
        &mut history,
        &mut outcomes,
        "r6",
        "read_file",
        read("e.rs"),
        "fn e",
        ok,
    );
    assert_eq!(calls(&history, &outcomes), 1);

    let edit = json!({"path":"a.rs","old":"x","new":"y"});
    exchange(
        &mut history,
        &mut outcomes,
        "e1",
        "edit_file",
        edit.clone(),
        "edited",
        Some(ToolOutcome::Changed),
    );
    assert_eq!(
        calls(&history, &outcomes),
        0,
        "a file change restarts the count"
    );

    exchange(
        &mut history,
        &mut outcomes,
        "e2",
        "edit_file",
        edit.clone(),
        "ERROR: no match",
        Some(ToolOutcome::Failed),
    );
    assert_eq!(
        focus(&history, &outcomes),
        StepFocus {
            calls: 1,
            edit_failed: true
        }
    );
    let multi = json!({"path":"a.rs","edits":[{"old":"x","new":"y"}]});
    exchange(
        &mut history,
        &mut outcomes,
        "e3",
        "multi_edit",
        multi,
        "ERROR",
        Some(ToolOutcome::Failed),
    );
    let write = json!({"path":"a.rs","content":"z"});
    exchange(
        &mut history,
        &mut outcomes,
        "e4",
        "write_file",
        write,
        "DENIED",
        Some(ToolOutcome::Failed),
    );
    assert_eq!(
        focus(&history, &outcomes),
        StepFocus {
            calls: 3,
            edit_failed: true
        }
    );
    exchange(
        &mut history,
        &mut outcomes,
        "r7",
        "read_file",
        read("a.rs"),
        "fn a",
        ok,
    );
    assert_eq!(
        focus(&history, &outcomes),
        StepFocus {
            calls: 4,
            edit_failed: false
        },
        "the read that follows clears the failed-edit flag"
    );
    exchange(
        &mut history,
        &mut outcomes,
        "sh2",
        "shell",
        json!({"command":"make"}),
        "ERROR",
        Some(ToolOutcome::Failed),
    );
    assert_eq!(
        focus(&history, &outcomes),
        StepFocus {
            calls: 5,
            edit_failed: false
        },
        "a failed command is not a failed edit"
    );
    history.push(Message::tool("ghost", "orphaned result".into()));
    assert_eq!(
        calls(&history, &outcomes),
        5,
        "a result without its call is ignored"
    );
}

#[test]
fn focus_stages_offer_edits_shell_and_plan_updates_and_reads_only_while_reading() {
    for tool in [
        "write_file",
        "edit_file",
        "multi_edit",
        "shell",
        "todo_write",
    ] {
        assert!(FocusStage::Read.offers(tool), "{tool}");
        assert!(FocusStage::Edit.offers(tool), "{tool}");
    }
    assert!(FocusStage::Read.offers("read_file"));
    assert!(!FocusStage::Edit.offers("read_file"));
    for tool in [
        "search",
        "list_files",
        "code_search",
        "subagent",
        "research",
        "memory_search",
    ] {
        assert!(!FocusStage::Read.offers(tool), "{tool}");
        assert!(!FocusStage::Edit.offers(tool), "{tool}");
    }
    assert!(FocusStage::Read < FocusStage::Edit);
}

#[test]
fn read_log_counts_successful_reads_since_the_last_change_and_marks_visible_results() {
    let mut history = vec![Message::text(Role::User, "inspect")];
    let mut outcomes = Outcomes::new();
    let ok = Some(ToolOutcome::Succeeded);
    exchange(
        &mut history,
        &mut outcomes,
        "r1",
        "read_file",
        read("a.rs"),
        "fn a",
        ok,
    );
    exchange(
        &mut history,
        &mut outcomes,
        "r2",
        "read_file",
        read("./a.rs"),
        "fn a",
        ok,
    );
    exchange(
        &mut history,
        &mut outcomes,
        "r3",
        "read_file",
        read("b.rs"),
        "fn b",
        ok,
    );
    exchange(
        &mut history,
        &mut outcomes,
        "r4",
        "read_file",
        read("c.rs"),
        "ERROR",
        Some(ToolOutcome::Failed),
    );
    exchange(
        &mut history,
        &mut outcomes,
        "r5",
        "read_file",
        read("d.rs"),
        "DENIED",
        Some(ToolOutcome::Denied),
    );
    exchange(
        &mut history,
        &mut outcomes,
        "s1",
        "search",
        json!({"query":"q"}),
        "hit",
        ok,
    );
    exchange(
        &mut history,
        &mut outcomes,
        "r6",
        "read_file",
        read("e.rs"),
        "fn e",
        None,
    );

    let active = &history[history.len() - 2..];
    let log = ReadLog::new(&history, &outcomes, active);
    assert_eq!(log.repeats(), 1);
    let text = planning_reminder(5, 12, &log);
    assert!(
        text.contains(" Since the last file change you have made 4 successful reads of 3 files, 1 of them a repeat: a.rs ×2, b.rs, e.rs (latest result still in context)."),
        "{text}"
    );
    assert!(
        text.contains("5 calls without new inspection evidence"),
        "{text}"
    );
    assert!(
        text.contains("At 12 calls without fresh evidence"),
        "{text}"
    );

    let edit = json!({"path":"a.rs","old":"x","new":"y"});
    exchange(
        &mut history,
        &mut outcomes,
        "e1",
        "edit_file",
        edit,
        "edited",
        Some(ToolOutcome::Changed),
    );
    let log = ReadLog::new(&history, &outcomes, &history);
    assert_eq!(log.repeats(), 0);
    assert!(!planning_reminder(1, 12, &log).contains("Since the last file change"));

    for id in ["r7", "r8", "r9"] {
        exchange(
            &mut history,
            &mut outcomes,
            id,
            "read_file",
            read("f.rs"),
            "fn f",
            ok,
        );
    }
    let log = ReadLog::new(&history, &outcomes, &history);
    assert_eq!(log.repeats(), 2);
    assert!(
        planning_reminder(3, 12, &log).contains(
            "3 successful reads of 1 file, 2 of them repeats: f.rs ×3 (latest result still in context)."
        ),
        "{}",
        planning_reminder(3, 12, &log)
    );
}

#[test]
fn read_log_summary_lists_the_eight_most_read_files_and_counts_the_rest() {
    let mut history = vec![Message::text(Role::User, "inspect")];
    let mut outcomes = Outcomes::new();
    for index in 0..11 {
        let id = format!("r{index}");
        exchange(
            &mut history,
            &mut outcomes,
            &id,
            "read_file",
            read(&format!("file-{index}.rs")),
            "src",
            Some(ToolOutcome::Succeeded),
        );
    }
    for id in ["again-1", "again-2"] {
        exchange(
            &mut history,
            &mut outcomes,
            id,
            "read_file",
            read("file-0.rs"),
            "src",
            Some(ToolOutcome::Succeeded),
        );
    }
    let log = ReadLog::new(&history, &outcomes, &[]);
    assert_eq!(log.repeats(), 2);
    let text = planning_reminder(13, 12, &log);
    assert!(
        text.contains(
            "13 successful reads of 11 files, 2 of them repeats: file-0.rs ×3, file-1.rs, "
        ),
        "{text}"
    );
    assert!(text.contains("file-7.rs, and 3 more."), "{text}");
    assert!(!text.contains("file-8.rs"), "{text}");
    assert!(!text.contains("still in context"), "{text}");
}

fn empty_log() -> ReadLog {
    ReadLog::new(&[], &Outcomes::new(), &[])
}

#[test]
fn progress_nudge_points_at_the_current_step_when_edits_are_allowed() {
    let list = todo_list(&[
        ("read parser", Status::Completed),
        ("fix parser", Status::InProgress),
        ("run tests", Status::Pending),
    ]);
    let reads = empty_log();
    let text = progress_nudge(&Nudge {
        calls: 12,
        check: 12,
        todos: Some(&list),
        planning: true,
        can_edit: true,
        reads: &reads,
    });
    assert!(
        text.starts_with("[Builder runtime continuation, not a new user request]"),
        "{text}"
    );
    assert!(
        text.contains("Progress check: 12 calls without new inspection evidence"),
        "{text}"
    );
    assert!(
        text.contains("make the change for todo item 2 of 3 (fix parser) now"),
        "{text}"
    );
    assert!(
        text.ends_with("Do not use shell as a substitute for the paused search."),
        "{text}"
    );
    assert!(!text.contains("This check has now continued"), "{text}");
}

#[test]
fn progress_nudge_asks_for_a_plan_and_gets_firmer_at_twice_the_check() {
    let reads = empty_log();
    let nudge = |calls, todos| {
        progress_nudge(&Nudge {
            calls,
            check: 12,
            todos,
            planning: true,
            can_edit: true,
            reads: &reads,
        })
    };
    let text = nudge(12, None);
    assert!(
        text.contains("call todo_write now with the ordered steps"),
        "{text}"
    );
    assert!(!text.contains("continued for"), "{text}");
    assert!(!nudge(23, None).contains("continued for"));
    let firmer = nudge(24, None);
    assert!(
        firmer.contains("This check has now continued for 12 calls without a plan"),
        "{firmer}"
    );
    let finished = todo_list(&[("done", Status::Completed)]);
    assert!(
        nudge(12, Some(&finished)).contains("call todo_write now"),
        "a finished list is no plan to follow"
    );
}

#[test]
fn progress_nudge_without_planning_or_edits_recommends_the_smallest_change_or_an_answer() {
    let reads = empty_log();
    let list = todo_list(&[("fix parser", Status::InProgress)]);
    let nudge = |planning, can_edit, todos| {
        progress_nudge(&Nudge {
            calls: 30,
            check: 12,
            todos,
            planning,
            can_edit,
            reads: &reads,
        })
    };
    let text = nudge(false, true, None);
    assert!(
        text.contains("make the smallest justified change now"),
        "{text}"
    );
    assert!(
        !text.contains("continued for"),
        "the firmer paragraph needs planning"
    );
    let text = nudge(false, false, None);
    assert!(text.contains("write the answer now"), "{text}");
    let text = nudge(false, false, Some(&list));
    assert!(
        text.contains("write the answer now"),
        "a plan needs edits to be worked: {text}"
    );
}

#[test]
fn edit_focus_text_follows_the_stage_and_a_failed_edit() {
    let reads = empty_log();
    let text = |stage, edit_failed| {
        edit_focus(&Focus {
            stage,
            calls: 16,
            step: (2, 3),
            item: "fix parser",
            edit_failed,
            reads: &reads,
        })
    };
    let read = text(FocusStage::Read, false);
    assert!(
        read.contains(
            "Todo item 2 of 3 (fix parser) has taken 16 tool calls without a file change."
        ),
        "{read}"
    );
    assert!(
        read.contains("Search and subagents are paused until a file changes."),
        "{read}"
    );
    assert_eq!(
        read,
        text(FocusStage::Read, true),
        "a failed edit changes nothing while reads are offered"
    );
    let edit = text(FocusStage::Edit, false);
    assert!(edit.contains("Only edits, todo_write and shell are available until a file changes. Make the next real change"), "{edit}");
    assert!(!edit.contains("read_file is back"), "{edit}");
    let rebuild = text(FocusStage::Edit, true);
    assert!(
        rebuild.contains("except that your last edit failed: read_file is back for this response"),
        "{rebuild}"
    );
    for variant in [&read, &edit, &rebuild] {
        assert!(variant.starts_with("[Builder runtime continuation, not a new user request]"));
        assert!(
            variant.contains("Never make a placeholder or unrelated edit to get tools back."),
            "{variant}"
        );
    }
}

#[test]
fn todo_packet_and_continuation_follow_the_current_item_and_the_conclusion_round() {
    let list = todo_list(&[
        ("read parser", Status::Completed),
        ("fix parser", Status::InProgress),
        ("run tests", Status::Pending),
    ]);
    let packet = todo_packet(&list, false);
    assert_eq!(packet.role, Role::System);
    let text = packet.content.unwrap();
    assert!(
        text.contains("1 of 3 items completed. Current item: 2. fix parser Work on that item now"),
        "{text}"
    );
    assert!(
        text.ends_with("[x] 1. read parser\n[>] 2. fix parser\n[ ] 3. run tests"),
        "{text}"
    );
    assert_eq!(
        todo_continuation(&list, false),
        " Your todo list is active: continue with item 2 (fix parser) instead of restarting discovery."
    );

    let finished = todo_list(&[("read parser", Status::Completed)]);
    let text = todo_packet(&finished, false).content.unwrap();
    assert!(
        text.contains("1 of 1 items completed. All items are complete."),
        "{text}"
    );
    assert_eq!(todo_continuation(&finished, false), "");

    let text = todo_packet(&list, true).content.unwrap();
    assert!(text.contains("Tools are unavailable for this request: report which items are complete and which remain."), "{text}");
    assert_eq!(
        todo_continuation(&list, true),
        " Include which todo items are complete and which remain."
    );
}

#[test]
fn progress_guidance_is_constant_until_the_enforced_conclusion() {
    let routine = progress_guidance(12, false, false);
    assert_eq!(routine.role, Role::System);
    assert_eq!(
        routine.content,
        progress_guidance(40, false, false).content,
        "the count stays out of the cacheable guidance"
    );
    let text = routine.content.unwrap();
    assert!(
        text.contains("the configured number of tool calls has completed"),
        "{text}"
    );
    assert!(
        text.contains("Broad list/search discovery is unavailable during recovery"),
        "{text}"
    );
    let forced = progress_guidance(17, true, false).content.unwrap();
    assert!(forced.contains("17 tool calls have completed"), "{forced}");
    assert!(
        forced.contains("This is the enforced conclusion round."),
        "{forced}"
    );
    let retry = progress_guidance(17, true, true).content.unwrap();
    assert!(
        retry.contains("second and final enforced conclusion request"),
        "{retry}"
    );
    assert!(
        !retry.contains("This is the enforced conclusion round."),
        "{retry}"
    );
}

#[test]
fn tool_call_structure_is_validated_before_dispatch() {
    let ids = |ids: &[&str]| {
        ids.iter()
            .map(|id| typed_call(id, "read_file", read("a.rs")))
            .collect::<Vec<_>>()
    };
    assert_eq!(invalid_tool_call_id(&ids(&[])), None);
    assert_eq!(invalid_tool_call_id(&ids(&["a", "b"])), None);
    assert_eq!(invalid_tool_call_id(&ids(&["a", ""])), Some(""));
    assert_eq!(invalid_tool_call_id(&ids(&["a", "b", "a"])), Some("a"));

    assert!(structurally_valid_tool_call(&typed_call(
        "1",
        "read_file",
        read("a.rs")
    )));
    let mut wrong_kind = typed_call("1", "read_file", read("a.rs"));
    wrong_kind.kind = "tool".into();
    assert!(!structurally_valid_tool_call(&wrong_kind));
    assert!(!structurally_valid_tool_call(&typed_call(
        "1",
        "",
        read("a.rs")
    )));
    assert!(!structurally_valid_tool_call(&typed_call(
        "1",
        "read_file",
        json!([])
    )));
    let mut not_json = typed_call("1", "read_file", json!({}));
    not_json.function.arguments = "{".into();
    assert!(!structurally_valid_tool_call(&not_json));
}

#[test]
fn guarded_calls_cover_mutations_executions_and_memory_writes_only() {
    for (name, arguments) in [
        ("read_file", read("a.rs")),
        ("list_files", json!({})),
        ("search", json!({"query":"q"})),
        ("code_search", json!({"query":"q"})),
        ("memory_get", json!({"key":"k"})),
        ("memory_search", json!({"query":"q"})),
        ("subagent", json!({"description":"d","prompt":"p"})),
        ("todo_write", json!({"todos":[]})),
        ("research", json!({"request":{"operation":"status"}})),
        ("unknown_tool", json!({})),
    ] {
        assert!(
            guarded_call(&typed_call("1", name, arguments)).is_none(),
            "{name}"
        );
    }
    for (name, arguments) in [
        ("write_file", json!({"path":"a","content":"b"})),
        ("edit_file", json!({"path":"a","old":"x","new":"y"})),
        (
            "multi_edit",
            json!({"path":"a","edits":[{"old":"x","new":"y"}]}),
        ),
        ("memory_forget", json!({"key":"k","expected_revision":1})),
        (
            "memory_upsert",
            json!({"key":"k","text":"t","expected_revision":0,"evidence_call_ids":[]}),
        ),
        (
            "research",
            json!({"request":{"operation":"verify","criterion":"c","command":"true","dependencies":[]}}),
        ),
    ] {
        let guarded = guarded_call(&typed_call("1", name, arguments)).unwrap();
        assert!(!guarded.signature.is_empty(), "{name}");
    }
    let plain = guarded_call(&typed_call("1", "shell", json!({"command":"make"}))).unwrap();
    let timed = guarded_call(&typed_call(
        "2",
        "shell",
        json!({"command":"make","timeout_secs":30}),
    ))
    .unwrap();
    assert_eq!(plain.signature, "shell\0\"make\"");
    assert_eq!(
        plain.signature, timed.signature,
        "a timeout does not change the command"
    );
    assert_ne!(
        plain.signature,
        guarded_call(&typed_call("3", "shell", json!({"command":"make test"})))
            .unwrap()
            .signature
    );
}

#[test]
fn identical_shell_commands_are_limited_within_a_batch_and_across_recorded_history() {
    let make = |id: &str| typed_call(id, "shell", json!({"command":"make"}));
    let batch = [make("1"), make("2"), make("3")];
    let fresh = ToolGuardState::default();
    match tool_guard_violation(&batch, &fresh, 2) {
        Some(ToolGuardViolation::Repeated { call, prior_count }) => {
            assert_eq!(call.id, "3");
            assert_eq!(prior_count, 2);
        }
        _ => panic!("the third identical command in one batch exceeds a limit of two"),
    }
    assert!(tool_guard_violation(&batch, &fresh, 3).is_none());

    let mut history = vec![Message::text(Role::User, "build")];
    let mut outcomes = Outcomes::new();
    for id in ["h1", "h2"] {
        exchange(
            &mut history,
            &mut outcomes,
            id,
            "shell",
            json!({"command":"make"}),
            "built",
            Some(ToolOutcome::Succeeded),
        );
    }
    let state = tool_guard_state(&history, &outcomes);
    assert_eq!(state.repetitions.len(), 1);
    assert_eq!(state.repetitions.values().next().unwrap().name, "shell");
    assert!(tool_guard_violation([&make("4")], &state, 3).is_none());
    match tool_guard_violation([&make("4"), &make("5")], &state, 3) {
        Some(ToolGuardViolation::Repeated { call, prior_count }) => {
            assert_eq!(call.id, "5");
            assert_eq!(prior_count, 3);
        }
        _ => panic!("recorded runs count toward the batch"),
    }
    let timed = typed_call("6", "shell", json!({"command":"make","timeout_secs":9}));
    assert!(matches!(
        tool_guard_violation([&timed], &state, 2),
        Some(ToolGuardViolation::Repeated { prior_count: 2, .. })
    ));
    let other = typed_call("7", "shell", json!({"command":"make test"}));
    assert!(tool_guard_violation([&other], &state, 2).is_none());

    let edit = |id: &str| typed_call(id, "edit_file", json!({"path":"a","old":"x","new":"y"}));
    let edits = [edit("a"), edit("b"), edit("c")];
    assert!(
        tool_guard_violation(&edits, &fresh, 1).is_none(),
        "edits are not repetition-limited"
    );
    let reads = [
        typed_call("r", "read_file", read("a.rs")),
        typed_call("r2", "read_file", read("a.rs")),
    ];
    assert!(
        tool_guard_violation(&reads, &fresh, 1).is_none(),
        "reads are not guarded"
    );
}

#[test]
fn blocked_outcomes_are_named_for_the_model() {
    assert_eq!(blocked_outcome_name(ToolOutcome::Denied), "denied");
    assert_eq!(blocked_outcome_name(ToolOutcome::Uncertain), "uncertain");
    for other in [
        ToolOutcome::Failed,
        ToolOutcome::Succeeded,
        ToolOutcome::Changed,
        ToolOutcome::Unknown,
    ] {
        assert_eq!(blocked_outcome_name(other), "blocked");
    }
}

#[test]
fn tool_phase_ranks_typed_activity_and_never_falls_back() {
    let phase_after = |calls: &[(&str, Value, Option<ToolOutcome>)]| {
        let mut history = vec![Message::text(Role::User, "work")];
        let mut outcomes = Outcomes::new();
        for (index, (name, arguments, outcome)) in calls.iter().enumerate() {
            exchange(
                &mut history,
                &mut outcomes,
                &index.to_string(),
                name,
                arguments.clone(),
                "result",
                *outcome,
            );
        }
        tool_phase(&history, &outcomes)
    };
    let ok = Some(ToolOutcome::Succeeded);
    let request = |operation: Value| json!({ "request": operation });
    assert_eq!(phase_after(&[]), Phase::Locate);
    assert_eq!(
        phase_after(&[("research", request(json!({"operation":"status"})), ok)]),
        Phase::Locate
    );
    assert_eq!(
        phase_after(&[(
            "research",
            request(json!({"operation":"plan","criteria":[]})),
            ok
        )]),
        Phase::Locate
    );
    for diagnose in [
        json!({"operation":"symbols","query":"q"}),
        json!({"operation":"observe","artifacts":[]}),
        json!({"operation":"analyze","question":"q","artifacts":[]}),
        json!({"operation":"semantic","server":"ra","args":[],"path":"a.rs","line":1,"character":1,"feature":"definition"}),
    ] {
        assert_eq!(
            phase_after(&[("research", request(diagnose.clone()), ok)]),
            Phase::Diagnose,
            "{diagnose}"
        );
    }
    for implement in [
        json!({"operation":"hypothesis","claim":"c","falsification":"f","evidence_ids":[]}),
        json!({"operation":"candidate_test","hypothesis_id":"h","criterion":"c","patches":[],"command":"true"}),
    ] {
        assert_eq!(
            phase_after(&[("research", request(implement.clone()), ok)]),
            Phase::Implement,
            "{implement}"
        );
    }
    for verify in [
        json!({"operation":"verify","criterion":"c","command":"true","dependencies":[]}),
        json!({"operation":"candidate_apply","candidate_id":"c"}),
    ] {
        assert_eq!(
            phase_after(&[("research", request(verify.clone()), ok)]),
            Phase::Verify,
            "{verify}"
        );
    }
    for (name, arguments) in [
        ("read_file", read("a.rs")),
        ("list_files", json!({})),
        ("code_search", json!({"query":"q"})),
        ("subagent", json!({"description":"d","prompt":"p"})),
    ] {
        assert_eq!(
            phase_after(&[(name, arguments.clone(), ok)]),
            Phase::Diagnose,
            "{name}"
        );
    }
    assert_eq!(
        phase_after(&[("read_file", read("a.rs"), Some(ToolOutcome::Failed))]),
        Phase::Locate
    );
    assert_eq!(
        phase_after(&[("read_file", read("a.rs"), None)]),
        Phase::Locate
    );
    assert_eq!(
        phase_after(&[("shell", json!({"command":"ls"}), ok)]),
        Phase::Locate
    );
    let write = json!({"path":"a","content":"b"});
    assert_eq!(
        phase_after(&[("write_file", write.clone(), ok)]),
        Phase::Locate,
        "an unchanged write proves nothing"
    );
    assert_eq!(
        phase_after(&[("write_file", write, Some(ToolOutcome::Changed))]),
        Phase::Verify
    );
    assert_eq!(
        phase_after(&[
            (
                "research",
                request(
                    json!({"operation":"verify","criterion":"c","command":"true","dependencies":[]})
                ),
                ok
            ),
            ("read_file", read("a.rs"), ok),
        ]),
        Phase::Verify,
        "a later read never lowers the phase"
    );
}
