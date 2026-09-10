use builder::{
    agent::{Agent, ApprovalMode, SYSTEM},
    memory::MemoryRuntime,
};
use builder_core::{
    config::Profile,
    protocol::{Function, Message, Role, ToolCall},
    store::Store,
};
use builder_provider::{Event, Provider};
use builder_tools::{Action, Workspace};
use serde_json::{Value, json};
use std::sync::Mutex;

struct Replies {
    replies: Mutex<Vec<Message>>,
    requests: Mutex<Vec<Vec<Message>>>,
}
impl Provider for Replies {
    async fn complete(
        &self,
        messages: &[Message],
        _: &[Value],
        _: &mut dyn FnMut(Event),
    ) -> anyhow::Result<Message> {
        self.requests.lock().unwrap().push(messages.to_vec());
        let mut replies = self.replies.lock().unwrap();
        anyhow::ensure!(!replies.is_empty(), "unexpected model call");
        Ok(replies.remove(0))
    }
}
async fn read(store: &mut Store, session: &str, workspace: &Workspace, id: &str) {
    let mut message = Message::text(Role::Assistant, "");
    message.tool_calls.push(ToolCall {
        id: id.into(),
        kind: "function".into(),
        function: Function {
            name: "read_file".into(),
            arguments: json!({"path":"rates.json"}).to_string(),
        },
    });
    store.append(session, &message).unwrap();
    store.claim_tool(session, id).unwrap();
    let output = workspace
        .execute(&Action::ReadFile {
            path: "rates.json".into(),
            start_line: None,
            end_line: None,
        })
        .await
        .unwrap();
    store.complete_tool(session, id, &output).unwrap();
}
fn save(id: &str, expected_revision: i64) -> Action {
    Action::MemoryUpsert {
        key: "shield".into(),
        text: "shield_chance is 0.025".into(),
        expected_revision,
        evidence_call_ids: vec![id.into()],
    }
}

#[tokio::test]
async fn findings_refresh_after_source_changes_and_cannot_cross_workspaces_or_rewinds() {
    let home = tempfile::tempdir().unwrap();
    let other = tempfile::tempdir().unwrap();
    std::fs::write(home.path().join("rates.json"), "{\"shield_chance\":0.025}").unwrap();
    let workspace = Workspace::new(home.path()).unwrap();
    let mut store = Store::open(home.path()).unwrap();
    let session = store.create("test", "local", home.path(), SYSTEM).unwrap();
    store
        .append(&session, &Message::text(Role::User, "fix shield"))
        .unwrap();
    let memory = MemoryRuntime::lexical();
    read(&mut store, &session, &workspace, "read1").await;
    memory
        .execute(&mut store, &session, &workspace, &save("read1", 0))
        .await
        .unwrap();
    let result = memory.search(&store, &workspace, "shield").await.unwrap();
    assert!(result.to_string().contains("0.025"));
    assert!(
        memory
            .search(&store, &Workspace::new(other.path()).unwrap(), "shield")
            .await
            .unwrap()["notes"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    std::fs::write(home.path().join("rates.json"), "{\"shield_chance\":0.1}").unwrap();
    let stale = memory
        .search(&store, &workspace, "shield")
        .await
        .unwrap()
        .to_string();
    assert!(stale.contains("stale_or_rewound"));
    assert!(!stale.contains("0.025"));
    assert!(
        memory
            .execute(&mut store, &session, &workspace, &save("read1", 1))
            .await
            .is_err()
    );
    read(&mut store, &session, &workspace, "read2").await;
    memory
        .execute(
            &mut store,
            &session,
            &workspace,
            &Action::MemoryUpsert {
                key: "shield".into(),
                text: "shield_chance is 0.1".into(),
                expected_revision: 1,
                evidence_call_ids: vec!["read2".into()],
            },
        )
        .await
        .unwrap();
    assert!(
        memory
            .search(&store, &workspace, "shield")
            .await
            .unwrap()
            .to_string()
            .contains("0.1")
    );
    store.rewind(&session).unwrap();
    assert!(
        !memory
            .search(&store, &workspace, "shield")
            .await
            .unwrap()
            .to_string()
            .contains("shield_chance is")
    );
}

#[tokio::test]
async fn automatic_extraction_persists_findings_and_task_and_rejects_invented_evidence() {
    let home = tempfile::tempdir().unwrap();
    std::fs::write(home.path().join("rates.json"), "{\"shield_chance\":0.025}").unwrap();
    let workspace = Workspace::new(home.path()).unwrap();
    let mut store = Store::open(home.path()).unwrap();
    let session = store.create("test", "local", home.path(), SYSTEM).unwrap();
    store
        .append(&session, &Message::text(Role::User, "fix shield"))
        .unwrap();
    read(&mut store, &session, &workspace, "read1").await;
    let response = |id: &str| {
        Message::text(Role::Assistant,json!({"findings":[{"key":"shield","text":"shield_chance is 0.025","evidence_call_ids":[id]}],"next_action":"change shield chance","questions":[]}).to_string())
    };
    let provider = Replies {
        replies: Mutex::new(vec![response("invented"), response("read1")]),
        requests: Mutex::new(vec![]),
    };
    let memory = MemoryRuntime::lexical();
    assert!(
        memory
            .extract(
                &provider,
                &mut store,
                &session,
                &workspace,
                true,
                (32768, &mut |_| {})
            )
            .await
            .is_err()
    );
    assert!(
        store
            .memory_list(&MemoryRuntime::scope(&workspace))
            .unwrap()
            .is_empty()
    );
    // The failed batch is not committed and the cursor does not advance, so a
    // fresh runtime (restart) reconsiders the same evidence instead of
    // permanently skipping it.
    let memory = MemoryRuntime::lexical();
    assert!(
        memory
            .extract(
                &provider,
                &mut store,
                &session,
                &workspace,
                true,
                (32768, &mut |_| {})
            )
            .await
            .unwrap()
    );
    assert_eq!(provider.requests.lock().unwrap().len(), 2);
    read(&mut store, &session, &workspace, "read1_fresh").await;
    *provider.replies.lock().unwrap() = vec![response("read1_fresh")];
    // A later idle pass (fresh runtime) extracts the new evidence batch.
    let memory = MemoryRuntime::lexical();
    assert!(
        memory
            .extract(
                &provider,
                &mut store,
                &session,
                &workspace,
                true,
                (32768, &mut |_| {})
            )
            .await
            .unwrap()
    );
    drop(store);
    let store = Store::open(home.path()).unwrap();
    assert_eq!(
        store.memory_task(&session).unwrap().unwrap().next_action,
        "change shield chance"
    );
    assert!(
        memory
            .packet(&store, &session, &workspace)
            .await
            .unwrap()
            .content
            .unwrap()
            .contains("0.025")
    );
}

#[tokio::test]
async fn malformed_extraction_output_is_not_saved_and_is_recovered_on_retry() {
    let home = tempfile::tempdir().unwrap();
    std::fs::write(home.path().join("rates.json"), "{\"shield_chance\":0.025}").unwrap();
    let workspace = Workspace::new(home.path()).unwrap();
    let mut store = Store::open(home.path()).unwrap();
    let session = store.create("test", "local", home.path(), SYSTEM).unwrap();
    store
        .append(&session, &Message::text(Role::User, "fix shield"))
        .unwrap();
    read(&mut store, &session, &workspace, "read1").await;
    let fenced = format!(
        "I extracted the following.\n```json\n{}\n```",
        json!({"findings":[{"key":"shield","text":"shield_chance is 0.025","evidence_call_ids":["read1"]}],"next_action":"change shield chance","questions":[]})
    );
    let provider = Replies {
        replies: Mutex::new(vec![
            Message::text(Role::Assistant, "Here is a summary of the session."),
            Message::text(Role::Assistant, fenced),
        ]),
        requests: Mutex::new(vec![]),
    };
    let memory = MemoryRuntime::lexical();
    assert!(
        memory
            .extract(
                &provider,
                &mut store,
                &session,
                &workspace,
                true,
                (32768, &mut |_| {})
            )
            .await
            .is_err()
    );
    assert!(
        store
            .memory_list(&MemoryRuntime::scope(&workspace))
            .unwrap()
            .is_empty()
    );
    assert!(store.memory_task(&session).unwrap().is_none());
    // After a restart the same batch is reconsidered, and prose-wrapped JSON
    // is recovered instead of discarded.
    let memory = MemoryRuntime::lexical();
    assert!(
        memory
            .extract(
                &provider,
                &mut store,
                &session,
                &workspace,
                true,
                (32768, &mut |_| {})
            )
            .await
            .unwrap()
    );
    assert_eq!(provider.requests.lock().unwrap().len(), 2);
    assert_eq!(
        store.memory_task(&session).unwrap().unwrap().next_action,
        "change shield chance"
    );
}

#[tokio::test]
async fn memory_is_loaded_automatically_and_new_user_correction_supersedes_old_task() {
    let home = tempfile::tempdir().unwrap();
    std::fs::write(home.path().join("rates.json"), "{\"shield_chance\":0.025}").unwrap();
    let workspace = Workspace::new(home.path()).unwrap();
    let mut store = Store::open(home.path()).unwrap();
    let session = store.create("test", "local", home.path(), SYSTEM).unwrap();
    store
        .append(&session, &Message::text(Role::User, "fix shield"))
        .unwrap();
    read(&mut store, &session, &workspace, "read1").await;
    let memory = MemoryRuntime::lexical();
    memory
        .execute(&mut store, &session, &workspace, &save("read1", 0))
        .await
        .unwrap();
    memory
        .execute(
            &mut store,
            &session,
            &workspace,
            &Action::TaskUpdate {
                next_action: "old task".into(),
                questions: vec![],
            },
        )
        .await
        .unwrap();
    store
        .interrupt_turn(&session, Some("Correction: explain shield without editing"))
        .unwrap();
    let provider = Replies {
        replies: Mutex::new(vec![
            Message::text(Role::Assistant, "Shield chance is 0.025."),
            Message::text(
                Role::Assistant,
                json!({"findings":[],"next_action":"nothing further","questions":[]}).to_string(),
            ),
        ]),
        requests: Mutex::new(vec![]),
    };
    let agent = Agent {
        provider,
        profile: Profile::default(),
        workspace,
        session: session.clone(),
        approval: ApprovalMode::ReadOnly,
        max_rounds: 2,
        memory: Some(memory),
    };
    agent
        .run(&mut store, &mut |_| {}, &mut |_| panic!("read only"))
        .await
        .unwrap();
    let requests = agent.provider.requests.lock().unwrap();
    let request = serde_json::to_string(&requests[0]).unwrap();
    assert!(request.contains("shield_chance is 0.025"));
    assert!(!request.contains("old task"));
    assert!(!store.history_messages(&session).unwrap().iter().any(|m| {
        m.content
            .as_deref()
            .unwrap_or("")
            .contains("Builder memory reference")
    }));
}

#[tokio::test]
async fn eligible_memory_maintenance_never_delays_the_foreground_answer() {
    let home = tempfile::tempdir().unwrap();
    std::fs::write(home.path().join("rates.json"), "{\"shield_chance\":0.025}").unwrap();
    let workspace = Workspace::new(home.path()).unwrap();
    let mut store = Store::open(home.path()).unwrap();
    let session = store.create("test", "local", home.path(), SYSTEM).unwrap();
    store
        .append(&session, &Message::text(Role::User, "clean up comments"))
        .unwrap();
    for index in 0..6 {
        read(&mut store, &session, &workspace, &format!("read{index}")).await;
    }
    let provider = Replies {
        replies: Mutex::new(vec![Message::text(
            Role::Assistant,
            "I inspected the requested files.",
        )]),
        requests: Mutex::new(vec![]),
    };
    let agent = Agent {
        provider,
        memory: Some(MemoryRuntime::lexical()),
        profile: Profile::default(),
        workspace,
        session: session.clone(),
        approval: ApprovalMode::ReadOnly,
        max_rounds: 1,
    };
    agent
        .run(&mut store, &mut |_| {}, &mut |_| false)
        .await
        .unwrap();
    let requests = agent.provider.requests.lock().unwrap();
    assert_eq!(requests.len(), 1);
    assert!(
        !serde_json::to_string(&requests[0])
            .unwrap()
            .contains("Extract reusable")
    );
    assert!(
        store
            .memory_list(&MemoryRuntime::scope(&agent.workspace))
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn memory_forgetting_requires_approval_and_uncertain_calls_are_not_replayed() {
    for uncertain in [false, true] {
        let home = tempfile::tempdir().unwrap();
        std::fs::write(home.path().join("rates.json"), "{\"shield_chance\":0.025}").unwrap();
        let workspace = Workspace::new(home.path()).unwrap();
        let mut store = Store::open(home.path()).unwrap();
        let session = store.create("test", "local", home.path(), SYSTEM).unwrap();
        store
            .append(&session, &Message::text(Role::User, "inspect shields"))
            .unwrap();
        read(&mut store, &session, &workspace, "read1").await;
        let memory = MemoryRuntime::lexical();
        memory
            .execute(&mut store, &session, &workspace, &save("read1", 0))
            .await
            .unwrap();
        let mut call = Message::text(Role::Assistant, "");
        call.tool_calls.push(ToolCall {
            id: "forget1".into(),
            kind: "function".into(),
            function: Function {
                name: "memory_forget".into(),
                arguments: json!({"key":"shield","expected_revision":1}).to_string(),
            },
        });
        store.append(&session, &call).unwrap();
        if uncertain {
            store.claim_tool(&session, "forget1").unwrap();
        }
        let agent = Agent {
            provider: Replies {
                replies: Mutex::new(vec![]),
                requests: Mutex::new(vec![]),
            },
            memory: Some(memory),
            profile: Profile::default(),
            workspace,
            session: session.clone(),
            approval: ApprovalMode::ReadOnly,
            max_rounds: 1,
        };
        assert!(
            agent
                .run(&mut store, &mut |_| {}, &mut |_| panic!(
                    "read-only mode cannot approve"
                ))
                .await
                .is_err()
        );
        let result = store.tool_result(&session, "forget1").unwrap().unwrap();
        assert!(result.contains(if uncertain { "uncertain" } else { "DENIED" }));
        assert!(
            store
                .memory_get(&MemoryRuntime::scope(&agent.workspace), "shield", None)
                .unwrap()
                .is_some()
        );
    }
}

#[tokio::test]
async fn automatic_packet_is_bounded_even_with_large_preferences_and_task() {
    use builder_core::memory::{Memory, MemoryKind, TaskState};
    let home = tempfile::tempdir().unwrap();
    let workspace = Workspace::new(home.path()).unwrap();
    let mut store = Store::open(home.path()).unwrap();
    let session = store.create("test", "local", home.path(), SYSTEM).unwrap();
    store
        .append(&session, &Message::text(Role::User, "continue"))
        .unwrap();
    for n in 0..8 {
        store
            .memory_put(
                "@user",
                0,
                Memory {
                    key: format!("preference{n}"),
                    revision: 0,
                    kind: MemoryKind::Preference,
                    text: "界".repeat(500),
                    evidence: vec![],
                    origin_session: String::new(),
                    origin_seq: 0,
                    created_at: String::new(),
                },
            )
            .unwrap();
    }
    store
        .memory_save_task(
            &session,
            &TaskState {
                next_action: "x".repeat(1200),
                questions: vec!["y".repeat(400); 8],
                source_seq: store.memory_latest_seq(&session).unwrap(),
            },
        )
        .unwrap();
    let before = store.history_messages(&session).unwrap().len();
    let packet = MemoryRuntime::lexical()
        .packet(&store, &session, &workspace)
        .await
        .unwrap()
        .content
        .unwrap();
    assert!(packet.len() <= 6000, "{}", packet.len());
    assert!(packet.contains("originals and all revisions retained"));
    assert_eq!(store.history_messages(&session).unwrap().len(), before);
    assert_eq!(store.memory_list("@user").unwrap().len(), 8);
}

#[tokio::test]
#[ignore = "Uses explicitly configured live embedding endpoint"]
async fn live_embedding_index_survives_restart_and_retrieves_in_another_session() {
    use builder_core::config::Config;
    let mut config = Config::load(std::path::Path::new(
        &std::env::var("BUILDER_LIVE_CONFIG_HOME").unwrap(),
    ))
    .unwrap();
    config.memory.enabled = true;
    config.memory.embedding_backend = builder_core::config::EmbeddingBackend::Remote;
    config.memory.embedding_profile = Some("Nomic Embed (llama.cpp)".into());
    config.memory.query_prefix = "search_query: ".into();
    config.memory.document_prefix = "search_document: ".into();
    let home = tempfile::tempdir().unwrap();
    std::fs::write(home.path().join("rates.json"), "{\"shield_chance\":0.025}").unwrap();
    let workspace = Workspace::new(home.path()).unwrap();
    let mut store = Store::open(home.path()).unwrap();
    let session = store.create("test", "local", home.path(), SYSTEM).unwrap();
    store
        .append(
            &session,
            &Message::text(Role::User, "inspect shield configuration"),
        )
        .unwrap();
    read(&mut store, &session, &workspace, "read1").await;
    let (_, profile) = config.profile(Some("Nomic Embed (llama.cpp)")).unwrap();
    let vector = builder_provider::OpenAiCompatible::new(profile)
        .unwrap()
        .embed("search_document: shield chance is 0.025")
        .await
        .expect("Live embedding endpoint must return a valid vector");
    eprintln!("Embedding dimension: {}", vector.len());
    let memory = MemoryRuntime::from_config(&config).unwrap().unwrap();
    memory
        .execute(&mut store, &session, &workspace, &save("read1", 0))
        .await
        .unwrap();
    memory.index_pending(&mut store, &workspace).await.unwrap();
    drop(store);
    let mut store = Store::open(home.path()).unwrap();
    let memory = MemoryRuntime::from_config(&config).unwrap().unwrap();
    let query = "protective powerup probability";
    let result = memory.search(&store, &workspace, query).await.unwrap();
    assert_eq!(result["retrieval"], "hybrid");
    assert_eq!(result["embedding_unavailable"], false);
    assert_eq!(
        result["notes"][0]["memory"]["text"],
        "shield_chance is 0.025"
    );
    let next = store
        .create("another session", "local", home.path(), SYSTEM)
        .unwrap();
    store
        .append(&next, &Message::text(Role::User, query))
        .unwrap();
    let packet = memory
        .packet(&store, &next, &workspace)
        .await
        .unwrap()
        .content
        .unwrap();
    assert!(packet.contains("shield_chance is 0.025"));
    eprintln!("Live embedding: persisted semantic retrieval and cross-session packet passed");
}

#[tokio::test]
async fn hybrid_indexing_is_durable_and_endpoint_failure_falls_back_without_losing_notes() {
    use axum::{Json, Router, http::StatusCode, routing::post};
    use builder_core::config::{Config, ModelRole};
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };
    let unavailable = Arc::new(AtomicBool::new(false));
    let flag = unavailable.clone();
    let app = Router::new().route(
        "/v1/embeddings",
        post(move || {
            let flag = flag.clone();
            async move {
                if flag.load(Ordering::SeqCst) {
                    (
                        StatusCode::BAD_GATEWAY,
                        Json(json!({"error":"private gateway message"})),
                    )
                } else {
                    (
                        StatusCode::OK,
                        Json(json!({"data":[{"index":0,"embedding":[1.0,0.25]}]})),
                    )
                }
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let profile = Profile {
        base_url: format!("http://{}/v1", listener.local_addr().unwrap()),
        roles: vec![ModelRole::Embed],
        ..Default::default()
    };
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let mut config = Config::default();
    config.profiles.insert("embedding-test".into(), profile);
    config.memory.enabled = true;
    config.memory.embedding_backend = builder_core::config::EmbeddingBackend::Remote;
    config.memory.embedding_profile = Some("embedding-test".into());
    let home = tempfile::tempdir().unwrap();
    std::fs::write(home.path().join("rates.json"), "{\"shield_chance\":0.025}").unwrap();
    let workspace = Workspace::new(home.path()).unwrap();
    let mut store = Store::open(home.path()).unwrap();
    let session = store.create("test", "local", home.path(), SYSTEM).unwrap();
    store
        .append(&session, &Message::text(Role::User, "inspect shield"))
        .unwrap();
    read(&mut store, &session, &workspace, "read1").await;
    let runtime = MemoryRuntime::from_config(&config).unwrap().unwrap();
    runtime
        .execute(&mut store, &session, &workspace, &save("read1", 0))
        .await
        .unwrap();
    runtime.index_pending(&mut store, &workspace).await.unwrap();
    drop(store);
    let mut store = Store::open(home.path()).unwrap();
    let runtime = MemoryRuntime::from_config(&config).unwrap().unwrap();
    let result = runtime
        .search(&store, &workspace, "protective powerup probability")
        .await
        .unwrap();
    assert_eq!(result["notes"][0]["memory"]["key"], "shield");
    assert_eq!(result["retrieval"], "hybrid");
    unavailable.store(true, Ordering::SeqCst);
    let runtime = MemoryRuntime::from_config(&config).unwrap().unwrap();
    let result = runtime.search(&store, &workspace, "shield").await.unwrap();
    assert_eq!(result["retrieval"], "lexical");
    assert_eq!(result["notes"][0]["memory"]["key"], "shield");
    assert_eq!(result["embedding_unavailable"], true);
    assert!(!result.to_string().contains("private gateway message"));
    // Changed model revision leaves the record pending for reindexing.
    config.memory.embedding_revision = "new-weights".into();
    let runtime = MemoryRuntime::from_config(&config).unwrap().unwrap();
    assert!(runtime.index_pending(&mut store, &workspace).await.is_err());
    assert!(
        store
            .memory_get(&MemoryRuntime::scope(&workspace), "shield", None)
            .unwrap()
            .is_some()
    );
    unavailable.store(false, Ordering::SeqCst);
    let runtime = MemoryRuntime::from_config(&config).unwrap().unwrap();
    runtime.index_pending(&mut store, &workspace).await.unwrap();
    assert_eq!(
        runtime
            .search(&store, &workspace, "protective powerup probability")
            .await
            .unwrap()["notes"][0]["memory"]["key"],
        "shield"
    );
    server.abort();
}

#[tokio::test]
async fn ineligible_batches_do_not_call_extractor_and_finals_do_not_run_maintenance() {
    let home = tempfile::tempdir().unwrap();
    std::fs::write(home.path().join("rates.json"), "old source").unwrap();
    let workspace = Workspace::new(home.path()).unwrap();
    let mut store = Store::open(home.path()).unwrap();
    let session = store
        .create("memory failures", "local", home.path(), SYSTEM)
        .unwrap();
    store
        .append(&session, &Message::text(Role::User, "inspect"))
        .unwrap();
    for index in 0..6 {
        read(&mut store, &session, &workspace, &format!("r{index}")).await;
    }
    std::fs::write(home.path().join("rates.json"), "changed contract").unwrap();
    let provider = Replies {
        replies: Mutex::new(vec![Message::text(Role::Assistant, "Done inspecting.")]),
        requests: Mutex::new(vec![]),
    };
    let memory = MemoryRuntime::lexical();
    assert!(
        !memory
            .extract(
                &provider,
                &mut store,
                &session,
                &workspace,
                false,
                (32768, &mut |_| {})
            )
            .await
            .unwrap()
    );
    assert!(provider.requests.lock().unwrap().is_empty());
    read(&mut store, &session, &workspace, "fresh").await;
    let agent = Agent {
        provider,
        memory: Some(memory),
        profile: Profile::default(),
        workspace,
        session: session.clone(),
        approval: ApprovalMode::ReadOnly,
        max_rounds: 2,
    };
    agent
        .run(&mut store, &mut |_| {}, &mut |_| false)
        .await
        .unwrap();
    assert_eq!(agent.provider.requests.lock().unwrap().len(), 1);
    // Calling run on a completed session also performs no extraction.
    agent
        .run(&mut store, &mut |_| {}, &mut |_| false)
        .await
        .unwrap();
    assert_eq!(agent.provider.requests.lock().unwrap().len(), 1);
    assert!(
        store
            .memory_list(&MemoryRuntime::scope(&agent.workspace))
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn embedding_deadline_opens_circuit_and_preserves_lexical_results() {
    use axum::{Router, routing::post};
    use builder_core::config::{Config, ModelRole};
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    let calls = Arc::new(AtomicUsize::new(0));
    let counter = calls.clone();
    let app = Router::new().route(
        "/v1/embeddings",
        post(move || {
            let counter = counter.clone();
            async move {
                counter.fetch_add(1, Ordering::SeqCst);
                tokio::time::sleep(std::time::Duration::from_secs(30)).await;
                "{}"
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let mut config = Config::default();
    config.profiles.insert(
        "embed".into(),
        Profile {
            base_url: format!("http://{}/v1", listener.local_addr().unwrap()),
            roles: vec![ModelRole::Embed],
            ..Default::default()
        },
    );
    config.memory.enabled = true;
    config.memory.embedding_backend = builder_core::config::EmbeddingBackend::Remote;
    config.memory.embedding_profile = Some("embed".into());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let home = tempfile::tempdir().unwrap();
    std::fs::write(home.path().join("rates.json"), "{\"shield_chance\":0.025}").unwrap();
    let workspace = Workspace::new(home.path()).unwrap();
    let mut store = Store::open(home.path()).unwrap();
    let session = store
        .create("timeout", "local", home.path(), SYSTEM)
        .unwrap();
    read(&mut store, &session, &workspace, "read1").await;
    let runtime = MemoryRuntime::from_config(&config).unwrap().unwrap();
    runtime
        .execute(&mut store, &session, &workspace, &save("read1", 0))
        .await
        .unwrap();
    let start = std::time::Instant::now();
    assert!(runtime.index_pending(&mut store, &workspace).await.is_err());
    let result = runtime.search(&store, &workspace, "shield").await.unwrap();
    assert_eq!(result["retrieval"], "lexical");
    assert_eq!(result["notes"][0]["memory"]["key"], "shield");
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert!(start.elapsed() < std::time::Duration::from_secs(5));
    server.abort();
}

#[tokio::test]
async fn default_local_memory_never_contacts_a_saved_remote_profile() {
    use axum::{Router, routing::post};
    use builder_core::config::{Config, EmbeddingBackend, ModelRole};
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    let calls = Arc::new(AtomicUsize::new(0));
    let counter = calls.clone();
    let app = Router::new().route(
        "/v1/embeddings",
        post(move || {
            counter.fetch_add(1, Ordering::SeqCst);
            async { "remote should not be called" }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let mut config = Config::default();
    config.profiles.insert(
        "old-remote".into(),
        Profile {
            base_url: format!("http://{}/v1", listener.local_addr().unwrap()),
            roles: vec![ModelRole::Embed],
            ..Default::default()
        },
    );
    config.memory.enabled = true;
    config.memory.embedding_profile = Some("old-remote".into());
    assert_eq!(config.memory.embedding_backend, EmbeddingBackend::Local);
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let home = tempfile::tempdir().unwrap();
    std::fs::write(home.path().join("rates.json"), "{\"shield_chance\":0.025}").unwrap();
    let workspace = Workspace::new(home.path()).unwrap();
    let mut store = Store::open(home.path()).unwrap();
    let session = store
        .create("local default", "local", workspace.root(), SYSTEM)
        .unwrap();
    read(&mut store, &session, &workspace, "read1").await;
    let runtime = MemoryRuntime::from_config(&config).unwrap().unwrap();
    runtime
        .execute(&mut store, &session, &workspace, &save("read1", 0))
        .await
        .unwrap();
    assert!(
        runtime
            .index_pending(&mut store, &workspace)
            .await
            .unwrap_err()
            .to_string()
            .contains("not installed")
    );
    let result = runtime.search(&store, &workspace, "shield").await.unwrap();
    assert_eq!(result["retrieval"], "lexical");
    assert_eq!(result["notes"][0]["memory"]["key"], "shield");
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    // Incomplete or corrupt local files must not trigger an implicit download.
    config.memory.local_model_dir = Some(home.path().join("model"));
    std::fs::create_dir_all(config.memory.local_model_dir.as_ref().unwrap()).unwrap();
    std::fs::write(
        config
            .memory
            .local_model_dir
            .as_ref()
            .unwrap()
            .join("tokenizer.json"),
        "bad",
    )
    .unwrap();
    let runtime = MemoryRuntime::from_config(&config).unwrap().unwrap();
    assert!(runtime.index_pending(&mut store, &workspace).await.is_err());
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    server.abort();
}

#[tokio::test]
#[ignore = "Needs the explicitly installed local model; performs no model download or remote embedding request"]
async fn local_semantic_vectors_persist_and_retrieve_offline() {
    use builder_core::config::{Config, EmbeddingBackend};
    let root = std::path::PathBuf::from(std::env::var("BUILDER_LOCAL_MODEL_DIR").unwrap());
    let local = builder_provider::local_embedding::LocalEmbedding::new(Some(root.clone()));
    let vector = local.embed("An automobile is a vehicle.").await.unwrap();
    assert_eq!(vector.len(), 384);
    assert!((vector.iter().map(|x| x * x).sum::<f32>() - 1.0).abs() < 0.001);
    let mut config = Config::default();
    config.memory.enabled = true;
    config.memory.embedding_backend = EmbeddingBackend::Local;
    config.memory.local_model_dir = Some(root);
    // An obsolete profile cannot affect local generation, including its secrets.
    config.memory.embedding_profile = Some("nonexistent remote".into());
    let home = tempfile::tempdir().unwrap();
    std::fs::write(
        home.path().join("rates.json"),
        "A mechanic repairs automobile engines. Bread dough uses flour and yeast.",
    )
    .unwrap();
    let workspace = Workspace::new(home.path()).unwrap();
    let mut store = Store::open(home.path()).unwrap();
    let session = store
        .create("offline", "local", workspace.root(), SYSTEM)
        .unwrap();
    read(&mut store, &session, &workspace, "read1").await;
    let runtime = MemoryRuntime::from_config(&config).unwrap().unwrap();
    for (key, text) in [
        ("vehicle", "A mechanic repairs automobile engines."),
        ("kitchen", "Bread dough uses flour and yeast."),
    ] {
        runtime
            .execute(
                &mut store,
                &session,
                &workspace,
                &Action::MemoryUpsert {
                    key: key.into(),
                    text: text.into(),
                    expected_revision: 0,
                    evidence_call_ids: vec!["read1".into()],
                },
            )
            .await
            .unwrap();
    }
    runtime.index_pending(&mut store, &workspace).await.unwrap();
    drop(runtime);
    drop(store);
    let store = Store::open(home.path()).unwrap();
    let runtime = MemoryRuntime::from_config(&config).unwrap().unwrap();
    let result = runtime
        .search(&store, &workspace, "fixing a broken car motor")
        .await
        .unwrap();
    assert_eq!(result["retrieval"], "hybrid");
    assert_eq!(result["embedding_unavailable"], false);
    assert_eq!(result["notes"][0]["memory"]["key"], "vehicle");
    eprintln!(
        "Offline CPU inference: 384 normalized dimensions; persisted vectors retrieved the semantically related note after reopen."
    );
}
