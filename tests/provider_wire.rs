//! Images leave the process as OpenAI multimodal content, only for profiles
//! that can read them, and the internal `images` field never crosses the wire.
use axum::{Json, Router, extract::State, routing::post};
use builder_core::{
    config::Profile,
    protocol::{Image, Message, Role},
};
use builder_provider::{OpenAiCompatible, Provider};
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};

type Requests = Arc<Mutex<Vec<Value>>>;

/// Send a system prompt and an image-bearing user message through a profile
/// and return the request body the server received.
async fn send_image(vision: bool) -> Value {
    let requests: Requests = Default::default();
    let app = Router::new()
        .route(
            "/v1/chat/completions",
            post(
                |State(requests): State<Requests>, Json(body): Json<Value>| async move {
                    requests.lock().unwrap().push(body);
                    Json(
                        json!({"choices":[{"message":{"role":"assistant","content":"ok"},"finish_reason":"stop"}]}),
                    )
                },
            ),
        )
        .with_state(requests.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let profile = Profile {
        base_url: format!("http://{}/v1", listener.local_addr().unwrap()),
        stream: false,
        max_attempts: 1,
        vision,
        ..Profile::default()
    };
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let provider = OpenAiCompatible::new(profile).unwrap();
    let image = Image {
        mime: "image/png".into(),
        data: "aGVsbG8=".into(),
    };
    provider
        .complete(
            &[
                Message::text(Role::System, "You are Builder."),
                Message::text(Role::User, "read this").with_images(vec![image]),
            ],
            &[],
            &mut |_| {},
        )
        .await
        .unwrap();
    server.abort();
    requests.lock().unwrap().pop().unwrap()
}

fn user(sent: &Value) -> &Value {
    sent["messages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["role"] == "user")
        .unwrap()
}

#[tokio::test]
async fn image_payloads_go_out_as_multimodal_content_and_the_field_stays_internal() {
    let sent = send_image(true).await;
    let user = user(&sent);
    assert_eq!(
        user["content"],
        json!([
            { "type": "text", "text": "read this" },
            {
                "type": "image_url",
                "image_url": { "url": "data:image/png;base64,aGVsbG8=" }
            }
        ])
    );
    assert!(
        user.get("images").is_none(),
        "internal field must not leave"
    );
    // Text-only history keeps the plain string shape.
    let system = &sent["messages"][0];
    assert_eq!(system["content"], "You are Builder.");
    assert!(system.get("images").is_none());
}

#[tokio::test]
async fn a_profile_without_vision_gets_a_note_instead_of_image_parts() {
    // History pasted under a vision model must not wedge a text-only model:
    // servers reject image parts, and every retry would resend them.
    let sent = send_image(false).await;
    let user = user(&sent);
    assert_eq!(
        user["content"],
        "read this\n[1 image attached; not shown here]"
    );
    assert!(user.get("images").is_none());
    assert!(!sent.to_string().contains("aGVsbG8="));
}
