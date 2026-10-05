use super::auth::{failure, problem};
use super::{RemoteOptions, Shared};
use anyhow::{Result, ensure};
use axum::{
    Json, Router,
    extract::{Path, Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::get,
};
use builder_core::{config::Config, store::Store};
use builder_tools::Workspace;
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    path::{Path as FilePath, PathBuf},
    sync::{Arc, atomic::Ordering},
};

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
pub(super) fn router() -> Router<Arc<Shared>> {
    Router::new()
        .route("/api/sessions", get(sessions))
        .route("/api/sessions/{id}/messages", get(history))
        .route("/api/sessions/{id}", get(chat_info).post(manage_chat))
        .route("/api/profiles", get(profiles))
        .route("/api/folders", get(folders))
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
pub(super) fn scoped_session(
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
pub(super) fn chat_folder(options: &RemoteOptions, folder: Option<&str>) -> Result<PathBuf> {
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
