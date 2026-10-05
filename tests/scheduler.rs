use axum::{Json, Router, routing::post};
use builder_core::{
    config::Config,
    schedule::{Access, Cadence, Definition, RunStatus, ScheduleState},
    store::Store,
};
use serde_json::{Value, json};
use std::{
    process::Command,
    sync::{Arc, Mutex},
};

fn definition(home: &std::path::Path) -> Definition {
    Definition {
        name: "Scheduled inspection".into(),
        prompt: "Read the project and report its status".into(),
        workspace: home.canonicalize().unwrap(),
        profile: "local".into(),
        cadence: Cadence::Every { seconds: 60 },
        access: Access::ReadOnly,
        max_rounds: 5,
        timeout_seconds: 30,
    }
}
fn cli(home: &std::path::Path, args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_builder"))
        .arg("--home")
        .arg(home)
        .arg("-C")
        .arg(home)
        .args(args)
        .output()
        .unwrap()
}
#[test]
fn shell_and_slash_commands_share_validation_and_preserve_prompt() {
    use builder::scheduler::commands::{ScheduleCommand, parse_slash};
    let parsed =
        parse_slash("/schedule every 90m Don't edit files; say \"hello\" --allow-writes").unwrap();
    let Some(ScheduleCommand::Add(add)) = parsed.command else {
        panic!("expected add")
    };
    assert_eq!(add.every.as_deref(), Some("90m"));
    assert_eq!(add.prompt, "Don't edit files; say \"hello\" --allow-writes");
    assert!(!add.allow_writes);
    assert!(parse_slash("/schedule add --every 1h --in 2h 'test'").is_err());
    assert!(parse_slash("/schedule add 'test'").is_err());
    assert!(
        parse_slash(
            "/schedule add --cron '0 9 * * MON-FRI' --timezone America/Chicago 'Review changes'"
        )
        .is_ok()
    );
    assert!(parse_slash("/schedule").unwrap().command.is_none());
    assert!(parse_slash("/scheduleevil").is_err());
    assert!(parse_slash("/schedule every   5m   Inspect files").is_ok());
}
#[test]
fn cli_creates_inspects_pauses_and_retains_history_without_model_connection() {
    let home = tempfile::tempdir().unwrap();
    let created = cli(
        home.path(),
        &[
            "--auto",
            "schedule",
            "add",
            "--every",
            "90m",
            "--name",
            "Morning review",
            "Inspect files",
            "--json",
        ],
    );
    assert!(
        created.status.success(),
        "{}",
        String::from_utf8_lossy(&created.stderr)
    );
    let created: Value = serde_json::from_slice(&created.stdout).unwrap();
    let id = created["schedule"]["id"].as_str().unwrap();
    assert_eq!(created["schedule"]["definition"]["access"], "read_only");
    assert_eq!(
        created["schedule"]["definition"]["cadence"]["seconds"],
        5400
    );
    assert_eq!(created["runner_connected"], false);
    let shown = cli(home.path(), &["schedule", "show", id]);
    let text = String::from_utf8(shown.stdout).unwrap();
    assert!(text.contains("Morning review"));
    assert!(text.contains("Offline"));
    for action in ["pause", "run", "delete"] {
        let result = cli(home.path(), &["schedule", action, id, "--json"]);
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
    }
    let history = cli(home.path(), &["schedule", "history", id, "--json"]);
    let history: Value = serde_json::from_slice(&history.stdout).unwrap();
    assert_eq!(history["runs"][0]["status"], "cancelled");
    let empty = cli(home.path(), &["daemon", "--once"]);
    assert!(empty.status.success());
    assert!(
        !cli(
            home.path(),
            &["--approval", "read-only", "daemon", "--once"]
        )
        .status
        .success()
    );
}

async fn fixture(
    replies: Vec<Value>,
) -> (
    tempfile::TempDir,
    Arc<Mutex<Vec<Value>>>,
    tokio::task::JoinHandle<()>,
) {
    let requests = Arc::new(Mutex::new(Vec::new()));
    let seen = requests.clone();
    let replies = Arc::new(Mutex::new(std::collections::VecDeque::from(replies)));
    let app=Router::new().route("/v1/chat/completions",post(move |Json(body):Json<Value>| {
        seen.lock().unwrap().push(body);
        let reply=replies.lock().unwrap().pop_front().unwrap_or_else(||json!({"choices":[{"message":{"role":"assistant","content":"Finished"},"finish_reason":"stop"}]}));
        async move {Json(reply)}
    }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let home = tempfile::tempdir().unwrap();
    let mut config = Config::default();
    let profile = config.profiles.get_mut("local").unwrap();
    profile.base_url = format!("http://{}/v1", listener.local_addr().unwrap());
    profile.stream = false;
    profile.max_attempts = 1;
    profile.pipeline.subagents = false;
    config.save(home.path()).unwrap();
    let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (home, requests, task)
}
#[tokio::test]
async fn runner_uses_saved_profile_workspace_instruction_and_fresh_sessions() {
    let (home, requests, server) = fixture(vec![]).await;
    std::fs::write(
        home.path().join("AGENTS.md"),
        "Use the project conventions.",
    )
    .unwrap();
    let mut store = Store::open(home.path()).unwrap();
    let schedule = store
        .schedule_create(&definition(home.path()), builder::scheduler::now())
        .unwrap();
    for _ in 0..2 {
        store
            .schedule_enqueue(&schedule.id, builder::scheduler::now())
            .unwrap();
        builder::scheduler::serve(home.path(), home.path(), true)
            .await
            .unwrap();
    }
    let runs = store.schedule_runs(&schedule.id, 10).unwrap();
    assert_eq!(runs.len(), 2);
    assert!(runs.iter().all(|r| r.status == RunStatus::Unverified));
    assert_ne!(runs[0].session_id, runs[1].session_id);
    let seen = requests.lock().unwrap();
    assert_eq!(seen.len(), 2);
    let serialized = seen[0].to_string();
    assert!(serialized.contains("Use the project conventions."));
    assert!(serialized.contains("Read the project and report its status"));
    assert!(!store.scheduler_running().unwrap());
    server.abort();
}
#[tokio::test]
async fn unattended_readonly_never_executes_mutation_and_pauses_blocked_job() {
    let reply = json!({"choices":[{"message":{"role":"assistant","content":null,"tool_calls":[{"id":"write1","type":"function","function":{"name":"write_file","arguments":"{\"path\":\"should-not-exist.txt\",\"content\":\"bad\"}"}}]},"finish_reason":"tool_calls"}]});
    let (home, _, server) = fixture(vec![reply]).await;
    let mut store = Store::open(home.path()).unwrap();
    let schedule = store
        .schedule_create(&definition(home.path()), builder::scheduler::now())
        .unwrap();
    store
        .schedule_enqueue(&schedule.id, builder::scheduler::now())
        .unwrap();
    builder::scheduler::serve(home.path(), home.path(), true)
        .await
        .unwrap();
    assert!(!home.path().join("should-not-exist.txt").exists());
    assert_eq!(
        store.schedule_runs(&schedule.id, 10).unwrap()[0].status,
        RunStatus::Blocked
    );
    assert!(matches!(
        store.schedule_get(&schedule.id).unwrap().state,
        ScheduleState::Paused
    ));
    server.abort();
}

#[tokio::test]
async fn nonzero_shell_exit_cannot_be_reported_as_schedule_success() {
    let reply = json!({"choices":[{"message":{"role":"assistant","content":null,"tool_calls":[{"id":"check","type":"function","function":{"name":"shell","arguments":"{\"command\":\"exit 7\",\"timeout_secs\":5}"}}]},"finish_reason":"tool_calls"}]});
    let answer = json!({"choices":[{"message":{"role":"assistant","content":"The required check failed. I cannot complete the task."},"finish_reason":"stop"}]});
    let (home, _, server) = fixture(vec![reply, answer]).await;
    let mut store = Store::open(home.path()).unwrap();
    let mut definition = definition(home.path());
    definition.access = Access::Trust;
    let schedule = store
        .schedule_create(&definition, builder::scheduler::now())
        .unwrap();
    store
        .schedule_enqueue(&schedule.id, builder::scheduler::now())
        .unwrap();
    builder::scheduler::serve(home.path(), home.path(), true)
        .await
        .unwrap();
    let runs = store.schedule_runs(&schedule.id, 10).unwrap();
    assert_eq!(runs[0].status, RunStatus::Failed);
    assert_eq!(
        store
            .tool_outcomes(runs[0].session_id.as_deref().unwrap())
            .unwrap()["check"],
        builder_core::store::ToolOutcome::Failed
    );
    assert!(matches!(
        store.schedule_get(&schedule.id).unwrap().state,
        ScheduleState::Paused
    ));
    server.abort();
}
#[tokio::test]
async fn configuration_failure_is_durable_and_does_not_poison_other_jobs() {
    let (home, _, server) = fixture(vec![]).await;
    let mut store = Store::open(home.path()).unwrap();
    let mut bad = definition(home.path());
    bad.profile = "removed-profile".into();
    let a = store.schedule_create(&bad, 1000).unwrap();
    let b = store
        .schedule_create(&definition(home.path()), 2000)
        .unwrap();
    builder::scheduler::serve(home.path(), home.path(), true)
        .await
        .unwrap();
    assert_eq!(
        store.schedule_runs(&a.id, 10).unwrap()[0].status,
        RunStatus::Failed
    );
    builder::scheduler::serve(home.path(), home.path(), true)
        .await
        .unwrap();
    assert_eq!(
        store.schedule_runs(&b.id, 10).unwrap()[0].status,
        RunStatus::Unverified
    );
    server.abort();
}
#[test]
fn service_templates_escape_arguments_without_executing_or_installing() {
    use builder::scheduler::service::{ServiceFormat, render};
    let home = tempfile::tempdir().unwrap();
    let systemd = render(
        ServiceFormat::Systemd,
        std::path::Path::new("/opt/My App/$builder%/builder"),
        Some(home.path()),
        home.path(),
    )
    .unwrap();
    assert!(systemd.contains("\"/opt/My App/$$builder%%/builder\""));
    let launchd = render(
        ServiceFormat::Launchd,
        std::path::Path::new("/opt/a&b/builder"),
        None,
        home.path(),
    )
    .unwrap();
    assert!(launchd.contains("/opt/a&amp;b/builder"));
    assert!(!launchd.contains("--home"));
    let output = cli(home.path(), &["daemon", "--service", "launchd"]);
    assert!(output.status.success());
    assert!(String::from_utf8(output.stdout).unwrap().contains("<plist"));
}

#[tokio::test]
async fn deadline_cancels_generation_and_pauses_without_replaying() {
    let home = tempfile::tempdir().unwrap();
    let app = Router::new().route(
        "/v1/chat/completions",
        post(|| async {
            tokio::time::sleep(std::time::Duration::from_secs(30)).await;
            Json(json!({"choices":[]}))
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let mut config = Config::default();
    let profile = config.profiles.get_mut("local").unwrap();
    profile.base_url = format!("http://{}/v1", listener.local_addr().unwrap());
    profile.stream = false;
    profile.pipeline.subagents = false;
    config.save(home.path()).unwrap();
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let mut store = Store::open(home.path()).unwrap();
    let mut def = definition(home.path());
    def.timeout_seconds = 1;
    let schedule = store
        .schedule_create(&def, builder::scheduler::now())
        .unwrap();
    store
        .schedule_enqueue(&schedule.id, builder::scheduler::now())
        .unwrap();
    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        builder::scheduler::serve(home.path(), home.path(), true),
    )
    .await
    .unwrap()
    .unwrap();
    let runs = store.schedule_runs(&schedule.id, 10).unwrap();
    assert_eq!(runs[0].status, RunStatus::Failed);
    assert!(runs[0].detail.as_ref().unwrap().contains("deadline"));
    assert!(runs[0].session_id.is_some());
    builder::scheduler::serve(home.path(), home.path(), true)
        .await
        .unwrap();
    assert_eq!(store.schedule_runs(&schedule.id, 10).unwrap().len(), 1);
    server.abort();
}

#[cfg(unix)]
#[tokio::test]
async fn sigterm_stops_an_active_daemon_and_records_interruption() {
    let home = tempfile::tempdir().unwrap();
    let app = Router::new().route(
        "/v1/chat/completions",
        post(|| async {
            tokio::time::sleep(std::time::Duration::from_secs(30)).await;
            Json(json!({"choices":[]}))
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let mut config = Config::default();
    let profile = config.profiles.get_mut("local").unwrap();
    profile.base_url = format!("http://{}/v1", listener.local_addr().unwrap());
    profile.stream = false;
    profile.pipeline.subagents = false;
    config.save(home.path()).unwrap();
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let mut store = Store::open(home.path()).unwrap();
    let schedule = store
        .schedule_create(&definition(home.path()), builder::scheduler::now())
        .unwrap();
    store
        .schedule_enqueue(&schedule.id, builder::scheduler::now())
        .unwrap();
    let mut child = tokio::process::Command::new(env!("CARGO_BIN_EXE_builder"))
        .arg("--home")
        .arg(home.path())
        .arg("daemon")
        .kill_on_drop(true)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            let runs = store.schedule_runs(&schedule.id, 10).unwrap();
            if runs[0].session_id.is_some() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    assert!(
        Command::new("kill")
            .args(["-TERM", &child.id().unwrap().to_string()])
            .status()
            .unwrap()
            .success()
    );
    let status = tokio::time::timeout(std::time::Duration::from_secs(5), child.wait())
        .await
        .unwrap()
        .unwrap();
    assert!(status.success());
    assert_eq!(
        store.schedule_runs(&schedule.id, 10).unwrap()[0].status,
        RunStatus::Interrupted
    );
    assert!(matches!(
        store.schedule_get(&schedule.id).unwrap().state,
        ScheduleState::Paused
    ));
    assert!(!store.scheduler_running().unwrap());
    server.abort();
}
