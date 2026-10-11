//! Bounded request relay. Ambiguous mutations are never replayed.
use super::security::{identity, problem, require_origin};
use super::{MAX_PENDING_REQUESTS, PendingRequest, REQUEST_TIMEOUT, Shared};
use axum::{
    body::{Body, to_bytes},
    extract::{Request, State},
    http::{Method, StatusCode},
    response::{IntoResponse, Response},
};
use builder_remote_protocol::{GatewayMessage, MAX_REQUEST_BODY};
use std::sync::Arc;
use tokio::sync::oneshot;
use uuid::Uuid;

/// Own a registry slot for exactly the lifetime of its response future.
/// Dropping the future releases capacity; a dispatched host action is never
/// cancelled or replayed, and a late response is simply no longer observed.
struct PendingResponse<'a> {
    shared: &'a Shared,
    request_id: &'a str,
}

impl Drop for PendingResponse<'_> {
    fn drop(&mut self) {
        self.shared.pending.lock().unwrap().remove(self.request_id);
    }
}

pub(super) async fn proxy(State(shared): State<Arc<Shared>>, request: Request) -> Response {
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
    let _pending = PendingResponse {
        shared: &shared,
        request_id: &request_id,
    };
    let message = GatewayMessage::Request {
        request_id: request_id.clone(),
        method: method.to_string(),
        path,
        body,
    };
    if connection.outgoing.try_send(message).is_err() {
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
        Ok(Err(_)) | Err(_) => uncertain_response(&method),
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
