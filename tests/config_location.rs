#[cfg(unix)]
use builder_core::{config::Config, store::Store};
use std::process::Command;

#[test]
fn explicit_home_remains_self_contained_even_with_xdg_set() {
    let portable = tempfile::tempdir().unwrap();
    let other = tempfile::tempdir().unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_builder"))
        .env("XDG_CONFIG_HOME", other.path())
        .args([
            "--home",
            portable.path().to_str().unwrap(),
            "config",
            "init",
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(portable.path().join("config.toml").is_file());
    assert!(!other.path().join("builder/config.toml").exists());
}

#[cfg(unix)]
#[test]
fn default_cli_migrates_config_and_keeps_sessions_in_existing_data_directory() {
    let user = tempfile::tempdir().unwrap();
    let xdg = user.path().join("config-root");
    #[cfg(target_os = "macos")]
    let data = user
        .path()
        .join("Library/Application Support/dev.builder.builder");
    #[cfg(not(target_os = "macos"))]
    let data = user.path().join("data/builder");
    let mut config = Config {
        default_profile: "preserved".into(),
        ..Default::default()
    };
    let profile = config.profiles.remove("local").unwrap();
    config.profiles.insert("preserved".into(), profile);
    config.save(&data).unwrap();
    let mut store = Store::open(&data).unwrap();
    let id = store
        .create("Keep this conversation", "preserved", user.path(), "system")
        .unwrap();
    drop(store);
    let run = |args: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_builder"))
            .env_remove("BUILDER_HOME")
            .env("HOME", user.path())
            .env("XDG_CONFIG_HOME", &xdg)
            .env("XDG_DATA_HOME", user.path().join("data"))
            .args(args)
            .output()
            .unwrap()
    };
    let output = run(&["config", "init"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).trim(),
        xdg.join("builder/config.toml").to_str().unwrap()
    );
    assert_eq!(
        Config::load(&xdg.join("builder")).unwrap().default_profile,
        "preserved"
    );
    let output = run(&["sessions"]);
    assert!(output.status.success());
    assert!(String::from_utf8_lossy(&output.stdout).contains("Keep this conversation"));
    assert!(Store::open(&data).unwrap().session(&id).is_ok());
    assert!(!xdg.join("builder/builder.sqlite3").exists());
    // A subsequent config command edits the new file, leaving the legacy copy unchanged.
    assert!(run(&["config", "model", "updated-model"]).status.success());
    assert_eq!(
        Config::load(&xdg.join("builder")).unwrap().profiles["preserved"].model,
        "updated-model"
    );
    assert_ne!(
        Config::load(&data).unwrap().profiles["preserved"].model,
        "updated-model"
    );
}
