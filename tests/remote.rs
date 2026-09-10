use axum::{Json, Router, extract::State, routing::post};
use builder::{
    agent::ApprovalMode,
    remote::{RemoteControl, RemoteOptions},
};
use builder_core::{
    config::{Config, Profile},
    protocol::{Message, Role},
    store::{Store, ToolRunState},
};
use serde_json::{Value, json};
use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
    time::Duration,
};
use tempfile::TempDir;

type ModelState = (Arc<Mutex<Vec<Value>>>, Arc<Mutex<VecDeque<Value>>>);

struct Host {
    home: TempDir,
    workspace: TempDir,
    control: RemoteControl,
    server: tokio::task::JoinHandle<()>,
    model: tokio::task::JoinHandle<()>,
    url: String,
    token: String,
    requests: Arc<Mutex<Vec<Value>>>,
    client: reqwest::Client,
}
impl Drop for Host {
    fn drop(&mut self) {
        self.server.abort();
        self.model.abort();
    }
}
fn answer(text: &str) -> Value {
    json!({"role":"assistant","content":text})
}
fn write_call() -> Value {
    json!({"role":"assistant","tool_calls":[{"id":"write1","type":"function","function":{"name":"write_file","arguments":json!({"path":"result.txt","content":"approved"}).to_string()}}]})
}
impl Host {
    async fn new(responses: Vec<Value>, approval: ApprovalMode) -> Self {
        let home = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let replies = Arc::new(Mutex::new(VecDeque::from(responses)));
        let app = Router::new()
            .route(
                "/v1/chat/completions",
                post(
                    |State((requests, replies)): State<ModelState>,
                     Json(body): Json<Value>| async move {
                        requests.lock().unwrap().push(body);
                        let message = replies
                            .lock()
                            .unwrap()
                            .pop_front()
                            .unwrap_or_else(|| answer("Finished"));
                        if message == "delay" {
                            tokio::time::sleep(Duration::from_secs(30)).await;
                        }
                        let finish = if message.get("tool_calls").is_some() {
                            "tool_calls"
                        } else {
                            "stop"
                        };
                        Json(json!({"choices":[{"message":message,"finish_reason":finish}]}))
                    },
                ),
            )
            .with_state((requests.clone(), replies));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let mut profile = Profile {
            base_url: format!("http://{}/v1", listener.local_addr().unwrap()),
            stream: false,
            max_attempts: 1,
            ..Default::default()
        };
        profile.pipeline.enabled = false;
        let mut config = Config::default();
        config
            .profiles
            .insert(config.default_profile.clone(), profile);
        config.save(home.path()).unwrap();
        let model = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let control = RemoteControl::new(RemoteOptions {
            home: home.path().into(),
            config_home: home.path().into(),
            workspace: workspace.path().into(),
            profile: None,
            approval,
            max_rounds: Some(5),
            origin: url.clone(),
        })
        .unwrap();
        let token = std::fs::read_to_string(control.token_path()).unwrap();
        let app = control.router();
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        Self {
            home,
            workspace,
            control,
            server,
            model,
            url,
            token,
            requests,
            client: reqwest::Client::new(),
        }
    }
    async fn post(&self, path: &str, body: Value) -> reqwest::Response {
        self.client
            .post(format!("{}/api/{path}", self.url))
            .header("x-builder-token", &self.token)
            .header("origin", &self.url)
            .json(&body)
            .send()
            .await
            .unwrap()
    }
    async fn get(&self, path: &str) -> Value {
        let r = self
            .client
            .get(format!("{}/api/{path}", self.url))
            .header("x-builder-token", &self.token)
            .send()
            .await
            .unwrap();
        assert!(r.status().is_success(), "{}", r.status());
        r.json().await.unwrap()
    }
    async fn start(&self, session: Option<&str>) -> Value {
        let r = self.post("run", json!({"request_id":uuid::Uuid::new_v4().to_string(),"session":session,"operation":{"action":"message","prompt":"Do the fixture task"}})).await;
        let status = r.status();
        let value: Value = r.json().await.unwrap();
        assert_eq!(status, 202, "{value}");
        value
    }
    async fn phase(&self, expected: &[&str]) -> Value {
        let last = Mutex::new(Value::Null);
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let value = self.get("status").await;
                if expected.contains(&value["state"]["phase"].as_str().unwrap()) {
                    return value["state"].clone();
                }
                *last.lock().unwrap() = value;
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        })
        .await
        .unwrap_or_else(|_| {
            panic!(
                "Expected remote phase {expected:?}; last status {}",
                last.lock().unwrap()
            )
        })
    }
}

#[tokio::test]
async fn remote_auth_origin_body_limits_and_workspace_scope_fail_closed() {
    let host = Host::new(vec![], ApprovalMode::Ask).await;
    assert_eq!(
        host.client
            .get(format!("{}/api/status", host.url))
            .send()
            .await
            .unwrap()
            .status(),
        401
    );
    assert_eq!(
        host.client
            .post(format!("{}/api/run", host.url))
            .header("x-builder-token", &host.token)
            .header("origin", "https://attacker.example")
            .json(&json!({}))
            .send()
            .await
            .unwrap()
            .status(),
        403
    );
    assert_eq!(
        host.client
            .post(format!("{}/api/run", host.url))
            .header("x-builder-token", &host.token)
            .json(&json!({}))
            .send()
            .await
            .unwrap()
            .status(),
        403
    );
    let oversized = host
        .post("run", json!({"prompt":"x".repeat(140*1024)}))
        .await;
    assert_eq!(oversized.status(), 413);
    let elsewhere = tempfile::tempdir().unwrap();
    let mut store = Store::open(host.home.path()).unwrap();
    let id = store
        .create("private", "local", elsewhere.path(), "secret")
        .unwrap();
    let r = host.post("run",json!({"request_id":uuid::Uuid::new_v4().to_string(),"session":id,"operation":{"action":"retry"}})).await;
    assert_eq!(r.status(), 409);
    assert!(
        host.get("sessions").await["sessions"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    let response = host
        .client
        .get(format!("{}/api/sessions/{id}/messages", host.url))
        .header("x-builder-token", &host.token)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 409);
    assert!(host.requests.lock().unwrap().is_empty());
    let response = host.client.get(&host.url).send().await.unwrap();
    assert_eq!(response.headers()["cache-control"], "no-store");
    assert!(
        response.headers()["content-security-policy"]
            .to_str()
            .unwrap()
            .contains("frame-ancestors 'none'")
    );
}

#[tokio::test]
async fn browser_approval_is_single_use_and_reconnect_does_not_replay() {
    let host = Host::new(vec![write_call(), answer("Saved")], ApprovalMode::Ask).await;
    let run = host.start(None).await;
    let state = host.phase(&["awaiting_approval"]).await;
    assert!(!host.workspace.path().join("result.txt").exists());
    assert!(
        state["approval"]["description"]
            .as_str()
            .unwrap()
            .contains("approved")
    );
    let body = json!({"run_id":run["run_id"],"approval_id":state["approval"]["id"],"allow":true});
    let stale = host
        .post(
            "approval",
            json!({"run_id":"old","approval_id":state["approval"]["id"],"allow":true}),
        )
        .await;
    assert_eq!(stale.status(), 409);
    assert_eq!(host.post("approval", body.clone()).await.status(), 200);
    host.phase(&["complete"]).await;
    assert_eq!(
        std::fs::read_to_string(host.workspace.path().join("result.txt")).unwrap(),
        "approved"
    );
    assert_eq!(host.post("approval", body).await.status(), 409);
    let replay = host.post("run",json!({"request_id":run["run_id"],"session":run["session"],"operation":{"action":"retry"}})).await;
    assert_eq!(replay.status(), 409);
    let id = run["session"].as_str().unwrap();
    let history = host.get(&format!("sessions/{id}/messages")).await;
    assert_eq!(
        history["entries"].as_array().unwrap().last().unwrap()["message"]["content"],
        "Saved"
    );
    host.get("status").await;
    host.get("sessions").await;
    assert_eq!(host.requests.lock().unwrap().len(), 2);
    host.control.shutdown().await;
    let store = Store::open(host.home.path()).unwrap();
    assert!(store.lock(id).is_ok());
}

#[tokio::test]
async fn denial_and_read_only_mode_never_mutate_files() {
    for mode in [ApprovalMode::Ask, ApprovalMode::ReadOnly] {
        let host = Host::new(vec![write_call(), answer("No change")], mode).await;
        let run = host.start(None).await;
        if mode == ApprovalMode::Ask {
            let state = host.phase(&["awaiting_approval"]).await;
            assert_eq!(host.post("approval",json!({"run_id":run["run_id"],"approval_id":state["approval"]["id"],"allow":false})).await.status(),200);
        }
        host.phase(&["complete"]).await;
        assert!(!host.workspace.path().join("result.txt").exists());
    }
}

#[tokio::test]
async fn pause_cancels_generation_and_explicit_retry_preserves_conversation() {
    let host = Host::new(vec![json!("delay"), answer("Recovered")], ApprovalMode::Ask).await;
    let run = host.start(None).await;
    tokio::time::timeout(Duration::from_secs(5), async {
        while host.requests.lock().unwrap().is_empty() {
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(
        host.post("pause", json!({"run_id":run["run_id"]}))
            .await
            .status(),
        200
    );
    host.phase(&["paused"]).await;
    assert_eq!(host.requests.lock().unwrap().len(), 1);
    let retry = host.post("run",json!({"request_id":uuid::Uuid::new_v4().to_string(),"session":run["session"],"operation":{"action":"retry"}})).await;
    assert_eq!(retry.status(), 202);
    host.phase(&["complete"]).await;
    let requests = host.requests.lock().unwrap();
    assert_eq!(requests[0]["messages"], requests[1]["messages"]);
}

#[tokio::test]
async fn locks_and_uncertain_tools_are_preserved_in_remote_mode() {
    let host = Host::new(vec![], ApprovalMode::Trust).await;
    let mut store = Store::open(host.home.path()).unwrap();
    let profile = Config::load(host.home.path()).unwrap().default_profile;
    let id = store
        .create("uncertain", &profile, host.control.workspace(), "fixture")
        .unwrap();
    store
        .append(&id, &Message::text(Role::User, "write once"))
        .unwrap();
    let call: Message = serde_json::from_value(write_call()).unwrap();
    store.append(&id, &call).unwrap();
    store.claim_tool(&id, "write1").unwrap();
    let lock = store.lock(&id).unwrap();
    let request = || json!({"request_id":uuid::Uuid::new_v4().to_string(),"session":id,"operation":{"action":"retry"}});
    assert_eq!(host.post("run", request()).await.status(), 409);
    drop(lock);
    assert_eq!(host.post("run", request()).await.status(), 202);
    let state = host.phase(&["failed"]).await;
    assert!(state["error"].as_str().unwrap().contains("uncertain"));
    assert!(!host.workspace.path().join("result.txt").exists());
    assert!(host.requests.lock().unwrap().is_empty());
    assert_eq!(
        store.tool_run_state(&id, "write1").unwrap(),
        ToolRunState::Finished
    );
}

#[tokio::test]
async fn history_pages_keep_compacted_and_rewound_originals() {
    let host = Host::new(vec![], ApprovalMode::Ask).await;
    let mut store = Store::open(host.home.path()).unwrap();
    let id = store
        .create("pages", "local", host.control.workspace(), "system")
        .unwrap();
    for i in 0..30 {
        store
            .append(&id, &Message::text(Role::User, format!("original {i}")))
            .unwrap();
        store
            .append(&id, &Message::text(Role::Assistant, format!("answer {i}")))
            .unwrap();
    }
    let original = store.messages(&id).unwrap();
    store
        .checkpoint(
            &id,
            &original,
            &[
                Message::text(Role::System, "system"),
                Message::text(Role::Assistant, "summary"),
            ],
        )
        .unwrap();
    let page = host.get(&format!("sessions/{id}/messages")).await;
    assert_eq!(page["entries"].as_array().unwrap().len(), 25);
    assert!(
        page["entries"]
            .as_array()
            .unwrap()
            .iter()
            .all(|e| e["message"]["content"] != "summary")
    );
    store.rewind(&id).unwrap();
    let mut cursor = None;
    let mut entries = Vec::new();
    loop {
        let page = host
            .get(&format!(
                "sessions/{id}/messages?include_archived=true{}",
                cursor.map(|c| format!("&before={c}")).unwrap_or_default()
            ))
            .await;
        entries.extend(page["entries"].as_array().unwrap().clone());
        cursor = page["next_before"].as_i64();
        if cursor.is_none() {
            break;
        }
    }
    assert!(
        entries
            .iter()
            .any(|e| e["message"]["content"] == "original 29" && e["active"] == false)
    );
    assert!(
        entries
            .iter()
            .any(|e| e["message"]["content"] == "original 0")
    );
    let unique: std::collections::HashSet<_> =
        entries.iter().map(|e| e["seq"].as_i64().unwrap()).collect();
    assert_eq!(unique.len(), entries.len());
}

#[tokio::test]
async fn pause_during_approval_rejects_late_allow_and_restart_does_not_run() {
    let host = Host::new(vec![write_call(), answer("No changes")], ApprovalMode::Ask).await;
    let run = host.start(None).await;
    let waiting = host.phase(&["awaiting_approval"]).await;
    assert_eq!(
        host.post("pause", json!({"run_id":run["run_id"]}))
            .await
            .status(),
        200
    );
    host.phase(&["paused"]).await;
    let late = host
        .post(
            "approval",
            json!({"run_id":run["run_id"],"approval_id":waiting["approval"]["id"],"allow":true}),
        )
        .await;
    assert_eq!(late.status(), 409);
    assert!(!host.workspace.path().join("result.txt").exists());
    host.control.shutdown().await;
    let restarted = RemoteControl::new(RemoteOptions {
        home: host.home.path().into(),
        config_home: host.home.path().into(),
        workspace: host.workspace.path().into(),
        profile: None,
        approval: ApprovalMode::Ask,
        max_rounds: Some(5),
        origin: host.url.clone(),
    })
    .unwrap();
    assert_eq!(
        std::fs::read_to_string(restarted.token_path()).unwrap(),
        host.token
    );
    let store = Store::open(host.home.path()).unwrap();
    assert!(store.lock(run["session"].as_str().unwrap()).is_ok());
    assert_eq!(host.requests.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn foreground_submission_cancels_remote_idle_memory_extraction() {
    let reads: Vec<Value> = (0..6).map(|i| json!({"id":format!("read{i}"),"type":"function","function":{"name":"read_file","arguments":json!({"path":format!("file{i}.txt")}).to_string()}})).collect();
    let host = Host::new(
        vec![
            json!({"role":"assistant","tool_calls":reads}),
            answer("Read the files"),
            json!("delay"),
            answer("New foreground answer"),
        ],
        ApprovalMode::Ask,
    )
    .await;
    for i in 0..6 {
        std::fs::write(
            host.workspace.path().join(format!("file{i}.txt")),
            format!("fixture source {i}"),
        )
        .unwrap();
    }
    let mut config = Config::load(host.home.path()).unwrap();
    config.memory.enabled = true;
    config.memory.embedding_backend = builder_core::config::EmbeddingBackend::Lexical;
    config.save(host.home.path()).unwrap();
    let run = host.start(None).await;
    tokio::time::timeout(Duration::from_secs(5), async {
        while host.requests.lock().unwrap().len() < 3 {
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("Idle extraction started");
    let second = host.start(run["session"].as_str()).await;
    assert_eq!(run["session"], second["session"]);
    host.phase(&["complete", "maintaining"]).await;
    host.control.shutdown().await;
    let store = Store::open(host.home.path()).unwrap();
    let messages = store
        .history_messages(run["session"].as_str().unwrap())
        .unwrap();
    assert_eq!(
        messages.last().unwrap().content.as_deref(),
        Some("New foreground answer")
    );
}

#[tokio::test]
async fn concurrent_duplicate_submissions_create_only_one_conversation() {
    let host = Host::new(vec![answer("Once")], ApprovalMode::Ask).await;
    let request = json!({"request_id":uuid::Uuid::new_v4().to_string(),"session":null,"operation":{"action":"message","prompt":"Only once"}});
    let (first, second) =
        tokio::join!(host.post("run", request.clone()), host.post("run", request));
    let mut codes = [first.status().as_u16(), second.status().as_u16()];
    codes.sort();
    assert_eq!(codes, [202, 409]);
    host.phase(&["complete"]).await;
    assert_eq!(
        host.get("sessions").await["sessions"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert_eq!(host.requests.lock().unwrap().len(), 1);
}

impl Host {
    async fn chat_phase(&self, id: &str, expected: &[&str]) -> Value {
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let status = self.get("status").await;
                if let Some(state) = status["states"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .find(|s| s["session"] == id)
                    && expected.contains(&state["phase"].as_str().unwrap())
                {
                    return state.clone();
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("Expected chat phase")
    }
}

#[tokio::test]
async fn simultaneous_chats_isolate_approvals_pause_and_saved_context() {
    let host = Host::new(
        vec![write_call(), write_call(), answer("Second chat finished")],
        ApprovalMode::Ask,
    )
    .await;
    let first = host.start(None).await;
    let first_id = first["session"].as_str().unwrap();
    let first_wait = host.chat_phase(first_id, &["awaiting_approval"]).await;
    let second = host.start(None).await;
    let second_id = second["session"].as_str().unwrap();
    let second_wait = host.chat_phase(second_id, &["awaiting_approval"]).await;
    assert_eq!(
        host.get("status").await["states"].as_array().unwrap().len(),
        2
    );
    // An approval from a different chat cannot authorize this chat's action.
    assert_eq!(host.post("approval", json!({"run_id":second["run_id"],"approval_id":first_wait["approval"]["id"],"allow":true})).await.status(),409);
    assert_eq!(host.post("run",json!({"request_id":uuid::Uuid::new_v4().to_string(),"session":first_id,"operation":{"action":"message","prompt":"competing"}})).await.status(),409);
    assert_eq!(
        host.post(
            &format!("sessions/{first_id}"),
            json!({"action":"archive","archived":true})
        )
        .await
        .status(),
        409
    );
    assert_eq!(
        host.post("pause", json!({"run_id":first["run_id"]}))
            .await
            .status(),
        200
    );
    host.chat_phase(first_id, &["paused"]).await;
    assert_eq!(
        host.chat_phase(second_id, &["awaiting_approval"]).await["approval"]["id"],
        second_wait["approval"]["id"]
    );
    assert_eq!(host.post("approval",json!({"run_id":first["run_id"],"approval_id":first_wait["approval"]["id"],"allow":true})).await.status(),409);
    assert_eq!(host.post("approval",json!({"run_id":second["run_id"],"approval_id":second_wait["approval"]["id"],"allow":true})).await.status(),200);
    host.chat_phase(second_id, &["complete"]).await;
    assert_eq!(
        std::fs::read_to_string(host.workspace.path().join("result.txt")).unwrap(),
        "approved"
    );
    let store = Store::open(host.home.path()).unwrap();
    assert!(store.history_messages(first_id).unwrap().iter().any(|m| {
        m.content
            .as_deref()
            .is_some_and(|c| c.starts_with("DENIED:"))
    }));
    assert!(
        store
            .history_messages(second_id)
            .unwrap()
            .iter()
            .any(|m| m.content.as_deref() == Some("Second chat finished"))
    );
    assert!(
        !store
            .history_messages(first_id)
            .unwrap()
            .iter()
            .any(|m| m.content.as_deref() == Some("Second chat finished"))
    );
    host.control.shutdown().await;
}

#[tokio::test]
async fn four_chat_limit_and_shutdown_release_every_session_lock() {
    let host = Host::new(vec![json!("delay"); 4], ApprovalMode::ReadOnly).await;
    let mut ids = Vec::new();
    for _ in 0..4 {
        ids.push(
            host.start(None).await["session"]
                .as_str()
                .unwrap()
                .to_owned(),
        );
    }
    assert_eq!(host.get("status").await["max_concurrent_runs"], 4);
    assert_eq!(host.post("run",json!({"request_id":uuid::Uuid::new_v4().to_string(),"session":null,"operation":{"action":"message","prompt":"fifth"}})).await.status(),409);
    assert_eq!(
        host.get("sessions").await["sessions"]
            .as_array()
            .unwrap()
            .len(),
        4
    );
    host.control.shutdown().await;
    let store = Store::open(host.home.path()).unwrap();
    for id in ids {
        let _guard = store.lock(&id).unwrap();
    }
    assert_eq!(host.post("run",json!({"request_id":uuid::Uuid::new_v4().to_string(),"session":null,"operation":{"action":"message","prompt":"after shutdown"}})).await.status(),503);
}

#[tokio::test]
async fn chat_management_preserves_originals_and_rejects_stale_rewind() {
    let host = Host::new(
        vec![answer("First"), answer("Second")],
        ApprovalMode::ReadOnly,
    )
    .await;
    let first = host.start(None).await;
    let id = first["session"].as_str().unwrap();
    host.chat_phase(id, &["complete"]).await;
    host.start(Some(id)).await;
    host.chat_phase(id, &["complete"]).await;
    let path = format!("sessions/{id}");
    // Wait for the final guard to drop, without stopping the server.
    tokio::time::sleep(Duration::from_millis(30)).await;
    let info = host.get(&path).await;
    assert_eq!(info["pending"], false);
    let original = host
        .get(&format!("{path}/messages?include_archived=true"))
        .await;
    assert_eq!(
        host.post(
            &path,
            json!({"action":"rename","title":"Named conversation"})
        )
        .await
        .status(),
        200
    );
    assert_eq!(
        host.get("sessions?search=Named").await["sessions"][0]["id"],
        id
    );
    assert_eq!(
        host.post(&path, json!({"action":"rename","title":"\n"}))
            .await
            .status(),
        409
    );
    assert_eq!(
        host.post(&path, json!({"action":"archive","archived":true}))
            .await
            .status(),
        200
    );
    assert!(
        host.get("sessions").await["sessions"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        host.get("sessions?archived=true").await["sessions"][0]["id"],
        id
    );
    assert_eq!(
        host.post(
            "run",
            json!({"request_id":uuid::Uuid::new_v4().to_string(),"session":id,"operation":{"action":"retry"}})
        )
        .await
        .status(),
        409
    );
    assert_eq!(
        host.post(&path, json!({"action":"archive","archived":false}))
            .await
            .status(),
        200
    );
    assert_eq!(
        host.post(&path, json!({"action":"rewind","expected_tip":info["tip"]}))
            .await
            .status(),
        200
    );
    assert_eq!(
        host.post(&path, json!({"action":"rewind","expected_tip":info["tip"]}))
            .await
            .status(),
        409
    );
    let after = host
        .get(&format!("{path}/messages?include_archived=true"))
        .await;
    assert_eq!(
        after["entries"].as_array().unwrap().len(),
        original["entries"].as_array().unwrap().len()
    );
    assert_eq!(
        after["entries"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|e| e["active"] == false)
            .count(),
        2
    );
    assert_eq!(host.get(&path).await["draft"], "Do the fixture task");
    // Starting after management exercises registry order consistency.
    host.start(Some(id)).await;
    host.chat_phase(id, &["complete"]).await;
    assert!(host.get(&path).await["draft"].is_null());
    assert!(
        !host.get("profiles").await["profiles"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    host.control.shutdown().await;
}

#[tokio::test]
async fn cancelling_saved_turn_checks_tip_and_never_replays_model_or_tools() {
    let host = Host::new(vec![json!("delay")], ApprovalMode::Ask).await;
    let first = host.start(None).await;
    let id = first["session"].as_str().unwrap();
    host.post("pause", json!({"run_id":first["run_id"]})).await;
    host.chat_phase(id, &["paused"]).await;
    tokio::time::sleep(Duration::from_millis(30)).await;
    let path = format!("sessions/{id}");
    let info = host.get(&path).await;
    assert_eq!(info["pending"], true);
    let count = host.requests.lock().unwrap().len();
    assert_eq!(
        host.post(&path, json!({"action":"cancel","expected_tip":-1}))
            .await
            .status(),
        409
    );
    assert_eq!(
        host.post(&path, json!({"action":"cancel","expected_tip":info["tip"]}))
            .await
            .status(),
        200
    );
    assert_eq!(host.get(&path).await["pending"], false);
    assert_eq!(host.requests.lock().unwrap().len(), count);
    host.control.shutdown().await;
}

#[tokio::test]
async fn manual_remote_compaction_preserves_original_chat_without_continuing_it() {
    let host = Host::new(
        vec![answer("A concise handoff of earlier turns")],
        ApprovalMode::ReadOnly,
    )
    .await;
    let mut store = Store::open(host.home.path()).unwrap();
    let id = store
        .create(
            "Compact fixture",
            "local",
            host.control.workspace(),
            "system",
        )
        .unwrap();
    for i in 0..10 {
        store
            .append(
                &id,
                &Message::text(Role::User, format!("Original instruction {i}")),
            )
            .unwrap();
        store
            .append(
                &id,
                &Message::text(Role::Assistant, format!("Original answer {i}")),
            )
            .unwrap();
    }
    let before = serde_json::to_value(store.history_messages(&id).unwrap()).unwrap();
    assert_eq!(host.post("run", json!({"request_id":uuid::Uuid::new_v4().to_string(),"session":id,"operation":{"action":"compact"}})).await.status(),202);
    host.chat_phase(&id, &["complete"]).await;
    assert_eq!(
        serde_json::to_value(store.history_messages(&id).unwrap()).unwrap(),
        before
    );
    assert!(store.messages(&id).unwrap().len() < 21);
    assert_eq!(host.requests.lock().unwrap().len(), 1);
    assert_eq!(
        host.get(&format!("sessions/{id}/messages")).await["entries"]
            .as_array()
            .unwrap()
            .len(),
        21
    );
    host.control.shutdown().await;
}

#[tokio::test]
async fn remote_reloads_the_separate_config_directory_without_moving_chat_data() {
    let host = Host::new(vec![], ApprovalMode::ReadOnly).await;
    let config_home = tempfile::tempdir().unwrap();
    let mut config = Config::load(host.home.path()).unwrap();
    let profile = config.profiles.remove(&config.default_profile).unwrap();
    config.default_profile = "separate-config".into();
    config
        .profiles
        .insert(config.default_profile.clone(), profile);
    config.save(config_home.path()).unwrap();
    let control = RemoteControl::new(RemoteOptions {
        home: host.home.path().into(),
        config_home: config_home.path().into(),
        workspace: host.workspace.path().into(),
        profile: None,
        approval: ApprovalMode::ReadOnly,
        max_rounds: Some(5),
        origin: host.url.clone(),
    })
    .unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let app = control.router();
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    config.profiles.get_mut("separate-config").unwrap().model = "reloaded-model".into();
    config.save(config_home.path()).unwrap();
    let response=host.client.post(format!("http://{address}/api/run")).header("x-builder-token",&host.token).header("origin",&host.url)
        .json(&json!({"request_id":uuid::Uuid::new_v4().to_string(),"session":null,"operation":{"action":"message","prompt":"Read updated config"}})).send().await.unwrap();
    assert_eq!(response.status(), 202);
    let run: Value = response.json().await.unwrap();
    let id = run["session"].as_str().unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if !host.requests.lock().unwrap().is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(host.requests.lock().unwrap()[0]["model"], "reloaded-model");
    assert_eq!(
        Store::open(host.home.path())
            .unwrap()
            .session(id)
            .unwrap()
            .profile,
        "separate-config"
    );
    assert!(!config_home.path().join("builder.sqlite3").exists());
    control.shutdown().await;
    server.abort();
    host.control.shutdown().await;
}

#[tokio::test]
async fn chats_open_in_folders_under_the_root_and_never_outside() {
    let host = Host::new(
        vec![write_call(), answer("Done in folder")],
        ApprovalMode::Trust,
    )
    .await;
    let project = host.workspace.path().join("apps/web");
    std::fs::create_dir_all(&project).unwrap();
    std::fs::create_dir_all(host.workspace.path().join(".hidden")).unwrap();
    std::fs::write(project.join("AGENTS.md"), "Folder rules apply").unwrap();
    // Folder browsing shows only real subdirectories and never leaves the root.
    let listing = host.get("folders").await;
    assert_eq!(listing["folders"], json!(["apps"]));
    assert_eq!(listing["path"], "");
    assert!(listing["parent"].is_null());
    let listing = host.get("folders?path=apps").await;
    assert_eq!(listing["folders"], json!(["web"]));
    assert_eq!(listing["parent"], "");
    for bad in ["..", "apps/../..", "missing", "/etc", ".git"] {
        let response = host
            .client
            .get(format!("{}/api/folders?path={bad}", host.url))
            .header("x-builder-token", &host.token)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 409, "{bad}");
    }
    // A rejected folder never creates a chat or contacts the model.
    for bad in ["..", "missing", ".git", "apps/web/AGENTS.md"] {
        let response = host.post("run", json!({"request_id":uuid::Uuid::new_v4().to_string(),"session":null,"workspace":bad,"operation":{"action":"message","prompt":"Do the fixture task"}})).await;
        assert_eq!(response.status(), 409, "{bad}");
    }
    assert!(
        host.get("sessions").await["sessions"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    assert!(host.requests.lock().unwrap().is_empty());
    // A chat opened in a subfolder runs its tools there and reads that folder's AGENTS.md.
    let response = host.post("run", json!({"request_id":uuid::Uuid::new_v4().to_string(),"session":null,"workspace":"apps/web","operation":{"action":"message","prompt":"Do the fixture task"}})).await;
    assert_eq!(response.status(), 202);
    let run: Value = response.json().await.unwrap();
    host.phase(&["complete"]).await;
    assert_eq!(
        std::fs::read_to_string(project.join("result.txt")).unwrap(),
        "approved"
    );
    assert!(!host.workspace.path().join("result.txt").exists());
    let id = run["session"].as_str().unwrap();
    let info = host.get(&format!("sessions/{id}")).await;
    assert_eq!(info["folder"], "apps/web");
    let history = host.get(&format!("sessions/{id}/messages")).await;
    assert!(
        history["entries"][0]["message"]["content"]
            .as_str()
            .unwrap()
            .contains("Folder rules apply")
    );
    let sessions = host.get("sessions").await;
    assert_eq!(sessions["sessions"][0]["folder"], "apps/web");
    // A saved chat keeps its folder; a follow-up cannot move it.
    let moved = host.post("run", json!({"request_id":uuid::Uuid::new_v4().to_string(),"session":id,"workspace":"","operation":{"action":"message","prompt":"again"}})).await;
    assert_eq!(moved.status(), 409);
    let same = host.post("run", json!({"request_id":uuid::Uuid::new_v4().to_string(),"session":id,"workspace":"apps/web","operation":{"action":"message","prompt":"again"}})).await;
    assert_eq!(same.status(), 202);
    host.phase(&["complete"]).await;
    // Chats created by the CLI inside the root are listed too; the root itself is a valid folder.
    let mut store = Store::open(host.home.path()).unwrap();
    let cli = store
        .create("cli chat", "local", &project, "system")
        .unwrap();
    let root_chat = host.post("run", json!({"request_id":uuid::Uuid::new_v4().to_string(),"session":null,"workspace":".","operation":{"action":"message","prompt":"Do the fixture task"}})).await;
    assert_eq!(root_chat.status(), 202);
    host.phase(&["complete"]).await;
    let listed = host.get("sessions").await;
    let folders: Vec<(String, String)> = listed["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| {
            (
                s["id"].as_str().unwrap().to_owned(),
                s["folder"].as_str().unwrap().to_owned(),
            )
        })
        .collect();
    assert!(
        folders
            .iter()
            .any(|(id, folder)| id == &cli && folder == "apps/web")
    );
    assert!(folders.iter().any(|(_, folder)| folder.is_empty()));
    assert_eq!(folders.len(), 3);
}

#[tokio::test]
async fn browser_selects_approval_mode_per_run_and_read_only_host_is_a_ceiling() {
    // An asking host lets the browser auto-approve one run and ask on the next.
    // Distinct call IDs: Builder refuses a tool call ID reused within one chat.
    let second = json!({"role":"assistant","tool_calls":[{"id":"write2","type":"function","function":{"name":"write_file","arguments":json!({"path":"result.txt","content":"approved"}).to_string()}}]});
    let host = Host::new(
        vec![write_call(), answer("Auto"), second, answer("Asked")],
        ApprovalMode::Ask,
    )
    .await;
    let response = host.post("run", json!({"request_id":uuid::Uuid::new_v4().to_string(),"session":null,"approval":"trust","operation":{"action":"message","prompt":"Do the fixture task"}})).await;
    assert_eq!(response.status(), 202);
    let run: Value = response.json().await.unwrap();
    let state = host.phase(&["complete"]).await;
    assert_eq!(state["approval_mode"], "trust");
    assert_eq!(
        std::fs::read_to_string(host.workspace.path().join("result.txt")).unwrap(),
        "approved"
    );
    std::fs::remove_file(host.workspace.path().join("result.txt")).unwrap();
    let id = run["session"].as_str().unwrap();
    let response = host.post("run", json!({"request_id":uuid::Uuid::new_v4().to_string(),"session":id,"approval":"ask","operation":{"action":"message","prompt":"Again"}})).await;
    assert_eq!(response.status(), 202);
    let state = host.phase(&["awaiting_approval"]).await;
    assert_eq!(state["approval_mode"], "ask");
    assert!(!host.workspace.path().join("result.txt").exists());
    let bogus = host.post("run", json!({"request_id":uuid::Uuid::new_v4().to_string(),"session":null,"approval":"yolo","operation":{"action":"message","prompt":"x"}})).await;
    assert_eq!(bogus.status(), 409);
    // A read-only host refuses escalation before contacting the model.
    let locked = Host::new(vec![write_call()], ApprovalMode::ReadOnly).await;
    assert_eq!(locked.get("status").await["approval_locked"], true);
    for mode in ["trust", "ask"] {
        let response = locked.post("run", json!({"request_id":uuid::Uuid::new_v4().to_string(),"session":null,"approval":mode,"operation":{"action":"message","prompt":"x"}})).await;
        assert_eq!(response.status(), 409, "{mode}");
    }
    assert!(locked.requests.lock().unwrap().is_empty());
    assert!(
        locked.get("sessions").await["sessions"]
            .as_array()
            .unwrap()
            .is_empty()
    );
}
