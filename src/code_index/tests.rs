use super::*;
use crate::memory::MemoryRuntime;
use builder_core::{
    config::PipelineSettings,
    protocol::{Message, Role},
    store::Store,
};
use builder_tools::Workspace;
use serde_json::{Value, json};

#[tokio::test]
async fn watcher_coalesces_a_source_change_and_ignores_build_output() {
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir(root.path().join("target")).unwrap();
    let mut watch = CodeIndexWatch::new(root.path()).unwrap();
    std::fs::write(root.path().join("arena.rs"), "fn changed() {}\n").unwrap();
    assert!(
        tokio::time::timeout(std::time::Duration::from_secs(5), watch.wait(60, 50))
            .await
            .unwrap()
    );
    assert!(!should_refresh(
        root.path(),
        &root.path().join("target/debug/builder")
    ));
}

#[tokio::test]
async fn lexical_search_refreshes_changed_source_and_never_returns_stale_text() {
    let home = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    std::fs::write(
        root.path().join("arena.rs"),
        "fn old_shield_drop() -> f32 { 0.5 }\n",
    )
    .unwrap();
    let workspace = Workspace::new(root.path()).unwrap();
    let mut store = Store::open(home.path()).unwrap();
    let settings = PipelineSettings {
        code_index_semantic: false,
        ..Default::default()
    };
    let first: Value = serde_json::from_str(
        &search(
            &mut store,
            "session",
            &workspace,
            None,
            &settings,
            "old_shield_drop",
            None,
        )
        .await
        .unwrap(),
    )
    .unwrap();
    assert_eq!(first["results"][0]["path"], "arena.rs");
    std::fs::write(
        root.path().join("arena.rs"),
        "fn boss_collision() -> bool { true }\n",
    )
    .unwrap();
    let changed: Value = serde_json::from_str(
        &search(
            &mut store,
            "session",
            &workspace,
            None,
            &settings,
            "boss_collision",
            None,
        )
        .await
        .unwrap(),
    )
    .unwrap();
    assert_eq!(changed["results"][0]["symbols"][0], "boss_collision");
    assert!(!changed.to_string().contains("old_shield_drop"));
}

#[tokio::test]
async fn search_serves_published_generation_while_another_holder_writes() {
    let home = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("arena.rs"), "fn shield_drop() {}\n").unwrap();
    let workspace = Workspace::new(root.path()).unwrap();
    let mut store = Store::open(home.path()).unwrap();
    let settings = PipelineSettings {
        code_index_semantic: false,
        ..Default::default()
    };
    refresh(&mut store, &workspace, &settings).unwrap();
    let maintenance = Store::open(home.path()).unwrap();
    let _writer = maintenance
        .try_code_index_lock(workspace.root().to_str().unwrap())
        .unwrap()
        .unwrap();
    let result: Value = serde_json::from_str(
        &search(
            &mut store,
            "session",
            &workspace,
            None,
            &settings,
            "shield_drop",
            None,
        )
        .await
        .unwrap(),
    )
    .unwrap();
    assert_eq!(result["results"][0]["path"], "arena.rs");
    assert_eq!(result["stale_paths_removed"], false);
    assert_eq!(
        result["telemetry"]["reason"],
        "index maintenance in progress"
    );
}

#[tokio::test]
async fn irrelevant_query_abstains_without_dense_vectors() {
    let home = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("arena.rs"), "fn shield_drop() {}\n").unwrap();
    let workspace = Workspace::new(root.path()).unwrap();
    let mut store = Store::open(home.path()).unwrap();
    let settings = PipelineSettings {
        code_index_semantic: false,
        ..Default::default()
    };
    let result: Value = serde_json::from_str(
        &search(
            &mut store,
            "session",
            &workspace,
            None,
            &settings,
            "unrelated_elephant",
            None,
        )
        .await
        .unwrap(),
    )
    .unwrap();
    assert_eq!(result["abstained"], true);
    assert!(result["results"].as_array().unwrap().is_empty());
}

#[tokio::test]
async fn each_retrieval_channel_votes_once_even_with_recursive_symbols() {
    let home = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    std::fs::write(
        root.path().join("signal.rs"),
        "fn signal() { signal(); }\nfn relay() { signal(); }\n",
    )
    .unwrap();
    let workspace = Workspace::new(root.path()).unwrap();
    let mut store = Store::open(home.path()).unwrap();
    let settings = PipelineSettings {
        code_index_semantic: false,
        code_history: false,
        ..Default::default()
    };
    let (_, trace) = search_with_trace(
        &mut store,
        "votes",
        &workspace,
        None,
        &settings,
        "signal",
        Some(10),
    )
    .await
    .unwrap();
    assert!(!trace.is_empty());
    for candidate in trace {
        let mut expected = 0.0;
        for (field, weight) in [("lexical_rank", 1.0), ("exact_rank", 1.35)] {
            if let Some(rank) = candidate[field].as_u64() {
                expected += reciprocal(rank as usize - 1, weight);
            }
        }
        if let Some(rank) = candidate["graph_rank"].as_u64() {
            expected += reciprocal(
                rank as usize - 1,
                0.45 / candidate["graph_hop"].as_u64().unwrap() as f64,
            );
        }
        assert!(
            (candidate["score"].as_f64().unwrap() - expected).abs() < 1e-12,
            "{candidate}"
        );
    }
}

#[tokio::test]
async fn held_out_issue_queries_localize_the_expected_file_at_rank_one() {
    let home = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    for (path, source) in [
        (
            "src/arena/powerups.rs",
            "pub fn shield_drop_weight(mode: Mode) -> f32 { if mode.is_arena() { 0.02 } else { 0.2 } }\n",
        ),
        (
            "src/arena/collision.rs",
            "pub fn resolve_boss_ship_collision(ship: &mut Ship, boss: &Boss) { separate_bodies(ship, boss); }\n",
        ),
        (
            "src/config/context.rs",
            "pub fn configured_context_window(profile: &Profile) -> usize { profile.context_tokens }\n",
        ),
        (
            "docs/arena.md",
            "Arena includes ships, a boss, shields, configuration, and collision rules.\n",
        ),
    ] {
        let file = root.path().join(path);
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(file, source).unwrap();
    }
    let workspace = Workspace::new(root.path()).unwrap();
    let mut store = Store::open(home.path()).unwrap();
    let settings = PipelineSettings {
        code_index_semantic: false,
        ..Default::default()
    };
    for (query, expected) in [
        (
            "greatly decrease shield powerup drops in arena mode",
            "src/arena/powerups.rs",
        ),
        (
            "prevent the player ship from passing through the arena boss body",
            "src/arena/collision.rs",
        ),
        (
            "honor the configured context token window",
            "src/config/context.rs",
        ),
    ] {
        let result: Value = serde_json::from_str(
            &search(
                &mut store,
                "held-out",
                &workspace,
                None,
                &settings,
                query,
                Some(5),
            )
            .await
            .unwrap(),
        )
        .unwrap();
        assert_eq!(result["results"][0]["path"], expected, "query: {query}");
    }
    let summary = query_summary(&store, &workspace).unwrap();
    assert_eq!(summary.queries, 3);
    assert_eq!(summary.abstentions, 0);
}

#[tokio::test]
async fn git_history_can_localize_current_code_but_is_labelled_historical() {
    let home = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir(root.path().join("src")).unwrap();
    std::fs::write(
        root.path().join("src/tuning.rs"),
        "pub fn competitive_weight() -> f32 { 0.02 }\n",
    )
    .unwrap();
    for arguments in [
        vec!["init"],
        vec!["config", "user.email", "builder@example.invalid"],
        vec!["config", "user.name", "Builder Test"],
        vec!["add", "src/tuning.rs"],
        vec!["commit", "-m", "Reduce arena shield drop rate"],
    ] {
        assert!(
            std::process::Command::new("git")
                .current_dir(root.path())
                .args(arguments)
                .output()
                .unwrap()
                .status
                .success()
        );
    }
    let workspace = Workspace::new(root.path()).unwrap();
    let mut store = Store::open(home.path()).unwrap();
    let settings = PipelineSettings {
        code_index_semantic: false,
        ..Default::default()
    };
    let result: Value = serde_json::from_str(
        &search(
            &mut store,
            "history",
            &workspace,
            None,
            &settings,
            "shield drop",
            Some(5),
        )
        .await
        .unwrap(),
    )
    .unwrap();
    assert_eq!(result["results"][0]["path"], "src/tuning.rs");
    assert_eq!(result["results"][0]["ranking"]["history_rank"], 1);
    assert_eq!(
        result["history"]["matches"][0]["freshness"],
        "historical_lead_only; current source must be read and verified"
    );
    assert_eq!(
        result["results"][0]["freshness"],
        "validated_against_current_workspace"
    );
}

#[tokio::test]
async fn exact_symbol_graph_expands_multiple_bounded_hops() {
    let home = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    std::fs::write(
        root.path().join("flow.rs"),
        "pub fn entry() { middle(); }\n\npub fn middle() { target(); }\n\npub fn target() {}\n",
    )
    .unwrap();
    let workspace = Workspace::new(root.path()).unwrap();
    let mut store = Store::open(home.path()).unwrap();
    let settings = PipelineSettings {
        code_index_semantic: false,
        code_history: false,
        code_index_graph_hops: 2,
        ..Default::default()
    };
    let result: Value = serde_json::from_str(
        &search(
            &mut store,
            "graph",
            &workspace,
            None,
            &settings,
            "entry",
            Some(10),
        )
        .await
        .unwrap(),
    )
    .unwrap();
    let middle = result["results"]
        .as_array()
        .unwrap()
        .iter()
        .find(|candidate| {
            candidate["symbols"]
                .as_array()
                .is_some_and(|symbols| symbols.contains(&json!("middle")))
        })
        .unwrap();
    assert_eq!(middle["ranking"]["graph_hop"], 2);
    assert_eq!(
        result["retrieval"]["graph"],
        "bounded_exact_symbol_reference_graph"
    );
}

#[test]
fn automatic_packet_uses_published_fresh_source_and_skips_raced_changes_read_only() {
    let home = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    std::fs::write(
        root.path().join("arena.rs"),
        "fn shield_drop_rate() -> f32 { 0.025 }\n",
    )
    .unwrap();
    let workspace = Workspace::new(root.path()).unwrap();
    let mut store = Store::open(home.path()).unwrap();
    let settings = PipelineSettings::default();
    refresh(&mut store, &workspace, &settings).unwrap();
    let session = store
        .create("arena", "local", root.path(), "system")
        .unwrap();
    store
        .append(
            &session,
            &Message::text(Role::User, "Please decrease the arena shield drop rate"),
        )
        .unwrap();
    let first = packet(&store, &session, &workspace, &settings)
        .unwrap()
        .unwrap()
        .content
        .unwrap();
    assert!(first.contains("shield_drop_rate"));
    std::fs::write(root.path().join("arena.rs"), "fn boss_collision() {}\n").unwrap();
    // A publishing writer holds the checkout lock and the SQLite writer.
    // The foreground packet must neither wait on nor fail behind it.
    let writer = store
        .try_code_index_lock(workspace.root().to_str().unwrap())
        .unwrap()
        .unwrap();
    let blocker = rusqlite::Connection::open(
        home.path()
            .join("code-index")
            .read_dir()
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .find(|path| path.extension().is_some_and(|ext| ext == "sqlite3"))
            .unwrap(),
    )
    .unwrap();
    blocker.execute_batch("BEGIN IMMEDIATE;").unwrap();
    let started = std::time::Instant::now();
    assert!(
        packet(&store, &session, &workspace, &settings)
            .unwrap()
            .is_none()
    );
    assert!(started.elapsed() < std::time::Duration::from_millis(200));
    blocker.execute_batch("ROLLBACK;").unwrap();
    drop(writer);
    assert_eq!(
        store
            .code_index_status(workspace.root().to_str().unwrap())
            .unwrap()
            .unwrap()
            .status,
        "ready"
    );
}

#[test]
fn natural_language_terms_drop_fillers_and_expand_identifiers() {
    let terms = query_terms("Can you fix shieldDropRate in arena_mode?");
    assert!(terms.contains(&"shield".into()));
    assert!(terms.contains(&"drop".into()));
    assert!(terms.contains(&"rate".into()));
    assert!(terms.contains(&"arena".into()));
    assert!(terms.contains(&"mode".into()));
    assert!(!terms.contains(&"you".into()));
}

#[test]
fn expanded_identifiers_stay_within_the_retrieval_term_bound() {
    let terms = query_terms("aa_bb_cc_dd_ee_ff_gg_hh_ii_jj_kk_ll_mm_nn_oo_pp_qq_rr_ss_tt");
    assert!(terms.len() <= 16);
}

#[tokio::test]
#[ignore = "Needs the explicitly installed local model; performs no download or remote request"]
async fn local_model_indexes_code_and_serves_hybrid_results_offline() {
    let model = std::env::var_os("BUILDER_LOCAL_MODEL_DIR")
        .map(std::path::PathBuf::from)
        .expect("Set BUILDER_LOCAL_MODEL_DIR to the installed pinned model");
    let home = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    std::fs::write(
        root.path().join("arena.rs"),
        "pub fn shield_drop_rate(mode: GameMode) -> f32 { if mode.is_arena() { 0.025 } else { 0.2 } }\n",
    )
    .unwrap();
    let workspace = Workspace::new(root.path()).unwrap();
    let mut store = Store::open(home.path()).unwrap();
    let mut config = builder_core::config::Config::default();
    config.memory.enabled = true;
    config.memory.embedding_backend = builder_core::config::EmbeddingBackend::Local;
    config.memory.local_model_dir = Some(model);
    let memory = MemoryRuntime::from_config(&config).unwrap().unwrap();
    let settings = PipelineSettings {
        code_index_min_similarity_percent: 0,
        ..Default::default()
    };
    maintain(
        &mut store,
        &workspace,
        Some(memory.embeddings()),
        &settings,
        None,
    )
    .await
    .unwrap();
    let result: Value = serde_json::from_str(
        &search(
            &mut store,
            "session",
            &workspace,
            Some(memory.embeddings()),
            &settings,
            "reduce defensive powerups in competitive play",
            None,
        )
        .await
        .unwrap(),
    )
    .unwrap();
    assert_eq!(result["retrieval"]["semantic"], true);
    assert_eq!(result["retrieval"]["semantic_coverage"]["indexed"], 1);
    assert_eq!(result["results"][0]["path"], "arena.rs");
}
