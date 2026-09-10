//! Deterministic development regressions, not a held-out quality benchmark.
use builder::{
    code_index,
    retrieval_eval::{Expectation, Region, Thresholds, assess},
};
use builder_core::{config::PipelineSettings, store::Store};
use builder_tools::Workspace;
use serde_json::{Value, json};

async fn retrieve(
    store: &mut Store,
    workspace: &Workspace,
    settings: &PipelineSettings,
    query: &str,
) -> (Value, Vec<Value>) {
    let (body, trace) =
        code_index::search_with_trace(store, "regression", workspace, None, settings, query, None)
            .await
            .unwrap();
    (serde_json::from_str(&body).unwrap(), trace)
}
fn settings() -> PipelineSettings {
    PipelineSettings {
        code_index_semantic: false,
        code_history: false,
        ..Default::default()
    }
}
fn label(path: &str, start: usize, end: usize) -> Region {
    Region {
        path: path.replace('/', std::path::MAIN_SEPARATOR_STR),
        lines: [start, end],
    }
}

#[test]
fn file_hits_cannot_hide_wrong_passages_or_clipped_evidence() {
    let labels = [label("a.rs", 20, 25)];
    let response = json!({"abstained":false,"results":[
        {"path":"a.rs","lines":[1,2],"excerpt":"header\nheader"},
        {"path":"a.rs","lines":[20,25],"excerpt":"visible\npart[excerpt limited]"}
    ]});
    let gates = Expectation::Retrieve(Thresholds {
        min_passage_reciprocal_rank: 1.0,
        min_excerpt_line_recall: 1.0,
        ..Default::default()
    });
    let report = assess(&labels, &gates, &response, &[]).unwrap();
    assert!(!report.passed);
    assert_eq!(report.selected.as_ref().unwrap().file_recall, 1.0);
    assert_eq!(
        report.selected.as_ref().unwrap().passage_reciprocal_rank,
        0.5
    );
    assert_eq!(report.at_k[&1].line_recall, 0.0);
    assert_eq!(report.candidates.unwrap().line_recall, 0.0);
    assert!(
        report
            .failures
            .iter()
            .any(|failure| failure.starts_with("excerpt_line_recall"))
    );
}

#[test]
fn clipped_partial_lines_and_duplicate_chunks_earn_no_extra_credit() {
    let result =
        json!({"path":"a.rs","lines":[1,100],"excerpt":"complete\npartial\n[excerpt limited]"});
    let response = json!({"abstained":false,"results":[result.clone(),result]});
    let report = assess(
        &[label("a.rs", 1, 2)],
        &Expectation::default(),
        &response,
        &[],
    )
    .unwrap();
    assert_eq!(report.excerpts.unwrap().line_recall, 0.5);
    assert_eq!(report.selected.unwrap().returned_lines, 100);
    assert!(assess(&[], &Expectation::Abstain, &response, &[]).is_ok_and(|r| !r.passed));
    assert!(
        assess(
            &[],
            &Expectation::Abstain,
            &json!({"abstained":true,"results":[]}),
            &[]
        )
        .unwrap()
        .passed
    );
    assert!(
        assess(
            &[],
            &Expectation::Abstain,
            &json!({"abstained":false,"results":[]}),
            &[]
        )
        .is_err()
    );
}

#[tokio::test]
async fn multilingual_passages_survive_distractors_and_result_limits() {
    for (path, source) in [
        (
            "src/cache.rs",
            "fn expire_session_cache() { remove_expired_entries(); }\n",
        ),
        (
            "src/cache.py",
            "def expire_session_cache():\n    remove_expired_entries()\n",
        ),
        (
            "src/cache.ts",
            "function expire_session_cache() { remove_expired_entries(); }\n",
        ),
        (
            "src/cache.go",
            "package cache\nfunc expire_session_cache() { remove_expired_entries() }\n",
        ),
    ] {
        let root = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join("src")).unwrap();
        std::fs::write(root.path().join(path), source).unwrap();
        std::fs::write(
            root.path().join("src/distractor.rs"),
            "fn session_statistics() {}\nfn cache_metrics() {}\n",
        )
        .unwrap();
        let workspace = Workspace::new(root.path()).unwrap();
        let mut store = Store::open(home.path()).unwrap();
        for limit in [1, 3] {
            let settings = PipelineSettings {
                code_search_results: limit,
                code_index_chunks_per_file: 1,
                ..settings()
            };
            let (response, trace) =
                retrieve(&mut store, &workspace, &settings, "expire_session_cache").await;
            let gates = Expectation::Retrieve(Thresholds {
                max_results: limit,
                min_passage_reciprocal_rank: 1.0,
                ..Default::default()
            });
            let line = if path.ends_with(".go") { 2 } else { 1 };
            let report = assess(&[label(path, line, line)], &gates, &response, &trace).unwrap();
            assert!(report.passed, "{path}: {:?}", report.failures);
        }
    }
}

#[tokio::test]
async fn contract_changes_renames_deletions_and_restart_never_return_old_evidence() {
    let root = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let workspace = Workspace::new(root.path()).unwrap();
    let mut store = Store::open(home.path()).unwrap();
    let settings = settings();
    let source = root.path().join("contract.rs");
    std::fs::write(
        &source,
        "fn response_contract() -> OldPayload { old_payload() }\n",
    )
    .unwrap();
    let (before, _) = retrieve(&mut store, &workspace, &settings, "response_contract").await;
    let old_hash = before["results"][0]["source_sha256"].clone();
    std::fs::write(
        &source,
        "fn response_contract() -> NewPayload { new_payload() }\n",
    )
    .unwrap();
    let (changed, _) = retrieve(&mut store, &workspace, &settings, "response_contract").await;
    assert!(!changed.to_string().contains("OldPayload"));
    assert_ne!(old_hash, changed["results"][0]["source_sha256"]);
    assert!(
        changed["results"][0]["excerpt"]
            .as_str()
            .unwrap()
            .contains("NewPayload")
    );
    std::fs::rename(&source, root.path().join("renamed.rs")).unwrap();
    drop(store);
    let mut store = Store::open(home.path()).unwrap();
    let (renamed, _) = retrieve(&mut store, &workspace, &settings, "response_contract").await;
    assert_eq!(renamed["results"][0]["path"], "renamed.rs");
    assert!(!renamed.to_string().contains("contract.rs"));
    std::fs::remove_file(root.path().join("renamed.rs")).unwrap();
    let (deleted, trace) = retrieve(&mut store, &workspace, &settings, "response_contract").await;
    assert!(
        assess(&[], &Expectation::Abstain, &deleted, &trace)
            .unwrap()
            .passed
    );
    assert!(trace.is_empty());
}

#[test]
fn invalid_gates_and_overflowing_labels_fail_explicitly() {
    let gates = Expectation::Retrieve(Thresholds {
        min_line_recall: f64::NAN,
        ..Default::default()
    });
    assert!(gates.validate(&[label("a", 1, 1)]).is_err());
    assert!(Expectation::Abstain.validate(&[label("a", 1, 1)]).is_err());
    assert!(
        builder::retrieval_eval::score(
            &[label("a", 1, usize::MAX - 1), label("b", 1, usize::MAX - 1)],
            &[]
        )
        .is_err()
    );
}

#[tokio::test]
async fn isolated_workspaces_and_disabled_index_do_not_leak_results() {
    let home = tempfile::tempdir().unwrap();
    let first = tempfile::tempdir().unwrap();
    let second = tempfile::tempdir().unwrap();
    std::fs::write(
        first.path().join("private.rs"),
        "fn unique_workspace_secret() {}\n",
    )
    .unwrap();
    std::fs::write(second.path().join("other.rs"), "fn public_operation() {}\n").unwrap();
    let first = Workspace::new(first.path()).unwrap();
    let second = Workspace::new(second.path()).unwrap();
    let mut store = Store::open(home.path()).unwrap();
    let settings = settings();
    let (found, _) = retrieve(&mut store, &first, &settings, "unique_workspace_secret").await;
    assert!(!found["results"].as_array().unwrap().is_empty());
    let (absent, trace) = retrieve(&mut store, &second, &settings, "unique_workspace_secret").await;
    assert!(
        assess(&[], &Expectation::Abstain, &absent, &trace)
            .unwrap()
            .passed
    );
    let disabled = PipelineSettings {
        code_index: false,
        ..settings
    };
    assert!(
        code_index::search(
            &mut store,
            "disabled",
            &first,
            None,
            &disabled,
            "unique_workspace_secret",
            None
        )
        .await
        .is_err()
    );
}

#[tokio::test]
async fn long_unicode_callable_tail_remains_a_retrievable_candidate() {
    let home = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    let mut source = String::from("fn process_stream() {\n");
    for _ in 0..250 {
        source.push_str("    consume(\"雪❄️ payload\");\n");
    }
    source.push_str("    flush_tail_checkpoint();\n}\n");
    std::fs::write(root.path().join("stream.rs"), source).unwrap();
    let workspace = Workspace::new(root.path()).unwrap();
    let mut store = Store::open(home.path()).unwrap();
    let (response, trace) =
        retrieve(&mut store, &workspace, &settings(), "flush_tail_checkpoint").await;
    let report = assess(
        &[label("stream.rs", 252, 252)],
        &Expectation::default(),
        &response,
        &trace,
    )
    .unwrap();
    assert!(report.passed, "{:?}", report.failures);
    assert_eq!(report.candidates.unwrap().line_recall, 1.0);
}
