//! Opt-in same-model ablations. Hidden graders live in a separate workspace and
//! are never sent to the actor. Ordinary cargo test runs only the grader contract.
use anyhow::Result;
use builder::{
    agent::{Agent, AgentEvent, ApprovalMode, SYSTEM},
    memory::MemoryRuntime,
};
use builder_core::{config::Config, protocol::Message, research::Request, store::Store};
use builder_provider::{Event, OpenAiCompatible, Provider};
use builder_tools::{Action, Workspace};
use serde_json::{Value, json};
use std::{
    path::Path,
    sync::atomic::{AtomicUsize, Ordering},
    time::Instant,
};

const MANIFEST: &str =
    "[package]\nname = \"research_fixture\"\nversion = \"0.1.0\"\nedition = \"2024\"\n";
struct Case {
    name: &'static str,
    source: &'static str,
    request: &'static str,
    hidden: &'static str,
}
const CASES: &[Case] = &[
    Case {
        name: "retry_boundary",
        source: "pub fn should_retry(attempt: u32, max: u32) -> bool { attempt <= max }\n",
        request: "Fix should_retry: attempt is zero-based and max is the total allowed attempts. Retry exactly when attempt is below max, including zero and integer boundaries. Add meaningful regression tests in tests/regression.rs. Do not change public names or Cargo.toml.",
        hidden: "use research_fixture::should_retry; #[test] fn boundaries(){assert!(!should_retry(0,0));assert!(should_retry(0,1));assert!(!should_retry(1,1));assert!(!should_retry(u32::MAX,u32::MAX));assert!(should_retry(u32::MAX-1,u32::MAX));}",
    },
    Case {
        name: "optional_contract",
        source: "pub struct Reply { pub duration_ms: Option<u64> }\npub fn duration_seconds(reply: Reply) -> Option<u64> { Some(reply.duration_ms.unwrap_or(0)) }\n",
        request: "The upstream Reply contract changed: missing duration_ms means unknown, not zero. Fix duration_seconds to preserve None and convert present milliseconds to whole seconds by truncating. Add meaningful regression tests in tests/regression.rs. Preserve public names and Cargo.toml.",
        hidden: "use research_fixture::{Reply,duration_seconds}; #[test] fn contract(){for (input,expected) in [(None,None),(Some(0),Some(0)),(Some(999),Some(0)),(Some(1999),Some(1)),(Some(u64::MAX),Some(u64::MAX/1000))]{assert_eq!(duration_seconds(Reply{duration_ms:input}),expected);}}",
    },
];
fn fixture(root: &Path, source: &str) -> Result<()> {
    std::fs::create_dir_all(root.join("src"))?;
    std::fs::create_dir_all(root.join("tests"))?;
    std::fs::write(root.join("Cargo.toml"), MANIFEST)?;
    std::fs::write(
        root.join("Cargo.lock"),
        "version = 4\n\n[[package]]\nname = \"research_fixture\"\nversion = \"0.1.0\"\n",
    )?;
    std::fs::write(root.join("src/lib.rs"), source)?;
    Ok(())
}
async fn grade(source: &str, hidden: &str) -> Result<bool> {
    let root = tempfile::tempdir()?;
    fixture(root.path(), source)?;
    std::fs::write(root.path().join("tests/hidden.rs"), hidden)?;
    let output = builder_tools::research::check(
        &Workspace::new(root.path())?,
        "cargo test --offline --quiet",
        Some(30),
    )
    .await?;
    Ok(builder_tools::research::passed(&output))
}
struct Ablation {
    inner: OpenAiCompatible,
    research: bool,
    calls: AtomicUsize,
}
impl Provider for Ablation {
    async fn complete(
        &self,
        messages: &[Message],
        tools: &[Value],
        emit: &mut dyn FnMut(Event),
    ) -> Result<Message> {
        self.complete_with_budget(messages, tools, 4096, emit).await
    }
    async fn complete_with_budget(
        &self,
        messages: &[Message],
        tools: &[Value],
        budget: usize,
        emit: &mut dyn FnMut(Event),
    ) -> Result<Message> {
        self.calls.fetch_add(1, Ordering::Relaxed);
        let messages = messages
            .iter()
            .filter(|m| {
                self.research
                    || !m
                        .content
                        .as_deref()
                        .unwrap_or("")
                        .starts_with("Builder research policy:")
            })
            .cloned()
            .collect::<Vec<_>>();
        let tools = tools
            .iter()
            .filter(|t| self.research || t["function"]["name"] != "research")
            .cloned()
            .collect::<Vec<_>>();
        self.inner
            .complete_with_budget(&messages, &tools, budget, emit)
            .await
    }
}
fn allowed(action: &Action) -> bool {
    let path_allowed = |path: &str| matches!(path, "src/lib.rs" | "tests/regression.rs");
    match action {
        Action::WriteFile { path, .. } | Action::EditFile { path, .. } => path_allowed(path),
        Action::Shell { command, .. } => command == "cargo test --offline --quiet",
        Action::Research { request } => match request {
            Request::Verify { command, .. } => command == "cargo test --offline --quiet",
            Request::CandidateTest {
                command, patches, ..
            } => {
                command == "cargo test --offline --quiet"
                    && patches.iter().all(|p| path_allowed(&p.path))
            }
            Request::CandidateApply { .. } => true,
            _ => false,
        },
        _ => false,
    }
}

#[tokio::test]
async fn held_out_graders_reject_original_bugs_and_accept_contract_fixes() {
    for case in CASES {
        assert!(!grade(case.source, case.hidden).await.unwrap());
    }
    assert!(
        grade(
            "pub fn should_retry(attempt:u32,max:u32)->bool {attempt<max}",
            CASES[0].hidden
        )
        .await
        .unwrap()
    );
    assert!(grade("pub struct Reply {pub duration_ms:Option<u64>} pub fn duration_seconds(r:Reply)->Option<u64>{r.duration_ms.map(|ms|ms/1000)}",CASES[1].hidden).await.unwrap());
}

#[tokio::test]
#[ignore = "Uses the configured model endpoint; run explicitly with BUILDER_RESEARCH_EVAL_HOME"]
async fn same_model_baseline_memory_and_research_ablation() -> Result<()> {
    let home = std::env::var("BUILDER_RESEARCH_EVAL_HOME")?;
    let config = Config::load(Path::new(&home))?;
    let profile_name = std::env::var("BUILDER_RESEARCH_EVAL_PROFILE").ok();
    let (_, profile) = config.profile(profile_name.as_deref())?;
    let repetitions = std::env::var("BUILDER_RESEARCH_EVAL_REPETITIONS")
        .ok()
        .and_then(|s| s.parse::<usize>().ok())
        .unwrap_or(1)
        .clamp(1, 10);
    let mut reports = Vec::new();
    for repetition in 0..repetitions {
        for mode in ["baseline", "memory", "research"] {
            if std::env::var("BUILDER_RESEARCH_EVAL_MODE").is_ok_and(|selected| selected != mode) {
                continue;
            }
            for case in CASES {
                if std::env::var("BUILDER_RESEARCH_EVAL_CASE")
                    .is_ok_and(|selected| selected != case.name)
                {
                    continue;
                }
                eprintln!("Research eval: {mode}/{}", case.name);
                let root = tempfile::tempdir()?;
                let home = tempfile::tempdir()?;
                fixture(root.path(), case.source)?;
                let workspace = Workspace::new(root.path())?;
                let mut store = Store::open(home.path())?;
                let session = store.create(case.name, "eval", workspace.root(), SYSTEM)?;
                let agent = Agent {
                    provider: Ablation {
                        inner: OpenAiCompatible::new(profile.clone())?,
                        research: mode == "research",
                        calls: AtomicUsize::new(0),
                    },
                    profile: profile.clone(),
                    workspace,
                    session: session.clone(),
                    approval: ApprovalMode::Ask,
                    max_rounds: 30,
                    memory: if mode == "baseline" {
                        None
                    } else {
                        Some(MemoryRuntime::lexical())
                    },
                };
                agent.submit(&mut store,&format!("{} Only src/lib.rs and tests/regression.rs may be changed. The only allowed executable check is exactly cargo test --offline --quiet. No external services or commits. Finish by reporting checked and unverified behavior.",case.request))?;
                if mode == "research" {
                    let plan = Request::Plan {
                        criteria: vec![case.request.into()],
                    };
                    let call = builder_core::protocol::ToolCall {
                        id: "eval_plan".into(),
                        kind: "function".into(),
                        function: builder_core::protocol::Function {
                            name: "research".into(),
                            arguments: json!({"request":plan}).to_string(),
                        },
                    };
                    let message = Message {
                        role: builder_core::protocol::Role::Assistant,
                        content: None,
                        reasoning: None,
                        tool_calls: vec![call],
                        tool_call_id: None,
                    };
                    store.append(&session, &message)?;
                    store.claim_tool(&session, "eval_plan")?;
                    let result = builder::research::execute(
                        &agent.provider,
                        &store,
                        &session,
                        &agent.workspace,
                        &plan,
                        agent.profile.context_tokens,
                    )
                    .await?;
                    store.complete_tool(&session, "eval_plan", &result)?;
                }
                let started = Instant::now();
                let mut bytes = 0;
                let mut tools = 0;
                let run = tokio::time::timeout(
                    std::time::Duration::from_secs(300),
                    agent.run(
                        &mut store,
                        &mut |event| match event {
                            AgentEvent::Model(Event::Prompt { bytes: n }) => bytes += n,
                            AgentEvent::ToolStarted { .. } => tools += 1,
                            _ => {}
                        },
                        &mut allowed,
                    ),
                )
                .await;
                let runtime_error = match &run {
                    Ok(Err(error)) => Some(format!("{error:#}")),
                    Err(_) => Some("trial deadline exceeded".to_string()),
                    _ => None,
                };
                let runtime_ok = matches!(run, Ok(Ok(())));
                let source = std::fs::read_to_string(root.path().join("src/lib.rs"))?;
                let solved = grade(&source, case.hidden).await?;
                let state = builder::research::status(&store, &session, &agent.workspace)?;
                let immutable =
                    std::fs::read_to_string(root.path().join("Cargo.toml"))? == MANIFEST;
                reports.push(json!({"case":case.name,"mode":mode,"repetition":repetition+1,"held_out_passed":solved&&immutable,"runtime_completed":runtime_ok,"runtime_error":runtime_error,"elapsed_ms":started.elapsed().as_millis(),"model_calls":agent.provider.calls.load(Ordering::Relaxed),"actor_serialized_prompt_bytes":bytes,"tool_calls":tools,"research_outcome":state["outcome"],"false_verified":state["outcome"]=="verified"&&!solved}));
            }
        }
    }
    let passed = !reports.is_empty()
        && reports.iter().all(|r| {
            r["held_out_passed"] == true
                && r["runtime_completed"] == true
                && r["false_verified"] == false
        });
    let report = json!({"suite":"research-v1","model":profile.model,"cases":reports,"limitations":"Two small held-out Rust contracts, not a general SWE benchmark. Prompt bytes are not token usage. Dollar cost is unavailable without endpoint usage/pricing. No memory shared between cases or ablations."});
    if let Ok(path) = std::env::var("BUILDER_RESEARCH_EVAL_OUTPUT") {
        std::fs::write(path, serde_json::to_vec_pretty(&report)?)?;
    } else {
        println!("{}", serde_json::to_string_pretty(&report)?);
    }
    anyhow::ensure!(
        passed,
        "Live research evaluation failed; inspect the report"
    );
    Ok(())
}
