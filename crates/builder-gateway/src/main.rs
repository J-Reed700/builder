use anyhow::{Context, Result};
use builder_gateway::{Gateway, GatewayOptions};
use std::{net::SocketAddr, path::PathBuf};

#[tokio::main]
async fn main() {
    if let Err(error) = run().await {
        eprintln!("builder-gateway: {error:#}");
        std::process::exit(1);
    }
}

async fn run() -> Result<()> {
    let listen: SocketAddr = environment("BUILDER_GATEWAY_LISTEN", "0.0.0.0:8080")
        .parse()
        .context("BUILDER_GATEWAY_LISTEN must be an IP address and port")?;
    let origin = environment("BUILDER_GATEWAY_ORIGIN", "http://127.0.0.1:8080");
    let identity_header = environment("BUILDER_GATEWAY_AUTH_HEADER", "Remote-User");
    let data = PathBuf::from(environment("BUILDER_GATEWAY_DATA", "/data"));
    let gateway = Gateway::new(GatewayOptions {
        origin,
        identity_header: (identity_header != "none").then_some(identity_header),
        data,
    })?;
    let listener = tokio::net::TcpListener::bind(listen).await?;
    eprintln!(
        "Builder Gateway listening on http://{}",
        listener.local_addr()?
    );
    axum::serve(listener, gateway.router())
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await?;
    Ok(())
}

fn environment(name: &str, default: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| default.into())
}
