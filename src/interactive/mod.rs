mod compaction;
mod driver;
mod maintenance;

pub(crate) use driver::drive;

use self::{
    compaction::{CompactionTask, start_compaction, wait_for_compaction},
    driver::drive_foreground,
    maintenance::{MemoryTask, configured_index_update_mode, start_memory_task, stop_memory_task},
};
use crate::commands;
use anyhow::{Context, Result, ensure};
use builder::{
    agent::{Agent, ApprovalMode, estimate_tokens, pending},
    ui,
};
use builder_core::{
    config::{self, Config},
    protocol::{Message, Role},
    store::Store,
};
use builder_provider::OpenAiCompatible;
use console::style;
use std::{
    io::{self, IsTerminal},
    path::PathBuf,
};

pub(crate) struct SessionOptions {
    pub(crate) profile_name: String,
    pub(crate) home: PathBuf,
    pub(crate) config_home: PathBuf,
    pub(crate) config: Config,
    pub(crate) plain_requested: bool,
}

pub(crate) async fn run(
    agent: &mut Agent<OpenAiCompatible>,
    store: &mut Store,
    initial_prompt: Option<String>,
    options: SessionOptions,
) -> Result<()> {
    let SessionOptions {
        profile_name,
        home,
        config_home,
        mut config,
        plain_requested,
    } = options;
    ui::banner(
        &profile_name,
        &agent.profile.model,
        agent.workspace.root(),
        &agent.session,
        match agent.approval {
            ApprovalMode::Ask => "approve changes",
            ApprovalMode::ReadOnly => "read only",
            ApprovalMode::Trust => "auto · all tools approved",
        },
    );
    let existing = store.messages(&agent.session)?;
    if existing.len() > 1 {
        eprintln!(
            "  {}",
            style(format!("Restored {} messages from disk", existing.len())).dim()
        );
        if let Some(answer) = restored_answer(&existing) {
            println!("\n{}\n", ui::safe(answer));
        }
        if let Some(list) = store
            .todos(&agent.session)?
            .filter(|list| !list.is_finished())
        {
            ui::print_todos(&list);
        }
    }
    if pending(&existing) {
        eprintln!("  Paused turn restored. Send a follow-up, /retry, /cancel, or /rewind.");
    }
    let mut memory_task = None;
    let mut compaction_task = None;
    if let Some(prompt) = initial_prompt {
        agent.submit(store, &prompt)?;
        if let Err(error) =
            drive_foreground(agent, store, true, &home, &config, &mut memory_task).await
        {
            report(&error);
        }
    }
    let mut editor = builder::input::Composer::default();
    if let Some(draft) = store.composer_draft(&agent.session)? {
        editor.set_draft(&draft)?;
        eprintln!("  Previous message restored as a draft; Enter sends it.");
    }
    let plain = plain_requested
        || std::env::var("TERM").is_ok_and(|term| term == "dumb")
        || !io::stdout().is_terminal();
    loop {
        let messages = store.messages(&agent.session)?;
        if memory_task.is_none() && compaction_task.is_none() && !pending(&messages) {
            memory_task = start_memory_task(agent, &home, &config);
        }
        let used = estimate_tokens(&messages);
        let percent = used.saturating_mul(100) / agent.profile.context_tokens;
        let status = compaction_task.as_ref().map_or_else(
            || {
                format!(
                    "{} · {} · {}% context",
                    if pending(&messages) {
                        "Paused: send a follow-up or /retry"
                    } else {
                        "Saved"
                    },
                    match agent.approval {
                        ApprovalMode::Ask => "approve changes",
                        ApprovalMode::ReadOnly => "read only",
                        ApprovalMode::Trust => "auto · all tools approved",
                    },
                    percent,
                )
            },
            CompactionTask::status,
        );
        let input = if plain {
            editor.read_plain()?
        } else if compaction_task.is_some() {
            let initial_status = status.clone();
            editor.read_with_status(&initial_status, || {
                compaction_task.as_ref().map(CompactionTask::status)
            })?
        } else {
            editor.read(&status)?
        };
        let prompt = match input {
            builder::input::Input::Submit(prompt) => prompt,
            builder::input::Input::Exit => {
                if let Some(task) = compaction_task.as_mut() {
                    task.cancel();
                }
                if compaction_task.is_some()
                    && let Err(error) = wait_for_compaction(&mut compaction_task).await
                {
                    report(&error);
                }
                break;
            }
            builder::input::Input::Interrupt => {
                if compaction_task.is_none() {
                    break;
                }
                if let Some(task) = compaction_task.as_mut() {
                    task.cancel();
                }
                if let Err(error) = wait_for_compaction(&mut compaction_task).await {
                    report(&error);
                }
                continue;
            }
        };
        let line = prompt.trim();
        if line.is_empty() {
            continue;
        }
        if compaction_task.is_some() {
            println!("\n  Message queued until context compaction finishes.");
            match wait_for_compaction(&mut compaction_task).await {
                Ok(true) => println!("  Context compacted · sending queued message."),
                Ok(false) => println!("  Compaction finished · sending queued message."),
                Err(error) => {
                    report(&error);
                    eprintln!("  Sending the queued message with the original context.");
                }
            }
        }
        match line {
            "/exit" | "/quit" => break,
            "/help" => println!("\n{}", ui::help(ui::panel::width())),
            command if command == "/schedule" || command.starts_with("/schedule ") => {
                let result = builder::scheduler::commands::parse_slash(command).and_then(|args| {
                    builder::scheduler::commands::handle(
                        &args,
                        store,
                        &config,
                        agent.workspace.root(),
                        Some(&profile_name),
                    )
                });
                if let Err(error) = result {
                    report(&error);
                }
            }
            "/memory" => {
                stop_memory_task(&mut memory_task).await;
                let chosen = builder::input::memory::choose(&config.memory, plain);
                match chosen {
                    Ok(None) => println!("Memory settings unchanged."),
                    Err(error) => report(&anyhow::anyhow!("{error}")),
                    Ok(Some(mode)) => {
                        let update = async {
                            let mut latest = Config::load(&config_home)?;
                            let baseline = latest.memory.clone();
                            match mode {
                                builder::input::memory::Mode::Local => {
                                    commands::memory::setup_local_memory(&mut latest, &home).await?
                                }
                                builder::input::memory::Mode::Lexical => {
                                    latest.memory.enabled = true;
                                    latest.memory.embedding_backend =
                                        config::EmbeddingBackend::Lexical;
                                }
                                builder::input::memory::Mode::Disabled => {
                                    latest.memory.enabled = false
                                }
                            }
                            let mut current = Config::load(&config_home)?;
                            ensure!(
                                current.memory == baseline,
                                "Memory settings changed during setup; reopen /memory before saving"
                            );
                            current.memory = latest.memory;
                            let memory = builder::memory::MemoryRuntime::from_config(&current)?;
                            current.save(&config_home)?;
                            agent.memory = memory;
                            config = current;
                            println!("Memory settings saved and applied to this session.");
                            Ok::<_, anyhow::Error>(())
                        }
                        .await;
                        if let Err(error) = update {
                            report(&error);
                        }
                    }
                }
            }
            "/settings" | "/pipeline" => {
                stop_memory_task(&mut memory_task).await;
                let baseline = Config::load(&config_home)?
                    .profiles
                    .get(&profile_name)
                    .context("Session profile no longer exists")?
                    .pipeline
                    .clone();
                let changed = builder::input::pipeline::edit(
                    &agent.profile.pipeline,
                    &profile_name,
                    plain,
                    |settings| {
                        let persist = || -> Result<()> {
                            let mut latest = Config::load(&config_home)?;
                            let profile = latest
                                .profiles
                                .get_mut(&profile_name)
                                .context("Session profile no longer exists")?;
                            ensure!(
                                profile.pipeline == baseline,
                                "Pipeline settings changed elsewhere. Cancel and reopen this menu to reload them."
                            );
                            profile.pipeline = settings.clone();
                            profile.validate()?;
                            latest.save(&config_home)?;
                            Ok(())
                        };
                        persist().map_err(|e| format!("Could not save: {e}"))
                    },
                );
                match changed {
                    Ok(Some(settings)) => {
                        agent.max_rounds = settings.max_rounds;
                        agent.profile.pipeline = settings;
                        println!(
                            "Pipeline settings saved for {} and applied to this session.",
                            ui::safe(&profile_name)
                        );
                    }
                    Ok(None) => println!("Settings unchanged."),
                    Err(error) => report(&error.into()),
                }
            }
            "/status" => {
                let messages = store.messages(&agent.session)?;
                let index_status = builder::code_index::status(store, &agent.workspace)?;
                let coverage = builder::code_index::coverage(
                    store,
                    &agent.workspace,
                    agent.memory.as_ref().map(|memory| memory.embeddings()),
                )?;
                let query_summary = builder::code_index::query_summary(store, &agent.workspace)?;
                let update_mode = memory_task.as_ref().map_or_else(
                    || configured_index_update_mode(&agent.profile),
                    MemoryTask::index_update_description,
                );
                let memory_status = memory_task
                    .as_ref()
                    .and_then(MemoryTask::memory_status)
                    .unwrap_or_else(|| {
                        if agent.memory.is_some() {
                            "enabled; worker idle or paused".into()
                        } else {
                            "disabled".into()
                        }
                    });
                let used = estimate_tokens(&messages);
                let limit = agent.profile.context_tokens;
                let percent = used.saturating_mul(100) / limit.max(1);
                let pipeline = &agent.profile.pipeline;
                let mut rows = vec![
                    ui::panel::section("Context"),
                    ui::panel::field("Messages", ui::panel::count(messages.len())),
                    ui::panel::field(
                        "History",
                        format!(
                            "{} of {} tokens  {}  {percent}%",
                            ui::panel::count(used),
                            ui::panel::count(limit),
                            ui::panel::meter(percent, 12)
                        ),
                    ),
                    ui::panel::field(
                        "Reserved",
                        format!(
                            "{} output tokens; tool schemas are counted separately",
                            ui::panel::count(agent.profile.max_output_tokens)
                        ),
                    ),
                    ui::panel::field(
                        "Compaction",
                        if agent.profile.auto_compact {
                            format!(
                                "automatic at {}% · originals retained",
                                agent.profile.compact_at_percent
                            )
                        } else {
                            "off · /compact summarizes on request".into()
                        },
                    ),
                    ui::panel::field(
                        "State",
                        if pending(&messages) {
                            "paused; send a follow-up, /retry, /cancel, or /rewind"
                        } else {
                            "ready"
                        },
                    ),
                    ui::panel::section("Session"),
                    ui::panel::field(
                        "Profile",
                        format!(
                            "{} · {}",
                            ui::safe(&profile_name),
                            ui::safe(&agent.profile.model)
                        ),
                    ),
                    ui::panel::field(
                        "Approval",
                        match agent.approval {
                            ApprovalMode::Ask => "approve changes",
                            ApprovalMode::ReadOnly => "read only",
                            ApprovalMode::Trust => "auto · all tools approved",
                        },
                    ),
                    ui::panel::field("Rounds per run", agent.max_rounds.to_string()),
                    ui::panel::field("Workspace", ui::short_path(agent.workspace.root())),
                    ui::panel::field("Storage", ui::short_path(&home)),
                    ui::panel::section("Code index"),
                ];
                match index_status {
                    None => rows.push(ui::panel::field("Build", "not built yet")),
                    Some(status) => {
                        rows.push(ui::panel::field(
                            "Build",
                            format!(
                                "generation {} · {} files · {} chunks{}",
                                status.generation,
                                ui::panel::count(status.files),
                                ui::panel::count(status.chunks),
                                coverage
                                    .map(|(indexed, total)| format!(
                                        " · semantic {indexed}/{total}"
                                    ))
                                    .unwrap_or_default()
                            ),
                        ));
                        rows.push(ui::panel::field(
                            "State",
                            format!(
                                "{} · {} skipped · refreshed {}",
                                ui::safe(&status.status),
                                status.skipped,
                                ui::safe(&status.completed_at)
                            ),
                        ));
                        rows.push(ui::panel::field("Updates", ui::safe(&update_mode)));
                        rows.push(ui::panel::field(
                            "Queries",
                            format!(
                                "{} · {} abstained · {} stale suppressed · {}ms average",
                                query_summary.queries,
                                query_summary.abstentions,
                                query_summary.stale_suppressions,
                                query_summary.average_elapsed_ms
                            ),
                        ));
                    }
                }
                rows.extend([
                    ui::panel::section("Memory"),
                    ui::panel::field("Status", ui::safe(&memory_status)),
                    ui::panel::section("Guards"),
                    ui::panel::field(
                        "Progress",
                        format!(
                            "focus after {} tool calls; tool-free conclusion at {}; full context preserved",
                            pipeline.progress_check_calls,
                            pipeline.max_no_progress_calls()
                        ),
                    ),
                    ui::panel::field(
                        "Failures",
                        format!(
                            "recover after {} consecutive failures; {} recovery rounds",
                            pipeline.failure_check_calls, pipeline.failure_recovery_rounds
                        ),
                    ),
                    ui::panel::field(
                        "Tool limits",
                        format!(
                            "{} per response; {} completed identical shell calls",
                            pipeline.tool_calls_per_response, pipeline.identical_shell_calls
                        ),
                    ),
                ]);
                println!(
                    "\n{}",
                    ui::panel::render(
                        "Status",
                        &format!(
                            "session {}",
                            agent.session.chars().take(8).collect::<String>()
                        ),
                        &rows,
                        ui::panel::width()
                    )
                );
            }
            "/history" | "/history archived" => {
                let history = if line == "/history archived" {
                    store.archived_messages(&agent.session)?
                } else {
                    store.history_messages(&agent.session)?
                };
                for message in history.iter().filter(|m| m.role != Role::System) {
                    println!(
                        "\n{}\n{}",
                        style(&message.role).bold(),
                        ui::safe(message.content.as_deref().unwrap_or("[tool call]"))
                    );
                }
            }
            "/todo" => match store.todos(&agent.session)? {
                Some(list) => ui::print_todos(&list),
                None => println!(
                    "No todo list yet. The agent writes one when it is ready to implement multi-step work."
                ),
            },
            "/cancel" => {
                stop_memory_task(&mut memory_task).await;
                match store.interrupt_turn(&agent.session, None) {
                    Ok(uncertain) => {
                        println!(
                            "Pending response cancelled. Send your next instruction when ready."
                        );
                        if uncertain > 0 {
                            eprintln!(
                                "An interrupted tool has an uncertain outcome. Inspect the workspace before continuing."
                            );
                        }
                    }
                    Err(error) => report(&error),
                }
            }
            "/rewind" => {
                stop_memory_task(&mut memory_task).await;
                match store.rewind(&agent.session) {
                    Ok((prompt, uncertain)) => {
                        editor.set_draft(&prompt)?;
                        println!(
                            "Last turn archived; previous message restored for editing. Enter sends it. Workspace changes are not undone. View originals with /history archived."
                        );
                        if uncertain > 0 {
                            eprintln!(
                                "An interrupted tool has an uncertain outcome. Inspect the workspace before sending the revised message."
                            );
                        }
                    }
                    Err(error) => report(&error),
                }
            }
            "/compact" => {
                stop_memory_task(&mut memory_task).await;
                match start_compaction(agent, &home, &config) {
                    Ok(task) => {
                        compaction_task = Some(task);
                        println!("Compaction started · enter a message to queue it.");
                    }
                    Err(error) => report(&error),
                }
            }
            "/clear" => {
                stop_memory_task(&mut memory_task).await;
                let uncertain = store.close_pending_turn(&agent.session)?;
                match store.clear(&agent.session) {
                    Ok(archived) => {
                        // Rich mode: wipe the visible screen so the fresh composer
                        // starts at the top, like a brand-new conversation. The
                        // transcript stays in /history archived. Plain/piped output
                        // keeps its normal scrollback, so only clear a real TTY.
                        if !plain {
                            use crossterm::{
                                cursor, execute,
                                terminal::{Clear, ClearType},
                            };
                            let _ =
                                execute!(io::stdout(), cursor::MoveTo(0, 0), Clear(ClearType::All));
                        }
                        println!(
                            "Conversation cleared ({} messages archived); starting fresh. The transcript remains in /history archived. Workspace changes are not undone.",
                            archived
                        );
                        if uncertain > 0 {
                            eprintln!(
                                "An interrupted tool has an uncertain outcome. Inspect the workspace before continuing."
                            );
                        }
                    }
                    Err(error) => report(&error),
                }
            }
            "/retry" => {
                if let Err(error) =
                    drive_foreground(agent, store, true, &home, &config, &mut memory_task).await
                {
                    report(&error);
                }
            }
            _ if line.starts_with("/attach ") && !line.contains('\n') => {
                stop_memory_task(&mut memory_task).await;
                let path = line.trim_start_matches("/attach ").trim();
                match agent
                    .workspace
                    .execute(&builder_tools::Action::ReadFile {
                        path: path.into(),
                        start_line: None,
                        end_line: None,
                    })
                    .await
                {
                    Ok(content) => {
                        let text = format!(
                            "Attached file: {path}\n\n{content}\n\nPlease acknowledge this file; I will send my task next."
                        );
                        match agent.submit(store, &text) {
                            Ok(()) => {
                                if let Err(error) = drive_foreground(
                                    agent,
                                    store,
                                    true,
                                    &home,
                                    &config,
                                    &mut memory_task,
                                )
                                .await
                                {
                                    report(&error);
                                }
                            }
                            Err(error) => report(&error),
                        }
                    }
                    Err(error) => report(&error),
                }
            }
            _ if builder::input::is_unknown_command(line) => {
                eprintln!("Unknown command. Type /help.")
            }
            _ => {
                stop_memory_task(&mut memory_task).await;
                match agent.submit(store, &prompt) {
                    Ok(()) => {
                        if let Err(error) =
                            drive_foreground(agent, store, true, &home, &config, &mut memory_task)
                                .await
                        {
                            report(&error);
                        }
                    }
                    Err(error) => report(&error),
                }
            }
        }
    }
    stop_memory_task(&mut memory_task).await;
    eprintln!(
        "\n  {} builder resume {}\n",
        style("Saved. Continue with").dim(),
        &agent.session[..8]
    );
    Ok(())
}

fn restored_answer(messages: &[Message]) -> Option<&str> {
    if pending(messages) {
        return None;
    }
    messages
        .last()
        .filter(|message| message.role == Role::Assistant && message.tool_calls.is_empty())
        .and_then(|message| message.content.as_deref())
}

fn report(error: &anyhow::Error) {
    eprintln!(
        "\n{} {}\n",
        style("!").yellow(),
        ui::safe(&format!("{error:#}"))
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn restore_never_presents_an_older_answer_as_the_pending_turn() {
        let completed = Message::text(Role::Assistant, "old clarification");
        assert_eq!(
            restored_answer(std::slice::from_ref(&completed)),
            Some("old clarification")
        );

        let pending_turn = vec![
            Message::text(Role::User, "first request"),
            completed,
            Message::text(Role::User, "current instruction"),
        ];
        assert_eq!(restored_answer(&pending_turn), None);
    }
}
