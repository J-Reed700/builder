//! OpenAI-compatible `/chat/completions` (llama.cpp, Ollama, vLLM, gateways).
use super::{Options, RESPONSE_LIMIT, Request, sensitive};
use crate::{Activity, Event, OutputLimit, SseDecoder, failure::Failure, reply::validate};
use anyhow::{Result, bail, ensure};
use builder_core::config::{Profile, Secret};
use builder_core::protocol::{Function, Message, Role, ToolCall};
use futures_util::StreamExt;
use reqwest::header::{AUTHORIZATION, HeaderMap};
use serde_json::{Value, json};
use std::collections::BTreeMap;

/// Bearer auth from `api_key` or `api_key_env`. Explicit proxy auth wins, and
/// a shadowed API-key env var need not exist.
pub(super) fn authorize(profile: &Profile, headers: &mut HeaderMap) -> Result<()> {
    if headers.contains_key(AUTHORIZATION) {
        return Ok(());
    }
    let key = match (&profile.api_key, &profile.api_key_env) {
        (Some(secret), _) => Some(secret.resolve()?),
        (_, Some(env)) => Some(Secret::Environment { env: env.clone() }.resolve()?),
        _ => None,
    };
    if let Some(key) = key {
        headers.insert(
            AUTHORIZATION,
            sensitive(&format!("Bearer {key}"), "API key")?,
        );
    }
    Ok(())
}

pub(super) fn encode(
    profile: &Profile,
    messages: &[Message],
    tools: &[Value],
    options: &Options,
) -> Result<Request> {
    // Some Jinja chat templates accept exactly one leading system message.
    // Preserve all policy text, order, and durable records; merge only the
    // consecutive leading system messages in the outbound representation.
    let leading = messages
        .iter()
        .take_while(|m| m.role == Role::System)
        .count();
    let mut outbound = messages.to_vec();
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
    // Earlier-turn reasoning stays transcript-only unless the profile asks
    // for it; see `builder_core::config::ReasoningHistory`.
    let send_reasoning = profile.sends_reasoning();
    let mut wire = Vec::with_capacity(outbound.len());
    for message in &outbound {
        let mut value = serde_json::to_value(message)?;
        let object = value
            .as_object_mut()
            .expect("a message serializes as an object");
        object.remove("reasoning");
        if send_reasoning
            && message.role == Role::Assistant
            && let Some(text) = message
                .reasoning
                .as_deref()
                .filter(|t| !t.trim().is_empty())
        {
            object.insert("reasoning_content".into(), Value::String(text.to_owned()));
        }
        wire.push(value);
    }
    let prompt_bytes = serde_json::to_vec(&wire)?.len();
    let mut body = json!({"model": profile.model, "messages": wire, "stream": profile.stream, "max_tokens": options.max_output_tokens});
    if profile.tools && !tools.is_empty() {
        body["tools"] = json!(tools);
    }
    if options.json_mode {
        body["response_format"] = json!({"type": "json_object"});
    }
    let completion = serde_json::to_value(&profile.completion)?;
    for (key, value) in completion.as_object().expect("completion is a struct") {
        if !value.is_null() {
            body[key] = value.clone();
        }
    }
    for (key, value) in &profile.extra_body {
        body[key] = value.clone();
    }
    if options.suppress_thinking
        && (profile.extra_body.contains_key("chat_template_kwargs") || options.llama_cpp)
    {
        // Only where the switch is understood: an explicit extra_body entry
        // or a server that answered the llama.cpp probe.
        if !body["chat_template_kwargs"].is_object() {
            body["chat_template_kwargs"] = json!({});
        }
        body["chat_template_kwargs"]["enable_thinking"] = json!(false);
    }
    Ok(Request { body, prompt_bytes })
}

/// Decode a buffered (non-streaming) completion.
pub(super) fn parse(
    value: &Value,
    emit: &mut dyn FnMut(Event),
) -> std::result::Result<Message, Failure> {
    let choice = &value["choices"][0];
    check_finish(choice["finish_reason"].as_str()).map_err(Failure::finish)?;
    let mut message: Message = serde_json::from_value(choice["message"].clone())
        .map_err(|e| Failure::permanent(e.to_string()))?;
    validate(&message, reasoning_text(&choice["message"]).is_some())
        .map_err(Failure::validation)?;
    if let Some(text) = reasoning_text(&choice["message"]) {
        emit(Event::Reasoning(text.to_owned()));
        message.reasoning = Some(text.to_owned());
    }
    if let Some(content) = &message.content {
        emit(Event::Delta(content.clone()));
    }
    Ok(message)
}

/// Decode a streamed completion.
pub(super) async fn stream(
    response: reqwest::Response,
    emit: &mut dyn FnMut(Event),
) -> std::result::Result<Message, Failure> {
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
        if received > RESPONSE_LIMIT {
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
            let progress = &value["prompt_progress"];
            if let (Some(processed), Some(total)) =
                (progress["processed"].as_u64(), progress["total"].as_u64())
                && total > 0
            {
                emit(Event::PromptProgress {
                    processed: processed.min(total),
                    total,
                });
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
                if let Some(latest) = calls.values().next_back() {
                    emit(Event::ToolProgress {
                        name: latest.function.name.clone(),
                        calls: calls.len(),
                        bytes: calls
                            .values()
                            .map(|call| call.function.arguments.len())
                            .sum(),
                    });
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
fn reasoning_text(value: &Value) -> Option<&str> {
    ["reasoning_content", "reasoning"]
        .iter()
        .find_map(|key| value[*key].as_str().filter(|s| !s.trim().is_empty()))
}
