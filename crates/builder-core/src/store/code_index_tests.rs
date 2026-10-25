use super::*;
use crate::code_index::{CodeIndexFile, CodeQuerySource};

fn snapshot(text: &str) -> CodeIndexSnapshot {
    let hash = crate::memory::digest(text.as_bytes());
    CodeIndexSnapshot {
        snapshot_hash: hash.clone(),
        files: vec![CodeIndexFile {
            path: "arena.rs".into(),
            hash: hash.clone(),
            source_bytes: text.len(),
            language: "rust".into(),
        }],
        chunks: vec![CodeChunk {
            id: hash.clone(),
            path: "arena.rs".into(),
            file_hash: hash.clone(),
            content_hash: hash,
            language: "rust".into(),
            kind: "declaration".into(),
            start_line: 1,
            end_line: 1,
            symbols: vec!["shield_drop".into()],
            references: vec!["shield".into(), "drop".into()],
            content: text.into(),
        }],
        source_bytes: text.len(),
        skipped: 0,
    }
}

#[test]
fn complete_generation_is_searchable_and_failed_replacement_preserves_it() {
    let home = tempfile::tempdir().unwrap();
    let mut store = Store::open(home.path()).unwrap();
    let writer = store.try_code_index_lock("repo").unwrap().unwrap();
    let valid = snapshot("fn shield_drop() {}");
    assert!(store.code_index_replace(&writer, &valid).unwrap());
    assert_eq!(
        store.code_index_lexical("repo", "\"shield\"", 10).unwrap()[0]
            .0
            .path,
        "arena.rs"
    );
    let exact = store
        .code_index_symbol_chunks("repo", &["shield_drop".into()], 10)
        .unwrap();
    assert_eq!(exact[0].1, "shield_drop");
    assert_eq!(exact[0].2, "declaration");
    let mut invalid = snapshot("fn boss_collision() {}");
    invalid.files.clear();
    assert!(store.code_index_replace(&writer, &invalid).is_err());
    assert_eq!(
        store.code_index_status("repo").unwrap().unwrap().generation,
        1
    );
    assert_eq!(
        store.code_index_lexical("repo", "\"shield\"", 10).unwrap()[0]
            .0
            .content,
        "fn shield_drop() {}"
    );
    store
        .code_index_record_failure(&writer, "bounded scan failed\nsecret second line")
        .unwrap();
    let state = store.code_index_status("repo").unwrap().unwrap();
    assert_eq!(state.generation, 1);
    assert!(state.status.starts_with("refresh_failed:"));
    assert!(!state.status.contains('\n'));
}

#[test]
fn query_telemetry_is_bounded_and_aggregated_without_source_text() {
    let home = tempfile::tempdir().unwrap();
    let mut store = Store::open(home.path()).unwrap();
    let writer = store.try_code_index_lock("repo").unwrap().unwrap();
    let id = store
        .code_index_record_query(
            &writer,
            &CodeQueryTelemetry {
                session: "session".into(),
                source: CodeQuerySource::Tool,
                query: "shield drop".into(),
                elapsed_ms: 42,
                candidates: 5,
                returned: 0,
                stale_suppressed: 1,
                semantic: true,
                coverage: Some((4, 10)),
                result_paths: vec![],
            },
        )
        .unwrap();
    assert!(id > 0);
    assert_eq!(
        store.code_index_query_summary("repo").unwrap(),
        CodeQuerySummary {
            queries: 1,
            abstentions: 1,
            stale_suppressions: 1,
            average_elapsed_ms: 42,
            last_query_at: store
                .code_index_query_summary("repo")
                .unwrap()
                .last_query_at,
        }
    );
}

#[test]
fn history_replacement_is_atomic_searchable_and_content_addressed() {
    let home = tempfile::tempdir().unwrap();
    let mut store = Store::open(home.path()).unwrap();
    let writer = store.try_code_index_lock("repo").unwrap().unwrap();
    let snapshot = CodeHistorySnapshot {
        head: "abc".into(),
        snapshot_hash: "history-one".into(),
        entries: vec![CodeHistoryEntry {
            revision: "abc".into(),
            unix_time: 100,
            subject: "Reduce arena shield drops".into(),
            paths: vec!["src/arena.rs".into()],
        }],
    };
    assert!(store.code_history_replace(&writer, &snapshot).unwrap());
    assert!(!store.code_history_replace(&writer, &snapshot).unwrap());
    let hits = store
        .code_history_lexical("repo", "\"shield\"", 10)
        .unwrap();
    assert_eq!(hits[0].0.paths, ["src/arena.rs"]);
    let invalid = CodeHistorySnapshot {
        entries: vec![CodeHistoryEntry {
            revision: String::new(),
            ..snapshot.entries[0].clone()
        }],
        snapshot_hash: "history-two".into(),
        ..snapshot
    };
    assert!(store.code_history_replace(&writer, &invalid).is_err());
    assert_eq!(
        store
            .code_history_lexical("repo", "\"shield\"", 10)
            .unwrap()
            .len(),
        1
    );
}

#[test]
fn vectors_are_tracked_per_fingerprint_and_removed_paths_mark_the_index_stale() {
    let home = tempfile::tempdir().unwrap();
    let mut store = Store::open(home.path()).unwrap();
    let writer = store.try_code_index_lock("repo").unwrap().unwrap();
    let valid = snapshot("fn shield_drop() {}");
    assert!(store.code_index_replace(&writer, &valid).unwrap());
    let hash = valid.chunks[0].content_hash.clone();

    assert_eq!(
        store.code_index_vector_coverage("repo", "fp-a").unwrap(),
        (0, 1)
    );
    let pending = store
        .code_index_pending_vectors("repo", "fp-a", 10)
        .unwrap();
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].content_hash, hash);
    assert!(
        store
            .code_index_dense("repo", "fp-a", 10)
            .unwrap()
            .is_empty()
    );

    assert!(
        store.code_index_set_vectors(&writer, "fp-a", &[]).is_err(),
        "an empty batch is a bug, not a no-op"
    );
    assert!(
        store
            .code_index_set_vectors(&writer, "fp-a", &[("missing".into(), vec![0.5, 0.5])])
            .is_err(),
        "a vector for a chunk that is gone is refused"
    );
    assert!(
        store
            .code_index_set_vectors(&writer, "fp-a", &[(hash.clone(), vec![f32::NAN, 0.5])])
            .is_err(),
        "non-finite vectors are refused"
    );
    assert!(
        store
            .code_index_set_vectors(&writer, "fp-a", &[(hash.clone(), vec![])])
            .is_err(),
        "empty vectors are refused"
    );
    assert_eq!(
        store.code_index_vector_coverage("repo", "fp-a").unwrap(),
        (0, 1),
        "a refused batch writes nothing"
    );

    store
        .code_index_set_vectors(&writer, "fp-a", &[(hash.clone(), vec![0.25, -0.5, 1.0])])
        .unwrap();
    assert_eq!(
        store.code_index_vector_coverage("repo", "fp-a").unwrap(),
        (1, 1)
    );
    assert_eq!(
        store.code_index_vector_coverage("repo", "fp-b").unwrap(),
        (0, 1),
        "another embedding model has its own vectors"
    );
    assert!(
        store
            .code_index_pending_vectors("repo", "fp-a", 10)
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        store
            .code_index_pending_vectors("repo", "fp-b", 10)
            .unwrap()
            .len(),
        1
    );
    let dense = store.code_index_dense("repo", "fp-a", 10).unwrap();
    assert_eq!(dense.len(), 1);
    assert_eq!(dense[0].0.path, "arena.rs");
    assert_eq!(dense[0].1, vec![0.25, -0.5, 1.0]);
    store
        .code_index_set_vectors(&writer, "fp-a", &[(hash.clone(), vec![1.0, 0.0])])
        .unwrap();
    assert_eq!(
        store.code_index_dense("repo", "fp-a", 10).unwrap()[0].1,
        vec![1.0, 0.0],
        "re-embedding replaces the stored vector"
    );

    let chunks = store
        .code_index_chunks_for_paths("repo", &["arena.rs".into(), "missing.rs".into()], 4)
        .unwrap();
    assert_eq!(chunks.len(), 1);
    assert_eq!(chunks[0].path, "arena.rs");
    assert!(
        store
            .code_index_chunks_for_paths("repo", &vec!["x".to_owned(); 101], 1)
            .is_err()
    );
    assert!(
        store
            .code_index_symbol_chunks("repo", &vec!["x".to_owned(); 33], 1)
            .is_err()
    );

    store.code_index_remove_path(&writer, "arena.rs").unwrap();
    assert!(
        store
            .code_index_lexical("repo", "\"shield\"", 10)
            .unwrap()
            .is_empty()
    );
    assert!(
        store
            .code_index_symbol_chunks("repo", &["shield_drop".into()], 10)
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        store.code_index_vector_coverage("repo", "fp-a").unwrap(),
        (0, 0)
    );
    assert!(
        store
            .code_index_chunks_for_paths("repo", &["arena.rs".into()], 4)
            .unwrap()
            .is_empty()
    );
    assert!(
        store
            .code_index_dense("repo", "fp-a", 10)
            .unwrap()
            .is_empty()
    );
    let state = store.code_index_status("repo").unwrap().unwrap();
    assert_eq!(state.generation, 1);
    assert_eq!(state.status, "stale");
}
