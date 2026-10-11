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
