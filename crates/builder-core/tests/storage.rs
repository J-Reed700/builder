use builder_core::store::{Store, ToolRunState};

#[test]
fn tool_completion_is_idempotent_and_requires_a_claim() {
    let home = tempfile::tempdir().unwrap();
    let mut store = Store::open(home.path()).unwrap();
    let id = store
        .create("atomic", "local", home.path(), "system")
        .unwrap();
    assert_eq!(
        store.tool_run_state(&id, "call").unwrap(),
        ToolRunState::Unclaimed
    );
    assert!(store.complete_tool(&id, "missing", "invalid").is_err());
    assert!(store.claim_tool(&id, "call").unwrap());
    assert_eq!(
        store.tool_run_state(&id, "call").unwrap(),
        ToolRunState::Started
    );
    store.complete_tool(&id, "call", "first result").unwrap();
    assert_eq!(
        store.tool_run_state(&id, "call").unwrap(),
        ToolRunState::Finished
    );
    store
        .complete_tool(&id, "call", "duplicate result")
        .unwrap();
    assert_eq!(store.messages(&id).unwrap().len(), 2);
    assert_eq!(
        store.tool_result(&id, "call").unwrap().as_deref(),
        Some("first result")
    );
}

#[test]
fn missing_finished_result_repairs_to_typed_uncertainty() {
    use builder_core::protocol::{Message, Role};
    use builder_core::store::ToolOutcome;
    let home = tempfile::tempdir().unwrap();
    let mut store = Store::open(home.path()).unwrap();
    let id = store
        .create("repair", "local", home.path(), "system")
        .unwrap();
    store
        .append(&id, &Message::text(Role::User, "run command"))
        .unwrap();
    assert!(store.claim_tool(&id, "missing_result").unwrap());
    store
        .complete_tool_with_outcome(
            &id,
            "missing_result",
            "temporary result",
            ToolOutcome::Succeeded,
        )
        .unwrap();
    let conn = rusqlite::Connection::open(home.path().join("builder.sqlite3")).unwrap();
    conn.execute(
        "UPDATE tool_runs SET result=NULL,outcome=NULL WHERE session_id=?1 AND call_id=?2",
        rusqlite::params![id, "missing_result"],
    )
    .unwrap();
    drop(conn);

    assert!(
        store
            .restore_finished_tool_message(&id, "missing_result")
            .unwrap()
    );
    assert_eq!(
        store.tool_outcomes(&id).unwrap()["missing_result"],
        ToolOutcome::Uncertain
    );
    assert!(
        store
            .messages(&id)
            .unwrap()
            .last()
            .unwrap()
            .content
            .as_deref()
            .unwrap()
            .contains("Execution uncertain")
    );
}

#[test]
fn future_database_schema_is_rejected_without_downgrading() {
    let home = tempfile::tempdir().unwrap();
    let conn = rusqlite::Connection::open(home.path().join("builder.sqlite3")).unwrap();
    conn.execute_batch("PRAGMA user_version=999;").unwrap();
    assert!(Store::open(home.path()).is_err());
    let version: u32 = conn
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .unwrap();
    assert_eq!(version, 999);
}

#[cfg(unix)]
#[test]
fn database_is_private_without_changing_existing_directory_permissions() {
    use std::os::unix::fs::PermissionsExt;
    let home = tempfile::tempdir().unwrap();
    std::fs::set_permissions(home.path(), std::fs::Permissions::from_mode(0o755)).unwrap();
    let _store = Store::open(home.path()).unwrap();
    assert_eq!(
        std::fs::metadata(home.path()).unwrap().permissions().mode() & 0o777,
        0o755
    );
    assert_eq!(
        std::fs::metadata(home.path().join("builder.sqlite3"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
    assert_eq!(
        std::fs::metadata(home.path().join("builder-index.sqlite3"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
}

#[test]
fn bulk_index_writer_cannot_block_conversation_persistence() {
    use builder_core::{
        code_index::{CodeChunk, CodeIndexFile, CodeIndexSnapshot},
        protocol::{Message, Role},
    };

    let home = tempfile::tempdir().unwrap();
    let mut store = Store::open(home.path()).unwrap();
    let session = store
        .create("independent journal", "local", home.path(), "system")
        .unwrap();
    let snapshot = CodeIndexSnapshot {
        snapshot_hash: "snapshot".into(),
        files: vec![CodeIndexFile {
            path: "main.rs".into(),
            hash: "file".into(),
            source_bytes: 12,
            language: "rust".into(),
        }],
        chunks: vec![CodeChunk {
            id: "chunk".into(),
            path: "main.rs".into(),
            file_hash: "file".into(),
            content_hash: "content".into(),
            language: "rust".into(),
            kind: "declaration".into(),
            start_line: 1,
            end_line: 1,
            symbols: vec!["main".into()],
            references: vec![],
            content: "fn main() {}".into(),
        }],
        source_bytes: 12,
        skipped: 0,
    };
    store.code_index_replace("repo", &snapshot).unwrap();

    let main = rusqlite::Connection::open(home.path().join("builder.sqlite3")).unwrap();
    let legacy_tables: usize = main
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE name='code_index_state'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(legacy_tables, 0);

    let index = rusqlite::Connection::open(home.path().join("builder-index.sqlite3")).unwrap();
    assert_eq!(
        index
            .query_row("SELECT COUNT(*) FROM code_index_chunks", [], |row| {
                row.get::<_, usize>(0)
            })
            .unwrap(),
        1
    );
    index.execute_batch("BEGIN IMMEDIATE;").unwrap();

    let started = std::time::Instant::now();
    let mut concurrent = Store::open(home.path()).unwrap();
    concurrent
        .append(&session, &Message::text(Role::User, "save while indexing"))
        .unwrap();
    assert!(started.elapsed() < std::time::Duration::from_secs(1));
    assert_eq!(concurrent.messages(&session).unwrap().len(), 2);
    index.execute_batch("ROLLBACK;").unwrap();
}

#[test]
fn opening_a_current_journal_does_not_request_the_writer_lock() {
    let home = tempfile::tempdir().unwrap();
    let store = Store::open(home.path()).unwrap();
    drop(store);

    let writer = rusqlite::Connection::open(home.path().join("builder.sqlite3")).unwrap();
    writer.execute_batch("BEGIN IMMEDIATE;").unwrap();

    let started = std::time::Instant::now();
    let concurrent = Store::open(home.path()).unwrap();
    assert!(started.elapsed() < std::time::Duration::from_secs(1));
    assert!(concurrent.sessions().unwrap().is_empty());
    writer.execute_batch("ROLLBACK;").unwrap();
}

#[test]
fn read_then_write_transaction_waits_before_taking_a_snapshot() {
    use builder_core::store::ToolOutcome;

    let home = tempfile::tempdir().unwrap();
    let mut store = Store::open(home.path()).unwrap();
    let session = store
        .create("writer contention", "local", home.path(), "system")
        .unwrap();
    assert!(store.claim_tool(&session, "call").unwrap());

    let path = home.path().join("builder.sqlite3");
    let (locked_tx, locked_rx) = std::sync::mpsc::channel();
    let writer = std::thread::spawn(move || {
        let connection = rusqlite::Connection::open(path).unwrap();
        connection.execute_batch("BEGIN IMMEDIATE;").unwrap();
        locked_tx.send(()).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(100));
        connection.execute_batch("ROLLBACK;").unwrap();
    });
    locked_rx.recv().unwrap();

    store
        .complete_tool_with_outcome(&session, "call", "finished", ToolOutcome::Succeeded)
        .unwrap();
    writer.join().unwrap();
    assert_eq!(
        store.tool_result(&session, "call").unwrap().unwrap(),
        "finished"
    );
}

#[test]
fn broken_derived_index_does_not_make_the_journal_unavailable() {
    use builder_core::protocol::{Message, Role};

    let home = tempfile::tempdir().unwrap();
    drop(Store::open(home.path()).unwrap());
    std::fs::write(home.path().join("builder-index.sqlite3"), b"not sqlite").unwrap();

    let mut store = Store::open(home.path()).unwrap();
    let session = store
        .create("degraded index", "local", home.path(), "system")
        .unwrap();
    store
        .append(
            &session,
            &Message::text(Role::User, "the journal still works"),
        )
        .unwrap();
    assert_eq!(store.messages(&session).unwrap().len(), 2);
    assert!(store.code_index_status("repo").is_err());
}

#[test]
fn code_index_maintenance_lock_is_scoped_and_raii_released() {
    let home = tempfile::tempdir().unwrap();
    let first = Store::open(home.path()).unwrap();
    let second = Store::open(home.path()).unwrap();

    let guard = first.try_code_index_lock("repo-a").unwrap().unwrap();
    assert!(second.try_code_index_lock("repo-a").unwrap().is_none());
    assert!(second.try_code_index_lock("repo-b").unwrap().is_some());
    drop(guard);
    assert!(second.try_code_index_lock("repo-a").unwrap().is_some());
}

#[test]
fn v11_upgrade_preserves_journal_and_archives_legacy_index_in_place() {
    let home = tempfile::tempdir().unwrap();
    let mut store = Store::open(home.path()).unwrap();
    let session = store
        .create("schema twelve", "local", home.path(), "system")
        .unwrap();
    drop(store);
    std::fs::remove_file(home.path().join("builder-index.sqlite3")).unwrap();

    let main = rusqlite::Connection::open(home.path().join("builder.sqlite3")).unwrap();
    main.execute_batch(
        "CREATE TABLE code_index_state (
         scope TEXT PRIMARY KEY, generation INTEGER NOT NULL, snapshot_hash TEXT NOT NULL,
         status TEXT NOT NULL, completed_at TEXT NOT NULL, files INTEGER NOT NULL,
         chunks INTEGER NOT NULL, source_bytes INTEGER NOT NULL, skipped INTEGER NOT NULL);
         INSERT INTO code_index_state VALUES ('legacy',1,'old','ready','before',1,1,1,0);
         PRAGMA user_version=11;",
    )
    .unwrap();
    drop(main);

    let store = Store::open(home.path()).unwrap();
    assert_eq!(store.session(&session).unwrap().title, "schema twelve");
    assert!(store.code_index_status("legacy").unwrap().is_none());
    let main = rusqlite::Connection::open(home.path().join("builder.sqlite3")).unwrap();
    assert_eq!(
        main.query_row("PRAGMA user_version", [], |row| row.get::<_, u32>(0))
            .unwrap(),
        13
    );
    assert_eq!(
        main.query_row("SELECT COUNT(*) FROM code_index_state", [], |row| {
            row.get::<_, usize>(0)
        })
        .unwrap(),
        1
    );
}

#[test]
fn v1_migration_preserves_messages_claims_and_session_identity() {
    let home = tempfile::tempdir().unwrap();
    let conn = rusqlite::Connection::open(home.path().join("builder.sqlite3")).unwrap();
    conn.execute_batch("CREATE TABLE sessions (
        id TEXT PRIMARY KEY,title TEXT NOT NULL,profile TEXT NOT NULL,
        workspace TEXT NOT NULL,updated_at TEXT NOT NULL);
        CREATE TABLE messages (seq INTEGER PRIMARY KEY AUTOINCREMENT,
        session_id TEXT NOT NULL REFERENCES sessions(id),body TEXT NOT NULL);
        INSERT INTO sessions VALUES ('old','Original','local','/tmp','before');
        INSERT INTO messages(session_id,body) VALUES ('old','{\"role\":\"user\",\"content\":\"keep me\"}');
        CREATE TABLE tool_runs (session_id TEXT NOT NULL,call_id TEXT NOT NULL,
        state TEXT NOT NULL,result TEXT,PRIMARY KEY(session_id,call_id));
        INSERT INTO tool_runs VALUES ('old','claimed','started',NULL);
        PRAGMA user_version=1;").unwrap();
    drop(conn);
    let store = Store::open(home.path()).unwrap();
    assert_eq!(
        store.messages("old").unwrap()[0].content.as_deref(),
        Some("keep me")
    );
    assert_eq!(store.sessions().unwrap()[0].title, "Original");
    assert!(!store.claim_tool("old", "claimed").unwrap());
    assert!(store.archived_messages("old").unwrap().is_empty());
    drop(store);
    assert!(Store::open(home.path()).is_ok());
}

#[test]
fn redirect_is_atomic_if_saving_the_new_instruction_fails() {
    use builder_core::protocol::{Message, Role};
    let home = tempfile::tempdir().unwrap();
    let mut store = Store::open(home.path()).unwrap();
    let id = store
        .create("atomic redirect", "local", home.path(), "system")
        .unwrap();
    store
        .append(&id, &Message::text(Role::User, "original"))
        .unwrap();
    let call: Message = serde_json::from_value(serde_json::json!({"role":"assistant",
        "tool_calls":[{"id":"queued","type":"function","function":{"name":"shell","arguments":"{}"}}]})).unwrap();
    store.append(&id, &call).unwrap();
    let conn = rusqlite::Connection::open(home.path().join("builder.sqlite3")).unwrap();
    conn.execute_batch(
        "CREATE TRIGGER fail_redirect BEFORE INSERT ON messages
        WHEN json_extract(NEW.body, '$.content')='replacement'
        BEGIN SELECT RAISE(ABORT, 'simulated storage failure'); END;",
    )
    .unwrap();
    assert!(store.interrupt_turn(&id, Some("replacement")).is_err());
    assert_eq!(store.messages(&id).unwrap().len(), 3);
    // The cancelled result/claim also rolled back.
    assert!(store.claim_tool(&id, "queued").unwrap());
}

#[test]
fn interrupt_records_claimed_uncertainty_as_a_typed_atomic_outcome() {
    use builder_core::protocol::{Message, Role};
    use builder_core::store::ToolOutcome;
    let home = tempfile::tempdir().unwrap();
    let mut store = Store::open(home.path()).unwrap();
    let id = store
        .create("typed interruption", "local", home.path(), "system")
        .unwrap();
    store
        .append(&id, &Message::text(Role::User, "run commands"))
        .unwrap();
    let calls: Message = serde_json::from_value(serde_json::json!({
        "role":"assistant",
        "tool_calls":[
            {"id":"started","type":"function","function":{"name":"shell","arguments":"{\"command\":\"one\"}"}},
            {"id":"queued","type":"function","function":{"name":"shell","arguments":"{\"command\":\"two\"}"}}
        ]
    }))
    .unwrap();
    store.append(&id, &calls).unwrap();
    assert!(store.claim_tool(&id, "started").unwrap());
    assert_eq!(store.interrupt_turn(&id, None).unwrap(), 1);
    let outcomes = store.tool_outcomes(&id).unwrap();
    assert_eq!(outcomes["started"], ToolOutcome::Uncertain);
    assert!(!outcomes.contains_key("queued"));
    assert_eq!(
        store.tool_run_state(&id, "started").unwrap(),
        ToolRunState::Finished
    );
    assert_eq!(
        store.tool_run_state(&id, "queued").unwrap(),
        ToolRunState::Finished
    );
}

#[test]
fn rewind_preserves_originals_and_draft_across_restart() {
    use builder_core::protocol::{Message, Role};
    let home = tempfile::tempdir().unwrap();
    let mut store = Store::open(home.path()).unwrap();
    let id = store
        .create("rewind", "local", home.path(), "system")
        .unwrap();
    store
        .append(&id, &Message::text(Role::User, "first task"))
        .unwrap();
    store
        .append(&id, &Message::text(Role::Assistant, "first answer"))
        .unwrap();
    let prompt = "  revise this 🦀\r\nkeep whitespace\n";
    store
        .append(&id, &Message::text(Role::User, prompt))
        .unwrap();
    store
        .append(&id, &Message::text(Role::Assistant, "answer to discard"))
        .unwrap();
    assert_eq!(store.rewind(&id).unwrap(), (prompt.into(), 0));
    drop(store);
    let mut store = Store::open(home.path()).unwrap();
    assert_eq!(store.composer_draft(&id).unwrap().as_deref(), Some(prompt));
    assert_eq!(store.messages(&id).unwrap().len(), 3);
    let archive = store.archived_messages(&id).unwrap();
    assert_eq!(archive.len(), 2);
    assert_eq!(archive[0].content.as_deref(), Some(prompt));
    assert_eq!(archive[1].content.as_deref(), Some("answer to discard"));
    store.interrupt_turn(&id, Some("replacement")).unwrap();
    assert!(store.composer_draft(&id).unwrap().is_none());
    assert_eq!(store.archived_messages(&id).unwrap().len(), 2);
    assert_eq!(store.rewind(&id).unwrap().0, "replacement");
    assert_eq!(store.rewind(&id).unwrap().0, "first task");
    assert!(store.rewind(&id).is_err());
}

#[test]
fn v2_upgrade_preserves_rewind_draft_and_originals_and_checkpoints_reject_stale_context() {
    use builder_core::protocol::{Message, Role};
    let home = tempfile::tempdir().unwrap();
    let mut store = Store::open(home.path()).unwrap();
    let id = store
        .create("upgrade", "local", home.path(), "system")
        .unwrap();
    store
        .append(&id, &Message::text(Role::User, "original"))
        .unwrap();
    store.rewind(&id).unwrap();
    drop(store);
    // v2 had active message flags and drafts, but no context checkpoints.
    let conn = rusqlite::Connection::open(home.path().join("builder.sqlite3")).unwrap();
    conn.execute_batch("DROP TABLE context_checkpoints; ALTER TABLE tool_runs DROP COLUMN outcome; PRAGMA user_version=2;")
        .unwrap();
    drop(conn);
    let mut store = Store::open(home.path()).unwrap();
    assert_eq!(
        store.composer_draft(&id).unwrap().as_deref(),
        Some("original")
    );
    assert_eq!(
        store.archived_messages(&id).unwrap()[0].content.as_deref(),
        Some("original")
    );
    let stale = store.messages(&id).unwrap();
    store.interrupt_turn(&id, Some("replacement")).unwrap();
    let current = store.messages(&id).unwrap();
    assert!(store.checkpoint(&id, &stale, &stale).is_err());
    assert_eq!(store.messages(&id).unwrap(), current);
}

#[test]
fn tool_outcomes_are_durable_atomic_and_independent_of_display_text() {
    use builder_core::store::ToolOutcome;
    let home = tempfile::tempdir().unwrap();
    let mut store = Store::open(home.path()).unwrap();
    let session = store
        .create("typed outcomes", "local", home.path(), "system")
        .unwrap();
    for (id, text, outcome) in [
        ("failed", "All good", ToolOutcome::Failed),
        (
            "source",
            "ERROR: literal source content",
            ToolOutcome::Succeeded,
        ),
        ("edit", "DENIED: literal filename", ToolOutcome::Changed),
    ] {
        store.claim_tool(&session, id).unwrap();
        store
            .complete_tool_with_outcome(&session, id, text, outcome)
            .unwrap();
        // A retry cannot replace either half of the durable result.
        store
            .complete_tool_with_outcome(&session, id, "replacement", ToolOutcome::Unknown)
            .unwrap();
        assert_eq!(
            store.tool_result(&session, id).unwrap().as_deref(),
            Some(text)
        );
        assert_eq!(
            store.tool_outcomes(&session).unwrap().get(id),
            Some(&outcome)
        );
    }
    store.claim_tool(&session, "legacy").unwrap();
    store
        .complete_tool(&session, "legacy", "ERROR: historical output")
        .unwrap();
    drop(store);
    let store = Store::open(home.path()).unwrap();
    let outcomes = store.tool_outcomes(&session).unwrap();
    assert_eq!(outcomes["failed"], ToolOutcome::Failed);
    assert_eq!(outcomes["source"], ToolOutcome::Succeeded);
    assert_eq!(outcomes["edit"], ToolOutcome::Changed);
    assert_eq!(outcomes["legacy"], ToolOutcome::Unknown);
    assert_eq!(store.messages(&session).unwrap().len(), 5);
}

#[test]
fn chat_metadata_migration_preserves_transcript_claims_and_archive_state() {
    use builder_core::protocol::{Message, Role};
    let home = tempfile::tempdir().unwrap();
    let mut store = Store::open(home.path()).unwrap();
    let id = store
        .create("Existing chat", "local", home.path(), "system")
        .unwrap();
    store
        .append(&id, &Message::text(Role::User, "Original instruction"))
        .unwrap();
    assert!(store.claim_tool(&id, "uncertain").unwrap());
    drop(store);
    let conn = rusqlite::Connection::open(home.path().join("builder.sqlite3")).unwrap();
    conn.execute_batch("DROP TABLE chat_metadata; PRAGMA user_version=5;")
        .unwrap();
    drop(conn);
    let store = Store::open(home.path()).unwrap();
    assert!(!store.chat_archived(&id).unwrap());
    assert_eq!(store.history_messages(&id).unwrap().len(), 2);
    assert_eq!(
        store.tool_run_state(&id, "uncertain").unwrap(),
        ToolRunState::Started
    );
    store.archive_chat(&id, true).unwrap();
    store.rename_chat(&id, "Renamed").unwrap();
    drop(store);
    let store = Store::open(home.path()).unwrap();
    assert!(store.chat_archived(&id).unwrap());
    assert_eq!(store.session(&id).unwrap().title, "Renamed");
    assert_eq!(store.history_messages(&id).unwrap().len(), 2);
    assert_eq!(
        store
            .chat_sessions_page(home.path(), 0, true, "named")
            .unwrap()
            .len(),
        1
    );
    assert!(
        store
            .chat_sessions_page(home.path(), 0, false, "")
            .unwrap()
            .is_empty()
    );
}

#[test]
fn provider_conformance_is_bounded_and_replaced_by_fingerprint() {
    let home = tempfile::tempdir().unwrap();
    let mut store = Store::open(home.path()).unwrap();
    store
        .save_provider_conformance(
            "fingerprint",
            "local",
            "http://localhost:11434/v1",
            "model",
            &serde_json::json!({"normal_generation":"passed"}),
        )
        .unwrap();
    store
        .save_provider_conformance(
            "fingerprint",
            "local",
            "http://localhost:11434/v1",
            "model",
            &serde_json::json!({"normal_generation":"failed"}),
        )
        .unwrap();
    let connection = rusqlite::Connection::open(home.path().join("builder.sqlite3")).unwrap();
    let (count, report): (usize, String) = connection
        .query_row(
            "SELECT COUNT(*),MAX(report) FROM provider_conformance WHERE fingerprint='fingerprint'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(count, 1);
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&report).unwrap()["normal_generation"],
        "failed"
    );
}

#[test]
fn v12_upgrade_adds_subagent_links_and_side_effect_free_claims() {
    let home = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let mut store = Store::open(home.path()).unwrap();
    let parent = store
        .create("parent", "p", workspace.path(), "system")
        .unwrap();
    assert!(store.claim_tool(&parent, "legacy").unwrap());
    drop(store);
    let conn = rusqlite::Connection::open(home.path().join("builder.sqlite3")).unwrap();
    conn.execute_batch(
        "DROP TABLE subagent_sessions; ALTER TABLE tool_runs DROP COLUMN side_effect_free; PRAGMA user_version=12;",
    )
    .unwrap();
    drop(conn);

    let mut store = Store::open(home.path()).unwrap();
    // A legacy claim is conservatively not side-effect free.
    assert!(
        !store
            .close_interrupted_side_effect_free_tool(&parent, "legacy")
            .unwrap()
    );
    let child = store
        .create_subagent(&parent, "call", "subagent · look", "child system")
        .unwrap();
    assert!(store.is_subagent(&child).unwrap());
    assert!(!store.is_subagent(&parent).unwrap());
    assert_eq!(
        store
            .sessions()
            .unwrap()
            .into_iter()
            .map(|session| session.id)
            .collect::<Vec<_>>(),
        std::slice::from_ref(&parent)
    );
    assert_eq!(store.resolve(&child[..8]).unwrap().profile, "p");
    assert!(
        store
            .create_subagent(&parent, "call", "again", "child system")
            .is_err(),
        "one call starts at most one subagent"
    );
    assert!(
        store
            .create_subagent(&child, "nested", "deeper", "child system")
            .unwrap_err()
            .to_string()
            .contains("cannot start another subagent")
    );
}

#[test]
fn clear_archives_the_conversation_and_deactivates_checkpoints() {
    use builder_core::protocol::{Message, Role};
    let home = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let mut store = Store::open(home.path()).unwrap();
    let id = store
        .create("clear", "p", workspace.path(), "system")
        .unwrap();
    store
        .append(&id, &Message::text(Role::User, "first task"))
        .unwrap();
    store
        .append(&id, &Message::text(Role::Assistant, "first answer"))
        .unwrap();
    store
        .append(&id, &Message::text(Role::User, "second task"))
        .unwrap();
    // A pending turn is closed before the archive so no claim stays started.
    assert_eq!(store.close_pending_turn(&id).unwrap(), 0);
    assert_eq!(store.clear(&id).unwrap(), 4);
    let messages = store.messages(&id).unwrap();
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0].role, Role::System);
    let archived = store.archived_messages(&id).unwrap();
    assert!(
        archived
            .iter()
            .any(|m| m.content.as_deref() == Some("first task"))
    );
    assert!(
        archived
            .iter()
            .any(|m| m.content.as_deref() == Some("first answer"))
    );
    // A cleared session starts fresh, and checkpoints no longer project context.
    store
        .append(&id, &Message::text(Role::User, "fresh start"))
        .unwrap();
    let current = store.messages(&id).unwrap();
    store.checkpoint(&id, &current, &current).unwrap();
    store
        .append(&id, &Message::text(Role::User, "after checkpoint"))
        .unwrap();
    assert_eq!(store.messages(&id).unwrap().len(), 3);
    assert_eq!(store.clear(&id).unwrap(), 2);
    let messages = store.messages(&id).unwrap();
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0].role, Role::System);
}
