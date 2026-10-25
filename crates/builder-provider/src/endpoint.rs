//! One configured model endpoint: HTTP transport, resolved credentials,
//! capability probes, and the retry policy. The wire format is delegated to
//! the profile's [`Protocol`], so every API shares the same guarantees.
use crate::protocol::{Options, Protocol};
use crate::{Event, OutputLimit, Provider, failure::Failure};
use anyhow::{Context, Result, bail, ensure};
use builder_core::{config::Profile, protocol::Message};
use futures_util::StreamExt;
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use serde_json::{Value, json};
use std::time::Duration;

/// What one `/props` probe learned about a llama.cpp server.
#[derive(Clone, Copy, Debug)]
struct ServerProps {
    slots: usize,
    context: Option<usize>,
}

pub struct Endpoint {
    client: reqwest::Client,
    profile: Profile,
    headers: HeaderMap,
    protocol: Protocol,
    props: tokio::sync::OnceCell<Option<ServerProps>>,
    /// Derived requests (memory extraction) that must not spend their small
    /// output budget on hidden reasoning.
    suppress_thinking: bool,
}

impl Endpoint {
    /// The same endpoint, with thinking switched off where the protocol or
    /// server exposes the switch. Used for memory extraction, whose JSON
    /// budget hidden reasoning would otherwise consume.
    pub fn without_thinking(mut self) -> Self {
        self.suppress_thinking = true;
        self
    }
    pub fn new(profile: Profile) -> Result<Self> {
        profile.validate()?;
        let mut headers = HeaderMap::new();
        for (name, secret) in &profile.headers {
            let name = HeaderName::from_bytes(name.as_bytes())
                .map_err(|_| anyhow::anyhow!("Invalid custom header name"))?;
            ensure!(
                !headers.contains_key(&name),
                "Duplicate custom header (names are case-insensitive)"
            );
            let mut value = HeaderValue::from_str(&secret.resolve()?).map_err(|_| {
                anyhow::anyhow!("Invalid custom header value; expected a single-line HTTP header")
            })?;
            value.set_sensitive(true);
            headers.insert(name, value);
        }
        let protocol = Protocol::connect(&profile, &mut headers)?;
        let client = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(profile.connect_timeout_secs))
            .read_timeout(Duration::from_secs(profile.idle_timeout_secs))
            .timeout(Duration::from_secs(profile.request_timeout_secs))
            .redirect(reqwest::redirect::Policy::none())
            .build()?;
        Ok(Self {
            client,
            profile,
            headers,
            protocol,
            props: tokio::sync::OnceCell::new(),
            suppress_thinking: false,
        })
    }
    fn request(&self, method: reqwest::Method, suffix: &str) -> reqwest::RequestBuilder {
        self.client
            .request(method, self.profile.endpoint(suffix))
            .headers(self.headers.clone())
    }
    /// The endpoint's model list (`GET /models`; both protocols serve it).
    pub async fn models(&self) -> Result<Value> {
        Ok(self
            .request(reqwest::Method::GET, "models")
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?)
    }
    // Probe once per connection, including unsupported/error responses. Strip only
    // the compatibility API suffix so reverse-proxy path prefixes are preserved.
    async fn discover_props(&self) -> Option<ServerProps> {
        if !self.protocol.has_server_extensions() {
            return None;
        }
        let base = self.profile.base_url.trim_end_matches('/');
        let root = base.strip_suffix("/v1").unwrap_or(base);
        let response = self
            .client
            .get(format!("{root}/props"))
            .headers(self.headers.clone())
            .query(&[("model", self.profile.model.as_str())])
            .timeout(Duration::from_secs(2))
            .send()
            .await
            .ok()?
            .error_for_status()
            .ok()?;
        let mut bytes = Vec::new();
        let mut stream = response.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.ok()?;
            if bytes.len().saturating_add(chunk.len()) > 1024 * 1024 {
                return None;
            }
            bytes.extend_from_slice(&chunk);
        }
        let value: Value = serde_json::from_slice(&bytes).ok()?;
        let slots = usize::try_from(value.get("total_slots")?.as_u64()?).ok()?;
        if slots == 0 {
            return None;
        }
        // llama.cpp reports the per-slot window here; a missing or zero value
        // leaves the configured window in charge.
        let context = value
            .get("default_generation_settings")
            .and_then(|settings| settings.get("n_ctx"))
            .and_then(Value::as_u64)
            .and_then(|n| usize::try_from(n).ok())
            .filter(|&n| n > 0);
        Some(ServerProps { slots, context })
    }
    async fn props(&self) -> Option<ServerProps> {
        *self
            .props
            .get_or_init(|| async {
                tokio::time::timeout(Duration::from_secs(2), self.discover_props())
                    .await
                    .unwrap_or(None)
            })
            .await
    }
    /// A bounded, read-only embedding request. Never logs response bodies or credentials.
    pub async fn embed(&self, input: &str) -> Result<Vec<f32>> {
        ensure!(
            self.protocol.has_server_extensions(),
            "This profile's API has no embeddings endpoint; use a local or OpenAI-compatible embedding profile"
        );
        ensure!(
            !input.is_empty() && input.len() <= 8192,
            "Embedding input must be 1–8192 bytes"
        );
        let response = self
            .request(reqwest::Method::POST, "embeddings")
            .timeout(Duration::from_secs(8))
            .json(&json!({"model":self.profile.model,"input":input,"encoding_format":"float"}))
            .send()
            .await
            .map_err(|e| anyhow::anyhow!("Embedding transport: {}", e.without_url()))?;
        ensure!(
            response.status().is_success(),
            "Embedding endpoint returned HTTP {}",
            response.status()
        );
        let mut bytes = Vec::new();
        let mut stream = response.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|_| anyhow::anyhow!("Embedding stream failed"))?;
            ensure!(
                bytes.len() + chunk.len() <= 512 * 1024,
                "Embedding response exceeds 512 KiB"
            );
            bytes.extend_from_slice(&chunk);
        }
        let value: Value = serde_json::from_slice(&bytes)
            .map_err(|_| anyhow::anyhow!("Invalid embedding response JSON"))?;
        let data = value["data"].as_array().context("Missing embedding data")?;
        ensure!(
            data.len() == 1 && data[0]["index"] == 0,
            "Expected exactly one indexed embedding"
        );
        let values = data[0]["embedding"]
            .as_array()
            .context("Embedding must be float values")?;
        let vector = values
            .iter()
            .map(|v| {
                v.as_f64()
                    .map(|f| f as f32)
                    .context("Invalid embedding value")
            })
            .collect::<Result<Vec<_>>>()?;
        builder_core::memory::validate_vector(&vector)?;
        Ok(vector)
    }
    async fn attempt(
        &self,
        body: &Value,
        emit: &mut dyn FnMut(Event),
    ) -> std::result::Result<Message, Failure> {
        let response = self
            .request(reqwest::Method::POST, self.protocol.generate_path())
            .json(body)
            .send()
            .await
            .map_err(|e| Failure::transient(e.without_url().to_string()))?;
        let status = response.status();
        if !status.is_success() {
            let retry_after = response
                .headers()
                .get("retry-after")
                .and_then(|s| s.to_str().ok())
                .and_then(|s| s.parse().ok());
            return Err(Failure::status(status, retry_after));
        }
        emit(Event::Activity(crate::Activity::Connected));
        self.protocol
            .decode(response, self.profile.stream, emit)
            .await
    }
    async fn respond(
        &self,
        messages: &[Message],
        tools: &[Value],
        max_output_tokens: usize,
        json_mode: bool,
        emit: &mut dyn FnMut(Event),
    ) -> Result<Message> {
        let options = Options {
            max_output_tokens,
            json_mode,
            suppress_thinking: self.suppress_thinking,
            // Probe only when it can change the request.
            llama_cpp: self.suppress_thinking && self.props().await.is_some(),
        };
        let request = self
            .protocol
            .encode(&self.profile, messages, tools, &options)?;
        emit(Event::Prompt {
            bytes: request.prompt_bytes,
        });
        // Identical immutable request body on every attempt. Partial output never
        // enters the durable conversation and can never dispatch a tool.
        for number in 1..=self.profile.max_attempts {
            emit(Event::Attempt {
                number,
                maximum: self.profile.max_attempts,
            });
            match self.attempt(&request.body, emit).await {
                Ok(message) => return Ok(message),
                Err(failure) => {
                    if !failure.retry || number == self.profile.max_attempts {
                        if failure.output_limit {
                            return Err(OutputLimit.into());
                        }
                        bail!(
                            "{} (attempt {number}/{}). Conversation saved; resume to retry.",
                            failure.reason,
                            self.profile.max_attempts
                        );
                    }
                    let ceiling = (500u64 * 2u64.pow(number - 1)).min(30_000);
                    let jitter = rand::random_range(ceiling / 2..=ceiling);
                    let delay_ms = failure
                        .retry_after
                        .map(|s| s.saturating_mul(1000).min(300_000))
                        .unwrap_or(jitter);
                    emit(Event::Retry {
                        delay_ms,
                        reason: failure.reason,
                    });
                    tokio::time::sleep(Duration::from_millis(delay_ms)).await;
                }
            }
        }
        unreachable!("profile validates a nonzero attempt count")
    }
}

impl Provider for Endpoint {
    async fn parallel_capacity(&self) -> Option<usize> {
        self.props().await.map(|props| props.slots)
    }
    async fn context_capacity(&self) -> Option<usize> {
        self.props().await.and_then(|props| props.context)
    }
    async fn complete(
        &self,
        messages: &[Message],
        tools: &[Value],
        emit: &mut dyn FnMut(Event),
    ) -> Result<Message> {
        self.complete_with_budget(messages, tools, self.profile.max_output_tokens, emit)
            .await
    }
    async fn complete_with_budget(
        &self,
        messages: &[Message],
        tools: &[Value],
        max_output_tokens: usize,
        emit: &mut dyn FnMut(Event),
    ) -> Result<Message> {
        self.respond(messages, tools, max_output_tokens, false, emit)
            .await
    }
    async fn complete_json(
        &self,
        messages: &[Message],
        max_output_tokens: usize,
        emit: &mut dyn FnMut(Event),
    ) -> Result<Message> {
        self.respond(messages, &[], max_output_tokens, true, emit)
            .await
    }
}
