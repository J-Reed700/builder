//! Public relay for Builder's outbound remote-control connection.
mod connection;
mod pairing;
mod proxy;
mod security;
mod storage;

use anyhow::{Context, Result};
use axum::{
    Router,
    extract::DefaultBodyLimit,
    http::HeaderName,
    middleware,
    routing::{any, get, post},
};
use builder_remote_protocol::{GatewayMessage, MAX_REQUEST_BODY};
use connection::connect_host;
use pairing::{create_invitation, gateway_status};
use proxy::proxy;
use security::{health, security_headers, validate_origin};
use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use storage::{DeviceRecord, create_private_directory, load_device};
use tokio::sync::{mpsc, oneshot};

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

#[cfg(test)]
mod tests;
