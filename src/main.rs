mod cli;
mod commands;
mod interactive;
use anyhow::{Context, Result, ensure};
use builder::{
    agent::{Agent, SYSTEM},
    ui,
};
use builder_core::{
    config::{self, Config},
    store::Store,
};
use builder_provider::OpenAiCompatible;
use builder_tools::Workspace;
use clap::Parser;
use cli::{Cli, Command};
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
        return commands::remote::run(
            &cli,
            command.as_ref(),
            *listen,
            origin.as_ref(),
            home,
            config_home,
        )
        .await;
    }
    commands::config::validate_transient_pipeline_scope(&cli)?;

    if let Some(Command::Config { command }) = &cli.command {
        commands::config::run(command, &cli, &mut config, &config_home)?;
        return Ok(());
    }
    if let Some(Command::Daemon { once, service }) = &cli.command {
        ensure!(
            !cli.auto && matches!(cli.approval, cli::Approval::Ask),
            "Daemon permissions are stored per schedule. Use schedule add --allow-writes to authorize tools; --auto and --approval do not apply to the runner."
        );
        if let Some(format) = service {
            print!(
                "{}",
                builder::scheduler::service::render(
                    *format,
                    &std::env::current_exe()?,
                    cli.home.as_deref(),
                    &home
                )?
            );
            return Ok(());
        }
        return builder::scheduler::serve(&home, &config_home, *once).await;
    }
    let mut store = Store::open(&home)?;
    if let Some(Command::Schedule(args)) = &cli.command {
        return builder::scheduler::commands::handle(
            args,
            &mut store,
            &config,
            &cli.workspace,
            cli.profile.as_deref(),
        );
    }
    match &cli.command {
        Some(Command::Sessions) => {
            commands::sessions::list(&store)?;
            return Ok(());
        }
        Some(Command::Export { session, json }) => {
            commands::sessions::export(&store, session, *json)?;
            return Ok(());
        }
        _ => {}
    }
    if let Some(Command::Memory { command }) = &cli.command {
        commands::memory::run(command, &cli, &mut config, &config_home, &home, &mut store).await?;
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
        return commands::diagnostics::run(
            matches!(cli.command, Some(Command::Doctor)),
            &home,
            &profile_name,
            &profile,
            &provider,
            &mut store,
        )
        .await;
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
        interactive::drive(&agent, &mut store, false).await?;
        if let Some(message) = store.messages(&agent.session)?.last() {
            println!("{}", ui::safe(message.content.as_deref().unwrap_or("")));
        }
        eprintln!("Session saved: {}", &agent.session[..8]);
        return Ok(());
    }
    interactive::run(
        &mut agent,
        &mut store,
        initial_prompt,
        interactive::SessionOptions {
            profile_name,
            home,
            config_home,
            config,
            plain_requested: cli.plain,
        },
    )
    .await
}
