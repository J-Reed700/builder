pub mod import;
pub mod location;
pub mod pipeline;
pub use pipeline::PipelineSettings;
mod secret;
pub use secret::Secret;

use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use std::io::Write;
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Profile {
    pub pipeline: PipelineSettings,
    pub base_url: String,
    pub model: String,
    pub api_key_env: Option<String>,
    pub api_key: Option<Secret>,
    pub headers: BTreeMap<String, Secret>,
    pub roles: Vec<ModelRole>,
    pub completion: CompletionOptions,
    pub extra_body: BTreeMap<String, serde_json::Value>,
    pub stream: bool,
    pub tools: bool,
    pub context_tokens: usize,
    pub max_output_tokens: usize,
    pub auto_compact: bool,
    pub compact_at_percent: u8,
    pub max_attempts: u32,
    pub connect_timeout_secs: u64,
    pub idle_timeout_secs: u64,
    pub request_timeout_secs: u64,
}
impl Default for Profile {
    fn default() -> Self {
        Self {
            pipeline: PipelineSettings::default(),
            base_url: "http://localhost:11434/v1".into(),
            model: "qwen3:8b".into(),
            api_key_env: None,
            api_key: None,
            headers: BTreeMap::new(),
            roles: vec![ModelRole::Chat],
            completion: CompletionOptions::default(),
            extra_body: BTreeMap::new(),
            stream: true,
            tools: true,
            context_tokens: 32768,
            max_output_tokens: 4096,
            auto_compact: true,
            compact_at_percent: 75,
            max_attempts: 5,
            connect_timeout_secs: 10,
            idle_timeout_secs: 90,
            request_timeout_secs: 900,
        }
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ModelRole {
    Chat,
    Edit,
    Apply,
    Embed,
    Autocomplete,
    Rerank,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CompletionOptions {
    pub temperature: Option<f64>,
    pub top_p: Option<f64>,
    pub top_k: Option<u32>,
    pub min_p: Option<f64>,
    pub presence_penalty: Option<f64>,
    pub frequency_penalty: Option<f64>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub stop: Vec<String>,
}
impl Profile {
    pub fn supports_chat(&self) -> bool {
        self.roles
            .iter()
            .any(|r| matches!(r, ModelRole::Chat | ModelRole::Edit | ModelRole::Apply))
    }

    pub fn validate(&self) -> Result<()> {
        self.pipeline.validate()?;
        let url = url::Url::parse(&self.base_url).context("Invalid endpoint URL")?;
        ensure!(
            matches!(url.scheme(), "http" | "https") && url.host_str().is_some(),
            "Endpoint must be an HTTP(S) URL"
        );
        ensure!(
            url.username().is_empty()
                && url.password().is_none()
                && url.query().is_none()
                && url.fragment().is_none(),
            "Keep credentials and query parameters out of base_url; use api_key_env"
        );
        ensure!(!self.model.trim().is_empty(), "Model cannot be empty");
        ensure!(
            (1..=20).contains(&self.max_attempts),
            "max_attempts must be between 1 and 20"
        );
        ensure!(
            self.max_output_tokens > 0
                && self.context_tokens.saturating_sub(self.max_output_tokens) > 1024,
            "Context budget must exceed output budget by more than 1024 tokens"
        );
        ensure!(
            (50..=90).contains(&self.compact_at_percent),
            "compact_at_percent must be between 50 and 90"
        );
        ensure!(
            self.connect_timeout_secs > 0
                && self.idle_timeout_secs > 0
                && self.request_timeout_secs > 0,
            "Timeouts must be positive"
        );
        ensure!(
            self.api_key.is_none() || self.api_key_env.is_none(),
            "Use either api_key or api_key_env, not both"
        );
        ensure!(
            !self.roles.is_empty(),
            "A model must have at least one role"
        );
        for (value, min, max, name) in [
            (self.completion.temperature, 0.0, 2.0, "temperature"),
            (self.completion.top_p, 0.0, 1.0, "top_p"),
            (self.completion.min_p, 0.0, 1.0, "min_p"),
            (
                self.completion.presence_penalty,
                -2.0,
                2.0,
                "presence_penalty",
            ),
            (
                self.completion.frequency_penalty,
                -2.0,
                2.0,
                "frequency_penalty",
            ),
        ] {
            ensure!(
                value.is_none_or(|v| v.is_finite() && (min..=max).contains(&v)),
                "Invalid {name} range"
            );
        }
        let mut header_names = std::collections::BTreeSet::new();
        for (name, value) in &self.headers {
            ensure!(
                !name.is_empty()
                    && name
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&b)),
                "Invalid custom header name"
            );
            let lower = name.to_ascii_lowercase();
            ensure!(
                header_names.insert(lower.clone()),
                "Duplicate custom header (names are case-insensitive)"
            );
            ensure!(
                !matches!(
                    lower.as_str(),
                    "host" | "content-length" | "transfer-encoding" | "connection"
                ),
                "Custom headers cannot override HTTP routing or framing"
            );
            if let Secret::Literal(value) = value {
                ensure!(
                    !value.bytes().any(|b| b == 127 || (b < 32 && b != b'\t')),
                    "Invalid custom header value; expected a single-line HTTP header"
                );
            }
        }
        for name in self.extra_body.keys() {
            ensure!(
                !matches!(
                    name.as_str(),
                    "model"
                        | "messages"
                        | "stream"
                        | "stream_options"
                        | "tools"
                        | "tool_choice"
                        | "max_tokens"
                        | "max_completion_tokens"
                        | "n"
                ),
                "extra_body cannot override protocol field {name}"
            );
        }
        Ok(())
    }
    pub fn endpoint(&self, suffix: &str) -> String {
        format!("{}/{}", self.base_url.trim_end_matches('/'), suffix)
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EmbeddingBackend {
    #[default]
    Local,
    Remote,
    Lexical,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct MemorySettings {
    pub enabled: bool,
    pub embedding_profile: Option<String>,
    pub embedding_backend: EmbeddingBackend,
    pub local_model_dir: Option<std::path::PathBuf>,
    pub query_prefix: String,
    pub document_prefix: String,
    /// Change when the serving weights change under the same model name.
    pub embedding_revision: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default)]
    pub memory: MemorySettings,
    pub default_profile: String,
    pub profiles: BTreeMap<String, Profile>,
}
impl Default for Config {
    fn default() -> Self {
        Self {
            memory: MemorySettings::default(),
            default_profile: "local".into(),
            profiles: BTreeMap::from([("local".into(), Profile::default())]),
        }
    }
}
impl Config {
    pub fn load(home: &Path) -> Result<Self> {
        let path = home.join("config.toml");
        if !path.exists() {
            return Ok(Self::default());
        }
        let source = location::read_source(&path)?;
        // Parser diagnostics can quote credential-bearing source lines.
        toml::from_str(&source).map_err(|_| anyhow::anyhow!("Invalid config: {} (TOML syntax or unsupported field/type; source hidden to protect credentials)", path.display()))
    }
    pub fn save(&self, home: &Path) -> Result<()> {
        let source = toml::to_string_pretty(self)?;
        ensure!(
            source.len() <= 1024 * 1024,
            "Config exceeds the 1 MiB limit; existing config unchanged"
        );
        ensure_home(home)?;
        let tmp = home.join(format!("config-{}.tmp", uuid::Uuid::new_v4()));
        let result = (|| -> Result<()> {
            let mut options = std::fs::OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            let mut file = options.open(&tmp)?;
            file.write_all(source.as_bytes())?;
            file.sync_all()?;
            std::fs::rename(&tmp, home.join("config.toml"))?;
            #[cfg(unix)]
            std::fs::File::open(home)?.sync_all()?;
            Ok(())
        })();
        if result.is_err() {
            let _ = std::fs::remove_file(&tmp);
        }
        result?;
        Ok(())
    }
    pub fn redacted(&self) -> Self {
        let mut result = self.clone();
        for profile in result.profiles.values_mut() {
            profile.api_key = profile.api_key.as_ref().map(Secret::redacted);
            for value in profile.headers.values_mut() {
                *value = value.redacted();
            }
        }
        result
    }
    pub fn profile(&self, name: Option<&str>) -> Result<(String, Profile)> {
        let name = name.unwrap_or(&self.default_profile);
        match self.profiles.get(name) {
            Some(profile) => {
                profile.validate()?;
                Ok((name.into(), profile.clone()))
            }
            None => bail!("Unknown profile '{name}'. Use builder config add."),
        }
    }
}
pub(crate) fn ensure_home(home: &Path) -> Result<()> {
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(home)?;
    Ok(())
}
pub fn home(override_path: Option<PathBuf>) -> Result<PathBuf> {
    if let Some(path) = override_path {
        return Ok(path);
    }
    let dirs = directories::ProjectDirs::from("dev", "builder", "builder")
        .context("Cannot find data directory; set BUILDER_HOME")?;
    Ok(dirs.data_local_dir().to_path_buf())
}
