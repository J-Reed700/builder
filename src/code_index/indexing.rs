use super::{clip, scope, watcher::CodeIndexWatch};
use crate::memory::MemoryRuntime;
use anyhow::{Result, ensure};
use builder_core::{
    code_index::{CodeChunk, CodeIndexStatus},
    config::PipelineSettings,
    store::{CodeIndexGuard, Store},
};
use builder_tools::Workspace;

pub fn refresh(
    store: &mut Store,
    workspace: &Workspace,
    settings: &PipelineSettings,
) -> Result<bool> {
    ensure!(settings.code_index, "Code index is disabled in /settings");
    let Some(writer) = store.try_code_index_lock(&scope(workspace))? else {
        // Another holder already owns the rebuild. Readers continue using the
        // last atomically published generation instead of queuing more work.
        return Ok(false);
    };
    refresh_locked(store, &writer, workspace, settings)
}

pub(super) fn refresh_locked(
    store: &mut Store,
    writer: &CodeIndexGuard,
    workspace: &Workspace,
    settings: &PipelineSettings,
) -> Result<bool> {
    match builder_tools::code_index::capture(workspace, settings) {
        Ok(snapshot) => store.code_index_replace(writer, &snapshot),
        Err(error) => {
            store.code_index_record_failure(writer, &error.to_string())?;
            Err(error)
        }
    }
}

pub async fn maintain(
    store: &mut Store,
    workspace: &Workspace,
    memory: Option<&MemoryRuntime>,
    settings: &PipelineSettings,
    mut watch: Option<&mut CodeIndexWatch>,
) -> Result<()> {
    if !settings.code_index || !settings.code_index_background {
        return Ok(());
    }
    let index_scope = scope(workspace);
    // Held for the whole pass so two processes on one checkout never embed
    // the same chunks twice; foreground callers skip writes meanwhile.
    let Some(writer) = store.try_code_index_lock(&index_scope)? else {
        return Ok(());
    };
    refresh_locked(store, &writer, workspace, settings)?;
    if settings.code_history {
        let _ = refresh_history_locked(store, &writer, workspace, settings).await;
    }
    if !settings.code_index_semantic {
        return Ok(());
    }
    let Some(memory) = memory else {
        return Ok(());
    };
    memory.reset_embeddings();
    let Some(fingerprint) = memory.embedding_fingerprint() else {
        return Ok(());
    };
    loop {
        let pending = store.code_index_pending_vectors(
            &index_scope,
            fingerprint,
            settings.code_index_embedding_batch,
        )?;
        if pending.is_empty() {
            break;
        }
        let inputs = pending.iter().map(embedding_text).collect::<Vec<_>>();
        let Some(vectors) = memory
            .embed_code_batch(&inputs, settings.code_index_embedding_timeout_secs)
            .await
        else {
            break;
        };
        let batch = pending
            .into_iter()
            .zip(vectors)
            .map(|(chunk, vector)| (chunk.content_hash, vector))
            .collect::<Vec<_>>();
        store.code_index_set_vectors(&writer, fingerprint, &batch)?;
        // A large first-time vector build must not postpone current-code
        // publication. Reconcile a debounced change between bounded batches.
        if let Some(watch) = watch.as_deref_mut()
            && watch.take_pending(settings.code_index_debounce_ms).await
        {
            refresh_locked(store, &writer, workspace, settings)?;
        }
        tokio::task::yield_now().await;
    }
    Ok(())
}

pub fn status(store: &Store, workspace: &Workspace) -> Result<Option<CodeIndexStatus>> {
    store.code_index_status(&scope(workspace))
}

pub(super) async fn refresh_history_locked(
    store: &mut Store,
    writer: &CodeIndexGuard,
    workspace: &Workspace,
    settings: &PipelineSettings,
) -> Result<bool> {
    let Some(snapshot) = builder_tools::git_history::capture(
        workspace,
        settings.code_history_commits,
        settings.code_history_timeout_secs,
    )
    .await?
    else {
        return Ok(false);
    };
    store.code_history_replace(writer, &snapshot)?;
    Ok(true)
}

pub fn coverage(
    store: &Store,
    workspace: &Workspace,
    memory: Option<&MemoryRuntime>,
) -> Result<Option<(usize, usize)>> {
    memory
        .and_then(MemoryRuntime::embedding_fingerprint)
        .map(|fingerprint| store.code_index_vector_coverage(&scope(workspace), fingerprint))
        .transpose()
}

pub fn query_summary(
    store: &Store,
    workspace: &Workspace,
) -> Result<builder_core::code_index::CodeQuerySummary> {
    store.code_index_query_summary(&scope(workspace))
}

fn embedding_text(chunk: &CodeChunk) -> String {
    clip(
        &format!(
            "path: {}\nlanguage: {}\nkind: {}\nsymbols: {}\n{}",
            chunk.path,
            chunk.language,
            chunk.kind,
            chunk.symbols.join(", "),
            chunk.content
        ),
        8192,
    )
}
