use super::*;
use axum::{
    body::Body,
    extract::State,
    http::{Request, StatusCode},
};
use builder_remote_protocol::{Credential, GatewayMessage, HostMessage, PROTOCOL_VERSION};
use futures_util::{SinkExt, StreamExt};
use reqwest::Client;
use serde_json::json;
use tokio_tungstenite::{connect_async, tungstenite::Message as ClientMessage};
use uuid::Uuid;

#[tokio::test]
async fn cancelled_proxy_releases_capacity_without_replaying_the_request() {
    let data = tempfile::tempdir().unwrap();
    let gateway = Gateway::new(GatewayOptions {
        origin: "https://builder.example.com".into(),
        identity_header: Some("Remote-User".into()),
        data: data.path().into(),
    })
    .unwrap();
    let shared = gateway.shared;
    let (outgoing, mut host) = mpsc::channel(MAX_PENDING_REQUESTS);
    *shared.connection.lock().unwrap() = Some(Connection {
        generation: "connection".into(),
        device_id: "device".into(),
        owner: "user".into(),
        outgoing,
    });

    // More cancellations than the registry capacity must leave room for a
    // subsequent request. Each mutation reaches the host exactly once here;
    // cancelling the response does not authorize any replay.
    for _ in 0..=MAX_PENDING_REQUESTS {
        let request = Request::builder()
            .method("POST")
            .uri("/api/chats")
            .header("Remote-User", "user")
            .header("Origin", "https://builder.example.com")
            .body(Body::from("{}"))
            .unwrap();
        let mut response = Box::pin(proxy::proxy(State(shared.clone()), request));
        let dispatched = tokio::select! {
            _ = &mut response => panic!("Proxy returned before the host responded"),
            message = host.recv() => message.unwrap(),
        };
        assert!(matches!(dispatched, GatewayMessage::Request { .. }));
        assert_eq!(shared.pending.lock().unwrap().len(), 1);
        drop(response);
        assert!(shared.pending.lock().unwrap().is_empty());
        assert!(
            host.try_recv().is_err(),
            "Cancellation must not replay a mutation"
        );
    }
}

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
