//! Memory tool policy. Tool schemas belong to builder-tools.
use super::MemoryRuntime;
use anyhow::{Context, Result};
use builder_core::{
    memory::{Memory, MemoryKind},
    store::Store,
};
use builder_tools::{Action, Workspace};
use serde_json::json;
use std::collections::HashMap;

pub use builder_tools::memory_definitions as definitions;

impl MemoryRuntime {
    pub async fn execute(
        &self,
        store: &mut Store,
        session: &str,
        workspace: &Workspace,
        action: &Action,
    ) -> Result<String> {
        self.execute_with_settings(store, session, workspace, action, None)
            .await
    }

    pub async fn execute_with_settings(
        &self,
        store: &mut Store,
        session: &str,
        workspace: &Workspace,
        action: &Action,
        settings: Option<&builder_core::config::PipelineSettings>,
    ) -> Result<String> {
        let scope = Self::scope(workspace);
        let value = match action {
            Action::MemorySearch { query } => match settings {
                Some(settings) => {
                    self.search_with_settings(store, workspace, query, settings)
                        .await?
                }
                None => self.search(store, workspace, query).await?,
            },
            Action::MemoryGet { key, revision } => {
                let memory = store
                    .memory_get(&scope, key, *revision)?
                    .context("Memory not found")?;
                let fresh = Self::fresh(store, workspace, &memory, &mut HashMap::new());
                json!({"fresh":fresh,"historical_revision":revision.is_some(),"memory":memory,"instruction":"Historical/stale contents are not current truth"})
            }
            Action::MemoryUpsert {
                key,
                text,
                expected_revision,
                evidence_call_ids,
            } => {
                let evidence = self.evidence(store, session, workspace, evidence_call_ids)?;
                let memory = Memory {
                    key: key.clone(),
                    revision: 0,
                    kind: MemoryKind::Finding,
                    text: text.clone(),
                    evidence,
                    origin_session: session.into(),
                    origin_seq: store.memory_latest_seq(session)?,
                    created_at: String::new(),
                };
                json!(store.memory_put(&scope, *expected_revision, memory)?)
            }
            Action::MemoryForget {
                key,
                expected_revision,
            } => {
                store.memory_forget(&scope, key, *expected_revision)?;
                json!({"forgotten":key,"note":"Removed from retrieval; original transcript and revisions retained"})
            }
            _ => anyhow::bail!("Not a memory action"),
        };
        Ok(serde_json::to_string(&value)?)
    }
}

pub fn is_memory(action: &Action) -> bool {
    matches!(
        action,
        Action::MemorySearch { .. }
            | Action::MemoryGet { .. }
            | Action::MemoryUpsert { .. }
            | Action::MemoryForget { .. }
    )
}
