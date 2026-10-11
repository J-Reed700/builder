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
