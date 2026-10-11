//! The outbound prompt must stay append-only for servers that cache it.
//! llama.cpp keeps `reasoning_content` in the rendered transcript, so the
//! reasoning goes back to it (and only to it, unless configured), and derived
//! extraction requests switch thinking off there.
use axum::{
    Json, Router,
    extract::State,
    routing::{get, post},
};
use builder_core::{
    config::{Profile, ReasoningHistory},
    protocol::{Message, Role},
};
use builder_provider::{OpenAiCompatible, Provider};
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};

type Requests = Arc<Mutex<Vec<Value>>>;

async fn server(props: bool) -> (String, Requests) {
    let requests: Requests = Arc::new(Mutex::new(Vec::new()));
    let mut router = Router::new().route(
        "/v1/chat/completions",
        post(|State(requests): State<Requests>, Json(body): Json<Value>| async move {
            requests.lock().unwrap().push(body);
            Json(json!({"choices":[{"message":{"role":"assistant","content":"ok"},"finish_reason":"stop"}]}))
        }),
    );
    if props {
        router = router.route("/props", get(|| async { Json(json!({"total_slots": 4})) }));
    }
    let app = router.with_state(requests.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/v1", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (url, requests)
}

fn history() -> Vec<Message> {
    let mut call = Message::text(Role::Assistant, "Writing it now.");
    call.reasoning = Some("The user wants the file first.".into());
    vec![
        Message::text(Role::System, "You are Builder."),
        Message::text(Role::User, "Write it."),
        call,
        Message::text(Role::User, "Thanks."),
    ]
}

async fn body(
    url: &str,
    requests: &Requests,
    history: ReasoningHistory,
    thinking_off: bool,
) -> Value {
    let profile = Profile {
        base_url: url.into(),
        stream: false,
        max_attempts: 1,
        reasoning_history: history,
        ..Profile::default()
    };
    let mut provider = OpenAiCompatible::new(profile).unwrap();
    if thinking_off {
        provider = provider.without_thinking();
    }
    provider
        .complete(&self::history(), &[], &mut |_| {})
        .await
        .unwrap();
    requests.lock().unwrap().pop().unwrap()
}

fn assistant(body: &Value) -> &Value {
    body["messages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["role"] == "assistant")
        .unwrap()
}

#[tokio::test]
async fn auto_returns_reasoning_only_to_a_llama_cpp_server() {
    let (url, requests) = server(true).await;
    let sent = body(&url, &requests, ReasoningHistory::Auto, false).await;
    let turn = assistant(&sent);
    assert_eq!(turn["reasoning_content"], "The user wants the file first.");
    assert!(
        turn.get("reasoning").is_none(),
        "the internal field never leaves"
    );
    assert_eq!(turn["content"], "Writing it now.");

    let (url, requests) = server(false).await;
    let sent = body(&url, &requests, ReasoningHistory::Auto, false).await;
    let turn = assistant(&sent);
    assert!(turn.get("reasoning_content").is_none() && turn.get("reasoning").is_none());
}

#[tokio::test]
async fn explicit_send_and_strip_override_detection() {
    let (url, requests) = server(false).await;
    let sent = body(&url, &requests, ReasoningHistory::Send, false).await;
    assert_eq!(
        assistant(&sent)["reasoning_content"],
        "The user wants the file first."
    );

    let (url, requests) = server(true).await;
    let sent = body(&url, &requests, ReasoningHistory::Strip, false).await;
    assert!(assistant(&sent).get("reasoning_content").is_none());
}

#[tokio::test]
async fn extraction_requests_switch_thinking_off_where_the_switch_exists() {
    let (url, requests) = server(true).await;
    let sent = body(&url, &requests, ReasoningHistory::Auto, true).await;
    assert_eq!(sent["chat_template_kwargs"]["enable_thinking"], false);

    // No probe answer and no explicit entry: the field is not understood, so it is not sent.
    let (url, requests) = server(false).await;
    let sent = body(&url, &requests, ReasoningHistory::Auto, true).await;
    assert!(sent.get("chat_template_kwargs").is_none());

    // An explicit entry is understood even without the probe, and is overridden.
    let (url, requests) = server(false).await;
    let mut profile = Profile {
        base_url: url,
        stream: false,
        max_attempts: 1,
        ..Profile::default()
    };
    profile.extra_body.insert(
        "chat_template_kwargs".into(),
        json!({"enable_thinking": true, "reasoning_effort": "high"}),
    );
    OpenAiCompatible::new(profile)
        .unwrap()
        .without_thinking()
        .complete(&history(), &[], &mut |_| {})
        .await
        .unwrap();
    let sent = requests.lock().unwrap().pop().unwrap();
    assert_eq!(sent["chat_template_kwargs"]["enable_thinking"], false);
    assert_eq!(sent["chat_template_kwargs"]["reasoning_effort"], "high");
}
