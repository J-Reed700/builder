use super::Shared;
use anyhow::{Context, Result, ensure};
use axum::{
    Json,
    extract::Request,
    http::{HeaderMap, HeaderValue, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
};
use serde_json::json;
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;
use uuid::Uuid;

pub(super) fn validate_origin(origin: &str) -> Result<()> {
    ensure!(
        origin.starts_with("http://") || origin.starts_with("https://"),
        "BUILDER_GATEWAY_ORIGIN must start with http:// or https://"
    );
    let authority = origin.split_once("://").unwrap().1;
    ensure!(
        !authority.is_empty()
            && !authority.contains(['/', '?', '#', '@', '"', '\''])
            && !authority.chars().any(char::is_whitespace),
        "BUILDER_GATEWAY_ORIGIN must be an exact browser origin"
    );
    HeaderValue::from_str(origin).context("Invalid BUILDER_GATEWAY_ORIGIN")?;
    Ok(())
}

pub(super) fn secret() -> String {
    format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple())
}

pub(super) fn hash(value: &str) -> String {
    format!("{:x}", Sha256::digest(value.as_bytes()))
}

pub(super) fn is_hex_secret(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

pub(super) fn constant_time_equal(left: &str, right: &str) -> bool {
    left.len() == right.len() && bool::from(left.as_bytes().ct_eq(right.as_bytes()))
}

pub(super) async fn security_headers(request: Request, next: Next) -> Response {
    let mut response = next.run(request).await;
    for (key, value) in [
        ("cache-control", "no-store"),
        ("x-content-type-options", "nosniff"),
        ("referrer-policy", "no-referrer"),
        ("x-frame-options", "DENY"),
        (
            "content-security-policy",
            "default-src 'none'; script-src 'self'; style-src 'self'; connect-src 'self'; img-src 'self'; base-uri 'none'; frame-ancestors 'none'; form-action 'self'",
        ),
    ] {
        response
            .headers_mut()
            .insert(key, HeaderValue::from_static(value));
    }
    response
}

pub(super) async fn health() -> &'static str {
    "ok"
}

pub(super) fn problem(status: StatusCode, message: impl Into<String>) -> Response {
    (status, Json(json!({"error": message.into()}))).into_response()
}

pub(super) fn identity(headers: &HeaderMap, shared: &Shared) -> Result<String, Box<Response>> {
    let Some(header) = &shared.identity_header else {
        return Ok("local".into());
    };
    let value = headers
        .get(header)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("");
    if value.is_empty() || value.len() > 256 {
        return Err(Box::new(problem(
            StatusCode::UNAUTHORIZED,
            "Sign in through the configured gateway before using Builder",
        )));
    }
    Ok(value.into())
}

pub(super) fn require_origin(headers: &HeaderMap, shared: &Shared) -> Result<(), Box<Response>> {
    if headers.get("origin").and_then(|value| value.to_str().ok()) != Some(shared.origin.as_str()) {
        return Err(Box::new(problem(
            StatusCode::FORBIDDEN,
            "Browser origin does not match the gateway",
        )));
    }
    Ok(())
}
