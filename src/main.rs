use builder_core::protocol::{Message, Role};
mod cli;
use anyhow::{Context, Result, ensure};
use builder::{
    agent::{Agent, ApprovalMode, SYSTEM, estimate_tokens, pending},
    ui,
};
use builder_core::{
    config::{self, Config, Profile},
    store::Store,
};
use builder_provider::OpenAiCompatible;
use builder_tools::Workspace;
use clap::Parser;
use cli::{Cli, Command, ConfigCommand, MemoryCommand, PipelineCommand};
use console::style;
use std::io::{self, IsTerminal, Read};

#[tokio::main]
async fn main() {
    if let Err(error) = app(Cli::parse()).await {
        eprintln!(
            "\n{} {}",
            style("error:").red().bold(),
            ui::safe(&format!("{error:#}"))
        );
        std::process::exit(1);
    }
}

async fn app(cli: Cli) -> Result<()> {
    let home = config::home(cli.home.clone())?;
    let config_home = config::location::directory(cli.home.as_deref())?;
    if config::location::migrate_legacy(&home, &config_home)? {
        eprintln!(
            "Configuration copied to {}. Existing data and the original config remain in {}.",
            config_home.join("config.toml").display(),
            home.display()
        );
    }
    let mut config = Config::load(&config_home)?;
    if let Some(Command::Remote { listen, origin }) = &cli.command {
        ensure!(
            cli.pipeline.is_empty(),
            "Save pipeline settings with config pipeline set before starting remote control"
        );
        let remote = builder::remote::RemoteControl::new(builder::remote::RemoteOptions {
            home,
            config_home,
            workspace: cli.workspace.clone(),
            profile: cli.profile.clone(),
            approval: cli.approval_mode(),
            max_rounds: cli.max_rounds.map(usize::from),
            origin: origin.clone().unwrap_or_else(|| format!("http://{listen}")),
        })?;
        let listener = tokio::net::TcpListener::bind(listen).await?;
        eprintln!(
            "Remote control: http://{}\nWorkspace: {}\nToken file: {}\nKeep this process running. Use HTTPS at your public reverse proxy.",
            listener.local_addr()?,
            remote.workspace().display(),
            remote.token_path().display()
        );
        axum::serve(listener, remote.router())
            .with_graceful_shutdown(async {
                let _ = tokio::signal::ctrl_c().await;
            })
            .await?;
        remote.shutdown().await;
        return Ok(());
    }
    if !cli.pipeline.is_empty() {
        ensure!(
            matches!(
                &cli.command,
                None | Some(
                    Command::Chat { .. }
                        | Command::Run { .. }
                        | Command::Resume { .. }
                        | Command::Doctor
                        | Command::Models
                        | Command::Config {
                            command: ConfigCommand::Pipeline {
                                command: PipelineCommand::Show
                                    | PipelineCommand::Set { .. }
                                    | PipelineCommand::Reset
                            }
                        }
                )
            ),
            "Transient --pipeline overrides apply to chat/run/resume, doctor/models, or config pipeline show. Use config pipeline set to save changes."
        );
    }

    if let Some(Command::Config { command }) = &cli.command {
        match command {
            ConfigCommand::Pipeline { command } => {
                let name = cli
                    .profile
                    .as_deref()
                    .unwrap_or(&config.default_profile)
                    .to_owned();
                let mut profile = config
                    .profiles
                    .get(&name)
                    .context("Profile not found")?
                    .clone();
                match command {
                    PipelineCommand::Show => {
                        let effective = profile.pipeline.updated(&cli.pipeline)?;
                        println!(
                            "Profile: {}\n{}",
                            ui::safe(&name),
                            toml::to_string_pretty(&effective)?
                        );
                        let mut available = effective.clone();
                        available.enabled &= profile.tools;
                        available.procedures &= config.memory.enabled;
                        println!(
                            "Available research operations: {}",
                            available.operations().join(", ")
                        );
                        println!(
                            "Profile tools enabled: {}; memory enabled: {}",
                            profile.tools, config.memory.enabled
                        );
                        println!("Transient overrides: {}", !cli.pipeline.is_empty());
                    }
                    PipelineCommand::Set { settings } => {
                        ensure!(
                            cli.pipeline.is_empty(),
                            "Use config pipeline set KEY=VALUE without transient --pipeline overrides"
                        );
                        profile.pipeline = profile.pipeline.updated(settings)?;
                        profile.validate()?;
                        config.profiles.insert(name.clone(), profile);
                        config.save(&config_home)?;
                        println!("Updated pipeline for {}", ui::safe(&name));
                    }
                    PipelineCommand::Reset => {
                        ensure!(
                            cli.pipeline.is_empty(),
                            "Reset does not accept transient --pipeline overrides"
                        );
                        profile.pipeline = Default::default();
                        profile.validate()?;
                        config.profiles.insert(name.clone(), profile);
                        config.save(&config_home)?;
                        println!("Restored pipeline defaults for {}", ui::safe(&name));
                    }
                }
            }
            ConfigCommand::Init => {
                if !config_home.join("config.toml").exists() {
                    config.save(&config_home)?;
                }
                println!("{}", config_home.join("config.toml").display());
            }
            ConfigCommand::Show => println!("{}", toml::to_string_pretty(&config.redacted())?),
            ConfigCommand::Import {
                path,
                format,
                replace,
                activate,
            } => {
                let imported = config::import::read(path, (*format).into())?;
                let names: Vec<_> = imported.config.profiles.keys().cloned().collect();
                let warnings = imported.merge(&mut config, *replace, *activate)?;
                config.save(&config_home)?;
                for name in names {
                    println!("Imported profile: {}", ui::safe(&name));
                }
                for warning in warnings {
                    eprintln!("{}", ui::safe(&warning));
                }
                println!("Default profile: {}", ui::safe(&config.default_profile));
            }
            ConfigCommand::Model { model } => {
                let (name, mut profile) = config.profile(cli.profile.as_deref())?;
                profile.model = model.clone();
                profile.validate()?;
                config.profiles.insert(name.clone(), profile);
                config.save(&config_home)?;
                println!("Updated {} model: {}", ui::safe(&name), ui::safe(model));
            }
            ConfigCommand::Use { name } => {
                config.profile(Some(name))?;
                config.default_profile = name.clone();
                config.save(&config_home)?;
                println!("Default profile: {}", ui::safe(name));
            }
            ConfigCommand::Add {
                name,
                base_url,
                model,
                api_key_env,
                no_stream,
                no_tools,
                context_tokens,
                max_output_tokens,
            } => {
                ensure!(!name.trim().is_empty(), "Profile name cannot be empty");
                let profile = Profile {
                    base_url: base_url.clone(),
                    model: model.clone(),
                    api_key_env: api_key_env.clone(),
                    stream: !no_stream,
                    tools: !no_tools,
                    context_tokens: *context_tokens,
                    max_output_tokens: *max_output_tokens,
                    ..Profile::default()
                };
                profile.validate()?;
                config.profiles.insert(name.clone(), profile);
                config.save(&config_home)?;
                println!("Saved profile {}", ui::safe(name));
            }
        }
        return Ok(());
    }
    let mut store = Store::open(&home)?;
    match &cli.command {
        Some(Command::Sessions) => {
            let sessions = store.sessions()?;
            if sessions.is_empty() {
                println!("No sessions yet. Run builder to start one.");
            }
            for session in sessions {
                println!(
                    "{}  {:<12}  {}  {}",
                    &session.id[..8],
                    ui::safe(&session.profile),
                    &session.updated_at[..19],
                    ui::safe(&session.title)
                );
            }
            return Ok(());
        }
        Some(Command::Export { session, json }) => {
            let session = store.resolve(session)?;
            let messages = store.history_messages(&session.id)?;
            if *json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(
                        &serde_json::json!({"session":session,"messages":messages})
                    )?
                );
            } else {
                println!("# {}\n", ui::safe(&session.title));
                for message in messages {
                    println!(
                        "## {}\n\n{}\n",
                        message.role,
                        ui::safe(message.content.as_deref().unwrap_or(""))
                    );
                    for call in message.tool_calls {
                        println!(
                            "Tool: {}\n\n```json\n{}\n```\n",
                            ui::safe(&call.function.name),
                            ui::safe(&call.function.arguments)
                        );
                    }
                }
            }
            return Ok(());
        }
        _ => {}
    }
    if let Some(Command::Memory { command }) = &cli.command {
        use builder::memory::MemoryRuntime;
        use builder_core::memory::{Memory, MemoryKind};
        let workspace = Workspace::new(&cli.workspace)?;
        let scope = MemoryRuntime::scope(&workspace);
        let result = match command {
            MemoryCommand::Enable {
                local: _,
                lexical,
                embedding_profile,
                query_prefix,
                document_prefix,
                embedding_revision,
            } => {
                let previous = config.memory.clone();
                config.memory.enabled = true;
                if let Some(value) = query_prefix {
                    config.memory.query_prefix = value.clone();
                }
                if let Some(value) = document_prefix {
                    config.memory.document_prefix = value.clone();
                }
                if let Some(value) = embedding_revision {
                    config.memory.embedding_revision = value.clone();
                }
                if *lexical {
                    config.memory.embedding_backend = config::EmbeddingBackend::Lexical;
                } else if let Some(name) = embedding_profile {
                    config.memory.embedding_backend = config::EmbeddingBackend::Remote;
                    config.memory.embedding_profile = Some(name.clone());
                } else {
                    setup_local_memory(&mut config, &home).await?;
                }
                if let Err(error) = MemoryRuntime::from_config(&config) {
                    config.memory = previous;
                    return Err(error);
                }
                let mut latest = Config::load(&config_home)?;
                ensure!(
                    latest.memory == previous,
                    "Memory settings changed during setup; reopen /memory before saving"
                );
                latest.memory = config.memory.clone();
                latest.save(&config_home)?;
                config = latest;
                serde_json::json!({"enabled":true,"backend":config.memory.embedding_backend,"local_model_dir":config.memory.local_model_dir,"note":"Restart running sessions to load memory settings"})
            }
            MemoryCommand::Disable => {
                config.memory.enabled = false;
                config.save(&config_home)?;
                serde_json::json!({"enabled":false})
            }
            MemoryCommand::Status => {
                serde_json::json!({"settings":config.memory,"checkout_records":store.memory_list(&scope)?.len(),"user_preferences":store.memory_list("@user")?.len(),"storage":"SQLite; revisions retained; memory never grants permissions"})
            }
            MemoryCommand::List { user } => {
                serde_json::json!(store.memory_list(if *user { "@user" } else { &scope })?)
            }
            MemoryCommand::Get {
                key,
                revision,
                user,
            } => serde_json::json!(store.memory_get(
                if *user { "@user" } else { &scope },
                key,
                *revision
            )?),
            MemoryCommand::Remember { key, text } => {
                let previous = store.memory_get("@user", key, None)?;
                serde_json::json!(store.memory_put(
                    "@user",
                    previous.map_or(0, |m| m.revision),
                    Memory {
                        key: key.clone(),
                        revision: 0,
                        kind: MemoryKind::Preference,
                        text: text.clone(),
                        evidence: vec![],
                        origin_session: String::new(),
                        origin_seq: 0,
                        created_at: String::new()
                    }
                )?)
            }
            MemoryCommand::Forget { key, user } => {
                let scope = if *user { "@user" } else { &scope };
                let memory = store
                    .memory_get(scope, key, None)?
                    .context("Memory not found")?;
                store.memory_forget(scope, key, memory.revision)?;
                serde_json::json!({"forgotten":key,"note":"Original transcript and revision audit remain"})
            }
            MemoryCommand::Task { session } => {
                serde_json::json!(store.memory_task(&store.resolve(session)?.id)?)
            }
            MemoryCommand::Search { query } => {
                let runtime =
                    MemoryRuntime::from_config(&config)?.unwrap_or_else(MemoryRuntime::lexical);
                runtime.search(&store, &workspace, query).await?
            }
            MemoryCommand::Index => {
                let runtime =
                    MemoryRuntime::from_config(&config)?.context("Enable memory first")?;
                runtime.index_pending(&mut store, &workspace).await?;
                serde_json::json!({"processed":"up to two missing vectors; repeat to drain queue"})
            }
        };
        println!("{}", ui::safe(&serde_json::to_string_pretty(&result)?));
        return Ok(());
    }
    let resume = match &cli.command {
        Some(Command::Resume { session }) => Some(match session {
            Some(id) => store.resolve(id)?,
            None => store
                .sessions()?
                .into_iter()
                .next()
                .context("No saved sessions")?,
        }),
        Some(Command::Run {
            session: Some(id), ..
        }) => Some(store.resolve(id)?),
        _ => None,
    };
    let profile_name = cli
        .profile
        .as_deref()
        .or_else(|| resume.as_ref().map(|s| s.profile.as_str()));
    let (profile_name, mut profile) = config.profile(profile_name)?;
    profile.pipeline = profile.pipeline.updated(&cli.pipeline)?;
    profile.validate()?;
    let provider = OpenAiCompatible::new(profile.clone())?;
    if matches!(cli.command, Some(Command::Models | Command::Doctor)) {
        if matches!(cli.command, Some(Command::Doctor)) {
            println!(
                "✓ configuration valid\n✓ durable storage: {}\n✓ profile: {}\n  endpoint: {}",
                home.display(),
                ui::safe(&profile_name),
                profile.base_url
            );
        }
        let models = provider.models().await.context("Model discovery failed. Check the URL (usually ending in /v1), credentials, and whether the server exposes GET /models")?;
        if let Some(models) = models["data"].as_array() {
            for model in models {
                if let Some(id) = model["id"].as_str() {
                    println!("{}", ui::safe(id));
                }
            }
            if matches!(cli.command, Some(Command::Doctor)) {
                if models
                    .iter()
                    .any(|m| m["id"].as_str() == Some(&profile.model))
                {
                    println!("✓ configured model is advertised");
                } else {
                    println!(
                        "! configured model '{}' was not advertised; verify its ID",
                        ui::safe(&profile.model)
                    );
                }
                println!("Discovery does not verify generation or tool-call support.");
            }
        } else {
            println!("{}", ui::safe(&serde_json::to_string_pretty(&models)?));
        }
        return Ok(());
    }
    ensure!(
        profile.supports_chat(),
        "This profile is for embedding/autocomplete/reranking; select a chat model with --profile"
    );
    let workspace = Workspace::new(
        resume
            .as_ref()
            .map(|s| s.workspace.as_path())
            .unwrap_or(&cli.workspace),
    )?;
    let initial_prompt = match &cli.command {
        Some(Command::Run { prompt, .. })
        | Some(Command::Chat {
            prompt: Some(prompt),
        }) => Some(if prompt == "-" {
            let mut text = String::new();
            io::stdin().read_to_string(&mut text)?;
            text
        } else {
            prompt.clone()
        }),
        _ => None,
    };
    let session = if let Some(session) = resume {
        session.id
    } else {
        let mut system = format!("{SYSTEM}\n\nWorkspace: {}", workspace.root().display());
        let instructions = workspace.root().join("AGENTS.md");
        if instructions.is_file() {
            let content = std::fs::read_to_string(instructions)?;
            ensure!(
                content.len() <= 64 * 1024,
                "Workspace AGENTS.md exceeds 64 KiB"
            );
            system.push_str(&format!(
                "\n\nWorkspace instructions (AGENTS.md):\n{content}"
            ));
        }
        store.create(
            initial_prompt.as_deref().unwrap_or("Interactive session"),
            &profile_name,
            workspace.root(),
            &system,
        )?
    };
    let _guard = store.lock(&session)?;
    let interactive = !matches!(cli.command, Some(Command::Run { .. }));
    if interactive {
        ensure!(
            io::stdin().is_terminal(),
            "Interactive mode requires a terminal. Use builder run - for piped input."
        );
    }
    let max_rounds = cli
        .max_rounds
        .map(usize::from)
        .unwrap_or(profile.pipeline.max_rounds);
    let mut agent = Agent {
        memory: builder::memory::MemoryRuntime::from_config(&config)?,
        provider,
        profile,
        workspace,
        session,
        approval: cli.approval_mode(),
        max_rounds,
    };
    if !interactive {
        agent.submit(
            &mut store,
            initial_prompt.as_deref().context("Missing prompt")?,
        )?;
        drive(&agent, &mut store, false).await?;
        if let Some(message) = store.messages(&agent.session)?.last() {
            println!("{}", ui::safe(message.content.as_deref().unwrap_or("")));
        }
        eprintln!("Session saved: {}", &agent.session[..8]);
        return Ok(());
    }
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
    }
    if pending(&existing) {
        eprintln!("  Paused turn restored. Send a follow-up, /retry, /cancel, or /rewind.");
    }
    let mut memory_task = None;
    if let Some(prompt) = initial_prompt {
        agent.submit(&mut store, &prompt)?;
        if let Err(error) =
            drive_foreground(&agent, &mut store, true, &home, &config, &mut memory_task).await
        {
            report(&error);
        }
    }
    let mut editor = builder::input::Composer::default();
    if let Some(draft) = store.composer_draft(&agent.session)? {
        editor.set_draft(&draft)?;
        eprintln!("  Previous message restored as a draft; Enter sends it.");
    }
    let plain = cli.plain
        || std::env::var("TERM").is_ok_and(|term| term == "dumb")
        || !io::stdout().is_terminal();
    loop {
        let messages = store.messages(&agent.session)?;
        let used = estimate_tokens(&messages);
        let percent = used.saturating_mul(100) / agent.profile.context_tokens;
        let status = format!(
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
        );
        let input = if plain {
            editor.read_plain()?
        } else {
            editor.read(&status)?
        };
        let builder::input::Input::Submit(prompt) = input else {
            break;
        };
        let line = prompt.trim();
        if line.is_empty() {
            continue;
        }
        match line {
            "/exit" | "/quit" => break,
            "/help" => println!(
                "\n  /status         Session, context estimate, and recovery state\n  /settings       Open pipeline settings menu\n  /memory         Local memory settings and model setup\n  /history        Show active conversation (/history archived shows rewound turns)\n  /cancel         End pending work; keep completed context\n  /rewind         Archive the last turn and edit its user message (files stay changed)\n  /retry          Continue an unfinished turn\n  /compact        Summarize context now; preserve originals\n  /attach PATH    Send a workspace file into the conversation\n  /exit           Save and leave\n\nEdits and commands require approval unless --auto or --approval trust is set.\nEnter sends · Alt+Enter / Ctrl+J inserts a new line\nCtrl+V pastes directly from clipboard (macOS)\nCtrl+Z undo · Ctrl+Y redo · Ctrl+W delete word · Ctrl+U clear\n↑/↓ navigate lines (history when empty) · Ctrl+P/N history\n/ opens commands · ↑/↓ browse · Tab or Enter completes · Enter runs · Esc closes\nLarge pastes fold into blocks; their full text is sent on Enter.\nCtrl+C during a response pauses it; send a follow-up to change direction.\nAll messages are saved automatically. Rewound turns remain archived.\n"
            ),
            "/memory" => {
                stop_memory_task(&mut memory_task).await;
                println!(
                    "\nMemory settings\n1. Local embeddings (one-time ~91 MB model download; then offline)\n2. Keyword-only memory (no embedding model)\n3. Disable memory (retain stored records)\nEnter to cancel"
                );
                let choice = if plain {
                    editor.read_plain()?
                } else {
                    editor.read("Memory settings · enter 1, 2 or 3")?
                };
                if let builder::input::Input::Submit(choice) = choice {
                    let update = async {
                        let mut latest = Config::load(&config_home)?;
                        let baseline = latest.memory.clone();
                        match choice.trim() {
                            "1" => setup_local_memory(&mut latest, &home).await?,
                            "2" => {
                                latest.memory.enabled = true;
                                latest.memory.embedding_backend = config::EmbeddingBackend::Lexical;
                            }
                            "3" => latest.memory.enabled = false,
                            "" => return Ok::<_, anyhow::Error>(()),
                            _ => anyhow::bail!("Choose 1, 2 or 3; memory settings unchanged"),
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
                        Ok(())
                    }
                    .await;
                    if let Err(error) = update {
                        report(&error);
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
                println!(
                    "\nSession: {}\nMessages: {}\nEstimated history tokens: {} / {} ({} reserved for output; tool schemas extra)\nState: {}\nStorage: {}\nModel: {}\nAgent rounds per run: {}\nProgress guard: focus after {} tool calls; tool-free conclusion at {}; full context preserved\nFailure guard: recover after {} consecutive failures; {} recovery rounds\nTool limits: {} per response; {} completed identical shell calls\nCompaction: {}\n",
                    agent.session,
                    messages.len(),
                    estimate_tokens(&messages),
                    agent.profile.context_tokens,
                    agent.profile.max_output_tokens,
                    if pending(&messages) {
                        "paused; send a follow-up, /retry, /cancel, or /rewind"
                    } else {
                        "ready"
                    },
                    home.display(),
                    ui::safe(&agent.profile.model),
                    agent.max_rounds,
                    agent.profile.pipeline.progress_check_calls,
                    agent.profile.pipeline.max_no_progress_calls(),
                    agent.profile.pipeline.failure_check_calls,
                    agent.profile.pipeline.failure_recovery_rounds,
                    agent.profile.pipeline.tool_calls_per_response,
                    agent.profile.pipeline.identical_shell_calls,
                    if agent.profile.auto_compact {
                        format!(
                            "compact at {}% · originals retained",
                            agent.profile.compact_at_percent
                        )
                    } else {
                        "auto-compact off".into()
                    }
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
                let mut renderer = ui::Renderer::new(true);
                let result = {
                    let mut emit = |event| renderer.event(event);
                    tokio::select! {
                        result = agent.compact(&mut store, &mut emit) => result,
                        signal = tokio::signal::ctrl_c() => {
                            signal?;
                            Err(anyhow::anyhow!("Compaction interrupted; original context is intact."))
                        }
                    }
                };
                renderer.finish();
                match result {
                    Ok(false) => println!(
                        "No older context to compact; latest prompt and recent exchanges are retained."
                    ),
                    Ok(true) => {}
                    Err(error) => {
                        store.interrupt_attempts(&agent.session)?;
                        report(&error);
                    }
                }
            }
            "/retry" => {
                if let Err(error) =
                    drive_foreground(&agent, &mut store, true, &home, &config, &mut memory_task)
                        .await
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
                        match agent.submit(&mut store, &text) {
                            Ok(()) => {
                                if let Err(error) = drive_foreground(
                                    &agent,
                                    &mut store,
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
                match agent.submit(&mut store, &prompt) {
                    Ok(()) => {
                        if let Err(error) = drive_foreground(
                            &agent,
                            &mut store,
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

struct MemoryTask {
    cancel: Option<tokio::sync::oneshot::Sender<()>>,
    stopped: tokio::sync::oneshot::Receiver<()>,
}

async fn stop_memory_task(task: &mut Option<MemoryTask>) {
    if let Some(mut task) = task.take() {
        if let Some(cancel) = task.cancel.take() {
            let _ = cancel.send(());
        }
        // Cancellation is normally immediate because network and embedding
        // work is awaited. Never make foreground chat wait on maintenance if
        // the worker happens to be inside a synchronous SQLite operation.
        let _ =
            tokio::time::timeout(std::time::Duration::from_millis(100), &mut task.stopped).await;
    }
}

fn start_memory_task(
    agent: &Agent<OpenAiCompatible>,
    home: &std::path::Path,
    config: &Config,
) -> Option<MemoryTask> {
    agent.memory.as_ref()?;
    let home = home.to_path_buf();
    let config = config.clone();
    let profile = agent.profile.clone();
    let workspace = agent.workspace.root().to_path_buf();
    let session = agent.session.clone();
    let (cancel, cancelled) = tokio::sync::oneshot::channel();
    let (stopped, stopped_rx) = tokio::sync::oneshot::channel();
    std::thread::Builder::new()
        .name("builder-memory".into())
        .spawn(move || {
            let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            else {
                let _ = stopped.send(());
                return;
            };
            runtime.block_on(async move {
                let Ok(Some(memory)) = builder::memory::MemoryRuntime::from_config(&config) else {
                    return;
                };
                let Ok(provider) = OpenAiCompatible::new(profile.clone()) else {
                    return;
                };
                let Ok(workspace) = Workspace::new(&workspace) else {
                    return;
                };
                let Ok(mut store) = Store::open(&home) else {
                    return;
                };
                tokio::select! {
                    _ = cancelled => {}
                    _ = memory.maintain(
                        &provider,
                        &mut store,
                        &session,
                        &workspace,
                        profile.context_tokens,
                    ) => {}
                }
            });
            runtime.shutdown_background();
            let _ = stopped.send(());
        })
        .ok()?;
    Some(MemoryTask {
        cancel: Some(cancel),
        stopped: stopped_rx,
    })
}

async fn drive_foreground(
    agent: &Agent<OpenAiCompatible>,
    store: &mut Store,
    interactive: bool,
    home: &std::path::Path,
    config: &Config,
    memory_task: &mut Option<MemoryTask>,
) -> Result<()> {
    stop_memory_task(memory_task).await;
    let result = drive(agent, store, interactive).await;
    if store
        .messages(&agent.session)
        .is_ok_and(|messages| !pending(&messages))
    {
        *memory_task = start_memory_task(agent, home, config);
    }
    result
}

async fn drive(
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
    result
}
fn report(error: &anyhow::Error) {
    eprintln!(
        "\n{} {}\n",
        style("!").yellow(),
        ui::safe(&format!("{error:#}"))
    );
}

async fn setup_local_memory(config: &mut Config, home: &std::path::Path) -> Result<()> {
    let directory = home.join("models").join("all-MiniLM-L6-v2");
    eprintln!("Preparing local embeddings (~91 MB download on first setup)…");
    tokio::time::timeout(
        std::time::Duration::from_secs(300),
        builder_provider::local_embedding::install(&directory),
    )
    .await
    .context("Local model setup deadline exceeded; rerun to resume completed files")??;
    let local = builder_provider::local_embedding::LocalEmbedding::new(Some(directory.clone()));
    tokio::time::timeout(
        std::time::Duration::from_secs(30),
        local.embed("Local embedding readiness check"),
    )
    .await
    .context("Local model readiness deadline exceeded")??;
    config.memory.enabled = true;
    config.memory.embedding_backend = config::EmbeddingBackend::Local;
    config.memory.local_model_dir = Some(directory);
    config.memory.embedding_profile = None;
    config.memory.query_prefix.clear();
    config.memory.document_prefix.clear();
    Ok(())
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
