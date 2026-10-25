//! Anthropic Messages API translation for [`crate::OpenAiCompatible`] when a
//! profile sets `api = "anthropic"`. Transport, retries, and durable message
//! validation stay shared; this module only maps the wire format and auth.
use crate::openai::{Failure, validate};
use crate::{Activity, Event, OutputLimit, SseDecoder};
use anyhow::{Result, bail, ensure};
use builder_core::config::{Profile, Secret};
use builder_core::protocol::{Function, Message, Role, ToolCall};
use futures_util::StreamExt;
use reqwest::header::{AUTHORIZATION, HeaderMap, HeaderName, HeaderValue, USER_AGENT};
use serde_json::{Value, json};
use std::collections::BTreeMap;

pub(crate) const VERSION: &str = "2023-06-01";
/// Claude Pro/Max subscription tokens carry this marker; API keys never do.
const OAUTH_MARKER: &str = "sk-ant-oat";
const OAUTH_BETAS: &str = "claude-code-20250219,oauth-2025-04-20";
/// Subscription tokens are only accepted from requests that identify as
/// Claude Code (same identity pi sends).
const OAUTH_USER_AGENT: &str = "claude-cli/2.1.280";
const OAUTH_IDENTITY: &str = "You are Claude Code, Anthropic's official CLI for Claude.";
/// Environment fallbacks, in priority order, when the profile has no credential.
pub const TOKEN_ENV: [&str; 2] = ["ANTHROPIC_OAUTH_TOKEN", "ANTHROPIC_API_KEY"];

fn sensitive(value: &str) -> Result<HeaderValue> {
    let mut value = HeaderValue::from_str(value)
        .map_err(|_| anyhow::anyhow!("Invalid Claude credential; expected a single-line token"))?;
    value.set_sensitive(true);
    Ok(value)
}

/// The profile's credential: `api_key`, then `api_key_env`, then the standard
/// Anthropic environment variables. Profile settings override the environment.
fn token(profile: &Profile) -> Result<Option<String>> {
    if let Some(secret) = &profile.api_key {
        return secret.resolve().map(Some);
    }
    if let Some(env) = &profile.api_key_env {
        return Secret::Environment { env: env.clone() }.resolve().map(Some);
    }
    Ok(TOKEN_ENV
        .iter()
        .find_map(|env| std::env::var(env).ok().filter(|v| !v.trim().is_empty())))
}

/// Adds Anthropic auth and version headers. Returns whether the credential is
/// a subscription OAuth token, which changes the request body.
pub(crate) fn authorize(profile: &Profile, headers: &mut HeaderMap) -> Result<bool> {
    headers
        .entry(HeaderName::from_static("anthropic-version"))
        .or_insert(HeaderValue::from_static(VERSION));
    // Explicit proxy auth wins, as for OpenAI-compatible profiles.
    if headers.contains_key(AUTHORIZATION) || headers.contains_key("x-api-key") {
        return Ok(false);
    }
    let Some(token) = token(profile)? else {
        bail!(
            "No Claude credential. Set ANTHROPIC_API_KEY or ANTHROPIC_OAUTH_TOKEN, or run `builder config token`"
        );
    };
    let token = token.trim();
    if !token.contains(OAUTH_MARKER) {
        headers.insert(HeaderName::from_static("x-api-key"), sensitive(token)?);
        return Ok(false);
    }
    headers.insert(AUTHORIZATION, sensitive(&format!("Bearer {token}"))?);
    let beta = HeaderName::from_static("anthropic-beta");
    match headers.get(&beta).and_then(|v| v.to_str().ok()) {
        Some(existing) if existing.contains("oauth-2025-04-20") => {}
        Some(existing) => {
            let merged = format!("{OAUTH_BETAS},{existing}");
            headers.insert(beta, HeaderValue::from_str(&merged)?);
        }
        None => {
            headers.insert(beta, HeaderValue::from_static(OAUTH_BETAS));
        }
    }
    headers
        .entry(USER_AGENT)
        .or_insert(HeaderValue::from_static(OAUTH_USER_AGENT));
    headers
        .entry(HeaderName::from_static("x-app"))
        .or_insert(HeaderValue::from_static("cli"));
    Ok(true)
}

/// Anthropic tool IDs must match `^[a-zA-Z0-9_-]+$`; history written by other
/// providers may not. The mapping is deterministic so pairs stay matched.
fn tool_id(id: &str) -> String {
    let id: String = id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect();
    if id.is_empty() { "tool".into() } else { id }
}

fn text_block(text: &str) -> Value {
    json!({"type": "text", "text": text})
}

/// Appends content to the conversation, merging into the previous turn when
/// the role repeats (the API requires alternation in practice for tool results).
fn push(wire: &mut Vec<Value>, role: &str, blocks: Vec<Value>) {
    if blocks.is_empty() {
        return;
    }
    if let Some(last) = wire.last_mut()
        && last["role"] == role
        && let Some(content) = last["content"].as_array_mut()
    {
        // Tool results must lead a user turn that answers tool calls.
        let results_first = blocks.iter().any(|b| b["type"] == "tool_result");
        if results_first {
            let at = content
                .iter()
                .take_while(|b| b["type"] == "tool_result")
                .count();
            content.splice(at..at, blocks);
        } else {
            content.extend(blocks);
        }
        return;
    }
    wire.push(json!({"role": role, "content": blocks}));
}

/// Build the Messages request body. Returns the body and prompt size.
pub(crate) fn body(
    profile: &Profile,
    messages: &[Message],
    tools: &[Value],
    max_output_tokens: usize,
    oauth: bool,
    suppress_thinking: bool,
) -> Result<(Value, usize)> {
    let leading = messages
        .iter()
        .take_while(|m| m.role == Role::System)
        .count();
    let mut system: Vec<Value> = Vec::new();
    if oauth {
        system.push(text_block(OAUTH_IDENTITY));
    }
    for message in &messages[..leading] {
        ensure!(
            message.tool_calls.is_empty() && message.tool_call_id.is_none(),
            "Leading system messages must not contain tool metadata"
        );
        if let Some(text) = message.content.as_deref().filter(|t| !t.trim().is_empty()) {
            system.push(text_block(text));
        }
    }
    let mut wire: Vec<Value> = Vec::with_capacity(messages.len());
    for message in &messages[leading..] {
        let text = message.content.as_deref().filter(|t| !t.trim().is_empty());
        match message.role {
            // Later system messages are request-only context; the API has one
            // top-level system field, so they travel as tagged user content.
            Role::System => {
                if let Some(text) = text {
                    push(
                        &mut wire,
                        "user",
                        vec![text_block(&format!("<system>\n{text}\n</system>"))],
                    );
                }
            }
            Role::User => push(
                &mut wire,
                "user",
                vec![text_block(text.unwrap_or("(empty)"))],
            ),
            Role::Assistant => {
                let mut blocks: Vec<Value> = text.map(text_block).into_iter().collect();
                for call in &message.tool_calls {
                    let input: Value = serde_json::from_str(&call.function.arguments)
                        .ok()
                        .filter(Value::is_object)
                        .unwrap_or_else(|| json!({}));
                    blocks.push(json!({
                        "type": "tool_use",
                        "id": tool_id(&call.id),
                        "name": call.function.name,
                        "input": input,
                    }));
                }
                push(&mut wire, "assistant", blocks);
            }
            Role::Tool => {
                let id = message.tool_call_id.as_deref().unwrap_or_default();
                push(
                    &mut wire,
                    "user",
                    vec![json!({
                        "type": "tool_result",
                        "tool_use_id": tool_id(id),
                        "content": text.unwrap_or("(no output)"),
                    })],
                );
            }
        }
    }
    // The conversation must open with a user turn.
    if wire.first().is_none_or(|m| m["role"] != "user") {
        wire.insert(
            0,
            json!({"role": "user", "content": [text_block("(continue)")]}),
        );
    }
    // Cache the stable prefix: system prompt and tool definitions.
    if let Some(last) = system.last_mut() {
        last["cache_control"] = json!({"type": "ephemeral"});
    }
    let prompt_bytes = serde_json::to_vec(&wire)?.len() + serde_json::to_vec(&system)?.len();
    let mut body = json!({
        "model": profile.model,
        "messages": wire,
        "max_tokens": max_output_tokens,
        "stream": profile.stream,
    });
    if !system.is_empty() {
        body["system"] = Value::Array(system);
    }
    if profile.tools && !tools.is_empty() {
        let mut defs: Vec<Value> = tools
            .iter()
            .map(|tool| {
                let f = tool.get("function").unwrap_or(tool);
                json!({
                    "name": f["name"],
                    "description": f.get("description").cloned().unwrap_or(json!("")),
                    "input_schema": f.get("parameters").cloned()
                        .unwrap_or_else(|| json!({"type": "object", "properties": {}})),
                })
            })
            .collect();
        if let Some(last) = defs.last_mut() {
            last["cache_control"] = json!({"type": "ephemeral"});
        }
        body["tools"] = Value::Array(defs);
    }
    let c = &profile.completion;
    if let Some(v) = c.temperature {
        // Anthropic accepts 0..=1.
        body["temperature"] = json!(v.min(1.0));
    }
    if let Some(v) = c.top_p {
        body["top_p"] = json!(v);
    }
    if let Some(v) = c.top_k {
        body["top_k"] = json!(v);
    }
    if !c.stop.is_empty() {
        body["stop_sequences"] = json!(c.stop);
    }
    for (key, value) in &profile.extra_body {
        body[key] = value.clone();
    }
    if suppress_thinking && let Some(object) = body.as_object_mut() {
        object.remove("thinking");
    }
    Ok((body, prompt_bytes))
}

fn check_stop(reason: Option<&str>) -> std::result::Result<(), Failure> {
    match reason {
        Some("end_turn" | "stop_sequence" | "tool_use") => Ok(()),
        Some("max_tokens" | "model_context_window_exceeded") => {
            Err(Failure::finish(OutputLimit.into()))
        }
        Some("refusal") => Err(Failure::permanent("Claude declined to respond (refusal)")),
        Some(other) => Err(Failure::permanent(format!(
            "Unsupported stop reason: {other}"
        ))),
        None => Err(Failure::transient("Response ended without a stop reason")),
    }
}

fn finish(
    mut message: Message,
    calls: BTreeMap<u64, ToolCall>,
    reason: Option<&str>,
) -> std::result::Result<Message, Failure> {
    check_stop(reason)?;
    message.tool_calls = calls
        .into_values()
        .map(|mut call| {
            if call.function.arguments.trim().is_empty() {
                call.function.arguments = "{}".into();
            }
            call
        })
        .collect();
    let thinking = message.reasoning.is_some();
    validate(&message, thinking).map_err(Failure::validation)?;
    Ok(message)
}

fn new_call(block: &Value) -> ToolCall {
    ToolCall {
        id: block["id"].as_str().unwrap_or_default().into(),
        kind: "function".into(),
        function: Function {
            name: block["name"].as_str().unwrap_or_default().into(),
            arguments: String::new(),
        },
    }
}

/// Parse a non-streaming Messages response.
pub(crate) fn parse(
    value: &Value,
    emit: &mut dyn FnMut(Event),
) -> std::result::Result<Message, Failure> {
    let mut message = Message::text(Role::Assistant, "");
    let mut calls = BTreeMap::new();
    for (index, block) in value["content"]
        .as_array()
        .into_iter()
        .flatten()
        .enumerate()
    {
        match block["type"].as_str() {
            Some("text") => {
                let text = block["text"].as_str().unwrap_or_default();
                message.content.as_mut().unwrap().push_str(text);
                emit(Event::Delta(text.into()));
            }
            Some("thinking") => {
                let text = block["thinking"].as_str().unwrap_or_default();
                message
                    .reasoning
                    .get_or_insert_with(String::new)
                    .push_str(text);
                emit(Event::Reasoning(text.into()));
            }
            Some("tool_use") => {
                let mut call = new_call(block);
                call.function.arguments = block["input"].to_string();
                calls.insert(index as u64, call);
            }
            _ => {}
        }
    }
    finish(message, calls, value["stop_reason"].as_str())
}

/// Consume a streaming Messages response.
pub(crate) async fn stream(
    response: reqwest::Response,
    emit: &mut dyn FnMut(Event),
) -> std::result::Result<Message, Failure> {
    let mut bytes_stream = response.bytes_stream();
    let mut decoder = SseDecoder::default();
    let mut message = Message::text(Role::Assistant, "");
    let mut calls: BTreeMap<u64, ToolCall> = BTreeMap::new();
    let mut stop: Option<String> = None;
    let mut received = 0usize;
    let mut thinking = false;
    let mut preparing_tools = false;
    while let Some(chunk) = bytes_stream.next().await {
        let bytes = chunk.map_err(|e| Failure::transient(e.without_url().to_string()))?;
        received += bytes.len();
        if received > 16 * 1024 * 1024 {
            return Err(Failure::permanent("Response exceeds 16 MiB"));
        }
        for event in decoder
            .push(&bytes)
            .map_err(|e| Failure::permanent(e.to_string()))?
        {
            let value: Value = serde_json::from_str(&event)
                .map_err(|e| Failure::permanent(format!("Invalid SSE JSON: {e}")))?;
            match value["type"].as_str() {
                Some("error") => {
                    // overloaded_error and api_error are worth another attempt.
                    let kind = value["error"]["type"].as_str().unwrap_or("error");
                    let reason = format!("Claude returned an in-stream {kind}");
                    return Err(if matches!(kind, "overloaded_error" | "api_error") {
                        Failure::transient(reason)
                    } else {
                        Failure::permanent(reason)
                    });
                }
                Some("content_block_start") => {
                    let index = value["index"].as_u64().unwrap_or_default();
                    let block = &value["content_block"];
                    match block["type"].as_str() {
                        Some("tool_use") => {
                            if index >= 128 {
                                return Err(Failure::permanent("Too many tool calls"));
                            }
                            if !preparing_tools {
                                preparing_tools = true;
                                emit(Event::Activity(Activity::PreparingTools));
                            }
                            calls.insert(index, new_call(block));
                        }
                        Some("thinking" | "redacted_thinking") if !thinking => {
                            thinking = true;
                            emit(Event::Activity(Activity::Thinking));
                        }
                        _ => {}
                    }
                }
                Some("content_block_delta") => {
                    let index = value["index"].as_u64().unwrap_or_default();
                    let delta = &value["delta"];
                    match delta["type"].as_str() {
                        Some("text_delta") => {
                            if let Some(text) = delta["text"].as_str().filter(|s| !s.is_empty()) {
                                message.content.as_mut().unwrap().push_str(text);
                                emit(Event::Delta(text.into()));
                            }
                        }
                        Some("thinking_delta") => {
                            if let Some(text) = delta["thinking"].as_str() {
                                message
                                    .reasoning
                                    .get_or_insert_with(String::new)
                                    .push_str(text);
                                emit(Event::Reasoning(text.into()));
                            }
                        }
                        Some("input_json_delta") => {
                            if let (Some(call), Some(part)) =
                                (calls.get_mut(&index), delta["partial_json"].as_str())
                            {
                                call.function.arguments.push_str(part);
                            }
                            if let Some(latest) = calls.values().next_back() {
                                emit(Event::ToolProgress {
                                    name: latest.function.name.clone(),
                                    calls: calls.len(),
                                    bytes: calls.values().map(|c| c.function.arguments.len()).sum(),
                                });
                            }
                        }
                        _ => {}
                    }
                }
                Some("message_delta") => {
                    if let Some(reason) = value["delta"]["stop_reason"].as_str() {
                        stop = Some(reason.into());
                    }
                }
                Some("message_stop") => return finish(message, calls, stop.as_deref()),
                _ => {}
            }
        }
    }
    if stop.is_some() {
        finish(message, calls, stop.as_deref())
    } else {
        Err(Failure::transient(
            "Connection ended before the generation completed",
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn call(id: &str, args: &str) -> ToolCall {
        ToolCall {
            id: id.into(),
            kind: "function".into(),
            function: Function {
                name: "read".into(),
                arguments: args.into(),
            },
        }
    }

    #[test]
    fn maps_history_tools_and_oauth_identity() {
        let profile = Profile {
            model: "claude-sonnet-4-5".into(),
            ..Default::default()
        };
        let mut assistant = Message::text(Role::Assistant, "");
        assistant.tool_calls = vec![call("call:1", r#"{"path":"a"}"#), call("call_2", "")];
        let messages = vec![
            Message::text(Role::System, "policy"),
            Message::text(Role::User, "hi"),
            assistant,
            Message::tool("call:1", "one".into()),
            Message::tool("call_2", "two".into()),
            Message::text(Role::System, "late note"),
        ];
        let tools = vec![
            json!({"type":"function","function":{"name":"read","description":"d","parameters":{"type":"object"}}}),
        ];
        let (body, _) = body(&profile, &messages, &tools, 100, true, false).unwrap();
        assert_eq!(body["system"][0]["text"], OAUTH_IDENTITY);
        assert_eq!(body["system"][1]["text"], "policy");
        let wire = body["messages"].as_array().unwrap();
        assert_eq!(wire.len(), 3);
        assert_eq!(wire[1]["content"][0]["id"], "call_1");
        assert_eq!(wire[1]["content"][1]["input"], json!({}));
        let results = wire[2]["content"].as_array().unwrap();
        assert_eq!(results[0]["tool_use_id"], "call_1");
        assert_eq!(results[1]["tool_use_id"], "call_2");
        assert_eq!(results[2]["type"], "text");
        assert_eq!(body["tools"][0]["input_schema"], json!({"type":"object"}));
        let (plain, _) = body_for_api_key(&profile, &messages);
        assert_eq!(plain["system"][0]["text"], "policy");
    }

    fn body_for_api_key(profile: &Profile, messages: &[Message]) -> (Value, usize) {
        body(profile, messages, &[], 100, false, false).unwrap()
    }

    #[test]
    fn auth_uses_x_api_key_or_oauth_bearer() {
        let mut headers = HeaderMap::new();
        let profile = Profile {
            api_key: Some(Secret::Literal("sk-ant-api03-test".into())),
            ..Default::default()
        };
        assert!(!authorize(&profile, &mut headers).unwrap());
        assert_eq!(headers["x-api-key"], "sk-ant-api03-test");
        assert_eq!(headers["anthropic-version"], VERSION);
        assert!(!headers.contains_key(AUTHORIZATION));

        let mut headers = HeaderMap::new();
        let profile = Profile {
            api_key: Some(Secret::Literal("sk-ant-oat01-test".into())),
            ..Default::default()
        };
        assert!(authorize(&profile, &mut headers).unwrap());
        assert_eq!(headers[AUTHORIZATION], "Bearer sk-ant-oat01-test");
        assert!(
            headers["anthropic-beta"]
                .to_str()
                .unwrap()
                .contains("oauth-2025-04-20")
        );
        assert!(!headers.contains_key("x-api-key"));
    }

    #[test]
    fn parses_tool_use_and_limits() {
        let value = json!({"stop_reason":"tool_use","content":[
            {"type":"text","text":"ok"},
            {"type":"tool_use","id":"toolu_1","name":"read","input":{"path":"a"}}]});
        let message = parse(&value, &mut |_| {}).unwrap();
        assert_eq!(message.content.as_deref(), Some("ok"));
        assert_eq!(message.tool_calls[0].function.arguments, r#"{"path":"a"}"#);
        let limit = json!({"stop_reason":"max_tokens","content":[{"type":"text","text":"x"}]});
        assert!(parse(&limit, &mut |_| {}).unwrap_err().output_limit);
    }
}
