//! Typed decoding of model tool calls, their risk class, and what the user sees.
use builder_core::protocol::{Function, ToolCall};
use builder_tools::{Action, MAX_EDITS, Replacement, Risk, SUBAGENT_TOOL};
use serde_json::{Value, json};

fn call(name: &str, arguments: &str) -> ToolCall {
    ToolCall {
        id: "call-1".into(),
        kind: "function".into(),
        function: Function {
            name: name.into(),
            arguments: arguments.into(),
        },
    }
}

fn action(name: &str, arguments: Value) -> Action {
    Action::from_call(&call(name, &arguments.to_string())).unwrap()
}

fn research(request: Value) -> Action {
    action("research", json!({ "request": request }))
}

#[test]
fn every_tool_name_decodes_to_its_typed_action() {
    assert!(matches!(
        action("list_files", json!({})),
        Action::ListFiles { glob: None }
    ));
    assert!(matches!(
        action("list_files", json!({"glob":"*.rs"})),
        Action::ListFiles { glob: Some(glob) } if glob == "*.rs"
    ));
    assert!(matches!(
        action("read_file", json!({"path":"a.rs"})),
        Action::ReadFile { path, start_line: None, end_line: None } if path == "a.rs"
    ));
    assert!(matches!(
        action(
            "read_file",
            json!({"path":"a.rs","start_line":3,"end_line":9})
        ),
        Action::ReadFile {
            start_line: Some(3),
            end_line: Some(9),
            ..
        }
    ));
    assert!(matches!(
        action("search", json!({"query":"needle"})),
        Action::Search { query, glob: None } if query == "needle"
    ));
    assert!(matches!(
        action("code_search", json!({"query":"needle","limit":5})),
        Action::CodeSearch { limit: Some(5), .. }
    ));
    assert!(matches!(
        action("write_file", json!({"path":"a.rs","content":"x"})),
        Action::WriteFile { .. }
    ));
    assert!(matches!(
        action("edit_file", json!({"path":"a.rs","old":"a","new":"b"})),
        Action::EditFile { .. }
    ));
    match action(
        "multi_edit",
        json!({"path":"a.rs","edits":[{"old":"a","new":"b"},{"old":"c","new":"d","replace_all":true}]}),
    ) {
        Action::MultiEdit { path, edits } => {
            assert_eq!(path, "a.rs");
            assert_eq!(edits.len(), 2);
            assert!(!edits[0].replace_all);
            assert!(edits[1].replace_all);
        }
        other => panic!("{other:?}"),
    }
    assert!(matches!(
        action("memory_search", json!({"query":"q"})),
        Action::MemorySearch { .. }
    ));
    assert!(matches!(
        action("memory_get", json!({"key":"k"})),
        Action::MemoryGet { revision: None, .. }
    ));
    assert!(matches!(
        action("memory_get", json!({"key":"k","revision":3})),
        Action::MemoryGet {
            revision: Some(3),
            ..
        }
    ));
    assert!(matches!(
        action(
            "memory_upsert",
            json!({"key":"k","text":"t","expected_revision":0,"evidence_call_ids":["c1"]})
        ),
        Action::MemoryUpsert {
            expected_revision: 0,
            ..
        }
    ));
    assert!(matches!(
        action("memory_forget", json!({"key":"k","expected_revision":2})),
        Action::MemoryForget {
            expected_revision: 2,
            ..
        }
    ));
    match action(
        "todo_write",
        json!({"todos":[{"content":"step","status":"in_progress"}]}),
    ) {
        Action::TodoWrite { todos } => assert_eq!(todos.items.len(), 1),
        other => panic!("{other:?}"),
    }
    assert!(matches!(
        action(SUBAGENT_TOOL, json!({"description":"map","prompt":"find"})),
        Action::Subagent { .. }
    ));
    assert!(matches!(
        action("shell", json!({"command":"ls"})),
        Action::Shell {
            timeout_secs: None,
            ..
        }
    ));
    assert!(matches!(
        action("shell", json!({"command":"sleep 1","timeout_secs":5})),
        Action::Shell {
            timeout_secs: Some(5),
            ..
        }
    ));
    assert!(matches!(
        research(json!({"operation":"status"})),
        Action::Research { .. }
    ));
}

#[test]
fn malformed_calls_are_rejected_rather_than_guessed() {
    let bad = [
        ("read_file", "{not json"),
        ("read_file", "[]"),
        ("teleport", r#"{"path":"a.rs"}"#),
        ("edit_file", r#"{"path":"a.rs","old":"a"}"#),
        (
            "multi_edit",
            r#"{"path":"a.rs","edits":[{"old":"a","new":"b","extra":1}]}"#,
        ),
        ("multi_edit", r#"{"path":"a.rs","edits":[{"old":"a"}]}"#),
        ("shell", r#"{"command":"ls","timeout_secs":"soon"}"#),
        ("research", r#"{"request":{"operation":"teleport"}}"#),
    ];
    for (name, arguments) in bad {
        assert!(
            Action::from_call(&call(name, arguments)).is_err(),
            "{name} {arguments}"
        );
    }
}

#[test]
fn a_decoded_action_serializes_back_to_the_tool_call_shape() {
    let value =
        serde_json::to_value(action("read_file", json!({"path":"a.rs","start_line":2}))).unwrap();
    assert_eq!(value["name"], "read_file");
    assert_eq!(value["arguments"]["path"], "a.rs");
    assert_eq!(value["arguments"]["start_line"], 2);
    assert_eq!(value["arguments"]["end_line"], Value::Null);
}

#[test]
fn only_workspace_reads_count_as_inspection() {
    for (name, arguments) in [
        ("list_files", json!({})),
        ("read_file", json!({"path":"a.rs"})),
        ("search", json!({"query":"q"})),
    ] {
        assert!(action(name, arguments).is_inspection(), "{name}");
    }
    for (name, arguments) in [
        ("code_search", json!({"query":"q"})),
        ("subagent", json!({"description":"d","prompt":"p"})),
        ("shell", json!({"command":"ls"})),
        ("write_file", json!({"path":"a","content":"b"})),
        ("memory_search", json!({"query":"q"})),
        ("todo_write", json!({"todos":[]})),
        ("research", json!({"request":{"operation":"status"}})),
    ] {
        assert!(!action(name, arguments).is_inspection(), "{name}");
    }
}

#[test]
fn risk_follows_what_the_action_can_change() {
    for (name, arguments) in [
        ("list_files", json!({})),
        ("read_file", json!({"path":"a"})),
        ("search", json!({"query":"q"})),
        ("code_search", json!({"query":"q"})),
        ("memory_search", json!({"query":"q"})),
        ("memory_get", json!({"key":"k"})),
        (
            "memory_upsert",
            json!({"key":"k","text":"t","expected_revision":0,"evidence_call_ids":[]}),
        ),
        ("todo_write", json!({"todos":[]})),
        ("subagent", json!({"description":"d","prompt":"p"})),
    ] {
        assert_eq!(action(name, arguments).risk(), Risk::Read, "{name}");
    }
    for (name, arguments) in [
        ("write_file", json!({"path":"a","content":"b"})),
        ("edit_file", json!({"path":"a","old":"x","new":"y"})),
        (
            "multi_edit",
            json!({"path":"a","edits":[{"old":"x","new":"y"}]}),
        ),
        ("memory_forget", json!({"key":"k","expected_revision":1})),
    ] {
        assert_eq!(action(name, arguments).risk(), Risk::Write, "{name}");
    }
    assert_eq!(
        action("shell", json!({"command":"ls"})).risk(),
        Risk::Execute
    );
}

#[test]
fn research_risk_depends_on_the_operation() {
    for request in [
        json!({"operation":"status"}),
        json!({"operation":"plan","criteria":[]}),
        json!({"operation":"review","verification_ids":[]}),
        json!({"operation":"symbols","query":"q"}),
        json!({"operation":"observe","artifacts":[]}),
        json!({"operation":"analyze","question":"q","artifacts":[]}),
        json!({"operation":"hypothesis","claim":"c","falsification":"f","evidence_ids":[]}),
        json!({"operation":"history_search","query":"q","include_archived":false}),
        json!({"operation":"history_read","seq":2,"offset":0}),
    ] {
        assert_eq!(research(request.clone()).risk(), Risk::Read, "{request}");
    }
    for request in [
        json!({"operation":"candidate_apply","candidate_id":"c"}),
        json!({"operation":"retire","key":"k","reason":"r"}),
    ] {
        assert_eq!(research(request.clone()).risk(), Risk::Write, "{request}");
    }
    for request in [
        json!({"operation":"semantic","server":"rust-analyzer","args":[],"path":"a.rs","line":1,"character":1,"feature":"definition"}),
        json!({"operation":"verify","criterion":"c","command":"true","dependencies":[]}),
        json!({"operation":"candidate_test","hypothesis_id":"h","criterion":"c","patches":[],"command":"true"}),
    ] {
        assert_eq!(research(request.clone()).risk(), Risk::Execute, "{request}");
    }
}

#[test]
fn descriptions_show_the_user_what_each_action_will_do() {
    assert_eq!(
        action("list_files", json!({})).description(),
        "list files · **/*"
    );
    assert_eq!(
        action("list_files", json!({"glob":"src/**"})).description(),
        "list files · src/**"
    );
    assert_eq!(
        action("read_file", json!({"path":"a.rs","start_line":1})).description(),
        "read · a.rs"
    );
    assert_eq!(
        action("search", json!({"query":"needle","glob":"*.rs"})).description(),
        "search · needle"
    );
    assert_eq!(
        action("code_search", json!({"query":"needle"})).description(),
        "code index · needle"
    );
    assert_eq!(
        action("write_file", json!({"path":"a.rs","content":"héllo"})).description(),
        "write · a.rs (6 bytes)\nhéllo"
    );
    assert_eq!(
        action("edit_file", json!({"path":"a.rs","old":"a","new":"b"})).description(),
        "edit · a.rs\n--- old\na\n+++ new\nb"
    );
    assert_eq!(
        action(
            "multi_edit",
            json!({"path":"a.rs","edits":[{"old":"a","new":"b"},{"old":"c","new":"d","replace_all":true}]})
        )
        .description(),
        "edit · a.rs (2 edits)\n[1]\n--- old\na\n+++ new\nb\n[2] (every match)\n--- old\nc\n+++ new\nd"
    );
    assert_eq!(
        action("memory_search", json!({"query":"tabs"})).description(),
        "recall · tabs"
    );
    assert_eq!(
        action("memory_get", json!({"key":"style"})).description(),
        "memory · style"
    );
    assert_eq!(
        action(
            "memory_upsert",
            json!({"key":"style","text":"t","expected_revision":0,"evidence_call_ids":[]})
        )
        .description(),
        "remember finding · style"
    );
    assert_eq!(
        action(
            "memory_forget",
            json!({"key":"style","expected_revision":1})
        )
        .description(),
        "forget memory · style"
    );
    assert_eq!(
        action(
            "todo_write",
            json!({"todos":[{"content":"one","status":"completed"},{"content":"two","status":"in_progress"},{"content":" three ","status":"pending"}]})
        )
        .description(),
        "todo list\n[x] 1. one\n[>] 2. two\n[ ] 3. three"
    );
    assert_eq!(
        action(
            SUBAGENT_TOOL,
            json!({"description":"map DI","prompt":"find every container"})
        )
        .description(),
        "subagent · map DI\nfind every container"
    );
    assert_eq!(
        action("shell", json!({"command":"cargo test","timeout_secs":30})).description(),
        "shell · cargo test"
    );
    let text = research(json!({"operation":"status"})).description();
    let (prefix, body) = text.split_once(" · ").unwrap();
    assert_eq!(prefix, "research");
    assert_eq!(
        serde_json::from_str::<Value>(body).unwrap(),
        json!({"operation":"status"})
    );
}

#[test]
fn replacements_default_to_one_match_and_reject_unknown_fields() {
    let one: Replacement = serde_json::from_value(json!({"old":"a","new":"b"})).unwrap();
    assert_eq!(
        (one.old.as_str(), one.new.as_str(), one.replace_all),
        ("a", "b", false)
    );
    assert!(
        serde_json::from_value::<Replacement>(json!({"old":"a","new":"b","all":true})).is_err()
    );
    assert_eq!(MAX_EDITS, 50);
    assert_eq!(SUBAGENT_TOOL, "subagent");
}
