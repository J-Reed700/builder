//! Anthropic Messages protocol through the shared `Endpoint` transport,
//! against an in-process server: auth headers, request encoding, streamed and
//! buffered decoding, and the shared retry policy.
use axum::{
    Json, Router,
    extract::State,
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use builder_core::{
    config::{Api, Profile, Secret},
    protocol::{Function, Message, Role, ToolCall},
};
use builder_provider::{Endpoint, Event, OutputLimit, Provider};
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};

type Seen = Arc<Mutex<Vec<(HeaderMap, Value)>>>;
type Replies = Arc<Mutex<Vec<Response>>>;

fn sse(events: &[Value]) -> Response {
    let body: String = events
        .iter()
        .map(|e| format!("event: {}\ndata: {e}\n\n", e["type"].as_str().unwrap()))
        .collect();
    ([("content-type", "text/event-stream")], body).into_response()
}

/// Serves `replies` in order on `/v1/messages` and a model list on `/v1/models`,
/// recording every request's headers and body.
async fn server(replies: Vec<Response>) -> (String, Seen, tokio::task::JoinHandle<()>) {
    let seen: Seen = Default::default();
    let replies: Replies = Arc::new(Mutex::new(replies.into_iter().rev().collect()));
    let app = Router::new()
        .route(
            "/v1/messages",
            post(
                |State((seen, replies)): State<(Seen, Replies)>,
                 headers: HeaderMap,
                 Json(body): Json<Value>| async move {
                    seen.lock().unwrap().push((headers, body));
                    replies
                        .lock()
                        .unwrap()
                        .pop()
                        .expect("unexpected extra request")
                },
            ),
        )
        .route(
            "/v1/models",
            get(
                |State((seen, _)): State<(Seen, Replies)>, headers: HeaderMap| async move {
                    seen.lock().unwrap().push((headers, Value::Null));
                    Json(json!({"data":[{"id":"claude-test"}]}))
                },
            ),
        )
        .with_state((seen.clone(), replies));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/v1", listener.local_addr().unwrap());
    let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (url, seen, task)
}

fn profile(url: String, token: &str, stream: bool) -> Profile {
    Profile {
        api: Api::Anthropic,
        base_url: url,
        model: "claude-test".into(),
        api_key: Some(Secret::Literal(token.into())),
        stream,
        context_tokens: 200_000,
        max_output_tokens: 1024,
        max_attempts: 3,
        ..Default::default()
    }
}

fn read_tool() -> Value {
    json!({"type":"function","function":{"name":"read","description":"Read a file",
        "parameters":{"type":"object","properties":{"path":{"type":"string"}}}}})
}

#[tokio::test]
async fn oauth_stream_decodes_text_and_tool_calls_after_retrying_overload() {
    let streamed = sse(&[
        json!({"type":"message_start","message":{"role":"assistant","content":[]}}),
        json!({"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":""}}),
        json!({"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"plan"}}),
        json!({"type":"content_block_stop","index":0}),
        json!({"type":"content_block_start","index":1,"content_block":{"type":"text","text":""}}),
        json!({"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":"Hel"}}),
        json!({"type":"ping"}),
        json!({"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":"lo"}}),
        json!({"type":"content_block_stop","index":1}),
        json!({"type":"content_block_start","index":2,"content_block":{"type":"tool_use","id":"toolu_1","name":"read","input":{}}}),
        json!({"type":"content_block_delta","index":2,"delta":{"type":"input_json_delta","partial_json":"{\"pa"}}),
        json!({"type":"content_block_delta","index":2,"delta":{"type":"input_json_delta","partial_json":"th\":\"a\"}"}}),
        json!({"type":"content_block_stop","index":2}),
        json!({"type":"content_block_start","index":3,"content_block":{"type":"tool_use","id":"toolu_2","name":"read","input":{}}}),
        json!({"type":"content_block_stop","index":3}),
        json!({"type":"message_delta","delta":{"stop_reason":"tool_use"}}),
        json!({"type":"message_stop"}),
    ]);
    let overloaded = sse(&[
        json!({"type":"message_start","message":{"role":"assistant","content":[]}}),
        json!({"type":"error","error":{"type":"overloaded_error","message":"busy"}}),
    ]);
    let (url, seen, task) = server(vec![
        (
            StatusCode::from_u16(529).unwrap(),
            [("retry-after", "0")],
            "",
        )
            .into_response(),
        overloaded,
        streamed,
    ])
    .await;
    let endpoint = Endpoint::new(profile(url, "sk-ant-oat01-test", true)).unwrap();
    let mut assistant = Message::text(Role::Assistant, "");
    assistant.tool_calls = vec![ToolCall {
        id: "call:earlier".into(),
        kind: "function".into(),
        function: Function {
            name: "read".into(),
            arguments: r#"{"path":"b"}"#.into(),
        },
    }];
    let history = [
        Message::text(Role::System, "policy"),
        Message::text(Role::User, "read a"),
        assistant,
        Message::tool("call:earlier", "contents".into()),
    ];
    let mut events = Vec::new();
    let reply = endpoint
        .complete(&history, &[read_tool()], &mut |e| events.push(e))
        .await
        .unwrap();

    assert_eq!(reply.content.as_deref(), Some("Hello"));
    assert_eq!(reply.reasoning.as_deref(), Some("plan"));
    assert_eq!(reply.tool_calls.len(), 2);
    assert_eq!(reply.tool_calls[0].id, "toolu_1");
    assert_eq!(reply.tool_calls[0].function.arguments, r#"{"path":"a"}"#);
    // A tool call with no input streams no JSON; it is an empty object.
    assert_eq!(reply.tool_calls[1].function.arguments, "{}");
    assert_eq!(
        events
            .iter()
            .filter(|e| matches!(e, Event::Retry { .. }))
            .count(),
        2
    );
    assert!(
        events
            .iter()
            .any(|e| matches!(e, Event::ToolProgress { .. }))
    );

    let seen = seen.lock().unwrap();
    assert_eq!(seen.len(), 3);
    for (headers, body) in seen.iter() {
        assert_eq!(headers["authorization"], "Bearer sk-ant-oat01-test");
        assert!(!headers.contains_key("x-api-key"));
        assert_eq!(headers["anthropic-version"], "2023-06-01");
        assert!(
            headers["anthropic-beta"]
                .to_str()
                .unwrap()
                .contains("oauth-2025-04-20")
        );
        assert_eq!(headers["x-app"], "cli");
        // Every attempt sends the identical request.
        assert_eq!(body, &seen[0].1);
    }
    let body = &seen[0].1;
    assert_eq!(body["model"], "claude-test");
    assert_eq!(body["stream"], true);
    assert_eq!(body["max_tokens"], 1024);
    assert!(
        body["system"][0]["text"]
            .as_str()
            .unwrap()
            .starts_with("You are Claude Code")
    );
    assert_eq!(body["system"][1]["text"], "policy");
    let wire = body["messages"].as_array().unwrap();
    assert_eq!(wire[1]["content"][0]["type"], "tool_use");
    assert_eq!(wire[1]["content"][0]["id"], "call_earlier");
    assert_eq!(wire[2]["content"][0]["tool_use_id"], "call_earlier");
    assert_eq!(body["tools"][0]["name"], "read");
    assert_eq!(body["tools"][0]["input_schema"]["type"], "object");
    task.abort();
}

#[tokio::test]
async fn api_key_buffered_reply_models_and_output_limit() {
    let (url, seen, task) = server(vec![
        Json(json!({"role":"assistant","stop_reason":"end_turn",
            "content":[{"type":"text","text":"done"}]}))
        .into_response(),
        Json(json!({"role":"assistant","stop_reason":"max_tokens",
            "content":[{"type":"text","text":"trunc"}]}))
        .into_response(),
    ])
    .await;
    let endpoint = Endpoint::new(profile(url, "sk-ant-api03-test", false)).unwrap();
    assert_eq!(
        endpoint.models().await.unwrap()["data"][0]["id"],
        "claude-test"
    );
    let reply = endpoint
        .complete(&[Message::text(Role::User, "hi")], &[], &mut |_| {})
        .await
        .unwrap();
    assert_eq!(reply.content.as_deref(), Some("done"));
    let error = endpoint
        .complete(&[Message::text(Role::User, "hi")], &[], &mut |_| {})
        .await
        .unwrap_err();
    assert!(error.is::<OutputLimit>());
    // Hosted Claude has no llama.cpp /props or /embeddings.
    assert_eq!(endpoint.parallel_capacity().await, None);
    assert!(endpoint.embed("text").await.is_err());

    let seen = seen.lock().unwrap();
    for (headers, _) in seen.iter() {
        assert_eq!(headers["x-api-key"], "sk-ant-api03-test");
        assert!(!headers.contains_key("authorization"));
        assert!(!headers.contains_key("anthropic-beta"));
    }
    assert_eq!(seen[1].1["stream"], false);
    assert!(seen[1].1.get("system").is_none());
    task.abort();
}

#[test]
fn credentials_resolve_in_documented_order() {
    // An unset profile env reference fails instead of falling back silently.
    let p = Profile {
        api: Api::Anthropic,
        api_key_env: Some("BUILDER_TEST_INTENTIONALLY_UNSET_ANTHROPIC_KEY_5521".into()),
        ..Default::default()
    };
    assert!(Endpoint::new(p).is_err());
    // An explicit proxy header wins and needs no Claude credential at all.
    let p = Profile {
        api: Api::Anthropic,
        api_key_env: Some("BUILDER_TEST_INTENTIONALLY_UNSET_ANTHROPIC_KEY_5521".into()),
        headers: [("x-api-key".to_string(), Secret::Literal("proxy".into()))].into(),
        ..Default::default()
    };
    assert!(Endpoint::new(p).is_ok());
}
