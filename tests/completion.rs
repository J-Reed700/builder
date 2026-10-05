use builder::completion::{Outcome, assess};
use builder_core::{
    config::PipelineSettings,
    protocol::{Function, Message, Role, ToolCall},
    store::{Store, ToolOutcome},
};
use builder_tools::Workspace;

fn call(store: &mut Store, session: &str, id: &str, command: &str, outcome: ToolOutcome) {
    let mut message = Message::text(Role::Assistant, "");
    message.tool_calls.push(ToolCall {
        id: id.into(),
        kind: "function".into(),
        function: Function {
            name: "shell".into(),
            arguments: serde_json::json!({"command":command,"timeout_secs":30}).to_string(),
        },
    });
    store.append(session, &message).unwrap();
    store.claim_tool(session, id).unwrap();
    store
        .complete_tool_with_outcome(session, id, "tool output", outcome)
        .unwrap();
}

#[test]
fn completion_requires_evidence_and_tracks_recovery_without_prose_heuristics() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = Store::open(dir.path()).unwrap();
    let workspace = Workspace::new(dir.path()).unwrap();
    let session = store.create("test", "local", dir.path(), "system").unwrap();
    let settings = PipelineSettings::default();
    store
        .append(&session, &Message::text(Role::User, "test it"))
        .unwrap();
    assert_eq!(
        assess(&store, &session, &workspace, &settings).unwrap(),
        Outcome::Pending
    );
    call(
        &mut store,
        &session,
        "failed",
        "cargo test",
        ToolOutcome::Failed,
    );
    store
        .append(
            &session,
            &Message::text(Role::Assistant, "Everything passed!"),
        )
        .unwrap();
    assert_eq!(
        assess(&store, &session, &workspace, &settings).unwrap(),
        Outcome::Failed
    );
    call(
        &mut store,
        &session,
        "unrelated",
        "pwd",
        ToolOutcome::Succeeded,
    );
    store
        .append(&session, &Message::text(Role::Assistant, "done"))
        .unwrap();
    assert_eq!(
        assess(&store, &session, &workspace, &settings).unwrap(),
        Outcome::Failed
    );
    call(
        &mut store,
        &session,
        "recovered",
        "cargo test",
        ToolOutcome::Succeeded,
    );
    store
        .append(&session, &Message::text(Role::Assistant, "done"))
        .unwrap();
    assert_eq!(
        assess(&store, &session, &workspace, &settings).unwrap(),
        Outcome::Unverified
    );
    call(
        &mut store,
        &session,
        "timedout",
        "make deploy",
        ToolOutcome::Uncertain,
    );
    store
        .append(&session, &Message::text(Role::Assistant, "done"))
        .unwrap();
    assert_eq!(
        assess(&store, &session, &workspace, &settings).unwrap(),
        Outcome::Blocked
    );
    store
        .append(&session, &Message::text(Role::User, "new question"))
        .unwrap();
    store
        .append(&session, &Message::text(Role::Assistant, "answer"))
        .unwrap();
    assert_eq!(
        assess(&store, &session, &workspace, &settings).unwrap(),
        Outcome::Unverified
    );
}

struct NoModel;
impl builder_provider::Provider for NoModel {
    async fn complete(
        &self,
        _: &[Message],
        _: &[serde_json::Value],
        _: &mut dyn FnMut(builder_provider::Event),
    ) -> anyhow::Result<Message> {
        anyhow::bail!("No inference needed for completion checks")
    }
}

#[cfg(unix)]
#[tokio::test]
async fn a_real_timeout_blocks_identical_side_effects_under_a_new_call_id() {
    struct Retry(std::sync::atomic::AtomicUsize);
    impl builder_provider::Provider for Retry {
        async fn complete(
            &self,
            _: &[Message],
            _: &[serde_json::Value],
            _: &mut dyn FnMut(builder_provider::Event),
        ) -> anyhow::Result<Message> {
            let id = self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let mut message = Message::text(Role::Assistant, "");
            message.tool_calls.push(ToolCall { id: format!("attempt_{id}"), kind: "function".into(), function: Function { name: "shell".into(), arguments: serde_json::json!({"command":"printf x >> marker; sleep 5", "timeout_secs":id + 1}).to_string() } });
            Ok(message)
        }
    }
    let home = tempfile::tempdir().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let mut store = Store::open(home.path()).unwrap();
    let workspace = Workspace::new(dir.path()).unwrap();
    let session = store
        .create("timeout", "local", dir.path(), "system")
        .unwrap();
    let mut profile = builder_core::config::Profile::default();
    profile.pipeline.subagents = false;
    profile.pipeline.code_index = false;
    let agent = builder::agent::Agent {
        provider: Retry(std::sync::atomic::AtomicUsize::new(0)),
        memory: None,
        profile,
        workspace,
        session: session.clone(),
        approval: builder::agent::ApprovalMode::Trust,
        max_rounds: 3,
    };
    agent.submit(&mut store, "run the check").unwrap();
    let error = agent
        .run(&mut store, &mut |_| {}, &mut |_| true)
        .await
        .unwrap_err();
    assert!(error.to_string().contains("uncertain"), "{error:#}");
    assert_eq!(
        std::fs::read_to_string(dir.path().join("marker")).unwrap(),
        "x"
    );
    let outcomes = store.tool_outcomes(&session).unwrap();
    assert_eq!(outcomes.len(), 1);
    assert_eq!(outcomes["attempt_0"], ToolOutcome::Uncertain);
}

#[tokio::test]
async fn verified_finish_requires_fresh_proof_and_cannot_hide_a_later_failure() {
    use builder_core::research::{Artifact, Completion, Request, Selector};
    let home = tempfile::tempdir().unwrap();
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("source.txt"), "current").unwrap();
    let mut store = Store::open(home.path()).unwrap();
    let workspace = Workspace::new(dir.path()).unwrap();
    let session = store.create("test", "local", dir.path(), "system").unwrap();
    store
        .append(&session, &Message::text(Role::User, "verify"))
        .unwrap();
    let settings = PipelineSettings {
        review: false,
        ..Default::default()
    };
    call(
        &mut store,
        &session,
        "earlier_failure",
        "failed initial check",
        ToolOutcome::Failed,
    );
    for (id, request) in [
        (
            "plan",
            Request::Plan {
                criteria: vec!["source verified".into()],
            },
        ),
        (
            "verify",
            Request::Verify {
                hypothesis_id: None,
                criterion: "source verified".into(),
                command: "echo check".into(),
                dependencies: vec![Artifact {
                    path: "source.txt".into(),
                    selector: Selector::File,
                }],
                timeout_secs: Some(5),
            },
        ),
        (
            "finish",
            Request::Finish {
                outcome: Completion::Verified,
                explanation: "fresh passing check".into(),
            },
        ),
    ] {
        let mut message = Message::text(Role::Assistant, "");
        message.tool_calls.push(ToolCall {
            id: id.into(),
            kind: "function".into(),
            function: Function {
                name: "research".into(),
                arguments: serde_json::json!({"request":request}).to_string(),
            },
        });
        store.append(&session, &message).unwrap();
        store.claim_tool(&session, id).unwrap();
        let output = builder::research::execute_with_settings(
            &NoModel, &store, &session, &workspace, &request, 32768, &settings,
        )
        .await
        .unwrap();
        store
            .complete_tool_with_outcome(&session, id, &output, ToolOutcome::Succeeded)
            .unwrap();
    }
    store
        .append(&session, &Message::text(Role::Assistant, "done"))
        .unwrap();
    assert_eq!(
        assess(&store, &session, &workspace, &settings).unwrap(),
        Outcome::Verified
    );
    call(
        &mut store,
        &session,
        "later_failure",
        "failed final check",
        ToolOutcome::Failed,
    );
    store
        .append(&session, &Message::text(Role::Assistant, "done"))
        .unwrap();
    assert_eq!(
        assess(&store, &session, &workspace, &settings).unwrap(),
        Outcome::Failed
    );
    call(
        &mut store,
        &session,
        "recovered_final",
        "failed final check",
        ToolOutcome::Succeeded,
    );
    store
        .append(&session, &Message::text(Role::Assistant, "done"))
        .unwrap();
    assert_eq!(
        assess(&store, &session, &workspace, &settings).unwrap(),
        Outcome::Verified
    );
    std::fs::write(dir.path().join("source.txt"), "changed").unwrap();
    assert_ne!(
        assess(&store, &session, &workspace, &settings).unwrap(),
        Outcome::Verified
    );
}
