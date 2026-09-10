use builder::{
    agent::{Agent, ApprovalMode, SYSTEM},
    research,
};
use builder_core::{
    config::{Config, PipelineSettings, Profile},
    protocol::{Function, Message, Role, ToolCall},
    research::{Artifact, Completion, Request, Selector},
    store::Store,
};
use builder_provider::{Event, Provider};
use builder_tools::Workspace;
use serde_json::{Value, json};
use std::{path::Path, process::Command, sync::Mutex};

fn cli(home: &Path, args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_builder"))
        .arg("--home")
        .arg(home)
        .args(args)
        .output()
        .unwrap()
}
#[test]
fn cli_persists_profile_scoped_settings_atomically_and_overrides_are_transient() {
    let home = tempfile::tempdir().unwrap();
    assert!(cli(home.path(), &["config", "init"]).status.success());
    assert!(
        cli(
            home.path(),
            &[
                "config",
                "add",
                "other",
                "--base-url",
                "http://localhost:9/v1",
                "--model",
                "other"
            ]
        )
        .status
        .success()
    );
    assert!(
        cli(
            home.path(),
            &[
                "--profile",
                "other",
                "config",
                "pipeline",
                "set",
                "candidates=false",
                "review=false",
                "candidate-attempts=7",
                "completion_retries=0"
            ]
        )
        .status
        .success()
    );
    let config = Config::load(home.path()).unwrap();
    let other = &config.profiles["other"];
    assert!(!other.pipeline.candidates);
    assert!(!other.pipeline.review);
    assert_eq!(other.pipeline.candidate_attempts, 7);
    assert!(config.profiles["local"].pipeline.candidates);
    let original = std::fs::read(home.path().join("config.toml")).unwrap();
    let show = cli(
        home.path(),
        &[
            "--profile",
            "other",
            "--pipeline",
            "candidates=true",
            "config",
            "pipeline",
            "show",
        ],
    );
    assert!(show.status.success());
    let text = String::from_utf8(show.stdout).unwrap();
    assert!(text.contains("candidates = true"));
    assert!(text.contains("Transient overrides: true"));
    assert_eq!(
        original,
        std::fs::read(home.path().join("config.toml")).unwrap()
    );
    for settings in [
        vec!["analysis=false", "candidate_attempts=0"],
        vec!["unknown=true"],
        vec!["enabled=yes"],
        vec!["analysis_output_tokens=999999"],
        vec!["snapshot_max_bytes=-1"],
    ] {
        let mut args = vec!["--profile", "other", "config", "pipeline", "set"];
        args.extend(settings);
        assert!(!cli(home.path(), &args).status.success());
        assert_eq!(
            original,
            std::fs::read(home.path().join("config.toml")).unwrap()
        );
    }
    assert!(
        cli(
            home.path(),
            &["--profile", "other", "config", "model", "renamed-model"]
        )
        .status
        .success()
    );
    assert!(
        !Config::load(home.path()).unwrap().profiles["other"]
            .pipeline
            .review
    );
    assert!(
        cli(
            home.path(),
            &["--profile", "other", "config", "pipeline", "reset"]
        )
        .status
        .success()
    );
    assert_eq!(
        Config::load(home.path()).unwrap().profiles["other"].pipeline,
        PipelineSettings::default()
    );
}
#[test]
fn legacy_profiles_keep_defaults_and_all_typed_settings_validate() {
    let profile: Profile = toml::from_str("model='legacy'\n").unwrap();
    assert_eq!(profile.pipeline, PipelineSettings::default());
    let defaults = PipelineSettings::default();
    let value = serde_json::to_value(&defaults).unwrap();
    for (key, value) in value.as_object().unwrap() {
        let assignment = format!(
            "{key}={}",
            if value.is_boolean() {
                "false".into()
            } else {
                value.to_string()
            }
        );
        let changed = defaults.updated(&[assignment]).unwrap();
        assert_eq!(
            serde_json::to_value(changed).unwrap()[key],
            if value.is_boolean() {
                json!(false)
            } else {
                value.clone()
            }
        );
    }
    assert!(toml::from_str::<Profile>("[pipeline]\nunknown=true\n").is_err());
    let invalid: Profile = toml::from_str("[pipeline]\nanalysis_timeout_secs=0\n").unwrap();
    assert!(invalid.validate().is_err());
    assert_eq!(defaults, PipelineSettings::default());
}
struct Replies {
    messages: Mutex<Vec<Message>>,
    requests: Mutex<Vec<(Vec<Message>, Vec<Value>)>>,
}
impl Replies {
    fn new(messages: Vec<Message>) -> Self {
        Self {
            messages: Mutex::new(messages),
            requests: Mutex::new(vec![]),
        }
    }
}
impl Provider for Replies {
    async fn complete(
        &self,
        messages: &[Message],
        tools: &[Value],
        _: &mut dyn FnMut(Event),
    ) -> anyhow::Result<Message> {
        self.requests
            .lock()
            .unwrap()
            .push((messages.to_vec(), tools.to_vec()));
        let mut replies = self.messages.lock().unwrap();
        anyhow::ensure!(!replies.is_empty(), "Unexpected model call");
        Ok(replies.remove(0))
    }
}
fn call(id: &str, request: &Request) -> Message {
    let mut message = Message::text(Role::Assistant, "");
    message.tool_calls.push(ToolCall {
        id: id.into(),
        kind: "function".into(),
        function: Function {
            name: "research".into(),
            arguments: json!({"request":request}).to_string(),
        },
    });
    message
}
fn artifact() -> Artifact {
    Artifact {
        path: "contract.json".into(),
        selector: Selector::File,
    }
}
fn verify() -> Request {
    Request::Verify {
        hypothesis_id: None,
        criterion: "contract exists".into(),
        command: "touch must_not_exist".into(),
        dependencies: vec![artifact()],
        timeout_secs: None,
    }
}

#[tokio::test]
async fn disabled_research_and_verification_cannot_execute_queued_calls_in_trust_mode() {
    for disable_all in [false, true] {
        let home = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("contract.json"), "{}").unwrap();
        let mut store = Store::open(home.path()).unwrap();
        let workspace = Workspace::new(root.path()).unwrap();
        let session = store
            .create("pending", "test", workspace.root(), SYSTEM)
            .unwrap();
        store
            .append(&session, &Message::text(Role::User, "check"))
            .unwrap();
        store.append(&session, &call("queued", &verify())).unwrap();
        let mut profile = Profile::default();
        profile.pipeline.verification = false;
        profile.pipeline.enabled = !disable_all;
        let agent = Agent {
            provider: Replies::new(vec![Message::text(
                Role::Assistant,
                "Disabled check was not run.",
            )]),
            profile,
            workspace,
            session: session.clone(),
            approval: ApprovalMode::Trust,
            max_rounds: 2,
            memory: None,
        };
        agent
            .run(&mut store, &mut |_| {}, &mut |_| true)
            .await
            .unwrap();
        assert!(!root.path().join("must_not_exist").exists());
        assert!(
            store
                .tool_result(&session, "queued")
                .unwrap()
                .unwrap()
                .contains("disabled by pipeline configuration")
        );
        let requests = agent.provider.requests.lock().unwrap();
        let (_, tools) = &requests[0];
        let research = tools.iter().find(|t| t["function"]["name"] == "research");
        if disable_all {
            assert!(research.is_none());
            assert!(!requests[0].0.iter().any(|m| {
                m.content
                    .as_deref()
                    .unwrap_or("")
                    .starts_with("Builder research policy:")
            }));
        } else {
            let operations =
                research.unwrap()["function"]["parameters"]["properties"]["request"]["properties"]
                    ["operation"]["enum"]
                    .as_array()
                    .unwrap();
            assert!(!operations.contains(&json!("verify")));
            assert!(!operations.contains(&json!("review")));
        }
    }
}

#[tokio::test]
async fn completion_gate_and_review_can_be_disabled_without_bypassing_freshness() {
    let home = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("contract.json"), "{}").unwrap();
    let mut store = Store::open(home.path()).unwrap();
    let workspace = Workspace::new(root.path()).unwrap();
    let session = store
        .create("plan", "test", workspace.root(), SYSTEM)
        .unwrap();
    store
        .append(&session, &Message::text(Role::User, "check contract"))
        .unwrap();
    let settings = PipelineSettings {
        review: false,
        ..Default::default()
    };
    for (id, request) in [
        (
            "plan",
            Request::Plan {
                criteria: vec!["contract exists".into()],
            },
        ),
        (
            "check",
            Request::Verify {
                hypothesis_id: None,
                criterion: "contract exists".into(),
                command: "test -f contract.json".into(),
                dependencies: vec![artifact()],
                timeout_secs: None,
            },
        ),
    ] {
        store.append(&session, &call(id, &request)).unwrap();
        store.claim_tool(&session, id).unwrap();
        let result = research::execute_with_settings(
            &Replies::new(vec![]),
            &store,
            &session,
            &workspace,
            &request,
            32768,
            &settings,
        )
        .await
        .unwrap();
        store.complete_tool(&session, id, &result).unwrap();
    }
    assert_eq!(
        research::status_with_settings(&store, &session, &workspace, &settings).unwrap()["ready_to_finish_verified"],
        true
    );
    std::fs::write(root.path().join("contract.json"), "{\"changed\":true}").unwrap();
    assert!(
        research::execute_with_settings(
            &Replies::new(vec![]),
            &store,
            &session,
            &workspace,
            &Request::Finish {
                outcome: Completion::Verified,
                explanation: "old check".into()
            },
            32768,
            &settings
        )
        .await
        .is_err()
    );
    let mut profile = Profile::default();
    profile.pipeline.completion_gate = false;
    let agent = Agent {
        provider: Replies::new(vec![Message::text(
            Role::Assistant,
            "No current verification.",
        )]),
        profile,
        workspace,
        session: session.clone(),
        approval: ApprovalMode::ReadOnly,
        max_rounds: 1,
        memory: None,
    };
    agent
        .run(&mut store, &mut |_| {}, &mut |_| false)
        .await
        .unwrap();
    assert!(!builder::agent::pending(&store.messages(&session).unwrap()));
}

#[tokio::test]
async fn every_feature_toggle_rejects_its_operations_before_work_starts() {
    let home = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("contract.json"), "{}").unwrap();
    let mut store = Store::open(home.path()).unwrap();
    let workspace = Workspace::new(root.path()).unwrap();
    let session = store
        .create("toggles", "test", workspace.root(), SYSTEM)
        .unwrap();
    store
        .append(&session, &Message::text(Role::User, "inspect"))
        .unwrap();
    let provider = Replies::new(vec![]);
    let requests = vec![
        ("enabled", Request::Status),
        (
            "planning",
            Request::Plan {
                criteria: vec!["check".into()],
            },
        ),
        (
            "observations",
            Request::Observe {
                artifacts: vec![artifact()],
            },
        ),
        (
            "symbols",
            Request::Symbols {
                query: "value".into(),
                glob: None,
            },
        ),
        (
            "semantic",
            Request::Semantic {
                server: "does-not-exist".into(),
                args: vec![],
                path: "contract.json".into(),
                line: 0,
                character: 0,
                feature: builder_core::research::SymbolFeature::Definition,
            },
        ),
        (
            "hypotheses",
            Request::Hypothesis {
                claim: "x".into(),
                falsification: "y".into(),
                evidence_ids: vec![],
            },
        ),
        ("verification", verify()),
        (
            "candidates",
            Request::CandidateTest {
                hypothesis_id: "missing".into(),
                criterion: "x".into(),
                patches: vec![],
                command: "touch must_not_exist".into(),
                timeout_secs: None,
            },
        ),
        (
            "candidates",
            Request::CandidateApply {
                candidate_id: "missing".into(),
            },
        ),
        (
            "review",
            Request::Review {
                verification_ids: vec![],
            },
        ),
        (
            "procedures",
            Request::Learn {
                key: "x".into(),
                phase: builder_core::research::Phase::Verify,
                procedure: "x".into(),
                applicability: "x".into(),
                verification_ids: vec![],
                supersedes: None,
            },
        ),
        (
            "procedures",
            Request::Recall {
                query: "x".into(),
                phase: builder_core::research::Phase::Verify,
            },
        ),
        (
            "procedures",
            Request::Retire {
                key: "x".into(),
                reason: "x".into(),
            },
        ),
        (
            "history",
            Request::HistorySearch {
                query: "x".into(),
                before_seq: None,
                include_archived: false,
            },
        ),
        ("history", Request::HistoryRead { seq: 1, offset: 0 }),
        (
            "analysis",
            Request::Analyze {
                question: "why?".into(),
                artifacts: vec![artifact()],
            },
        ),
    ];
    for (key, request) in requests {
        let settings = PipelineSettings::default()
            .updated(&[format!("{key}=false")])
            .unwrap();
        let error = research::execute_with_settings(
            &provider, &store, &session, &workspace, &request, 32768, &settings,
        )
        .await
        .unwrap_err();
        assert!(
            error.to_string().contains("disabled by pipeline"),
            "{key}: {error}"
        );
        let definitions = builder_tools::definitions_with_pipeline(&settings);
        if let Some(tool) = definitions
            .iter()
            .find(|t| t["function"]["name"] == "research")
        {
            assert!(
                !tool["function"]["parameters"]["properties"]["request"]["properties"]["operation"]
                    ["enum"]
                    .as_array()
                    .unwrap()
                    .contains(&json!(request.operation()))
            );
        }
    }
    assert!(provider.requests.lock().unwrap().is_empty());
    assert!(!root.path().join("must_not_exist").exists());
    let settings = PipelineSettings {
        guidance: false,
        auto_recall: false,
        ..Default::default()
    };
    let packet = research::packet_with_settings(&store, &session, &workspace, true, &settings)
        .unwrap()
        .content
        .unwrap();
    assert!(packet.contains("guidance is disabled"));
    assert!(!packet.contains("Use enabled operations to gather"));
}

#[tokio::test]
async fn configured_snapshot_history_and_analysis_bounds_are_enforced() {
    let home = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("contract.json"), "{}").unwrap();
    std::fs::write(root.path().join("caller.rs"), "fn main() {}\n").unwrap();
    let mut store = Store::open(home.path()).unwrap();
    let workspace = Workspace::new(root.path()).unwrap();
    let session = store
        .create("limits", "test", workspace.root(), SYSTEM)
        .unwrap();
    store
        .append(&session, &Message::text(Role::User, "λ".repeat(1000)))
        .unwrap();
    let seq = store.latest_user_seq(&session).unwrap();
    let provider = Replies::new(vec![]);
    for setting in [
        "snapshot_max_files=1",
        "snapshot_max_file_bytes=1",
        "snapshot_max_bytes=1",
    ] {
        let settings = PipelineSettings::default()
            .updated(&[setting.into()])
            .unwrap();
        assert!(
            research::execute_with_settings(
                &provider,
                &store,
                &session,
                &workspace,
                &verify(),
                32768,
                &settings
            )
            .await
            .is_err()
        );
        assert!(!root.path().join("must_not_exist").exists());
    }
    let settings = PipelineSettings {
        history_page_bytes: 256,
        history_search_results: 1,
        analysis_artifacts: 1,
        ..Default::default()
    };
    let page: Value = serde_json::from_str(
        &research::execute_with_settings(
            &provider,
            &store,
            &session,
            &workspace,
            &Request::HistoryRead { seq, offset: 0 },
            32768,
            &settings,
        )
        .await
        .unwrap(),
    )
    .unwrap();
    assert!(page["original_json_page"].as_str().unwrap().len() <= 256);
    assert!(page["next_offset"].is_number());
    let search: Value = serde_json::from_str(
        &research::execute_with_settings(
            &provider,
            &store,
            &session,
            &workspace,
            &Request::HistorySearch {
                query: String::new(),
                before_seq: None,
                include_archived: true,
            },
            32768,
            &settings,
        )
        .await
        .unwrap(),
    )
    .unwrap();
    assert_eq!(search["results"].as_array().unwrap().len(), 1);
    assert!(
        research::execute_with_settings(
            &provider,
            &store,
            &session,
            &workspace,
            &Request::Analyze {
                question: "why".into(),
                artifacts: vec![artifact(), artifact()]
            },
            32768,
            &settings
        )
        .await
        .unwrap_err()
        .to_string()
        .contains("artifact limit (1)")
    );
    assert!(provider.requests.lock().unwrap().is_empty());
}

#[tokio::test]
async fn cli_run_override_changes_the_actual_model_request_without_saving() {
    use axum::{Json, Router, extract::State, routing::post};
    use std::sync::Arc;
    let requests = Arc::new(Mutex::new(Vec::<Value>::new()));
    let app=Router::new().route("/v1/chat/completions",post(|State(seen):State<Arc<Mutex<Vec<Value>>>>,Json(body):Json<Value>|async move{seen.lock().unwrap().push(body);Json(json!({"choices":[{"finish_reason":"stop","message":{"role":"assistant","content":"checked configuration"}}]}))})).with_state(requests.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/v1", listener.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let home = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    let mut config = Config::default();
    let profile = config.profiles.get_mut("local").unwrap();
    profile.base_url = url;
    profile.stream = false;
    profile.pipeline.enabled = false;
    config.save(home.path()).unwrap();
    let original = std::fs::read(home.path().join("config.toml")).unwrap();
    let output = tokio::process::Command::new(env!("CARGO_BIN_EXE_builder"))
        .arg("--home")
        .arg(home.path())
        .arg("-C")
        .arg(root.path())
        .args([
            "--pipeline",
            "enabled=true",
            "--pipeline",
            "semantic=false",
            "--pipeline",
            "analysis=false",
            "run",
            "say hello",
        ])
        .output()
        .await
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        original,
        std::fs::read(home.path().join("config.toml")).unwrap()
    );
    let requests = requests.lock().unwrap();
    assert_eq!(requests.len(), 1);
    let tool = requests[0]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["function"]["name"] == "research")
        .unwrap();
    let operations =
        tool["function"]["parameters"]["properties"]["request"]["properties"]["operation"]["enum"]
            .as_array()
            .unwrap();
    assert!(!operations.contains(&json!("semantic")));
    assert!(!operations.contains(&json!("analyze")));
    assert!(operations.contains(&json!("plan")));
    server.abort();
}

struct BudgetProbe(Mutex<Vec<usize>>);
impl Provider for BudgetProbe {
    async fn complete(
        &self,
        _: &[Message],
        _: &[Value],
        _: &mut dyn FnMut(Event),
    ) -> anyhow::Result<Message> {
        anyhow::bail!("expected explicit budget")
    }
    async fn complete_with_budget(
        &self,
        _: &[Message],
        tools: &[Value],
        budget: usize,
        _: &mut dyn FnMut(Event),
    ) -> anyhow::Result<Message> {
        assert!(tools.is_empty());
        self.0.lock().unwrap().push(budget);
        Ok(Message::text(
            Role::Assistant,
            "Interpretation only; test the contract.",
        ))
    }
}
struct SlowProbe;
impl Provider for SlowProbe {
    async fn complete(
        &self,
        _: &[Message],
        _: &[Value],
        _: &mut dyn FnMut(Event),
    ) -> anyhow::Result<Message> {
        std::future::pending().await
    }
}
#[tokio::test]
async fn analysis_and_command_budgets_reach_the_real_executors() {
    let home = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("contract.json"), "{}").unwrap();
    let mut store = Store::open(home.path()).unwrap();
    let workspace = Workspace::new(root.path()).unwrap();
    let session = store
        .create("budgets", "test", workspace.root(), SYSTEM)
        .unwrap();
    store
        .append(&session, &Message::text(Role::User, "inspect"))
        .unwrap();
    let settings = PipelineSettings {
        analysis_output_tokens: 512,
        analysis_timeout_secs: 1,
        command_timeout_secs: 1,
        ..Default::default()
    };
    let request = Request::Analyze {
        question: "what is the contract?".into(),
        artifacts: vec![artifact()],
    };
    let probe = BudgetProbe(Mutex::new(vec![]));
    research::execute_with_settings(
        &probe, &store, &session, &workspace, &request, 32768, &settings,
    )
    .await
    .unwrap();
    assert_eq!(*probe.0.lock().unwrap(), vec![512]);
    let error = research::execute_with_settings(
        &SlowProbe, &store, &session, &workspace, &request, 32768, &settings,
    )
    .await
    .unwrap_err();
    assert!(error.to_string().contains("deadline"));
    #[cfg(unix)]
    {
        let request = Request::Verify {
            hypothesis_id: None,
            criterion: "timeout".into(),
            command: "sleep 5; touch must_not_exist".into(),
            dependencies: vec![artifact()],
            timeout_secs: Some(120),
        };
        let error = research::execute_with_settings(
            &probe, &store, &session, &workspace, &request, 32768, &settings,
        )
        .await
        .unwrap_err();
        assert!(error.to_string().contains("timed out"));
        assert!(!root.path().join("must_not_exist").exists());
    }
}

#[test]
fn research_schemas_reject_cross_operation_fields_and_require_decoder_inputs() {
    let definition = builder_tools::research::definition();
    let variants = definition["function"]["parameters"]["properties"]["request"]["anyOf"]
        .as_array()
        .unwrap();
    let finish = variants
        .iter()
        .find(|v| v["properties"]["operation"]["enum"][0] == "finish")
        .unwrap();
    assert_eq!(finish["additionalProperties"], false);
    assert!(finish["properties"].get("verification_ids").is_none());
    assert_eq!(
        finish["required"],
        json!(["operation", "outcome", "explanation"])
    );
    assert_eq!(variants.len(), 17);
    for variant in variants {
        assert_eq!(variant["additionalProperties"], false);
        for required in variant["required"].as_array().unwrap() {
            assert!(
                variant["properties"]
                    .get(required.as_str().unwrap())
                    .is_some()
            );
        }
    }
}

#[test]
fn runtime_budgets_default_migrate_validate_and_persist() {
    let settings: builder_core::config::PipelineSettings = serde_json::from_str("{}").unwrap();
    assert_eq!(settings.max_rounds, 100);
    assert_eq!(settings.progress_check_calls, 12);
    assert_eq!(settings.progress_recovery_rounds, 88);
    assert_eq!(settings.max_no_progress_calls(), 100);
    assert_eq!(settings.failure_check_calls, 3);
    assert_eq!(settings.failure_recovery_rounds, 3);
    assert_eq!(settings.identical_shell_calls, 3);
    assert_eq!(settings.tool_calls_per_response, 128);
    for assignment in [
        "max_rounds=0",
        "max_rounds=1001",
        "progress_check_calls=0",
        "progress_recovery_rounds=1001",
        "failure_check_calls=101",
        "failure_recovery_rounds=0",
        "identical_shell_calls=0",
        "tool_calls_per_response=129",
    ] {
        assert!(settings.updated(&[assignment.into()]).is_err());
    }
    let home = tempfile::tempdir().unwrap();
    assert!(cli(home.path(), &["config", "init"]).status.success());
    assert!(
        cli(
            home.path(),
            &[
                "config",
                "pipeline",
                "set",
                "max_rounds=250",
                "progress_check_calls=40",
                "progress_recovery_rounds=210",
                "failure_check_calls=5",
                "failure_recovery_rounds=7",
                "identical_shell_calls=4",
                "tool_calls_per_response=24",
            ]
        )
        .status
        .success()
    );
    let loaded = Config::load(home.path()).unwrap();
    let pipeline = &loaded.profiles["local"].pipeline;
    assert_eq!(pipeline.max_rounds, 250);
    assert_eq!(pipeline.progress_check_calls, 40);
    assert_eq!(pipeline.progress_recovery_rounds, 210);
    assert_eq!(pipeline.max_no_progress_calls(), 250);
    assert_eq!(pipeline.failure_check_calls, 5);
    assert_eq!(pipeline.failure_recovery_rounds, 7);
    assert_eq!(pipeline.identical_shell_calls, 4);
    assert_eq!(pipeline.tool_calls_per_response, 24);
}
