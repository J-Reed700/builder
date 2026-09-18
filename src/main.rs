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
use cli::{Cli, Command, ConfigCommand, MemoryCommand, PipelineCommand, RemoteCommand};
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
    if let Some(Command::Remote {
        command,
        listen,
        origin,
    }) = &cli.command
    {
        ensure!(
            cli.pipeline.is_empty(),
            "Save pipeline settings with config pipeline set before starting remote control"
        );
        let public_origin = match command {
            Some(RemoteCommand::Connect { gateway, .. }) => gateway.clone(),
            None => origin.clone().unwrap_or_else(|| format!("http://{listen}")),
        };
        let remote = builder::remote::RemoteControl::new(builder::remote::RemoteOptions {
            home,
            config_home,
            workspace: cli.workspace.clone(),
            profile: cli.profile.clone(),
            approval: cli.approval_mode(),
            max_rounds: cli.max_rounds.map(usize::from),
            origin: public_origin,
        })?;
        match command {
            Some(RemoteCommand::Connect {
                gateway,
                code,
                pair_only,
            }) => {
                eprintln!("Workspace root: {}", remote.workspace().display());
                let result = tokio::select! {
                    result = builder::remote_connect::serve(
                        &remote,
                        builder::remote_connect::ConnectOptions {
                            gateway: gateway.clone(),
                            code: code.clone(),
                            home: remote.token_path().parent().unwrap().to_owned(),
                            pair_only: *pair_only,
                        },
                    ) => result,
                    _ = tokio::signal::ctrl_c() => Ok(()),
                };
                remote.shutdown().await;
                result?;
            }
            None => {
                let listener = tokio::net::TcpListener::bind(listen).await?;
                eprintln!(
                    "Remote control: http://{}\nWorkspace root: {}\nToken file: {}\nKeep this process running. Use HTTPS at your public reverse proxy.",
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
            }
        }
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
            MemoryCommand::Refresh { session } => {
                let saved = store.resolve(session)?;
                let _guard = store.lock(&saved.id)?;
                let workspace = Workspace::new(std::path::Path::new(&saved.workspace))?;
                let (_, profile) =
                    config.profile(cli.profile.as_deref().or(Some(&saved.profile)))?;
                let provider = MemoryRuntime::extraction_provider(&profile)?;
                let runtime =
                    MemoryRuntime::from_config(&config)?.context("Enable memory first")?;
                let extracted = runtime
                    .extract(
                        &provider,
                        &mut store,
                        &saved.id,
                        &workspace,
                        true,
                        (profile.context_tokens, &mut |_| {}),
                    )
                    .await?;
                runtime.index_pending(&mut store, &workspace).await?;
                serde_json::json!({"session":saved.id,"workspace":workspace.root(),"extracted":extracted,
                    "checkout_records":store.memory_list(&MemoryRuntime::scope(&workspace))?.len()})
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
                println!("Running active generation, JSON, and tool-call probes…");
                let conformance = builder::doctor::conformance(&provider, profile.tools).await;
                let conformance_value = serde_json::to_value(&conformance)?;
                let conformance_identity = serde_json::to_vec(&(
                    &profile.base_url,
                    &profile.model,
                    profile.stream,
                    profile.tools,
                    profile.context_tokens,
                    profile.max_output_tokens,
                    profile.max_attempts,
                    profile.connect_timeout_secs,
                    profile.idle_timeout_secs,
                    profile.request_timeout_secs,
                    &profile.completion,
                ))?;
                store.save_provider_conformance(
                    &builder_core::memory::digest(&conformance_identity),
                    &profile_name,
                    &profile.base_url,
                    &profile.model,
                    &conformance_value,
                )?;
                println!(
                    "Active conformance (saved by endpoint/model fingerprint):\n{}",
                    ui::safe(&serde_json::to_string_pretty(&conformance_value)?)
                );
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
        if memory_task.is_none() && !pending(&messages) {
            memory_task = start_memory_task(&agent, &home, &config);
        }
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
            "/help" => println!("\n{}", ui::help(ui::panel::width())),
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
                                    setup_local_memory(&mut latest, &home).await?
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
                let index_status = builder::code_index::status(&store, &agent.workspace)?;
                let coverage =
                    builder::code_index::coverage(&store, &agent.workspace, agent.memory.as_ref())?;
                let query_summary = builder::code_index::query_summary(&store, &agent.workspace)?;
                let update_mode = memory_task.as_ref().map_or_else(
                    || configured_index_update_mode(&agent.profile),
                    MemoryTask::index_update_description,
                );
                let memory_status = memory_task
                    .as_ref()
                    .and_then(|task| task.memory_status.lock().ok().map(|s| s.clone()))
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
    index_updates: std::sync::Arc<std::sync::Mutex<IndexUpdateState>>,
    memory_status: std::sync::Arc<std::sync::Mutex<String>>,
}

#[derive(Debug, Clone)]
enum IndexUpdateState {
    Starting,
    Watching,
    PeriodicOnly,
    WatchUnavailable(String),
}

impl MemoryTask {
    fn index_update_description(&self) -> String {
        let state = self
            .index_updates
            .lock()
            .map(|state| state.clone())
            .unwrap_or_else(|_| {
                IndexUpdateState::WatchUnavailable("worker state unavailable".into())
            });
        match state {
            IndexUpdateState::Starting => "background index worker starting".into(),
            IndexUpdateState::Watching => {
                "live filesystem watch + periodic full-scan fallback".into()
            }
            IndexUpdateState::PeriodicOnly => "periodic full scans (watch disabled)".into(),
            IndexUpdateState::WatchUnavailable(error) => {
                format!("periodic full-scan fallback (watch unavailable: {error})")
            }
        }
    }
}

fn configured_index_update_mode(profile: &Profile) -> String {
    if !profile.pipeline.code_index_background {
        return "foreground search refresh only".into();
    }
    if profile.pipeline.code_index_watch {
        format!(
            "watch configured at {}ms + full scan {}s; worker idle or paused",
            profile.pipeline.code_index_debounce_ms, profile.pipeline.code_index_refresh_secs
        )
    } else {
        format!(
            "full scan configured every {}s; worker idle or paused",
            profile.pipeline.code_index_refresh_secs
        )
    }
}

fn start_code_watch(
    root: &std::path::Path,
    state: &std::sync::Arc<std::sync::Mutex<IndexUpdateState>>,
) -> Option<builder::code_index::CodeIndexWatch> {
    match builder::code_index::CodeIndexWatch::new(root) {
        Ok(watch) => {
            if let Ok(mut state) = state.lock() {
                *state = IndexUpdateState::Watching;
            }
            Some(watch)
        }
        Err(error) => {
            let mut text = error.to_string().replace(['\r', '\n'], " ");
            text.truncate(text.floor_char_boundary(160));
            if let Ok(mut state) = state.lock() {
                *state = IndexUpdateState::WatchUnavailable(text);
            }
            None
        }
    }
}

async fn stop_memory_task(task: &mut Option<MemoryTask>) {
    if let Some(mut running) = task.take() {
        if let Some(cancel) = running.cancel.take() {
            let _ = cancel.send(());
        }
        // Cancellation is normally immediate because network and embedding
        // work is awaited. If synchronous capture or SQLite work is still in
        // flight, retain ownership so a replacement worker cannot overlap it.
        if tokio::time::timeout(std::time::Duration::from_millis(100), &mut running.stopped)
            .await
            .is_err()
        {
            *task = Some(running);
        }
    }
}

fn reap_memory_task(task: &mut Option<MemoryTask>) {
    let finished = task.as_mut().is_some_and(|running| {
        !matches!(
            running.stopped.try_recv(),
            Err(tokio::sync::oneshot::error::TryRecvError::Empty)
        )
    });
    if finished {
        task.take();
    }
}

fn start_memory_task(
    agent: &Agent<OpenAiCompatible>,
    home: &std::path::Path,
    config: &Config,
) -> Option<MemoryTask> {
    if agent.memory.is_none()
        && !(agent.profile.pipeline.code_index && agent.profile.pipeline.code_index_background)
    {
        return None;
    }
    let home = home.to_path_buf();
    let config = config.clone();
    let profile = agent.profile.clone();
    let workspace = agent.workspace.root().to_path_buf();
    let session = agent.session.clone();
    let (cancel, cancelled) = tokio::sync::oneshot::channel();
    let (stopped, stopped_rx) = tokio::sync::oneshot::channel();
    let index_updates = std::sync::Arc::new(std::sync::Mutex::new(
        if profile.pipeline.code_index && profile.pipeline.code_index_background {
            IndexUpdateState::Starting
        } else {
            IndexUpdateState::PeriodicOnly
        },
    ));
    let worker_index_updates = index_updates.clone();
    let memory_status = std::sync::Arc::new(std::sync::Mutex::new(
        "waiting for idle maintenance".to_string(),
    ));
    let worker_memory_status = memory_status.clone();
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
                let setup = (|| -> Result<_> {
                    Ok((
                        builder::memory::MemoryRuntime::from_config(&config)?,
                        builder::memory::MemoryRuntime::extraction_provider(&profile)?,
                        Workspace::new(&workspace)?,
                        Store::open(&home)?,
                    ))
                })();
                let (memory, provider, workspace, mut store) = match setup {
                    Ok(setup) => setup,
                    Err(error) => {
                        if let Ok(mut status) = worker_memory_status.lock() {
                            *status = format!("maintenance could not start: {error}");
                        }
                        return;
                    }
                };
                let maintenance = async {
                    let mut code_watch = if profile.pipeline.code_index
                        && profile.pipeline.code_index_background
                        && profile.pipeline.code_index_watch
                    {
                        start_code_watch(workspace.root(), &worker_index_updates)
                    } else {
                        if let Ok(mut state) = worker_index_updates.lock() {
                            *state = IndexUpdateState::PeriodicOnly;
                        }
                        None
                    };
                    // Separate connections let extraction retry
                    // while a large code-vector queue is still being drained.
                    let memory_work = async {
                        let Some(memory) = &memory else {
                            if let Ok(mut status) = worker_memory_status.lock() {
                                *status = "disabled".into();
                            }
                            return;
                        };
                        loop {
                            let result = memory
                                .maintain(
                                    &provider,
                                    &mut store,
                                    &session,
                                    &workspace,
                                    profile.context_tokens,
                                )
                                .await;
                            if let Ok(mut status) = worker_memory_status.lock() {
                                *status = match result {
                                    Ok(()) => "idle pass complete; no extraction error".into(),
                                    Err(error) => {
                                        format!("maintenance failed; will retry: {error}")
                                    }
                                };
                            }
                            tokio::time::sleep(std::time::Duration::from_secs(30)).await;
                        }
                    };
                    let code_work = async {
                        if !profile.pipeline.code_index || !profile.pipeline.code_index_background {
                            return;
                        }
                        let Ok(mut code_store) = Store::open(&home) else {
                            return;
                        };
                        loop {
                            let _ = builder::code_index::maintain(
                                &mut code_store,
                                &workspace,
                                memory.as_ref(),
                                &profile.pipeline,
                                code_watch.as_mut(),
                            )
                            .await;
                            if let Some(watch) = &mut code_watch {
                                let _ = watch
                                    .wait(
                                        profile.pipeline.code_index_refresh_secs,
                                        profile.pipeline.code_index_debounce_ms,
                                    )
                                    .await;
                            } else {
                                tokio::time::sleep(std::time::Duration::from_secs(
                                    profile.pipeline.code_index_refresh_secs,
                                ))
                                .await;
                            }
                        }
                    };
                    tokio::join!(biased; memory_work, code_work);
                };
                tokio::select! { _ = cancelled => {}, _ = maintenance => {} }
            });
            runtime.shutdown_background();
            let _ = stopped.send(());
        })
        .ok()?;
    Some(MemoryTask {
        cancel: Some(cancel),
        stopped: stopped_rx,
        index_updates,
        memory_status,
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

    #[test]
    fn unavailable_filesystem_watch_is_reported_as_periodic_fallback() {
        let root = tempfile::tempdir().unwrap();
        let missing = root.path().join("removed");
        let state = std::sync::Arc::new(std::sync::Mutex::new(IndexUpdateState::Starting));
        assert!(start_code_watch(&missing, &state).is_none());
        let state = state.lock().unwrap().clone();
        assert!(matches!(state, IndexUpdateState::WatchUnavailable(_)));

        let (_cancel, stopped) = tokio::sync::oneshot::channel();
        let task = MemoryTask {
            memory_status: Default::default(),
            cancel: None,
            stopped,
            index_updates: std::sync::Arc::new(std::sync::Mutex::new(state)),
        };
        let description = task.index_update_description();
        assert!(description.contains("periodic full-scan fallback"));
        assert!(description.contains("watch unavailable"));
    }

    #[tokio::test]
    async fn idle_worker_extracts_while_code_embeddings_are_stalled() {
        use axum::{Json, Router, routing::post};
        use builder_core::{
            config::{EmbeddingBackend, ModelRole},
            protocol::{Function, ToolCall},
        };
        use serde_json::json;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let profile = Profile {
            base_url: format!("http://{}/v1", listener.local_addr().unwrap()),
            stream: false,
            extra_body: std::collections::BTreeMap::from([(
                "chat_template_kwargs".into(),
                json!({"enable_thinking":true}),
            )]),
            roles: vec![ModelRole::Chat, ModelRole::Embed],
            ..Default::default()
        };
        let app = Router::new()
            .route("/v1/chat/completions", post(|Json(request): Json<serde_json::Value>| async move {
                assert_eq!(request["chat_template_kwargs"]["enable_thinking"], false);
                Json(json!({"choices":[{"message":{"role":"assistant","content":json!({
                    "findings":[{"key":"shield", "text":"Shield rate is 0.025", "evidence_call_ids":["read"]}],
                    "next_action":"no remaining action observed", "questions":[]
                }).to_string()},"finish_reason":"stop"}]}))
            }))
            .route("/v1/embeddings", post(|| async {
                std::future::pending::<Json<serde_json::Value>>().await
            }));
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let home = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        std::fs::write(
            root.path().join("shield.rs"),
            "pub fn shield() -> f32 { 0.025 }\n",
        )
        .unwrap();
        let workspace = Workspace::new(root.path()).unwrap();
        let mut store = Store::open(home.path()).unwrap();
        let session = store
            .create("idle worker", "test", root.path(), SYSTEM)
            .unwrap();
        let mut call = Message::text(Role::Assistant, "");
        call.tool_calls.push(ToolCall {
            id: "read".into(),
            kind: "function".into(),
            function: Function {
                name: "read_file".into(),
                arguments: json!({"path":"shield.rs"}).to_string(),
            },
        });
        store.append(&session, &call).unwrap();
        store.claim_tool(&session, "read").unwrap();
        let result = workspace
            .execute(&builder_tools::Action::ReadFile {
                path: "shield.rs".into(),
                start_line: None,
                end_line: None,
            })
            .await
            .unwrap();
        store.complete_tool(&session, "read", &result).unwrap();
        store
            .append(&session, &Message::text(Role::Assistant, "Done."))
            .unwrap();
        let mut config = Config::default();
        config.memory.enabled = true;
        config.memory.embedding_backend = EmbeddingBackend::Remote;
        config.memory.embedding_profile = Some("test".into());
        config.profiles.insert("test".into(), profile.clone());
        let agent = Agent {
            provider: OpenAiCompatible::new(profile.clone()).unwrap(),
            profile,
            memory: builder::memory::MemoryRuntime::from_config(&config).unwrap(),
            workspace,
            session,
            approval: ApprovalMode::ReadOnly,
            max_rounds: 2,
        };
        let mut task = start_memory_task(&agent, home.path(), &config);
        assert_eq!(
            agent.profile.extra_body["chat_template_kwargs"]["enable_thinking"],
            true
        );
        let scope = builder::memory::MemoryRuntime::scope(&agent.workspace);
        let outcome = tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while store.memory_list(&scope).unwrap().is_empty() {
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await;
        stop_memory_task(&mut task).await;
        server.abort();
        assert!(
            outcome.is_ok(),
            "a blocked embedding queue must not starve extraction"
        );
        assert!(
            task.is_none(),
            "idle work must cancel before foreground work"
        );
    }

    #[tokio::test]
    async fn timed_out_maintenance_remains_owned_until_it_stops() {
        let (cancel, mut cancelled) = tokio::sync::oneshot::channel();
        let (stopped_tx, stopped) = tokio::sync::oneshot::channel();
        let mut task = Some(MemoryTask {
            cancel: Some(cancel),
            stopped,
            index_updates: std::sync::Arc::new(std::sync::Mutex::new(IndexUpdateState::Starting)),
            memory_status: Default::default(),
        });

        stop_memory_task(&mut task).await;
        assert!(task.is_some(), "a live worker must not be detached");
        assert_eq!(cancelled.try_recv(), Ok(()));

        stopped_tx.send(()).unwrap();
        reap_memory_task(&mut task);
        assert!(task.is_none());
    }
}
