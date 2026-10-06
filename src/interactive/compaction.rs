use anyhow::{Context, Result};
use builder::{
    agent::{Agent, AgentEvent},
    ui,
};
use builder_core::{config::Config, store::Store};
use builder_provider::OpenAiCompatible;

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
    agent: &Agent<OpenAiCompatible>,
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
                let provider = OpenAiCompatible::new(profile.clone())?;
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
