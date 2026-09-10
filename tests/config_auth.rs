use axum::{
    Json, Router,
    extract::State,
    http::{HeaderMap, StatusCode},
    routing::{get, post},
};
use builder_core::{
    config::{CompletionOptions, Profile, Secret},
    protocol::{Message, Role},
};
use builder_provider::{OpenAiCompatible, Provider};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
};

type Requests = Arc<Mutex<Vec<(HeaderMap, Value)>>>;
#[tokio::test]
async fn custom_auth_survives_discovery_generation_and_retry_without_duplicate_bearer() {
    let requests: Requests = Default::default();
    let app = Router::new()
        .route("/v1/models", get(|State(seen): State<Requests>, headers: HeaderMap| async move {
            seen.lock().unwrap().push((headers, Value::Null));
            Json(json!({"data":[{"id":"Qwen3.8.gguf"}]}))
        }))
        .route("/v1/chat/completions", post(|State(seen): State<Requests>, headers: HeaderMap, Json(body): Json<Value>| async move {
            let mut seen = seen.lock().unwrap();
            seen.push((headers, body));
            if seen.len() == 2 { (StatusCode::SERVICE_UNAVAILABLE, [("retry-after", "0")], Json(json!({}))) }
            else { (StatusCode::OK, [("retry-after", "0")], Json(json!({"choices":[{"finish_reason":"stop","message":{"role":"assistant","content":"connected"}}]}))) }
        }))
        .with_state(requests.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let p = Profile {
        base_url: format!("http://{}/v1", listener.local_addr().unwrap()),
        model: "Qwen3.8.gguf".into(),
        // Must not even resolve the shadowed bearer key.
        api_key_env: Some("BUILDER_TEST_INTENTIONALLY_UNSET_SHADOWED_KEY_238199".into()),
        headers: BTreeMap::from([
            (
                "authorization".into(),
                Secret::Literal("Basic synthetic-test-secret".into()),
            ),
            (
                "X-Gateway-Token".into(),
                Secret::Literal("gateway-test".into()),
            ),
        ]),
        completion: CompletionOptions {
            temperature: Some(0.6),
            top_p: Some(0.85),
            top_k: Some(20),
            min_p: Some(0.0),
            presence_penalty: Some(0.0),
            frequency_penalty: Some(0.0),
            ..Default::default()
        },
        extra_body: BTreeMap::from([(
            "chat_template_kwargs".into(),
            json!({"enable_thinking":false}),
        )]),
        stream: false,
        max_output_tokens: 8192,
        ..Default::default()
    };
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let provider = OpenAiCompatible::new(p).unwrap();
    assert_eq!(
        provider.models().await.unwrap()["data"][0]["id"],
        "Qwen3.8.gguf"
    );
    provider
        .complete(&[Message::text(Role::User, "hello")], &[], &mut |_| {})
        .await
        .unwrap();
    let requests = requests.lock().unwrap();
    assert_eq!(requests.len(), 3);
    for (headers, _) in requests.iter() {
        assert_eq!(headers.get_all("authorization").iter().count(), 1);
        assert_eq!(headers["authorization"], "Basic synthetic-test-secret");
        assert_eq!(headers["x-gateway-token"], "gateway-test");
    }
    let body = &requests[1].1;
    assert_eq!(body, &requests[2].1);
    assert_eq!(body["temperature"], 0.6);
    assert_eq!(body["top_p"], 0.85);
    assert_eq!(body["top_k"], 20);
    assert_eq!(body["min_p"], 0.0);
    assert_eq!(body["presence_penalty"], 0.0);
    assert_eq!(body["frequency_penalty"], 0.0);
    assert_eq!(body["max_tokens"], 8192);
    assert_eq!(body["chat_template_kwargs"]["enable_thinking"], false);
    server.abort();
}

#[test]
fn invalid_headers_and_missing_credentials_fail_without_echoing_values() {
    let p = Profile {
        headers: BTreeMap::from([(
            "Authorization".into(),
            Secret::Literal("private-secret\r\nx-injected: true".into()),
        )]),
        ..Default::default()
    };
    let error = OpenAiCompatible::new(p).err().unwrap();
    assert!(!format!("{error:#}").contains("private-secret"));
    let p = Profile {
        api_key_env: Some("BUILDER_TEST_INTENTIONALLY_UNSET_KEY_238199".into()),
        ..Default::default()
    };
    assert!(OpenAiCompatible::new(p).is_err());
    let p = Profile {
        headers: BTreeMap::from([
            ("Authorization".into(), Secret::Literal("one".into())),
            ("authorization".into(), Secret::Literal("two".into())),
        ]),
        ..Default::default()
    };
    assert!(OpenAiCompatible::new(p).is_err());
}

#[tokio::test]
async fn embedding_requests_preserve_auth_and_reject_invalid_responses() {
    let seen: Requests = Default::default();
    let app = Router::new().route("/v1/embeddings", post(|State(seen): State<Requests>, headers: HeaderMap, Json(body): Json<Value>| async move {
        seen.lock().unwrap().push((headers, body.clone()));
        let data = match body["input"].as_str().unwrap() {
            "valid" => json!({"data":[{"index":0,"embedding":[0.5,-0.5]}]}),
            "zero" => json!({"data":[{"index":0,"embedding":[0.0,0.0]}]}),
            "overflow" => json!({"data":[{"index":0,"embedding":[1e100,1.0]}]}),
            "multiple" => json!({"data":[{"index":0,"embedding":[1.0]},{"index":1,"embedding":[1.0]}]}),
            "wrong-index" => json!({"data":[{"index":1,"embedding":[1.0]}]}),
            _ => return (StatusCode::BAD_GATEWAY, Json(json!({"error":"secret-provider-detail"}))),
        };
        (StatusCode::OK, Json(data))
    })).with_state(seen.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let profile = Profile {
        base_url: format!("http://{}/v1", listener.local_addr().unwrap()),
        model: "test-embed".into(),
        headers: BTreeMap::from([(
            "Authorization".into(),
            Secret::Literal("Basic synthetic".into()),
        )]),
        extra_body: BTreeMap::from([("chat_only".into(), json!(true))]),
        ..Default::default()
    };
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let provider = OpenAiCompatible::new(profile).unwrap();
    assert_eq!(provider.embed("valid").await.unwrap(), vec![0.5, -0.5]);
    for input in ["zero", "overflow", "multiple", "wrong-index", "failure"] {
        let error = provider.embed(input).await.unwrap_err().to_string();
        assert!(!error.contains("secret-provider-detail"));
    }
    assert!(provider.embed("").await.is_err());
    assert!(provider.embed(&"x".repeat(8193)).await.is_err());
    let seen = seen.lock().unwrap();
    assert_eq!(seen.len(), 6);
    for (headers, body) in seen.iter() {
        assert_eq!(headers["authorization"], "Basic synthetic");
        assert_eq!(headers.get_all("authorization").iter().count(), 1);
        assert_eq!(body["model"], "test-embed");
        assert!(body.get("chat_only").is_none());
    }
    server.abort();
}
