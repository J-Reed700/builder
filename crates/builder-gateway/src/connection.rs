//! Host WebSocket lifecycle, heartbeats, and connection generation ownership.
use super::pairing::authenticate_host;
use super::{Connection, DeviceResponse, MAX_PENDING_REQUESTS, Shared};
use anyhow::{Result, ensure};
use axum::{
    extract::{
        State, WebSocketUpgrade,
        ws::{Message, WebSocket},
    },
    response::Response,
};
use builder_remote_protocol::{
    GatewayMessage, HostMessage, MAX_RESPONSE_BODY, MAX_WIRE_MESSAGE, PROTOCOL_VERSION,
};
use futures_util::SinkExt;
use std::{sync::Arc, time::Duration};
use tokio::sync::mpsc;
use uuid::Uuid;

pub(super) async fn connect_host(
    State(shared): State<Arc<Shared>>,
    upgrade: WebSocketUpgrade,
) -> Response {
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
