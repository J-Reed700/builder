//! The real gateway and the real host connector, paired in-process: enrollment,
//! the saved device credential, and requests relayed to the host's remote API.
use builder::{
    agent::ApprovalMode,
    remote::{RemoteControl, RemoteOptions},
    remote_connect::{ConnectOptions, serve},
};
use builder_gateway::{Gateway, GatewayOptions};
use serde_json::Value;
use std::{path::Path, time::Duration};

struct GatewayServer {
    origin: String,
    task: tokio::task::JoinHandle<()>,
    _data: tempfile::TempDir,
}
impl Drop for GatewayServer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn gateway() -> GatewayServer {
    let data = tempfile::tempdir().unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let gateway = Gateway::new(GatewayOptions {
        origin: origin.clone(),
        identity_header: Some("Remote-User".into()),
        data: data.path().into(),
    })
    .unwrap();
    let task = tokio::spawn(async move {
        axum::serve(listener, gateway.router()).await.unwrap();
    });
    GatewayServer {
        origin,
        task,
        _data: data,
    }
}

async fn invitation(client: &reqwest::Client, origin: &str) -> String {
    let value: Value = client
        .post(format!("{origin}/api/gateway/invitations"))
        .header("Remote-User", "owner")
        .header("Origin", origin)
        .json(&serde_json::json!({}))
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

async fn gateway_status(client: &reqwest::Client, origin: &str, user: &str) -> Value {
    client
        .get(format!("{origin}/api/gateway/status"))
        .header("Remote-User", user)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap()
}

fn host(home: &Path, workspace: &Path, origin: &str) -> RemoteControl {
    RemoteControl::new(RemoteOptions {
        home: home.to_owned(),
        config_home: home.to_owned(),
        workspace: workspace.to_owned(),
        profile: None,
        approval: ApprovalMode::Ask,
        max_rounds: None,
        origin: origin.into(),
    })
    .unwrap()
}

fn options(origin: &str, code: Option<&str>, home: &Path, pair_only: bool) -> ConnectOptions {
    ConnectOptions {
        gateway: origin.into(),
        code: code.map(Into::into),
        home: home.to_owned(),
        pair_only,
    }
}

fn credential_home(control: &RemoteControl) -> std::path::PathBuf {
    control.token_path().parent().unwrap().to_owned()
}

fn saved_credentials(home: &Path) -> Vec<std::path::PathBuf> {
    std::fs::read_dir(home.join("remote-gateways"))
        .map(|entries| entries.map(|entry| entry.unwrap().path()).collect())
        .unwrap_or_default()
}

#[tokio::test]
async fn pairing_enrolls_once_saves_a_private_credential_and_reconnects_with_it() {
    let gateway = gateway().await;
    let client = reqwest::Client::new();
    let home = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let control = host(home.path(), workspace.path(), &gateway.origin);
    let credentials = credential_home(&control);
    let refused = |error: anyhow::Error, expected: &str| {
        assert!(error.to_string().contains(expected), "{error:#}");
    };

    refused(
        serve(
            &control,
            options(&gateway.origin, Some("short"), &credentials, true),
        )
        .await
        .unwrap_err(),
        "Pairing code must be the complete code shown by the gateway",
    );
    refused(
        serve(&control, options(&gateway.origin, None, &credentials, true))
            .await
            .unwrap_err(),
        "is not paired with",
    );
    refused(
        serve(
            &control,
            options(&gateway.origin, Some(&"0".repeat(64)), &credentials, true),
        )
        .await
        .unwrap_err(),
        "Gateway rejected this computer: Pairing code is invalid or expired",
    );
    assert!(saved_credentials(&credentials).is_empty());
    assert_eq!(
        gateway_status(&client, &gateway.origin, "owner").await["paired"],
        false
    );

    let code = invitation(&client, &gateway.origin).await;
    serve(
        &control,
        options(&gateway.origin, Some(&code), &credentials, true),
    )
    .await
    .unwrap();
    let status = gateway_status(&client, &gateway.origin, "owner").await;
    assert_eq!(status["paired"], true);
    assert!(
        !status["device"]["name"].as_str().unwrap().is_empty(),
        "{status}"
    );

    let files = saved_credentials(&credentials);
    assert_eq!(files.len(), 1, "{files:?}");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(&files[0]).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
    let saved: Value = serde_json::from_str(&std::fs::read_to_string(&files[0]).unwrap()).unwrap();
    assert_eq!(saved["version"], 1);
    assert_eq!(saved["gateway"], gateway.origin);
    assert_eq!(saved["device_id"], status["device"]["id"]);
    assert_eq!(saved["token"].as_str().unwrap().len(), 64);
    assert_ne!(saved["token"], code, "the pairing code is never stored");

    // Reconnecting uses the saved device credential; presenting the same code
    // again from the same computer is accepted as a retry.
    serve(&control, options(&gateway.origin, None, &credentials, true))
        .await
        .unwrap();
    serve(
        &control,
        options(&gateway.origin, Some(&code), &credentials, true),
    )
    .await
    .unwrap();
    assert_eq!(saved_credentials(&credentials).len(), 1);
    assert_eq!(
        gateway_status(&client, &gateway.origin, "owner").await["device"]["id"],
        saved["device_id"],
        "re-pairing keeps the device identity"
    );

    // Another computer cannot reuse the code, and another user cannot see the device.
    let other_home = tempfile::tempdir().unwrap();
    let other_workspace = tempfile::tempdir().unwrap();
    let other = host(other_home.path(), other_workspace.path(), &gateway.origin);
    let other_credentials = credential_home(&other);
    refused(
        serve(
            &other,
            options(&gateway.origin, Some(&code), &other_credentials, true),
        )
        .await
        .unwrap_err(),
        "already used by another computer",
    );
    assert!(saved_credentials(&other_credentials).is_empty());
    let response = client
        .get(format!("{}/api/gateway/status", gateway.origin))
        .header("Remote-User", "someone-else")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), reqwest::StatusCode::FORBIDDEN);

    // A tampered credential is refused before any connection is attempted.
    let mut tampered = saved.clone();
    tampered["token"] = Value::String("zz".repeat(32));
    std::fs::write(&files[0], tampered.to_string()).unwrap();
    refused(
        serve(&control, options(&gateway.origin, None, &credentials, true))
            .await
            .unwrap_err(),
        "Invalid saved gateway device token",
    );
    // A forged token of the right shape is rejected by the gateway.
    tampered["token"] = Value::String("ab".repeat(32));
    std::fs::write(&files[0], tampered.to_string()).unwrap();
    refused(
        serve(&control, options(&gateway.origin, None, &credentials, true))
            .await
            .unwrap_err(),
        "Gateway rejected this computer: Device credential was rejected",
    );
    control.shutdown().await;
    other.shutdown().await;
}

#[tokio::test]
async fn a_connected_host_answers_relayed_requests_for_its_owner_only() {
    let gateway = gateway().await;
    let client = reqwest::Client::new();
    let home = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let control = host(home.path(), workspace.path(), &gateway.origin);
    let credentials = credential_home(&control);
    let code = invitation(&client, &gateway.origin).await;
    let origin = gateway.origin.clone();

    let relay = async {
        let mut connected = false;
        for _ in 0..200 {
            if gateway_status(&client, &origin, "owner").await["connected"] == true {
                connected = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        assert!(connected, "the host never reached the gateway");

        let response = client
            .get(format!("{origin}/api/status"))
            .header("Remote-User", "owner")
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), reqwest::StatusCode::OK);
        let status: Value = response.json().await.unwrap();
        assert!(status.is_object(), "{status}");

        let sessions = client
            .get(format!("{origin}/api/sessions"))
            .header("Remote-User", "owner")
            .send()
            .await
            .unwrap();
        assert_eq!(sessions.status(), reqwest::StatusCode::OK);
        let sessions: Value = sessions.json().await.unwrap();
        assert!(sessions.is_array() || sessions.is_object(), "{sessions}");

        // A mutation reaches the host with the browser origin; the host's own
        // validation answers it, not the relay.
        let response = client
            .post(format!("{origin}/api/pause"))
            .header("Remote-User", "owner")
            .header("Origin", &origin)
            .header("content-type", "application/json")
            .body("{}")
            .send()
            .await
            .unwrap();
        assert!(response.status().is_client_error(), "{}", response.status());
        assert!(response.status() != reqwest::StatusCode::BAD_GATEWAY);

        // Other identities cannot use the paired host, and anonymous requests
        // never leave the gateway.
        let response = client
            .get(format!("{origin}/api/status"))
            .header("Remote-User", "intruder")
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), reqwest::StatusCode::SERVICE_UNAVAILABLE);
        let response = client
            .get(format!("{origin}/api/status"))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), reqwest::StatusCode::UNAUTHORIZED);
    };
    tokio::select! {
        result = serve(&control, options(&origin, Some(&code), &credentials, false)) => {
            panic!("the host connection ended early: {result:?}");
        }
        () = relay => {}
    }
    control.shutdown().await;
}
