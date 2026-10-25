//! Assistant reasoning stays out of the outbound history unless the profile
//! asks for it, on llama.cpp too, and derived extraction requests switch
//! thinking off where the switch is understood.
use axum::{
    Json, Router,
    extract::State,
    routing::{get, post},
};
use builder_core::{
    config::{Profile, ReasoningHistory},
    protocol::{Message, Role},
};
use builder_provider::{Endpoint, Provider};
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
    let mut provider = Endpoint::new(profile).unwrap();
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
async fn reasoning_stays_out_of_the_history_by_default_even_on_llama_cpp() {
    for props in [true, false] {
        let (url, requests) = server(props).await;
        let sent = body(&url, &requests, ReasoningHistory::default(), false).await;
        let turn = assistant(&sent);
        assert!(
            turn.get("reasoning_content").is_none() && turn.get("reasoning").is_none(),
            "props: {props}"
        );
        assert_eq!(turn["content"], "Writing it now.");
    }
}

#[test]
fn the_old_auto_setting_reads_as_strip() {
    #[derive(serde::Deserialize)]
    struct Setting {
        reasoning_history: ReasoningHistory,
    }
    let setting: Setting = toml::from_str("reasoning_history = \"auto\"").unwrap();
    assert_eq!(setting.reasoning_history, ReasoningHistory::Strip);
}

#[tokio::test]
async fn send_returns_reasoning_and_strip_never_does() {
    let (url, requests) = server(false).await;
    let sent = body(&url, &requests, ReasoningHistory::Send, false).await;
    let turn = assistant(&sent);
    assert_eq!(turn["reasoning_content"], "The user wants the file first.");
    assert!(
        turn.get("reasoning").is_none(),
        "the internal field never leaves"
    );

    let (url, requests) = server(true).await;
    let sent = body(&url, &requests, ReasoningHistory::Strip, false).await;
    assert!(assistant(&sent).get("reasoning_content").is_none());
}

#[tokio::test]
async fn extraction_requests_switch_thinking_off_where_the_switch_exists() {
    let (url, requests) = server(true).await;
    let sent = body(&url, &requests, ReasoningHistory::default(), true).await;
    assert_eq!(sent["chat_template_kwargs"]["enable_thinking"], false);

    // No probe answer and no explicit entry: the field is not understood, so it is not sent.
    let (url, requests) = server(false).await;
    let sent = body(&url, &requests, ReasoningHistory::default(), true).await;
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
    Endpoint::new(profile)
        .unwrap()
        .without_thinking()
        .complete(&history(), &[], &mut |_| {})
        .await
        .unwrap();
    let sent = requests.lock().unwrap().pop().unwrap();
    assert_eq!(sent["chat_template_kwargs"]["enable_thinking"], false);
    assert_eq!(sent["chat_template_kwargs"]["reasoning_effort"], "high");
}
