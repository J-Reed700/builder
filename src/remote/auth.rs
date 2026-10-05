use super::Shared;
use anyhow::{Result, ensure};
use axum::{
    Json,
    extract::{Request, State},
    http::{HeaderValue, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
};
use builder_core::store::Store;
use serde_json::json;
use std::{
    io::{Read, Write},
    path::Path,
    sync::Arc,
    time::Duration,
};
use subtle::ConstantTimeEq;
use uuid::Uuid;

pub(super) fn load_token(home: &Path) -> Result<String> {
    drop(Store::open(home)?);
    let path = home.join("remote-token");
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    match options.open(&path) {
        Ok(mut file) => {
            let token = format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple());
            file.write_all(token.as_bytes())?;
            file.sync_all()?;
            Ok(token)
        }
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            let metadata = std::fs::symlink_metadata(&path)?;
            ensure!(
                metadata.file_type().is_file(),
                "remote-token must be a regular file"
            );
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                ensure!(
                    metadata.permissions().mode() & 0o077 == 0,
                    "remote-token must be private: chmod 600 the token file"
                );
            }
            let mut token = String::new();
            std::fs::File::open(path)?
                .take(129)
                .read_to_string(&mut token)?;
            ensure!(
                token.len() == 64 && token.bytes().all(|b| b.is_ascii_hexdigit()),
                "Invalid remote-token; stop remote control and remove the file to regenerate it"
            );
            Ok(token)
        }
        Err(error) => Err(error.into()),
    }
}

pub(super) async fn headers(request: Request, next: Next) -> Response {
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

pub(super) async fn authenticate(
    State(shared): State<Arc<Shared>>,
    request: Request,
    next: Next,
) -> Response {
    let supplied = request
        .headers()
        .get("x-builder-token")
        .and_then(|h| h.to_str().ok())
        .unwrap_or("");
    if supplied.len() != shared.token.len()
        || !bool::from(supplied.as_bytes().ct_eq(shared.token.as_bytes()))
    {
        return problem(
            StatusCode::UNAUTHORIZED,
            "Enter the host's remote-control token",
        );
    }
    // No cookie auth or CORS. Browser writes must come from the configured exact origin.
    let origin = request
        .headers()
        .get("origin")
        .and_then(|h| h.to_str().ok());
    if origin.is_some_and(|origin| origin != shared.options.origin)
        || (request.method() != axum::http::Method::GET
            && origin != Some(shared.options.origin.as_str()))
    {
        return problem(
            StatusCode::FORBIDDEN,
            "Browser origin differs from builder remote --origin",
        );
    }
    let Ok(_permit) = shared.readers.clone().try_acquire_owned() else {
        return problem(StatusCode::TOO_MANY_REQUESTS, "Too many requests");
    };
    match tokio::time::timeout(Duration::from_secs(10), next.run(request)).await {
        Ok(response) => response,
        Err(_) => problem(
            StatusCode::REQUEST_TIMEOUT,
            "Request timed out; refresh status before taking another action",
        ),
    }
}

pub(super) fn problem(code: StatusCode, message: &str) -> Response {
    (code, Json(json!({"error":message}))).into_response()
}
pub(super) fn failure(error: anyhow::Error) -> Response {
    problem(StatusCode::CONFLICT, &format!("{error:#}"))
}
