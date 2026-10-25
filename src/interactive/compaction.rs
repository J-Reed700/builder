use anyhow::{Context, Result};
use builder::{
    agent::{Agent, AgentEvent},
    ui,
};
use builder_core::{config::Config, store::Store};
use builder_provider::Endpoint;

struct CompactionState {
    status: std::sync::Mutex<String>,
    result: std::sync::Mutex<Option<std::result::Result<bool, String>>>,
}

pub(super) struct CompactionTask {
    state: std::sync::Arc<CompactionState>,
    cancel: Option<tokio::sync::oneshot::Sender<()>>,
    worker: Option<std::thread::JoinHandle<()>>,
}

impl CompactionTask {
    pub(super) fn status(&self) -> String {
        self.state
            .status
            .lock()
            .map(|status| status.clone())
            .unwrap_or_else(|_| "Compaction status unavailable".into())
    }

    fn finished(&self) -> bool {
        self.state
            .result
            .lock()
            .is_ok_and(|result| result.is_some())
    }

    pub(super) fn cancel(&mut self) {
        if let Some(cancel) = self.cancel.take() {
            let _ = cancel.send(());
        }
    }

    fn finish(mut self) -> Result<bool> {
        if let Some(worker) = self.worker.take() {
            worker
                .join()
                .map_err(|_| anyhow::anyhow!("Compaction worker stopped unexpectedly"))?;
        }
        let result = self
            .state
            .result
            .lock()
            .map_err(|_| anyhow::anyhow!("Compaction result became unavailable"))?
            .take()
            .context("Compaction worker finished without a result")?;
        result.map_err(|error| anyhow::anyhow!(error))
    }
}

pub(super) fn start_compaction(
    agent: &Agent<Endpoint>,
    home: &std::path::Path,
    config: &Config,
) -> Result<CompactionTask> {
    let state = std::sync::Arc::new(CompactionState {
        status: std::sync::Mutex::new("Compacting context · enter a message to queue it".into()),
        result: std::sync::Mutex::new(None),
    });
    let worker_state = state.clone();
    let (cancel, cancelled) = tokio::sync::oneshot::channel();
    let home = home.to_path_buf();
    let config = config.clone();
    let profile = agent.profile.clone();
    let workspace = agent.workspace.clone();
    let session = agent.session.clone();
    let approval = agent.approval;
    let max_rounds = agent.max_rounds;
    let worker = std::thread::Builder::new()
        .name("builder-compaction".into())
        .spawn(move || {
            let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .context("Could not start the compaction runtime")?;
                let mut store = Store::open(&home)?;
                let memory = builder::memory::MemoryRuntime::from_config(&config)?;
                let provider = Endpoint::new(profile.clone())?;
                let compact_agent = Agent {
                    provider,
                    memory,
                    profile,
                    workspace,
                    session,
                    approval,
                    max_rounds,
                };
                let event_state = worker_state.clone();
                let session_for_interrupt = compact_agent.session.clone();
                let mut renderer = ui::Renderer::new(true);
                let result = runtime.block_on(async {
                    let mut emit = |event| {
                        if let Some(status) = compaction_event_status(&event)
                            && let Ok(mut current) = event_state.status.lock()
                        {
                            *current = status;
                        }
                        renderer.event(event);
                    };
                    let compact = compact_agent.compact(&mut store, &mut emit);
                    tokio::pin!(compact);
                    tokio::select! {
                        result = &mut compact => result,
                        _ = cancelled => Err(anyhow::anyhow!(
                            "Compaction interrupted; original context is intact"
                        )),
                    }
                });
                renderer.finish();
                if result.is_err() {
                    let _ = store.interrupt_attempts(&session_for_interrupt);
                }
                result
            }));
            let outcome = match outcome {
                Ok(result) => result,
                Err(_) => Err(anyhow::anyhow!(
                    "Compaction worker panicked; original context is intact"
                )),
            };
            let status = match &outcome {
                Ok(true) => {
                    "Context compacted · originals searchable · enter a message to queue it"
                        .to_owned()
                }
                Ok(false) => "No older context to compact · enter a message to queue it".to_owned(),
                Err(_) => {
                    "Compaction failed · original context intact · enter a message to queue it"
                        .to_owned()
                }
            };
            if let Ok(mut current) = worker_state.status.lock() {
                *current = status;
            }
            if let Ok(mut result) = worker_state.result.lock() {
                *result = Some(outcome.map_err(|error| error.to_string()));
            }
        })?;
    Ok(CompactionTask {
        state,
        cancel: Some(cancel),
        worker: Some(worker),
    })
}

fn compaction_event_status(event: &AgentEvent) -> Option<String> {
    match event {
        AgentEvent::Compacting { .. } => {
            Some("Compacting context · enter a message to queue it".into())
        }
        AgentEvent::CompactionProgress { fraction, .. } => Some(format!(
            "Compacting context · {:.0}% · enter a message to queue it",
            (fraction.clamp(0.0, 1.0) * 100.0).round()
        )),
        AgentEvent::SummaryRecovery { .. } => {
            Some("Tightening context summary · enter a message to queue it".into())
        }
        AgentEvent::OutputRecovery { .. } => {
            Some("Retrying context summary · enter a message to queue it".into())
        }
        AgentEvent::MemoryNotice(note) => {
            Some(format!("Compacting context · {}", one_line(note, 120)))
        }
        AgentEvent::Compacted { .. } => {
            Some("Context compacted · originals searchable · enter a message to queue it".into())
        }
        _ => None,
    }
}

fn one_line(text: &str, limit: usize) -> String {
    let mut text = text.replace(['\r', '\n'], " ");
    if text.len() > limit {
        text.truncate(text.floor_char_boundary(limit));
        text.push('…');
    }
    text
}

pub(super) async fn wait_for_compaction(task: &mut Option<CompactionTask>) -> Result<bool> {
    while task.as_ref().is_some_and(|task| !task.finished()) {
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    task.take()
        .context("Compaction task disappeared before it finished")?
        .finish()
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{Json, Router, routing::post};
    use builder::agent::{ApprovalMode, SummaryStage};
    use builder_core::{
        config::Profile,
        protocol::{Message, Role},
    };
    use builder_tools::Workspace;
    use serde_json::{Value, json};
    use std::time::Duration;

    async fn summary_server(delay: Duration) -> (String, tokio::task::JoinHandle<()>) {
        let app = Router::new().route(
            "/v1/chat/completions",
            post(move |Json(_body): Json<Value>| async move {
                tokio::time::sleep(delay).await;
                Json(json!({"choices":[{"index":0,"message":{"role":"assistant",
                    "content":"Handoff: the user asked for a parser refactor; a.rs and b.rs were read."},
                    "finish_reason":"stop"}]}))
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/v1", listener.local_addr().unwrap());
        let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (url, task)
    }

    /// A session with `findings` large assistant turns; a follow-up user
    /// message makes the earlier turns "older" context that can be summarized.
    fn agent(home: &std::path::Path, url: &str, findings: usize) -> Agent<Endpoint> {
        let workspace = Workspace::new(home).unwrap();
        let mut store = Store::open(home).unwrap();
        let session = store.create("compaction", "local", home, "system").unwrap();
        store
            .append(&session, &Message::text(Role::User, "Refactor the parser"))
            .unwrap();
        for index in 0..findings {
            store
                .append(
                    &session,
                    &Message::text(
                        Role::Assistant,
                        format!("Finding {index}: {}", "detail ".repeat(400)),
                    ),
                )
                .unwrap();
        }
        if findings > 0 {
            store
                .append(&session, &Message::text(Role::User, "continue"))
                .unwrap();
        }
        let profile = Profile {
            base_url: url.into(),
            model: "test-model".into(),
            stream: false,
            ..Profile::default()
        };
        Agent {
            provider: Endpoint::new(profile.clone()).unwrap(),
            memory: None,
            profile,
            workspace,
            session,
            approval: ApprovalMode::Trust,
            max_rounds: None,
        }
    }

    async fn settled(task: &CompactionTask) {
        while !task.finished() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    #[tokio::test]
    async fn compaction_runs_on_a_worker_thread_and_reports_its_status() {
        let (url, server) = summary_server(Duration::ZERO).await;
        let home = tempfile::tempdir().unwrap();
        let agent = agent(home.path(), &url, 6);
        let before = Store::open(home.path())
            .unwrap()
            .messages(&agent.session)
            .unwrap()
            .len();
        let task = start_compaction(&agent, home.path(), &Config::default()).unwrap();
        assert!(
            task.status().starts_with("Compacting context"),
            "{}",
            task.status()
        );
        settled(&task).await;
        assert_eq!(
            task.status(),
            "Context compacted · originals searchable · enter a message to queue it"
        );
        let mut slot = Some(task);
        assert!(wait_for_compaction(&mut slot).await.unwrap());
        assert!(slot.is_none());
        let messages = Store::open(home.path())
            .unwrap()
            .messages(&agent.session)
            .unwrap();
        assert!(messages.len() < before, "{} -> {}", before, messages.len());
        assert!(
            messages
                .iter()
                .any(|m| m.content.as_deref().is_some_and(|t| t.contains("Handoff:"))),
            "the model's handoff is in the active context"
        );
        server.abort();
    }

    #[tokio::test]
    async fn a_short_conversation_has_nothing_to_compact() {
        let (url, server) = summary_server(Duration::ZERO).await;
        let home = tempfile::tempdir().unwrap();
        let agent = agent(home.path(), &url, 0);
        let task = start_compaction(&agent, home.path(), &Config::default()).unwrap();
        settled(&task).await;
        assert_eq!(
            task.status(),
            "No older context to compact · enter a message to queue it"
        );
        let mut slot = Some(task);
        assert!(!wait_for_compaction(&mut slot).await.unwrap());
        server.abort();
    }

    #[tokio::test]
    async fn cancelling_leaves_the_original_context_intact() {
        let (url, server) = summary_server(Duration::from_secs(30)).await;
        let home = tempfile::tempdir().unwrap();
        let agent = agent(home.path(), &url, 6);
        let before = Store::open(home.path())
            .unwrap()
            .messages(&agent.session)
            .unwrap();
        let mut task = start_compaction(&agent, home.path(), &Config::default()).unwrap();
        task.cancel();
        task.cancel();
        settled(&task).await;
        assert_eq!(
            task.status(),
            "Compaction failed · original context intact · enter a message to queue it"
        );
        let error = task.finish().unwrap_err().to_string();
        assert!(error.contains("Compaction interrupted"), "{error}");
        let after = Store::open(home.path())
            .unwrap()
            .messages(&agent.session)
            .unwrap();
        assert_eq!(
            serde_json::to_value(&after).unwrap(),
            serde_json::to_value(&before).unwrap()
        );
        let mut empty = None;
        assert!(wait_for_compaction(&mut empty).await.is_err());
        server.abort();
    }

    #[test]
    fn status_lines_follow_compaction_events() {
        let status = |event| compaction_event_status(&event);
        assert_eq!(
            status(AgentEvent::Compacting {
                before: 1,
                context_tokens: 2
            })
            .as_deref(),
            Some("Compacting context · enter a message to queue it")
        );
        assert_eq!(
            status(AgentEvent::CompactionProgress {
                fraction: 0.456,
                fragment: 1,
                stage: SummaryStage::Waiting
            })
            .as_deref(),
            Some("Compacting context · 46% · enter a message to queue it")
        );
        assert_eq!(
            status(AgentEvent::CompactionProgress {
                fraction: 2.0,
                fragment: 1,
                stage: SummaryStage::Waiting
            })
            .as_deref(),
            Some("Compacting context · 100% · enter a message to queue it")
        );
        assert_eq!(
            status(AgentEvent::SummaryRecovery { size: 2, limit: 1 }).as_deref(),
            Some("Tightening context summary · enter a message to queue it")
        );
        assert_eq!(
            status(AgentEvent::OutputRecovery { budget: 1 }).as_deref(),
            Some("Retrying context summary · enter a message to queue it")
        );
        assert_eq!(
            status(AgentEvent::MemoryNotice("line one\nline two".into())).as_deref(),
            Some("Compacting context · line one line two")
        );
        let long = status(AgentEvent::MemoryNotice("x".repeat(200))).unwrap();
        assert!(long.ends_with('…'), "{long}");
        assert!(long.len() <= "Compacting context · ".len() + 120 + '…'.len_utf8());
        assert_eq!(
            status(AgentEvent::Compacted {
                before: 2,
                after: 1,
                context_tokens: 4
            })
            .as_deref(),
            Some("Context compacted · originals searchable · enter a message to queue it")
        );
        assert_eq!(
            status(AgentEvent::ToolStarted {
                name: "shell".into(),
                detail: "ls".into()
            }),
            None
        );
    }

    #[test]
    fn one_line_flattens_and_truncates_on_char_boundaries() {
        assert_eq!(one_line("a\r\nb", 10), "a  b");
        assert_eq!(one_line("short", 5), "short");
        assert_eq!(one_line(&"é".repeat(10), 7), format!("{}…", "é".repeat(3)));
    }
}
