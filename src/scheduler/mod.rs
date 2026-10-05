//! Local automation execution. The journal owns scheduling state, a process
//! lifetime lock owns dispatch, and the ordinary agent owns tool semantics.
pub mod commands;
pub mod service;
use crate::{
    agent::{Agent, ApprovalMode, SYSTEM, pending},
    memory::MemoryRuntime,
    ui,
};
use anyhow::{Context, Result, ensure};
use builder_core::{
    config::Config,
    schedule::{Access, Run, RunStatus, Schedule},
    store::Store,
};
use builder_provider::OpenAiCompatible;
use builder_tools::Workspace;
use std::{path::Path, time::Duration};

pub fn now() -> i64 {
    chrono::Utc::now().timestamp()
}

/// One worker deliberately serializes scheduled work across workspaces. Scale
/// only when workspace ownership and model capacity can be enforced together.
pub async fn serve(home: &Path, config_home: &Path, once: bool) -> Result<()> {
    let mut store = Store::open(home)?;
    let owner = store.scheduler_lock()?;
    let recovered = store.schedule_recover(&owner, now())?;
    eprintln!(
        "Scheduler ready · {} · {recovered} interrupted runs paused",
        home.display()
    );
    let shutdown = shutdown_signal();
    tokio::pin!(shutdown);
    loop {
        if let Some((schedule, run)) = store.schedule_claim(&owner, now())? {
            eprintln!(
                "Running {} · {}",
                &run.id[..8],
                ui::safe(&schedule.definition.name)
            );
            let outcome = {
                let task = execute(&mut store, config_home, &schedule, &run);
                tokio::pin!(task);
                tokio::select! {
                    biased;
                    signal = &mut shutdown => { signal?; None },
                    result = tokio::time::timeout(Duration::from_secs(schedule.definition.timeout_seconds), &mut task) => {
                        Some(match result {
                            Ok(result) => result,
                            Err(_) => Err(anyhow::anyhow!("Run exceeded its deadline; inspect the session before retrying")),
                        })
                    }
                }
            };
            // Drop the agent future before recording interruption, so tools no
            // longer own subprocesses when the journal reports the terminal state.
            if let Some(session) = store.schedule_run(&run.id)?.session_id {
                store.interrupt_attempts(&session)?;
            }
            let (status, detail) = match &outcome {
                None => (
                    RunStatus::Interrupted,
                    "Runner stopped; inspect the saved session before retrying".to_owned(),
                ),
                Some(Err(error)) => (RunStatus::Failed, format!("{error:#}")),
                Some(Ok((status, detail))) => (*status, detail.clone()),
            };
            store.schedule_finish(&run.id, status, &detail, now())?;
            eprintln!("{} · {:?} · {}", &run.id[..8], status, ui::safe(&detail));
            if outcome.is_none() || once {
                return Ok(());
            }
        } else if once {
            return Ok(());
        }
        tokio::select! {
            signal = &mut shutdown => { signal?; return Ok(()); },
            _ = tokio::time::sleep(Duration::from_secs(1)) => {},
        }
    }
}
async fn execute(
    store: &mut Store,
    config_home: &Path,
    schedule: &Schedule,
    run: &Run,
) -> Result<(RunStatus, String)> {
    let definition = &schedule.definition;
    let config = Config::load(config_home)?;
    let (_, profile) = config.profile(Some(&definition.profile))?;
    ensure!(
        profile.supports_chat(),
        "Scheduled profile must support chat"
    );
    let workspace = Workspace::new(&definition.workspace)?;
    let mut system = format!(
        "{SYSTEM}\n\nWorkspace: {}\n\nThis is an unattended scheduled task. Follow only the saved instruction within its permissions. Do not assume a human is watching or invent additional recurring work.",
        workspace.root().display()
    );
    let instructions = workspace.root().join("AGENTS.md");
    if instructions.is_file() {
        ensure!(
            std::fs::metadata(&instructions)?.len() <= 65536,
            "Workspace AGENTS.md exceeds 64 KiB"
        );
        let content = std::fs::read_to_string(instructions)?;
        ensure!(content.len() <= 65536, "Workspace AGENTS.md exceeds 64 KiB");
        system.push_str(&format!(
            "\n\nWorkspace instructions (AGENTS.md):\n{content}"
        ));
    }
    let session = store.create(
        &definition.name,
        &definition.profile,
        workspace.root(),
        &system,
    )?;
    store.schedule_attach_session(&run.id, &session)?;
    let _guard = store.lock(&session)?;
    let agent = Agent {
        provider: OpenAiCompatible::new(profile.clone())?,
        memory: MemoryRuntime::from_config(&config)?,
        profile,
        workspace,
        session,
        approval: match definition.access {
            Access::ReadOnly => ApprovalMode::ReadOnly,
            Access::Trust => ApprovalMode::Trust,
        },
        max_rounds: definition.max_rounds,
    };
    agent.submit(store, &definition.prompt)?;
    agent.run(store, &mut |_| {}, &mut |_| false).await?;
    ensure!(
        !pending(&store.messages(&agent.session)?),
        "Agent stopped with unfinished work; inspect the session"
    );
    let outcome = crate::completion::assess(
        store,
        &agent.session,
        &agent.workspace,
        &agent.profile.pipeline,
    )?;
    let status = match outcome {
        crate::completion::Outcome::Verified => RunStatus::Succeeded,
        crate::completion::Outcome::Unverified => RunStatus::Unverified,
        crate::completion::Outcome::Failed => RunStatus::Failed,
        _ => RunStatus::Blocked,
    };
    Ok((
        status,
        format!(
            "Task outcome: {outcome:?}. Session {}{}",
            agent.session,
            if matches!(status, RunStatus::Succeeded | RunStatus::Unverified) {
                ""
            } else {
                ". Schedule paused; inspect the recorded evidence before resuming."
            }
        ),
    ))
}
async fn shutdown_signal() -> Result<()> {
    #[cfg(unix)]
    {
        let mut terminate =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
        tokio::select! {
            result = tokio::signal::ctrl_c() => result.context("Could not listen for Ctrl-C"),
            _ = terminate.recv() => Ok(()),
        }
    }
    #[cfg(not(unix))]
    tokio::signal::ctrl_c()
        .await
        .context("Could not listen for Ctrl-C")
}
