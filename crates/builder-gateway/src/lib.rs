//! Small public relay for Builder's outbound remote-control connection.

use anyhow::{Context, Result, ensure};
use axum::{
    Json, Router,
    body::{Body, to_bytes},
    extract::{
        DefaultBodyLimit, Request, State, WebSocketUpgrade,
        ws::{Message, WebSocket},
    },
    http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{any, get, post},
};
use builder_remote_protocol::{
    Credential, GatewayMessage, HostMessage, MAX_REQUEST_BODY, MAX_RESPONSE_BODY, MAX_WIRE_MESSAGE,
    PROTOCOL_VERSION,
};
use futures_util::SinkExt;
use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    fs::OpenOptions,
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use subtle::ConstantTimeEq;
use tokio::sync::{mpsc, oneshot};
use uuid::Uuid;

const INVITATION_LIFETIME: Duration = Duration::from_secs(10 * 60);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(12);
const MAX_INVITATIONS: usize = 32;
const MAX_PENDING_REQUESTS: usize = 32;

pub struct GatewayOptions {
    pub origin: String,
    pub identity_header: Option<String>,
    pub data: PathBuf,
}

#[derive(Clone)]
pub struct Gateway {
    shared: Arc<Shared>,
}

struct Shared {
    origin: String,
    identity_header: Option<HeaderName>,
    device_path: PathBuf,
    device: Mutex<Option<DeviceRecord>>,
    invitations: Mutex<HashMap<String, Invitation>>,
    connection: Mutex<Option<Connection>>,
    pending: Mutex<HashMap<String, PendingRequest>>,
}

#[derive(Clone, Deserialize, Serialize)]
struct DeviceRecord {
    version: u16,
    id: String,
    name: String,
    owner: String,
    token_hash: String,
}

struct Invitation {
    owner: String,
    expires: Instant,
    enrollment: Option<CompletedEnrollment>,
}

struct CompletedEnrollment {
    device_id: String,
    token: String,
}

#[derive(Clone)]
struct Connection {
    generation: String,
    device_id: String,
    owner: String,
    outgoing: mpsc::Sender<GatewayMessage>,
}

struct DeviceResponse {
    status: u16,
    body: String,
}

struct PendingRequest {
    generation: String,
    reply: oneshot::Sender<DeviceResponse>,
}

impl Gateway {
    pub fn new(options: GatewayOptions) -> Result<Self> {
        validate_origin(&options.origin)?;
        let identity_header = options
            .identity_header
            .as_deref()
            .map(str::parse)
            .transpose()
            .context("BUILDER_GATEWAY_AUTH_HEADER is not a valid HTTP header name")?;
        create_private_directory(&options.data)?;
        let device_path = options.data.join("device.json");
        let device = load_device(&device_path)?;
        Ok(Self {
            shared: Arc::new(Shared {
                origin: options.origin,
                identity_header,
                device_path,
                device: Mutex::new(device),
                invitations: Mutex::new(HashMap::new()),
                connection: Mutex::new(None),
                pending: Mutex::new(HashMap::new()),
            }),
        })
    }

    pub fn router(&self) -> Router {
        Router::new()
            .route("/healthz", get(health))
            .route("/api/gateway/status", get(gateway_status))
            .route("/api/gateway/invitations", post(create_invitation))
            .route("/api/gateway/connect", get(connect_host))
            .route("/api/{*path}", any(proxy))
            .route(
                "/",
                get(|| async {
                    (
                        [("content-type", "text/html; charset=utf-8")],
                        include_str!("../../../remote/web/index.html"),
                    )
                }),
            )
            .route(
                "/app.js",
                get(|| async {
                    (
                        [("content-type", "text/javascript; charset=utf-8")],
                        include_str!("../../../remote/web/app.js"),
                    )
                }),
            )
            .route(
                "/style.css",
                get(|| async {
                    (
                        [("content-type", "text/css; charset=utf-8")],
                        include_str!("../../../remote/web/style.css"),
                    )
                }),
            )
            .layer(DefaultBodyLimit::max(MAX_REQUEST_BODY))
            .layer(middleware::from_fn(security_headers))
            .with_state(self.shared.clone())
    }
}

fn validate_origin(origin: &str) -> Result<()> {
    ensure!(
        origin.starts_with("http://") || origin.starts_with("https://"),
        "BUILDER_GATEWAY_ORIGIN must start with http:// or https://"
    );
    let authority = origin.split_once("://").unwrap().1;
    ensure!(
        !authority.is_empty()
            && !authority.contains(['/', '?', '#', '@', '"', '\''])
            && !authority.chars().any(char::is_whitespace),
        "BUILDER_GATEWAY_ORIGIN must be an exact browser origin"
    );
    HeaderValue::from_str(origin).context("Invalid BUILDER_GATEWAY_ORIGIN")?;
    Ok(())
}

fn create_private_directory(path: &Path) -> Result<()> {
    std::fs::create_dir_all(path)?;
    let metadata = std::fs::symlink_metadata(path)?;
    ensure!(
        metadata.file_type().is_dir(),
        "Gateway data path must be a directory"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

fn load_device(path: &Path) -> Result<Option<DeviceRecord>> {
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    ensure!(
        metadata.file_type().is_file(),
        "device.json must be a regular file"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        ensure!(
            metadata.permissions().mode() & 0o077 == 0,
            "device.json must be private"
        );
    }
    let mut contents = String::new();
    std::fs::File::open(path)?
        .take(16 * 1024 + 1)
        .read_to_string(&mut contents)?;
    ensure!(contents.len() <= 16 * 1024, "device.json is too large");
    let device: DeviceRecord = serde_json::from_str(&contents).context("Invalid device.json")?;
    ensure!(device.version == 1, "Unsupported device.json version");
    validate_device(&device)?;
    Ok(Some(device))
}

fn validate_device(device: &DeviceRecord) -> Result<()> {
    ensure!(Uuid::parse_str(&device.id).is_ok(), "Invalid device ID");
    ensure!(
        !device.name.is_empty() && device.name.len() <= 128,
        "Invalid device name"
    );
    ensure!(
        !device.owner.is_empty() && device.owner.len() <= 256,
        "Invalid device owner"
    );
    ensure!(
        is_hex_secret(&device.token_hash),
        "Invalid device token hash"
    );
    Ok(())
}

struct TemporaryFile(Option<PathBuf>);
impl Drop for TemporaryFile {
    fn drop(&mut self) {
        if let Some(path) = &self.0 {
            let _ = std::fs::remove_file(path);
        }
    }
}

fn save_device(path: &Path, device: &DeviceRecord) -> Result<()> {
    validate_device(device)?;
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
    serde_json::to_writer(&mut file, device)?;
    file.write_all(b"\n")?;
    file.sync_all()?;
    std::fs::rename(&temporary_path, path)?;
    cleanup.0 = None;
    if let Some(parent) = path.parent() {
        std::fs::File::open(parent)?.sync_all()?;
    }
    Ok(())
}

fn secret() -> String {
    format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple())
}

fn hash(value: &str) -> String {
    format!("{:x}", Sha256::digest(value.as_bytes()))
}

fn is_hex_secret(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn constant_time_equal(left: &str, right: &str) -> bool {
    left.len() == right.len() && bool::from(left.as_bytes().ct_eq(right.as_bytes()))
}

async fn security_headers(request: Request, next: Next) -> Response {
    let mut response = next.run(request).await;
    for (key, value) in [
        ("cache-control", "no-store"),
        ("x-content-type-options", "nosniff"),
        ("referrer-policy", "no-referrer"),
        ("x-frame-options", "DENY"),
        (
            "content-security-policy",
            "default-src 'none'; script-src 'self'; style-src 'self'; connect-src 'self'; img-src 'self'; base-uri 'none'; frame-ancestors 'none'; form-action 'self'",
        ),
    ] {
        response
            .headers_mut()
            .insert(key, HeaderValue::from_static(value));
    }
    response
}

async fn health() -> &'static str {
    "ok"
}

fn problem(status: StatusCode, message: impl Into<String>) -> Response {
    (status, Json(json!({"error": message.into()}))).into_response()
}

fn identity(headers: &HeaderMap, shared: &Shared) -> Result<String, Box<Response>> {
    let Some(header) = &shared.identity_header else {
        return Ok("local".into());
    };
    let value = headers
        .get(header)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("");
    if value.is_empty() || value.len() > 256 {
        return Err(Box::new(problem(
            StatusCode::UNAUTHORIZED,
            "Sign in through the configured gateway before using Builder",
        )));
    }
    Ok(value.into())
}

fn require_origin(headers: &HeaderMap, shared: &Shared) -> Result<(), Box<Response>> {
    if headers.get("origin").and_then(|value| value.to_str().ok()) != Some(shared.origin.as_str()) {
        return Err(Box::new(problem(
            StatusCode::FORBIDDEN,
            "Browser origin does not match the gateway",
        )));
    }
    Ok(())
}

async fn gateway_status(State(shared): State<Arc<Shared>>, headers: HeaderMap) -> Response {
    let owner = match identity(&headers, &shared) {
        Ok(owner) => owner,
        Err(response) => return *response,
    };
    let device = shared.device.lock().unwrap().clone();
    if device.as_ref().is_some_and(|device| device.owner != owner) {
        return problem(
            StatusCode::FORBIDDEN,
            "This gateway belongs to another user",
        );
    }
    let connection = shared.connection.lock().unwrap().clone();
    let connected = match (&device, &connection) {
        (Some(device), Some(connection)) => {
            device.id == connection.device_id && connection.owner == owner
        }
        _ => false,
    };
    Json(json!({
        "mode": "gateway",
        "connected": connected,
        "paired": device.is_some(),
        "device": device.map(|device| json!({"id":device.id,"name":device.name})),
        "invitation_lifetime_seconds": INVITATION_LIFETIME.as_secs(),
    }))
    .into_response()
}

async fn create_invitation(State(shared): State<Arc<Shared>>, headers: HeaderMap) -> Response {
    let owner = match identity(&headers, &shared) {
        Ok(owner) => owner,
        Err(response) => return *response,
    };
    if let Err(response) = require_origin(&headers, &shared) {
        return *response;
    }
    if shared
        .device
        .lock()
        .unwrap()
        .as_ref()
        .is_some_and(|device| device.owner != owner)
    {
        return problem(
            StatusCode::FORBIDDEN,
            "This gateway belongs to another user",
        );
    }
    let code = secret();
    let now = Instant::now();
    let mut invitations = shared.invitations.lock().unwrap();
    invitations.retain(|_, invitation| invitation.expires > now);
    if invitations.len() >= MAX_INVITATIONS {
        return problem(StatusCode::TOO_MANY_REQUESTS, "Too many open invitations");
    }
    invitations.insert(
        hash(&code),
        Invitation {
            owner,
            expires: now + INVITATION_LIFETIME,
            enrollment: None,
        },
    );
    let command = format!(
        "builder remote connect \"{}\" --code {}",
        shared.origin, code
    );
    Json(json!({
        "command": command,
        "expires_in_seconds": INVITATION_LIFETIME.as_secs(),
    }))
    .into_response()
}

async fn connect_host(State(shared): State<Arc<Shared>>, upgrade: WebSocketUpgrade) -> Response {
    upgrade
        .max_message_size(MAX_WIRE_MESSAGE)
        .max_frame_size(MAX_WIRE_MESSAGE)
        .on_upgrade(move |socket| host_socket(shared, socket))
}

async fn host_socket(shared: Arc<Shared>, mut socket: WebSocket) {
    let first = tokio::time::timeout(Duration::from_secs(10), socket.recv()).await;
    let hello = match first {
        Ok(Some(Ok(Message::Text(text)))) if text.len() <= MAX_WIRE_MESSAGE => {
            serde_json::from_str::<HostMessage>(&text).ok()
        }
        _ => None,
    };
    let Some(HostMessage::Hello {
        protocol,
        device_id,
        device_name,
        credential,
    }) = hello
    else {
        send_error(&mut socket, "Expected a Builder host hello message").await;
        return;
    };
    if protocol != PROTOCOL_VERSION
        || Uuid::parse_str(&device_id).is_err()
        || device_name.is_empty()
        || device_name.len() > 128
    {
        send_error(&mut socket, "Unsupported or invalid Builder host identity").await;
        return;
    }
    let authenticated = match authenticate_host(&shared, &device_id, &device_name, credential) {
        Ok(authenticated) => authenticated,
        Err(error) => {
            send_error(&mut socket, &format!("{error:#}")).await;
            return;
        }
    };
    let ready = GatewayMessage::Ready {
        protocol: PROTOCOL_VERSION,
        device_id: device_id.clone(),
        device_token: authenticated.new_token,
    };
    if send_json(&mut socket, &ready).await.is_err() {
        return;
    }

    let generation = Uuid::new_v4().to_string();
    let (outgoing, mut requests) = mpsc::channel(MAX_PENDING_REQUESTS);
    {
        let mut connection = shared.connection.lock().unwrap();
        *connection = Some(Connection {
            generation: generation.clone(),
            device_id,
            owner: authenticated.owner,
            outgoing,
        });
    }
    fail_other_generations(&shared, &generation);
    let mut heartbeat = tokio::time::interval(Duration::from_secs(20));
    heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    heartbeat.tick().await;

    loop {
        tokio::select! {
            _ = heartbeat.tick() => {
                if socket.send(Message::Ping(Vec::new().into())).await.is_err() { break; }
            }
            request = requests.recv() => {
                let Some(request) = request else { break };
                if send_json(&mut socket, &request).await.is_err() { break; }
            }
            incoming = socket.recv() => {
                match incoming {
                    Some(Ok(Message::Text(text))) if text.len() <= MAX_WIRE_MESSAGE => {
                        match serde_json::from_str::<HostMessage>(&text) {
                            Ok(HostMessage::Response { request_id, status, body }) if body.len() <= MAX_RESPONSE_BODY => {
                                let pending = {
                                    let mut pending = shared.pending.lock().unwrap();
                                    if pending.get(&request_id).is_some_and(|request| request.generation == generation) {
                                        pending.remove(&request_id)
                                    } else {
                                        None
                                    }
                                };
                                if let Some(pending) = pending {
                                    let _ = pending.reply.send(DeviceResponse { status, body });
                                }
                            }
                            _ => break,
                        }
                    }
                    Some(Ok(Message::Ping(value))) => {
                        if socket.send(Message::Pong(value)).await.is_err() { break; }
                    }
                    Some(Ok(Message::Close(_))) | None | Some(Err(_)) => break,
                    Some(Ok(_)) => {}
                }
            }
        }
    }
    let removed = {
        let mut connection = shared.connection.lock().unwrap();
        if connection
            .as_ref()
            .is_some_and(|current| current.generation == generation)
        {
            connection.take();
            true
        } else {
            false
        }
    };
    if removed {
        fail_generation(&shared, &generation);
    }
}

struct AuthenticatedHost {
    owner: String,
    new_token: Option<String>,
}

fn authenticate_host(
    shared: &Shared,
    device_id: &str,
    device_name: &str,
    credential: Credential,
) -> Result<AuthenticatedHost> {
    match credential {
        Credential::Enroll { code } => {
            ensure!(is_hex_secret(&code), "Pairing code is invalid or expired");
            let mut invitations = shared.invitations.lock().unwrap();
            let invitation = invitations
                .get_mut(&hash(&code))
                .filter(|invitation| invitation.expires > Instant::now())
                .context("Pairing code is invalid or expired")?;
            if let Some(enrollment) = &invitation.enrollment {
                ensure!(
                    enrollment.device_id == device_id,
                    "Pairing code was already used by another computer"
                );
                return Ok(AuthenticatedHost {
                    owner: invitation.owner.clone(),
                    new_token: Some(enrollment.token.clone()),
                });
            }
            let token = secret();
            let device = DeviceRecord {
                version: 1,
                id: device_id.into(),
                name: device_name.into(),
                owner: invitation.owner.clone(),
                token_hash: hash(&token),
            };
            save_device(&shared.device_path, &device)?;
            *shared.device.lock().unwrap() = Some(device);
            invitation.enrollment = Some(CompletedEnrollment {
                device_id: device_id.into(),
                token: token.clone(),
            });
            Ok(AuthenticatedHost {
                owner: invitation.owner.clone(),
                new_token: Some(token),
            })
        }
        Credential::Device { token } => {
            ensure!(is_hex_secret(&token), "Device credential was rejected");
            let device = shared
                .device
                .lock()
                .unwrap()
                .clone()
                .context("Pair this computer from the Builder dashboard")?;
            ensure!(device.id == device_id, "Device credential was rejected");
            ensure!(
                constant_time_equal(&device.token_hash, &hash(&token)),
                "Device credential was rejected"
            );
            Ok(AuthenticatedHost {
                owner: device.owner,
                new_token: None,
            })
        }
    }
}

async fn send_error(socket: &mut WebSocket, message: &str) {
    let _ = send_json(
        socket,
        &GatewayMessage::Error {
            message: message.into(),
        },
    )
    .await;
    let _ = socket.close().await;
}

async fn send_json(socket: &mut WebSocket, message: &GatewayMessage) -> Result<()> {
    let text = serde_json::to_string(message)?;
    ensure!(
        text.len() <= MAX_WIRE_MESSAGE,
        "Gateway message exceeds the wire limit"
    );
    socket.send(Message::Text(text.into())).await?;
    Ok(())
}

fn fail_generation(shared: &Shared, generation: &str) {
    shared
        .pending
        .lock()
        .unwrap()
        .retain(|_, pending| pending.generation != generation);
}

fn fail_other_generations(shared: &Shared, generation: &str) {
    shared
        .pending
        .lock()
        .unwrap()
        .retain(|_, pending| pending.generation == generation);
}

async fn proxy(State(shared): State<Arc<Shared>>, request: Request) -> Response {
    let owner = match identity(request.headers(), &shared) {
        Ok(owner) => owner,
        Err(response) => return *response,
    };
    let method = request.method().clone();
    if method != Method::GET && method != Method::POST {
        return problem(
            StatusCode::METHOD_NOT_ALLOWED,
            "Unsupported remote API method",
        );
    }
    if method == Method::POST
        && let Err(response) = require_origin(request.headers(), &shared)
    {
        return *response;
    }
    let path = request
        .uri()
        .path_and_query()
        .map(|value| value.as_str())
        .unwrap_or(request.uri().path())
        .to_owned();
    if path.len() > 2048 {
        return problem(StatusCode::URI_TOO_LONG, "Remote API path is too long");
    }
    let body = match to_bytes(request.into_body(), MAX_REQUEST_BODY).await {
        Ok(body) => match String::from_utf8(body.to_vec()) {
            Ok(body) => body,
            Err(_) => return problem(StatusCode::BAD_REQUEST, "Remote API body must be UTF-8"),
        },
        Err(_) => {
            return problem(
                StatusCode::PAYLOAD_TOO_LARGE,
                "Remote API body is too large",
            );
        }
    };
    let connection = shared.connection.lock().unwrap().clone();
    let Some(connection) = connection.filter(|connection| connection.owner == owner) else {
        return problem(StatusCode::SERVICE_UNAVAILABLE, "Builder host is offline");
    };
    let request_id = Uuid::new_v4().to_string();
    let (reply, response) = oneshot::channel();
    {
        let mut pending = shared.pending.lock().unwrap();
        if pending.len() >= MAX_PENDING_REQUESTS {
            return problem(
                StatusCode::TOO_MANY_REQUESTS,
                "Too many pending host requests",
            );
        }
        pending.insert(
            request_id.clone(),
            PendingRequest {
                generation: connection.generation.clone(),
                reply,
            },
        );
    }
    let message = GatewayMessage::Request {
        request_id: request_id.clone(),
        method: method.to_string(),
        path,
        body,
    };
    if connection.outgoing.try_send(message).is_err() {
        shared.pending.lock().unwrap().remove(&request_id);
        return problem(
            StatusCode::SERVICE_UNAVAILABLE,
            "Builder host connection is busy",
        );
    }
    match tokio::time::timeout(REQUEST_TIMEOUT, response).await {
        Ok(Ok(device_response)) => {
            let status =
                StatusCode::from_u16(device_response.status).unwrap_or(StatusCode::BAD_GATEWAY);
            (
                status,
                [("content-type", "application/json; charset=utf-8")],
                Body::from(device_response.body),
            )
                .into_response()
        }
        Ok(Err(_)) => uncertain_response(&method),
        Err(_) => {
            shared.pending.lock().unwrap().remove(&request_id);
            uncertain_response(&method)
        }
    }
}

fn uncertain_response(method: &Method) -> Response {
    if *method == Method::POST {
        problem(
            StatusCode::BAD_GATEWAY,
            "Connection lost before Builder acknowledged this request; inspect the chat before trying again",
        )
    } else {
        problem(
            StatusCode::BAD_GATEWAY,
            "Connection to the Builder host was lost",
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use builder_remote_protocol::{Credential, GatewayMessage, HostMessage};
    use futures_util::{SinkExt, StreamExt};
    use reqwest::Client;
    use tokio_tungstenite::{connect_async, tungstenite::Message as ClientMessage};

    async fn start() -> (tempfile::TempDir, String, tokio::task::JoinHandle<()>) {
        let data = tempfile::tempdir().unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        let gateway = Gateway::new(GatewayOptions {
            origin: origin.clone(),
            identity_header: Some("Remote-User".into()),
            data: data.path().into(),
        })
        .unwrap();
        let server = tokio::spawn(async move {
            axum::serve(listener, gateway.router()).await.unwrap();
        });
        (data, origin, server)
    }

    async fn invitation(client: &Client, origin: &str) -> String {
        let value: serde_json::Value = client
            .post(format!("{origin}/api/gateway/invitations"))
            .header("Remote-User", "owner")
            .header("Origin", origin)
            .json(&json!({}))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        value["command"]
            .as_str()
            .unwrap()
            .split_whitespace()
            .last()
            .unwrap()
            .into()
    }

    #[tokio::test]
    async fn pairing_relay_and_lost_mutation_fail_closed_without_replay() {
        let (_data, origin, server) = start().await;
        let client = Client::new();
        assert_eq!(
            client
                .get(format!("{origin}/api/gateway/status"))
                .send()
                .await
                .unwrap()
                .status(),
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(
            client
                .post(format!("{origin}/api/gateway/invitations"))
                .header("Remote-User", "owner")
                .header("Origin", "https://attacker.example")
                .json(&json!({}))
                .send()
                .await
                .unwrap()
                .status(),
            StatusCode::FORBIDDEN
        );

        let code = invitation(&client, &origin).await;
        let socket_url = origin.replace("http://", "ws://") + "/api/gateway/connect";
        let (mut host, _) = connect_async(&socket_url).await.unwrap();
        let device_id = Uuid::new_v4().to_string();
        host.send(ClientMessage::Text(
            serde_json::to_string(&HostMessage::Hello {
                protocol: PROTOCOL_VERSION,
                device_id: device_id.clone(),
                device_name: "test computer".into(),
                credential: Credential::Enroll { code },
            })
            .unwrap()
            .into(),
        ))
        .await
        .unwrap();
        let ready: GatewayMessage = serde_json::from_str(
            host.next()
                .await
                .unwrap()
                .unwrap()
                .into_text()
                .unwrap()
                .as_str(),
        )
        .unwrap();
        let GatewayMessage::Ready {
            device_token: Some(device_token),
            ..
        } = ready
        else {
            panic!("expected enrollment credential")
        };

        let request_client = client.clone();
        let request_origin = origin.clone();
        let browser = tokio::spawn(async move {
            request_client
                .post(format!("{request_origin}/api/run"))
                .header("Remote-User", "owner")
                .header("Origin", &request_origin)
                .json(&json!({"request_id":Uuid::new_v4().to_string()}))
                .send()
                .await
                .unwrap()
        });
        let request: GatewayMessage = serde_json::from_str(
            host.next()
                .await
                .unwrap()
                .unwrap()
                .into_text()
                .unwrap()
                .as_str(),
        )
        .unwrap();
        assert!(matches!(request, GatewayMessage::Request { ref method, .. } if method == "POST"));
        drop(host);
        let failed = browser.await.unwrap();
        assert_eq!(failed.status(), StatusCode::BAD_GATEWAY);
        assert!(failed.text().await.unwrap().contains("inspect the chat"));

        let (mut reconnected, _) = connect_async(&socket_url).await.unwrap();
        reconnected
            .send(ClientMessage::Text(
                serde_json::to_string(&HostMessage::Hello {
                    protocol: PROTOCOL_VERSION,
                    device_id,
                    device_name: "test computer".into(),
                    credential: Credential::Device {
                        token: device_token,
                    },
                })
                .unwrap()
                .into(),
            ))
            .await
            .unwrap();
        let ready = reconnected.next().await.unwrap().unwrap();
        assert!(matches!(
            serde_json::from_str::<GatewayMessage>(ready.into_text().unwrap().as_str()).unwrap(),
            GatewayMessage::Ready {
                device_token: None,
                ..
            }
        ));
        assert!(
            tokio::time::timeout(Duration::from_millis(150), reconnected.next())
                .await
                .is_err(),
            "The disconnected mutation must not be replayed"
        );

        let request_client = client.clone();
        let request_origin = origin.clone();
        let browser = tokio::spawn(async move {
            request_client
                .get(format!("{request_origin}/api/status"))
                .header("Remote-User", "owner")
                .send()
                .await
                .unwrap()
        });
        let request: GatewayMessage = serde_json::from_str(
            reconnected
                .next()
                .await
                .unwrap()
                .unwrap()
                .into_text()
                .unwrap()
                .as_str(),
        )
        .unwrap();
        let GatewayMessage::Request { request_id, .. } = request else {
            panic!("expected explicit read request")
        };
        reconnected
            .send(ClientMessage::Text(
                serde_json::to_string(&HostMessage::Response {
                    request_id,
                    status: 200,
                    body: json!({"workspace":"fixture"}).to_string(),
                })
                .unwrap()
                .into(),
            ))
            .await
            .unwrap();
        assert_eq!(browser.await.unwrap().status(), StatusCode::OK);
        server.abort();
    }
}
