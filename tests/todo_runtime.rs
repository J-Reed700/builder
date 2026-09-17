//! The todo list is derived from committed tool results: it is shown when
//! recorded, reminds every later request, and follows rewind and compaction.
use builder::agent::{Agent, AgentEvent, ApprovalMode, SYSTEM};
use builder_core::{
    config::Profile,
    protocol::{Function, Message, Role, ToolCall},
    store::Store,
    todo::Status,
};
use builder_provider::{Event, Provider};
use builder_tools::Workspace;
use serde_json::{Value, json};
use std::sync::Mutex;

struct Replies {
    replies: Mutex<Vec<Message>>,
    requests: Mutex<Vec<(Vec<Message>, Vec<Value>)>>,
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

fn plan(first: &str, second: &str) -> Value {
    json!({"todos":[
        {"content":"edit a.txt","status":first},
        {"content":"verify a.txt","status":second},
    ]})
}

fn todo_packet(messages: &[Message]) -> Option<&str> {
    messages
        .iter()
        .take_while(|message| message.role == Role::System)
        .filter_map(|message| message.content.as_deref())
        .find(|text| text.starts_with("Builder todo list"))
}

struct Fixture {
    _home: tempfile::TempDir,
    root: tempfile::TempDir,
    store: Store,
    session: String,
}
impl Fixture {
    fn new() -> Self {
        let home = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("a.txt"), "old\n").unwrap();
        let mut store = Store::open(home.path()).unwrap();
        let session = store
            .create(
                "todo",
                "test",
                Workspace::new(root.path()).unwrap().root(),
                SYSTEM,
            )
            .unwrap();
        store
            .append(
                &session,
                &Message::text(Role::User, "change a.txt in two steps"),
            )
            .unwrap();
        Self {
            _home: home,
            root,
            store,
            session,
        }
    }

    fn agent(&self, profile: Profile, replies: Vec<Message>) -> Agent<Replies> {
        Agent {
            provider: Replies {
                replies: Mutex::new(replies),
                requests: Mutex::new(vec![]),
            },
            memory: None,
            profile,
            workspace: Workspace::new(self.root.path()).unwrap(),
            session: self.session.clone(),
            approval: ApprovalMode::Trust,
            max_rounds: 8,
        }
    }
}

async fn run(fixture: &mut Fixture, agent: &Agent<Replies>) -> Vec<String> {
    let mut events = Vec::new();
    agent
        .run(
            &mut fixture.store,
            &mut |event| match event {
                AgentEvent::TodosUpdated(list) => {
                    events.push(format!("board {}/{}", list.completed(), list.items.len()))
                }
                AgentEvent::ToolFinished { name, failed, .. } => {
                    events.push(format!("{name} failed={failed}"))
                }
                _ => {}
            },
            &mut |_| true,
        )
        .await
        .unwrap();
    events
}

#[tokio::test]
async fn a_recorded_list_is_shown_and_reminds_every_later_request() {
    let mut f = Fixture::new();
    let agent = f.agent(
        Profile::default(),
        vec![
            call("plan", "todo_write", plan("in_progress", "pending")),
            call(
                "edit",
                "edit_file",
                json!({"path":"a.txt","old":"old","new":"new"}),
            ),
            call("done", "todo_write", plan("completed", "in_progress")),
            Message::text(Role::Assistant, "Changed a.txt."),
        ],
    );
    let events = run(&mut f, &agent).await;
    assert_eq!(
        events,
        [
            "todo_write failed=false",
            "board 0/2",
            "edit_file failed=false",
            "todo_write failed=false",
            "board 1/2",
        ]
    );
    let requests = agent.provider.requests.lock().unwrap();
    assert!(
        requests[0]
            .1
            .iter()
            .any(|tool| tool["function"]["name"] == "todo_write")
    );
    assert!(todo_packet(&requests[0].0).is_none());
    let packet = todo_packet(&requests[1].0).unwrap();
    assert!(packet.contains("0 of 2 items completed"), "{packet}");
    assert!(packet.contains("Current item: 1. edit a.txt"), "{packet}");
    assert!(packet.contains("[>] 1. edit a.txt\n[ ] 2. verify a.txt"));
    assert!(
        todo_packet(&requests[3].0)
            .unwrap()
            .contains("Current item: 2. verify a.txt")
    );

    let list = f.store.todos(&f.session).unwrap().unwrap();
    assert_eq!(list.items[1].status, Status::InProgress);
    assert!(
        f.store
            .tool_result(&f.session, "plan")
            .unwrap()
            .unwrap()
            .starts_with("Todo list saved: 0 of 2 completed. Current item: 1. edit a.txt")
    );

    // Rewinding the turn retracts the plan along with the calls that wrote it.
    f.store.rewind(&f.session).unwrap();
    assert!(f.store.todos(&f.session).unwrap().is_none());
}

#[tokio::test]
async fn compaction_cannot_summarize_the_list_away() {
    let mut f = Fixture::new();
    let writer = f.agent(
        Profile::default(),
        vec![
            call("plan", "todo_write", plan("in_progress", "pending")),
            Message::text(Role::Assistant, "Planned."),
        ],
    );
    run(&mut f, &writer).await;
    let expected = f.store.messages(&f.session).unwrap();
    let summary = vec![
        Message::text(Role::System, SYSTEM),
        Message::text(Role::System, "Summary without the plan."),
        Message::text(Role::User, "continue"),
    ];
    f.store.checkpoint(&f.session, &expected, &summary).unwrap();
    assert!(
        !f.store
            .messages(&f.session)
            .unwrap()
            .iter()
            .any(|message| !message.tool_calls.is_empty())
    );

    let reader = f.agent(
        Profile::default(),
        vec![Message::text(Role::Assistant, "Continuing.")],
    );
    run(&mut f, &reader).await;
    let requests = reader.provider.requests.lock().unwrap();
    assert!(
        todo_packet(&requests[0].0)
            .unwrap()
            .contains("Current item: 1. edit a.txt")
    );
}

#[tokio::test]
async fn invalid_and_finished_lists_do_not_steer_requests() {
    let mut f = Fixture::new();
    let agent = f.agent(
        Profile::default(),
        vec![
            call("invalid", "todo_write", plan("in_progress", "in_progress")),
            call("finished", "todo_write", plan("completed", "completed")),
            Message::text(Role::Assistant, "All done."),
        ],
    );
    let events = run(&mut f, &agent).await;
    assert_eq!(
        events,
        [
            "todo_write failed=true",
            "todo_write failed=false",
            "board 2/2",
        ]
    );
    assert!(
        f.store
            .tool_result(&f.session, "invalid")
            .unwrap()
            .unwrap()
            .contains("At most one todo item may be in_progress")
    );
    let requests = agent.provider.requests.lock().unwrap();
    assert!(
        requests
            .iter()
            .all(|(messages, _)| todo_packet(messages).is_none())
    );
    assert!(f.store.todos(&f.session).unwrap().unwrap().is_finished());
}

#[tokio::test]
async fn disabling_todos_hides_the_tool_and_rejects_queued_writes() {
    let mut f = Fixture::new();
    let queued = call("queued", "todo_write", plan("in_progress", "pending"));
    f.store.append(&f.session, &queued).unwrap();
    let mut profile = Profile::default();
    profile.pipeline.todos = false;
    let agent = f.agent(
        profile,
        vec![Message::text(Role::Assistant, "Proceeding without a list.")],
    );
    let events = run(&mut f, &agent).await;
    assert_eq!(events, ["todo_write failed=true"]);
    assert!(
        f.store
            .tool_result(&f.session, "queued")
            .unwrap()
            .unwrap()
            .contains("disabled by pipeline configuration")
    );
    assert!(f.store.todos(&f.session).unwrap().is_none());
    let requests = agent.provider.requests.lock().unwrap();
    assert!(
        !requests[0]
            .1
            .iter()
            .any(|tool| tool["function"]["name"] == "todo_write")
    );
}

#[tokio::test]
async fn an_unfinished_list_supersedes_extracted_next_actions() {
    use builder::memory::MemoryRuntime;
    use builder_core::memory::TaskState;
    let mut f = Fixture::new();
    let agent = f.agent(
        Profile::default(),
        vec![
            call("plan", "todo_write", plan("in_progress", "pending")),
            Message::text(Role::Assistant, "Planned."),
        ],
    );
    run(&mut f, &agent).await;
    let source_seq = f.store.memory_latest_seq(&f.session).unwrap();
    f.store
        .memory_save_task(
            &f.session,
            &TaskState {
                next_action: "EXTRACTED-PROPOSAL".into(),
                questions: vec![],
                source_seq,
            },
        )
        .unwrap();
    let memory = MemoryRuntime::lexical();
    let workspace = Workspace::new(f.root.path()).unwrap();
    let packet = |todos| memory.packet_with_todos(&f.store, &f.session, &workspace, todos);
    let active = packet(true).await.unwrap().content.unwrap();
    assert!(!active.contains("EXTRACTED-PROPOSAL"), "{active}");
    let disabled = packet(false).await.unwrap().content.unwrap();
    assert!(disabled.contains("EXTRACTED-PROPOSAL"), "{disabled}");
}
