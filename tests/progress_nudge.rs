//! Long investigations without a file change are nudged, never forced: every
//! tool except broad discovery stays available, while the trailing runtime
//! message shows the model its own read history and recommends a todo list.
use builder::agent::{Agent, AgentEvent, ApprovalMode, SYSTEM};
use builder_core::{
    config::Profile,
    protocol::{Function, Message, Role, ToolCall},
    store::{Store, ToolOutcome},
};
use builder_provider::{Event, Provider};
use builder_tools::Workspace;
use serde_json::{Value, json};
use std::sync::Mutex;

type Request = (Vec<Message>, Vec<Value>);

struct Replies {
    replies: Mutex<Vec<Message>>,
    requests: Mutex<Vec<Request>>,
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
        let mut replies = self.replies.lock().unwrap();
        anyhow::ensure!(!replies.is_empty(), "Unexpected model call");
        Ok(replies.remove(0))
    }
}

fn call(id: &str, name: &str, arguments: Value) -> Message {
    let mut message = Message::text(Role::Assistant, "");
    message.tool_calls.push(ToolCall {
        id: id.into(),
        kind: "function".into(),
        function: Function {
            name: name.into(),
            arguments: arguments.to_string(),
        },
    });
    message
}

struct Fixture {
    _home: tempfile::TempDir,
    root: tempfile::TempDir,
    store: Store,
    session: String,
    calls: usize,
}

impl Fixture {
    fn new() -> Self {
        let home = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("a.txt"), "alpha\n").unwrap();
        std::fs::write(root.path().join("b.txt"), "beta\n").unwrap();
        let mut store = Store::open(home.path()).unwrap();
        let session = store
            .create(
                "nudge",
                "test",
                Workspace::new(root.path()).unwrap().root(),
                SYSTEM,
            )
            .unwrap();
        store
            .append(&session, &Message::text(Role::User, "change a.txt"))
            .unwrap();
        Self {
            _home: home,
            root,
            store,
            session,
            calls: 0,
        }
    }

    /// Record a completed call as if an earlier run had executed it.
    fn record(&mut self, name: &str, arguments: Value, outcome: ToolOutcome) {
        self.calls += 1;
        let id = format!("seed_{}", self.calls);
        self.store
            .append(&self.session, &call(&id, name, arguments))
            .unwrap();
        self.store.claim_tool(&self.session, &id).unwrap();
        self.store
            .complete_tool_with_outcome(&self.session, &id, "recorded", outcome)
            .unwrap();
    }

    fn reads(&mut self, path: &str, count: usize) {
        for _ in 0..count {
            self.record("read_file", json!({ "path": path }), ToolOutcome::Succeeded);
        }
    }

    async fn run(
        &mut self,
        approval: ApprovalMode,
        replies: Vec<Message>,
    ) -> (Vec<Request>, Vec<AgentEvent>) {
        let agent = Agent {
            provider: Replies {
                replies: Mutex::new(replies),
                requests: Mutex::new(Vec::new()),
            },
            memory: None,
            profile: Profile::default(),
            workspace: Workspace::new(self.root.path()).unwrap(),
            session: self.session.clone(),
            approval,
            // Above the final-rounds budget notice, which varies per round.
            max_rounds: 20,
        };
        let mut events = Vec::new();
        agent
            .run(
                &mut self.store,
                &mut |event| {
                    if matches!(event, AgentEvent::ProgressNudge { .. }) {
                        events.push(event);
                    }
                },
                &mut |_| true,
            )
            .await
            .unwrap();
        (agent.provider.requests.into_inner().unwrap(), events)
    }
}

fn done() -> Message {
    Message::text(Role::Assistant, "done")
}

fn last_text(request: &Request) -> &str {
    request.0.last().unwrap().content.as_deref().unwrap_or("")
}

fn tool_names(request: &Request) -> Vec<&str> {
    request
        .1
        .iter()
        .filter_map(|tool| tool["function"]["name"].as_str())
        .collect()
}

fn leading_system(request: &Request) -> Vec<Option<String>> {
    request
        .0
        .iter()
        .take_while(|message| message.role == Role::System)
        .map(|message| message.content.clone())
        .collect()
}

#[tokio::test]
async fn halfway_reminder_suggests_a_plan_with_every_tool_still_available() {
    let mut fixture = Fixture::new();
    fixture.reads("a.txt", 3);
    fixture.reads("./b.txt", 2);
    let (requests, _) = fixture.run(ApprovalMode::Trust, vec![done()]).await;
    assert_eq!(requests[0].0.last().unwrap().role, Role::Tool);

    let mut fixture = Fixture::new();
    fixture.reads("a.txt", 4);
    fixture.reads("./b.txt", 2);
    let (requests, events) = fixture.run(ApprovalMode::Trust, vec![done()]).await;
    assert!(events.is_empty());
    let note = last_text(&requests[0]);
    assert!(note.starts_with("[Builder runtime note"), "{note}");
    assert!(note.contains("no todo list yet"), "{note}");
    assert!(
        note.contains(
            "6 successful reads of 2 files, 4 of them repeats: a.txt ×4 (latest result still in context), b.txt ×2 (latest result still in context)."
        ),
        "{note}"
    );
    let tools = tool_names(&requests[0]);
    for name in [
        "list_files",
        "search",
        "read_file",
        "todo_write",
        "subagent",
    ] {
        assert!(tools.contains(&name), "{name} missing from {tools:?}");
    }
    // The reminder is request-only.
    assert!(
        !fixture
            .store
            .history_messages(&fixture.session)
            .unwrap()
            .iter()
            .any(|message| message
                .content
                .as_deref()
                .is_some_and(|text| text.contains("runtime note")))
    );
}

#[tokio::test]
async fn progress_check_recommends_a_plan_but_leaves_the_choice_to_the_model() {
    let mut fixture = Fixture::new();
    fixture.reads("a.txt", 12);
    // The model chooses one more read, then answers.
    let replies = vec![call("more", "read_file", json!({"path":"b.txt"})), done()];
    let (requests, events) = fixture.run(ApprovalMode::Trust, replies).await;
    assert_eq!(requests.len(), 2);
    for request in &requests {
        let tools = tool_names(request);
        assert!(tools.contains(&"read_file") && tools.contains(&"todo_write"));
        assert!(tools.contains(&"write_file") && tools.contains(&"shell"));
        for paused in ["list_files", "search", "subagent"] {
            assert!(!tools.contains(&paused), "{paused} still offered");
        }
    }
    let first = last_text(&requests[0]);
    assert!(first.contains("Progress check: 12 tool calls"), "{first}");
    assert!(first.contains("a.txt ×12"), "{first}");
    assert!(
        first.contains("1. Recommended for an implementation request"),
        "{first}"
    );
    assert!(first.contains("call todo_write now"), "{first}");
    assert!(!first.contains("without a plan"), "{first}");
    let second = last_text(&requests[1]);
    assert!(second.contains("Progress check: 13 tool calls"), "{second}");
    assert!(
        second.contains("13 successful reads of 2 files"),
        "{second}"
    );
    // Everything that changes per round is at the end, so a server's prompt
    // cache keeps the whole leading block across guided rounds.
    assert_eq!(leading_system(&requests[0]), leading_system(&requests[1]));
    assert!(
        fixture
            .store
            .tool_result(&fixture.session, "more")
            .unwrap()
            .is_some()
    );
    assert!(matches!(
        events.as_slice(),
        [AgentEvent::ProgressNudge {
            calls: 12,
            step: None,
            repeated_reads: 11,
            planning: true,
        }]
    ));
}

#[tokio::test]
async fn the_nudge_grows_firmer_and_is_shown_again_while_no_plan_exists() {
    let mut fixture = Fixture::new();
    fixture.reads("a.txt", 24);
    let (requests, events) = fixture.run(ApprovalMode::Trust, vec![done()]).await;
    let text = last_text(&requests[0]);
    assert!(
        text.contains("This check has now continued for 12 calls without a plan"),
        "{text}"
    );
    assert!(matches!(
        events.as_slice(),
        [AgentEvent::ProgressNudge { calls: 24, .. }]
    ));
}

#[tokio::test]
async fn an_active_plan_turns_the_nudge_toward_its_current_step() {
    let mut fixture = Fixture::new();
    fixture.record(
        "todo_write",
        json!({"todos":[
            {"content":"edit a.txt","status":"in_progress"},
            {"content":"verify a.txt","status":"pending"},
        ]}),
        ToolOutcome::Succeeded,
    );
    fixture.reads("a.txt", 11);
    let (requests, events) = fixture.run(ApprovalMode::Trust, vec![done()]).await;
    let text = last_text(&requests[0]);
    assert!(
        text.contains("make the change for todo item 1 of 2 (edit a.txt) now"),
        "{text}"
    );
    assert!(!text.contains("call todo_write now"), "{text}");
    assert!(matches!(
        events.as_slice(),
        [AgentEvent::ProgressNudge {
            calls: 12,
            step: Some((1, 2)),
            planning: false,
            ..
        }]
    ));
}

#[tokio::test]
async fn read_history_starts_over_after_a_file_change() {
    let mut fixture = Fixture::new();
    fixture.reads("b.txt", 5);
    fixture.record(
        "write_file",
        json!({"path":"a.txt","content":"new\n"}),
        ToolOutcome::Changed,
    );
    fixture.reads("a.txt", 6);
    let (requests, _) = fixture.run(ApprovalMode::Trust, vec![done()]).await;
    let note = last_text(&requests[0]);
    assert!(
        note.contains("6 successful reads of 1 file, 5 of them repeats: a.txt ×6"),
        "{note}"
    );
    assert!(!note.contains("b.txt"), "{note}");
}

#[tokio::test]
async fn read_only_sessions_are_nudged_to_answer_instead_of_plan() {
    let mut fixture = Fixture::new();
    fixture.reads("a.txt", 12);
    let (requests, events) = fixture.run(ApprovalMode::ReadOnly, vec![done()]).await;
    let text = last_text(&requests[0]);
    assert!(text.contains("write the answer now"), "{text}");
    assert!(!text.contains("todo_write"), "{text}");
    assert!(matches!(
        events.as_slice(),
        [AgentEvent::ProgressNudge {
            planning: false,
            ..
        }]
    ));
}
