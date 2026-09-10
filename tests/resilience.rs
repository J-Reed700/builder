use axum::{Json, Router, extract::State, http::StatusCode, response::IntoResponse, routing::post};
use builder::agent::{Agent, ApprovalMode, pending};
use builder_core::protocol::Role;
use builder_core::{
    config::Profile,
    protocol::Message,
    store::{Store, ToolOutcome, ToolRunState},
};
use builder_provider::{Event, OpenAiCompatible, Provider, SseDecoder};
use builder_tools::Workspace;
use serde_json::{Value, json};
use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
};

type Responses = VecDeque<(StatusCode, String)>;
#[derive(Clone)]
struct Mock {
    replies: Arc<Mutex<Responses>>,
    requests: Arc<Mutex<Vec<Value>>>,
}
struct Server {
    profile: Profile,
    mock: Mock,
    task: tokio::task::JoinHandle<()>,
}
impl Drop for Server {
    fn drop(&mut self) {
        self.task.abort();
    }
}
async fn server(replies: Vec<(StatusCode, String)>) -> Server {
    let mock = Mock {
        replies: Arc::new(Mutex::new(replies.into())),
        requests: Default::default(),
    };
    let app = Router::new()
        .route(
            "/v1/chat/completions",
            post(
                |State(state): State<Mock>, Json(body): Json<Value>| async move {
                    state.requests.lock().unwrap().push(body);
                    let (status, body) = state.replies.lock().unwrap().pop_front().unwrap_or((
                        StatusCode::INTERNAL_SERVER_ERROR,
                        "unexpected request".into(),
                    ));
                    (
                        status,
                        [("content-type", "text/event-stream"), ("retry-after", "0")],
                        body,
                    )
                        .into_response()
                },
            ),
        )
        .with_state(mock.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let mut profile = Profile {
        base_url: format!("http://{}/v1", listener.local_addr().unwrap()),
        max_attempts: 3,
        ..Profile::default()
    };
    // Most resilience fixtures exercise the original compact 12 + 8 liveness
    // boundary. Product defaults allow up to 100 no-progress calls.
    profile.pipeline.progress_recovery_rounds = 8;
    let task = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    Server {
        profile,
        mock,
        task,
    }
}
fn delta(value: Value) -> String {
    format!(
        "data: {}\n\n",
        json!({"choices":[{"index":0,"delta":value,"finish_reason":null}]})
    )
}
fn finish(reason: &str) -> String {
    format!(
        "data: {}\n\ndata: [DONE]\n\n",
        json!({"choices":[{"index":0,"delta":{},"finish_reason":reason}]})
    )
}
fn text_reply(text: &str) -> (StatusCode, String) {
    (
        StatusCode::OK,
        format!("{}{}", delta(json!({"content":text})), finish("stop")),
    )
}

fn read_message(id: &str, args: Value) -> Message {
    serde_json::from_value(json!({
        "role":"assistant", "content":null,
        "tool_calls":[{"id":id,"type":"function","function":{
            "name":"read_file","arguments":args.to_string()
        }}]
    }))
    .unwrap()
}

fn shell_message(id: &str, command: &str) -> Message {
    serde_json::from_value(json!({
        "role":"assistant", "content":null,
        "tool_calls":[{"id":id,"type":"function","function":{
            "name":"shell","arguments":json!({"command":command}).to_string()
        }}]
    }))
    .unwrap()
}

fn seed_investigation(store: &mut Store, session: &str) {
    store
        .append(session, &Message::text(Role::User, "Fix the located rate"))
        .unwrap();
    for index in 0..12 {
        let id = format!("old_read_{index}");
        store
            .append(session, &read_message(&id, json!({"path":"rate.ts"})))
            .unwrap();
        store.claim_tool(session, &id).unwrap();
        store
            .complete_tool(session, &id, "    1  old rate")
            .unwrap();
    }
}

#[tokio::test]
async fn progress_check_preserves_large_context_and_clears_after_edit() {
    let mut write = tool_message();
    write.tool_calls[0].id = "progress_write".into();
    let server = server(vec![
        (
            StatusCode::OK,
            format!(
                "{}{}",
                delta(json!({"tool_calls":[{
                    "index":0,"id":"progress_write","type":"function",
                    "function": write.tool_calls[0].function
                }]})),
                finish("tool_calls")
            ),
        ),
        text_reply("Changed the rate."),
    ])
    .await;
    let home = tempfile::tempdir().unwrap();
    let mut store = Store::open(home.path()).unwrap();
    let session = store
        .create("progress", "local", home.path(), "system")
        .unwrap();
    seed_investigation(&mut store, &session);
    drop(store);
    let mut store = Store::open(home.path()).unwrap();
    let mut profile = server.profile.clone();
    profile.context_tokens = 163_840;
    let agent = Agent {
        memory: None,
        provider: OpenAiCompatible::new(server.profile.clone()).unwrap(),
        profile,
        workspace: Workspace::new(home.path()).unwrap(),
        session: session.clone(),
        approval: ApprovalMode::Trust,
        max_rounds: 4,
    };
    let mut notices = 0;
    agent
        .run(
            &mut store,
            &mut |e| {
                if matches!(
                    e,
                    builder::agent::AgentEvent::ExplorationRecovery { calls: 12 }
                ) {
                    notices += 1;
                }
            },
            &mut |_| panic!("auto already authorized"),
        )
        .await
        .unwrap();
    assert_eq!(notices, 1);
    assert_eq!(
        std::fs::read_to_string(home.path().join("result.txt")).unwrap(),
        "done"
    );
    let requests = server.mock.requests.lock().unwrap();
    assert_eq!(requests.len(), 2);
    assert!(requests[0]["tools"].is_array());
    let recovered = requests[0]["messages"].as_array().unwrap();
    assert!(recovered.iter().any(|m| m["role"] == "tool"));
    assert!(
        recovered
            .iter()
            .any(|m| m["role"] == "user" && m["content"] == "Fix the located rate")
    );
    assert!(
        recovered.last().unwrap()["content"]
            .as_str()
            .unwrap()
            .contains("Builder runtime continuation")
    );
    assert!(!store.history_messages(&session).unwrap().iter().any(|m| {
        m.content
            .as_deref()
            .unwrap_or("")
            .contains("Builder runtime continuation")
    }));
    assert!(store.archived_messages(&session).unwrap().is_empty());
    assert!(
        requests[0]["messages"][0]["content"]
            .as_str()
            .unwrap()
            .ends_with("\n\nsystem")
    );
    assert!(
        requests[0]["messages"][0]["content"]
            .as_str()
            .unwrap()
            .contains("Runtime progress check: 12")
    );
    assert!(
        !requests[1]["messages"]
            .to_string()
            .contains("Runtime progress check")
    );
    assert!(!store.history_messages(&session).unwrap().iter().any(|m| {
        m.content
            .as_deref()
            .unwrap_or("")
            .contains("Runtime progress check")
    }));
}

#[tokio::test]
async fn unique_shell_commands_consume_durable_liveness_budget_after_restart() {
    let server = server(vec![text_reply(
        "Answered from the existing command results without another tool.",
    )])
    .await;
    let home = tempfile::tempdir().unwrap();
    let mut store = Store::open(home.path()).unwrap();
    let session = store
        .create("shell liveness", "local", home.path(), "system")
        .unwrap();
    store
        .append(
            &session,
            &Message::text(Role::User, "Answer without spinning"),
        )
        .unwrap();
    for index in 0..12 {
        let id = format!("shell_history_{index}");
        let call = serde_json::from_value(json!({
            "role":"assistant",
            "content":null,
            "tool_calls":[{
                "id":id,
                "type":"function",
                "function":{
                    "name":"shell",
                    "arguments":json!({"command":format!("printf {index}")}).to_string()
                }
            }]
        }))
        .unwrap();
        store.append(&session, &call).unwrap();
        store.claim_tool(&session, &id).unwrap();
        store
            .complete_tool_with_outcome(
                &session,
                &id,
                "exit: exit status: 0",
                builder_core::store::ToolOutcome::Succeeded,
            )
            .unwrap();
    }
    drop(store);

    let mut store = Store::open(home.path()).unwrap();
    let agent = Agent {
        memory: None,
        provider: OpenAiCompatible::new(server.profile.clone()).unwrap(),
        profile: server.profile.clone(),
        workspace: Workspace::new(home.path()).unwrap(),
        session: session.clone(),
        approval: ApprovalMode::Trust,
        max_rounds: 3,
    };
    let mut recoveries = 0;
    agent
        .run(
            &mut store,
            &mut |event| {
                if matches!(
                    event,
                    builder::agent::AgentEvent::ExplorationRecovery { calls: 12 }
                ) {
                    recoveries += 1;
                }
            },
            &mut |_| panic!("no new tool should execute"),
        )
        .await
        .unwrap();
    assert_eq!(recoveries, 1);
    assert!(!pending(&store.messages(&session).unwrap()));
    let requests = server.mock.requests.lock().unwrap();
    assert_eq!(requests.len(), 1);
    assert!(requests[0]["tools"].is_array());
    assert!(
        requests[0]["messages"]
            .to_string()
            .contains("Runtime progress check: 12 tool calls")
    );
}

#[tokio::test]
async fn saved_unique_shell_batch_stops_at_durable_no_progress_execution_limit() {
    let server = server(vec![]).await;
    let home = tempfile::tempdir().unwrap();
    let mut store = Store::open(home.path()).unwrap();
    let session = store
        .create("durable execution bound", "local", home.path(), "system")
        .unwrap();
    seed_investigation(&mut store, &session);
    let calls = (0..16)
        .map(|index| {
            json!({
                "id":format!("bounded_shell_{index}"),
                "type":"function",
                "function":{
                    "name":"shell",
                    "arguments":json!({
                        "command":format!("printf '{index}\\n' >> bounded-marker")
                    }).to_string()
                }
            })
        })
        .collect::<Vec<_>>();
    let batch: Message = serde_json::from_value(json!({
        "role":"assistant", "content":null, "tool_calls":calls
    }))
    .unwrap();
    store.append(&session, &batch).unwrap();
    drop(store);

    let mut store = Store::open(home.path()).unwrap();
    let agent = Agent {
        memory: None,
        provider: OpenAiCompatible::new(server.profile.clone()).unwrap(),
        profile: server.profile.clone(),
        workspace: Workspace::new(home.path()).unwrap(),
        session: session.clone(),
        approval: ApprovalMode::Trust,
        max_rounds: 3,
    };
    let error = agent
        .run(&mut store, &mut |_| {}, &mut |_| false)
        .await
        .unwrap_err()
        .to_string();
    assert!(error.contains("configured limit of 20"), "{error}");
    assert!(error.contains("remains unclaimed"), "{error}");
    assert_eq!(
        std::fs::read_to_string(home.path().join("bounded-marker"))
            .unwrap()
            .lines()
            .count(),
        8
    );
    assert_eq!(store.tool_outcomes(&session).unwrap().len(), 20);
    for (index, call) in batch.tool_calls.iter().enumerate() {
        assert_eq!(
            store.tool_run_state(&session, &call.id).unwrap(),
            if index < 8 {
                ToolRunState::Finished
            } else {
                ToolRunState::Unclaimed
            }
        );
    }
    let retry_error = agent
        .run(&mut store, &mut |_| {}, &mut |_| false)
        .await
        .unwrap_err()
        .to_string();
    assert!(
        retry_error.contains("configured limit of 20"),
        "{retry_error}"
    );
    assert_eq!(store.tool_outcomes(&session).unwrap().len(), 20);
    assert!(server.mock.requests.lock().unwrap().is_empty());
}

#[tokio::test]
async fn ignored_progress_check_forces_a_tool_free_conclusion() {
    let replies = (0..7)
        .map(|i| {
            (
                StatusCode::OK,
                format!(
                    "{}{}",
                    delta(json!({"tool_calls":[{
                        "index":0,"id":format!("loop_{i}"),"type":"function",
                        "function": if i < 5 {
                            json!({"name":"read_file","arguments":"{\"path\":\"rate.ts\"}"})
                        } else {
                            json!({"name":"edit_file","arguments":json!({"path":"rate.ts","old":"missing exact text","new":"new rate"}).to_string()})
                        }
                    }]})),
                    finish("tool_calls")
                ),
            )
        })
        .chain([text_reply(
            "I could not complete the requested edit from the verified evidence.",
        )])
        .collect();
    let server = server(replies).await;
    let home = tempfile::tempdir().unwrap();
    std::fs::write(home.path().join("rate.ts"), "old rate").unwrap();
    let mut store = Store::open(home.path()).unwrap();
    let session = store
        .create("loop", "local", home.path(), "system")
        .unwrap();
    seed_investigation(&mut store, &session);
    let agent = Agent {
        memory: None,
        provider: OpenAiCompatible::new(server.profile.clone()).unwrap(),
        profile: server.profile.clone(),
        workspace: Workspace::new(home.path()).unwrap(),
        session: session.clone(),
        approval: ApprovalMode::Trust,
        max_rounds: 20,
    };
    agent
        .run(&mut store, &mut |_| {}, &mut |_| true)
        .await
        .unwrap();
    assert!(!pending(&store.messages(&session).unwrap()));
    assert_eq!(server.mock.requests.lock().unwrap().len(), 8);
    let requests = server.mock.requests.lock().unwrap();
    assert!(requests[7]["tools"].as_array().is_none_or(Vec::is_empty));
    assert!(
        requests[7]["messages"]
            .to_string()
            .contains("enforced conclusion round")
    );
    for request in &requests[..7] {
        let names = request["tools"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|tool| tool["function"]["name"].as_str())
            .collect::<Vec<_>>();
        assert!(!names.contains(&"list_files"), "{names:?}");
        assert!(!names.contains(&"search"), "{names:?}");
    }
}

#[tokio::test]
async fn tool_markup_at_progress_boundary_is_hidden_and_repaired_without_execution() {
    let server = server(vec![
        text_reply(
            "<tool_call>\n<function=search>\n<parameter=query>playerX</parameter>\n</function>\n</tool_call>",
        ),
        text_reply("I could not verify the requested collision change; no edit was completed."),
    ])
    .await;
    let home = tempfile::tempdir().unwrap();
    std::fs::write(home.path().join("arena.ts"), "export const arena = true;\n").unwrap();
    let mut store = Store::open(home.path()).unwrap();
    let session = store
        .create("raw conclusion", "local", home.path(), "system")
        .unwrap();
    seed_investigation(&mut store, &session);
    for index in 12..20 {
        let id = format!("more_read_{index}");
        store
            .append(&session, &read_message(&id, json!({"path":"arena.ts"})))
            .unwrap();
        store.claim_tool(&session, &id).unwrap();
        store
            .complete_tool(&session, &id, "    1  export const arena = true;")
            .unwrap();
    }
    let agent = Agent {
        memory: None,
        provider: OpenAiCompatible::new(server.profile.clone()).unwrap(),
        profile: server.profile.clone(),
        workspace: Workspace::new(home.path()).unwrap(),
        session: session.clone(),
        approval: ApprovalMode::Trust,
        max_rounds: 4,
    };
    let mut visible = String::new();
    agent
        .run(
            &mut store,
            &mut |event| {
                if let builder::agent::AgentEvent::Model(Event::Delta(text)) = event {
                    visible.push_str(&text);
                }
            },
            &mut |_| panic!("conclusion must not execute tools"),
        )
        .await
        .unwrap();

    assert_eq!(
        visible,
        "I could not verify the requested collision change; no edit was completed."
    );
    assert!(!visible.contains("<tool_call>"));
    assert_eq!(server.mock.requests.lock().unwrap().len(), 2);
    for request in server.mock.requests.lock().unwrap().iter() {
        assert!(request["tools"].as_array().is_none_or(Vec::is_empty));
    }
    assert_eq!(
        store
            .messages(&session)
            .unwrap()
            .last()
            .unwrap()
            .content
            .as_deref(),
        Some("I could not verify the requested collision change; no edit was completed.")
    );
}

#[tokio::test]
async fn repeated_shell_loop_is_rejected_before_fourth_execution_after_restart() {
    let replies = (0..4)
        .map(|index| {
            (
                StatusCode::OK,
                format!(
                    "{}{}",
                    delta(json!({"tool_calls":[{
                        "index":0,
                        "id":format!("same_shell_{index}"),
                        "type":"function",
                        "function":{
                            "name":"shell",
                            "arguments":"{\"command\":\"printf stable\"}"
                        }
                    }]})),
                    finish("tool_calls")
                ),
            )
        })
        .collect();
    let server = server(replies).await;
    let home = tempfile::tempdir().unwrap();
    let mut store = Store::open(home.path()).unwrap();
    let session = store
        .create("repeated shell", "local", home.path(), "system")
        .unwrap();
    let mut agent = Agent {
        memory: None,
        provider: OpenAiCompatible::new(server.profile.clone()).unwrap(),
        profile: server.profile.clone(),
        workspace: Workspace::new(home.path()).unwrap(),
        session: session.clone(),
        approval: ApprovalMode::Trust,
        max_rounds: 3,
    };
    agent.submit(&mut store, "Answer without looping").unwrap();
    assert!(
        agent
            .run(&mut store, &mut |_| {}, &mut |_| false)
            .await
            .unwrap_err()
            .to_string()
            .contains("configured budget of 3")
    );
    assert_eq!(store.tool_outcomes(&session).unwrap().len(), 2);

    drop(store);
    let mut store = Store::open(home.path()).unwrap();
    agent.max_rounds = 5;
    let error = agent
        .run(&mut store, &mut |_| {}, &mut |_| false)
        .await
        .unwrap_err()
        .to_string();
    assert!(error.contains("Stopped a repeated shell loop"), "{error}");
    assert!(error.contains("rejected before execution"), "{error}");
    assert_eq!(store.tool_outcomes(&session).unwrap().len(), 3);
    assert!(
        !store
            .used_call_ids(&session)
            .unwrap()
            .contains("same_shell_3")
    );
    assert!(pending(&store.messages(&session).unwrap()));

    let requests = server.mock.requests.lock().unwrap();
    assert_eq!(requests.len(), 4);
    assert!(
        requests[3]["messages"]
            .to_string()
            .contains("Runtime repetition check")
    );
}

#[tokio::test]
async fn repeated_calls_inside_one_batch_are_rejected_before_any_dispatch() {
    let calls = (0..4)
        .map(|index| {
            json!({
                "index":index,
                "id":format!("same_batch_{index}"),
                "type":"function",
                "function":{
                    "name":"shell",
                    "arguments":"{\"command\":\"printf stable\"}"
                }
            })
        })
        .collect::<Vec<_>>();
    let server = server(vec![(
        StatusCode::OK,
        format!(
            "{}{}",
            delta(json!({"tool_calls":calls})),
            finish("tool_calls")
        ),
    )])
    .await;
    let home = tempfile::tempdir().unwrap();
    let mut store = Store::open(home.path()).unwrap();
    let session = store
        .create("repeated batch", "local", home.path(), "system")
        .unwrap();
    let agent = Agent {
        memory: None,
        provider: OpenAiCompatible::new(server.profile.clone()).unwrap(),
        profile: server.profile.clone(),
        workspace: Workspace::new(home.path()).unwrap(),
        session: session.clone(),
        approval: ApprovalMode::Trust,
        max_rounds: 2,
    };
    agent.submit(&mut store, "Do bounded work").unwrap();
    let error = agent
        .run(&mut store, &mut |_| {}, &mut |_| false)
        .await
        .unwrap_err()
        .to_string();
    assert!(error.contains("Stopped a repeated shell loop"), "{error}");
    assert!(error.contains("entire response was rejected"), "{error}");
    assert!(store.used_call_ids(&session).unwrap().is_empty());
    assert!(store.tool_outcomes(&session).unwrap().is_empty());
    assert!(pending(&store.messages(&session).unwrap()));
}

#[tokio::test]
async fn oversized_tool_batch_is_rejected_as_a_whole_before_dispatch() {
    let calls = (0..17)
        .map(|index| {
            json!({
                "index":index,
                "id":format!("oversized_{index}"),
                "type":"function",
                "function":{
                    "name":"shell",
                    "arguments":json!({"command":format!("printf {index}")}).to_string()
                }
            })
        })
        .collect::<Vec<_>>();
    let server = server(vec![(
        StatusCode::OK,
        format!(
            "{}{}",
            delta(json!({"tool_calls":calls})),
            finish("tool_calls")
        ),
    )])
    .await;
    let home = tempfile::tempdir().unwrap();
    let mut store = Store::open(home.path()).unwrap();
    let session = store
        .create("oversized batch", "local", home.path(), "system")
        .unwrap();
    let agent = Agent {
        memory: None,
        provider: OpenAiCompatible::new(server.profile.clone()).unwrap(),
        profile: server.profile.clone(),
        workspace: Workspace::new(home.path()).unwrap(),
        session: session.clone(),
        approval: ApprovalMode::Trust,
        max_rounds: 2,
    };
    agent.submit(&mut store, "Do bounded work").unwrap();
    let error = agent
        .run(&mut store, &mut |_| {}, &mut |_| false)
        .await
        .unwrap_err()
        .to_string();
    assert!(error.contains("17 tool calls"), "{error}");
    assert!(error.contains("rejected before execution"), "{error}");
    assert!(store.used_call_ids(&session).unwrap().is_empty());
    assert!(store.tool_outcomes(&session).unwrap().is_empty());
    assert!(pending(&store.messages(&session).unwrap()));
}

#[tokio::test]
async fn saved_repeated_shell_is_rejected_before_resume_dispatch() {
    let server = server(vec![]).await;
    let home = tempfile::tempdir().unwrap();
    let marker = home.path().join("repeated-resume-marker");
    let command = "printf touched >> repeated-resume-marker";
    let mut store = Store::open(home.path()).unwrap();
    let session = store
        .create("saved repeated shell", "local", home.path(), "system")
        .unwrap();
    store
        .append(&session, &Message::text(Role::User, "Do bounded work"))
        .unwrap();
    for index in 0..3 {
        let id = format!("saved_repeat_{index}");
        store
            .append(&session, &shell_message(&id, command))
            .unwrap();
        assert!(store.claim_tool(&session, &id).unwrap());
        store
            .complete_tool_with_outcome(
                &session,
                &id,
                "synthetic prior result",
                ToolOutcome::Succeeded,
            )
            .unwrap();
    }
    store
        .append(&session, &shell_message("saved_repeat_3", command))
        .unwrap();
    assert_eq!(
        store.tool_run_state(&session, "saved_repeat_3").unwrap(),
        ToolRunState::Unclaimed
    );
    drop(store);

    let mut store = Store::open(home.path()).unwrap();
    let agent = Agent {
        memory: None,
        provider: OpenAiCompatible::new(server.profile.clone()).unwrap(),
        profile: server.profile.clone(),
        workspace: Workspace::new(home.path()).unwrap(),
        session: session.clone(),
        approval: ApprovalMode::Trust,
        max_rounds: 3,
    };
    let error = agent
        .run(&mut store, &mut |_| {}, &mut |_| false)
        .await
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("Stopped a saved repeated shell loop"),
        "{error}"
    );
    assert!(error.contains("rejected before execution"), "{error}");
    assert!(!marker.exists());
    assert_eq!(
        store.tool_run_state(&session, "saved_repeat_3").unwrap(),
        ToolRunState::Unclaimed
    );
    assert!(server.mock.requests.lock().unwrap().is_empty());
}

#[tokio::test]
async fn saved_oversized_batch_is_rejected_before_resume_dispatch() {
    let server = server(vec![]).await;
    let home = tempfile::tempdir().unwrap();
    let marker = home.path().join("oversized-resume-marker");
    let mut store = Store::open(home.path()).unwrap();
    let session = store
        .create("saved oversized batch", "local", home.path(), "system")
        .unwrap();
    store
        .append(&session, &Message::text(Role::User, "Do bounded work"))
        .unwrap();
    let calls = (0..17)
        .map(|index| {
            json!({
                "id":format!("saved_oversized_{index}"),
                "type":"function",
                "function":{
                    "name":"shell",
                    "arguments":json!({
                        "command":format!("printf {index} >> oversized-resume-marker")
                    }).to_string()
                }
            })
        })
        .collect::<Vec<_>>();
    let batch: Message = serde_json::from_value(json!({
        "role":"assistant", "content":null, "tool_calls":calls
    }))
    .unwrap();
    store.append(&session, &batch).unwrap();
    drop(store);

    let mut store = Store::open(home.path()).unwrap();
    let agent = Agent {
        memory: None,
        provider: OpenAiCompatible::new(server.profile.clone()).unwrap(),
        profile: server.profile.clone(),
        workspace: Workspace::new(home.path()).unwrap(),
        session: session.clone(),
        approval: ApprovalMode::Trust,
        max_rounds: 3,
    };
    let error = agent
        .run(&mut store, &mut |_| {}, &mut |_| false)
        .await
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("Saved assistant response contains 17 tool calls"),
        "{error}"
    );
    assert!(error.contains("rejected before execution"), "{error}");
    assert!(!marker.exists());
    for call in &batch.tool_calls {
        assert_eq!(
            store.tool_run_state(&session, &call.id).unwrap(),
            ToolRunState::Unclaimed
        );
    }
    assert!(server.mock.requests.lock().unwrap().is_empty());
}

#[tokio::test]
async fn saved_duplicate_call_ids_are_rejected_before_resume_dispatch() {
    let server = server(vec![]).await;
    let home = tempfile::tempdir().unwrap();
    let mut store = Store::open(home.path()).unwrap();
    let session = store
        .create("saved duplicate IDs", "local", home.path(), "system")
        .unwrap();
    store
        .append(&session, &Message::text(Role::User, "Run bounded work"))
        .unwrap();
    let batch: Message = serde_json::from_value(json!({
        "role":"assistant",
        "tool_calls":[
            {"id":"duplicate","type":"function","function":{"name":"shell","arguments":"{\"command\":\"printf one >> duplicate-marker\"}"}},
            {"id":"duplicate","type":"function","function":{"name":"shell","arguments":"{\"command\":\"printf two >> duplicate-marker\"}"}}
        ]
    }))
    .unwrap();
    store.append(&session, &batch).unwrap();
    drop(store);

    let mut store = Store::open(home.path()).unwrap();
    let agent = Agent {
        memory: None,
        provider: OpenAiCompatible::new(server.profile.clone()).unwrap(),
        profile: server.profile.clone(),
        workspace: Workspace::new(home.path()).unwrap(),
        session: session.clone(),
        approval: ApprovalMode::Trust,
        max_rounds: 3,
    };
    let error = agent
        .run(&mut store, &mut |_| {}, &mut |_| false)
        .await
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("duplicate tool call ID \"duplicate\""),
        "{error}"
    );
    assert!(!home.path().join("duplicate-marker").exists());
    assert_eq!(
        store.tool_run_state(&session, "duplicate").unwrap(),
        ToolRunState::Unclaimed
    );
    assert!(server.mock.requests.lock().unwrap().is_empty());
}

#[tokio::test]
async fn saved_empty_call_id_is_rejected_before_resume_dispatch() {
    let server = server(vec![]).await;
    let home = tempfile::tempdir().unwrap();
    let mut store = Store::open(home.path()).unwrap();
    let session = store
        .create("saved empty ID", "local", home.path(), "system")
        .unwrap();
    store
        .append(&session, &Message::text(Role::User, "Run bounded work"))
        .unwrap();
    let batch: Message = serde_json::from_value(json!({
        "role":"assistant",
        "tool_calls":[
            {"id":"","type":"function","function":{"name":"shell","arguments":"{\"command\":\"printf escaped >> empty-id-marker\"}"}}
        ]
    }))
    .unwrap();
    store.append(&session, &batch).unwrap();
    drop(store);

    let mut store = Store::open(home.path()).unwrap();
    let agent = Agent {
        memory: None,
        provider: OpenAiCompatible::new(server.profile.clone()).unwrap(),
        profile: server.profile.clone(),
        workspace: Workspace::new(home.path()).unwrap(),
        session: session.clone(),
        approval: ApprovalMode::Trust,
        max_rounds: 3,
    };
    let error = agent
        .run(&mut store, &mut |_| {}, &mut |_| false)
        .await
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("empty or duplicate tool call ID \"\""),
        "{error}"
    );
    assert!(!home.path().join("empty-id-marker").exists());
    assert_eq!(
        store.tool_run_state(&session, "").unwrap(),
        ToolRunState::Unclaimed
    );
    assert!(server.mock.requests.lock().unwrap().is_empty());
}

#[tokio::test]
async fn saved_non_function_call_is_rejected_before_resume_dispatch() {
    let server = server(vec![]).await;
    let home = tempfile::tempdir().unwrap();
    let mut store = Store::open(home.path()).unwrap();
    let session = store
        .create("saved invalid kind", "local", home.path(), "system")
        .unwrap();
    store
        .append(&session, &Message::text(Role::User, "Run bounded work"))
        .unwrap();
    let batch: Message = serde_json::from_value(json!({
        "role":"assistant",
        "tool_calls":[
            {"id":"invalid_kind","type":"command","function":{"name":"shell","arguments":"{\"command\":\"printf escaped >> invalid-kind-marker\"}"}}
        ]
    }))
    .unwrap();
    store.append(&session, &batch).unwrap();
    drop(store);

    let mut store = Store::open(home.path()).unwrap();
    let agent = Agent {
        memory: None,
        provider: OpenAiCompatible::new(server.profile.clone()).unwrap(),
        profile: server.profile.clone(),
        workspace: Workspace::new(home.path()).unwrap(),
        session: session.clone(),
        approval: ApprovalMode::Trust,
        max_rounds: 3,
    };
    let error = agent
        .run(&mut store, &mut |_| {}, &mut |_| false)
        .await
        .unwrap_err()
        .to_string();
    assert!(error.contains("malformed tool call"), "{error}");
    assert!(!home.path().join("invalid-kind-marker").exists());
    assert_eq!(
        store.tool_run_state(&session, "invalid_kind").unwrap(),
        ToolRunState::Unclaimed
    );
    assert!(server.mock.requests.lock().unwrap().is_empty());
}

#[tokio::test]
async fn interrupted_claim_takes_precedence_over_saved_structure_validation() {
    let server = server(vec![]).await;
    let home = tempfile::tempdir().unwrap();
    let mut store = Store::open(home.path()).unwrap();
    let session = store
        .create("claimed invalid kind", "local", home.path(), "system")
        .unwrap();
    store
        .append(&session, &Message::text(Role::User, "Run bounded work"))
        .unwrap();
    let batch: Message = serde_json::from_value(json!({
        "role":"assistant",
        "tool_calls":[
            {"id":"started_invalid_kind","type":"command","function":{"name":"shell","arguments":"{\"command\":\"printf escaped >> started-invalid-marker\"}"}}
        ]
    }))
    .unwrap();
    store.append(&session, &batch).unwrap();
    assert!(store.claim_tool(&session, "started_invalid_kind").unwrap());
    drop(store);

    let mut store = Store::open(home.path()).unwrap();
    let agent = Agent {
        memory: None,
        provider: OpenAiCompatible::new(server.profile.clone()).unwrap(),
        profile: server.profile.clone(),
        workspace: Workspace::new(home.path()).unwrap(),
        session: session.clone(),
        approval: ApprovalMode::Trust,
        max_rounds: 3,
    };
    let error = agent
        .run(&mut store, &mut |_| {}, &mut |_| false)
        .await
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("interrupted tool has an uncertain outcome"),
        "{error}"
    );
    assert!(!error.contains("malformed tool call"), "{error}");
    assert!(!home.path().join("started-invalid-marker").exists());
    assert_eq!(
        store.tool_outcomes(&session).unwrap()["started_invalid_kind"],
        ToolOutcome::Uncertain
    );
    assert!(server.mock.requests.lock().unwrap().is_empty());
}

#[tokio::test]
async fn saved_call_cannot_reuse_an_earlier_generation_id_or_result() {
    let server = server(vec![]).await;
    let home = tempfile::tempdir().unwrap();
    let mut store = Store::open(home.path()).unwrap();
    let session = store
        .create("saved reused ID", "local", home.path(), "system")
        .unwrap();
    store
        .append(&session, &Message::text(Role::User, "Earlier work"))
        .unwrap();
    let earlier: Message = serde_json::from_value(json!({
        "role":"assistant",
        "tool_calls":[
            {"id":"reused_generation_id","type":"function","function":{"name":"shell","arguments":"{\"command\":\"printf earlier\"}"}}
        ]
    }))
    .unwrap();
    store.append(&session, &earlier).unwrap();
    store
        .append(
            &session,
            &Message::tool("reused_generation_id", "earlier result".into()),
        )
        .unwrap();
    store
        .append(&session, &Message::text(Role::User, "Later work"))
        .unwrap();
    let reused: Message = serde_json::from_value(json!({
        "role":"assistant",
        "tool_calls":[
            {"id":"reused_generation_id","type":"function","function":{"name":"shell","arguments":"{\"command\":\"printf escaped >> reused-id-marker\"}"}}
        ]
    }))
    .unwrap();
    store.append(&session, &reused).unwrap();
    drop(store);

    let mut store = Store::open(home.path()).unwrap();
    let agent = Agent {
        memory: None,
        provider: OpenAiCompatible::new(server.profile.clone()).unwrap(),
        profile: server.profile.clone(),
        workspace: Workspace::new(home.path()).unwrap(),
        session: session.clone(),
        approval: ApprovalMode::Trust,
        max_rounds: 3,
    };
    let error = agent
        .run(&mut store, &mut |_| {}, &mut |_| false)
        .await
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("reuses earlier tool call ID \"reused_generation_id\""),
        "{error}"
    );
    assert!(!home.path().join("reused-id-marker").exists());
    assert!(server.mock.requests.lock().unwrap().is_empty());
}

#[tokio::test]
async fn fresh_duplicate_call_ids_are_rejected_as_a_whole() {
    let calls = json!([
        {"index":0,"id":"fresh_duplicate","type":"function","function":{"name":"shell","arguments":"{\"command\":\"printf one >> fresh-duplicate-marker\"}"}},
        {"index":1,"id":"fresh_duplicate","type":"function","function":{"name":"shell","arguments":"{\"command\":\"printf two >> fresh-duplicate-marker\"}"}}
    ]);
    let mut server = server(vec![(
        StatusCode::OK,
        format!(
            "{}{}",
            delta(json!({"tool_calls":calls})),
            finish("tool_calls")
        ),
    )])
    .await;
    server.profile.max_attempts = 1;
    let home = tempfile::tempdir().unwrap();
    let mut store = Store::open(home.path()).unwrap();
    let session = store
        .create("fresh duplicate IDs", "local", home.path(), "system")
        .unwrap();
    let agent = Agent {
        memory: None,
        provider: OpenAiCompatible::new(server.profile.clone()).unwrap(),
        profile: server.profile.clone(),
        workspace: Workspace::new(home.path()).unwrap(),
        session: session.clone(),
        approval: ApprovalMode::Trust,
        max_rounds: 3,
    };
    agent.submit(&mut store, "Run bounded work").unwrap();
    let error = agent
        .run(&mut store, &mut |_| {}, &mut |_| false)
        .await
        .unwrap_err()
        .to_string();
    assert!(error.contains("duplicate tool call ID"), "{error}");
    assert!(!home.path().join("fresh-duplicate-marker").exists());
    assert!(
        !store
            .used_call_ids(&session)
            .unwrap()
            .contains("fresh_duplicate")
    );
}

#[tokio::test]
async fn finished_tool_missing_from_legacy_projection_is_restored_without_reexecution() {
    let server = server(vec![text_reply(
        "Answered from the restored durable result.",
    )])
    .await;
    let home = tempfile::tempdir().unwrap();
    let mut store = Store::open(home.path()).unwrap();
    let session = store
        .create("legacy projection repair", "local", home.path(), "system")
        .unwrap();
    store
        .append(
            &session,
            &Message::text(Role::User, "Answer from the command result"),
        )
        .unwrap();
    store
        .append(
            &session,
            &shell_message("finished_projection", "printf touched >> projection-marker"),
        )
        .unwrap();
    assert!(store.claim_tool(&session, "finished_projection").unwrap());
    store
        .complete_tool_with_outcome(
            &session,
            "finished_projection",
            "synthetic durable result",
            ToolOutcome::Succeeded,
        )
        .unwrap();
    let expected = store.messages(&session).unwrap();
    let malformed_projection = expected[..expected.len() - 1].to_vec();
    store
        .checkpoint(&session, &expected, &malformed_projection)
        .unwrap();
    drop(store);

    let mut store = Store::open(home.path()).unwrap();
    let agent = Agent {
        memory: None,
        provider: OpenAiCompatible::new(server.profile.clone()).unwrap(),
        profile: server.profile.clone(),
        workspace: Workspace::new(home.path()).unwrap(),
        session: session.clone(),
        approval: ApprovalMode::Trust,
        max_rounds: 3,
    };
    let mut tool_starts = 0;
    agent
        .run(
            &mut store,
            &mut |event| {
                if matches!(event, builder::agent::AgentEvent::ToolStarted { .. }) {
                    tool_starts += 1;
                }
            },
            &mut |_| panic!("finished tool must not request approval"),
        )
        .await
        .unwrap();
    assert_eq!(tool_starts, 0);
    assert!(!home.path().join("projection-marker").exists());
    assert!(!pending(&store.messages(&session).unwrap()));
    let requests = server.mock.requests.lock().unwrap();
    assert_eq!(requests.len(), 1);
    assert!(
        requests[0]["messages"]
            .as_array()
            .unwrap()
            .iter()
            .any(|message| message["tool_call_id"] == "finished_projection"
                && message["content"] == "synthetic durable result")
    );
}

#[tokio::test]
async fn uncertain_finished_projection_is_restored_then_pauses_without_dispatch() {
    let server = server(vec![]).await;
    let home = tempfile::tempdir().unwrap();
    let mut store = Store::open(home.path()).unwrap();
    let session = store
        .create(
            "uncertain projection repair",
            "local",
            home.path(),
            "system",
        )
        .unwrap();
    store
        .append(
            &session,
            &Message::text(Role::User, "Do not continue past uncertainty"),
        )
        .unwrap();
    store
        .append(
            &session,
            &shell_message("uncertain_projection", "printf touched >> uncertain-marker"),
        )
        .unwrap();
    assert!(store.claim_tool(&session, "uncertain_projection").unwrap());
    store
        .complete_tool_with_outcome(
            &session,
            "uncertain_projection",
            "synthetic uncertain result",
            ToolOutcome::Uncertain,
        )
        .unwrap();
    let expected = store.messages(&session).unwrap();
    let malformed_projection = expected[..expected.len() - 1].to_vec();
    store
        .checkpoint(&session, &expected, &malformed_projection)
        .unwrap();
    drop(store);

    let mut store = Store::open(home.path()).unwrap();
    let agent = Agent {
        memory: None,
        provider: OpenAiCompatible::new(server.profile.clone()).unwrap(),
        profile: server.profile.clone(),
        workspace: Workspace::new(home.path()).unwrap(),
        session: session.clone(),
        approval: ApprovalMode::Trust,
        max_rounds: 3,
    };
    let error = agent
        .run(&mut store, &mut |_| {}, &mut |_| false)
        .await
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("restored with an uncertain outcome"),
        "{error}"
    );
    assert!(!home.path().join("uncertain-marker").exists());
    assert_eq!(
        store.tool_outcomes(&session).unwrap()["uncertain_projection"],
        ToolOutcome::Uncertain
    );
    assert!(server.mock.requests.lock().unwrap().is_empty());
}

#[tokio::test]
async fn completed_legacy_oversized_batch_can_resume_to_final_without_reexecution() {
    let server = server(vec![text_reply(
        "Final answer from the completed legacy batch.",
    )])
    .await;
    let home = tempfile::tempdir().unwrap();
    let marker = home.path().join("completed-legacy-marker");
    let mut store = Store::open(home.path()).unwrap();
    let session = store
        .create("completed legacy batch", "local", home.path(), "system")
        .unwrap();
    store
        .append(
            &session,
            &Message::text(Role::User, "Answer from the results"),
        )
        .unwrap();
    let calls = (0..17)
        .map(|index| {
            json!({
                "id":format!("completed_legacy_{index}"),
                "type":"function",
                "function":{
                    "name":"shell",
                    "arguments":json!({
                        "command":format!("printf {index} >> completed-legacy-marker")
                    }).to_string()
                }
            })
        })
        .collect::<Vec<_>>();
    let batch: Message = serde_json::from_value(json!({
        "role":"assistant", "content":null, "tool_calls":calls
    }))
    .unwrap();
    store.append(&session, &batch).unwrap();
    for call in &batch.tool_calls {
        assert!(store.claim_tool(&session, &call.id).unwrap());
        store
            .complete_tool_with_outcome(
                &session,
                &call.id,
                "synthetic completed result",
                ToolOutcome::Succeeded,
            )
            .unwrap();
    }
    drop(store);

    let mut store = Store::open(home.path()).unwrap();
    let agent = Agent {
        memory: None,
        provider: OpenAiCompatible::new(server.profile.clone()).unwrap(),
        profile: server.profile.clone(),
        workspace: Workspace::new(home.path()).unwrap(),
        session: session.clone(),
        approval: ApprovalMode::Trust,
        max_rounds: 3,
    };
    let mut tool_starts = 0;
    agent
        .run(
            &mut store,
            &mut |event| {
                if matches!(event, builder::agent::AgentEvent::ToolStarted { .. }) {
                    tool_starts += 1;
                }
            },
            &mut |_| panic!("completed calls must not request approval"),
        )
        .await
        .unwrap();
    assert_eq!(tool_starts, 0);
    assert!(!marker.exists());
    assert_eq!(store.tool_outcomes(&session).unwrap().len(), 17);
    assert!(!pending(&store.messages(&session).unwrap()));
    assert_eq!(server.mock.requests.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn interrupted_claim_takes_precedence_over_saved_repetition_guard() {
    let server = server(vec![]).await;
    let home = tempfile::tempdir().unwrap();
    let marker = home.path().join("uncertain-resume-marker");
    let command = "printf touched >> uncertain-resume-marker";
    let mut store = Store::open(home.path()).unwrap();
    let session = store
        .create("uncertain saved repeat", "local", home.path(), "system")
        .unwrap();
    store
        .append(&session, &Message::text(Role::User, "Do bounded work"))
        .unwrap();
    for index in 0..3 {
        let id = format!("uncertain_prior_{index}");
        store
            .append(&session, &shell_message(&id, command))
            .unwrap();
        assert!(store.claim_tool(&session, &id).unwrap());
        store
            .complete_tool_with_outcome(
                &session,
                &id,
                "synthetic prior result",
                ToolOutcome::Succeeded,
            )
            .unwrap();
    }
    store
        .append(&session, &shell_message("uncertain_saved", command))
        .unwrap();
    assert!(store.claim_tool(&session, "uncertain_saved").unwrap());
    drop(store);

    let mut store = Store::open(home.path()).unwrap();
    let agent = Agent {
        memory: None,
        provider: OpenAiCompatible::new(server.profile.clone()).unwrap(),
        profile: server.profile.clone(),
        workspace: Workspace::new(home.path()).unwrap(),
        session: session.clone(),
        approval: ApprovalMode::Trust,
        max_rounds: 3,
    };
    let error = agent
        .run(&mut store, &mut |_| {}, &mut |_| false)
        .await
        .unwrap_err()
        .to_string();
    assert!(error.contains("uncertain outcome"), "{error}");
    assert!(!error.contains("repeated shell loop"), "{error}");
    assert!(!marker.exists());
    assert_eq!(
        store.tool_run_state(&session, "uncertain_saved").unwrap(),
        ToolRunState::Finished
    );
    assert_eq!(
        store.tool_outcomes(&session).unwrap()["uncertain_saved"],
        ToolOutcome::Uncertain
    );
    assert!(server.mock.requests.lock().unwrap().is_empty());
}

#[tokio::test]
async fn uncertain_shell_cannot_be_replayed_with_a_new_id_or_timeout_on_retry() {
    let command = "printf touched >> uncertain-replay-marker";
    let mut replay = shell_message("uncertain_replay", command);
    replay.tool_calls[0].function.arguments =
        json!({"command":command, "timeout_secs":30}).to_string();
    let replay_call = replay.tool_calls[0].clone();
    let server = server(vec![(
        StatusCode::OK,
        format!(
            "{}{}",
            delta(json!({"tool_calls":[{
                "index":0,
                "id":replay_call.id,
                "type":"function",
                "function":replay_call.function
            }]})),
            finish("tool_calls")
        ),
    )])
    .await;
    let home = tempfile::tempdir().unwrap();
    let marker = home.path().join("uncertain-replay-marker");
    let mut store = Store::open(home.path()).unwrap();
    let session = store
        .create("uncertain replay", "local", home.path(), "system")
        .unwrap();
    store
        .append(&session, &Message::text(Role::User, "Run the command"))
        .unwrap();
    store
        .append(&session, &shell_message("uncertain_original", command))
        .unwrap();
    assert!(store.claim_tool(&session, "uncertain_original").unwrap());
    drop(store);

    let mut store = Store::open(home.path()).unwrap();
    let agent = Agent {
        memory: None,
        provider: OpenAiCompatible::new(server.profile.clone()).unwrap(),
        profile: server.profile.clone(),
        workspace: Workspace::new(home.path()).unwrap(),
        session: session.clone(),
        approval: ApprovalMode::Trust,
        max_rounds: 3,
    };
    let first_error = agent
        .run(&mut store, &mut |_| {}, &mut |_| false)
        .await
        .unwrap_err()
        .to_string();
    assert!(first_error.contains("uncertain outcome"), "{first_error}");
    assert!(server.mock.requests.lock().unwrap().is_empty());

    let retry_error = agent
        .run(&mut store, &mut |_| {}, &mut |_| false)
        .await
        .unwrap_err()
        .to_string();
    assert!(
        retry_error.contains("Refused to replay shell"),
        "{retry_error}"
    );
    assert!(retry_error.contains("uncertain outcome"), "{retry_error}");
    assert!(!marker.exists());
    assert!(
        !store
            .used_call_ids(&session)
            .unwrap()
            .contains("uncertain_replay")
    );
    assert_eq!(server.mock.requests.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn denied_shell_cannot_be_replayed_under_a_new_call_id() {
    let command = "printf touched >> denied-replay-marker";
    let response = |id: &str| {
        let call = shell_message(id, command).tool_calls.remove(0);
        (
            StatusCode::OK,
            format!(
                "{}{}",
                delta(json!({"tool_calls":[{
                    "index":0,
                    "id":call.id,
                    "type":"function",
                    "function":call.function
                }]})),
                finish("tool_calls")
            ),
        )
    };
    let server = server(vec![response("denied_original"), response("denied_replay")]).await;
    let home = tempfile::tempdir().unwrap();
    let marker = home.path().join("denied-replay-marker");
    let mut store = Store::open(home.path()).unwrap();
    let session = store
        .create("denied replay", "local", home.path(), "system")
        .unwrap();
    let agent = Agent {
        memory: None,
        provider: OpenAiCompatible::new(server.profile.clone()).unwrap(),
        profile: server.profile.clone(),
        workspace: Workspace::new(home.path()).unwrap(),
        session: session.clone(),
        approval: ApprovalMode::ReadOnly,
        max_rounds: 3,
    };
    agent.submit(&mut store, "Run the command").unwrap();
    let error = agent
        .run(&mut store, &mut |_| {}, &mut |_| panic!("read-only mode"))
        .await
        .unwrap_err()
        .to_string();
    assert!(error.contains("Refused to replay shell"), "{error}");
    assert!(error.contains("denied outcome"), "{error}");
    assert!(!marker.exists());
    assert_eq!(
        store.tool_outcomes(&session).unwrap()["denied_original"],
        ToolOutcome::Denied
    );
    assert!(
        !store
            .used_call_ids(&session)
            .unwrap()
            .contains("denied_replay")
    );
    assert_eq!(server.mock.requests.lock().unwrap().len(), 2);
}

#[tokio::test]
async fn giant_read_is_redirected_to_targeted_read_before_it_can_fill_context() {
    let broad = read_message("broad", json!({"path":"large.ts"}));
    let narrow = read_message(
        "narrow",
        json!({"path":"large.ts","start_line":490,"end_line":510}),
    );
    let reply = |message: &Message| {
        (
            StatusCode::OK,
            format!(
                "{}{}",
                delta(
                    json!({"tool_calls": message.tool_calls.iter().enumerate().map(|(index, call)| {
            let mut value = serde_json::to_value(call).unwrap();
            value["index"] = json!(index);
            value
        }).collect::<Vec<_>>()})
                ),
                finish("tool_calls")
            ),
        )
    };
    let server = server(vec![
        reply(&broad),
        reply(&narrow),
        text_reply("Ready to edit the located function."),
    ])
    .await;
    let home = tempfile::tempdir().unwrap();
    std::fs::write(
        home.path().join("large.ts"),
        (1..=1400)
            .map(|n| format!("source line {n}\n"))
            .collect::<String>(),
    )
    .unwrap();
    let mut store = Store::open(home.path()).unwrap();
    let session = store
        .create("bounded reads", "local", home.path(), "system")
        .unwrap();
    let agent = Agent {
        memory: None,
        provider: OpenAiCompatible::new(server.profile.clone()).unwrap(),
        profile: server.profile.clone(),
        workspace: Workspace::new(home.path()).unwrap(),
        session: session.clone(),
        approval: ApprovalMode::Trust,
        max_rounds: 4,
    };
    agent
        .submit(&mut store, "Inspect the relevant function")
        .unwrap();
    agent
        .run(&mut store, &mut |_| {}, &mut |_| {
            panic!("reads need no approval")
        })
        .await
        .unwrap();
    let requests = server.mock.requests.lock().unwrap();
    assert_eq!(requests.len(), 3);
    let broad_result = requests[1]["messages"].as_array().unwrap().last().unwrap()["content"]
        .as_str()
        .unwrap();
    assert!(broad_result.contains("Large file"));
    assert!(!broad_result.contains("source line"));
    let narrow_result = requests[2]["messages"].as_array().unwrap().last().unwrap()["content"]
        .as_str()
        .unwrap();
    assert!(narrow_result.contains("  490|source line 490"));
    assert!(narrow_result.contains("  510|source line 510"));
    assert!(!narrow_result.contains("source line 511"));
    assert!(builder::agent::estimate_tokens(&store.messages(&session).unwrap()) < 2000);
}

#[tokio::test]
async fn read_inventory_survives_repeated_compaction_and_restart_even_if_model_forgets() {
    let server = server(vec![
        text_reply("Proceed to implementation."),
        text_reply("Continue implementation."),
    ])
    .await;
    let home = tempfile::tempdir().unwrap();
    let mut store = Store::open(home.path()).unwrap();
    let session = store
        .create("read inventory", "local", home.path(), "system")
        .unwrap();
    let agent = Agent {
        memory: None,
        provider: OpenAiCompatible::new(server.profile.clone()).unwrap(),
        profile: server.profile.clone(),
        workspace: Workspace::new(home.path()).unwrap(),
        session: session.clone(),
        approval: ApprovalMode::Trust,
        max_rounds: 4,
    };
    agent.submit(&mut store, "Implement the change").unwrap();
    for (id, path, result) in [
        ("legacy", "old.ts", "  480  old source\n  481  next source"),
        (
            "new",
            "new.ts",
            "File: \"new.ts\" · 1400 total lines · 20000 bytes\n  960|  config\n  961|  more config",
        ),
        (
            "failed",
            "failed.ts",
            "ERROR: Large file; no source returned",
        ),
    ] {
        store
            .append(&session, &read_message(id, json!({"path":path})))
            .unwrap();
        store.claim_tool(&session, id).unwrap();
        store.complete_tool(&session, id, result).unwrap();
    }
    for cycle in 0..2 {
        store
            .append(
                &session,
                &Message::text(Role::Assistant, "exploration evidence ".repeat(900)),
            )
            .unwrap();
        agent
            .submit(&mut store, "Continue from established findings")
            .unwrap();
        let original = store.history_messages(&session).unwrap();
        assert!(agent.compact(&mut store, &mut |_| {}).await.unwrap());
        let context = store.messages(&session).unwrap();
        let handoff = context
            .iter()
            .find_map(|m| {
                m.content
                    .as_deref()
                    .filter(|s| s.contains("[Read inventory rebuilt"))
            })
            .unwrap();
        assert!(handoff.contains("\"path\":\"old.ts\""));
        assert!(handoff.contains("\"returned_lines\":[480,481]"));
        assert!(handoff.contains("\"path\":\"new.ts\""));
        assert!(handoff.contains("\"returned_lines\":[960,961]"));
        assert!(!handoff.contains("failed.ts"));
        assert!(!handoff.contains("old source"));
        assert_eq!(handoff.matches("[Read inventory rebuilt").count(), 1);
        assert_eq!(store.history_messages(&session).unwrap(), original);
        assert!(pending(&context));
        drop(store);
        store = Store::open(home.path()).unwrap();
        assert_eq!(store.messages(&session).unwrap(), context, "cycle {cycle}");
    }
    assert_eq!(server.mock.requests.lock().unwrap().len(), 2);
}

#[test]
fn sse_preserves_utf8_split_at_every_byte_and_crlf() {
    let source = ": ping\r\ndata: {\"text\":\"hello 🦀\"}\r\n\r\ndata: [DONE]\r\n\r\n";
    let mut decoder = SseDecoder::default();
    let mut events = vec![];
    for byte in source.as_bytes() {
        events.extend(decoder.push(&[*byte]).unwrap());
    }
    assert_eq!(events, vec!["{\"text\":\"hello 🦀\"}", "[DONE]"]);
}

#[tokio::test]
async fn dropped_stream_retries_identical_context_and_discards_partial() {
    let server = server(vec![
        (StatusCode::OK, delta(json!({"content":"discard me"}))),
        text_reply("complete response"),
    ])
    .await;
    let provider = OpenAiCompatible::new(server.profile.clone()).unwrap();
    let messages = vec![
        Message::text(Role::System, "remember everything"),
        Message::text(Role::User, "first"),
        Message::text(Role::Assistant, "earlier answer"),
        Message::text(Role::User, "next"),
    ];
    let mut retries = 0;
    let result = provider
        .complete(&messages, &[], &mut |event| {
            if matches!(event, Event::Retry { .. }) {
                retries += 1;
            }
        })
        .await
        .unwrap();
    assert_eq!(result.content.as_deref(), Some("complete response"));
    assert_eq!(retries, 1);
    let requests = server.mock.requests.lock().unwrap();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[0], requests[1]);
    assert_eq!(requests[1]["messages"], json!(messages));
}

#[tokio::test]
async fn rate_limits_and_service_failures_retry_but_auth_does_not() {
    let retrying = server(vec![
        (StatusCode::TOO_MANY_REQUESTS, "".into()),
        (StatusCode::SERVICE_UNAVAILABLE, "".into()),
        text_reply("ok"),
    ])
    .await;
    let provider = OpenAiCompatible::new(retrying.profile.clone()).unwrap();
    provider
        .complete(&[Message::text(Role::User, "hi")], &[], &mut |_| {})
        .await
        .unwrap();
    assert_eq!(retrying.mock.requests.lock().unwrap().len(), 3);
    let auth = server(vec![
        (
            StatusCode::UNAUTHORIZED,
            "secret must not appear in error".into(),
        ),
        text_reply("must not run"),
    ])
    .await;
    let error = OpenAiCompatible::new(auth.profile.clone())
        .unwrap()
        .complete(&[Message::text(Role::User, "hi")], &[], &mut |_| {})
        .await
        .unwrap_err();
    assert!(error.to_string().contains("401"));
    assert!(!error.to_string().contains("secret"));
    assert_eq!(auth.mock.requests.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn fragmented_tool_arguments_assemble_before_execution() {
    let reply = format!(
        "{}{}{}",
        delta(
            json!({"tool_calls":[{"index":0,"id":"call_1","type":"function","function":{"name":"read_file","arguments":"{\"pa"}}]})
        ),
        delta(json!({"tool_calls":[{"index":0,"function":{"arguments":"th\":\"hello.rs\"}"}}]})),
        finish("tool_calls")
    );
    let server = server(vec![(StatusCode::OK, reply)]).await;
    let response = OpenAiCompatible::new(server.profile.clone())
        .unwrap()
        .complete(&[Message::text(Role::User, "read")], &[], &mut |_| {})
        .await
        .unwrap();
    assert_eq!(
        response.tool_calls[0].function.arguments,
        "{\"path\":\"hello.rs\"}"
    );
}

#[tokio::test]
async fn json_mode_requests_response_format_and_plain_requests_do_not() {
    let server = server(vec![text_reply("{\"findings\":[]}"), text_reply("plain")]).await;
    let provider = OpenAiCompatible::new(server.profile.clone()).unwrap();
    provider
        .complete_json(&[Message::text(Role::User, "extract")], 2048, &mut |_| {})
        .await
        .unwrap();
    provider
        .complete(&[Message::text(Role::User, "chat")], &[], &mut |_| {})
        .await
        .unwrap();
    let requests = server.mock.requests.lock().unwrap();
    assert_eq!(
        requests[0]["response_format"],
        json!({"type": "json_object"})
    );
    assert!(requests[1].get("response_format").is_none());
}

#[tokio::test]
async fn output_limit_never_commits_or_dispatches_incomplete_tool() {
    let server = server(vec![(
        StatusCode::OK,
        format!(
            "{}{}",
            delta(json!({"content":"unfinished"})),
            finish("length")
        ),
    )])
    .await;
    let error = OpenAiCompatible::new(server.profile.clone())
        .unwrap()
        .complete(&[Message::text(Role::User, "hi")], &[], &mut |_| {})
        .await
        .unwrap_err();
    assert!(error.to_string().contains("output limit"));
    assert_eq!(server.mock.requests.lock().unwrap().len(), 1);
}

#[test]
fn session_survives_reopen_and_has_exclusive_lock() {
    let home = tempfile::tempdir().unwrap();
    let id;
    {
        let mut store = Store::open(home.path()).unwrap();
        id = store
            .create("persist", "local", home.path(), "system")
            .unwrap();
        store
            .append(&id, &Message::text(Role::User, "never forget this"))
            .unwrap();
        let guard = store.lock(&id).unwrap();
        assert!(store.lock(&id).is_err());
        drop(guard);
        assert!(store.lock(&id).is_ok());
    }
    let store = Store::open(home.path()).unwrap();
    assert_eq!(
        store.messages(&id).unwrap()[1].content.as_deref(),
        Some("never forget this")
    );
    assert!(pending(&store.messages(&id).unwrap()));
}

fn tool_message() -> Message {
    serde_json::from_value(json!({"role":"assistant","content":null,"tool_calls":[{"id":"write_1","type":"function","function":{"name":"write_file","arguments":"{\"path\":\"result.txt\",\"content\":\"done\"}"}}]})).unwrap()
}

#[tokio::test]
async fn interrupted_tool_is_not_reexecuted_on_resume() {
    let server = server(vec![]).await;
    let home = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let mut store = Store::open(home.path()).unwrap();
    let session = store
        .create("crash", "local", workspace.path(), "system")
        .unwrap();
    store
        .append(&session, &Message::text(Role::User, "write"))
        .unwrap();
    store.append(&session, &tool_message()).unwrap();
    assert!(store.claim_tool(&session, "write_1").unwrap());
    // Simulate a process crash after claiming execution but before committing result.
    drop(store);
    let mut store = Store::open(home.path()).unwrap();
    let agent = Agent {
        memory: None,
        provider: OpenAiCompatible::new(server.profile.clone()).unwrap(),
        profile: server.profile.clone(),
        workspace: Workspace::new(workspace.path()).unwrap(),
        session: session.clone(),
        approval: ApprovalMode::Trust,
        max_rounds: 5,
    };
    let error = agent
        .run(&mut store, &mut |_| {}, &mut |_| true)
        .await
        .unwrap_err();
    assert!(error.to_string().contains("uncertain"));
    assert!(!workspace.path().join("result.txt").exists());
    assert_eq!(server.mock.requests.lock().unwrap().len(), 0);
    assert!(
        store
            .messages(&session)
            .unwrap()
            .last()
            .unwrap()
            .content
            .as_ref()
            .unwrap()
            .contains("uncertain")
    );
}

#[tokio::test]
async fn completed_tool_is_not_repeated_when_next_generation_fails() {
    let mut reply = delta(
        json!({"tool_calls":[{"index":0,"id":"write_1","type":"function","function":{"name":"write_file","arguments":"{\"path\":\"result.txt\",\"content\":\"done\"}"}}]}),
    );
    reply.push_str(&finish("tool_calls"));
    let mut server = server(vec![
        (StatusCode::OK, reply),
        (StatusCode::SERVICE_UNAVAILABLE, "".into()),
        text_reply("finished"),
    ])
    .await;
    server.profile.max_attempts = 1;
    let home = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let mut store = Store::open(home.path()).unwrap();
    let session = store
        .create("retry", "local", workspace.path(), "system")
        .unwrap();
    let agent = Agent {
        memory: None,
        provider: OpenAiCompatible::new(server.profile.clone()).unwrap(),
        profile: server.profile.clone(),
        workspace: Workspace::new(workspace.path()).unwrap(),
        session: session.clone(),
        approval: ApprovalMode::Trust,
        // Stay outside the last-five-round notice: a new run refreshes that
        // ephemeral budget, while the conversation and retry body stay intact.
        max_rounds: 10,
    };
    agent.submit(&mut store, "write the file").unwrap();
    assert!(
        agent
            .run(&mut store, &mut |_| {}, &mut |_| true)
            .await
            .is_err()
    );
    assert_eq!(
        std::fs::read_to_string(workspace.path().join("result.txt")).unwrap(),
        "done"
    );
    // A manual edit between runs would be overwritten if the tool were replayed.
    std::fs::write(workspace.path().join("result.txt"), "user changed it").unwrap();
    agent
        .run(&mut store, &mut |_| {}, &mut |_| true)
        .await
        .unwrap();
    assert_eq!(
        std::fs::read_to_string(workspace.path().join("result.txt")).unwrap(),
        "user changed it"
    );
    let requests = server.mock.requests.lock().unwrap();
    assert_eq!(requests[1], requests[2]);
    let messages = store.messages(&session).unwrap();
    assert_eq!(messages.iter().filter(|m| m.role == Role::Tool).count(), 1);
    assert!(!pending(&messages));
}

#[tokio::test]
async fn context_overflow_preserves_every_message_without_contacting_endpoint() {
    let server = server(vec![]).await;
    let home = tempfile::tempdir().unwrap();
    let mut store = Store::open(home.path()).unwrap();
    let session = store
        .create("budget", "local", home.path(), "system")
        .unwrap();
    let profile = Profile {
        context_tokens: 2048,
        max_output_tokens: 512,
        ..server.profile.clone()
    };
    let agent = Agent {
        memory: None,
        provider: OpenAiCompatible::new(profile.clone()).unwrap(),
        profile,
        workspace: Workspace::new(home.path()).unwrap(),
        session: session.clone(),
        approval: ApprovalMode::ReadOnly,
        max_rounds: 2,
    };
    agent.submit(&mut store, &"x".repeat(10_000)).unwrap();
    assert!(
        agent
            .run(&mut store, &mut |_| {}, &mut |_| false)
            .await
            .unwrap_err()
            .to_string()
            .contains("Context budget")
    );
    assert_eq!(
        store.messages(&session).unwrap()[1]
            .content
            .as_ref()
            .unwrap()
            .len(),
        10_000
    );
    assert!(server.mock.requests.lock().unwrap().is_empty());
}

#[tokio::test]
async fn read_only_denies_model_mutations_and_records_denial() {
    let call = tool_message();
    let body = json!({"choices":[{"message":call,"finish_reason":"tool_calls"}]}).to_string();
    let done = json!({"choices":[{"message":{"role":"assistant","content":"Change was denied."},"finish_reason":"stop"}]}).to_string();
    let mut server = server(vec![(StatusCode::OK, body), (StatusCode::OK, done)]).await;
    server.profile.stream = false;
    let home = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let mut store = Store::open(home.path()).unwrap();
    let session = store
        .create("denial", "local", workspace.path(), "system")
        .unwrap();
    let agent = Agent {
        memory: None,
        provider: OpenAiCompatible::new(server.profile.clone()).unwrap(),
        profile: server.profile.clone(),
        workspace: Workspace::new(workspace.path()).unwrap(),
        session: session.clone(),
        approval: ApprovalMode::ReadOnly,
        max_rounds: 3,
    };
    agent.submit(&mut store, "write").unwrap();
    agent
        .run(&mut store, &mut |_| {}, &mut |_| {
            panic!("read-only must not ask")
        })
        .await
        .unwrap();
    assert!(!workspace.path().join("result.txt").exists());
    assert!(
        store
            .messages(&session)
            .unwrap()
            .iter()
            .any(|m| m.role == Role::Tool && m.content.as_ref().unwrap().starts_with("DENIED:"))
    );
}

#[tokio::test]
async fn cancelled_generation_leaves_durable_pending_turn() {
    struct SlowProvider;
    impl Provider for SlowProvider {
        async fn complete(
            &self,
            _: &[Message],
            _: &[Value],
            _: &mut dyn FnMut(Event),
        ) -> anyhow::Result<Message> {
            std::future::pending().await
        }
    }
    let home = tempfile::tempdir().unwrap();
    let mut store = Store::open(home.path()).unwrap();
    let session = store
        .create("cancel", "local", home.path(), "system")
        .unwrap();
    let agent = Agent {
        memory: None,
        provider: SlowProvider,
        profile: Profile::default(),
        workspace: Workspace::new(home.path()).unwrap(),
        session: session.clone(),
        approval: ApprovalMode::ReadOnly,
        max_rounds: 3,
    };
    agent.submit(&mut store, "keep this request").unwrap();
    assert!(
        tokio::time::timeout(
            std::time::Duration::from_millis(20),
            agent.run(&mut store, &mut |_| {}, &mut |_| false)
        )
        .await
        .is_err()
    );
    drop(store);
    let store = Store::open(home.path()).unwrap();
    let messages = store.messages(&session).unwrap();
    assert_eq!(messages.len(), 2);
    assert!(pending(&messages));
}

#[tokio::test]
async fn idle_endpoint_times_out_within_budget() {
    let app = Router::new().route(
        "/v1/chat/completions",
        post(|| async {
            tokio::time::sleep(std::time::Duration::from_secs(30)).await;
            "too late"
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let profile = Profile {
        base_url: format!("http://{}/v1", listener.local_addr().unwrap()),
        idle_timeout_secs: 1,
        request_timeout_secs: 2,
        max_attempts: 1,
        ..Profile::default()
    };
    let task = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let start = std::time::Instant::now();
    let result = OpenAiCompatible::new(profile)
        .unwrap()
        .complete(&[Message::text(Role::User, "hi")], &[], &mut |_| {})
        .await;
    task.abort();
    assert!(result.is_err());
    assert!(start.elapsed() < std::time::Duration::from_secs(5));
}

#[tokio::test]
async fn cli_runs_and_resumes_with_complete_context() {
    let server = server(vec![
        text_reply("first answer"),
        text_reply("second answer"),
    ])
    .await;
    let home = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let exe = env!("CARGO_BIN_EXE_builder");
    let config = tokio::process::Command::new(exe)
        .args([
            "--home",
            home.path().to_str().unwrap(),
            "config",
            "add",
            "local",
            "--base-url",
            &server.profile.base_url,
            "--model",
            "test-model",
        ])
        .output()
        .await
        .unwrap();
    assert!(
        config.status.success(),
        "{}",
        String::from_utf8_lossy(&config.stderr)
    );
    let first = tokio::process::Command::new(exe)
        .args([
            "--home",
            home.path().to_str().unwrap(),
            "-C",
            workspace.path().to_str().unwrap(),
            "run",
            "first task",
        ])
        .output()
        .await
        .unwrap();
    assert!(
        first.status.success(),
        "{}",
        String::from_utf8_lossy(&first.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&first.stdout), "first answer\n");
    let store = Store::open(home.path()).unwrap();
    let session = store.sessions().unwrap().remove(0);
    let second = tokio::process::Command::new(exe)
        .args([
            "--home",
            home.path().to_str().unwrap(),
            "run",
            "--session",
            &session.id[..8],
            "second task",
        ])
        .output()
        .await
        .unwrap();
    assert!(
        second.status.success(),
        "{}",
        String::from_utf8_lossy(&second.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&second.stdout), "second answer\n");
    let requests = server.mock.requests.lock().unwrap();
    assert_eq!(requests[1]["messages"][1]["content"], "first task");
    assert_eq!(requests[1]["messages"][2]["content"], "first answer");
    assert_eq!(requests[1]["messages"][3]["content"], "second task");
}

#[tokio::test]
async fn oversized_nonstream_response_is_rejected_without_retry() {
    let mut server = server(vec![(StatusCode::OK, "x".repeat(17 * 1024 * 1024))]).await;
    server.profile.stream = false;
    let error = OpenAiCompatible::new(server.profile.clone())
        .unwrap()
        .complete(&[Message::text(Role::User, "hi")], &[], &mut |_| {})
        .await
        .unwrap_err();
    assert!(error.to_string().contains("16 MiB"));
    assert_eq!(server.mock.requests.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn cli_auto_approves_mutations_while_default_denies_without_a_terminal() {
    for auto in [false, true] {
        let mut reply = delta(
            json!({"tool_calls":[{"index":0,"id":"write_auto","type":"function","function":{"name":"write_file","arguments":"{\"path\":\"result.txt\",\"content\":\"done\"}"}}]}),
        );
        reply.push_str(&finish("tool_calls"));
        let server = server(vec![(StatusCode::OK, reply), text_reply("finished")]).await;
        let home = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        builder_core::config::Config {
            memory: Default::default(),
            default_profile: "local".into(),
            profiles: std::collections::BTreeMap::from([("local".into(), server.profile.clone())]),
        }
        .save(home.path())
        .unwrap();
        let mut command = tokio::process::Command::new(env!("CARGO_BIN_EXE_builder"));
        command
            .arg("--home")
            .arg(home.path())
            .arg("-C")
            .arg(workspace.path())
            .args(["run", "write the file"]);
        if auto {
            command.arg("--auto");
        }
        let output = command.output().await.unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(workspace.path().join("result.txt").exists(), auto);
        if auto {
            assert_eq!(
                std::fs::read_to_string(workspace.path().join("result.txt")).unwrap(),
                "done"
            );
        }
        let requests = server.mock.requests.lock().unwrap();
        let result = requests[1]["messages"].as_array().unwrap().last().unwrap()["content"]
            .as_str()
            .unwrap();
        assert_eq!(result.starts_with("DENIED:"), !auto);
    }
}

#[tokio::test]
async fn reasoning_activity_is_reported_without_becoming_conversation_content() {
    use builder_provider::Activity;
    let mut reply = delta(json!({"role":"assistant","content":""}));
    reply.push_str(&delta(
        json!({"reasoning_content":"private intermediate reasoning"}),
    ));
    reply.push_str(&delta(json!({"reasoning_content":"more reasoning"})));
    reply.push_str(&delta(json!({"content":"final answer"})));
    reply.push_str(&finish("stop"));
    let server = server(vec![(StatusCode::OK, reply)]).await;
    let provider = OpenAiCompatible::new(server.profile.clone()).unwrap();
    let mut activities = vec![];
    let mut visible = String::new();
    let mut reasoning = String::new();
    let mut prompt_bytes = 0;
    // History carrying earlier reasoning must reach the server without it.
    let mut earlier = Message::text(Role::Assistant, "earlier answer");
    earlier.reasoning = Some("earlier private reasoning".into());
    let result = provider
        .complete(
            &[
                Message::text(Role::User, "hello"),
                earlier,
                Message::text(Role::User, "continue"),
            ],
            &[],
            &mut |event| match event {
                Event::Prompt { bytes } => prompt_bytes = bytes,
                Event::Activity(activity) => activities.push(activity),
                Event::Delta(text) => {
                    assert!(!text.is_empty());
                    visible.push_str(&text);
                }
                Event::Reasoning(text) => reasoning.push_str(&text),
                _ => {}
            },
        )
        .await
        .unwrap();
    assert!(prompt_bytes >= 5);
    assert_eq!(activities, [Activity::Connected, Activity::Thinking]);
    assert_eq!(visible, "final answer");
    assert_eq!(result.content.as_deref(), Some("final answer"));
    // Reasoning is kept for the transcript, separately from the answer.
    assert_eq!(reasoning, "private intermediate reasoningmore reasoning");
    assert_eq!(result.reasoning.as_deref(), Some(reasoning.as_str()));
    let sent = server.mock.requests.lock().unwrap();
    assert_eq!(sent[0]["messages"][1]["content"], "earlier answer");
    assert!(
        !sent[0].to_string().contains("reasoning"),
        "reasoning must never be sent back to the model"
    );
}

#[tokio::test]
async fn follow_up_after_interruption_cancels_queued_tools_and_uses_new_instruction() {
    let server = server(vec![text_reply("Here is the handoff document.")]).await;
    let home = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let mut store = Store::open(home.path()).unwrap();
    let session = store
        .create("redirect", "local", workspace.path(), "system")
        .unwrap();
    let agent = Agent {
        memory: None,
        provider: OpenAiCompatible::new(server.profile.clone()).unwrap(),
        profile: server.profile.clone(),
        workspace: Workspace::new(workspace.path()).unwrap(),
        session: session.clone(),
        approval: ApprovalMode::Trust,
        max_rounds: 5,
    };
    agent.submit(&mut store, "Implement the change").unwrap();
    store.append(&session, &tool_message()).unwrap();
    // Interrupt after a complete tool batch is saved but before dispatch.
    agent
        .submit(&mut store, "Write a detailed handoff document instead")
        .unwrap();
    agent
        .run(&mut store, &mut |_| {}, &mut |_| {
            panic!("must not execute queued tools")
        })
        .await
        .unwrap();
    assert!(!workspace.path().join("result.txt").exists());
    let requests = server.mock.requests.lock().unwrap();
    assert_eq!(requests.len(), 1);
    let messages = requests[0]["messages"].as_array().unwrap();
    assert_eq!(
        messages.last().unwrap()["content"],
        "Write a detailed handoff document instead"
    );
    let result = messages.iter().find(|m| m["role"] == "tool").unwrap();
    assert_eq!(result["tool_call_id"], "write_1");
    assert!(
        result["content"]
            .as_str()
            .unwrap()
            .starts_with("CANCELLED:")
    );
}

#[tokio::test]
async fn steering_persists_new_prompt_but_halts_for_uncertain_execution_even_in_auto_mode() {
    let server = server(vec![text_reply("Continuing after inspection")]).await;
    let home = tempfile::tempdir().unwrap();
    let mut store = Store::open(home.path()).unwrap();
    let session = store
        .create("uncertain redirect", "local", home.path(), "system")
        .unwrap();
    let agent = Agent {
        memory: None,
        provider: OpenAiCompatible::new(server.profile.clone()).unwrap(),
        profile: server.profile.clone(),
        workspace: Workspace::new(home.path()).unwrap(),
        session: session.clone(),
        approval: ApprovalMode::Trust,
        max_rounds: 5,
    };
    agent.submit(&mut store, "write").unwrap();
    store.append(&session, &tool_message()).unwrap();
    store.claim_tool(&session, "write_1").unwrap();
    std::fs::write(home.path().join("result.txt"), "existing side effect").unwrap();
    let error = agent
        .submit(&mut store, "Explain what happened instead")
        .unwrap_err();
    assert!(error.to_string().contains("uncertain"));
    assert!(error.to_string().contains("new message is saved"));
    assert!(server.mock.requests.lock().unwrap().is_empty());
    assert_eq!(
        store.tool_outcomes(&session).unwrap()["write_1"],
        ToolOutcome::Uncertain
    );
    assert_eq!(
        store
            .messages(&session)
            .unwrap()
            .last()
            .unwrap()
            .content
            .as_deref(),
        Some("Explain what happened instead")
    );
    // Explicit /retry after inspecting: no replay of the interrupted write.
    agent
        .run(&mut store, &mut |_| {}, &mut |_| true)
        .await
        .unwrap();
    assert_eq!(
        std::fs::read_to_string(home.path().join("result.txt")).unwrap(),
        "existing side effect"
    );
}

#[tokio::test]
async fn rewind_excludes_abandoned_turn_and_reserves_its_tool_ids() {
    let call = delta(
        json!({"tool_calls":[{"index":0,"id":"write_1","type":"function",
        "function":{"name":"write_file","arguments":"{\"path\":\"result.txt\",\"content\":\"overwritten\"}"}}]}),
    );
    let server = server(vec![(
        StatusCode::OK,
        format!("{call}{}", finish("tool_calls")),
    )])
    .await;
    let home = tempfile::tempdir().unwrap();
    let mut store = Store::open(home.path()).unwrap();
    let session = store
        .create("rewind", "local", home.path(), "system")
        .unwrap();
    let agent = Agent {
        memory: None,
        provider: OpenAiCompatible::new(server.profile.clone()).unwrap(),
        profile: server.profile.clone(),
        workspace: Workspace::new(home.path()).unwrap(),
        session: session.clone(),
        approval: ApprovalMode::Trust,
        max_rounds: 5,
    };
    agent.submit(&mut store, "abandoned task").unwrap();
    store.append(&session, &tool_message()).unwrap();
    store.claim_tool(&session, "write_1").unwrap();
    std::fs::write(home.path().join("result.txt"), "keep existing changes").unwrap();
    store.complete_tool(&session, "write_1", "done").unwrap();
    let (draft, uncertain) = store.rewind(&session).unwrap();
    assert_eq!(draft, "abandoned task");
    assert_eq!(uncertain, 0);
    agent.submit(&mut store, "revised task").unwrap();
    let error = agent
        .run(&mut store, &mut |_| {}, &mut |_| true)
        .await
        .unwrap_err();
    assert!(error.to_string().contains("reused"));
    let requests = server.mock.requests.lock().unwrap();
    let context = requests[0]["messages"].as_array().unwrap();
    assert!(
        !context
            .iter()
            .any(|m| m["content"] == "abandoned task" || m["role"] == "tool")
    );
    assert_eq!(context.last().unwrap()["content"], "revised task");
    assert!(
        store
            .archived_messages(&session)
            .unwrap()
            .iter()
            .any(|m| m.content.as_deref() == Some("abandoned task"))
    );
    assert_eq!(
        std::fs::read_to_string(home.path().join("result.txt")).unwrap(),
        "keep existing changes"
    );
}

#[tokio::test]
async fn auto_compaction_preserves_tool_evidence_originals_and_latest_instruction() {
    let server = server(vec![
        text_reply("Inspected source and completed the write. Next: verify tests."),
        text_reply("Verified."),
    ])
    .await;
    let home = tempfile::tempdir().unwrap();
    let mut store = Store::open(home.path()).unwrap();
    let session = store
        .create("compact", "local", home.path(), "instructions stay exact")
        .unwrap();
    let mut profile = server.profile.clone();
    profile.context_tokens = 20000;
    profile.max_output_tokens = 1024;
    let agent = Agent {
        memory: None,
        provider: OpenAiCompatible::new(profile.clone()).unwrap(),
        profile,
        workspace: Workspace::new(home.path()).unwrap(),
        session: session.clone(),
        approval: ApprovalMode::Trust,
        max_rounds: 5,
    };
    agent
        .submit(&mut store, "Implement only Arena changes; do not commit.")
        .unwrap();
    store.append(&session, &tool_message()).unwrap();
    store.claim_tool(&session, "write_1").unwrap();
    store
        .complete_tool(&session, "write_1", &"source evidence ".repeat(1900))
        .unwrap();
    store
        .append(
            &session,
            &Message::text(Role::Assistant, "Finished inspecting."),
        )
        .unwrap();
    agent.submit(&mut store, "Verify the tests now.").unwrap();
    let original = store.history_messages(&session).unwrap();
    let mut compacted = false;
    agent
        .run(
            &mut store,
            &mut |event| {
                if let builder::agent::AgentEvent::Compacted { before, after, .. } = event {
                    assert!(after < before);
                    compacted = true;
                }
            },
            &mut |_| panic!("no replay"),
        )
        .await
        .unwrap();
    assert!(compacted);
    let requests = server.mock.requests.lock().unwrap();
    assert_eq!(requests.len(), 2);
    assert!(requests[0].get("tools").is_none());
    assert!(
        requests[1]["messages"][0]["content"]
            .as_str()
            .unwrap()
            .ends_with("instructions stay exact")
    );
    assert_eq!(
        store.history_messages(&session).unwrap()[0]
            .content
            .as_deref(),
        Some("instructions stay exact")
    );
    assert_eq!(
        requests[1]["messages"].as_array().unwrap().last().unwrap()["content"],
        "Verify the tests now."
    );
    assert!(
        !requests[1]["messages"]
            .as_array()
            .unwrap()
            .iter()
            .any(|m| m["role"] == "tool")
    );
    assert_eq!(
        &store.history_messages(&session).unwrap()[..original.len()],
        original
    );
    assert!(store.used_call_ids(&session).unwrap().contains("write_1"));
    let context = store.messages(&session).unwrap();
    drop(store);
    let mut store = Store::open(home.path()).unwrap();
    assert_eq!(store.messages(&session).unwrap(), context);
    assert!(!pending(&context));
    assert_eq!(store.rewind(&session).unwrap().0, "Verify the tests now.");
    assert!(
        store
            .messages(&session)
            .unwrap()
            .iter()
            .any(|m| m.role == Role::Tool)
    );
}

#[tokio::test]
async fn failed_compaction_never_changes_context_or_dispatches_summary_tools() {
    let reply = format!(
        "{}{}",
        delta(
            json!({"tool_calls":[{"index":0,"id":"summary_write","type":"function",
        "function":{"name":"write_file","arguments":"{\"path\":\"bad.txt\",\"content\":\"bad\"}"}}]})
        ),
        finish("tool_calls")
    );
    let server = server(vec![(StatusCode::OK, reply)]).await;
    let home = tempfile::tempdir().unwrap();
    let mut store = Store::open(home.path()).unwrap();
    let session = store
        .create("compact failure", "local", home.path(), "system")
        .unwrap();
    let agent = Agent {
        memory: None,
        provider: OpenAiCompatible::new(server.profile.clone()).unwrap(),
        profile: server.profile.clone(),
        workspace: Workspace::new(home.path()).unwrap(),
        session: session.clone(),
        approval: ApprovalMode::Trust,
        max_rounds: 5,
    };
    agent.submit(&mut store, "old goal").unwrap();
    store
        .append(
            &session,
            &Message::text(Role::Assistant, "evidence ".repeat(2000)),
        )
        .unwrap();
    agent.submit(&mut store, "new goal").unwrap();
    let original = store.messages(&session).unwrap();
    assert!(
        agent
            .compact(&mut store, &mut |_| {})
            .await
            .unwrap_err()
            .to_string()
            .contains("tool calls instead of a handoff")
    );
    assert_eq!(store.messages(&session).unwrap(), original);
    assert!(store.archived_messages(&session).unwrap().is_empty());
    assert!(!home.path().join("bad.txt").exists());
}

#[tokio::test]
async fn oversized_history_is_summarized_in_bounded_fragments_without_losing_bytes() {
    struct Summarizer {
        requests: std::sync::Mutex<Vec<Vec<Message>>>,
    }
    impl Provider for Summarizer {
        async fn complete(
            &self,
            messages: &[Message],
            tools: &[Value],
            _: &mut dyn FnMut(Event),
        ) -> anyhow::Result<Message> {
            assert!(tools.is_empty());
            assert!(builder::agent::estimate_tokens(messages) + 1024 <= 10000);
            self.requests.lock().unwrap().push(messages.to_vec());
            Ok(Message::text(
                Role::Assistant,
                "Earlier inspection complete; no mutations authorized.",
            ))
        }
    }
    let home = tempfile::tempdir().unwrap();
    let mut store = Store::open(home.path()).unwrap();
    let session = store
        .create("large compact", "local", home.path(), "system")
        .unwrap();
    let agent = Agent {
        memory: None,
        provider: Summarizer {
            requests: Default::default(),
        },
        profile: Profile {
            context_tokens: 10000,
            max_output_tokens: 1024,
            ..Profile::default()
        },
        workspace: Workspace::new(home.path()).unwrap(),
        session: session.clone(),
        approval: ApprovalMode::ReadOnly,
        max_rounds: 3,
    };
    agent.submit(&mut store, "original goal").unwrap();
    let evidence = Message::text(Role::Assistant, "source 🦀\n".repeat(9000));
    store.append(&session, &evidence).unwrap();
    agent.submit(&mut store, "latest goal").unwrap();
    let original = store.messages(&session).unwrap();
    assert!(agent.compact(&mut store, &mut |_| {}).await.unwrap());
    let requests = agent.provider.requests.lock().unwrap();
    assert!(requests.len() > 1);
    let fragments: String = requests
        .iter()
        .map(|r| {
            r[1].content
                .as_ref()
                .unwrap()
                .split_once("JSON may span fragments):\n")
                .unwrap()
                .1
        })
        .collect();
    assert_eq!(
        serde_json::from_str::<Vec<Message>>(&fragments).unwrap(),
        vec![original[1].clone(), evidence]
    );
    assert_eq!(store.history_messages(&session).unwrap(), original);
    assert!(pending(&store.messages(&session).unwrap()));
    assert_eq!(
        store
            .messages(&session)
            .unwrap()
            .last()
            .unwrap()
            .content
            .as_deref(),
        Some("latest goal")
    );
}

#[tokio::test]
async fn output_limit_recovery_is_bounded_and_never_executes_partial_tool_calls() {
    let partial = format!(
        "{}{}",
        delta(
            json!({"tool_calls":[{"index":0,"id":"incomplete","type":"function",
        "function":{"name":"write_file","arguments":"{\"path\":\"bad.txt\",\"content\":\"unfinished"}}]})
        ),
        finish("length")
    );
    for success in [true, false] {
        let server = server(vec![
            (StatusCode::OK, partial.clone()),
            if success {
                text_reply("Done")
            } else {
                (StatusCode::OK, partial.clone())
            },
        ])
        .await;
        let home = tempfile::tempdir().unwrap();
        let mut store = Store::open(home.path()).unwrap();
        let session = store
            .create("output retry", "local", home.path(), "system")
            .unwrap();
        let agent = Agent {
            memory: None,
            provider: OpenAiCompatible::new(server.profile.clone()).unwrap(),
            profile: server.profile.clone(),
            workspace: Workspace::new(home.path()).unwrap(),
            session: session.clone(),
            approval: ApprovalMode::Trust,
            max_rounds: 8,
        };
        agent.submit(&mut store, "write").unwrap();
        let result = agent
            .run(&mut store, &mut |_| {}, &mut |_| {
                panic!("partial tool dispatched")
            })
            .await;
        assert_eq!(result.is_ok(), success);
        if let Err(error) = result {
            assert!(error.is::<builder_provider::OutputLimit>());
        }
        assert!(!home.path().join("bad.txt").exists());
        let requests = server.mock.requests.lock().unwrap();
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[0]["messages"], requests[1]["messages"]);
        assert_eq!(
            requests[1]["max_tokens"].as_u64().unwrap(),
            requests[0]["max_tokens"].as_u64().unwrap() * 2
        );
        assert!(store.used_call_ids(&session).unwrap().is_empty());
    }
}

#[tokio::test]
async fn cancelled_compaction_keeps_active_context_unchanged() {
    struct SlowSummary;
    impl Provider for SlowSummary {
        async fn complete(
            &self,
            _: &[Message],
            _: &[Value],
            _: &mut dyn FnMut(Event),
        ) -> anyhow::Result<Message> {
            std::future::pending().await
        }
    }
    let home = tempfile::tempdir().unwrap();
    let mut store = Store::open(home.path()).unwrap();
    let session = store
        .create("cancel summary", "local", home.path(), "system")
        .unwrap();
    let agent = Agent {
        memory: None,
        provider: SlowSummary,
        profile: Profile::default(),
        workspace: Workspace::new(home.path()).unwrap(),
        session: session.clone(),
        approval: ApprovalMode::Trust,
        max_rounds: 5,
    };
    agent.submit(&mut store, "task").unwrap();
    store
        .append(
            &session,
            &Message::text(Role::Assistant, "evidence ".repeat(2000)),
        )
        .unwrap();
    agent.submit(&mut store, "continue").unwrap();
    let original = store.messages(&session).unwrap();
    assert!(
        tokio::time::timeout(
            std::time::Duration::from_millis(20),
            agent.compact(&mut store, &mut |_| {})
        )
        .await
        .is_err()
    );
    drop(store);
    let store = Store::open(home.path()).unwrap();
    assert_eq!(store.messages(&session).unwrap(), original);
    assert!(store.archived_messages(&session).unwrap().is_empty());
}

#[tokio::test]
async fn compaction_retains_denied_actions_verbatim_even_if_summary_omits_them() {
    let server = server(vec![text_reply("Generic summary without the denial")]).await;
    let home = tempfile::tempdir().unwrap();
    let mut store = Store::open(home.path()).unwrap();
    let session = store
        .create("denial summary", "local", home.path(), "system")
        .unwrap();
    let agent = Agent {
        memory: None,
        provider: OpenAiCompatible::new(server.profile.clone()).unwrap(),
        profile: server.profile.clone(),
        workspace: Workspace::new(home.path()).unwrap(),
        session: session.clone(),
        approval: ApprovalMode::Trust,
        max_rounds: 5,
    };
    agent.submit(&mut store, "task").unwrap();
    store.append(&session, &tool_message()).unwrap();
    store.claim_tool(&session, "write_1").unwrap();
    let denied = "DENIED: The user did not authorize this tool. Do not repeat it.";
    store.complete_tool(&session, "write_1", denied).unwrap();
    store
        .append(
            &session,
            &Message::text(Role::Assistant, "evidence ".repeat(2000)),
        )
        .unwrap();
    agent.submit(&mut store, "continue").unwrap();
    assert!(agent.compact(&mut store, &mut |_| {}).await.unwrap());
    let context = store.messages(&session).unwrap();
    let guard = context
        .iter()
        .find(|m| {
            m.content
                .as_deref()
                .is_some_and(|s| s.contains("Preserved execution constraint"))
        })
        .unwrap();
    assert!(guard.content.as_ref().unwrap().contains(denied));
    assert!(guard.content.as_ref().unwrap().contains("result.txt"));
}

#[tokio::test]
async fn compaction_output_limit_retries_once_then_continues_or_preserves_originals() {
    for succeeds in [true, false] {
        // Reasoning-only output can exhaust the generation budget without
        // producing a usable handoff. Provisional tool calls must not escape.
        let incomplete = (
            StatusCode::OK,
            format!(
                "{}{}{}",
                delta(json!({"reasoning_content":"thinking about the transcript"})),
                delta(json!({"content":"discard this incomplete handoff"})),
                finish("length")
            ),
        );
        let mut replies = vec![incomplete.clone()];
        if succeeds {
            replies.push(text_reply(
                "Inspected the source. Next: implement Arena-only changes.",
            ));
            replies.push(text_reply("Task continued after compaction."));
        } else {
            replies.push(incomplete);
        }
        let server = server(replies).await;
        let home = tempfile::tempdir().unwrap();
        let mut store = Store::open(home.path()).unwrap();
        let session = store
            .create("summary retry", "local", home.path(), "system")
            .unwrap();
        let profile = Profile {
            context_tokens: 81920,
            max_output_tokens: 8192,
            ..server.profile.clone()
        };
        let agent = Agent {
            memory: None,
            provider: OpenAiCompatible::new(profile.clone()).unwrap(),
            profile,
            workspace: Workspace::new(home.path()).unwrap(),
            session: session.clone(),
            approval: ApprovalMode::Trust,
            max_rounds: 5,
        };
        agent.submit(&mut store, "Inspect source").unwrap();
        store
            .append(
                &session,
                &Message::text(Role::Assistant, "file evidence ".repeat(8500)),
            )
            .unwrap();
        agent
            .submit(&mut store, "Implement Arena-only changes; do not commit.")
            .unwrap();
        let original = store.messages(&session).unwrap();
        let mut recoveries = vec![];
        let result = agent
            .run(
                &mut store,
                &mut |event| {
                    if let builder::agent::AgentEvent::OutputRecovery { budget } = event {
                        recoveries.push(budget);
                    }
                },
                &mut |_| panic!("compaction cannot execute tools"),
            )
            .await;
        assert_eq!(recoveries, vec![16384]);
        let requests = server.mock.requests.lock().unwrap();
        assert_eq!(requests.len(), if succeeds { 3 } else { 2 });
        assert_eq!(requests[0]["max_tokens"], 8192);
        assert_eq!(requests[1]["max_tokens"], 16384);
        let mut first = requests[0].clone();
        first["max_tokens"] = json!(16384);
        assert_eq!(first, requests[1], "retry must preserve exact input");
        for request in &requests[..2] {
            assert!(request.get("tools").is_none());
            let messages: Vec<Message> =
                serde_json::from_value(request["messages"].clone()).unwrap();
            assert!(
                builder::agent::estimate_tokens(&messages)
                    + request["max_tokens"].as_u64().unwrap() as usize
                    <= 81920
            );
        }
        if succeeds {
            result.unwrap();
            assert!(!pending(&store.messages(&session).unwrap()));
            assert!(
                !serde_json::to_string(&requests[2])
                    .unwrap()
                    .contains("discard this incomplete")
            );
            assert_eq!(
                &store.history_messages(&session).unwrap()[..original.len()],
                original
            );
        } else {
            assert!(
                result
                    .unwrap_err()
                    .to_string()
                    .contains("Compaction failed")
            );
            assert_eq!(store.messages(&session).unwrap(), original);
            assert!(store.archived_messages(&session).unwrap().is_empty());
        }
    }
}

#[tokio::test]
async fn larger_compaction_generation_budget_does_not_allow_an_oversized_handoff() {
    let server = server(vec![
        text_reply(&"x".repeat(10000)),
        text_reply(&"x".repeat(10000)),
    ])
    .await;
    let home = tempfile::tempdir().unwrap();
    let mut store = Store::open(home.path()).unwrap();
    let session = store
        .create("summary bound", "local", home.path(), "system")
        .unwrap();
    let profile = Profile {
        context_tokens: 81920,
        max_output_tokens: 8192,
        ..server.profile.clone()
    };
    let agent = Agent {
        memory: None,
        provider: OpenAiCompatible::new(profile.clone()).unwrap(),
        profile,
        workspace: Workspace::new(home.path()).unwrap(),
        session: session.clone(),
        approval: ApprovalMode::Trust,
        max_rounds: 5,
    };
    agent.submit(&mut store, "Old request").unwrap();
    store
        .append(
            &session,
            &Message::text(Role::Assistant, "evidence ".repeat(5000)),
        )
        .unwrap();
    agent.submit(&mut store, "Latest request").unwrap();
    let original = store.messages(&session).unwrap();
    let error = agent.compact(&mut store, &mut |_| {}).await.unwrap_err();
    assert!(error.to_string().contains("after one shortening retry"));
    assert_eq!(server.mock.requests.lock().unwrap().len(), 2);
    assert_eq!(store.messages(&session).unwrap(), original);
}

#[tokio::test]
async fn compaction_shortens_from_original_evidence_and_can_recover_output_limit() {
    let server = server(vec![
        text_reply(&"oversized draft ".repeat(800)),
        (
            StatusCode::OK,
            format!(
                "{}{}",
                delta(json!({"content":"partial draft"})),
                finish("length")
            ),
        ),
        text_reply("Source inspected; apply Arena-only changes, then test. Do not commit."),
    ])
    .await;
    let home = tempfile::tempdir().unwrap();
    let mut store = Store::open(home.path()).unwrap();
    let session = store
        .create("shorten", "local", home.path(), "exact system")
        .unwrap();
    let profile = Profile {
        context_tokens: 81920,
        max_output_tokens: 8192,
        ..server.profile.clone()
    };
    let agent = Agent {
        memory: None,
        provider: OpenAiCompatible::new(profile.clone()).unwrap(),
        profile,
        workspace: Workspace::new(home.path()).unwrap(),
        session: session.clone(),
        approval: ApprovalMode::Trust,
        max_rounds: 5,
    };
    agent.submit(&mut store, "Original goal").unwrap();
    store
        .append(
            &session,
            &Message::text(Role::Assistant, "evidence 🦀 ".repeat(5000)),
        )
        .unwrap();
    agent
        .submit(&mut store, "Arena only; do not commit.")
        .unwrap();
    let original = store.messages(&session).unwrap();
    let mut shorten_count = 0;
    let mut output_count = 0;
    assert!(
        agent
            .compact(&mut store, &mut |event| match event {
                builder::agent::AgentEvent::SummaryRecovery { size, limit } => {
                    assert!(size > limit);
                    shorten_count += 1;
                }
                builder::agent::AgentEvent::OutputRecovery { .. } => output_count += 1,
                _ => (),
            })
            .await
            .unwrap()
    );
    assert_eq!((shorten_count, output_count), (1, 1));
    let requests = server.mock.requests.lock().unwrap();
    assert_eq!(requests.len(), 3);
    assert_eq!(requests[0]["messages"][1], requests[1]["messages"][1]);
    assert_eq!(requests[1]["messages"], requests[2]["messages"]);
    assert_eq!(requests[1]["max_tokens"], 8192);
    assert_eq!(requests[2]["max_tokens"], 16384);
    for request in requests.iter() {
        assert!(request.get("tools").is_none());
        assert!(!request.to_string().contains("oversized draft"));
        assert!(!request.to_string().contains("partial draft"));
        let messages: Vec<Message> = serde_json::from_value(request["messages"].clone()).unwrap();
        assert!(
            builder::agent::estimate_tokens(&messages)
                + request["max_tokens"].as_u64().unwrap() as usize
                <= 81920
        );
    }
    assert_eq!(store.history_messages(&session).unwrap(), original);
    assert!(pending(&store.messages(&session).unwrap()));
    assert_eq!(
        store
            .messages(&session)
            .unwrap()
            .last()
            .unwrap()
            .content
            .as_deref(),
        Some("Arena only; do not commit.")
    );
}

#[tokio::test]
async fn empty_and_reasoning_only_responses_preserve_pending_turn_without_blind_retries() {
    for stream in [true, false] {
        for reasoning in [None, Some("reasoning_content"), Some("reasoning")] {
            let mut payload = json!({"role":"assistant", "content":" \n"});
            if let Some(key) = reasoning {
                payload[key] = json!("private reasoning");
            }
            let body = if stream {
                format!("{}{}", delta(payload), finish("stop"))
            } else {
                json!({"choices":[{"message":payload,"finish_reason":"stop"}]}).to_string()
            };
            let mut server = server(vec![(StatusCode::OK, body)]).await;
            server.profile.stream = stream;
            server.profile.extra_body.insert(
                "chat_template_kwargs".into(),
                json!({"enable_thinking":false}),
            );
            let home = tempfile::tempdir().unwrap();
            let mut store = Store::open(home.path()).unwrap();
            let session = store
                .create("empty", "local", home.path(), "system")
                .unwrap();
            let agent = Agent {
                memory: None,
                provider: OpenAiCompatible::new(server.profile.clone()).unwrap(),
                profile: server.profile.clone(),
                workspace: Workspace::new(home.path()).unwrap(),
                session: session.clone(),
                approval: ApprovalMode::Trust,
                max_rounds: 4,
            };
            agent.submit(&mut store, "make a change").unwrap();
            let before = serde_json::to_value(store.messages(&session).unwrap()).unwrap();
            let mut visible = String::new();
            let error = agent
                .run(
                    &mut store,
                    &mut |event| {
                        if let builder::agent::AgentEvent::Model(Event::Delta(text)) = event {
                            visible.push_str(&text);
                        }
                    },
                    &mut |_| panic!("no tool should execute"),
                )
                .await
                .unwrap_err()
                .to_string();
            assert!(
                error.contains(if reasoning.is_some() {
                    "only reasoning"
                } else {
                    "Empty model response"
                }),
                "{error}"
            );
            assert!(error.contains("before /retry"));
            assert!(!visible.contains("private reasoning"));
            assert_eq!(
                before,
                serde_json::to_value(store.messages(&session).unwrap()).unwrap()
            );
            assert!(pending(&store.messages(&session).unwrap()));
            let requests = server.mock.requests.lock().unwrap();
            assert_eq!(requests.len(), 1);
            assert_eq!(
                requests[0]["chat_template_kwargs"]["enable_thinking"],
                false
            );
        }
    }
}

#[tokio::test]
async fn failed_model_request_during_progress_recovery_preserves_full_context() {
    for reply in [(StatusCode::BAD_REQUEST, "rejected".into())] {
        let server = server(vec![reply]).await;
        let home = tempfile::tempdir().unwrap();
        let mut store = Store::open(home.path()).unwrap();
        let session = store
            .create("recovery failure", "local", home.path(), "system")
            .unwrap();
        seed_investigation(&mut store, &session);
        let original = store.messages(&session).unwrap();
        let agent = Agent {
            memory: None,
            provider: OpenAiCompatible::new(server.profile.clone()).unwrap(),
            profile: server.profile.clone(),
            workspace: Workspace::new(home.path()).unwrap(),
            session: session.clone(),
            approval: ApprovalMode::Trust,
            max_rounds: 4,
        };
        assert!(
            agent
                .run(&mut store, &mut |_| {}, &mut |_| panic!(
                    "failed request cannot dispatch tools"
                ))
                .await
                .is_err()
        );
        assert_eq!(store.messages(&session).unwrap(), original);
        assert_eq!(store.history_messages(&session).unwrap(), original);
        assert!(store.archived_messages(&session).unwrap().is_empty());
        assert!(pending(&original));
        assert!(!home.path().join("bad.txt").exists());
        assert_eq!(server.mock.requests.lock().unwrap().len(), 1);
    }
}

#[tokio::test]
async fn recovery_resolves_queued_batch_and_retains_denials_without_granting_permission() {
    let mut write = tool_message();
    write.tool_calls[0].id = "after_recovery".into();
    write.tool_calls[0].function.arguments =
        r#"{"path":"result.txt","content":"different proposed content"}"#.into();
    let server = server(vec![
        (StatusCode::OK, format!("{}{}", delta(json!({"tool_calls":[{
            "index":0,"id":"after_recovery","type":"function","function":write.tool_calls[0].function
        }]})), finish("tool_calls"))),
        text_reply("The write was denied; implementation remains unfinished."),
    ]).await;
    let home = tempfile::tempdir().unwrap();
    let mut store = Store::open(home.path()).unwrap();
    let session = store
        .create("recovery permission", "local", home.path(), "system")
        .unwrap();
    seed_investigation(&mut store, &session);
    // This queued mutation must resolve before recovery guidance is generated.
    store.append(&session, &tool_message()).unwrap();
    let agent = Agent {
        memory: None,
        provider: OpenAiCompatible::new(server.profile.clone()).unwrap(),
        profile: server.profile.clone(),
        workspace: Workspace::new(home.path()).unwrap(),
        session: session.clone(),
        approval: ApprovalMode::ReadOnly,
        max_rounds: 5,
    };
    agent
        .run(&mut store, &mut |_| {}, &mut |_| panic!("read-only policy"))
        .await
        .unwrap();
    assert!(
        store
            .tool_result(&session, "write_1")
            .unwrap()
            .unwrap()
            .starts_with("DENIED:")
    );
    assert!(
        store
            .tool_result(&session, "after_recovery")
            .unwrap()
            .unwrap()
            .starts_with("DENIED:")
    );
    assert!(!home.path().join("result.txt").exists());
    let requests = server.mock.requests.lock().unwrap();
    assert!(requests[0]["messages"].to_string().contains("DENIED:"));
    assert!(
        requests[1]["messages"][0]["content"]
            .as_str()
            .unwrap()
            .contains("Runtime progress check")
    );
    assert!(store.archived_messages(&session).unwrap().is_empty());
    assert!(!pending(&store.messages(&session).unwrap()));
}

#[tokio::test]
async fn recovery_restricts_broad_discovery_and_rejects_unknown_batches() {
    for outcome in ["edit", "answer", "unavailable"] {
        let mut replies = vec![];
        for i in 0..4 {
            replies.push((
                StatusCode::OK,
                format!(
                    "{}{}",
                    delta(json!({"tool_calls":[{
                        "index":0,"id":format!("decision_read_{i}"),"type":"function",
                        "function":{"name":"read_file","arguments":"{\"path\":\"rate.ts\"}"}
                    }]})),
                    finish("tool_calls")
                ),
            ));
        }
        if outcome == "answer" {
            replies.push(text_reply(
                "I can explain the findings, but need the requested specification before editing.",
            ));
        } else {
            let mut calls = vec![json!({"index":0,"id":"decision_edit","type":"function",
                "function":{"name":"edit_file","arguments":json!({"path":"rate.ts","old":"old rate","new":"new rate"}).to_string()}})];
            if outcome == "unavailable" {
                calls.push(json!({"index":1,"id":"unavailable_read","type":"function",
                    "function":{"name":"unknown_tool","arguments":"{\"path\":\"rate.ts\"}"}}));
            }
            replies.push((
                StatusCode::OK,
                format!(
                    "{}{}",
                    delta(json!({"tool_calls":calls})),
                    finish("tool_calls")
                ),
            ));
            if outcome == "edit" {
                replies.push(text_reply("Changed the rate."));
            }
        }
        let server = server(replies).await;
        let home = tempfile::tempdir().unwrap();
        std::fs::write(home.path().join("rate.ts"), "old rate").unwrap();
        let mut store = Store::open(home.path()).unwrap();
        let session = store
            .create("decision", "local", home.path(), "system")
            .unwrap();
        seed_investigation(&mut store, &session);
        let agent = Agent {
            memory: None,
            provider: OpenAiCompatible::new(server.profile.clone()).unwrap(),
            profile: server.profile.clone(),
            workspace: Workspace::new(home.path()).unwrap(),
            session: session.clone(),
            approval: ApprovalMode::Trust,
            max_rounds: 12,
        };
        let result = agent.run(&mut store, &mut |_| {}, &mut |_| true).await;
        let requests = server.mock.requests.lock().unwrap();
        let decision_tools: Vec<_> = requests[4]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["function"]["name"].as_str().unwrap())
            .collect();
        assert_eq!(
            decision_tools,
            ["read_file", "write_file", "edit_file", "shell", "research"]
        );
        if outcome == "unavailable" {
            assert!(result.unwrap_err().to_string().contains("unavailable"));
            assert!(
                !store
                    .used_call_ids(&session)
                    .unwrap()
                    .contains("decision_edit")
            );
            assert!(pending(&store.messages(&session).unwrap()));
        } else {
            result.unwrap();
            assert!(!pending(&store.messages(&session).unwrap()));
        }
        assert_eq!(
            std::fs::read_to_string(home.path().join("rate.ts")).unwrap(),
            if outcome == "edit" {
                "new rate"
            } else {
                "old rate"
            }
        );
        if outcome == "edit" {
            assert_eq!(
                requests[5]["tools"].as_array().unwrap().len(),
                7,
                "inspection must return after a successful edit"
            );
        }
    }
}

#[tokio::test]
async fn invalid_research_loop_recovers_after_restart_and_has_a_hard_bound() {
    for recovers in [true, false] {
        let invalid = |id: &str| -> Message {
            serde_json::from_value(json!({"role":"assistant","content":null,"tool_calls":[{
                "id":id,"type":"function","function":{"name":"research","arguments":json!({"request":{"operation":"finish","outcome":"verified","verification_ids":["shell1"]}}).to_string()}
            }]})).unwrap()
        };
        let mut replies = vec![];
        if recovers {
            replies.push(text_reply("The change and checks completed."));
        } else {
            for index in 3..6 {
                let call = invalid(&format!("bad{index}")).tool_calls.remove(0);
                replies.push((StatusCode::OK, format!("{}{}", delta(json!({"tool_calls":[{"index":0,"id":call.id,"type":"function","function":call.function}]})), finish("tool_calls"))));
            }
        }
        let server = server(replies).await;
        let home = tempfile::tempdir().unwrap();
        let mut store = Store::open(home.path()).unwrap();
        let session = store
            .create("failure recovery", "local", home.path(), "system")
            .unwrap();
        store
            .append(
                &session,
                &Message::text(Role::User, "Report completed work"),
            )
            .unwrap();
        for index in 0..3 {
            let id = format!("bad{index}");
            store.append(&session, &invalid(&id)).unwrap();
            store.claim_tool(&session, &id).unwrap();
            store
                .complete_tool_with_outcome(
                    &session,
                    &id,
                    "ERROR: Invalid tool request: unknown field verification_ids",
                    builder_core::store::ToolOutcome::Failed,
                )
                .unwrap();
        }
        let original = store.history_messages(&session).unwrap();
        drop(store);
        let mut store = Store::open(home.path()).unwrap();
        let agent = Agent {
            memory: None,
            provider: OpenAiCompatible::new(server.profile.clone()).unwrap(),
            profile: server.profile.clone(),
            workspace: Workspace::new(home.path()).unwrap(),
            session: session.clone(),
            approval: ApprovalMode::ReadOnly,
            max_rounds: 12,
        };
        let result = agent
            .run(&mut store, &mut |_| {}, &mut |_| {
                panic!("invalid requests cannot execute")
            })
            .await;
        if recovers {
            result.unwrap();
        } else {
            assert!(
                result
                    .unwrap_err()
                    .to_string()
                    .contains("Tool recovery exhausted")
            );
        }
        assert_eq!(pending(&store.messages(&session).unwrap()), !recovers);
        let history = store.history_messages(&session).unwrap();
        assert_eq!(
            serde_json::to_value(&history[..original.len()]).unwrap(),
            serde_json::to_value(original).unwrap()
        );
        let requests = server.mock.requests.lock().unwrap();
        assert_eq!(requests.len(), if recovers { 1 } else { 3 });
        assert!(requests[0]["tools"].is_array());
        assert!(
            requests[0]["messages"]
                .to_string()
                .contains("Runtime tool failure recovery")
        );
    }
}

#[tokio::test]
async fn research_results_expose_the_durable_id_for_followup_evidence() {
    let server = server(vec![(StatusCode::OK, format!("{}{}", delta(json!({"tool_calls":[{
        "index":0,"id":"observation_1","type":"function","function":{"name":"research","arguments":json!({"request":{"operation":"observe","artifacts":[{"path":"contract.json","selector":{"kind":"file"}}]}}).to_string()}
    }]})), finish("tool_calls"))), text_reply("Observed the contract.")]).await;
    let home = tempfile::tempdir().unwrap();
    std::fs::write(home.path().join("contract.json"), "{\"optional\":true}").unwrap();
    let workspace = Workspace::new(home.path()).unwrap();
    let mut store = Store::open(home.path()).unwrap();
    let session = store
        .create("evidence ids", "local", workspace.root(), "system")
        .unwrap();
    let agent = Agent {
        provider: OpenAiCompatible::new(server.profile.clone()).unwrap(),
        memory: None,
        profile: server.profile.clone(),
        workspace,
        session: session.clone(),
        approval: ApprovalMode::ReadOnly,
        max_rounds: 3,
    };
    agent.submit(&mut store, "Inspect the contract").unwrap();
    agent
        .run(&mut store, &mut |_| {}, &mut |_| false)
        .await
        .unwrap();
    let requests = server.mock.requests.lock().unwrap().clone();
    let result = requests[1]["messages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["tool_call_id"] == "observation_1")
        .unwrap();
    let value: Value = serde_json::from_str(result["content"].as_str().unwrap()).unwrap();
    let id = value["record_id"].as_str().unwrap();
    assert_eq!(id, format!("{session}:observation_1"));
    let hypothesis = builder_core::research::Request::Hypothesis {
        claim: "The contract permits an optional value".into(),
        falsification: "The optional flag is false".into(),
        evidence_ids: vec![id.into()],
    };
    assert!(
        builder::research::execute(
            &agent.provider,
            &store,
            &session,
            &agent.workspace,
            &hypothesis,
            agent.profile.context_tokens
        )
        .await
        .is_ok()
    );
    std::fs::write(home.path().join("contract.json"), "{\"optional\":false}").unwrap();
    assert!(
        builder::research::execute(
            &agent.provider,
            &store,
            &session,
            &agent.workspace,
            &hypothesis,
            agent.profile.context_tokens
        )
        .await
        .is_err()
    );
}

#[tokio::test]
async fn successful_memory_loop_recovers_or_concludes_with_originals_retained() {
    for recovers in [true, false] {
        let memory_call = |id: &str, query: &str| -> Message {
            serde_json::from_value(json!({"role":"assistant","tool_calls":[{
                "id":id,"type":"function","function":{"name":"memory_search","arguments":json!({"query":query}).to_string()}
            }]})).unwrap()
        };
        let mut replies = vec![];
        if recovers {
            let call = read_message("verify_source", json!({"path":"rate.ts"}))
                .tool_calls
                .remove(0);
            replies.push((StatusCode::OK, format!("{}{}", delta(json!({"tool_calls":[{"index":0,"id":call.id,"type":"function","function":call.function}]})), finish("tool_calls"))));
            replies.push(text_reply(
                "Yes: current rate.ts sets the Arena multiplier to 0.25.",
            ));
        } else {
            for index in 0..7 {
                let call =
                    memory_call(&format!("again{index}"), &format!("shield wording {index}"))
                        .tool_calls
                        .remove(0);
                replies.push((StatusCode::OK, format!("{}{}", delta(json!({"tool_calls":[{"index":0,"id":call.id,"type":"function","function":call.function}]})), finish("tool_calls"))));
            }
            replies.push(text_reply(
                "I could not verify more than the recorded stale memory hints.",
            ));
        }
        let server = server(replies).await;
        let home = tempfile::tempdir().unwrap();
        std::fs::write(
            home.path().join("rate.ts"),
            "const arenaMultiplier = 0.25;\n",
        )
        .unwrap();
        let mut store = Store::open(home.path()).unwrap();
        let session = store
            .create("memory loop", "local", home.path(), "system")
            .unwrap();
        store
            .append(
                &session,
                &Message::text(
                    Role::User,
                    "Was the Arena shield change implemented? Do not edit files.",
                ),
            )
            .unwrap();
        for index in 0..12 {
            let id = format!("old{index}");
            store
                .append(
                    &session,
                    &memory_call(&id, &format!("arena shield {index}")),
                )
                .unwrap();
            store.claim_tool(&session, &id).unwrap();
            store.complete_tool(&session, &id, r#"{"notes":[{"freshness":"stale_or_rewound","paths":["rate.ts"]}],"retrieval":"lexical"}"#).unwrap();
        }
        let original = store.history_messages(&session).unwrap();
        drop(store);
        let mut store = Store::open(home.path()).unwrap();
        let agent = Agent {
            memory: Some(builder::memory::MemoryRuntime::lexical()),
            provider: OpenAiCompatible::new(server.profile.clone()).unwrap(),
            profile: server.profile.clone(),
            workspace: Workspace::new(home.path()).unwrap(),
            session: session.clone(),
            approval: ApprovalMode::ReadOnly,
            max_rounds: 20,
        };
        agent
            .run(&mut store, &mut |_| {}, &mut |_| {
                panic!("read-only question")
            })
            .await
            .unwrap();
        assert!(!pending(&store.messages(&session).unwrap()));
        assert_eq!(
            &store.history_messages(&session).unwrap()[..original.len()],
            original
        );
        assert!(store.archived_messages(&session).unwrap().is_empty());
        assert_eq!(
            std::fs::read_to_string(home.path().join("rate.ts")).unwrap(),
            "const arenaMultiplier = 0.25;\n"
        );
        let requests = server.mock.requests.lock().unwrap();
        assert_eq!(requests.len(), if recovers { 2 } else { 8 });
        assert!(
            requests[0]["messages"]
                .to_string()
                .contains("stop rephrasing the query")
        );
        if !recovers {
            assert!(
                requests.last().unwrap()["messages"]
                    .to_string()
                    .contains("enforced conclusion round")
            );
            assert!(
                requests.last().unwrap()["tools"]
                    .as_array()
                    .is_none_or(Vec::is_empty)
            );
        }
        if recovers {
            assert!(
                store
                    .tool_result(&session, "verify_source")
                    .unwrap()
                    .unwrap()
                    .contains("0.25")
            );
        }
    }
}

#[tokio::test]
async fn recorded_outcomes_distinguish_real_edits_noops_and_execution_errors() {
    use builder_core::store::ToolOutcome;
    let mut first = tool_message().tool_calls.remove(0);
    first.id = "changed".into();
    let mut same = first.clone();
    same.id = "unchanged".into();
    let missing = read_message("missing", json!({"path":"missing.txt"}))
        .tool_calls
        .remove(0);
    let mut replies = vec![];
    for call in [first, same, missing] {
        replies.push((
            StatusCode::OK,
            format!(
                "{}{}",
                delta(json!({"tool_calls":[{
                    "index":0,"id":call.id,"type":"function","function":call.function
                }]})),
                finish("tool_calls")
            ),
        ));
    }
    replies.push(text_reply(
        "The edit was written once; the missing file could not be read.",
    ));
    let server = server(replies).await;
    let home = tempfile::tempdir().unwrap();
    let mut store = Store::open(home.path()).unwrap();
    let session = store
        .create("outcomes", "local", home.path(), "system")
        .unwrap();
    let agent = Agent {
        memory: None,
        provider: OpenAiCompatible::new(server.profile.clone()).unwrap(),
        profile: server.profile.clone(),
        workspace: Workspace::new(home.path()).unwrap(),
        session: session.clone(),
        approval: ApprovalMode::Trust,
        max_rounds: 4,
    };
    agent.submit(&mut store, "Write the file").unwrap();
    agent
        .run(&mut store, &mut |_| {}, &mut |_| false)
        .await
        .unwrap();
    drop(store);
    let store = Store::open(home.path()).unwrap();
    let outcomes = store.tool_outcomes(&session).unwrap();
    assert_eq!(outcomes["changed"], ToolOutcome::Changed);
    assert_eq!(outcomes["unchanged"], ToolOutcome::Succeeded);
    assert_eq!(outcomes["missing"], ToolOutcome::Failed);
    assert!(!pending(&store.messages(&session).unwrap()));
}

#[tokio::test]
async fn productive_work_passes_twenty_rounds_and_budget_pause_resumes_pending_call_once() {
    let mut replies = Vec::new();
    for index in 0..21 {
        replies.push((StatusCode::OK, format!("{}{}", delta(json!({"tool_calls":[{
            "index":0,"id":format!("write-{index}"),"type":"function",
            "function":{"name":"write_file","arguments":json!({"path":"progress.txt","content":index.to_string()}).to_string()}
        }]})), finish("tool_calls"))));
    }
    replies.push(text_reply("Completed the requested writes."));
    let server = server(replies).await;
    let home = tempfile::tempdir().unwrap();
    let mut store = Store::open(home.path()).unwrap();
    let session = store
        .create("budget", "local", home.path(), "system")
        .unwrap();
    let agent = Agent {
        memory: None,
        provider: OpenAiCompatible::new(server.profile.clone()).unwrap(),
        profile: server.profile.clone(),
        workspace: Workspace::new(home.path()).unwrap(),
        session: session.clone(),
        approval: ApprovalMode::Trust,
        max_rounds: 21,
    };
    agent
        .submit(&mut store, "Perform the requested writes and verify.")
        .unwrap();
    let error = agent
        .run(&mut store, &mut |_| {}, &mut |_| false)
        .await
        .unwrap_err();
    assert!(error.to_string().contains("configured budget of 21"));
    assert_eq!(server.mock.requests.lock().unwrap().len(), 21);
    assert_eq!(
        std::fs::read_to_string(home.path().join("progress.txt")).unwrap(),
        "19"
    );
    assert!(pending(&store.messages(&session).unwrap()));
    assert!(
        !store
            .tool_outcomes(&session)
            .unwrap()
            .contains_key("write-20")
    );
    assert!(
        server.mock.requests.lock().unwrap().last().unwrap()["messages"]
            .as_array()
            .unwrap()
            .iter()
            .any(|m| m["content"]
                .as_str()
                .is_some_and(|s| s.contains("1 model rounds remain")))
    );
    drop(store);
    let mut store = Store::open(home.path()).unwrap();
    agent
        .run(&mut store, &mut |_| {}, &mut |_| false)
        .await
        .unwrap();
    assert_eq!(
        std::fs::read_to_string(home.path().join("progress.txt")).unwrap(),
        "20"
    );
    assert_eq!(store.tool_outcomes(&session).unwrap().len(), 21);
    assert!(!pending(&store.messages(&session).unwrap()));
    agent
        .run(&mut store, &mut |_| {}, &mut |_| false)
        .await
        .unwrap();
    assert_eq!(server.mock.requests.lock().unwrap().len(), 22);
}
