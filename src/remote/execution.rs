use super::{
    ApprovalRequest, FinishRun, MAX_RETAINED_CHATS, MAX_RUNS, Phase, RemoteOptions, Run, Shared,
    Snapshot,
    auth::{failure, problem},
    catalog::{chat_folder, scoped_session},
};
use crate::{
    agent::{Agent, AgentEvent, ApprovalMode, SYSTEM},
    memory::MemoryRuntime,
};
use anyhow::{Context, Result, ensure};
use axum::{
    Json, Router,
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{get, post},
};
use builder_core::{config::Config, store::Store};
use builder_provider::{Activity, Event, OpenAiCompatible};
use builder_tools::{Action, Workspace};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    io::Read,
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    time::{Duration, Instant},
};
use tokio::sync::watch;
use uuid::Uuid;

pub(super) fn router() -> Router<Arc<Shared>> {
    Router::new()
        .route("/api/status", get(status))
        .route("/api/run", post(start))
        .route("/api/pause", post(pause))
        .route("/api/approval", post(approval))
}

async fn status(State(shared): State<Arc<Shared>>) -> Response {
    let registry = shared.registry.lock().unwrap();
    let states: Vec<Value> = registry
        .order
        .iter()
        .filter_map(|id| registry.runs.get(id))
        .map(|run| serde_json::to_value(&*run.snapshot.lock().unwrap()).unwrap())
        .collect();
    let latest = states
        .last()
        .cloned()
        .unwrap_or_else(|| json!(Snapshot::default()));
    Json(json!({"workspace":shared.options.workspace, "root":shared.options.workspace, "approval_mode":approval_name(shared.options.approval), "approval_locked":shared.options.approval==ApprovalMode::ReadOnly, "state":latest,"states":states,"max_concurrent_runs":MAX_RUNS})).into_response()
}
fn approval_name(mode: ApprovalMode) -> &'static str {
    match mode {
        ApprovalMode::Ask => "ask",
        ApprovalMode::ReadOnly => "read-only",
        ApprovalMode::Trust => "trust",
    }
}
/// The browser may pick the approval mode for each run. A host started with
/// `--approval read-only` is a hard ceiling: remote inspection stays read-only.
fn run_approval(options: &RemoteOptions, requested: Option<&str>) -> Result<ApprovalMode> {
    let mode = match requested {
        None => options.approval,
        Some("ask") => ApprovalMode::Ask,
        Some("read-only") => ApprovalMode::ReadOnly,
        Some("trust" | "auto") => ApprovalMode::Trust,
        Some(other) => {
            anyhow::bail!("Unknown approval mode {other:?}; use ask, read-only, or trust")
        }
    };
    ensure!(
        options.approval != ApprovalMode::ReadOnly || mode == ApprovalMode::ReadOnly,
        "This host was started read-only; changes and shell commands stay disabled"
    );
    Ok(mode)
}
#[derive(Deserialize)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
enum RunAction {
    Message { prompt: String },
    Retry,
    Compact,
}

fn operation_name(operation: &RunAction) -> &'static str {
    match operation {
        RunAction::Message { .. } => "message",
        RunAction::Retry => "retry",
        RunAction::Compact => "compact",
    }
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RunRequest {
    request_id: String,
    session: Option<String>,
    operation: RunAction,
    profile: Option<String>,
    /// Folder inside the host root for a new chat; ignored (but checked) for saved chats.
    workspace: Option<String>,
    /// `ask`, `read-only`, or `trust` for this run; defaults to the host's `--approval`.
    approval: Option<String>,
}

async fn start(State(shared): State<Arc<Shared>>, Json(request): Json<RunRequest>) -> Response {
    if Uuid::parse_str(&request.request_id).is_err() {
        return problem(StatusCode::BAD_REQUEST, "request_id must be a UUID");
    }
    if let RunAction::Message { prompt } = &request.operation
        && (prompt.trim().is_empty() || prompt.len() > 64 * 1024)
    {
        return problem(StatusCode::BAD_REQUEST, "Prompt must contain 1–65536 bytes");
    }
    // Serialize only admission/storage preparation. Model runs remain independent.
    let gate = match shared.starts.clone().acquire_owned().await {
        Ok(gate) => gate,
        Err(_) => return problem(StatusCode::SERVICE_UNAVAILABLE, "Host is stopping"),
    };
    if shared.closing.load(Ordering::Acquire) {
        return problem(StatusCode::SERVICE_UNAVAILABLE, "Host is stopping");
    }
    {
        let registry = shared.registry.lock().unwrap();
        if registry.requests.contains(&request.request_id) || registry.requests.len() >= 4096 {
            return problem(
                StatusCode::CONFLICT,
                "Request already accepted or request limit reached; inspect status",
            );
        }
    }
    if let Some(id) = &request.session {
        let current = shared.registry.lock().unwrap().runs.get(id).cloned();
        if let Some(run) = current {
            let maintaining = run.snapshot.lock().unwrap().phase == Phase::Maintaining;
            if maintaining && let Some(cancel) = &run.snapshot.lock().unwrap().cancel {
                let _ = cancel.send(true);
            }
            if !matches!(
                run.snapshot.lock().unwrap().phase,
                Phase::Running | Phase::AwaitingApproval
            ) {
                let _ = tokio::time::timeout(Duration::from_millis(500), async {
                    while !run.done.load(Ordering::Acquire) {
                        tokio::time::sleep(Duration::from_millis(10)).await;
                    }
                })
                .await;
            }
            if !run.done.load(Ordering::Acquire) {
                return problem(
                    StatusCode::CONFLICT,
                    "This chat is running; pause it before sending a follow-up",
                );
            }
        }
    }
    let permit = match shared.worker.clone().try_acquire_owned() {
        Ok(permit) => permit,
        Err(_) => {
            return problem(
                StatusCode::CONFLICT,
                "Four chats are running; pause one or wait for completion",
            );
        }
    };
    let worker_shared = shared.clone();
    let prepared = tokio::task::spawn_blocking(move || -> Result<_> {
        let options = &worker_shared.options;
        let mut store = Store::open(&options.home)?;
        let config = Config::load(&options.config_home)?;
        let existing = request
            .session
            .as_deref()
            .map(|id| scoped_session(&store, options, id))
            .transpose()?;
        if let Some(session) = &existing {
            ensure!(
                !store.chat_archived(&session.id)?,
                "Restore this archived chat before continuing"
            );
        }
        if let (Some(fixed), Some(selected)) = (&options.profile, &request.profile) {
            ensure!(fixed == selected, "The host fixes the endpoint profile");
        }
        let (name, profile) = config.profile(
            options
                .profile
                .as_deref()
                .or(request.profile.as_deref())
                .or_else(|| existing.as_ref().map(|s| s.profile.as_str())),
        )?;
        ensure!(profile.supports_chat(), "Select a chat profile");
        let approval = run_approval(options, request.approval.as_deref())?;
        // A saved chat is pinned to the folder it was created in; a new chat
        // resolves its folder from the request (the host root when omitted).
        let workspace = match &existing {
            Some(session) => {
                if request.workspace.is_some() {
                    ensure!(
                        chat_folder(options, request.workspace.as_deref())? == session.workspace,
                        "A saved chat keeps its folder"
                    );
                }
                session.workspace.clone()
            }
            None => chat_folder(options, request.workspace.as_deref())?,
        };
        let session = if let Some(session) = existing {
            session.id
        } else {
            let RunAction::Message { prompt } = &request.operation else {
                anyhow::bail!("Retry requires a saved session");
            };
            let mut system = format!("{SYSTEM}\n\nWorkspace: {}", workspace.display());
            let instructions = workspace.join("AGENTS.md");
            match std::fs::File::open(instructions) {
                Ok(file) => {
                    let mut content = String::new();
                    file.take(65537).read_to_string(&mut content)?;
                    ensure!(content.len() <= 65536, "Workspace AGENTS.md exceeds 64 KiB");
                    system.push_str(&format!(
                        "\n\nWorkspace instructions (AGENTS.md):\n{content}"
                    ));
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(e.into()),
            }
            store.create(prompt, &name, &workspace, &system)?
        };
        let guard = store.lock(&session)?;
        Ok((
            store, guard, config, profile, session, request, workspace, approval, permit, gate,
        ))
    })
    .await;
    let (store, guard, config, profile, session, request, workspace, approval, permit, gate) =
        match prepared {
            Ok(Ok(prepared)) => prepared,
            Ok(Err(error)) => return failure(error),
            Err(_) => {
                return problem(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "Could not prepare session",
                );
            }
        };
    let run_id = request.request_id.clone();
    let operation = operation_name(&request.operation);
    let (cancel, receiver) = watch::channel(false);
    if shared.closing.load(Ordering::Acquire) {
        return problem(StatusCode::SERVICE_UNAVAILABLE, "Host is stopping");
    }
    let run = Arc::new(Run {
        snapshot: Mutex::new(Snapshot {
            phase: Phase::Running,
            approval_mode: Some(approval_name(approval)),
            operation: Some(operation),
            compacting: false,
            run_id: Some(run_id.clone()),
            session: Some(session.clone()),
            cancel: Some(cancel),
            ..Default::default()
        }),
        done: AtomicBool::new(false),
    });
    {
        let mut registry = shared.registry.lock().unwrap();
        registry.requests.insert(run_id.clone());
        registry.order.retain(|id| id != &session);
        if registry.runs.len() >= MAX_RETAINED_CHATS
            && !registry.runs.contains_key(&session)
            && let Some(index) = registry
                .order
                .iter()
                .position(|id| registry.runs[id].done.load(Ordering::Acquire))
        {
            let old = registry.order.remove(index).unwrap();
            registry.runs.remove(&old);
        }
        registry.order.push_back(session.clone());
        registry.runs.insert(session.clone(), run.clone());
    }
    drop(gate);
    let worker_shared = shared.clone();
    let worker_run = run.clone();
    let (started, ready) = tokio::sync::oneshot::channel();
    let spawn = std::thread::Builder::new()
        .name("builder-remote".into())
        .spawn(move || {
            let _finish = FinishRun(worker_run.clone());
            let _permit = permit;
            let _guard = guard;
            let result =
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| -> Result<()> {
                    let runtime = tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build()?;
                    let result = runtime.block_on(drive(
                        worker_shared.clone(),
                        worker_run.clone(),
                        RunInput {
                            store,
                            config,
                            profile,
                            request,
                            workspace,
                            approval,
                        },
                        receiver,
                        started,
                    ));
                    // Local inference may finish on a blocking worker after its
                    // caller is cancelled. It must not retain the session lock
                    // or delay the next foreground run while the runtime drops.
                    runtime.shutdown_background();
                    result
                }));
            if let Err(error) = result.unwrap_or_else(|_| {
                Err(anyhow::anyhow!(
                    "Host worker stopped unexpectedly; inspect the saved session before retrying"
                ))
            }) {
                let mut state = worker_run.snapshot.lock().unwrap();
                state.phase = Phase::Failed;
                state.error = Some(format!("{error:#}"));
                state.approval = None;
                state.reply = None;
            }
        });
    if let Err(error) = spawn {
        run.done.store(true, Ordering::Release);
        let mut state = run.snapshot.lock().unwrap();
        state.phase = Phase::Failed;
        state.error = Some("Could not start host worker".into());
        return failure(error.into());
    }
    match ready.await {
        Ok(Ok(())) => {}
        Ok(Err(error)) => {
            return (
                StatusCode::CONFLICT,
                Json(json!({"error":error,"run_id":run_id,"session":session})),
            )
                .into_response();
        }
        Err(_) => {
            return problem(
                StatusCode::INTERNAL_SERVER_ERROR,
                "Host failed to start; inspect status before resubmitting",
            );
        }
    }
    (
        StatusCode::ACCEPTED,
        Json(json!({"run_id":run_id,"session":session})),
    )
        .into_response()
}

async fn cancelled(receiver: &mut watch::Receiver<bool>) {
    loop {
        if *receiver.borrow() {
            return;
        }
        if receiver.changed().await.is_err() {
            return;
        }
    }
}

struct RunInput {
    store: Store,
    config: Config,
    profile: builder_core::config::Profile,
    request: RunRequest,
    workspace: PathBuf,
    approval: ApprovalMode,
}

async fn drive(
    shared: Arc<Shared>,
    run: Arc<Run>,
    input: RunInput,
    mut cancel: watch::Receiver<bool>,
    started: tokio::sync::oneshot::Sender<std::result::Result<(), String>>,
) -> Result<()> {
    let RunInput {
        mut store,
        config,
        profile,
        request,
        workspace,
        approval,
    } = input;
    let session = run.snapshot.lock().unwrap().session.clone().unwrap();
    if profile.tools && profile.pipeline.todos {
        run.snapshot.lock().unwrap().todos = store.todos(&session)?;
    }
    let agent = Agent {
        memory: MemoryRuntime::from_config(&config)?,
        provider: OpenAiCompatible::new(profile.clone())?,
        max_rounds: shared
            .options
            .max_rounds
            .unwrap_or(profile.pipeline.max_rounds),
        profile,
        workspace: Workspace::new(&workspace)?,
        session: session.clone(),
        approval,
    };
    if let RunAction::Message { prompt } = &request.operation
        && let Err(error) = agent.submit(&mut store, prompt)
    {
        let _ = started.send(Err(format!("{error:#}")));
        return Err(error);
    }
    // The caller receives 202 only after the submitted user message is durable.
    let _ = started.send(Ok(()));
    let mut emit = |event| event_update(&run, event);
    let approval_cancel = cancel.clone();
    let mut approve = |action: &Action| ask(&run, action, &approval_cancel);
    let result = tokio::select! {
        biased;
        _ = cancelled(&mut cancel) => None,
        result = tokio::time::timeout(Duration::from_secs(3600), async {
            if matches!(request.operation, RunAction::Compact) { agent.compact(&mut store, &mut emit).await.map(|_| ()) }
            else { agent.run(&mut store, &mut emit, &mut approve).await }
        }) => Some(result.context("Run exceeded its one-hour deadline; inspect before retrying").and_then(|r| r)),
    };
    store.interrupt_attempts(&session)?;
    let task_outcome =
        if matches!(result, Some(Ok(()))) && !matches!(request.operation, RunAction::Compact) {
            Some(crate::completion::assess(
                &store,
                &session,
                &agent.workspace,
                &agent.profile.pipeline,
            )?)
        } else {
            None
        };
    {
        let mut state = run.snapshot.lock().unwrap();
        state.task_outcome = task_outcome;
        state.approval = None;
        state.reply = None;
        state.compacting = false;
        // A preview is never represented as a committed answer.
        state.preview.clear();
        state.preview_limited = false;
        state.thinking.clear();
        match &result {
            None => state.phase = Phase::Paused,
            Some(Ok(())) if task_outcome.is_some_and(|outcome| outcome.requires_attention()) => {
                state.phase = Phase::Failed;
                state.error = Some(format!(
                    "Task outcome: {:?}. Inspect the saved evidence before retrying.",
                    task_outcome.unwrap()
                ));
            }
            Some(Ok(())) => state.phase = Phase::Complete,
            Some(Err(error)) => {
                state.phase = Phase::Failed;
                state.error = Some(format!("{error:#}"));
            }
        }
    }
    if matches!(result, Some(Ok(())))
        && !task_outcome.is_some_and(|outcome| outcome.requires_attention())
        && let Some(memory) = &agent.memory
    {
        let extraction_provider = MemoryRuntime::extraction_provider(&agent.profile)?;
        run.snapshot.lock().unwrap().phase = Phase::Maintaining;
        tokio::select! {
            biased;
            _ = cancelled(&mut cancel) => {},
            _ = tokio::time::timeout(Duration::from_secs(105), memory.maintain(&extraction_provider, &mut store, &session, &agent.workspace, agent.profile.context_tokens)) => {},
        }
        run.snapshot.lock().unwrap().phase = Phase::Complete;
    }
    Ok(())
}

fn event_update(run: &Run, event: AgentEvent) {
    let mut state = run.snapshot.lock().unwrap();
    match event {
        AgentEvent::Model(Event::Delta(text)) => {
            if state.preview.len() + text.len() <= 64 * 1024 {
                state.preview.push_str(&text);
            } else {
                state.preview_limited = true;
            }
        }
        AgentEvent::Model(Event::Reasoning(text)) => {
            if state.thinking.len() + text.len() <= 64 * 1024 {
                state.thinking.push_str(&text);
            }
        }
        AgentEvent::CompactionProgress { fraction, .. } => {
            // One live line, replaced in place rather than flooding the log.
            const PREFIX: &str = "Summarizing older context · ";
            if state
                .notices
                .back()
                .is_some_and(|notice| notice.starts_with(PREFIX))
            {
                state.notices.pop_back();
            } else if state.notices.len() == 32 {
                state.notices.pop_front();
            }
            state
                .notices
                .push_back(format!("{PREFIX}{:.0}%", fraction * 100.0));
        }
        AgentEvent::Model(Event::Prompt { .. } | Event::Attempt { .. } | Event::Retry { .. }) => {
            state.preview.clear();
            state.preview_limited = false;
            state.thinking.clear();
        }
        event => {
            if matches!(&event, AgentEvent::Compacting { .. }) {
                state.compacting = true;
            } else if matches!(&event, AgentEvent::Compacted { .. }) {
                state.compacting = false;
            }
            let notice = match event {
                AgentEvent::ToolStarted { name, .. } => {
                    state.preview.clear();
                    state.thinking.clear();
                    format!("Running {name}")
                }
                AgentEvent::ToolFinished { name, failed, .. } => format!(
                    "{name}: {}",
                    if failed {
                        "failed or denied"
                    } else {
                        "finished"
                    }
                ),
                AgentEvent::MemoryNotice(text) => text,
                AgentEvent::Model(Event::Activity(activity)) => match activity {
                    Activity::Connected => "Model connected".into(),
                    Activity::Thinking => "Model is thinking".into(),
                    Activity::PreparingTools => "Model is preparing tools".into(),
                },
                AgentEvent::AutoCompact { .. } => "Context is nearing its configured limit".into(),
                AgentEvent::Compacting { .. } => {
                    "Summarizing older context; originals remain saved".into()
                }
                AgentEvent::Compacted { .. } => "Context summary saved".into(),
                AgentEvent::OutputRecovery { .. } => {
                    "Retrying an incomplete response with more output room".into()
                }
                AgentEvent::SummaryRecovery { .. } => "Shortening the context summary".into(),
                AgentEvent::ExplorationRecovery { .. } => {
                    "Recovering from repeated tool failures".into()
                }
                AgentEvent::ProgressNudge {
                    calls,
                    step,
                    repeated_reads,
                    planning,
                } => crate::presentation::nudge_line(calls, step, repeated_reads, planning),
                AgentEvent::RepetitionNotice { name, .. } => {
                    format!("Repeated {name} call; checking progress")
                }
                AgentEvent::SubagentProgress {
                    description,
                    actions,
                    activity,
                    ..
                } => format!("Subagent {description} · {actions} actions · {activity}"),
                AgentEvent::TodosUpdated(list) => {
                    state.todos = Some(list);
                    return;
                }
                AgentEvent::Model(_) | AgentEvent::CompactionProgress { .. } => return,
            };
            let notice = if notice.len() > 2048 {
                "Activity details exceed the live display limit; inspect saved history".into()
            } else {
                notice
            };
            if state.notices.len() == 32 {
                state.notices.pop_front();
            }
            state.notices.push_back(notice);
        }
    }
}

fn ask(run: &Run, action: &Action, cancel: &watch::Receiver<bool>) -> bool {
    let description = action.description();
    // Never ask a user to authorize a clipped action.
    if description.len() > 128 * 1024 {
        return false;
    }
    let id = Uuid::new_v4().to_string();
    let (send, receive) = mpsc::sync_channel(1);
    {
        let mut state = run.snapshot.lock().unwrap();
        state.phase = Phase::AwaitingApproval;
        state.approval = Some(ApprovalRequest { id, description });
        state.reply = Some(send);
    }
    let deadline = Instant::now() + Duration::from_secs(120);
    let allowed = loop {
        if *cancel.borrow() || Instant::now() >= deadline {
            break false;
        }
        match receive.recv_timeout(Duration::from_millis(100)) {
            Ok(allowed) => break allowed && !*cancel.borrow(),
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => break false,
        }
    };
    let mut state = run.snapshot.lock().unwrap();
    state.approval = None;
    state.reply = None;
    state.phase = Phase::Running;
    allowed
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RunIdentity {
    run_id: String,
}
async fn pause(State(shared): State<Arc<Shared>>, Json(request): Json<RunIdentity>) -> Response {
    let Some(run) = find_run(&shared, &request.run_id) else {
        return problem(StatusCode::CONFLICT, "This run is no longer current");
    };
    let state = run.snapshot.lock().unwrap();
    if state.run_id.as_deref() != Some(&request.run_id) {
        return problem(StatusCode::CONFLICT, "This run is no longer current");
    }
    if let Some(cancel) = &state.cancel {
        let _ = cancel.send(true);
    }
    Json(json!({"pausing":true})).into_response()
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ApprovalReply {
    run_id: String,
    approval_id: String,
    allow: bool,
}
async fn approval(
    State(shared): State<Arc<Shared>>,
    Json(request): Json<ApprovalReply>,
) -> Response {
    let Some(run) = find_run(&shared, &request.run_id) else {
        return problem(StatusCode::CONFLICT, "This run is no longer current");
    };
    let mut state = run.snapshot.lock().unwrap();
    if state.run_id.as_deref() != Some(&request.run_id)
        || state.approval.as_ref().map(|a| a.id.as_str()) != Some(&request.approval_id)
        || state.cancel.as_ref().is_some_and(|cancel| *cancel.borrow())
    {
        return problem(
            StatusCode::CONFLICT,
            "Approval is stale or the run is paused",
        );
    }
    let Some(reply) = state.reply.take() else {
        return problem(StatusCode::CONFLICT, "Approval already answered");
    };
    match reply.try_send(request.allow) {
        Ok(()) => Json(json!({"accepted":true})).into_response(),
        Err(_) => problem(StatusCode::CONFLICT, "Approval expired"),
    }
}

fn find_run(shared: &Shared, id: &str) -> Option<Arc<Run>> {
    shared
        .registry
        .lock()
        .unwrap()
        .runs
        .values()
        .find(|run| run.snapshot.lock().unwrap().run_id.as_deref() == Some(id))
        .cloned()
}
