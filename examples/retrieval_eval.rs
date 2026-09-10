//! Offline retrieval evaluation. The manifest supplies independent labels;
//! reports retain ranked evidence so aggregate scores can be audited.
use anyhow::{Result, ensure};
use builder::{
    code_index,
    memory::MemoryRuntime,
    retrieval_eval::{Expectation, Region, assess},
};
use builder_core::{config::PipelineSettings, store::Store};
use builder_tools::Workspace;
use serde::Deserialize;
use serde_json::json;
use std::{io::Read, path::PathBuf};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Case {
    id: String,
    workspace: PathBuf,
    query: String,
    relevant: Vec<Region>,
    source_hashes: std::collections::BTreeMap<String, String>,
    #[serde(default)]
    expectation: Expectation,
}

#[tokio::main]
async fn main() -> Result<()> {
    let path = std::env::args_os()
        .nth(1)
        .ok_or_else(|| anyhow::anyhow!("Pass a JSON case manifest"))?;
    let path = PathBuf::from(path).canonicalize()?;
    let mut bytes = Vec::new();
    std::fs::File::open(&path)?
        .take(4 * 1024 * 1024 + 1)
        .read_to_end(&mut bytes)?;
    ensure!(bytes.len() <= 4 * 1024 * 1024, "Manifest exceeds 4 MiB");
    let mut cases: Vec<Case> = serde_json::from_slice(&bytes)?;
    for case in &mut cases {
        if case.workspace.is_relative() {
            case.workspace = path.parent().unwrap().join(&case.workspace);
        }
    }
    ensure!(
        !cases.is_empty() && cases.len() <= 1000,
        "Expected 1–1000 cases"
    );
    let mut ids = std::collections::HashSet::new();
    for case in &cases {
        ensure!(ids.insert(&case.id), "Duplicate case ID");
        ensure!(case.workspace.is_absolute(), "Workspace must be absolute");
        case.expectation.validate(&case.relevant)?;
    }
    let memory = if let Some(model) = std::env::var_os("BUILDER_LOCAL_MODEL_DIR") {
        let mut config = builder_core::config::Config::default();
        config.memory.enabled = true;
        config.memory.embedding_backend = builder_core::config::EmbeddingBackend::Local;
        config.memory.local_model_dir = Some(model.into());
        MemoryRuntime::from_config(&config)?
    } else {
        None
    };
    let mut variants = vec![(0, false), (2, false)];
    if memory.is_some() {
        variants.push((2, true));
    }
    let mut evaluated = 0usize;
    let mut failed = 0usize;
    for case in cases {
        let workspace = Workspace::new(&case.workspace)?;
        validate_sources(&case, &workspace)?;
        let mut snapshot = None;
        for &(graph_hops, semantic) in &variants {
            let home = tempfile::tempdir()?;
            let mut store = Store::open(home.path())?;
            let settings = PipelineSettings {
                code_index_semantic: semantic,
                code_history: false,
                code_index_graph_hops: graph_hops,
                ..Default::default()
            };
            let build_started = std::time::Instant::now();
            if semantic {
                tokio::time::timeout(
                    std::time::Duration::from_secs(1800),
                    code_index::maintain(&mut store, &workspace, memory.as_ref(), &settings, None),
                )
                .await??;
            }
            let build_ms = build_started.elapsed().as_millis();
            let started = std::time::Instant::now();
            let (response, trace) = code_index::search_with_trace(
                &mut store,
                "evaluation",
                &workspace,
                if semantic { memory.as_ref() } else { None },
                &settings,
                &case.query,
                Some(10),
            )
            .await?;
            let response: serde_json::Value = serde_json::from_str(&response)?;
            let current_snapshot = response["index"]["snapshot_hash"]
                .as_str()
                .ok_or_else(|| anyhow::anyhow!("Missing snapshot identity"))?;
            if let Some(previous) = &snapshot {
                ensure!(
                    previous == current_snapshot,
                    "Checkout changed between evaluation variants"
                );
            } else {
                snapshot = Some(current_snapshot.to_owned());
            }
            for (path, expected) in &case.source_hashes {
                ensure!(
                    &workspace.source_hash(path)? == expected,
                    "Labeled source changed during evaluation: {path}"
                );
            }
            if semantic {
                let coverage = &response["retrieval"]["semantic_coverage"];
                ensure!(
                    response["retrieval"]["semantic"] == true
                        && coverage["indexed"].as_u64().is_some()
                        && coverage["indexed"] == coverage["total"],
                    "Incomplete semantic coverage; refusing misleading comparison"
                );
            }
            let assessment = assess(&case.relevant, &case.expectation, &response, &trace)?;
            evaluated += 1;
            failed += usize::from(!assessment.passed);
            println!(
                "{}",
                json!({"case":case.id,"workspace":case.workspace,
                "variant":format!("graph_{graph_hops}_semantic_{semantic}"),"metrics":assessment.selected,"assessment":assessment,
                "embedding_build_ms":build_ms,"elapsed_ms":started.elapsed().as_millis(),"evidence":response,"candidate_trace":trace})
            );
        }
    }
    eprintln!("Evaluated {evaluated} case/variant pairs; {failed} failed quality gates");
    ensure!(
        failed == 0,
        "Retrieval evaluation failed: {failed}/{evaluated} pairs"
    );
    Ok(())
}

fn validate_sources(case: &Case, workspace: &Workspace) -> Result<()> {
    for (path, expected) in &case.source_hashes {
        ensure!(
            &workspace.source_hash(path)? == expected,
            "Stale evaluation source: {path}"
        );
    }
    for region in &case.relevant {
        let expected = case
            .source_hashes
            .get(&region.path)
            .ok_or_else(|| anyhow::anyhow!("Missing labeled source hash for {}", region.path))?;
        let mut source = String::new();
        std::fs::File::open(workspace.resolve(&region.path)?)?
            .take(4 * 1024 * 1024 + 1)
            .read_to_string(&mut source)?;
        ensure!(
            source.len() <= 4 * 1024 * 1024,
            "Evaluation source exceeds 4 MiB"
        );
        ensure!(
            region.lines[1] <= source.lines().count(),
            "Label extends past source: {}",
            region.path
        );
        ensure!(
            &workspace.source_hash(&region.path)? == expected,
            "Stale evaluation label: {}",
            region.path
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn changed_labels_are_rejected_before_evaluation() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("a.rs"), "fn before() {}\n").unwrap();
        let workspace = Workspace::new(root.path()).unwrap();
        let mut case = Case {
            id: "stale".into(),
            expectation: Expectation::default(),
            workspace: root.path().into(),
            query: "function".into(),
            relevant: vec![Region {
                path: "a.rs".into(),
                lines: [1, 1],
            }],
            source_hashes: [("a.rs".into(), workspace.source_hash("a.rs").unwrap())].into(),
        };
        validate_sources(&case, &workspace).unwrap();
        case.relevant[0].lines = [2, 2];
        assert!(validate_sources(&case, &workspace).is_err());
        case.relevant[0].lines = [1, 1];
        std::fs::write(root.path().join("a.rs"), "fn after() {}\n").unwrap();
        assert!(validate_sources(&case, &workspace).is_err());
        case.relevant.clear();
        case.expectation = Expectation::Abstain;
        // Negative cases still validate their pinned distractor sources.
        assert!(validate_sources(&case, &workspace).is_err());
    }
}
