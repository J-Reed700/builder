use builder_core::config::{
    Config, Profile, Secret,
    import::{self, Format},
};

const CONTINUE: &str = r#"
name: Local Assistant
version: 1.0.0
schema: v1
models:
  - name: Qwen
    provider: openai
    model: old-model.gguf
    apiBase: https://models.example.test/v1
    apiKey: dummy-key
    capabilities: [tool_use]
    roles: [chat, edit, apply]
    defaultCompletionOptions:
      maxTokens: 8192
      contextLength: 163840
      temperature: 0.6
      topP: 0.85
      topK: 20
      minP: 0.0
      presencePenalty: 0.0
      frequencyPenalty: 0.0
    requestOptions:
      timeout: 1800000
      headers:
        Authorization: Basic test-credential
  - name: Nomic
    provider: openai
    model: nomic-embed-text-v1.5
    apiBase: https://embed.example.test/v1
    roles: [embed]
"#;

#[test]
fn continue_import_preserves_real_world_settings_and_embedding_roles() {
    let result = import::parse(CONTINUE, Format::Continue).unwrap();
    assert!(result.warnings.is_empty());
    assert_eq!(result.config.default_profile, "Qwen");
    let p = &result.config.profiles["Qwen"];
    assert_eq!(p.context_tokens, 163840);
    assert_eq!(p.max_output_tokens, 8192);
    assert_eq!(p.request_timeout_secs, 1800);
    assert_eq!(p.idle_timeout_secs, 1800);
    assert_eq!(p.completion.top_p, Some(0.85));
    assert_eq!(p.completion.top_k, Some(20));
    assert_eq!(p.completion.min_p, Some(0.0));
    assert_eq!(
        p.headers["Authorization"].resolve().unwrap(),
        "Basic test-credential"
    );
    assert!(!result.config.profiles["Nomic"].supports_chat());
    assert!(!result.config.profiles["Nomic"].tools);
}

#[test]
fn credentials_survive_private_roundtrip_but_never_show_or_debug() {
    let config = import::parse(CONTINUE, Format::Continue).unwrap().config;
    let home = tempfile::tempdir().unwrap();
    config.save(home.path()).unwrap();
    let loaded = Config::load(home.path()).unwrap();
    assert_eq!(
        loaded.profiles["Qwen"]
            .api_key
            .as_ref()
            .unwrap()
            .resolve()
            .unwrap(),
        "dummy-key"
    );
    for output in [
        toml::to_string(&loaded.redacted()).unwrap(),
        format!("{loaded:?}"),
    ] {
        assert!(!output.contains("test-credential"));
        assert!(!output.contains("dummy-key"));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(home.path().join("config.toml"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        // Replacing an older broad-permission file creates a private inode.
        std::fs::set_permissions(
            home.path().join("config.toml"),
            std::fs::Permissions::from_mode(0o644),
        )
        .unwrap();
        config.save(home.path()).unwrap();
        assert_eq!(
            std::fs::metadata(home.path().join("config.toml"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }
}

#[test]
fn malformed_credentials_are_not_quoted_by_parser_errors() {
    let bad = CONTINUE.replace("apiKey: dummy-key", "apiKey: [credential-never-print");
    assert!(
        !format!("{:#}", import::parse(&bad, Format::Continue).err().unwrap())
            .contains("credential-never-print")
    );
    let home = tempfile::tempdir().unwrap();
    std::fs::write(
        home.path().join("config.toml"),
        "api_key = 'credential-never-print\n",
    )
    .unwrap();
    assert!(
        !format!("{:#}", Config::load(home.path()).unwrap_err()).contains("credential-never-print")
    );
}

#[test]
fn imports_refuse_collisions_atomically_unless_replacement_is_explicit() {
    let mut target = import::parse(CONTINUE, Format::Continue).unwrap().config;
    let original = toml::to_string(&target).unwrap();
    let update = CONTINUE.replace("old-model.gguf", "new-model.gguf");
    assert!(
        import::parse(&update, Format::Continue)
            .unwrap()
            .merge(&mut target, false, true)
            .is_err()
    );
    assert_eq!(toml::to_string(&target).unwrap(), original);
    import::parse(&update, Format::Continue)
        .unwrap()
        .merge(&mut target, true, true)
        .unwrap();
    assert_eq!(target.profiles["Qwen"].model, "new-model.gguf");
}

#[test]
fn opencode_custom_provider_jsonc_maps_auth_limits_and_selected_model() {
    let source = r#"{
      // JSONC comments and trailing commas are accepted.
      "$schema": "https://opencode.ai/config.json",
      "model": "private/qwen",
      "provider": {
        "private": {
          "npm": "@ai-sdk/openai-compatible",
          "options": {
            "baseURL": "https://models.example.test/v1",
            "apiKey": "{env:PRIVATE_MODEL_KEY}",
            "headers": {"Authorization": "Basic test-credential"},
            "timeout": 1800000,
            "maxRetries": 4,
          },
          "models": {
            "qwen": {"id": "Qwen3.8.gguf", "name": "Qwen 3.8", "limit": {"context": 163840, "output": 8192}}
          }
        }
      }
    }"#;
    let imported = import::parse(source, Format::OpenCode).unwrap();
    assert!(imported.warnings.is_empty());
    let p = &imported.config.profiles["private/qwen"];
    assert_eq!(p.model, "Qwen3.8.gguf");
    assert_eq!(p.max_attempts, 5);
    assert!(matches!(&p.api_key, Some(Secret::Environment { env }) if env == "PRIVATE_MODEL_KEY"));
}

#[test]
fn incompatible_protocols_templates_and_request_overrides_fail_explicitly() {
    assert!(
        import::parse(
            &CONTINUE.replace("provider: openai", "provider: anthropic"),
            Format::Continue
        )
        .is_err()
    );
    assert!(
        import::parse(
            &CONTINUE.replace("timeout: 1800000", "verifySsl: false"),
            Format::Continue
        )
        .is_err()
    );
    assert!(Secret::imported("Bearer {env:KEY}".into()).is_err());
    assert!(Secret::imported("{file:somewhere}".into()).is_err());
    assert!(matches!(
        Secret::imported("${{ secrets.MY_KEY }}".into()).unwrap(),
        Secret::Environment { .. }
    ));
    let mut p = Profile::default();
    p.extra_body
        .insert("messages".into(), serde_json::json!([]));
    assert!(p.validate().is_err());
    p.extra_body.clear();
    p.completion.top_p = Some(2.0);
    assert!(p.validate().is_err());
}

#[test]
fn oversized_save_keeps_the_previous_readable_config() {
    let home = tempfile::tempdir().unwrap();
    let mut config = Config::default();
    config.save(home.path()).unwrap();
    let before = std::fs::read(home.path().join("config.toml")).unwrap();
    config.profiles.get_mut("local").unwrap().model = "x".repeat(1024 * 1024);
    assert!(config.save(home.path()).is_err());
    assert_eq!(
        std::fs::read(home.path().join("config.toml")).unwrap(),
        before
    );
    assert!(Config::load(home.path()).is_ok());
}

#[test]
fn opencode_provider_sampling_and_basic_auth_import_together() {
    let source = r#"{"model":"remote/qwen","provider":{"remote":{"npm":"@ai-sdk/openai-compatible","options":{"baseURL":"https://example.test/v1","headers":{"Authorization":"Basic fixture"},"temperature":0.6,"topP":0.85,"topK":20},"models":{"qwen":{"limit":{"context":32768,"output":4096}}}}}}"#;
    let imported = import::parse(source, Format::OpenCode).unwrap();
    let profile = &imported.config.profiles["remote/qwen"];
    assert_eq!(profile.completion.temperature, Some(0.6));
    assert_eq!(profile.completion.top_p, Some(0.85));
    assert_eq!(profile.completion.top_k, Some(20));
    assert_eq!(
        profile.headers["Authorization"].resolve().unwrap(),
        "Basic fixture"
    );
    assert!(
        import::parse(
            &source.replace("\"topK\":20", "\"unknownTransportOption\":20"),
            Format::OpenCode
        )
        .is_err()
    );
}
