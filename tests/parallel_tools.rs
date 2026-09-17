//! Consecutive side-effect-free calls run together but are claimed, recorded
//! and reported in call order; interrupting them never creates uncertainty.
use builder::agent::{Agent, AgentEvent, ApprovalMode, SYSTEM};
use builder_core::{
    config::Profile,
    protocol::{Function, Message, Role, ToolCall},
    store::{INTERRUPTED_SIDE_EFFECT_FREE, Store, ToolOutcome},
};
use builder_provider::{Event, Provider};
use builder_tools::Workspace;
use serde_json::{Value, json};
use std::sync::Mutex;

struct Replies(Mutex<Vec<Message>>);
impl Provider for Replies {
    async fn complete(
        &self,
        _: &[Message],
        _: &[Value],
        _: &mut dyn FnMut(Event),
    ) -> anyhow::Result<Message> {
        let mut replies = self.0.lock().unwrap();
        anyhow::ensure!(!replies.is_empty(), "Unexpected model call");
        Ok(replies.remove(0))
    }
}

fn batch(calls: &[(&str, &str, Value)]) -> Message {
    let mut message = Message::text(Role::Assistant, "");
    for (id, name, arguments) in calls {
        message.tool_calls.push(ToolCall {
            id: (*id).into(),
            kind: "function".into(),
            function: Function {
                name: (*name).into(),
                arguments: arguments.to_string(),
            },
        });
    }
    message
}

fn setup() -> (tempfile::TempDir, tempfile::TempDir, Store, String) {
    let home = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    for name in ["a", "b", "c"] {
        std::fs::write(
            root.path().join(format!("{name}.txt")),
            format!("{name} body\n"),
        )
        .unwrap();
    }
    let mut store = Store::open(home.path()).unwrap();
    let session = store
        .create(
            "parallel",
            "test",
            Workspace::new(root.path()).unwrap().root(),
            SYSTEM,
        )
        .unwrap();
    store
        .append(&session, &Message::text(Role::User, "inspect and edit"))
        .unwrap();
    (home, root, store, session)
}

fn agent(
    root: &std::path::Path,
    session: &str,
    profile: Profile,
    replies: Vec<Message>,
) -> Agent<Replies> {
    Agent {
        provider: Replies(Mutex::new(replies)),
        memory: None,
        profile,
        workspace: Workspace::new(root).unwrap(),
        session: session.into(),
        approval: ApprovalMode::Trust,
        max_rounds: 4,
    }
}

async fn run(agent: &Agent<Replies>, store: &mut Store) -> Vec<String> {
    let mut events = Vec::new();
    agent
        .run(
            store,
            &mut |event| match event {
                AgentEvent::ToolStarted { detail, .. } => events.push(format!("start {detail}")),
                AgentEvent::ToolFinished { detail, .. } => events.push(format!("finish {detail}")),
                _ => {}
            },
            &mut |_| true,
        )
        .await
        .unwrap();
    events
}

fn mixed_batch() -> Message {
    batch(&[
        ("r1", "read_file", json!({"path":"a.txt"})),
        ("r2", "read_file", json!({"path":"b.txt"})),
        ("r3", "search", json!({"query":"body"})),
        (
            "e1",
            "multi_edit",
            json!({"path":"a.txt","edits":[{"old":"a","new":"A"},{"old":"body","new":"text"}]}),
        ),
        ("r4", "read_file", json!({"path":"a.txt"})),
    ])
}

#[tokio::test]
async fn inspections_run_as_a_group_and_commit_in_call_order() {
    let (_home, root, mut store, session) = setup();
    let agent = agent(
        root.path(),
        &session,
        Profile::default(),
        vec![mixed_batch(), Message::text(Role::Assistant, "done")],
    );
    let events = run(&agent, &mut store).await;
    assert_eq!(
        events,
        [
            "start a.txt",
            "start b.txt",
            "start body",
            "finish a.txt",
            "finish b.txt",
            "finish body",
            "start a.txt · 2 edits",
            "finish a.txt · 2 edits",
            "start a.txt",
            "finish a.txt",
        ]
    );
    let order = store
        .history_messages(&session)
        .unwrap()
        .into_iter()
        .filter_map(|message| message.tool_call_id)
        .collect::<Vec<_>>();
    assert_eq!(order, ["r1", "r2", "r3", "e1", "r4"]);
    // The read after the edit sees the edit: groups never cross a mutation.
    let after = store.tool_result(&session, "r4").unwrap().unwrap();
    assert!(after.contains("    1|A text"), "{after}");
    let before = store.tool_result(&session, "r1").unwrap().unwrap();
    assert!(before.contains("    1|a body"), "{before}");
    assert_eq!(
        store.tool_outcomes(&session).unwrap()["e1"],
        ToolOutcome::Changed
    );
}

#[tokio::test]
async fn one_parallel_slot_runs_every_call_in_turn() {
    let (_home, root, mut store, session) = setup();
    let mut profile = Profile::default();
    profile.pipeline.parallel_tools = 1;
    let agent = agent(
        root.path(),
        &session,
        profile,
        vec![mixed_batch(), Message::text(Role::Assistant, "done")],
    );
    let events = run(&agent, &mut store).await;
    assert_eq!(
        events,
        [
            "start a.txt",
            "finish a.txt",
            "start b.txt",
            "finish b.txt",
            "start body",
            "finish body",
            "start a.txt · 2 edits",
            "finish a.txt · 2 edits",
            "start a.txt",
            "finish a.txt",
        ]
    );
}

#[tokio::test]
async fn a_group_never_runs_past_the_no_progress_limit() {
    let (_home, root, mut store, session) = setup();
    let mut profile = Profile::default();
    profile.pipeline.progress_check_calls = 1;
    profile.pipeline.progress_recovery_rounds = 1;
    store
        .append(
            &session,
            &batch(&[
                ("r1", "read_file", json!({"path":"a.txt"})),
                ("r2", "read_file", json!({"path":"b.txt"})),
                ("r3", "read_file", json!({"path":"c.txt"})),
            ]),
        )
        .unwrap();
    let agent = agent(root.path(), &session, profile, vec![]);
    let error = agent
        .run(&mut store, &mut |_| {}, &mut |_| true)
        .await
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("Stopped before executing saved read_file call r3"),
        "{error}"
    );
    assert!(store.tool_result(&session, "r2").unwrap().is_some());
    assert!(store.tool_result(&session, "r3").unwrap().is_none());
}

#[tokio::test]
async fn interrupted_inspections_close_as_retryable_not_uncertain() {
    let (_home, root, mut store, session) = setup();
    store
        .append(
            &session,
            &batch(&[
                ("read", "read_file", json!({"path":"a.txt"})),
                ("next", "read_file", json!({"path":"b.txt"})),
            ]),
        )
        .unwrap();
    // A process died after claiming the read but before saving its result.
    assert!(store.claim_side_effect_free_tool(&session, "read").unwrap());
    let agent = agent(
        root.path(),
        &session,
        Profile::default(),
        vec![Message::text(Role::Assistant, "continued")],
    );
    run(&agent, &mut store).await;
    assert_eq!(
        store.tool_result(&session, "read").unwrap().as_deref(),
        Some(INTERRUPTED_SIDE_EFFECT_FREE)
    );
    let outcomes = store.tool_outcomes(&session).unwrap();
    assert_eq!(outcomes["read"], ToolOutcome::Failed);
    assert_eq!(outcomes["next"], ToolOutcome::Succeeded);
    assert!(!builder::agent::pending(&store.messages(&session).unwrap()));

    // A new instruction closes the same situation without reporting
    // uncertainty; an ordinary interrupted claim still does.
    store
        .append(
            &session,
            &batch(&[
                ("free", "read_file", json!({"path":"a.txt"})),
                ("effect", "shell", json!({"command":"true"})),
            ]),
        )
        .unwrap();
    store.claim_side_effect_free_tool(&session, "free").unwrap();
    assert_eq!(
        store
            .interrupt_turn(&session, Some("new direction"))
            .unwrap(),
        0
    );
    store
        .append(
            &session,
            &batch(&[("effect2", "shell", json!({"command":"true"}))]),
        )
        .unwrap();
    store.claim_tool(&session, "effect2").unwrap();
    assert_eq!(store.interrupt_turn(&session, Some("again")).unwrap(), 1);
}
