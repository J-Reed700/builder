//! Invitation lifecycle and authenticated device enrollment.
use super::security::{
    constant_time_equal, hash, identity, is_hex_secret, problem, require_origin, secret,
};
use super::storage::{DeviceRecord, save_device};
use super::{CompletedEnrollment, INVITATION_LIFETIME, Invitation, MAX_INVITATIONS, Shared};
use anyhow::{Context, Result, ensure};
use axum::{
    Json,
    extract::State,
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
};
use builder_remote_protocol::Credential;
use serde_json::json;
use std::{sync::Arc, time::Instant};

pub(super) async fn gateway_status(
    State(shared): State<Arc<Shared>>,
    headers: HeaderMap,
) -> Response {
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

pub(super) async fn create_invitation(
    State(shared): State<Arc<Shared>>,
    headers: HeaderMap,
) -> Response {
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

pub(super) struct AuthenticatedHost {
    pub(super) owner: String,
    pub(super) new_token: Option<String>,
}

pub(super) fn authenticate_host(
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
