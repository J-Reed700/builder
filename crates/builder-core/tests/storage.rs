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
