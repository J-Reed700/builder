//! Compaction must leave an agent able to edit from exact source, without
//! asking the model to reconstruct it from a lossy summary.
use builder::agent::{Agent, ApprovalMode, estimate_tokens};
use builder_core::{
    config::Profile,
    protocol::{Function, Message, Role, ToolCall},
    store::{Store, ToolOutcome},
};
use builder_provider::{Event, Provider};
use builder_tools::{Action, Workspace};
use serde_json::{Value, json};
use std::sync::Mutex;

#[derive(Default)]
struct Model {
    requests: Mutex<Vec<Vec<Message>>>,
    edit: Mutex<bool>,
    change_during_summary: Mutex<Option<std::path::PathBuf>>,
}
impl Provider for Model {
    async fn complete(
        &self,
        messages: &[Message],
        tools: &[Value],
        _: &mut dyn FnMut(Event),
    ) -> anyhow::Result<Message> {
        self.requests.lock().unwrap().push(messages.to_vec());
        if tools.is_empty() {
            if let Some(path) = self.change_during_summary.lock().unwrap().take() {
                std::fs::write(path, "external change during summary\n")?;
            }
            // Deliberately forget all source in the model-written summary.
            return Ok(Message::text(
                Role::Assistant,
                "The next action is to edit rate.txt.",
            ));
        }
        if !std::mem::replace(&mut *self.edit.lock().unwrap(), true) {
            let source = messages
                .iter()
                .find(|m| {
                    m.role == Role::Tool
                        && m.content
                            .as_deref()
                            .is_some_and(|t| t.contains("|old rate"))
                })
                .expect("exact source must survive compaction");
            assert!(
                source
                    .content
                    .as_deref()
                    .unwrap()
                    .contains("Source-SHA256:")
            );
            return Ok(call(
                "edit",
                "edit_file",
                json!({"path":"rate.txt","old":"old rate","new":"new rate"}),
            ));
        }
        Ok(Message::text(Role::Assistant, "Updated rate.txt."))
    }
}
fn call(id: &str, name: &str, arguments: Value) -> Message {
    let mut m = Message::text(Role::Assistant, "");
    m.tool_calls.push(ToolCall {
        id: id.into(),
        kind: "function".into(),
        function: Function {
            name: name.into(),
            arguments: arguments.to_string(),
        },
    });
    m
}
struct Fixture {
    home: tempfile::TempDir,
    store: Store,
    agent: Agent<Model>,
}
impl Fixture {
    fn new() -> Self {
        let home = tempfile::tempdir().unwrap();
        let workspace = Workspace::new(home.path()).unwrap();
        let mut store = Store::open(home.path()).unwrap();
        let session = store
            .create("working set", "test", home.path(), "system")
            .unwrap();
        store
            .append(
                &session,
                &Message::text(Role::User, "Change old rate to new rate in rate.txt"),
            )
            .unwrap();
        let mut profile = Profile::default();
        profile.pipeline.subagents = false;
        let agent = Agent {
            provider: Model::default(),
            memory: None,
            profile,
            workspace,
            session,
            approval: ApprovalMode::Trust,
            max_rounds: 10,
        };
        Self { home, store, agent }
    }
    fn read(&mut self, id: &str, path: &str) {
        let action = Action::ReadFile {
            path: path.into(),
            start_line: None,
            end_line: None,
        };
        let result = self.agent.workspace.inspect(&action).unwrap();
        self.store
            .append(
                &self.agent.session,
                &call(id, "read_file", json!({"path":path})),
            )
            .unwrap();
        self.store.claim_tool(&self.agent.session, id).unwrap();
        self.store
            .complete_tool_with_outcome(&self.agent.session, id, &result, ToolOutcome::Succeeded)
            .unwrap();
    }
    fn pad(&mut self) {
        for _ in 0..8 {
            let id = format!(
                "search_{}",
                self.store
                    .history_messages(&self.agent.session)
                    .unwrap()
                    .len()
            );
            self.store
                .append(
                    &self.agent.session,
                    &call(&id, "search", json!({"query":"rate"})),
                )
                .unwrap();
            self.store.claim_tool(&self.agent.session, &id).unwrap();
            self.store
                .complete_tool_with_outcome(
                    &self.agent.session,
                    &id,
                    &"earlier investigation ".repeat(300),
                    ToolOutcome::Succeeded,
                )
                .unwrap();
        }
    }
    async fn compact(&mut self) -> Vec<Message> {
        // Push reads out of the normal token-bounded tail, leaving enough material
        // for compaction to reclaim even when the summarizer knows nothing.
        self.pad();
        let before = estimate_tokens(&self.store.messages(&self.agent.session).unwrap());
        assert!(
            self.agent
                .compact(&mut self.store, &mut |_| {})
                .await
                .unwrap()
        );
        let active = self.store.messages(&self.agent.session).unwrap();
        assert!(estimate_tokens(&active) < before);
        active
    }
}
fn reads(messages: &[Message]) -> Vec<&Message> {
    messages
        .iter()
        .filter(|m| {
            m.role == Role::Tool && m.content.as_deref().is_some_and(|s| s.starts_with("File:"))
        })
        .collect()
}

#[tokio::test]
async fn repeated_compaction_and_restart_preserve_source_for_immediate_edit() {
    let mut f = Fixture::new();
    std::fs::write(f.home.path().join("rate.txt"), "old rate\n").unwrap();
    f.read("read", "rate.txt");
    for _ in 0..2 {
        let active = f.compact().await;
        assert_eq!(reads(&active).len(), 1);
        assert!(
            reads(&active)[0]
                .content
                .as_deref()
                .unwrap()
                .contains("|old rate")
        );
        f.store = Store::open(f.home.path()).unwrap();
        assert_eq!(f.store.messages(&f.agent.session).unwrap(), active);
    }
    f.agent
        .run(&mut f.store, &mut |_| {}, &mut |_| true)
        .await
        .unwrap();
    assert_eq!(
        std::fs::read_to_string(f.home.path().join("rate.txt")).unwrap(),
        "new rate\n"
    );
    let history = f.store.history_messages(&f.agent.session).unwrap();
    assert_eq!(
        history
            .iter()
            .flat_map(|m| &m.tool_calls)
            .filter(|c| c.function.name == "read_file")
            .count(),
        1,
        "no additional reads required"
    );
}

#[tokio::test]
async fn changed_or_deleted_source_is_not_restored_as_current() {
    for deleted in [false, true] {
        let mut f = Fixture::new();
        let path = f.home.path().join("rate.txt");
        std::fs::write(&path, "old rate\n").unwrap();
        f.read("read", "rate.txt");
        if deleted {
            std::fs::remove_file(&path).unwrap();
        } else {
            std::fs::write(&path, "external change\n").unwrap();
        }
        assert!(reads(&f.compact().await).is_empty());
    }
}

#[tokio::test]
async fn repeated_reads_are_deduplicated_and_working_set_is_bounded() {
    let mut f = Fixture::new();
    f.agent.profile.context_tokens = 131_072;
    for n in 0..12 {
        let path = format!("file{n}.txt");
        std::fs::write(f.home.path().join(&path), format!("file {n}\n")).unwrap();
        f.read(&format!("read{n}"), &path);
    }
    f.read("duplicate", "file11.txt");
    let active = f.compact().await;
    let restored = reads(&active);
    assert_eq!(restored.len(), 8);
    assert_eq!(
        restored
            .iter()
            .filter(|m| m.content.as_deref().unwrap().contains("file11.txt"))
            .count(),
        1
    );
    // Every retained result has its matching call; no fabricated execution IDs.
    for result in restored {
        assert!(
            active
                .iter()
                .flat_map(|m| &m.tool_calls)
                .any(|call| Some(&call.id) == result.tool_call_id.as_ref())
        );
    }
}

#[tokio::test]
async fn existing_tail_read_is_not_restored_twice() {
    let mut f = Fixture::new();
    std::fs::write(f.home.path().join("rate.txt"), "old rate\n").unwrap();
    for _ in 0..8 {
        f.store
            .append(
                &f.agent.session,
                &Message::text(Role::Assistant, "old investigation ".repeat(300)),
            )
            .unwrap();
    }
    f.read("read", "rate.txt");
    assert!(f.agent.compact(&mut f.store, &mut |_| {}).await.unwrap());
    assert_eq!(reads(&f.store.messages(&f.agent.session).unwrap()).len(), 1);
}

#[tokio::test]
async fn source_is_checked_after_the_summarizer_finishes() {
    let mut f = Fixture::new();
    let path = f.home.path().join("rate.txt");
    std::fs::write(&path, "old rate\n").unwrap();
    f.read("read", "rate.txt");
    *f.agent.provider.change_during_summary.lock().unwrap() = Some(path);
    assert!(reads(&f.compact().await).is_empty());
}

#[tokio::test]
async fn oversized_read_is_omitted_without_truncating_exact_source() {
    let mut f = Fixture::new();
    f.agent.profile.context_tokens = 32_768;
    std::fs::write(
        f.home.path().join("large.txt"),
        "long source line ".repeat(5).repeat(100),
    )
    .unwrap();
    f.read("large", "large.txt");
    std::fs::write(f.home.path().join("rate.txt"), "old rate\n").unwrap();
    f.read("small", "rate.txt");
    let active = f.compact().await;
    let kept = reads(&active);
    assert_eq!(kept.len(), 1);
    assert!(kept[0].content.as_deref().unwrap().contains("|old rate"));
}

#[tokio::test]
async fn edit_attribution_survives_forgetful_summaries_and_restart() {
    let mut f = Fixture::new();
    for (id, outcome) in [
        ("changed", ToolOutcome::Changed),
        ("noop", ToolOutcome::Succeeded),
        ("denied", ToolOutcome::Denied),
        ("failed", ToolOutcome::Failed),
        ("uncertain", ToolOutcome::Uncertain),
    ] {
        f.store
            .append(
                &f.agent.session,
                &call(
                    id,
                    "edit_file",
                    json!({
                        "path": format!("{id}.cpp"), "old": "before", "new": "after"
                    }),
                ),
            )
            .unwrap();
        f.store.claim_tool(&f.agent.session, id).unwrap();
        f.store
            .complete_tool_with_outcome(&f.agent.session, id, "tool result", outcome)
            .unwrap();
    }
    for _ in 0..2 {
        let active = f.compact().await;
        let handoff = active
            .iter()
            .filter_map(|m| m.content.as_deref())
            .find(|s| s.contains("[Verified edit history"))
            .unwrap();
        let inventory = handoff.split("[Verified edit history").nth(1).unwrap();
        assert!(inventory.contains("changed.cpp"));
        assert!(inventory.contains("latest_changed_call_id"));
        let edit = active
            .iter()
            .flat_map(|m| &m.tool_calls)
            .find(|c| c.id == "changed")
            .expect("exact successful edit must survive the forgetful summary");
        assert_eq!(
            serde_json::from_str::<Value>(&edit.function.arguments).unwrap()["new"],
            "after"
        );
        for path in ["noop.cpp", "denied.cpp", "failed.cpp", "uncertain.cpp"] {
            assert!(
                !inventory.contains(path),
                "{path} must not be attributed as a change"
            );
        }
        f.store = Store::open(f.home.path()).unwrap();
    }
}

#[tokio::test]
async fn exact_edits_roll_forward_without_accumulating_across_checkpoints() {
    let mut f = Fixture::new();
    for cycle in 0..3 {
        for n in 0..6 {
            let id = format!("edit_{cycle}_{n}");
            f.store.append(&f.agent.session, &call(&id, "edit_file", json!({
                "path": "rate.txt", "old": format!("old_{cycle}_{n}"), "new": format!("new_{cycle}_{n}")
            }))).unwrap();
            f.store.claim_tool(&f.agent.session, &id).unwrap();
            f.store
                .complete_tool_with_outcome(&f.agent.session, &id, "changed", ToolOutcome::Changed)
                .unwrap();
        }
        let active = f.compact().await;
        let edits: Vec<_> = active
            .iter()
            .filter(|m| {
                m.content
                    .as_deref()
                    .is_some_and(|s| s.starts_with("[Restored successful file mutation."))
            })
            .collect();
        assert!((1..=4).contains(&edits.len()));
        let ids: Vec<_> = edits.iter().map(|m| m.tool_calls[0].id.clone()).collect();
        assert_eq!(
            ids,
            ((6 - edits.len())..6)
                .map(|n| format!("edit_{cycle}_{n}"))
                .collect::<Vec<_>>()
        );
        let pairs: Vec<_> = active
            .iter()
            .filter(|m| {
                edits.iter().any(|e| std::ptr::eq(*e, *m))
                    || m.tool_call_id.as_ref().is_some_and(|id| ids.contains(id))
            })
            .cloned()
            .collect();
        assert!(estimate_tokens(&pairs) <= 2048);
        f.store = Store::open(f.home.path()).unwrap();
    }
}
