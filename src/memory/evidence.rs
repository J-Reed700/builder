//! Validate source evidence against active journal entries and current files.
use super::MemoryRuntime;
use anyhow::{Context, Result, ensure};
use builder_core::{
    memory::{Evidence, Memory, MemoryKind},
    protocol::Message,
    store::Store,
};
use builder_tools::{Action, Workspace};
use std::collections::HashMap;

impl MemoryRuntime {
    pub(super) fn evidence(
        &self,
        store: &Store,
        session: &str,
        workspace: &Workspace,
        ids: &[String],
    ) -> Result<Vec<Evidence>> {
        ensure!(
            !ids.is_empty() && ids.len() <= 8,
            "Findings need 1–8 successful read_file evidence IDs"
        );
        let mut out = Vec::new();
        for id in ids {
            let reads = store.memory_source_reads(session, 0, Some(id), 1)?;
            let (seq, result, call) = reads.first().context(
                "Evidence is not an active durable source read; read the relevant range again",
            )?;
            out.push(Self::source_evidence(
                session, workspace, *seq, result, call,
            )?);
        }
        Ok(out)
    }
    pub(super) fn source_evidence(
        session: &str,
        workspace: &Workspace,
        seq: i64,
        result: &Message,
        call: &builder_core::protocol::ToolCall,
    ) -> Result<Evidence> {
        let Action::ReadFile { path, .. } = Action::from_call(call)? else {
            anyhow::bail!("Only successful source reads support repository findings")
        };
        let result = result.content.as_deref().unwrap_or("");
        ensure!(
            !result.starts_with("ERROR:") && !result.starts_with("DENIED:"),
            "Failed reads cannot support findings"
        );
        let hash = result
            .lines()
            .nth(1)
            .and_then(|l| l.strip_prefix("Source-SHA256: "))
            .context("Read lacks source version; read the range again")?;
        ensure!(
            hash == workspace.source_hash(&path)?,
            "Source changed since this read; refresh evidence before saving"
        );
        Ok(Evidence {
            session: session.into(),
            seq,
            call_id: call.id.clone(),
            path,
            hash: hash.into(),
        })
    }
    pub(super) fn fresh(
        store: &Store,
        workspace: &Workspace,
        memory: &Memory,
        hashes: &mut HashMap<String, Option<String>>,
    ) -> bool {
        if memory.kind == MemoryKind::Preference {
            return true;
        }
        if memory.evidence.is_empty()
            || !store
                .memory_event_active(&memory.origin_session, memory.origin_seq)
                .unwrap_or(false)
        {
            return false;
        }
        memory.evidence.iter().all(|e| {
            store
                .memory_event_active(&e.session, e.seq)
                .unwrap_or(false)
                && hashes
                    .entry(e.path.clone())
                    .or_insert_with(|| workspace.source_hash(&e.path).ok())
                    .as_deref()
                    == Some(e.hash.as_str())
        })
    }
}
