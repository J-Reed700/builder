use crate::cli::{Cli, MemoryCommand};
use anyhow::{Context, Result, ensure};
use builder::{memory::MemoryRuntime, ui};
use builder_core::{
    config::{self, Config},
    memory::{Memory, MemoryKind},
    store::Store,
};
use builder_tools::Workspace;
use std::path::Path;

pub(crate) async fn run(
    command: &MemoryCommand,
    cli: &Cli,
    config: &mut Config,
    config_home: &Path,
    home: &Path,
    store: &mut Store,
) -> Result<()> {
    let workspace = Workspace::new(&cli.workspace)?;
    let scope = MemoryRuntime::scope(&workspace);
    let result = match command {
        MemoryCommand::Enable {
            local: _,
            lexical,
            embedding_profile,
            query_prefix,
            document_prefix,
            embedding_revision,
        } => {
            let previous = config.memory.clone();
            config.memory.enabled = true;
            if let Some(value) = query_prefix {
                config.memory.query_prefix = value.clone();
            }
            if let Some(value) = document_prefix {
                config.memory.document_prefix = value.clone();
            }
            if let Some(value) = embedding_revision {
                config.memory.embedding_revision = value.clone();
            }
            if *lexical {
                config.memory.embedding_backend = config::EmbeddingBackend::Lexical;
            } else if let Some(name) = embedding_profile {
                config.memory.embedding_backend = config::EmbeddingBackend::Remote;
                config.memory.embedding_profile = Some(name.clone());
            } else {
                setup_local_memory(config, home).await?;
            }
            if let Err(error) = MemoryRuntime::from_config(config) {
                config.memory = previous;
                return Err(error);
            }
            let mut latest = Config::load(config_home)?;
            ensure!(
                latest.memory == previous,
                "Memory settings changed during setup; reopen /memory before saving"
            );
            latest.memory = config.memory.clone();
            latest.save(config_home)?;
            *config = latest;
            serde_json::json!({"enabled":true,"backend":config.memory.embedding_backend,"local_model_dir":config.memory.local_model_dir,"note":"Restart running sessions to load memory settings"})
        }
        MemoryCommand::Disable => {
            config.memory.enabled = false;
            config.save(config_home)?;
            serde_json::json!({"enabled":false})
        }
        MemoryCommand::Status => {
            serde_json::json!({"settings":config.memory,"checkout_records":store.memory_list(&scope)?.len(),"user_preferences":store.memory_list("@user")?.len(),"storage":"SQLite; revisions retained; memory never grants permissions"})
        }
        MemoryCommand::List { user } => {
            serde_json::json!(store.memory_list(if *user { "@user" } else { &scope })?)
        }
        MemoryCommand::Get {
            key,
            revision,
            user,
        } => serde_json::json!(store.memory_get(
            if *user { "@user" } else { &scope },
            key,
            *revision
        )?),
        MemoryCommand::Remember { key, text } => {
            let previous = store.memory_get("@user", key, None)?;
            serde_json::json!(store.memory_put(
                "@user",
                previous.map_or(0, |memory| memory.revision),
                Memory {
                    key: key.clone(),
                    revision: 0,
                    kind: MemoryKind::Preference,
                    text: text.clone(),
                    evidence: vec![],
                    origin_session: String::new(),
                    origin_seq: 0,
                    created_at: String::new()
                }
            )?)
        }
        MemoryCommand::Forget { key, user } => {
            let scope = if *user { "@user" } else { &scope };
            let memory = store
                .memory_get(scope, key, None)?
                .context("Memory not found")?;
            store.memory_forget(scope, key, memory.revision)?;
            serde_json::json!({"forgotten":key,"note":"Original transcript and revision audit remain"})
        }
        MemoryCommand::Task { session } => {
            serde_json::json!(store.memory_task(&store.resolve(session)?.id)?)
        }
        MemoryCommand::Refresh { session } => {
            let saved = store.resolve(session)?;
            let _guard = store.lock(&saved.id)?;
            let workspace = Workspace::new(Path::new(&saved.workspace))?;
            let (_, profile) = config.profile(cli.profile.as_deref().or(Some(&saved.profile)))?;
            let provider = MemoryRuntime::extraction_provider(&profile)?;
            let runtime = MemoryRuntime::from_config(config)?.context("Enable memory first")?;
            let extracted = runtime
                .extract(
                    &provider,
                    store,
                    &saved.id,
                    &workspace,
                    true,
                    (profile.context_tokens, &mut |_| {}),
                )
                .await?;
            runtime.index_pending(store, &workspace).await?;
            serde_json::json!({"session":saved.id,"workspace":workspace.root(),"extracted":extracted,
                "checkout_records":store.memory_list(&MemoryRuntime::scope(&workspace))?.len()})
        }
        MemoryCommand::Search { query } => {
            let runtime =
                MemoryRuntime::from_config(config)?.unwrap_or_else(MemoryRuntime::lexical);
            runtime.search(store, &workspace, query).await?
        }
        MemoryCommand::Index => {
            let runtime = MemoryRuntime::from_config(config)?.context("Enable memory first")?;
            runtime.index_pending(store, &workspace).await?;
            serde_json::json!({"processed":"up to two missing vectors; repeat to drain queue"})
        }
    };
    println!("{}", ui::safe(&serde_json::to_string_pretty(&result)?));
    Ok(())
}

pub(crate) async fn setup_local_memory(config: &mut Config, home: &Path) -> Result<()> {
    let directory = home.join("models").join("all-MiniLM-L6-v2");
    eprintln!("Preparing local embeddings (~91 MB download on first setup)…");
    tokio::time::timeout(
        std::time::Duration::from_secs(300),
        builder_embedding::install(&directory),
    )
    .await
    .context("Local model setup deadline exceeded; rerun to resume completed files")??;
    let local = builder_embedding::LocalEmbedding::new(Some(directory.clone()));
    tokio::time::timeout(
        std::time::Duration::from_secs(30),
        local.embed("Local embedding readiness check"),
    )
    .await
    .context("Local model readiness deadline exceeded")??;
    config.memory.enabled = true;
    config.memory.embedding_backend = config::EmbeddingBackend::Local;
    config.memory.local_model_dir = Some(directory);
    config.memory.embedding_profile = None;
    config.memory.query_prefix.clear();
    config.memory.document_prefix.clear();
    Ok(())
}
