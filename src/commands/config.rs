use crate::cli::{Cli, ConfigCommand, PipelineCommand};
use anyhow::{Context, Result, ensure};
use builder::ui;
use builder_core::config::{self, Config, Profile};
use std::path::Path;

pub(crate) fn validate_transient_pipeline_scope(cli: &Cli) -> Result<()> {
    if cli.pipeline.is_empty() {
        return Ok(());
    }
    ensure!(
        matches!(
            &cli.command,
            None | Some(
                crate::cli::Command::Chat { .. }
                    | crate::cli::Command::Run { .. }
                    | crate::cli::Command::Resume { .. }
                    | crate::cli::Command::Doctor
                    | crate::cli::Command::Models
                    | crate::cli::Command::Config {
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
    Ok(())
}

pub(crate) fn run(
    command: &ConfigCommand,
    cli: &Cli,
    config: &mut Config,
    config_home: &Path,
) -> Result<()> {
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
                    config.save(config_home)?;
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
                    config.save(config_home)?;
                    println!("Restored pipeline defaults for {}", ui::safe(&name));
                }
            }
        }
        ConfigCommand::Init => {
            if !config_home.join("config.toml").exists() {
                config.save(config_home)?;
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
            let warnings = imported.merge(config, *replace, *activate)?;
            config.save(config_home)?;
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
            config.save(config_home)?;
            println!("Updated {} model: {}", ui::safe(&name), ui::safe(model));
        }
        ConfigCommand::Use { name } => {
            config.profile(Some(name))?;
            config.default_profile = name.clone();
            config.save(config_home)?;
            println!("Default profile: {}", ui::safe(name));
        }
        ConfigCommand::Token { clear } => {
            let (name, mut profile) = config.profile(cli.profile.as_deref())?;
            if *clear {
                profile.api_key = None;
                println!("Cleared stored credential for {}", ui::safe(&name));
            } else {
                let token = read_secret("Paste token (input hidden), then Enter: ")?;
                ensure!(!token.is_empty(), "No token provided; profile unchanged");
                profile.api_key = Some(config::Secret::Literal(token));
                // The stored token is the override; an env reference would conflict.
                profile.api_key_env = None;
                println!("Saved credential override for {}", ui::safe(&name));
            }
            profile.validate()?;
            config.profiles.insert(name, profile);
            config.save(config_home)?;
        }
        ConfigCommand::Add {
            name,
            api,
            base_url,
            model,
            api_key_env,
            no_stream,
            no_tools,
            context_tokens,
            max_output_tokens,
        } => {
            ensure!(!name.trim().is_empty(), "Profile name cannot be empty");
            let api: config::Api = (*api).into();
            let base_url = match (base_url, api) {
                (Some(url), _) => url.clone(),
                (None, config::Api::Anthropic) => config::ANTHROPIC_BASE_URL.into(),
                (None, config::Api::OpenAi) => {
                    anyhow::bail!("--base-url is required for OpenAI-compatible profiles")
                }
            };
            let profile = Profile {
                api,
                base_url,
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
            config.save(config_home)?;
            println!("Saved profile {}", ui::safe(name));
        }
    }
    Ok(())
}

/// Reads one line from stdin without echo when it is a terminal; piped input
/// (`pbpaste | builder config token`) is read as-is.
fn read_secret(prompt: &str) -> Result<String> {
    use std::io::{BufRead, IsTerminal, Write};
    let stdin = std::io::stdin();
    let mut line = String::new();
    if stdin.is_terminal() {
        eprint!("{prompt}");
        std::io::stderr().flush()?;
        let restore = disable_echo();
        let result = stdin.lock().read_line(&mut line);
        if restore {
            enable_echo();
        }
        eprintln!();
        result?;
    } else {
        stdin.lock().read_line(&mut line)?;
    }
    Ok(line.trim().to_owned())
}
#[cfg(unix)]
fn disable_echo() -> bool {
    std::process::Command::new("stty")
        .arg("-echo")
        .stdin(std::process::Stdio::inherit())
        .status()
        .is_ok_and(|s| s.success())
}
#[cfg(unix)]
fn enable_echo() {
    let _ = std::process::Command::new("stty")
        .arg("echo")
        .stdin(std::process::Stdio::inherit())
        .status();
}
#[cfg(not(unix))]
fn disable_echo() -> bool {
    false
}
#[cfg(not(unix))]
fn enable_echo() {}
