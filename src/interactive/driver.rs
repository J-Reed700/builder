use super::maintenance::{MemoryTask, reap_memory_task, start_memory_task, stop_memory_task};
use anyhow::{Result, ensure};
use builder::{
    agent::{Agent, pending},
    ui,
};
use builder_core::{config::Config, store::Store};
use builder_provider::OpenAiCompatible;

pub(super) async fn drive_foreground(
    agent: &Agent<OpenAiCompatible>,
    store: &mut Store,
    interactive: bool,
    home: &std::path::Path,
    config: &Config,
    memory_task: &mut Option<MemoryTask>,
) -> Result<()> {
    stop_memory_task(memory_task).await;
    let result = drive(agent, store, interactive).await;
    reap_memory_task(memory_task);
    if memory_task.is_none()
        && store
            .messages(&agent.session)
            .is_ok_and(|messages| !pending(&messages))
    {
        *memory_task = start_memory_task(agent, home, config);
    }
    result
}

pub(crate) async fn drive(
    agent: &Agent<OpenAiCompatible>,
    store: &mut Store,
    interactive: bool,
) -> Result<()> {
    let renderer = std::cell::RefCell::new(ui::Renderer::new(interactive));
    let mut interrupted = false;
    let result = {
        let mut emit = |event| renderer.borrow_mut().event(event);
        let mut approve = |action: &builder_tools::Action| ui::approve(action);
        let task = agent.run(store, &mut emit, &mut approve);
        tokio::pin!(task);
        let mut frames = tokio::time::interval(std::time::Duration::from_millis(16));
        frames.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                result = &mut task => break result,
                _ = frames.tick(), if interactive => renderer.borrow_mut().flush(),
                signal = tokio::signal::ctrl_c() => {
                    signal?;
                    interrupted = true;
                    break Err(anyhow::anyhow!("Paused. Send a follow-up to change direction, /retry to continue, /cancel to end the response, or /rewind to edit your previous message."));
                }
            }
        }
    };
    renderer.borrow_mut().finish();
    if interrupted {
        store.interrupt_attempts(&agent.session)?;
    }
    result?;
    let outcome = builder::completion::assess(
        store,
        &agent.session,
        &agent.workspace,
        &agent.profile.pipeline,
    )?;
    ensure!(
        !outcome.requires_attention(),
        "Task {} ended with {:?}; review the result before treating it as complete",
        agent.session,
        outcome
    );
    Ok(())
}
