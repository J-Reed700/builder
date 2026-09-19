//! Outbound connector from the workspace-owning Builder process to a gateway.

use crate::remote::RemoteControl;
use anyhow::{Context, Result, anyhow, ensure};
use axum::{
    body::{Body, to_bytes},
    http::{Method, Request},
};
use builder_remote_protocol::{
    Credential, GatewayMessage, HostMessage, MAX_REQUEST_BODY, MAX_RESPONSE_BODY, MAX_WIRE_MESSAGE,
    PROTOCOL_VERSION,
};
use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    fs::OpenOptions,
    io::{Read, Write},
    path::{Path, PathBuf},
    time::Duration,
};
use tokio_tungstenite::{
    connect_async_with_config,
    tungstenite::{Error as WebSocketError, Message, protocol::WebSocketConfig},
};
use tower::ServiceExt;
use url::Url;
use uuid::Uuid;

pub struct ConnectOptions {
    pub gateway: String,
    pub code: Option<String>,
    pub home: PathBuf,
    pub pair_only: bool,
}

#[derive(Clone, Deserialize, Serialize)]
struct SavedCredential {
    version: u16,
    gateway: String,
    device_id: String,
    device_name: String,
    token: String,
}

enum AttemptError {
    Disconnected(anyhow::Error),
    Rejected(anyhow::Error),
}

pub async fn serve(control: &RemoteControl, options: ConnectOptions) -> Result<()> {
    let (gateway, websocket) = gateway_urls(&options.gateway)?;
    let credential_path = credential_path(&options.home, &gateway);
    let saved = load_credential(&credential_path)?;
    ensure!(
        saved.as_ref().is_none_or(|saved| saved.gateway == gateway),
        "Saved gateway credential belongs to another origin"
    );
    let device_id = saved
        .as_ref()
        .map(|saved| saved.device_id.clone())
        .unwrap_or_else(|| Uuid::new_v4().to_string());
    let device_name = saved
        .as_ref()
        .map(|saved| saved.device_name.clone())
        .unwrap_or_else(device_name);
    let mut credential = match (&options.code, saved.as_ref()) {
        (Some(code), _) => {
            ensure!(
                is_hex_secret(code),
                "Pairing code must be the complete code shown by the gateway"
            );
            Credential::Enroll { code: code.clone() }
        }
        (None, Some(saved)) => Credential::Device {
            token: saved.token.clone(),
        },
        (None, None) => anyhow::bail!(
            "This computer is not paired with {gateway}. Open the gateway dashboard and run its connect command."
        ),
    };
    let router = control.router();
    let origin = gateway.clone();
    let token = control.token().to_owned();
    let mut delay = Duration::from_secs(1);
    loop {
        let attempt_started = std::time::Instant::now();
        match connect_once(
            &websocket,
            &gateway,
            &credential_path,
            &device_id,
            &device_name,
            &mut credential,
            &origin,
            &token,
            router.clone(),
            options.pair_only,
        )
        .await
        {
            Ok(()) => {
                if options.pair_only {
                    return Ok(());
                }
                eprintln!("Gateway connection closed; reconnecting without replaying any request.");
                delay = Duration::from_secs(1);
            }
            Err(AttemptError::Disconnected(error)) => {
                // Only a link that never stabilised should back off further;
                // otherwise a nightly proxy restart compounds the delay day
                // after day (observed: 1s, 2s, 4s, 8s, 16s on consecutive days).
                if attempt_started.elapsed() >= STABLE_CONNECTION {
                    delay = Duration::from_secs(1);
                }
                eprintln!(
                    "Gateway unavailable: {error:#}. Retrying in {}s.",
                    delay.as_secs()
                );
            }
            Err(AttemptError::Rejected(error)) => return Err(error),
        }
        tokio::time::sleep(delay).await;
        delay = (delay * 2).min(Duration::from_secs(30));
    }
}

#[allow(clippy::too_many_arguments)]
async fn connect_once(
    websocket: &str,
    gateway: &str,
    credential_path: &Path,
    device_id: &str,
    device_name: &str,
    credential: &mut Credential,
    origin: &str,
    token: &str,
    router: axum::Router,
    pair_only: bool,
) -> std::result::Result<(), AttemptError> {
    let config = WebSocketConfig::default()
        .max_message_size(Some(MAX_WIRE_MESSAGE))
        .max_frame_size(Some(MAX_WIRE_MESSAGE));
    let (mut socket, _) = connect_async_with_config(websocket, Some(config), false)
        .await
        .map_err(classify_connection_error)?;
    let hello = HostMessage::Hello {
        protocol: PROTOCOL_VERSION,
        device_id: device_id.into(),
        device_name: device_name.into(),
        credential: match credential {
            Credential::Enroll { code } => Credential::Enroll { code: code.clone() },
            Credential::Device { token } => Credential::Device {
                token: token.clone(),
            },
        },
    };
    send(&mut socket, &hello)
        .await
        .map_err(AttemptError::Disconnected)?;
    let ready = tokio::time::timeout(Duration::from_secs(10), receive(&mut socket))
        .await
        .map_err(|_| AttemptError::Disconnected(anyhow!("gateway handshake timed out")))?
        .map_err(AttemptError::Disconnected)?;
    match ready {
        GatewayMessage::Ready {
            protocol,
            device_id: accepted_id,
            device_token,
        } => {
            if protocol != PROTOCOL_VERSION || accepted_id != device_id {
                return Err(AttemptError::Rejected(anyhow!(
                    "Gateway returned an incompatible host identity"
                )));
            }
            if let Some(device_token) = device_token {
                if !is_hex_secret(&device_token) {
                    return Err(AttemptError::Rejected(anyhow!(
                        "Gateway returned an invalid device credential"
                    )));
                }
                let saved = SavedCredential {
                    version: 1,
                    gateway: gateway.into(),
                    device_id: device_id.into(),
                    device_name: device_name.into(),
                    token: device_token.clone(),
                };
                save_credential(credential_path, &saved).map_err(AttemptError::Rejected)?;
                *credential = Credential::Device {
                    token: device_token,
                };
            }
        }
        GatewayMessage::Error { message } => {
            return Err(AttemptError::Rejected(anyhow!(
                "Gateway rejected this computer: {message}"
            )));
        }
        GatewayMessage::Request { .. } => {
            return Err(AttemptError::Rejected(anyhow!(
                "Gateway sent a request before completing authentication"
            )));
        }
    }
    if pair_only {
        eprintln!("Paired {device_name} with {gateway}.");
        return Ok(());
    }
    eprintln!("Connected to {gateway} as {device_name}. Keep this process running.");

    let mut strikes = 0u32;
    loop {
        // Bounded wait: any frame (including a pong) proves the link is alive.
        let message = match tokio::time::timeout(HEARTBEAT_IDLE, receive_frame(&mut socket)).await {
            Ok(frame) => match frame.map_err(AttemptError::Disconnected)? {
                Incoming::Message(message) => {
                    strikes = 0;
                    message
                }
                Incoming::Liveness => {
                    strikes = 0;
                    continue;
                }
            },
            Err(_) => {
                strikes += 1;
                if strikes > HEARTBEAT_STRIKES {
                    return Err(AttemptError::Disconnected(anyhow!(
                        "gateway stopped answering heartbeats after {}s",
                        HEARTBEAT_IDLE.as_secs() * u64::from(strikes)
                    )));
                }
                socket
                    .send(Message::Ping(Vec::new().into()))
                    .await
                    .map_err(|error| AttemptError::Disconnected(error.into()))?;
                continue;
            }
        };
        match message {
            GatewayMessage::Request {
                request_id,
                method,
                path,
                body,
            } => {
                let response = dispatch(
                    router.clone(),
                    token,
                    origin,
                    &request_id,
                    &method,
                    &path,
                    body,
                )
                .await;
                send(&mut socket, &response)
                    .await
                    .map_err(AttemptError::Disconnected)?;
            }
            GatewayMessage::Error { message } => {
                return Err(AttemptError::Rejected(anyhow!(
                    "Gateway closed the connection: {message}"
                )));
            }
            GatewayMessage::Ready { .. } => {
                return Err(AttemptError::Rejected(anyhow!(
                    "Gateway repeated its authentication response"
                )));
            }
        }
    }
}

fn classify_connection_error(error: WebSocketError) -> AttemptError {
    match error {
        WebSocketError::Http(response) => AttemptError::Rejected(anyhow!(
            "Gateway WebSocket returned HTTP {}. Verify that Pangolin bypasses authentication only for /api/gateway/connect",
            response.status()
        )),
        WebSocketError::Tls(error) => {
            AttemptError::Rejected(anyhow!("Gateway TLS validation failed: {error}"))
        }
        error => AttemptError::Disconnected(error.into()),
    }
}

async fn dispatch(
    router: axum::Router,
    token: &str,
    origin: &str,
    request_id: &str,
    method: &str,
    path: &str,
    body: String,
) -> HostMessage {
    let response = async {
        ensure!(
            Uuid::parse_str(request_id).is_ok(),
            "Invalid gateway request ID"
        );
        ensure!(
            body.len() <= MAX_REQUEST_BODY,
            "Gateway request body is too large"
        );
        ensure!(
            path.starts_with("/api/") && !path.starts_with("/api/gateway/") && path.len() <= 2048,
            "Gateway request path is outside the remote API"
        );
        let method: Method = method.parse().context("Invalid gateway request method")?;
        ensure!(
            method == Method::GET || method == Method::POST,
            "Unsupported gateway request method"
        );
        let mut request = Request::builder()
            .method(method.clone())
            .uri(path)
            .header("x-builder-token", token);
        if method == Method::POST {
            request = request
                .header("origin", origin)
                .header("content-type", "application/json");
        }
        let response = router
            .oneshot(request.body(Body::from(body))?)
            .await
            .context("Remote API failed")?;
        let status = response.status().as_u16();
        let bytes = to_bytes(response.into_body(), MAX_RESPONSE_BODY)
            .await
            .context("Remote API response exceeded the gateway limit")?;
        let body =
            String::from_utf8(bytes.to_vec()).context("Remote API returned non-UTF-8 data")?;
        Ok::<_, anyhow::Error>((status, body))
    }
    .await;
    let (status, body) = match response {
        Ok(response) => response,
        Err(error) => (
            502,
            json_error(&format!("Host rejected the gateway request: {error:#}")),
        ),
    };
    HostMessage::Response {
        request_id: request_id.into(),
        status,
        body,
    }
}

fn json_error(message: &str) -> String {
    serde_json::to_string(&serde_json::json!({"error":message}))
        .unwrap_or_else(|_| "{\"error\":\"Host request failed\"}".into())
}

/// How long the host waits for any frame before probing the gateway.
///
/// Without this the serve loop sat in `socket.next().await` forever: a proxy
/// restart or NAT timeout kills the TCP connection without a close frame, so
/// the host kept reporting "Connected" while the gateway had already marked it
/// offline. That is what made the dashboard ask for a new connect command even
/// though pairing was intact — and re-pairing then invalidated the credential
/// the still-running service was holding, which is the loop we are breaking.
const HEARTBEAT_IDLE: Duration = Duration::from_secs(30);
/// Unanswered probes tolerated before declaring the connection dead.
const HEARTBEAT_STRIKES: u32 = 2;
/// A connection that survived this long is treated as healthy, so the next
/// drop retries immediately instead of inheriting a grown backoff.
const STABLE_CONNECTION: Duration = Duration::from_secs(60);

/// A frame from the gateway: either a protocol message, or mere evidence that
/// the peer is still alive (pong/ping). Liveness must be visible to the serve
/// loop, otherwise an idle-but-healthy link looks identical to a dead one.
enum Incoming {
    Message(GatewayMessage),
    Liveness,
}

async fn send(
    socket: &mut tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
    message: &HostMessage,
) -> Result<()> {
    let text = serde_json::to_string(message)?;
    ensure!(
        text.len() <= MAX_WIRE_MESSAGE,
        "Host message exceeds the wire limit"
    );
    socket.send(Message::Text(text.into())).await?;
    Ok(())
}

async fn receive(
    socket: &mut tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
) -> Result<GatewayMessage> {
    loop {
        match receive_frame(socket).await? {
            Incoming::Message(message) => return Ok(message),
            Incoming::Liveness => {}
        }
    }
}

/// Reads exactly one frame, reporting keepalive traffic instead of hiding it.
async fn receive_frame(
    socket: &mut tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
) -> Result<Incoming> {
    match socket.next().await {
        Some(Ok(Message::Text(text))) => {
            ensure!(
                text.len() <= MAX_WIRE_MESSAGE,
                "Gateway message exceeds the wire limit"
            );
            Ok(Incoming::Message(
                serde_json::from_str(&text).context("Invalid gateway message")?,
            ))
        }
        Some(Ok(Message::Ping(value))) => {
            socket.send(Message::Pong(value)).await?;
            Ok(Incoming::Liveness)
        }
        Some(Ok(Message::Close(_))) | None => anyhow::bail!("gateway disconnected"),
        Some(Err(error)) => Err(error.into()),
        Some(Ok(_)) => Ok(Incoming::Liveness),
    }
}

fn gateway_urls(value: &str) -> Result<(String, String)> {
    let parsed = Url::parse(value).context("Gateway must be a complete http:// or https:// URL")?;
    ensure!(
        matches!(parsed.scheme(), "http" | "https"),
        "Gateway URL must use HTTP or HTTPS"
    );
    ensure!(
        parsed.host_str().is_some(),
        "Gateway URL must include a host"
    );
    ensure!(
        parsed.username().is_empty() && parsed.password().is_none(),
        "Gateway URL cannot contain credentials"
    );
    ensure!(
        parsed.path() == "/" && parsed.query().is_none() && parsed.fragment().is_none(),
        "Gateway URL must be an exact origin without a path, query, or fragment"
    );
    let gateway = parsed.origin().ascii_serialization();
    let mut websocket = parsed;
    websocket
        .set_scheme(if websocket.scheme() == "https" {
            "wss"
        } else {
            "ws"
        })
        .map_err(|_| anyhow!("Could not build the gateway WebSocket URL"))?;
    websocket.set_path("/api/gateway/connect");
    Ok((gateway, websocket.into()))
}

fn device_name() -> String {
    ["COMPUTERNAME", "HOSTNAME"]
        .iter()
        .filter_map(|name| std::env::var(name).ok())
        .find(|value| !value.trim().is_empty() && value.len() <= 128)
        .unwrap_or_else(|| "Builder computer".into())
}

fn credential_path(home: &Path, gateway: &str) -> PathBuf {
    let digest = format!("{:x}", Sha256::digest(gateway.as_bytes()));
    home.join("remote-gateways")
        .join(format!("{}.json", &digest[..24]))
}

fn load_credential(path: &Path) -> Result<Option<SavedCredential>> {
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    ensure!(
        metadata.file_type().is_file(),
        "Gateway credential must be a regular file"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        ensure!(
            metadata.permissions().mode() & 0o077 == 0,
            "Gateway credential must be private"
        );
    }
    let mut contents = String::new();
    std::fs::File::open(path)?
        .take(16 * 1024 + 1)
        .read_to_string(&mut contents)?;
    ensure!(
        contents.len() <= 16 * 1024,
        "Gateway credential is too large"
    );
    let credential: SavedCredential =
        serde_json::from_str(&contents).context("Invalid gateway credential")?;
    ensure!(
        credential.version == 1,
        "Unsupported gateway credential version"
    );
    ensure!(
        Uuid::parse_str(&credential.device_id).is_ok(),
        "Invalid saved gateway device ID"
    );
    ensure!(
        !credential.device_name.is_empty() && credential.device_name.len() <= 128,
        "Invalid saved gateway device name"
    );
    gateway_urls(&credential.gateway).context("Invalid saved gateway origin")?;
    ensure!(
        is_hex_secret(&credential.token),
        "Invalid saved gateway device token"
    );
    Ok(Some(credential))
}

struct TemporaryFile(Option<PathBuf>);
impl Drop for TemporaryFile {
    fn drop(&mut self) {
        if let Some(path) = &self.0 {
            let _ = std::fs::remove_file(path);
        }
    }
}

fn save_credential(path: &Path, credential: &SavedCredential) -> Result<()> {
    let parent = path
        .parent()
        .context("Gateway credential path has no parent")?;
    std::fs::create_dir_all(parent)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700))?;
    }
    let temporary_path = path.with_extension(format!("{}.tmp", Uuid::new_v4().simple()));
    let mut cleanup = TemporaryFile(Some(temporary_path.clone()));
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(&temporary_path)?;
    serde_json::to_writer(&mut file, credential)?;
    file.write_all(b"\n")?;
    file.sync_all()?;
    std::fs::rename(&temporary_path, path)?;
    cleanup.0 = None;
    std::fs::File::open(parent)?.sync_all()?;
    Ok(())
}

fn is_hex_secret(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gateway_url_is_restricted_to_an_origin() {
        let (origin, socket) = gateway_urls("https://builder.example.com").unwrap();
        assert_eq!(origin, "https://builder.example.com");
        assert_eq!(socket, "wss://builder.example.com/api/gateway/connect");
        assert!(gateway_urls("https://builder.example.com/path").is_err());
        assert!(gateway_urls("file:///tmp/socket").is_err());
    }
}
