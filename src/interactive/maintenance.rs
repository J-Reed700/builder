use anyhow::Result;
use builder::agent::Agent;
use builder_core::{
    config::{Config, Profile},
    store::Store,
};
use builder_provider::OpenAiCompatible;
use builder_tools::Workspace;

pub(super) struct MemoryTask {
    cancel: Option<tokio::sync::oneshot::Sender<()>>,
    stopped: tokio::sync::oneshot::Receiver<()>,
    index_updates: std::sync::Arc<std::sync::Mutex<IndexUpdateState>>,
    memory_status: std::sync::Arc<std::sync::Mutex<String>>,
}

#[derive(Debug, Clone)]
enum IndexUpdateState {
    Starting,
    Watching,
    PeriodicOnly,
    WatchUnavailable(String),
}

impl MemoryTask {
    pub(super) fn memory_status(&self) -> Option<String> {
        self.memory_status.lock().ok().map(|status| status.clone())
    }

    pub(super) fn index_update_description(&self) -> String {
        let state = self
            .index_updates
            .lock()
            .map(|state| state.clone())
            .unwrap_or_else(|_| {
                IndexUpdateState::WatchUnavailable("worker state unavailable".into())
            });
        match state {
            IndexUpdateState::Starting => "background index worker starting".into(),
            IndexUpdateState::Watching => {
                "live filesystem watch + periodic full-scan fallback".into()
            }
            IndexUpdateState::PeriodicOnly => "periodic full scans (watch disabled)".into(),
            IndexUpdateState::WatchUnavailable(error) => {
                format!("periodic full-scan fallback (watch unavailable: {error})")
            }
        }
    }
}

pub(super) fn configured_index_update_mode(profile: &Profile) -> String {
    if !profile.pipeline.code_index_background {
        return "foreground search refresh only".into();
    }
    if profile.pipeline.code_index_watch {
        format!(
            "watch configured at {}ms + full scan {}s; worker idle or paused",
            profile.pipeline.code_index_debounce_ms, profile.pipeline.code_index_refresh_secs
        )
    } else {
        format!(
            "full scan configured every {}s; worker idle or paused",
            profile.pipeline.code_index_refresh_secs
        )
    }
}

fn start_code_watch(
    root: &std::path::Path,
    state: &std::sync::Arc<std::sync::Mutex<IndexUpdateState>>,
) -> Option<builder::code_index::CodeIndexWatch> {
    match builder::code_index::CodeIndexWatch::new(root) {
        Ok(watch) => {
            if let Ok(mut state) = state.lock() {
                *state = IndexUpdateState::Watching;
            }
            Some(watch)
        }
        Err(error) => {
            let mut text = error.to_string().replace(['\r', '\n'], " ");
            text.truncate(text.floor_char_boundary(160));
            if let Ok(mut state) = state.lock() {
                *state = IndexUpdateState::WatchUnavailable(text);
            }
            None
        }
    }
}

pub(super) async fn stop_memory_task(task: &mut Option<MemoryTask>) {
    if let Some(mut running) = task.take() {
        if let Some(cancel) = running.cancel.take() {
            let _ = cancel.send(());
        }
        // Cancellation is normally immediate because network and embedding
        // work is awaited. If synchronous capture or SQLite work is still in
        // flight, retain ownership so a replacement worker cannot overlap it.
        if tokio::time::timeout(std::time::Duration::from_millis(100), &mut running.stopped)
            .await
            .is_err()
        {
            *task = Some(running);
        }
    }
}

pub(super) fn reap_memory_task(task: &mut Option<MemoryTask>) {
    let finished = task.as_mut().is_some_and(|running| {
        !matches!(
            running.stopped.try_recv(),
            Err(tokio::sync::oneshot::error::TryRecvError::Empty)
        )
    });
    if finished {
        task.take();
    }
}

pub(super) fn start_memory_task(
    agent: &Agent<OpenAiCompatible>,
    home: &std::path::Path,
    config: &Config,
) -> Option<MemoryTask> {
    if agent.memory.is_none()
        && !(agent.profile.pipeline.code_index && agent.profile.pipeline.code_index_background)
    {
        return None;
    }
    let home = home.to_path_buf();
    let config = config.clone();
    let profile = agent.profile.clone();
    let workspace = agent.workspace.root().to_path_buf();
    let session = agent.session.clone();
    let (cancel, cancelled) = tokio::sync::oneshot::channel();
    let (stopped, stopped_rx) = tokio::sync::oneshot::channel();
    let index_updates = std::sync::Arc::new(std::sync::Mutex::new(
        if profile.pipeline.code_index && profile.pipeline.code_index_background {
            IndexUpdateState::Starting
        } else {
            IndexUpdateState::PeriodicOnly
        },
    ));
    let worker_index_updates = index_updates.clone();
    let memory_status = std::sync::Arc::new(std::sync::Mutex::new(
        "waiting for idle maintenance".to_string(),
    ));
    let worker_memory_status = memory_status.clone();
    std::thread::Builder::new()
        .name("builder-memory".into())
        .spawn(move || {
            let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            else {
                let _ = stopped.send(());
                return;
            };
            runtime.block_on(async move {
                let setup = (|| -> Result<_> {
                    Ok((
                        builder::memory::MemoryRuntime::from_config(&config)?,
                        builder::memory::MemoryRuntime::extraction_provider(&profile)?,
                        Workspace::new(&workspace)?,
                        Store::open(&home)?,
                    ))
                })();
                let (memory, provider, workspace, mut store) = match setup {
                    Ok(setup) => setup,
                    Err(error) => {
                        if let Ok(mut status) = worker_memory_status.lock() {
                            *status = format!("maintenance could not start: {error}");
                        }
                        return;
                    }
                };
                let maintenance = async {
                    let mut code_watch = if profile.pipeline.code_index
                        && profile.pipeline.code_index_background
                        && profile.pipeline.code_index_watch
                    {
                        start_code_watch(workspace.root(), &worker_index_updates)
                    } else {
                        if let Ok(mut state) = worker_index_updates.lock() {
                            *state = IndexUpdateState::PeriodicOnly;
                        }
                        None
                    };
                    // Separate connections let extraction retry
                    // while a large code-vector queue is still being drained.
                    let memory_work = async {
                        let Some(memory) = &memory else {
                            if let Ok(mut status) = worker_memory_status.lock() {
                                *status = "disabled".into();
                            }
                            return;
                        };
                        loop {
                            let result = memory
                                .maintain(
                                    &provider,
                                    &mut store,
                                    &session,
                                    &workspace,
                                    profile.context_tokens,
                                )
                                .await;
                            if let Ok(mut status) = worker_memory_status.lock() {
                                *status = match result {
                                    Ok(()) => "idle pass complete; no extraction error".into(),
                                    Err(error) => {
                                        format!("maintenance failed; will retry: {error}")
                                    }
                                };
                            }
                            tokio::time::sleep(std::time::Duration::from_secs(30)).await;
                        }
                    };
                    let code_work = async {
                        if !profile.pipeline.code_index || !profile.pipeline.code_index_background {
                            return;
                        }
                        let Ok(mut code_store) = Store::open(&home) else {
                            return;
                        };
                        loop {
                            let _ = builder::code_index::maintain(
                                &mut code_store,
                                &workspace,
                                memory.as_ref(),
                                &profile.pipeline,
                                code_watch.as_mut(),
                            )
                            .await;
                            if let Some(watch) = &mut code_watch {
                                let _ = watch
                                    .wait(
                                        profile.pipeline.code_index_refresh_secs,
                                        profile.pipeline.code_index_debounce_ms,
                                    )
                                    .await;
                            } else {
                                tokio::time::sleep(std::time::Duration::from_secs(
                                    profile.pipeline.code_index_refresh_secs,
                                ))
                                .await;
                            }
                        }
                    };
                    tokio::join!(biased; memory_work, code_work);
                };
                tokio::select! { _ = cancelled => {}, _ = maintenance => {} }
            });
            runtime.shutdown_background();
            let _ = stopped.send(());
        })
        .ok()?;
    Some(MemoryTask {
        cancel: Some(cancel),
        stopped: stopped_rx,
        index_updates,
        memory_status,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use builder::agent::{ApprovalMode, SYSTEM};
    use builder_core::protocol::{Message, Role};

    #[test]
    fn unavailable_filesystem_watch_is_reported_as_periodic_fallback() {
        let root = tempfile::tempdir().unwrap();
        let missing = root.path().join("removed");
        let state = std::sync::Arc::new(std::sync::Mutex::new(IndexUpdateState::Starting));
        assert!(start_code_watch(&missing, &state).is_none());
        let state = state.lock().unwrap().clone();
        assert!(matches!(state, IndexUpdateState::WatchUnavailable(_)));

        let (_cancel, stopped) = tokio::sync::oneshot::channel();
        let task = MemoryTask {
            memory_status: Default::default(),
            cancel: None,
            stopped,
            index_updates: std::sync::Arc::new(std::sync::Mutex::new(state)),
        };
        let description = task.index_update_description();
        assert!(description.contains("periodic full-scan fallback"));
        assert!(description.contains("watch unavailable"));
    }

    #[tokio::test]
    async fn idle_worker_extracts_while_code_embeddings_are_stalled() {
        use axum::{Json, Router, routing::post};
        use builder_core::{
            config::{EmbeddingBackend, ModelRole},
            protocol::{Function, ToolCall},
        };
        use serde_json::json;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let profile = Profile {
            base_url: format!("http://{}/v1", listener.local_addr().unwrap()),
            stream: false,
            extra_body: std::collections::BTreeMap::from([(
                "chat_template_kwargs".into(),
                json!({"enable_thinking":true}),
            )]),
            roles: vec![ModelRole::Chat, ModelRole::Embed],
            ..Default::default()
        };
        let app = Router::new()
            .route("/v1/chat/completions", post(|Json(request): Json<serde_json::Value>| async move {
                assert_eq!(request["chat_template_kwargs"]["enable_thinking"], false);
                Json(json!({"choices":[{"message":{"role":"assistant","content":json!({
                    "findings":[{"key":"shield", "text":"Shield rate is 0.025", "evidence_call_ids":["read"]}],
                    "next_action":"no remaining action observed", "questions":[]
                }).to_string()},"finish_reason":"stop"}]}))
            }))
            .route("/v1/embeddings", post(|| async {
                std::future::pending::<Json<serde_json::Value>>().await
            }));
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let home = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        std::fs::write(
            root.path().join("shield.rs"),
            "pub fn shield() -> f32 { 0.025 }\n",
        )
        .unwrap();
        let workspace = Workspace::new(root.path()).unwrap();
        let mut store = Store::open(home.path()).unwrap();
        let session = store
            .create("idle worker", "test", root.path(), SYSTEM)
            .unwrap();
        let mut call = Message::text(Role::Assistant, "");
        call.tool_calls.push(ToolCall {
            id: "read".into(),
            kind: "function".into(),
            function: Function {
                name: "read_file".into(),
                arguments: json!({"path":"shield.rs"}).to_string(),
            },
        });
        store.append(&session, &call).unwrap();
        store.claim_tool(&session, "read").unwrap();
        let result = workspace
            .execute(&builder_tools::Action::ReadFile {
                path: "shield.rs".into(),
                start_line: None,
                end_line: None,
            })
            .await
            .unwrap();
        store.complete_tool(&session, "read", &result).unwrap();
        store
            .append(&session, &Message::text(Role::Assistant, "Done."))
            .unwrap();
        let mut config = Config::default();
        config.memory.enabled = true;
        config.memory.embedding_backend = EmbeddingBackend::Remote;
        config.memory.embedding_profile = Some("test".into());
        config.profiles.insert("test".into(), profile.clone());
        let agent = Agent {
            provider: OpenAiCompatible::new(profile.clone()).unwrap(),
            profile,
            memory: builder::memory::MemoryRuntime::from_config(&config).unwrap(),
            workspace,
            session,
            approval: ApprovalMode::ReadOnly,
            max_rounds: 2,
        };
        let mut task = start_memory_task(&agent, home.path(), &config);
        assert_eq!(
            agent.profile.extra_body["chat_template_kwargs"]["enable_thinking"],
            true
        );
        let scope = builder::memory::MemoryRuntime::scope(&agent.workspace);
        let outcome = tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while store.memory_list(&scope).unwrap().is_empty() {
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await;
        stop_memory_task(&mut task).await;
        server.abort();
        assert!(
            outcome.is_ok(),
            "a blocked embedding queue must not starve extraction"
        );
        assert!(
            task.is_none(),
            "idle work must cancel before foreground work"
        );
    }

    #[tokio::test]
    async fn timed_out_maintenance_remains_owned_until_it_stops() {
        let (cancel, mut cancelled) = tokio::sync::oneshot::channel();
        let (stopped_tx, stopped) = tokio::sync::oneshot::channel();
        let mut task = Some(MemoryTask {
            cancel: Some(cancel),
            stopped,
            index_updates: std::sync::Arc::new(std::sync::Mutex::new(IndexUpdateState::Starting)),
            memory_status: Default::default(),
        });

        stop_memory_task(&mut task).await;
        assert!(task.is_some(), "a live worker must not be detached");
        assert_eq!(cancelled.try_recv(), Ok(()));

        stopped_tx.send(()).unwrap();
        reap_memory_task(&mut task);
        assert!(task.is_none());
    }
}
