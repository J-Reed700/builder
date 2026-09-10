use builder_core::{
    memory::{Memory, MemoryKind, TaskState, cosine},
    protocol::{Message, Role},
    store::Store,
};

fn fixture() -> (tempfile::TempDir, Store, String, Memory) {
    let home = tempfile::tempdir().unwrap();
    let mut store = Store::open(home.path()).unwrap();
    let session = store
        .create("memory", "local", home.path(), "system")
        .unwrap();
    store
        .append(&session, &Message::text(Role::User, "Investigate rates"))
        .unwrap();
    let seq = store.memory_latest_seq(&session).unwrap();
    let memory = Memory {
        key: "rates".into(),
        revision: 0,
        kind: MemoryKind::Finding,
        text: "rates.json controls shield chance".into(),
        evidence: vec![],
        origin_session: session.clone(),
        origin_seq: seq,
        created_at: String::new(),
    };
    (home, store, session, memory)
}

#[test]
fn revisions_scope_conflicts_forgetting_and_restart() {
    let (home, mut store, _, memory) = fixture();
    let first = store.memory_put("checkout-a", 0, memory.clone()).unwrap();
    assert!(
        store
            .memory_get("checkout-b", "rates", None)
            .unwrap()
            .is_none()
    );
    let mut updated = memory.clone();
    updated.text = "new value".into();
    store.memory_put("checkout-a", 1, updated.clone()).unwrap();
    assert!(store.memory_put("checkout-a", 1, updated).is_err());
    assert_eq!(
        store
            .memory_get("checkout-a", "rates", Some(1))
            .unwrap()
            .unwrap()
            .text,
        first.text
    );
    assert!(
        store
            .memory_set_vector("checkout-a", "rates", 1, "model-a", &[1., 2.])
            .is_err()
    );
    store
        .memory_set_vector("checkout-a", "rates", 2, "model-a", &[1., 2.])
        .unwrap();
    assert!(
        store
            .memory_vector("checkout-a", "rates", 2, "model-b")
            .unwrap()
            .is_none()
    );
    drop(store);
    let mut store = Store::open(home.path()).unwrap();
    assert_eq!(
        store
            .memory_get("checkout-a", "rates", None)
            .unwrap()
            .unwrap()
            .revision,
        2
    );
    assert!(store.memory_forget("checkout-a", "rates", 1).is_err());
    store.memory_forget("checkout-a", "rates", 2).unwrap();
    assert!(
        store
            .memory_get("checkout-a", "rates", None)
            .unwrap()
            .is_none()
    );
    assert!(
        store
            .memory_vector("checkout-a", "rates", 2, "model-a")
            .unwrap()
            .is_none()
    );
    assert!(
        store
            .memory_keywords("checkout-a", "rates")
            .unwrap()
            .is_empty()
    );
    assert!(store.memory_put("checkout-a", 2, memory).is_err());
}

#[test]
fn task_and_evidence_origins_cannot_survive_rewind_as_current() {
    let (_, mut store, session, memory) = fixture();
    let task = TaskState {
        next_action: "inspect rate".into(),
        questions: vec![],
        source_seq: memory.origin_seq,
    };
    store.memory_save_task(&session, &task).unwrap();
    assert!(store.memory_task(&session).unwrap().is_some());
    store.rewind(&session).unwrap();
    assert!(store.memory_task(&session).unwrap().is_none());
    assert!(store.memory_put("checkout", 0, memory).is_err());
    assert!(store.memory_save_task(&session, &task).is_err());
}

#[test]
fn vectors_are_finite_dimension_checked_and_model_separated() {
    let (_, mut store, _, memory) = fixture();
    store.memory_put("a", 0, memory).unwrap();
    for bad in [
        vec![],
        vec![0., 0.],
        vec![f32::NAN],
        vec![f32::INFINITY],
        vec![1.; 8193],
    ] {
        assert!(store.memory_set_vector("a", "rates", 1, "m", &bad).is_err());
    }
    assert!(cosine(&[1., 0.], &[1.]).is_err());
    assert!((cosine(&[1., 0.], &[2., 0.]).unwrap() - 1.).abs() < 1e-6);
    store
        .memory_set_vector("a", "rates", 1, "m1", &[1., 2.])
        .unwrap();
    store
        .memory_set_vector("a", "rates", 1, "m2", &[1., 2., 3.])
        .unwrap();
    assert_eq!(
        store
            .memory_vector("a", "rates", 1, "m1")
            .unwrap()
            .unwrap()
            .len(),
        2
    );
}

#[test]
fn keyword_queries_are_literal_and_preferences_are_explicitly_scoped() {
    let (_, mut store, _, mut memory) = fixture();
    store.memory_put("a", 0, memory.clone()).unwrap();
    assert_eq!(
        store
            .memory_keywords("a", "\"rates\" OR shield --")
            .unwrap(),
        vec!["rates"]
    );
    assert!(store.memory_keywords("b", "rates").unwrap().is_empty());
    memory.kind = MemoryKind::Preference;
    memory.origin_session.clear();
    memory.origin_seq = 0;
    assert!(store.memory_put("a", 1, memory.clone()).is_err());
    store.memory_put("@user", 0, memory).unwrap();
    assert_eq!(store.memory_list("@user").unwrap().len(), 1);
}

#[test]
fn concurrent_connections_cannot_overwrite_newer_revision() {
    let (home, mut first, _, memory) = fixture();
    first.memory_put("a", 0, memory.clone()).unwrap();
    let mut second = Store::open(home.path()).unwrap();
    second.memory_put("a", 1, memory.clone()).unwrap();
    assert!(first.memory_put("a", 1, memory).is_err());
    assert_eq!(
        first
            .memory_get("a", "rates", None)
            .unwrap()
            .unwrap()
            .revision,
        2
    );
}

#[test]
fn latest_user_survives_long_tool_runs_without_reloading_all_history() {
    let (_home, mut store, session, _) = fixture();
    for _ in 0..70 {
        store
            .append(
                &session,
                &Message::text(Role::Assistant, "another observation"),
            )
            .unwrap();
    }
    assert_eq!(
        store
            .memory_latest_user(&session)
            .unwrap()
            .unwrap()
            .1
            .content
            .as_deref(),
        Some("Investigate rates")
    );
    store
        .append(&session, &Message::text(Role::User, "Corrected request"))
        .unwrap();
    assert_eq!(
        store
            .memory_latest_user(&session)
            .unwrap()
            .unwrap()
            .1
            .content
            .as_deref(),
        Some("Corrected request")
    );
    store.rewind(&session).unwrap();
    assert_eq!(
        store
            .memory_latest_user(&session)
            .unwrap()
            .unwrap()
            .1
            .content
            .as_deref(),
        Some("Investigate rates")
    );
}
