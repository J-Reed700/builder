//! Repeated evidence consumes the progress budget, while new inspection
//! results reset it. The trailing runtime message recommends a todo list.
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

    fn say(&mut self, text: &str) {
        self.store
            .append(&self.session, &Message::text(Role::User, text))
            .unwrap();
    }

    fn plan(&mut self, current: usize) {
        let todos = (1..=3)
            .map(|n| {
                let status = match n.cmp(&current) {
                    std::cmp::Ordering::Less => "completed",
                    std::cmp::Ordering::Equal => "in_progress",
                    std::cmp::Ordering::Greater => "pending",
                };
                json!({"content": format!("edit part {n}"), "status": status})
            })
            .collect::<Vec<_>>();
        self.record(
            "todo_write",
            json!({ "todos": todos }),
            ToolOutcome::Succeeded,
        );
    }

    /// Reads of files never read before, so every one is new evidence.
    fn fresh_reads(&mut self, count: usize) {
        for _ in 0..count {
            let path = format!("src/file{}.rs", self.calls);
            self.record("read_file", json!({ "path": path }), ToolOutcome::Succeeded);
        }
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
            max_rounds: Some(20),
        };
        let mut events = Vec::new();
        agent
            .run(
                &mut self.store,
                &mut |event| {
                    if matches!(
                        event,
                        AgentEvent::ProgressNudge { .. } | AgentEvent::EditFocus { .. }
                    ) {
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

#[tokio::test]
async fn halfway_reminder_suggests_a_plan_with_every_tool_still_available() {
    let mut fixture = Fixture::new();
    fixture.reads("a.txt", 3);
    fixture.reads("./b.txt", 2);
    let (requests, _) = fixture.run(ApprovalMode::Trust, vec![done()]).await;
    // Below the halfway mark: request-only packets may trail the history, but
    // no runtime note does.
    assert!(!last_text(&requests[0]).starts_with("[Builder runtime note"));
    assert!(requests[0].0.iter().all(|m| {
        m.role != Role::User
            || !m
                .content
                .as_deref()
                .unwrap_or("")
                .starts_with("[Builder runtime note")
    }));

    let mut fixture = Fixture::new();
    fixture.reads("a.txt", 3);
    fixture.reads("./b.txt", 7);
    let (requests, events) = fixture.run(ApprovalMode::Trust, vec![done()]).await;
    assert!(events.is_empty());
    let note = last_text(&requests[0]);
    assert!(note.starts_with("[Builder runtime note"), "{note}");
    assert!(note.contains("no todo list yet"), "{note}");
    assert!(
        note.contains(
            "10 successful reads of 2 files, 8 of them repeats: b.txt ×7 (latest result still in context), a.txt ×3 (latest result still in context)."
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
    fixture.reads("a.txt", 13);
    // This new file result resets stagnation, so discovery tools return.
    let replies = vec![call("more", "read_file", json!({"path":"b.txt"})), done()];
    let (requests, events) = fixture.run(ApprovalMode::Trust, replies).await;
    assert_eq!(requests.len(), 2);
    let tools = tool_names(&requests[0]);
    assert!(tools.contains(&"read_file") && tools.contains(&"todo_write"));
    assert!(tools.contains(&"write_file") && tools.contains(&"shell"));
    for paused in ["list_files", "search", "subagent"] {
        assert!(!tools.contains(&paused), "{paused} still offered");
    }
    let first = last_text(&requests[0]);
    assert!(
        first.contains("Progress check: 12 calls without new inspection evidence"),
        "{first}"
    );
    assert!(first.contains("a.txt ×13"), "{first}");
    assert!(
        first.contains("1. Recommended for an implementation request"),
        "{first}"
    );
    assert!(first.contains("call todo_write now"), "{first}");
    assert!(!first.contains("without a plan"), "{first}");
    let second_tools = tool_names(&requests[1]);
    for name in ["list_files", "search", "subagent"] {
        assert!(
            second_tools.contains(&name),
            "{name} missing from {second_tools:?}"
        );
    }
    assert!(matches!(
        events.as_slice(),
        [AgentEvent::ProgressNudge {
            calls: 12,
            step: None,
            repeated_reads: 12,
            planning: true,
        }]
    ));
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
            repeated_reads: 12,
            planning: true,
        }]
    ));
}

#[tokio::test]
async fn the_nudge_grows_firmer_and_is_shown_again_while_no_plan_exists() {
    let mut fixture = Fixture::new();
    fixture.reads("a.txt", 25);
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
    fixture.reads("a.txt", 13);
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
    fixture.reads("a.txt", 7);
    let (requests, _) = fixture.run(ApprovalMode::Trust, vec![done()]).await;
    let note = last_text(&requests[0]);
    assert!(
        note.contains("7 successful reads of 1 file, 6 of them repeats: a.txt ×7"),
        "{note}"
    );
    assert!(!note.contains("b.txt"), "{note}");
}

#[tokio::test]
async fn read_only_sessions_are_nudged_to_answer_instead_of_plan() {
    let mut fixture = Fixture::new();
    fixture.reads("a.txt", 13);
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

#[tokio::test]
async fn a_run_has_no_round_budget_unless_one_is_requested() {
    let mut f = Fixture::new();
    // Twenty more rounds than the retired default of 100, each with new
    // evidence, so only a round budget could stop the run.
    let rounds = 120;
    let mut replies = Vec::new();
    for n in 0..rounds {
        let path = format!("part{n}.txt");
        std::fs::write(f.root.path().join(&path), format!("part {n}\n")).unwrap();
        replies.push(call(&format!("r{n}"), "read_file", json!({ "path": path })));
    }
    replies.push(done());
    let agent = Agent {
        provider: Replies {
            replies: Mutex::new(replies),
            requests: Mutex::new(Vec::new()),
        },
        memory: None,
        profile: Profile {
            context_tokens: 400_000,
            auto_compact: false,
            ..Profile::default()
        },
        workspace: Workspace::new(f.root.path()).unwrap(),
        session: f.session.clone(),
        approval: ApprovalMode::Trust,
        max_rounds: None,
    };
    agent
        .run(&mut f.store, &mut |_| {}, &mut |_| true)
        .await
        .unwrap();
    assert_eq!(agent.provider.requests.lock().unwrap().len(), rounds + 1);
    assert_eq!(
        f.store
            .messages(&f.session)
            .unwrap()
            .last()
            .and_then(|message| message.content.as_deref()),
        Some("done")
    );
}

const EDITS: [&str; 4] = ["write_file", "edit_file", "multi_edit", "todo_write"];
const CIRCLING: [&str; 4] = ["list_files", "search", "subagent", "shell"];

fn assert_offers(request: &Request, offered: &[&str], withheld: &[&str]) {
    let tools = tool_names(request);
    for name in offered {
        assert!(tools.contains(name), "{name} missing from {tools:?}");
    }
    for name in withheld {
        assert!(!tools.contains(name), "{name} still offered in {tools:?}");
    }
}

#[tokio::test]
async fn new_reads_do_not_hide_a_step_that_never_reaches_an_edit() {
    let mut fixture = Fixture::new();
    fixture.plan(2);
    fixture.fresh_reads(15);
    let (requests, events) = fixture.run(ApprovalMode::Trust, vec![done()]).await;
    assert_offers(&requests[0], &CIRCLING, &[]);
    assert!(events.is_empty(), "{events:?}");

    let mut fixture = Fixture::new();
    fixture.plan(2);
    fixture.fresh_reads(16);
    let (requests, events) = fixture.run(ApprovalMode::Trust, vec![done()]).await;
    assert_offers(
        &requests[0],
        &[&EDITS[..], &["read_file"]].concat(),
        &CIRCLING,
    );
    let text = last_text(&requests[0]);
    assert!(
        text.contains(
            "Todo item 2 of 3 (edit part 2) has taken 16 tool calls without a file change"
        ),
        "{text}"
    );
    assert!(
        text.contains("Search, shell and subagents are paused"),
        "{text}"
    );
    assert!(
        matches!(
            events.as_slice(),
            [AgentEvent::EditFocus {
                calls: 16,
                step: (2, 3),
                reads: true,
            }]
        ),
        "{events:?}"
    );
}

#[tokio::test]
async fn at_twice_the_focus_budget_only_edits_and_plan_updates_remain() {
    let mut fixture = Fixture::new();
    fixture.plan(1);
    fixture.fresh_reads(32);
    let (requests, events) = fixture.run(ApprovalMode::Trust, vec![done()]).await;
    assert_offers(
        &requests[0],
        &EDITS,
        &[&CIRCLING[..], &["read_file"]].concat(),
    );
    let text = last_text(&requests[0]);
    assert!(
        text.contains("Only edits and todo_write are available"),
        "{text}"
    );
    assert!(
        text.contains("tell the user the concrete blocker"),
        "{text}"
    );
    assert!(matches!(
        events.as_slice(),
        [AgentEvent::EditFocus {
            calls: 32,
            reads: false,
            ..
        }]
    ));
}

#[tokio::test]
async fn continue_does_not_reset_the_focus_budget() {
    let mut fixture = Fixture::new();
    fixture.plan(1);
    fixture.fresh_reads(10);
    fixture.say("continue");
    fixture.fresh_reads(6);
    let (requests, _) = fixture.run(ApprovalMode::Trust, vec![done()]).await;
    assert_offers(&requests[0], &EDITS, &CIRCLING);
    assert!(last_text(&requests[0]).contains("has taken 16 tool calls"));
}

#[tokio::test]
async fn an_edit_or_a_completed_step_restores_every_tool() {
    let mut fixture = Fixture::new();
    fixture.plan(1);
    fixture.fresh_reads(40);
    fixture.record(
        "write_file",
        json!({"path":"a.txt","content":"new\n"}),
        ToolOutcome::Changed,
    );
    fixture.fresh_reads(3);
    let (requests, events) = fixture.run(ApprovalMode::Trust, vec![done()]).await;
    assert_offers(&requests[0], &CIRCLING, &[]);
    assert!(events.is_empty(), "{events:?}");

    let mut fixture = Fixture::new();
    fixture.plan(1);
    fixture.fresh_reads(40);
    fixture.plan(2);
    fixture.fresh_reads(3);
    let (requests, _) = fixture.run(ApprovalMode::Trust, vec![done()]).await;
    assert_offers(&requests[0], &CIRCLING, &[]);

    // Rewriting the list without moving its current item is not progress.
    let mut fixture = Fixture::new();
    fixture.plan(1);
    fixture.fresh_reads(10);
    fixture.plan(1);
    fixture.fresh_reads(5);
    let (requests, _) = fixture.run(ApprovalMode::Trust, vec![done()]).await;
    assert!(last_text(&requests[0]).contains("has taken 16 tool calls"));
}

#[tokio::test]
async fn a_failed_edit_brings_reads_back_so_it_can_be_rebuilt() {
    let mut fixture = Fixture::new();
    fixture.plan(1);
    fixture.fresh_reads(32);
    fixture.record(
        "edit_file",
        json!({"path":"a.txt","old":"gamma","new":"delta"}),
        ToolOutcome::Failed,
    );
    let (requests, _) = fixture.run(ApprovalMode::Trust, vec![done()]).await;
    assert_offers(
        &requests[0],
        &[&EDITS[..], &["read_file"]].concat(),
        &CIRCLING,
    );
    assert!(last_text(&requests[0]).contains("your last edit failed"));
}

#[tokio::test]
async fn read_only_sessions_and_sessions_without_a_plan_keep_their_tools() {
    let mut fixture = Fixture::new();
    fixture.plan(1);
    fixture.fresh_reads(40);
    let (requests, events) = fixture.run(ApprovalMode::ReadOnly, vec![done()]).await;
    assert!(tool_names(&requests[0]).contains(&"read_file"));
    assert!(events.is_empty(), "{events:?}");

    let mut fixture = Fixture::new();
    fixture.fresh_reads(40);
    let (requests, events) = fixture.run(ApprovalMode::Trust, vec![done()]).await;
    assert_offers(&requests[0], &CIRCLING, &[]);
    assert!(events.is_empty(), "{events:?}");
}

#[tokio::test]
async fn evidence_already_seen_stays_seen_after_continue() {
    let mut fixture = Fixture::new();
    fixture.reads("a.txt", 3);
    fixture.say("continue");
    fixture.reads("a.txt", 12);
    let (requests, _) = fixture.run(ApprovalMode::Trust, vec![done()]).await;
    let text = last_text(&requests[0]);
    assert!(
        text.contains("Progress check: 12 calls without new inspection evidence"),
        "{text}"
    );
}

/// The saved history is a prefix of every request. Request-only context
/// (guidance, packets, budget notice, continuation) is regenerated per round,
/// so it trails the history; placing it first would change the prompt prefix
/// every round and defeat a server's prompt cache.
#[tokio::test]
async fn saved_history_is_the_prefix_of_every_request() {
    let mut fixture = Fixture::new();
    fixture.reads("a.txt", 3);
    fixture.reads("./b.txt", 7);
    let history = fixture.store.messages(&fixture.session).unwrap();
    let (requests, _) = fixture.run(ApprovalMode::Trust, vec![done()]).await;
    let request = &requests[0].0;
    assert!(request.len() > history.len(), "a runtime note was expected");
    // Only stable, per-session system notices may precede the history (they
    // merge into the system block); the history itself is contiguous and
    // untouched; everything after it is request-only.
    let start = request.iter().position(|m| m == &history[0]).unwrap();
    assert!(request[..start].iter().all(|m| m.role == Role::System));
    assert_eq!(&request[start..start + history.len()], &history[..]);
    assert_eq!(history[0].content.as_deref(), Some(SYSTEM));
    assert!(
        request[start + history.len()..]
            .iter()
            .all(|m| m.tool_call_id.is_none())
    );
    assert!(last_text(&requests[0]).starts_with("[Builder runtime note"));
}
