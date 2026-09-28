use axum::{
    Router,
    extract::Query,
    http::{HeaderMap, StatusCode},
    routing::get,
};
use builder_core::config::Profile;
use builder_provider::{OpenAiCompatible, Provider};
use std::{
    collections::HashMap,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};

#[tokio::test]
async fn capacity_probe_preserves_proxy_prefix_auth_and_model_and_caches_results() {
    let hits = Arc::new(AtomicUsize::new(0));
    let count = hits.clone();
    let app = Router::new().route(
        "/proxy/props",
        get(
            move |headers: HeaderMap, Query(query): Query<HashMap<String, String>>| {
                let count = count.clone();
                async move {
                    assert_eq!(headers["authorization"], "Bearer test-key");
                    assert_eq!(query["model"], "test-model");
                    count.fetch_add(1, Ordering::SeqCst);
                    axum::Json(serde_json::json!({"total_slots": 4}))
                }
            },
        ),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let mut profile = Profile {
        base_url: format!("http://{}/proxy/v1/", listener.local_addr().unwrap()),
        model: "test-model".into(),
        ..Profile::default()
    };
    profile.headers.insert(
        "authorization".into(),
        builder_core::config::Secret::Literal("Bearer test-key".into()),
    );
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let provider = OpenAiCompatible::new(profile).unwrap();
    let (a, b) = tokio::join!(provider.parallel_capacity(), provider.parallel_capacity());
    assert_eq!((a, b), (Some(4), Some(4)));
    assert_eq!(hits.load(Ordering::SeqCst), 1);
    server.abort();
}

#[tokio::test]
async fn missing_or_invalid_capacity_is_unknown_and_cached() {
    for (status, body) in [
        (StatusCode::NOT_FOUND, "{}"),
        (StatusCode::UNAUTHORIZED, "{}"),
        (StatusCode::OK, "not json"),
        (StatusCode::OK, "{}"),
        (StatusCode::OK, r#"{"total_slots":0}"#),
        (StatusCode::OK, r#"{"total_slots":-1}"#),
        (StatusCode::OK, r#"{"total_slots":"4"}"#),
    ] {
        let hits = Arc::new(AtomicUsize::new(0));
        let count = hits.clone();
        let app = Router::new().route(
            "/props",
            get(move || {
                count.fetch_add(1, Ordering::SeqCst);
                async move { (status, body) }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let profile = Profile {
            base_url: format!("http://{}/v1", listener.local_addr().unwrap()),
            ..Profile::default()
        };
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let provider = OpenAiCompatible::new(profile).unwrap();
        assert_eq!(provider.parallel_capacity().await, None);
        assert_eq!(provider.parallel_capacity().await, None);
        assert_eq!(hits.load(Ordering::SeqCst), 1);
        server.abort();
    }
}
