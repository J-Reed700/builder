//! The command-line surface, driven through the real binary against a
//! disposable home and a scripted endpoint. Nothing here needs a model.
use axum::{
    Json, Router,
    extract::State,
    routing::{get, post},
};
use builder_core::{
    config::{Config, EmbeddingBackend, Profile, Secret},
    protocol::{Message, Role},
    store::Store,
};
use serde_json::{Value, json};
use std::{path::Path, process::Output};

#[derive(Clone, Copy, PartialEq, Eq)]
enum Tools {
    /// Answers probe calls exactly and ordinary runs with prose.
    Faithful,
    /// Never emits a tool call.
    Prose,
}

#[derive(Clone)]
struct Endpoint {
    tools: Tools,
    models: Value,
    context: usize,
}

struct Server {
    url: String,
    task: tokio::task::JoinHandle<()>,
}
impl Drop for Server {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn models(State(endpoint): State<Endpoint>) -> Json<Value> {
    Json(endpoint.models.clone())
}

async fn props(State(endpoint): State<Endpoint>) -> Json<Value> {
    Json(json!({"total_slots":1,"default_generation_settings":{"n_ctx":endpoint.context}}))
}

async fn completions(State(endpoint): State<Endpoint>, Json(body): Json<Value>) -> Json<Value> {
    let reply = |content: Value, tool_calls: Vec<Value>, reason: &str| {
        Json(
            json!({"choices":[{"index":0,"message":{"role":"assistant","content":content,"tool_calls":tool_calls},"finish_reason":reason}]}),
        )
    };
    let tools = body["tools"].as_array().cloned().unwrap_or_default();
    let probe = tools
        .first()
        .is_some_and(|tool| tool["function"]["name"] == "builder_capability_probe");
    if probe && endpoint.tools == Tools::Faithful {
        let properties = &tools[0]["function"]["parameters"]["properties"];
        let nonce = properties["nonce"]["enum"][0].clone();
        let calls = properties["slot"]["enum"]
            .as_array()
            .unwrap()
            .iter()
            .enumerate()
            .map(|(index, slot)| {
                json!({"id":format!("call-{index}"),"type":"function","function":{
                    "name":"builder_capability_probe",
                    "arguments":json!({"nonce":nonce,"slot":slot}).to_string()
                }})
            })
            .collect();
        return reply(Value::Null, calls, "tool_calls");
    }
    if body["response_format"]["type"] == "json_object" {
        return reply(json!("{\"ok\":true}"), vec![], "stop");
    }
    reply(json!("Done."), vec![], "stop")
}

async fn serve(tools: Tools, catalog: Value, context: usize) -> Server {
    let endpoint = Endpoint {
        tools,
        models: catalog,
        context,
    };
    let app = Router::new()
        .route("/v1/models", get(models))
        .route("/props", get(props))
        .route("/v1/chat/completions", post(completions))
        .with_state(endpoint);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/v1", listener.local_addr().unwrap());
    let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    Server { url, task }
}

fn save_config(home: &Path, base_url: &str, tools: bool) {
    let mut config = Config::default();
    config.profiles.insert(
        "local".into(),
        Profile {
            base_url: base_url.into(),
            model: "test-model".into(),
            stream: false,
            tools,
            ..Profile::default()
        },
    );
    config.save(home).unwrap();
}

struct Cli {
    home: std::path::PathBuf,
    workspace: std::path::PathBuf,
}

impl Cli {
    fn new(home: &Path, workspace: &Path) -> Self {
        Self {
            home: home.to_owned(),
            workspace: workspace.to_owned(),
        }
    }
    async fn run(&self, args: &[&str]) -> Output {
        builder(&self.home, &self.workspace, args).await
    }
}

async fn builder(home: &Path, workspace: &Path, args: &[&str]) -> Output {
    tokio::process::Command::new(env!("CARGO_BIN_EXE_builder"))
        .env_remove("BUILDER_HOME")
        .arg("--home")
        .arg(home)
        .arg("-C")
        .arg(workspace)
        .args(args)
        .stdin(std::process::Stdio::null())
        .output()
        .await
        .unwrap()
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}
fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}
fn ok(output: &Output) -> String {
    assert!(output.status.success(), "{}", stderr(output));
    stdout(output)
}
fn failed(output: &Output) -> String {
    assert!(
        !output.status.success(),
        "unexpected success: {}",
        stdout(output)
    );
    stderr(output)
}
fn json_out(output: &Output) -> Value {
    serde_json::from_str(&ok(output)).unwrap()
}

#[tokio::test]
async fn models_lists_advertised_ids_and_doctor_saves_a_passing_conformance_report() {
    let server = serve(
        Tools::Faithful,
        json!({"data":[{"id":"test-model"},{"id":"other-model"},{"object":"nameless"}]}),
        131_072,
    )
    .await;
    let home = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    save_config(home.path(), &server.url, true);

    let output = builder(home.path(), workspace.path(), &["models"]).await;
    assert_eq!(ok(&output), "test-model\nother-model\n");

    let output = builder(home.path(), workspace.path(), &["doctor"]).await;
    let text = ok(&output);
    for expected in [
        "✓ configuration valid",
        &format!("✓ durable storage: {}", home.path().display()),
        "✓ profile: local",
        &format!("endpoint: {}", server.url),
        "✓ configured model is advertised",
        "Running active generation, JSON, and tool-call probes",
        "Active conformance (saved by endpoint/model fingerprint):",
        "\"normal_generation\": \"passed\"",
        "\"json_object\": \"passed\"",
        "\"native_tool_call\": \"passed\"",
        "\"parallel_tool_calls\": \"passed\"",
    ] {
        assert!(text.contains(expected), "missing {expected:?} in {text}");
    }
    // A transient pipeline override is allowed for diagnostics.
    let output = builder(
        home.path(),
        workspace.path(),
        &["--pipeline", "subagents=false", "models"],
    )
    .await;
    assert_eq!(ok(&output), "test-model\nother-model\n");
}

#[tokio::test]
async fn doctor_fails_on_required_tool_probes_and_warns_about_an_unadvertised_model() {
    let server = serve(
        Tools::Prose,
        json!({"data":[{"id":"something-else"}]}),
        131_072,
    )
    .await;
    let home = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    save_config(home.path(), &server.url, true);

    let output = builder(home.path(), workspace.path(), &["doctor"]).await;
    let text = stdout(&output);
    assert!(
        text.contains("! configured model 'test-model' was not advertised; verify its ID"),
        "{text}"
    );
    assert!(text.contains("\"failed\""), "{text}");
    assert!(
        failed(&output).contains(
            "Required provider conformance probes failed: native_tool_call, parallel_tool_calls"
        ),
        "{}",
        stderr(&output)
    );

    // The same endpoint passes once the profile does not use tools.
    save_config(home.path(), &server.url, false);
    let output = builder(home.path(), workspace.path(), &["doctor"]).await;
    let text = ok(&output);
    assert!(text.contains("\"skipped\""), "{text}");
    assert!(
        text.contains("tools are disabled for this profile"),
        "{text}"
    );
}

#[tokio::test]
async fn model_discovery_failures_and_unexpected_shapes_are_reported() {
    let home = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let closed = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = closed.local_addr().unwrap();
    drop(closed);
    save_config(home.path(), &format!("http://{address}/v1"), true);
    let output = builder(home.path(), workspace.path(), &["models"]).await;
    let text = failed(&output);
    assert!(text.contains("Model discovery failed"), "{text}");
    assert!(text.contains("GET /models"), "{text}");

    let server = serve(Tools::Faithful, json!({"data":"nope"}), 131_072).await;
    save_config(home.path(), &server.url, true);
    let output = builder(home.path(), workspace.path(), &["models"]).await;
    assert_eq!(json_out(&output), json!({"data":"nope"}));
    let output = builder(home.path(), workspace.path(), &["doctor"]).await;
    let text = ok(&output);
    assert!(text.contains("\"data\": \"nope\""), "{text}");
    assert!(!text.contains("Active conformance"), "{text}");
}

#[tokio::test]
async fn memory_commands_manage_preferences_without_a_model() {
    let home = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let cli = Cli::new(home.path(), workspace.path());

    let status = json_out(&cli.run(&["memory", "status"]).await);
    assert_eq!(status["settings"]["enabled"], false);
    assert_eq!(status["checkout_records"], 0);
    assert_eq!(status["user_preferences"], 0);
    assert!(status["storage"].as_str().unwrap().contains("SQLite"));

    // Searching works lexically even before memory is enabled.
    assert!(
        cli.run(&["memory", "search", "anything"])
            .await
            .status
            .success()
    );
    let text = failed(&cli.run(&["memory", "index"]).await);
    assert!(text.contains("Enable memory first"), "{text}");

    let enabled = json_out(&cli.run(&["memory", "enable", "--lexical"]).await);
    assert_eq!(enabled["enabled"], true);
    assert_eq!(enabled["backend"], "lexical");
    let saved = Config::load(home.path()).unwrap().memory;
    assert!(saved.enabled);
    assert_eq!(saved.embedding_backend, EmbeddingBackend::Lexical);

    let first = json_out(&cli.run(&["memory", "remember", "style", "Use tabs"]).await);
    assert_eq!(first["key"], "style");
    assert_eq!(first["text"], "Use tabs");
    assert_eq!(first["revision"], 1);
    let second = json_out(
        &cli.run(&["memory", "remember", "style", "Use spaces"])
            .await,
    );
    assert_eq!(second["revision"], 2);

    let latest = json_out(&cli.run(&["memory", "get", "style", "--user"]).await);
    assert_eq!(latest["text"], "Use spaces");
    let earlier = json_out(
        &cli.run(&["memory", "get", "style", "--user", "--revision", "1"])
            .await,
    );
    assert_eq!(earlier["text"], "Use tabs");
    assert_eq!(
        json_out(&cli.run(&["memory", "get", "style"]).await),
        Value::Null
    );

    let preferences = json_out(&cli.run(&["memory", "list", "--user"]).await);
    assert_eq!(preferences.as_array().unwrap().len(), 1);
    assert_eq!(preferences[0]["key"], "style");
    assert_eq!(json_out(&cli.run(&["memory", "list"]).await), json!([]));
    assert_eq!(
        json_out(&cli.run(&["memory", "status"]).await)["user_preferences"],
        1
    );

    assert!(
        cli.run(&["memory", "search", "spaces"])
            .await
            .status
            .success()
    );
    let indexed = json_out(&cli.run(&["memory", "index"]).await);
    assert!(indexed["processed"].is_string());

    let forgotten = json_out(&cli.run(&["memory", "forget", "style", "--user"]).await);
    assert_eq!(forgotten["forgotten"], "style");
    assert_eq!(
        json_out(&cli.run(&["memory", "get", "style", "--user"]).await),
        Value::Null
    );
    assert_eq!(
        json_out(&cli.run(&["memory", "list", "--user"]).await),
        json!([])
    );
    let text = failed(&cli.run(&["memory", "forget", "style", "--user"]).await);
    assert!(text.contains("Memory not found"), "{text}");

    let mut store = Store::open(home.path()).unwrap();
    let session = store
        .create("Task state", "local", workspace.path(), "system")
        .unwrap();
    drop(store);
    assert_eq!(
        json_out(&cli.run(&["memory", "task", &session[..8]]).await),
        Value::Null
    );
    let text = failed(&cli.run(&["memory", "task", "nope"]).await);
    assert!(text.contains("matched 0"), "{text}");

    let disabled = json_out(&cli.run(&["memory", "disable"]).await);
    assert_eq!(disabled["enabled"], false);
    assert!(!Config::load(home.path()).unwrap().memory.enabled);

    // A remote backend needs a real profile; a failed enable restores the
    // previous settings instead of saving a broken configuration.
    let text = failed(
        &cli.run(&["memory", "enable", "--embedding-profile", "ghost"])
            .await,
    );
    assert!(text.contains("Unknown profile 'ghost'"), "{text}");
    assert!(!Config::load(home.path()).unwrap().memory.enabled);
    let text = failed(
        &cli.run(&["memory", "enable", "--embedding-profile", "local"])
            .await,
    );
    assert!(text.contains("embed role"), "{text}");
    assert!(!Config::load(home.path()).unwrap().memory.enabled);
}

#[tokio::test]
async fn sessions_and_markdown_export_render_saved_conversations() {
    let home = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let output = builder(home.path(), workspace.path(), &["sessions"]).await;
    assert_eq!(ok(&output), "No sessions yet. Run builder to start one.\n");

    let mut store = Store::open(home.path()).unwrap();
    let id = store
        .create("Fix the parser", "local", workspace.path(), "system prompt")
        .unwrap();
    store
        .append(&id, &Message::text(Role::User, "Exact question"))
        .unwrap();
    let mut answer = Message::text(Role::Assistant, "Looking.");
    answer.tool_calls.push(serde_json::from_value(json!({
        "id":"c1","type":"function","function":{"name":"read_file","arguments":"{\"path\":\"a.rs\"}"}
    })).unwrap());
    store.append(&id, &answer).unwrap();
    drop(store);

    let output = builder(home.path(), workspace.path(), &["sessions"]).await;
    let text = ok(&output);
    assert!(text.contains(&id[..8]), "{text}");
    assert!(text.contains("local"), "{text}");
    assert!(text.contains("Fix the parser"), "{text}");

    let output = builder(home.path(), workspace.path(), &["export", &id[..8]]).await;
    let text = ok(&output);
    for expected in [
        "# Fix the parser\n",
        "## system\n\nsystem prompt\n",
        "## user\n\nExact question\n",
        "## assistant\n\nLooking.\n",
        "Tool: read_file\n\n```json\n{\"path\":\"a.rs\"}\n```",
    ] {
        assert!(text.contains(expected), "missing {expected:?} in {text}");
    }
    let output = builder(home.path(), workspace.path(), &["export", "zzz"]).await;
    let text = failed(&output);
    assert!(text.contains("matched 0"), "{text}");
}

#[tokio::test]
async fn remote_refuses_transient_overrides_and_invalid_gateways() {
    let home = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let output = builder(
        home.path(),
        workspace.path(),
        &["--pipeline", "subagents=false", "remote"],
    )
    .await;
    let text = failed(&output);
    assert!(
        text.contains(
            "Save pipeline settings with config pipeline set before starting remote control"
        ),
        "{text}"
    );
    let output = builder(
        home.path(),
        workspace.path(),
        &["remote", "connect", "not-a-url"],
    )
    .await;
    let text = failed(&output);
    assert!(
        text.contains("Origin must start with http:// or https://"),
        "{text}"
    );
    // An origin the remote control accepts but that is not a parseable URL
    // fails when the outbound connector is built, after the workspace is shown.
    let output = builder(
        home.path(),
        workspace.path(),
        &["remote", "connect", "https://gateway.example.com:notaport"],
    )
    .await;
    let text = failed(&output);
    assert!(text.contains("Workspace root:"), "{text}");
    assert!(
        text.contains("Gateway must be a complete http:// or https:// URL"),
        "{text}"
    );
}

#[tokio::test]
async fn config_commands_add_select_show_and_update_profiles() {
    let home = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let cli = Cli::new(home.path(), workspace.path());

    let path = ok(&cli.run(&["config", "init"]).await);
    assert_eq!(
        path.trim(),
        home.path().join("config.toml").to_str().unwrap()
    );
    assert_eq!(
        ok(&cli.run(&["config", "init"]).await),
        path,
        "init never overwrites"
    );

    let text = ok(&cli
        .run(&[
            "config",
            "add",
            "hosted",
            "--base-url",
            "https://models.example.test/v1",
            "--model",
            "big",
            "--api-key-env",
            "HOSTED_KEY",
            "--no-stream",
            "--no-tools",
            "--context-tokens",
            "65536",
            "--max-output-tokens",
            "8192",
        ])
        .await);
    assert_eq!(text, "Saved profile hosted\n");
    let hosted = Config::load(home.path()).unwrap().profiles["hosted"].clone();
    assert_eq!(hosted.base_url, "https://models.example.test/v1");
    assert_eq!(hosted.model, "big");
    assert_eq!(hosted.api_key_env.as_deref(), Some("HOSTED_KEY"));
    assert!(!hosted.stream);
    assert!(!hosted.tools);
    assert_eq!(
        (hosted.context_tokens, hosted.max_output_tokens),
        (65536, 8192)
    );

    let text = failed(
        &cli.run(&[
            "config",
            "add",
            " ",
            "--base-url",
            "https://x.test/v1",
            "--model",
            "m",
        ])
        .await,
    );
    assert!(text.contains("Profile name cannot be empty"), "{text}");
    let text = failed(
        &cli.run(&[
            "config",
            "add",
            "bad",
            "--base-url",
            "ftp://x.test",
            "--model",
            "m",
        ])
        .await,
    );
    assert!(text.contains("Endpoint must be an HTTP(S) URL"), "{text}");
    assert!(
        !Config::load(home.path())
            .unwrap()
            .profiles
            .contains_key("bad")
    );

    assert_eq!(
        ok(&cli.run(&["config", "use", "hosted"]).await),
        "Default profile: hosted\n"
    );
    assert_eq!(Config::load(home.path()).unwrap().default_profile, "hosted");
    let text = failed(&cli.run(&["config", "use", "ghost"]).await);
    assert!(text.contains("Unknown profile 'ghost'"), "{text}");

    assert_eq!(
        ok(&cli.run(&["config", "model", "newer"]).await),
        "Updated hosted model: newer\n"
    );
    assert_eq!(
        ok(&cli.run(&["-p", "local", "config", "model", "other"]).await),
        "Updated local model: other\n"
    );
    let config = Config::load(home.path()).unwrap();
    assert_eq!(config.profiles["hosted"].model, "newer");
    assert_eq!(config.profiles["local"].model, "other");

    let mut config = Config::load(home.path()).unwrap();
    config.profiles.get_mut("local").unwrap().api_key =
        Some(Secret::Literal("sk-secret-value".into()));
    config.save(home.path()).unwrap();
    let shown = ok(&cli.run(&["config", "show"]).await);
    assert!(shown.contains("[profiles.hosted]"), "{shown}");
    assert!(shown.contains("api_key_env = \"HOSTED_KEY\""), "{shown}");
    assert!(
        !shown.contains("sk-secret-value"),
        "secrets are redacted: {shown}"
    );

    let fixture = workspace.path().join("opencode.json");
    std::fs::write(&fixture, r#"{
      "$schema": "https://opencode.ai/config.json",
      "model": "private/qwen",
      "provider": {
        "private": {
          "npm": "@ai-sdk/openai-compatible",
          "options": {"baseURL": "https://models.example.test/v1", "apiKey": "{env:PRIVATE_MODEL_KEY}"},
          "models": {"qwen": {"id": "Qwen3.8.gguf", "limit": {"context": 163840, "output": 8192}}}
        }
      }
    }"#).unwrap();
    let fixture = fixture.to_str().unwrap();
    let text = ok(&cli
        .run(&[
            "config",
            "import",
            fixture,
            "--format",
            "opencode",
            "--activate",
        ])
        .await);
    assert!(text.contains("Imported profile: private/qwen"), "{text}");
    assert!(text.contains("Default profile: private/qwen"), "{text}");
    let config = Config::load(home.path()).unwrap();
    assert_eq!(config.default_profile, "private/qwen");
    assert_eq!(config.profiles["private/qwen"].model, "Qwen3.8.gguf");
    assert!(
        cli.run(&[
            "config",
            "import",
            fixture,
            "--format",
            "opencode",
            "--replace"
        ])
        .await
        .status
        .success()
    );

    let text = failed(
        &cli.run(&["--pipeline", "subagents=false", "sessions"])
            .await,
    );
    assert!(
        text.contains("Transient --pipeline overrides apply to"),
        "{text}"
    );
    let text = failed(
        &cli.run(&[
            "--pipeline",
            "subagents=false",
            "config",
            "pipeline",
            "set",
            "subagents=true",
        ])
        .await,
    );
    assert!(
        text.contains("without transient --pipeline overrides"),
        "{text}"
    );
    let text = failed(
        &cli.run(&[
            "--pipeline",
            "subagents=false",
            "config",
            "pipeline",
            "reset",
        ])
        .await,
    );
    assert!(text.contains("Reset does not accept transient"), "{text}");
    assert_eq!(
        ok(&cli
            .run(&["-p", "hosted", "config", "pipeline", "reset"])
            .await),
        "Restored pipeline defaults for hosted\n"
    );
}

#[tokio::test]
async fn run_loads_workspace_instructions_fits_the_context_and_reads_stdin_prompts() {
    let server = serve(
        Tools::Faithful,
        json!({"data":[{"id":"test-model"}]}),
        20_480,
    )
    .await;
    let home = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    save_config(home.path(), &server.url, true);
    // A generous output budget that the smaller server window cannot honour.
    let mut config = Config::load(home.path()).unwrap();
    config.profiles.get_mut("local").unwrap().max_output_tokens = 20_000;
    config.save(home.path()).unwrap();
    std::fs::write(
        workspace.path().join("AGENTS.md"),
        "Always answer briefly.\n",
    )
    .unwrap();

    let output = builder(
        home.path(),
        workspace.path(),
        &["--auto", "run", "Say hello"],
    )
    .await;
    assert_eq!(ok(&output), "Done.\n");
    let notes = stderr(&output);
    assert!(notes.contains("Session saved:"), "{notes}");
    assert!(
        notes.contains("Context window: the server reports 20480 tokens per request; using that instead of the profile's 32768. Output budget lowered to 5120."),
        "{notes}"
    );
    let store = Store::open(home.path()).unwrap();
    let session = store.sessions().unwrap().into_iter().next().unwrap();
    assert_eq!(session.title, "Say hello");
    let system = &store.messages(&session.id).unwrap()[0];
    assert_eq!(system.role, Role::System);
    let system = system.content.clone().unwrap();
    assert!(
        system.contains("Workspace instructions (AGENTS.md):\nAlways answer briefly."),
        "{system}"
    );
    assert!(
        system.contains(&format!(
            "Workspace: {}",
            workspace.path().canonicalize().unwrap().display()
        )),
        "{system}"
    );
    drop(store);

    let mut child = tokio::process::Command::new(env!("CARGO_BIN_EXE_builder"))
        .env_remove("BUILDER_HOME")
        .args([
            "--home",
            home.path().to_str().unwrap(),
            "-C",
            workspace.path().to_str().unwrap(),
            "--auto",
            "run",
            "-",
        ])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    {
        use tokio::io::AsyncWriteExt;
        let mut stdin = child.stdin.take().unwrap();
        stdin.write_all(b"From standard input").await.unwrap();
    }
    let output = child.wait_with_output().await.unwrap();
    assert_eq!(ok(&output), "Done.\n");
    let store = Store::open(home.path()).unwrap();
    assert_eq!(store.sessions().unwrap()[0].title, "From standard input");
    drop(store);

    std::fs::write(
        workspace.path().join("AGENTS.md"),
        "x".repeat(64 * 1024 + 1),
    )
    .unwrap();
    let output = builder(
        home.path(),
        workspace.path(),
        &["--auto", "run", "Say hello"],
    )
    .await;
    let text = failed(&output);
    assert!(
        text.contains("Workspace AGENTS.md exceeds 64 KiB"),
        "{text}"
    );

    std::fs::remove_file(workspace.path().join("AGENTS.md")).unwrap();
    let output = builder(home.path(), workspace.path(), &["chat"]).await;
    let text = failed(&output);
    assert!(
        text.contains("Interactive mode requires a terminal"),
        "{text}"
    );
}

#[tokio::test]
async fn resume_and_daemon_report_missing_sessions_and_misused_flags() {
    let home = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let text = failed(&builder(home.path(), workspace.path(), &["resume"]).await);
    assert!(text.contains("No saved sessions"), "{text}");
    let text = failed(
        &builder(
            home.path(),
            workspace.path(),
            &["run", "x", "--session", "nope"],
        )
        .await,
    );
    assert!(text.contains("matched 0"), "{text}");
    let text = failed(
        &builder(
            home.path(),
            workspace.path(),
            &["--auto", "daemon", "--once"],
        )
        .await,
    );
    assert!(
        text.contains("Daemon permissions are stored per schedule"),
        "{text}"
    );
    // Templates escape and canonicalize paths per format (systemd doubles
    // backslashes; Windows canonical paths gain a `\\?\` prefix), so match
    // the components that survive every rendering.
    let exe_name = Path::new(env!("CARGO_BIN_EXE_builder"))
        .file_name()
        .unwrap()
        .to_str()
        .unwrap();
    let home_name = home.path().file_name().unwrap().to_str().unwrap();
    for (format, marker) in [
        ("launchd", "<key>ProgramArguments</key>"),
        ("systemd", "ExecStart="),
    ] {
        let output = builder(
            home.path(),
            workspace.path(),
            &["daemon", "--service", format],
        )
        .await;
        let text = ok(&output);
        assert!(text.contains(marker), "{format}: {text}");
        assert!(text.contains(exe_name), "{format}: {text}");
        assert!(text.contains(home_name), "{format}: {text}");
        assert!(text.contains("daemon"), "{format}: {text}");
    }
}

#[tokio::test]
async fn schedule_commands_cover_every_cadence_and_state_transition() {
    let home = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let cli = Cli::new(home.path(), workspace.path());

    let created = json_out(
        &cli.run(&[
            "schedule",
            "add",
            "--every",
            "2h",
            "Review the open pull requests carefully and report",
            "--json",
        ])
        .await,
    );
    let name = created["schedule"]["definition"]["name"].as_str().unwrap();
    assert_eq!(
        name.chars().count(),
        40,
        "the name defaults to the prompt's first 40 characters"
    );
    assert!(name.starts_with("Review the open pull requests"));
    assert_eq!(
        created["schedule"]["definition"]["cadence"]["seconds"],
        7200
    );
    assert_eq!(created["schedule"]["definition"]["access"], "read_only");
    assert_eq!(created["schedule"]["definition"]["timeout_seconds"], 1800);
    assert_eq!(created["runner_connected"], false);

    let once = json_out(
        &cli.run(&[
            "schedule",
            "add",
            "--at",
            "2030-01-02T09:00:00-05:00",
            "--name",
            "Once",
            "Ping",
            "--json",
        ])
        .await,
    );
    assert_eq!(
        once["schedule"]["definition"]["cadence"]["timestamp"],
        1_893_592_800
    );

    let before = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    let soon = json_out(
        &cli.run(&[
            "schedule", "add", "--in", "20m", "--name", "Soon", "Ping", "--json",
        ])
        .await,
    );
    let due = soon["schedule"]["definition"]["cadence"]["timestamp"]
        .as_i64()
        .unwrap();
    assert!(
        (before + 20 * 60..=before + 20 * 60 + 60).contains(&due),
        "{due} vs {before}"
    );

    let cron = json_out(
        &cli.run(&[
            "schedule",
            "add",
            "--cron",
            "0 9 * * 1-5",
            "--timezone",
            "America/New_York",
            "--allow-writes",
            "--timeout",
            "60",
            "--name",
            "Weekday",
            "Summarize the week",
            "--json",
        ])
        .await,
    );
    let cron_id = cron["schedule"]["id"].as_str().unwrap().to_owned();
    assert_eq!(
        cron["schedule"]["definition"]["cadence"]["expression"],
        "0 9 * * 1-5"
    );
    assert_eq!(
        cron["schedule"]["definition"]["cadence"]["timezone"],
        "America/New_York"
    );
    assert_eq!(cron["schedule"]["definition"]["access"], "trust");
    assert_eq!(cron["schedule"]["definition"]["timeout_seconds"], 60);

    let text = failed(
        &cli.run(&[
            "schedule",
            "add",
            "--every",
            "5m",
            "--timezone",
            "Europe/Paris",
            "x",
        ])
        .await,
    );
    assert!(text.contains("--timezone applies only to --cron"), "{text}");
    let text = failed(&cli.run(&["schedule", "add", "--at", "tomorrow", "x"]).await);
    assert!(text.contains("Use a timestamp with an offset"), "{text}");
    let text = failed(&cli.run(&["schedule", "add", "--every", "5x", "x"]).await);
    assert!(text.contains("Use s, m, h, or d"), "{text}");
    let text = failed(
        &cli.run(&["schedule", "add", "--every", "5 minutes", "x"])
            .await,
    );
    assert!(text.contains("Invalid duration"), "{text}");
    let text = failed(
        &cli.run(&["-p", "ghost", "schedule", "add", "--every", "5m", "x"])
            .await,
    );
    assert!(text.contains("Unknown profile 'ghost'"), "{text}");
    let text = failed(&cli.run(&["schedule", "add", "x"]).await);
    assert!(text.contains("required"), "a cadence is required: {text}");

    let listed = json_out(&cli.run(&["schedule", "list", "--json"]).await);
    assert_eq!(listed["schedules"].as_array().unwrap().len(), 4);
    assert_eq!(listed["runner_connected"], false);
    let text = ok(&cli.run(&["schedule"]).await);
    for expected in [
        "Offline · start builder daemon to execute saved tasks",
        "Every 2h",
        "Once · 2030-01-02 14:00:00 UTC",
        "0 9 * * 1-5 · America/New_York",
        "all tools authorized",
        "read-only",
        "Weekday",
    ] {
        assert!(text.contains(expected), "missing {expected:?} in {text}");
    }
    assert_eq!(
        ok(&cli.run(&["schedule", "list"]).await),
        text,
        "bare schedule lists"
    );

    let text = ok(&cli.run(&["schedule", "show", &cron_id[..8]]).await);
    for expected in [
        "Workspace",
        "Profile",
        "local",
        "Timeout",
        "60s",
        "Instruction",
        "Summarize the week",
        "Recent runs",
        "No executions yet",
    ] {
        assert!(text.contains(expected), "missing {expected:?} in {text}");
    }

    let text = ok(&cli.run(&["schedule", "run", &cron_id]).await);
    assert!(text.contains("Queued"), "{text}");
    let text = ok(&cli.run(&["schedule", "pause", &cron_id]).await);
    assert!(text.contains("Paused"), "{text}");
    assert!(text.contains("Queued work cancelled"), "{text}");
    let text = ok(&cli.run(&["schedule", "resume", &cron_id]).await);
    assert!(text.contains("Active"), "{text}");
    assert!(!text.contains("Queued work cancelled"), "{text}");
    let history = json_out(
        &cli.run(&["schedule", "history", &cron_id, "--limit", "1", "--json"])
            .await,
    );
    assert_eq!(history["runs"].as_array().unwrap().len(), 1);
    assert_eq!(history["runs"][0]["status"], "cancelled");
    let text = ok(&cli.run(&["schedule", "show", &cron_id]).await);
    assert!(text.contains("Cancelled ·"), "{text}");
    let text = ok(&cli.run(&["schedule", "delete", &cron_id]).await);
    assert!(text.contains("Deleted"), "{text}");
    let text = failed(&cli.run(&["schedule", "show", "zzz"]).await);
    assert!(!text.is_empty());
}

#[tokio::test]
async fn rejected_invocations_never_leave_an_empty_session_behind() {
    let server = serve(Tools::Prose, json!({"data":[{"id":"test-model"}]}), 131_072).await;
    let home = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    save_config(home.path(), &server.url, false);
    let cli = Cli::new(home.path(), workspace.path());

    // Interactive mode without a terminal (stdin is /dev/null here).
    assert!(failed(&cli.run(&[]).await).contains("requires a terminal"));
    assert!(failed(&cli.run(&["chat", "hello"]).await).contains("requires a terminal"));
    // Blank prompts, from an argument or from empty stdin.
    assert!(failed(&cli.run(&["run", "   "]).await).contains("Prompt is empty"));
    assert!(failed(&cli.run(&["run", "-"]).await).contains("Prompt is empty"));

    assert!(ok(&cli.run(&["sessions"]).await).starts_with("No sessions yet"));
    assert!(
        Store::open(home.path())
            .unwrap()
            .sessions()
            .unwrap()
            .is_empty()
    );
}

#[cfg(unix)]
#[tokio::test]
async fn a_closed_stdout_ends_quietly_instead_of_panicking() {
    let home = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    save_config(home.path(), "http://127.0.0.1:9/v1", true);
    // A pipe whose reader is already gone, as with `builder config show | head`
    // once head has exited.
    let (reader, writer) = std::io::pipe().unwrap();
    drop(reader);
    let output = tokio::process::Command::new(env!("CARGO_BIN_EXE_builder"))
        .env_remove("BUILDER_HOME")
        .arg("--home")
        .arg(home.path())
        .arg("-C")
        .arg(workspace.path())
        .args(["config", "show"])
        .stdin(std::process::Stdio::null())
        .stdout(writer)
        .stderr(std::process::Stdio::piped())
        // `output()` would replace stdout with its own pipe; spawn keeps ours.
        .spawn()
        .unwrap()
        .wait_with_output()
        .await
        .unwrap();
    let stderr = stderr(&output);
    assert!(!stderr.contains("panicked"), "{stderr}");
    assert_eq!(output.status.code(), Some(141), "{stderr}");
}
