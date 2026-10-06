use crate::cli::{Cli, RemoteCommand};
use anyhow::{Context, Result, ensure};
use std::{net::SocketAddr, path::PathBuf};

pub(crate) async fn run(
    cli: &Cli,
    command: Option<&RemoteCommand>,
    listen: SocketAddr,
    origin: Option<&String>,
    home: PathBuf,
    config_home: PathBuf,
) -> Result<()> {
    ensure!(
        cli.pipeline.is_empty(),
        "Save pipeline settings with config pipeline set before starting remote control"
    );
    let public_origin = match command {
        Some(RemoteCommand::Connect { gateway, .. }) => gateway.clone(),
        None => origin
            .cloned()
            .unwrap_or_else(|| format!("http://{listen}")),
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
            let credential_home = remote
                .token_path()
                .parent()
                .context("Remote token path has no parent directory")?
                .to_owned();
            let result = tokio::select! {
                result = builder::remote_connect::serve(
                    &remote,
                    builder::remote_connect::ConnectOptions {
                        gateway: gateway.clone(),
                        code: code.clone(),
                        home: credential_home,
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
    Ok(())
}
