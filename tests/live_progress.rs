//! Opt-in endpoint check. Only disposable fixture files may be edited; shell is denied.
use builder::agent::{Agent, AgentEvent, ApprovalMode, SYSTEM, pending};
use builder_core::{
    config::Config,
    protocol::{Message, Role},
    store::Store,
};
use builder_provider::OpenAiCompatible;
use builder_tools::{Action, Workspace};
use serde_json::json;

#[tokio::test]
#[ignore = "requires BUILDER_LIVE_CONFIG_HOME and a running endpoint"]
async fn compacted_investigation_reaches_a_real_edit() -> anyhow::Result<()> {
    let config = Config::load(std::path::Path::new(&std::env::var(
        "BUILDER_LIVE_CONFIG_HOME",
    )?))?;
    let mut profile = config.profiles[&config.default_profile].clone();
    profile.max_attempts = 1;
    profile.request_timeout_secs = 90;
    profile.idle_timeout_secs = 60;
    let temp = tempfile::tempdir()?;
    let source = format!(
        "{}export function shieldChance(mode) {{ return mode === 'arena' ? 0.025 : 0.025; }}\n",
        "// unrelated fixture line\n".repeat(400)
    );
    std::fs::write(temp.path().join("rates.js"), &source)?;
    let workspace = Workspace::new(temp.path())?;
    let mut store = Store::open(temp.path())?;
    let session = store.create(
        "live disposable progress check",
        "fixture",
        temp.path(),
        SYSTEM,
    )?;
    let prompt = "Change rates.js so shieldChance returns 0.00625 for arena and preserves 0.025 for all other modes. Make the edit and verify it by reading the changed line. Do not create any other files or run shell commands.";
    store.append(&session, &Message::text(Role::User, prompt))?;
    for index in 0..12 {
        let id = format!("fixture_read_{index}");
        let message: Message = serde_json::from_value(json!({"role":"assistant","tool_calls":[{
            "id":id,"type":"function","function":{"name":"read_file","arguments":
                json!({"path":"rates.js","start_line":401,"end_line":401}).to_string()}
        }]}))?;
        let action = Action::from_call(&message.tool_calls[0])?;
        store.append(&session, &message)?;
        store.claim_tool(&session, &id)?;
        store.complete_tool(&session, &id, &workspace.execute(&action).await?)?;
    }
    let before = store.messages(&session)?;
    store.checkpoint(&session, &before, &[
        Message::text(Role::System, SYSTEM),
        Message::text(Role::Assistant, "Compacted findings: rates.js line 401 is export function shieldChance(mode) { return mode === 'arena' ? 0.025 : 0.025; }. All analysis is complete. No edits yet. Next action: edit only the arena branch to 0.00625, then read line 401 to verify."),
        Message::text(Role::User, prompt),
    ])?;
    drop(store);
    let mut store = Store::open(temp.path())?;
    let agent = Agent {
        memory: None,
        provider: OpenAiCompatible::new(profile.clone())?,
        profile,
        workspace,
        session: session.clone(),
        approval: ApprovalMode::Ask,
        max_rounds: 8,
    };
    let mut calls = 0;
    let mut notices = 0;
    let started = std::time::Instant::now();
    tokio::time::timeout(
        std::time::Duration::from_secs(180),
        agent.run(
            &mut store,
            &mut |event| match event {
                AgentEvent::ToolStarted { .. } => calls += 1,
                AgentEvent::ProgressNudge { .. } => notices += 1,
                _ => {}
            },
            &mut |action| matches!(action, Action::EditFile { path, .. } if path == "rates.js"),
        ),
    )
    .await??;
    let expected = source.replace("? 0.025 :", "? 0.00625 :");
    assert_eq!(
        std::fs::read_to_string(temp.path().join("rates.js"))?,
        expected
    );
    assert!(!pending(&store.messages(&session)?));
    assert_eq!(notices, 1);
    eprintln!(
        "Live compacted fixture: correct edit, {calls} tool calls, {:.1}s, original session untouched",
        started.elapsed().as_secs_f64()
    );
    Ok(())
}

#[tokio::test]
#[ignore = "requires BUILDER_LIVE_CONFIG_HOME and BUILDER_REPLAY_DIR disposable export"]
async fn stalled_session_recovery_completes_run_with_multiple_edits() -> anyhow::Result<()> {
    let config = Config::load(std::path::Path::new(&std::env::var(
        "BUILDER_LIVE_CONFIG_HOME",
    )?))?;
    let mut profile = config.profiles[&config.default_profile].clone();
    profile.request_timeout_secs = 90;
    profile.max_attempts = 1;
    let dir = std::path::PathBuf::from(std::env::var("BUILDER_REPLAY_DIR")?);
    let temp = tempfile::tempdir()?;
    fn copy_files(from: &std::path::Path, to: &std::path::Path) -> anyhow::Result<()> {
        std::fs::create_dir_all(to)?;
        for entry in std::fs::read_dir(from)? {
            let entry = entry?;
            let kind = entry.file_type()?;
            if kind.is_dir() {
                copy_files(&entry.path(), &to.join(entry.file_name()))?;
            } else if kind.is_file() {
                std::fs::copy(entry.path(), to.join(entry.file_name()))?;
            }
        }
        Ok(())
    }
    copy_files(&dir.join("workspace"), temp.path())?;
    let history: Vec<Message> = serde_json::from_slice(&std::fs::read(dir.join("history.json"))?)?;
    let mut context: Vec<Message> =
        serde_json::from_slice(&std::fs::read(dir.join("context.json"))?)?;
    for message in &mut context {
        if message.role == Role::System {
            message.content = Some(format!(
                "{}\nReplay workspace: {}. All file actions must use relative paths within this disposable workspace.",
                message.content.as_deref().unwrap_or(""),
                temp.path().display()
            ));
        }
    }
    let mut store = Store::open(temp.path())?;
    let session = store.create("stalled replay", "fixture", temp.path(), SYSTEM)?;
    for message in history.iter().skip(1) {
        store.append(&session, message)?;
    }
    let before = store.messages(&session)?;
    store.checkpoint(&session, &before, &context)?;
    let agent = Agent {
        memory: None,
        provider: OpenAiCompatible::new(profile.clone())?,
        profile,
        workspace: Workspace::new(temp.path())?,
        session: session.clone(),
        approval: ApprovalMode::Ask,
        max_rounds: 40,
    };
    let started = std::time::Instant::now();
    let result = tokio::time::timeout(
        std::time::Duration::from_secs(480),
        agent.run(
            &mut store,
            &mut |event| match event {
                AgentEvent::ToolStarted { name, detail } => {
                    eprintln!("Replay tool: {name} {detail}")
                }
                AgentEvent::ToolFinished { name, failed, .. } if failed => {
                    eprintln!("Replay failed tool: {name}")
                }
                AgentEvent::Compacted { before, after, .. } => {
                    eprintln!("Replay checkpoint: {before} -> {after}")
                }
                _ => {}
            },
            &mut |action| matches!(action, Action::EditFile { .. } | Action::WriteFile { .. }),
        ),
    )
    .await?;
    let mut changed = 0;
    for path in [
        "src/game/engine/types.ts",
        "src/game/engine/orbPlacement.ts",
        "src/game/engine/GameStateManagerReanimated.ts",
        "src/components/GameRenderer/GameRendererReanimated/gameConfig.ts",
        "src/components/GameRenderer/GameRendererReanimated/GameRendererReanimated.tsx",
    ] {
        if std::fs::read(temp.path().join(path))?
            != std::fs::read(dir.join("workspace").join(path))?
        {
            changed += 1;
        }
    }
    eprintln!(
        "Replay changed {changed} target files in {:.1}s; run outcome: {result:?}",
        started.elapsed().as_secs_f64()
    );
    result?;
    assert!(
        !pending(&store.messages(&session)?),
        "Replay is still unfinished"
    );
    assert!(
        changed >= 3,
        "Recovery must advance the actual multi-file task, not just read or explain"
    );
    Ok(())
}
