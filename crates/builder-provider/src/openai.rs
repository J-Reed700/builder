use crate::{Activity, Event, OutputLimit, Provider, SseDecoder};
use anyhow::{Context, Result, bail, ensure};
use builder_core::protocol::Role;
use builder_core::{
    config::Profile,
    protocol::{Function, Message, ToolCall},
};
use futures_util::StreamExt;
use serde_json::{Value, json};
use std::{collections::BTreeMap, time::Duration};

pub struct OpenAiCompatible {
    client: reqwest::Client,
    profile: Profile,
    headers: reqwest::header::HeaderMap,
}

#[derive(Debug)]
enum MissingResponse {
    Empty,
    ReasoningOnly,
}
impl std::fmt::Display for MissingResponse {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let reason = match self {
            Self::Empty => "Empty model response: endpoint finished without an answer or tool call",
            Self::ReasoningOnly => "Model returned only reasoning, without an answer or tool call",
        };
        write!(f, "{reason}. No response was committed")
    }
}
impl std::error::Error for MissingResponse {}

#[derive(Debug)]
struct Failure {
    reason: String,
    retry: bool,
    retry_after: Option<u64>,
    output_limit: bool,
}
impl Failure {
    fn validation(error: anyhow::Error) -> Self {
        if error.is::<MissingResponse>() {
            Self::transient(error.to_string())
        } else {
            Self::permanent(error.to_string())
        }
    }

    fn finish(error: anyhow::Error) -> Self {
        Self {
            output_limit: error.is::<OutputLimit>(),
            ..Self::permanent(error.to_string())
        }
    }
    fn transient(reason: impl Into<String>) -> Self {
        Self {
            reason: reason.into(),
            retry: true,
            retry_after: None,
            output_limit: false,
        }
    }
    fn permanent(reason: impl Into<String>) -> Self {
        Self {
            reason: reason.into(),
            retry: false,
            retry_after: None,
            output_limit: false,
        }
    }
}

impl OpenAiCompatible {
    pub fn new(profile: Profile) -> Result<Self> {
        profile.validate()?;
        let mut headers = reqwest::header::HeaderMap::new();
        for (name, secret) in &profile.headers {
            let name = reqwest::header::HeaderName::from_bytes(name.as_bytes())
                .map_err(|_| anyhow::anyhow!("Invalid custom header name"))?;
            ensure!(
                !headers.contains_key(&name),
                "Duplicate custom header (names are case-insensitive)"
            );
            let mut value =
                reqwest::header::HeaderValue::from_str(&secret.resolve()?).map_err(|_| {
                    anyhow::anyhow!(
                        "Invalid custom header value; expected a single-line HTTP header"
                    )
                })?;
            value.set_sensitive(true);
            headers.insert(name, value);
        }
        // Explicit proxy auth wins, and a shadowed API-key env var need not exist.
        if !headers.contains_key(reqwest::header::AUTHORIZATION) {
            let key = match (&profile.api_key, &profile.api_key_env) {
                (Some(secret), _) => Some(secret.resolve()?),
                (_, Some(env)) => {
                    Some(builder_core::config::Secret::Environment { env: env.clone() }.resolve()?)
                }
                _ => None,
            };
            if let Some(key) = key {
                let mut value = reqwest::header::HeaderValue::from_str(&format!("Bearer {key}"))
                    .map_err(|_| anyhow::anyhow!("Invalid API key header value"))?;
                value.set_sensitive(true);
                headers.insert(reqwest::header::AUTHORIZATION, value);
            }
        }
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
        })
    }
    fn request(&self, method: reqwest::Method, suffix: &str) -> reqwest::RequestBuilder {
        self.client
            .request(method, self.profile.endpoint(suffix))
            .headers(self.headers.clone())
    }
    pub async fn models(&self) -> Result<Value> {
        Ok(self
            .request(reqwest::Method::GET, "models")
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?)
    }
    /// A bounded, read-only embedding request. Never logs response bodies or credentials.
    pub async fn embed(&self, input: &str) -> Result<Vec<f32>> {
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
        let request = self
            .request(reqwest::Method::POST, "chat/completions")
            .json(body);
        let response = request
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
            // Do not persist response bodies: proxies can echo credentials.
            return Err(Failure {
                reason: format!("Endpoint returned HTTP {status}"),
                retry: matches!(status.as_u16(), 408 | 429 | 500 | 502 | 503 | 504),
                retry_after,
                output_limit: false,
            });
        }
        emit(Event::Activity(Activity::Connected));
        if !self.profile.stream {
            let mut stream = response.bytes_stream();
            let mut bytes = Vec::new();
            while let Some(chunk) = stream.next().await {
                let chunk = chunk.map_err(|e| Failure::transient(e.without_url().to_string()))?;
                if bytes.len().saturating_add(chunk.len()) > 16 * 1024 * 1024 {
                    return Err(Failure::permanent("Response exceeds 16 MiB"));
                }
                bytes.extend_from_slice(&chunk);
            }
            let value: Value = serde_json::from_slice(&bytes)
                .map_err(|_| Failure::transient("Endpoint returned incomplete or invalid JSON"))?;
            let choice = &value["choices"][0];
            check_finish(choice["finish_reason"].as_str()).map_err(Failure::finish)?;
            let mut message: Message = serde_json::from_value(choice["message"].clone())
                .map_err(|e| Failure::permanent(e.to_string()))?;
            validate(&message, has_reasoning(&choice["message"])).map_err(Failure::validation)?;
            if let Some(text) = reasoning_text(&choice["message"]) {
                emit(Event::Reasoning(text.to_owned()));
                message.reasoning = Some(text.to_owned());
            }
            if let Some(content) = &message.content {
                emit(Event::Delta(content.clone()));
            }
            return Ok(message);
        }
        let mut stream = response.bytes_stream();
        let mut decoder = SseDecoder::default();
        let mut message = Message::text(Role::Assistant, "");
        let mut calls: BTreeMap<u64, ToolCall> = BTreeMap::new();
        let mut finish = None;
        let mut received = 0usize;
        let mut thinking = false;
        let mut preparing_tools = false;
        while let Some(chunk) = stream.next().await {
            let bytes = chunk.map_err(|e| Failure::transient(e.without_url().to_string()))?;
            received += bytes.len();
            if received > 16 * 1024 * 1024 {
                return Err(Failure::permanent("Response exceeds 16 MiB"));
            }
            for event in decoder
                .push(&bytes)
                .map_err(|e| Failure::permanent(e.to_string()))?
            {
                if event == "[DONE]" {
                    if finish.is_none() {
                        return Err(Failure::transient("Stream ended without a finish reason"));
                    }
                    return assemble(message, calls, finish.as_deref(), thinking);
                }
                let value: Value = serde_json::from_str(&event)
                    .map_err(|e| Failure::permanent(format!("Invalid SSE JSON: {e}")))?;
                if value.get("error").is_some() {
                    return Err(Failure::transient("Endpoint returned an in-stream error"));
                }
                let choice = &value["choices"][0];
                if let Some(reason) = choice["finish_reason"].as_str() {
                    finish = Some(reason.to_owned());
                }
                let delta = &choice["delta"];
                if let Some(text) = reasoning_text(delta) {
                    if !thinking {
                        thinking = true;
                        emit(Event::Activity(Activity::Thinking));
                    }
                    message
                        .reasoning
                        .get_or_insert_with(String::new)
                        .push_str(text);
                    emit(Event::Reasoning(text.to_owned()));
                }
                if let Some(content) = delta["content"].as_str().filter(|s| !s.is_empty()) {
                    message.content.as_mut().unwrap().push_str(content);
                    emit(Event::Delta(content.into()));
                }
                if let Some(parts) = delta["tool_calls"].as_array() {
                    if !preparing_tools && !parts.is_empty() {
                        preparing_tools = true;
                        emit(Event::Activity(Activity::PreparingTools));
                    }
                    for part in parts {
                        let index = part["index"]
                            .as_u64()
                            .ok_or_else(|| Failure::permanent("Tool delta has no index"))?;
                        if index >= 128 {
                            return Err(Failure::permanent("Too many tool calls"));
                        }
                        let call = calls.entry(index).or_insert_with(|| ToolCall {
                            id: String::new(),
                            kind: "function".into(),
                            function: Function::default(),
                        });
                        if let Some(id) = part["id"].as_str() {
                            call.id.push_str(id);
                        }
                        if let Some(name) = part["function"]["name"].as_str() {
                            call.function.name.push_str(name);
                        }
                        if let Some(args) = part["function"]["arguments"].as_str() {
                            call.function.arguments.push_str(args);
                        }
                    }
                }
            }
        }
        // Some compatible servers use a finish_reason plus EOF instead of [DONE].
        if finish.is_some() {
            assemble(message, calls, finish.as_deref(), thinking)
        } else {
            Err(Failure::transient(
                "Connection ended before the generation completed",
            ))
        }
    }
}

impl Provider for OpenAiCompatible {
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

impl OpenAiCompatible {
    async fn respond(
        &self,
        messages: &[Message],
        tools: &[Value],
        max_output_tokens: usize,
        json_mode: bool,
        emit: &mut dyn FnMut(Event),
    ) -> Result<Message> {
        // Some Jinja chat templates accept exactly one leading system message.
        // Preserve all policy text, order, and durable records; merge only the
        // consecutive leading system messages in the outbound representation.
        let leading = messages
            .iter()
            .take_while(|m| m.role == Role::System)
            .count();
        let mut outbound = messages.to_vec();
        // Reasoning is transcript-only: servers differ on whether they accept it
        // back, and it would only inflate the prompt.
        for message in &mut outbound {
            message.reasoning = None;
        }
        if leading > 1 {
            ensure!(
                messages[..leading]
                    .iter()
                    .all(|m| m.tool_calls.is_empty() && m.tool_call_id.is_none()),
                "Leading system messages must not contain tool metadata"
            );
            let system = Message::text(
                Role::System,
                messages[..leading]
                    .iter()
                    .filter_map(|m| m.content.as_deref())
                    .collect::<Vec<_>>()
                    .join("\n\n"),
            );
            outbound.splice(..leading, [system]);
        }
        let prompt_bytes = serde_json::to_vec(&outbound)?.len();
        let mut body = json!({"model": self.profile.model, "messages": outbound, "stream": self.profile.stream, "max_tokens": max_output_tokens});
        if self.profile.tools && !tools.is_empty() {
            body["tools"] = json!(tools);
        }
        if json_mode {
            body["response_format"] = json!({"type": "json_object"});
        }
        let completion = serde_json::to_value(&self.profile.completion)?;
        for (key, value) in completion.as_object().expect("completion is a struct") {
            if !value.is_null() {
                body[key] = value.clone();
            }
        }
        for (key, value) in &self.profile.extra_body {
            body[key] = value.clone();
        }
        emit(Event::Prompt {
            bytes: prompt_bytes,
        });
        // Identical immutable request body on every attempt. Partial output never
        // enters the durable conversation and can never dispatch a tool.
        for number in 1..=self.profile.max_attempts {
            emit(Event::Attempt {
                number,
                maximum: self.profile.max_attempts,
            });
            match self.attempt(&body, emit).await {
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
fn check_finish(reason: Option<&str>) -> Result<()> {
    match reason {
        Some("stop" | "tool_calls") => Ok(()),
        Some("length") => Err(OutputLimit.into()),
        Some(other) => bail!("Unsupported finish reason: {other}"),
        None => bail!("Missing finish reason"),
    }
}
fn assemble(
    mut message: Message,
    calls: BTreeMap<u64, ToolCall>,
    reason: Option<&str>,
    thinking: bool,
) -> std::result::Result<Message, Failure> {
    check_finish(reason).map_err(Failure::finish)?;
    message.tool_calls = calls.into_values().collect();
    validate(&message, thinking).map_err(Failure::validation)?;
    Ok(message)
}
fn has_reasoning(value: &Value) -> bool {
    reasoning_text(value).is_some()
}
fn reasoning_text(value: &Value) -> Option<&str> {
    ["reasoning_content", "reasoning"]
        .iter()
        .find_map(|key| value[*key].as_str().filter(|s| !s.trim().is_empty()))
}

fn validate(message: &Message, thinking: bool) -> Result<()> {
    ensure!(message.role == Role::Assistant, "Expected assistant role");
    ensure!(
        message
            .content
            .as_ref()
            .is_some_and(|s| !s.trim().is_empty())
            || !message.tool_calls.is_empty(),
        if thinking {
            MissingResponse::ReasoningOnly
        } else {
            MissingResponse::Empty
        }
    );
    let mut ids = std::collections::HashSet::new();
    for call in &message.tool_calls {
        ensure!(
            !call.id.is_empty() && ids.insert(&call.id),
            "Missing or duplicate tool call ID"
        );
        ensure!(
            call.kind == "function" && !call.function.name.is_empty(),
            "Invalid tool call"
        );
        let args: Value =
            serde_json::from_str(&call.function.arguments).context("Incomplete tool arguments")?;
        ensure!(args.is_object(), "Tool arguments must be an object");
    }
    Ok(())
}
