//! External config formats terminate here; transport and agent use native profiles.
use super::{CompletionOptions, Config, ModelRole, Profile, Secret};
use anyhow::{Context, Result, bail, ensure};
use serde::Deserialize;
use serde_json::Value;
use std::{collections::BTreeMap, io::Read, path::Path};

#[derive(Clone, Copy)]
pub enum Format {
    Continue,
    OpenCode,
}

pub struct Imported {
    pub config: Config,
    pub warnings: Vec<String>,
}
impl Imported {
    /// Merge as one validated operation; accidental name collisions never replace profiles.
    pub fn merge(self, target: &mut Config, replace: bool, activate: bool) -> Result<Vec<String>> {
        for name in self.config.profiles.keys() {
            ensure!(
                replace || !target.profiles.contains_key(name),
                "Profile '{name}' exists; use --replace to update it"
            );
        }
        if activate {
            target.default_profile = self.config.default_profile;
        }
        target.profiles.extend(self.config.profiles);
        Ok(self.warnings)
    }
}

pub fn read(path: &Path, format: Format) -> Result<Imported> {
    let file = std::fs::File::open(path).context("Cannot open import file")?;
    let mut source = String::new();
    file.take(4 * 1024 * 1024 + 1).read_to_string(&mut source)?;
    ensure!(source.len() <= 4 * 1024 * 1024, "Config exceeds 4 MiB");
    parse(&source, format)
}
pub fn parse(source: &str, format: Format) -> Result<Imported> {
    match format {
        Format::Continue => {
            let config: Continue = serde_yaml_ng::from_str(source)
                .map_err(|_| anyhow::anyhow!("Invalid Continue YAML or unsupported field type; source hidden to protect credentials"))?;
            convert_continue(config)
        }
        Format::OpenCode => {
            let config: OpenCode = json5::from_str(source)
                .map_err(|_| anyhow::anyhow!("Invalid OpenCode JSON/JSONC or unsupported field type; source hidden to protect credentials"))?;
            convert_opencode(config)
        }
    }
}
fn warnings(fields: &BTreeMap<String, Value>, scope: &str, warnings: &mut Vec<String>) {
    for key in fields.keys() {
        warnings.push(format!("Ignored {scope} field: {key}"));
    }
}
fn credentials(values: BTreeMap<String, String>) -> Result<BTreeMap<String, Secret>> {
    values
        .into_iter()
        .map(|(k, v)| Ok((k, Secret::imported(v)?)))
        .collect()
}
fn set_timeout(profile: &mut Profile, milliseconds: Option<u64>) -> Result<()> {
    if let Some(ms) = milliseconds {
        ensure!(ms > 0, "Request timeout must be positive");
        profile.request_timeout_secs = ms.div_ceil(1000);
        // Continue has a single request timeout. Preserve long prompt-prefill waits.
        profile.idle_timeout_secs = profile.request_timeout_secs;
    }
    Ok(())
}
fn finish(
    profiles: BTreeMap<String, Profile>,
    preferred: Option<String>,
    warnings: Vec<String>,
) -> Result<Imported> {
    ensure!(!profiles.is_empty(), "No supported models found");
    for p in profiles.values() {
        p.validate()?;
    }
    let default_profile = preferred
        .filter(|n| profiles.get(n).is_some_and(Profile::supports_chat))
        .or_else(|| {
            profiles
                .iter()
                .find(|(_, p)| p.supports_chat())
                .map(|(n, _)| n.clone())
        })
        .context("Import must contain at least one chat/edit/apply model")?;
    Ok(Imported {
        config: Config {
            memory: Default::default(),
            default_profile,
            profiles,
        },
        warnings,
    })
}

#[derive(Deserialize)]
struct Continue {
    schema: Option<String>,
    #[serde(default)]
    models: Vec<ContinueModel>,
    #[serde(flatten)]
    extra: BTreeMap<String, Value>,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ContinueModel {
    name: String,
    provider: String,
    model: String,
    api_base: Option<String>,
    api_key: Option<String>,
    capabilities: Option<Vec<String>>,
    roles: Option<Vec<ModelRole>>,
    #[serde(default)]
    default_completion_options: ContinueCompletion,
    #[serde(default)]
    request_options: ContinueRequest,
    #[serde(flatten)]
    extra: BTreeMap<String, Value>,
}
#[derive(Default, Deserialize)]
#[serde(default, rename_all = "camelCase", deny_unknown_fields)]
struct ContinueCompletion {
    context_length: Option<usize>,
    max_tokens: Option<usize>,
    temperature: Option<f64>,
    top_p: Option<f64>,
    top_k: Option<u32>,
    min_p: Option<f64>,
    presence_penalty: Option<f64>,
    frequency_penalty: Option<f64>,
    stop: Vec<String>,
    stream: Option<bool>,
}
#[derive(Default, Deserialize)]
#[serde(default, rename_all = "camelCase", deny_unknown_fields)]
struct ContinueRequest {
    timeout: Option<u64>,
    headers: BTreeMap<String, String>,
    extra_body_properties: BTreeMap<String, Value>,
}
fn convert_continue(mut config: Continue) -> Result<Imported> {
    ensure!(
        config.schema.as_deref().is_none_or(|v| v == "v1"),
        "Only Continue schema v1 is supported"
    );
    let mut notes = vec![];
    config.extra.remove("name");
    config.extra.remove("version");
    warnings(&config.extra, "Continue", &mut notes);
    let mut profiles = BTreeMap::new();
    let mut preferred = None;
    for model in config.models {
        ensure!(
            model.provider == "openai",
            "Continue import currently supports provider: openai (including compatible custom endpoints)"
        );
        warnings(&model.extra, "Continue model", &mut notes);
        let completion = model.default_completion_options;
        let mut profile = Profile {
            model: model.model,
            base_url: model.api_base.context("Custom models require apiBase")?,
            api_key: model.api_key.map(Secret::imported).transpose()?,
            headers: credentials(model.request_options.headers)?,
            roles: model
                .roles
                .unwrap_or_else(|| vec![ModelRole::Chat, ModelRole::Edit, ModelRole::Apply]),
            completion: CompletionOptions {
                temperature: completion.temperature,
                top_p: completion.top_p,
                top_k: completion.top_k,
                min_p: completion.min_p,
                presence_penalty: completion.presence_penalty,
                frequency_penalty: completion.frequency_penalty,
                stop: completion.stop,
            },
            extra_body: model.request_options.extra_body_properties,
            ..Profile::default()
        };
        if let Some(tokens) = completion.context_length {
            profile.context_tokens = tokens;
        }
        if let Some(tokens) = completion.max_tokens {
            profile.max_output_tokens = tokens;
        }
        if let Some(stream) = completion.stream {
            profile.stream = stream;
        }
        profile.tools = profile.supports_chat()
            && model
                .capabilities
                .is_none_or(|c| c.iter().any(|s| s == "tool_use"));
        set_timeout(&mut profile, model.request_options.timeout)?;
        ensure!(!model.name.trim().is_empty(), "Model name cannot be empty");
        if preferred.is_none() && profile.supports_chat() {
            preferred = Some(model.name.clone());
        }
        ensure!(
            profiles.insert(model.name, profile).is_none(),
            "Duplicate Continue model name"
        );
    }
    finish(profiles, preferred, notes)
}

#[derive(Deserialize)]
struct OpenCode {
    model: Option<String>,
    #[serde(default)]
    provider: BTreeMap<String, OpenCodeProvider>,
    #[serde(flatten)]
    extra: BTreeMap<String, Value>,
}
#[derive(Deserialize)]
struct OpenCodeProvider {
    npm: String,
    #[serde(default)]
    options: OpenCodeOptions,
    #[serde(default)]
    models: BTreeMap<String, OpenCodeModel>,
    #[serde(flatten)]
    extra: BTreeMap<String, Value>,
}
#[derive(Default, Deserialize)]
#[serde(default, rename_all = "camelCase", deny_unknown_fields)]
struct OpenCodeOptions {
    #[serde(rename = "baseURL")]
    base_url: Option<String>,
    api_key: Option<String>,
    headers: BTreeMap<String, String>,
    timeout: Option<u64>,
    max_retries: Option<u32>,
    temperature: Option<f64>,
    top_p: Option<f64>,
    top_k: Option<u32>,
}
#[derive(Deserialize)]
struct OpenCodeModel {
    id: Option<String>,
    #[serde(default)]
    limit: Limits,
    #[serde(default)]
    options: BTreeMap<String, Value>,
    #[serde(default)]
    headers: BTreeMap<String, String>,
    tool_call: Option<bool>,
    #[serde(flatten)]
    extra: BTreeMap<String, Value>,
}
#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct Limits {
    context: Option<usize>,
    output: Option<usize>,
    input: Option<usize>,
}
fn convert_opencode(mut config: OpenCode) -> Result<Imported> {
    let mut notes = vec![];
    config.extra.remove("$schema");
    warnings(&config.extra, "OpenCode", &mut notes);
    let mut profiles = BTreeMap::new();
    for (provider_id, mut provider) in config.provider {
        ensure!(
            provider.npm == "@ai-sdk/openai-compatible",
            "OpenCode import requires npm: @ai-sdk/openai-compatible (Chat Completions)"
        );
        provider.extra.remove("name");
        warnings(&provider.extra, "OpenCode provider", &mut notes);
        for (model_id, mut model) in provider.models {
            // Protocol overrides cannot safely be ignored.
            ensure!(
                !model.extra.contains_key("provider"),
                "Per-model OpenCode provider overrides are not supported"
            );
            model.extra.remove("name");
            warnings(&model.extra, "OpenCode model", &mut notes);
            let mut headers = provider.options.headers.clone();
            for (key, value) in model.headers {
                headers.retain(|k, _| !k.eq_ignore_ascii_case(&key));
                headers.insert(key, value);
            }
            let mut profile = Profile {
                base_url: provider
                    .options
                    .base_url
                    .clone()
                    .context("Custom providers require options.baseURL")?,
                model: model.id.unwrap_or(model_id.clone()),
                api_key: provider
                    .options
                    .api_key
                    .clone()
                    .map(Secret::imported)
                    .transpose()?,
                headers: credentials(headers)?,
                completion: CompletionOptions {
                    temperature: provider.options.temperature,
                    top_p: provider.options.top_p,
                    top_k: provider.options.top_k,
                    ..Default::default()
                },
                tools: model.tool_call.unwrap_or(true),
                extra_body: model.options,
                ..Profile::default()
            };
            if let Some(tokens) = model.limit.context {
                profile.context_tokens = tokens;
            }
            if let Some(tokens) = model.limit.output {
                profile.max_output_tokens = tokens;
            }
            if model.limit.input.is_some() {
                notes.push(
                    "OpenCode limit.input is not imported; Builder budgets against limit.context"
                        .into(),
                );
            }
            set_timeout(&mut profile, provider.options.timeout)?;
            if let Some(retries) = provider.options.max_retries {
                profile.max_attempts = retries.checked_add(1).context("maxRetries is too large")?;
            }
            let name = format!("{provider_id}/{model_id}");
            if profiles.insert(name, profile).is_some() {
                bail!("Duplicate OpenCode model identifier");
            }
        }
    }
    finish(profiles, config.model, notes)
}
