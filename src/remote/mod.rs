//! Authenticated application adapter. The existing agent owns all durable execution semantics.

mod auth;
mod catalog;
pub mod connection;
mod execution;

use self::auth::{authenticate, headers, load_token};
use crate::agent::ApprovalMode;
use anyhow::{Context, Result, ensure};
use axum::{Router, extract::DefaultBodyLimit, http::HeaderValue, middleware, routing::get};
use builder_core::config::Config;
use builder_tools::Workspace;
use serde::Serialize;
use std::{
    collections::{HashMap, HashSet, VecDeque},
    path::{Path as FilePath, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    time::Duration,
};
use tokio::sync::{Semaphore, watch};

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
    /// Transport completion is separate from evidence-backed task completion.
    task_outcome: Option<crate::completion::Outcome>,
    /// Approval mode this run was started with.
    approval_mode: Option<&'static str>,
    /// The operation currently represented by this run. The browser combines
    /// this with `compacting` to identify where a queued message belongs.
    operation: Option<&'static str>,
    /// True while this run is inside context compaction. Automatic compaction
    /// happens inside an ordinary message run, so the operation name alone is
    /// not enough for clients to identify this window.
    compacting: bool,
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
            .merge(catalog::router())
            .merge(execution::router())
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
                        include_str!("../../remote/web/index.html"),
                    )
                }),
            )
            .route(
                "/app.js",
                get(|| async {
                    (
                        [("content-type", "text/javascript; charset=utf-8")],
                        include_str!("../../remote/web/app.js"),
                    )
                }),
            )
            .route(
                "/style.css",
                get(|| async {
                    (
                        [("content-type", "text/css; charset=utf-8")],
                        include_str!("../../remote/web/style.css"),
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
