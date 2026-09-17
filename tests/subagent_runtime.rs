//! Subagents: parallel, read-only, durable child sessions whose reports are
//! the only thing the parent's context receives.
use builder::agent::{Agent, AgentEvent, ApprovalMode, SYSTEM};
use builder_core::{
    config::Profile,
    protocol::{Function, Message, Role, ToolCall},
    store::{Store, ToolOutcome},
};
use builder_provider::{Event, Provider};
use builder_tools::Workspace;
use serde_json::{Value, json};
use std::{collections::HashMap, sync::Mutex, time::Duration};

/// Each model request: whether a subagent sent it, its messages and tools.
type Recorded = Vec<(bool, Vec<Message>, Vec<Value>)>;

/// Answers the parent from a script and each subagent from a script keyed
/// by its delegated prompt. Children can be required to overlap.
struct Router {
    parent: Mutex<Vec<Message>>,
    children: Mutex<HashMap<String, Vec<Message>>>,
    requests: Mutex<Recorded>,
    overlap: Option<tokio::sync::Barrier>,
}

impl Router {
    fn new(parent: Vec<Message>, children: Vec<(&str, Vec<Message>)>) -> Self {
        Self {
            parent: Mutex::new(parent),
            children: Mutex::new(
                children
                    .into_iter()
                    .map(|(prompt, replies)| (prompt.to_owned(), replies))
                    .collect(),
            ),
            requests: Mutex::new(vec![]),
            overlap: None,
        }
    }
    fn child_requests(&self) -> Vec<(Vec<Message>, Vec<Value>)> {
        self.requests
            .lock()
            .unwrap()
            .iter()
            .filter(|(child, _, _)| *child)
            .map(|(_, messages, tools)| (messages.clone(), tools.clone()))
            .collect()
    }
    fn parent_requests(&self) -> Vec<(Vec<Message>, Vec<Value>)> {
        self.requests
            .lock()
            .unwrap()
            .iter()
            .filter(|(child, _, _)| !*child)
            .map(|(_, messages, tools)| (messages.clone(), tools.clone()))
            .collect()
    }
}

impl Provider for Router {
    async fn complete(
        &self,
        messages: &[Message],
        tools: &[Value],
        _: &mut dyn FnMut(Event),
    ) -> anyhow::Result<Message> {
        let prompt = messages
            .iter()
            .find(|message| message.role == Role::User)
            .and_then(|message| message.content.clone())
            .unwrap_or_default();
        let child = messages.iter().any(|message| {
            message.role == Role::System
                && message
                    .content
                    .as_deref()
                    .is_some_and(|text| text.contains("Builder research subagent"))
        });
        let first_child_request = child
            && !messages
                .iter()
                .any(|message| message.role == Role::Assistant);
        self.requests
            .lock()
            .unwrap()
            .push((child, messages.to_vec(), tools.to_vec()));
        if first_child_request && let Some(overlap) = &self.overlap {
            tokio::time::timeout(Duration::from_secs(5), overlap.wait())
                .await
                .map_err(|_| anyhow::anyhow!("subagents did not run concurrently"))?;
        }
        let reply = if child {
            let mut children = self.children.lock().unwrap();
            let script = children
                .get_mut(&prompt)
                .ok_or_else(|| anyhow::anyhow!("no script for subagent prompt {prompt:?}"))?;
            anyhow::ensure!(!script.is_empty(), "unexpected subagent request");
            script.remove(0)
        } else {
            let mut parent = self.parent.lock().unwrap();
            anyhow::ensure!(!parent.is_empty(), "unexpected parent request");
            parent.remove(0)
        };
        Ok(reply)
    }
}

fn calls(calls: &[(&str, &str, Value)]) -> Message {
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

fn delegate(id: &str, description: &str, prompt: &str) -> (String, &'static str, Value) {
    (
        id.to_owned(),
        "subagent",
        json!({"description": description, "prompt": prompt}),
    )
}

struct Fixture {
    home: tempfile::TempDir,
    root: tempfile::TempDir,
    store: Store,
    session: String,
}

impl Fixture {
    fn new() -> Self {
        let home = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("notes.txt"), "RAW-FILE-CONTENT\n").unwrap();
        let mut store = Store::open(home.path()).unwrap();
        let session = store
            .create(
                "parent",
                "test",
                Workspace::new(root.path()).unwrap().root(),
                SYSTEM,
            )
            .unwrap();
        store
            .append(
                &session,
                &Message::text(Role::User, "investigate the notes"),
            )
            .unwrap();
        Self {
            home,
            root,
            store,
            session,
        }
    }

    fn agent(&self, profile: Profile, provider: Router) -> Agent<Router> {
        Agent {
            provider,
            memory: None,
            profile,
            workspace: Workspace::new(self.root.path()).unwrap(),
            session: self.session.clone(),
            approval: ApprovalMode::Trust,
            max_rounds: 6,
        }
    }

    async fn run(&mut self, agent: &Agent<Router>) -> (anyhow::Result<()>, Vec<String>) {
        let mut events = Vec::new();
        let result = agent
            .run(
                &mut self.store,
                &mut |event| match event {
                    AgentEvent::ToolStarted { name, detail } => {
                        events.push(format!("start {name} {detail}"))
                    }
                    AgentEvent::ToolFinished {
                        name, note, failed, ..
                    } => events.push(format!(
                        "finish {name} failed={failed} {}",
                        note.unwrap_or_default()
                    )),
                    AgentEvent::SubagentProgress {
                        description,
                        actions,
                        ..
                    } => events.push(format!("progress {description} {actions}")),
                    _ => {}
                },
                &mut |_| panic!("subagents and reads need no approval"),
            )
            .await;
        (result, events)
    }
}

fn tool_names(tools: &[Value]) -> Vec<&str> {
    tools
        .iter()
        .filter_map(|tool| tool["function"]["name"].as_str())
        .collect()
}

#[tokio::test]
async fn parallel_subagents_return_reports_without_their_raw_reads() {
    let mut f = Fixture::new();
    let (a, b) = (
        delegate("sub-a", "read the notes", "Report what notes.txt says."),
        delegate("sub-b", "check the layout", "List the workspace files."),
    );
    let mut router = Router::new(
        vec![
            calls(&[(&a.0, a.1, a.2), (&b.0, b.1, b.2)]),
            Message::text(Role::Assistant, "Both investigations are done."),
        ],
        vec![
            (
                "Report what notes.txt says.",
                vec![
                    calls(&[("a-read", "read_file", json!({"path":"notes.txt"}))]),
                    Message::text(Role::Assistant, "notes.txt:1 holds the marker."),
                ],
            ),
            (
                "List the workspace files.",
                vec![Message::text(Role::Assistant, "Only notes.txt exists.")],
            ),
        ],
    );
    router.overlap = Some(tokio::sync::Barrier::new(2));
    let agent = f.agent(Profile::default(), router);
    let (result, events) = f.run(&agent).await;
    result.unwrap();
    assert_eq!(
        events,
        [
            "start subagent read the notes",
            "start subagent check the layout",
            "progress read the notes 1",
            "finish subagent failed=false 1 tool calls",
            "finish subagent failed=false 0 tool calls",
        ]
    );

    let parent = agent.provider.parent_requests();
    assert!(tool_names(&parent[0].1).contains(&"subagent"));
    let context = serde_json::to_string(&parent[1].0).unwrap();
    assert!(context.contains("notes.txt:1 holds the marker."));
    assert!(context.contains("Only notes.txt exists."));
    assert!(
        !context.contains("RAW-FILE-CONTENT"),
        "a subagent's reads must stay in its own context"
    );

    let children = agent.provider.child_requests();
    assert_eq!(children.len(), 3);
    for (_, tools) in &children {
        for name in tool_names(tools) {
            assert!(
                ["list_files", "read_file", "search", "code_search"].contains(&name),
                "subagent was offered {name}"
            );
        }
    }
    assert!(
        serde_json::to_string(&children)
            .unwrap()
            .contains("RAW-FILE-CONTENT")
    );

    let outcomes = f.store.tool_outcomes(&f.session).unwrap();
    assert_eq!(outcomes["sub-a"], ToolOutcome::Succeeded);
    assert_eq!(outcomes["sub-b"], ToolOutcome::Succeeded);

    // Child sessions are durable and inspectable, but never listed as chats.
    let listed = f.store.sessions().unwrap();
    assert_eq!(listed.len(), 1);
    let report = f.store.tool_result(&f.session, "sub-a").unwrap().unwrap();
    let child = report
        .lines()
        .next()
        .unwrap()
        .rsplit("session ")
        .next()
        .unwrap();
    let resolved = f.store.resolve(child).unwrap();
    assert!(resolved.title.starts_with("subagent · read the notes"));
    assert!(f.store.is_subagent(&resolved.id).unwrap());
    let reopened = Store::open(f.home.path()).unwrap();
    assert_eq!(reopened.sessions().unwrap().len(), 1);
    assert!(
        reopened
            .history_messages(&resolved.id)
            .unwrap()
            .iter()
            .any(|message| message.content.as_deref() == Some("notes.txt:1 holds the marker."))
    );
}

#[tokio::test]
async fn a_subagent_cannot_change_the_workspace_even_when_it_asks() {
    let mut f = Fixture::new();
    let task = delegate("sub", "look around", "Inspect the workspace.");
    let router = Router::new(
        vec![
            calls(&[(&task.0, task.1, task.2)]),
            Message::text(Role::Assistant, "Reported."),
        ],
        vec![(
            "Inspect the workspace.",
            vec![
                calls(&[
                    (
                        "w",
                        "write_file",
                        json!({"path":"created.txt","content":"x"}),
                    ),
                    ("s", "shell", json!({"command":"touch shell.txt"})),
                ]),
                Message::text(Role::Assistant, "I could only read."),
            ],
        )],
    );
    // Even a parent trusted with every tool delegates read-only work.
    let agent = f.agent(Profile::default(), router);
    let (result, _) = f.run(&agent).await;
    result.unwrap();
    assert!(!f.root.path().join("created.txt").exists());
    assert!(!f.root.path().join("shell.txt").exists());
    let report = f.store.tool_result(&f.session, "sub").unwrap().unwrap();
    assert!(report.contains("I could only read."), "{report}");
    let child_context = serde_json::to_string(&agent.provider.child_requests()[1].0).unwrap();
    assert!(child_context.contains("DENIED"), "{child_context}");
}

#[tokio::test]
async fn a_subagent_that_runs_out_of_rounds_still_reports() {
    let mut f = Fixture::new();
    let task = delegate("sub", "dig deep", "Keep reading.");
    let router = Router::new(
        vec![
            calls(&[(&task.0, task.1, task.2)]),
            Message::text(Role::Assistant, "Used the partial report."),
        ],
        vec![(
            "Keep reading.",
            vec![
                calls(&[("r1", "read_file", json!({"path":"notes.txt"}))]),
                Message::text(Role::Assistant, "Partial: notes.txt has one line."),
            ],
        )],
    );
    let mut profile = Profile::default();
    profile.pipeline.subagent_rounds = 1;
    let agent = f.agent(profile, router);
    let (result, _) = f.run(&agent).await;
    result.unwrap();
    let report = f.store.tool_result(&f.session, "sub").unwrap().unwrap();
    assert!(
        report.contains("Partial: notes.txt has one line."),
        "{report}"
    );
    let children = agent.provider.child_requests();
    let (conclusion, tools) = children.last().unwrap();
    assert!(tools.is_empty(), "the fallback report request is tool-free");
    assert!(
        conclusion
            .last()
            .and_then(|message| message.content.as_deref())
            .is_some_and(|text| text.contains("Stop investigating"))
    );
}

#[tokio::test]
async fn disabled_subagents_are_hidden_and_reject_queued_calls() {
    let mut f = Fixture::new();
    let task = delegate("queued", "anything", "Anything.");
    f.store
        .append(&f.session, &calls(&[(&task.0, task.1, task.2)]))
        .unwrap();
    let mut profile = Profile::default();
    profile.pipeline.subagents = false;
    let agent = f.agent(
        profile,
        Router::new(
            vec![Message::text(Role::Assistant, "Worked without delegation.")],
            vec![],
        ),
    );
    let (result, events) = f.run(&agent).await;
    result.unwrap();
    assert_eq!(
        events[1],
        "finish subagent failed=true Subagents are disabled by pipeline configuration"
    );
    assert!(
        f.store
            .tool_result(&f.session, "queued")
            .unwrap()
            .unwrap()
            .contains("disabled by pipeline configuration")
    );
    assert!(!tool_names(&agent.provider.parent_requests()[0].1).contains(&"subagent"));
    assert!(agent.provider.child_requests().is_empty());
}
