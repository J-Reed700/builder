//! Authenticated application adapter. The existing agent owns all durable execution semantics.
use crate::{
    agent::{Agent, AgentEvent, ApprovalMode, SYSTEM},
    memory::MemoryRuntime,
};
use anyhow::{Context, Result, ensure};
use axum::{
    Json, Router,
    extract::{DefaultBodyLimit, Path, Query, Request, State},
    http::{HeaderValue, StatusCode},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use builder_core::{config::Config, store::Store};
use builder_provider::{Activity, Event, OpenAiCompatible};
use builder_tools::{Action, Workspace};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::{HashMap, HashSet, VecDeque},
    io::{Read, Write},
    path::{Path as FilePath, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    time::{Duration, Instant},
};
use subtle::ConstantTimeEq;
use tokio::sync::{Semaphore, watch};
use uuid::Uuid;

pub struct RemoteOptions {
    pub home: PathBuf,
    pub config_home: PathBuf,
    pub workspace: PathBuf,
    pub profile: Option<String>,
    pub approval: ApprovalMode,
    pub max_rounds: Option<usize>,
    pub origin: String,
}

#[derive(Clone, Copy, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum Phase {
    #[default]
    Idle,
    Running,
    AwaitingApproval,
    Maintaining,
    Complete,
    Paused,
    Failed,
}

#[derive(Clone, Serialize)]
struct ApprovalRequest {
    id: String,
    description: String,
}

#[derive(Default, Serialize)]
struct Snapshot {
    phase: Phase,
    /// Approval mode this run was started with.
    approval_mode: Option<&'static str>,
    run_id: Option<String>,
    session: Option<String>,
    preview: String,
    preview_limited: bool,
    /// Reasoning streamed for the response in progress, shown beside the preview.
    thinking: String,
    notices: VecDeque<String>,
    /// The session's current todo list, refreshed whenever the agent records one.
    todos: Option<builder_core::todo::List>,
    approval: Option<ApprovalRequest>,
    error: Option<String>,
    #[serde(skip)]
    reply: Option<mpsc::SyncSender<bool>>,
    #[serde(skip)]
    cancel: Option<watch::Sender<bool>>,
}

const MAX_RUNS: u32 = 4;
const MAX_RETAINED_CHATS: usize = 64;

struct Run {
    snapshot: Mutex<Snapshot>,
    done: AtomicBool,
}
struct FinishRun(Arc<Run>);
impl Drop for FinishRun {
    fn drop(&mut self) {
        self.0.done.store(true, Ordering::Release);
    }
}
#[derive(Default)]
struct Registry {
    runs: HashMap<String, Arc<Run>>,
    order: VecDeque<String>,
    requests: HashSet<String>,
}

struct Shared {
    options: RemoteOptions,
    token: String,
    registry: Mutex<Registry>,
    starts: Arc<Semaphore>,
    closing: AtomicBool,
    worker: Arc<Semaphore>,
    readers: Arc<Semaphore>,
}

/// Dropping the owner cancels the host worker, including a pending approval.
pub struct RemoteControl {
    shared: Arc<Shared>,
}
impl RemoteControl {
    pub fn new(mut options: RemoteOptions) -> Result<Self> {
        options.workspace = Workspace::new(&options.workspace)?.root().to_owned();
        ensure!(
            options.origin.starts_with("http://") || options.origin.starts_with("https://"),
            "Origin must start with http:// or https://"
        );
        let authority = options.origin.split_once("://").unwrap().1;
        ensure!(
            !authority.is_empty()
                && !authority.contains(['/', '?', '#', '@'])
                && !authority.chars().any(char::is_whitespace),
            "Origin must be an exact browser origin without a path or trailing slash"
        );
        HeaderValue::from_str(&options.origin).context("Invalid origin")?;
        let config = Config::load(&options.config_home)?;
        config.profile(options.profile.as_deref())?;
        let token = load_token(&options.home)?;
        Ok(Self {
            shared: Arc::new(Shared {
                options,
                token,
                registry: Mutex::new(Registry::default()),
                starts: Arc::new(Semaphore::new(1)),
                closing: AtomicBool::new(false),
                worker: Arc::new(Semaphore::new(MAX_RUNS as usize)),
                readers: Arc::new(Semaphore::new(16)),
            }),
        })
    }
    pub fn workspace(&self) -> &FilePath {
        &self.shared.options.workspace
    }
    pub fn token_path(&self) -> PathBuf {
        self.shared.options.home.join("remote-token")
    }
    pub(crate) fn token(&self) -> &str {
        &self.shared.token
    }
    pub fn router(&self) -> Router {
        let api = Router::new()
            .route("/api/status", get(status))
            .route("/api/sessions", get(sessions))
            .route("/api/sessions/{id}/messages", get(history))
            .route("/api/sessions/{id}", get(chat_info).post(manage_chat))
            .route("/api/profiles", get(profiles))
            .route("/api/folders", get(folders))
            .route("/api/run", post(start))
            .route("/api/pause", post(pause))
            .route("/api/approval", post(approval))
            .layer(DefaultBodyLimit::max(128 * 1024))
            .route_layer(middleware::from_fn_with_state(
                self.shared.clone(),
                authenticate,
            ));
        Router::new()
            .merge(api)
            .route(
                "/",
                get(|| async {
                    (
                        [("content-type", "text/html; charset=utf-8")],
                        include_str!("../remote/web/index.html"),
                    )
                }),
            )
            .route(
                "/app.js",
                get(|| async {
                    (
                        [("content-type", "text/javascript; charset=utf-8")],
                        include_str!("../remote/web/app.js"),
                    )
                }),
            )
            .route(
                "/style.css",
                get(|| async {
                    (
                        [("content-type", "text/css; charset=utf-8")],
                        include_str!("../remote/web/style.css"),
                    )
                }),
            )
            .layer(middleware::from_fn(headers))
            .with_state(self.shared.clone())
    }
    pub async fn shutdown(&self) {
        self.cancel();
        // Wait for every worker's RAII cleanup, not just the last selected chat.
        let _ = tokio::time::timeout(
            Duration::from_secs(5),
            self.shared.worker.acquire_many(MAX_RUNS),
        )
        .await;
    }
    fn cancel(&self) {
        self.shared.closing.store(true, Ordering::Release);
        for run in self.shared.registry.lock().unwrap().runs.values() {
            if let Some(cancel) = &run.snapshot.lock().unwrap().cancel {
                let _ = cancel.send(true);
            }
        }
    }
}
impl Drop for RemoteControl {
    fn drop(&mut self) {
        self.cancel();
    }
}

fn load_token(home: &FilePath) -> Result<String> {
    drop(Store::open(home)?);
    let path = home.join("remote-token");
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    match options.open(&path) {
        Ok(mut file) => {
            let token = format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple());
            file.write_all(token.as_bytes())?;
            file.sync_all()?;
            Ok(token)
        }
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            let metadata = std::fs::symlink_metadata(&path)?;
            ensure!(
                metadata.file_type().is_file(),
                "remote-token must be a regular file"
            );
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                ensure!(
                    metadata.permissions().mode() & 0o077 == 0,
                    "remote-token must be private: chmod 600 the token file"
                );
            }
            let mut token = String::new();
            std::fs::File::open(path)?
                .take(129)
                .read_to_string(&mut token)?;
            ensure!(
                token.len() == 64 && token.bytes().all(|b| b.is_ascii_hexdigit()),
                "Invalid remote-token; stop remote control and remove the file to regenerate it"
            );
            Ok(token)
        }
        Err(error) => Err(error.into()),
    }
}

async fn headers(request: Request, next: Next) -> Response {
    let mut response = next.run(request).await;
    for (key, value) in [
        ("cache-control", "no-store"),
        ("x-content-type-options", "nosniff"),
        ("referrer-policy", "no-referrer"),
        ("x-frame-options", "DENY"),
        (
            "content-security-policy",
            "default-src 'none'; script-src 'self'; style-src 'self'; connect-src 'self'; img-src 'self'; base-uri 'none'; frame-ancestors 'none'; form-action 'self'",
        ),
    ] {
        response
            .headers_mut()
            .insert(key, HeaderValue::from_static(value));
    }
    response
}

async fn authenticate(State(shared): State<Arc<Shared>>, request: Request, next: Next) -> Response {
    let supplied = request
        .headers()
        .get("x-builder-token")
        .and_then(|h| h.to_str().ok())
        .unwrap_or("");
    if supplied.len() != shared.token.len()
        || !bool::from(supplied.as_bytes().ct_eq(shared.token.as_bytes()))
    {
        return problem(
            StatusCode::UNAUTHORIZED,
            "Enter the host's remote-control token",
        );
    }
    // No cookie auth or CORS. Browser writes must come from the configured exact origin.
    let origin = request
        .headers()
        .get("origin")
        .and_then(|h| h.to_str().ok());
    if origin.is_some_and(|origin| origin != shared.options.origin)
        || (request.method() != axum::http::Method::GET
            && origin != Some(shared.options.origin.as_str()))
    {
        return problem(
            StatusCode::FORBIDDEN,
            "Browser origin differs from builder remote --origin",
        );
    }
    let Ok(_permit) = shared.readers.clone().try_acquire_owned() else {
        return problem(StatusCode::TOO_MANY_REQUESTS, "Too many requests");
    };
    match tokio::time::timeout(Duration::from_secs(10), next.run(request)).await {
        Ok(response) => response,
        Err(_) => problem(
            StatusCode::REQUEST_TIMEOUT,
            "Request timed out; refresh status before taking another action",
        ),
    }
}

fn problem(code: StatusCode, message: &str) -> Response {
    (code, Json(json!({"error":message}))).into_response()
}
fn failure(error: anyhow::Error) -> Response {
    problem(StatusCode::CONFLICT, &format!("{error:#}"))
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
#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct SessionQuery {
    #[serde(default)]
    offset: u32,
    #[serde(default)]
    archived: bool,
    #[serde(default)]
    search: String,
}
async fn sessions(
    State(shared): State<Arc<Shared>>,
    Query(query): Query<SessionQuery>,
) -> Response {
    read(shared, move |store, options| {
        let sessions = store.chat_sessions_within(
            &options.workspace,
            query.offset,
            query.archived,
            &query.search,
        )?;
        let next = (sessions.len() == 50).then_some(query.offset.saturating_add(50));
        let sessions: Vec<Value> = sessions
            .iter()
            .map(|chat| {
                let mut value = serde_json::to_value(chat).unwrap();
                value["folder"] = json!(folder_name(options, &chat.session.workspace));
                value
            })
            .collect();
        Ok(json!({"sessions":sessions,"next_offset":next}))
    })
    .await
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct HistoryQuery {
    before: Option<i64>,
    #[serde(default)]
    include_archived: bool,
}
async fn history(
    State(shared): State<Arc<Shared>>,
    Path(id): Path<String>,
    Query(query): Query<HistoryQuery>,
) -> Response {
    read(shared, move |store, options| {
        scoped_session(store, options, &id)?;
        Ok(serde_json::to_value(store.history_page(
            &id,
            query.before.unwrap_or(i64::MAX),
            query.include_archived,
        )?)?)
    })
    .await
}
async fn read(
    shared: Arc<Shared>,
    f: impl FnOnce(&Store, &RemoteOptions) -> Result<Value> + Send + 'static,
) -> Response {
    match tokio::task::spawn_blocking(move || {
        f(&Store::open(&shared.options.home)?, &shared.options)
    })
    .await
    {
        Ok(Ok(value)) => Json(value).into_response(),
        Ok(Err(error)) => failure(error),
        Err(_) => problem(StatusCode::INTERNAL_SERVER_ERROR, "Storage worker failed"),
    }
}
fn scoped_session(
    store: &Store,
    options: &RemoteOptions,
    id: &str,
) -> Result<builder_core::store::Session> {
    let session = store.session(id)?;
    // A chat may live in any folder at or below the host root, so the boundary
    // is a prefix check rather than an exact match (see `chat_sessions_within`).
    ensure!(
        session.workspace.starts_with(&options.workspace),
        "Session is outside the exposed workspace"
    );
    Ok(session)
}

/// Path of a chat folder relative to the host root; empty for the root itself.
fn folder_name(options: &RemoteOptions, workspace: &FilePath) -> String {
    workspace
        .strip_prefix(&options.workspace)
        .map(|relative| relative.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// Resolve a browser-supplied folder to an existing directory inside the root.
/// Reuses the tool workspace boundary, so `..`, symlink escapes, and `.git`
/// internals are rejected the same way file tools reject them.
fn chat_folder(options: &RemoteOptions, folder: Option<&str>) -> Result<PathBuf> {
    let folder = folder.unwrap_or("").trim();
    ensure!(folder.len() <= 4096, "Folder path is too long");
    let root = Workspace::new(&options.workspace)?;
    let resolved = root.resolve(folder)?;
    ensure!(
        resolved.is_dir(),
        "Chat folder must be an existing directory inside the workspace"
    );
    Ok(resolved)
}

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct FolderQuery {
    #[serde(default)]
    path: String,
}
/// Subdirectories the browser may pick as a chat folder.
async fn folders(State(shared): State<Arc<Shared>>, Query(query): Query<FolderQuery>) -> Response {
    read(shared, move |_store, options| {
        let directory = chat_folder(options, Some(&query.path))?;
        let path = folder_name(options, &directory);
        let parent = (directory != options.workspace).then(|| {
            directory
                .parent()
                .map(|parent| folder_name(options, parent))
                .unwrap_or_default()
        });
        let mut names: Vec<String> = std::fs::read_dir(&directory)?
            .filter_map(|entry| entry.ok())
            .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir()))
            .filter_map(|entry| entry.file_name().into_string().ok())
            .filter(|name| {
                !name.starts_with('.')
                    && !matches!(name.as_str(), "node_modules" | "target" | "__pycache__")
            })
            .collect();
        names.sort_unstable_by_key(|name| name.to_lowercase());
        let limited = names.len() > 500;
        names.truncate(500);
        Ok(json!({"path":path,"parent":parent,"folders":names,"limited":limited}))
    })
    .await
}

#[derive(Deserialize)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
enum RunAction {
    Message { prompt: String },
    Retry,
    Compact,
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
    let (cancel, receiver) = watch::channel(false);
    if shared.closing.load(Ordering::Acquire) {
        return problem(StatusCode::SERVICE_UNAVAILABLE, "Host is stopping");
    }
    let run = Arc::new(Run {
        snapshot: Mutex::new(Snapshot {
            phase: Phase::Running,
            approval_mode: Some(approval_name(approval)),
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
    {
        let mut state = run.snapshot.lock().unwrap();
        state.approval = None;
        state.reply = None;
        // A preview is never represented as a committed answer.
        state.preview.clear();
        state.preview_limited = false;
        state.thinking.clear();
        match &result {
            None => state.phase = Phase::Paused,
            Some(Ok(())) => state.phase = Phase::Complete,
            Some(Err(error)) => {
                state.phase = Phase::Failed;
                state.error = Some(format!("{error:#}"));
            }
        }
    }
    if matches!(result, Some(Ok(())))
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
                } => crate::ui::nudge_line(calls, step, repeated_reads, planning),
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

async fn chat_info(State(shared): State<Arc<Shared>>, Path(id): Path<String>) -> Response {
    read(shared, move |store, options| {
        let session = scoped_session(store, options, &id)?;
        let draft = store.composer_draft(&id)?;
        let draft_too_large = draft.as_ref().is_some_and(|value| value.len()>65536);
        let folder = folder_name(options, &session.workspace);
        Ok(json!({"session":session,"folder":folder,"archived":store.chat_archived(&id)?,"pending":store.chat_pending(&id)?,"tip":store.chat_tip(&id)?,"draft":if draft_too_large {None}else{draft},"draft_too_large":draft_too_large}))
    }).await
}

async fn profiles(State(shared): State<Arc<Shared>>) -> Response {
    read(shared, move |_store, options| {
        let config = Config::load(&options.config_home)?;
        let profiles: Vec<_> = config.profiles.iter().filter(|(name,p)| p.supports_chat() && options.profile.as_ref().is_none_or(|fixed| fixed==*name))
            .map(|(name,p)|json!({"name":name,"model":p.model})).collect();
        Ok(json!({"profiles":profiles,"default_profile":options.profile.as_ref().unwrap_or(&config.default_profile)}))
    }).await
}

#[derive(Deserialize)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
enum ChatChange {
    Rename { title: String },
    Archive { archived: bool },
    Cancel { expected_tip: i64 },
    Rewind { expected_tip: i64 },
}
async fn manage_chat(
    State(shared): State<Arc<Shared>>,
    Path(id): Path<String>,
    Json(change): Json<ChatChange>,
) -> Response {
    let current = shared.registry.lock().unwrap().runs.get(&id).cloned();
    if current
        .as_ref()
        .is_some_and(|run| !run.done.load(Ordering::Acquire))
    {
        return problem(
            StatusCode::CONFLICT,
            "Pause this chat and wait for it to stop before changing it",
        );
    }
    let result = tokio::task::spawn_blocking(move || -> Result<Value> {
        let mut store = Store::open(&shared.options.home)?;
        scoped_session(&store,&shared.options,&id)?;
        let _guard = store.lock(&id)?;
        let value = match change {
            ChatChange::Rename { title } => { store.rename_chat(&id,&title)?; json!({"renamed":true}) },
            ChatChange::Archive { archived } => { store.archive_chat(&id,archived)?; json!({"archived":archived}) },
            ChatChange::Cancel { expected_tip } => {
                ensure!(store.chat_tip(&id)?==expected_tip,"This chat changed; refresh before cancelling its saved turn");
                let uncertain = store.interrupt_turn(&id,None)?;
                json!({"cancelled":true,"uncertain":uncertain})
            },
            ChatChange::Rewind { expected_tip } => {
                ensure!(store.chat_tip(&id)?==expected_tip,"This chat changed; refresh before rewinding");
                let (draft,uncertain) = store.rewind(&id)?;
                json!({"rewound":true,"draft":if draft.len()<=65536 {Some(draft)}else{None},"uncertain":uncertain})
            },
        };
        // Remove stale terminal status after a transcript-management operation.
        let mut registry = shared.registry.lock().unwrap();
        registry.runs.remove(&id);
        registry.order.retain(|entry| entry != &id);
        Ok(value)
    }).await;
    match result {
        Ok(Ok(value)) => Json(value).into_response(),
        Ok(Err(error)) => failure(error),
        Err(_) => problem(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Chat update failed; inspect saved history",
        ),
    }
}
