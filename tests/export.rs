use builder_core::{
    protocol::{Message, Role},
    store::Store,
};

#[test]
fn export_returns_original_active_messages_after_compaction() {
    let home = tempfile::tempdir().unwrap();
    let mut store = Store::open(home.path()).unwrap();
    let id = store
        .create("original export", "local", home.path(), "instructions")
        .unwrap();
    store
        .append(&id, &Message::text(Role::User, "Exact original question"))
        .unwrap();
    store
        .append(
            &id,
            &Message::text(Role::Assistant, "Exact original answer"),
        )
        .unwrap();
    let original = store.messages(&id).unwrap();
    store
        .checkpoint(
            &id,
            &original,
            &[
                Message::text(Role::System, "instructions"),
                Message::text(Role::Assistant, "Lossy summary"),
            ],
        )
        .unwrap();
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_builder"))
        .arg("--home")
        .arg(home.path())
        .args(["export", &id, "--json"])
        .output()
        .unwrap();
    assert!(output.status.success());
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["messages"], serde_json::to_value(original).unwrap());
}
