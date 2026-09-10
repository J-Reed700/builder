//! Active provider conformance probes. These exercise the configured endpoint
//! through the provider contract without touching the workspace or journal.
use builder_core::protocol::{Message, Role};
use builder_provider::Provider;
use serde::Serialize;
use serde_json::{Value, json};

#[derive(Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ProbeStatus {
    Passed,
    Failed { reason: String },
    Skipped { reason: String },
}

#[derive(Debug, Serialize, PartialEq, Eq)]
pub struct ConformanceReport {
    pub normal_generation: ProbeStatus,
    pub json_object: ProbeStatus,
    pub native_tool_call: ProbeStatus,
    pub parallel_tool_calls: ProbeStatus,
}

pub async fn conformance<P: Provider>(provider: &P, tools_enabled: bool) -> ConformanceReport {
    let normal_generation = match provider
        .complete_with_budget(
            &[
                Message::text(
                    Role::System,
                    "Provider capability probe. Return a brief plain-text acknowledgement.",
                ),
                Message::text(Role::User, "Acknowledge this probe."),
            ],
            &[],
            512,
            &mut |_| {},
        )
        .await
    {
        Ok(message)
            if message.tool_calls.is_empty()
                && message
                    .content
                    .as_deref()
                    .is_some_and(|text| !text.trim().is_empty()) =>
        {
            ProbeStatus::Passed
        }
        Ok(_) => ProbeStatus::Failed {
            reason: "response was empty or unexpectedly contained tool calls".into(),
        },
        Err(error) => failed(error),
    };

    let json_object = match provider
        .complete_json(
            &[
                Message::text(
                    Role::System,
                    "Provider capability probe. Return one JSON object with a boolean field named ok.",
                ),
                Message::text(Role::User, "Return the probe object."),
            ],
            512,
            &mut |_| {},
        )
        .await
    {
        Ok(message)
            if message.tool_calls.is_empty()
                && message
                    .content
                    .as_deref()
                    .and_then(|text| serde_json::from_str::<Value>(text).ok())
                    .is_some_and(|value| value.is_object()) =>
        {
            ProbeStatus::Passed
        }
        Ok(_) => ProbeStatus::Failed {
            reason: "response was not one parseable JSON object".into(),
        },
        Err(error) => failed(error),
    };

    let (native_tool_call, parallel_tool_calls) = if tools_enabled {
        (
            tool_probe(provider, &["only"]).await,
            tool_probe(provider, &["first", "second"]).await,
        )
    } else {
        let skipped = || ProbeStatus::Skipped {
            reason: "tools are disabled for this profile".into(),
        };
        (skipped(), skipped())
    };

    ConformanceReport {
        normal_generation,
        json_object,
        native_tool_call,
        parallel_tool_calls,
    }
}

async fn tool_probe<P: Provider>(provider: &P, slots: &[&str]) -> ProbeStatus {
    let nonce = uuid::Uuid::new_v4().to_string();
    let tool = json!({
        "type":"function",
        "function":{
            "name":"builder_capability_probe",
            "description":"Return the required nonce and slot for an endpoint conformance check.",
            "parameters":{
                "type":"object",
                "properties":{
                    "nonce":{"type":"string","enum":[nonce.clone()]},
                    "slot":{"type":"string","enum":slots},
                },
                "required":["nonce","slot"],
                "additionalProperties":false,
            }
        }
    });
    let request = if slots.len() == 1 {
        format!(
            "Call builder_capability_probe exactly once with nonce {nonce} and slot only. Return no prose."
        )
    } else {
        format!(
            "In one response, call builder_capability_probe once for every slot first and second, using nonce {nonce}. Return no prose."
        )
    };
    match provider
        .complete_with_budget(
            &[
                Message::text(Role::System, "Provider native-tool capability probe."),
                Message::text(Role::User, request),
            ],
            &[tool],
            1024,
            &mut |_| {},
        )
        .await
    {
        Ok(message) => validate_tool_calls(&message, &nonce, slots),
        Err(error) => failed(error),
    }
}

fn validate_tool_calls(message: &Message, nonce: &str, slots: &[&str]) -> ProbeStatus {
    if message.tool_calls.len() != slots.len() {
        return ProbeStatus::Failed {
            reason: format!(
                "expected {} tool call(s), received {}",
                slots.len(),
                message.tool_calls.len()
            ),
        };
    }
    let mut returned = Vec::new();
    for call in &message.tool_calls {
        if call.function.name != "builder_capability_probe" {
            return ProbeStatus::Failed {
                reason: "endpoint returned the wrong tool name".into(),
            };
        }
        let Ok(arguments) = serde_json::from_str::<Value>(&call.function.arguments) else {
            return ProbeStatus::Failed {
                reason: "tool arguments were not valid JSON".into(),
            };
        };
        if arguments["nonce"].as_str() != Some(nonce) {
            return ProbeStatus::Failed {
                reason: "tool arguments did not preserve the probe nonce".into(),
            };
        }
        let Some(slot) = arguments["slot"].as_str() else {
            return ProbeStatus::Failed {
                reason: "tool arguments omitted the required slot".into(),
            };
        };
        returned.push(slot.to_owned());
    }
    returned.sort_unstable();
    let mut expected = slots
        .iter()
        .map(|slot| (*slot).to_owned())
        .collect::<Vec<_>>();
    expected.sort_unstable();
    if returned == expected {
        ProbeStatus::Passed
    } else {
        ProbeStatus::Failed {
            reason: "tool calls did not cover the requested slots exactly once".into(),
        }
    }
}

fn failed(error: anyhow::Error) -> ProbeStatus {
    let mut reason = error.to_string().replace(['\r', '\n'], " ");
    if reason.len() > 300 {
        reason.truncate(300);
    }
    ProbeStatus::Failed { reason }
}

#[cfg(test)]
mod tests {
    use super::*;
    use builder_core::protocol::{Function, ToolCall};

    #[test]
    fn tool_probe_requires_typed_names_nonce_and_exact_slot_coverage() {
        let message = Message {
            role: Role::Assistant,
            content: None,
            reasoning: None,
            tool_calls: vec![
                ToolCall {
                    id: "one".into(),
                    kind: "function".into(),
                    function: Function {
                        name: "builder_capability_probe".into(),
                        arguments: json!({"nonce":"n","slot":"second"}).to_string(),
                    },
                },
                ToolCall {
                    id: "two".into(),
                    kind: "function".into(),
                    function: Function {
                        name: "builder_capability_probe".into(),
                        arguments: json!({"nonce":"n","slot":"first"}).to_string(),
                    },
                },
            ],
            tool_call_id: None,
        };
        assert_eq!(
            validate_tool_calls(&message, "n", &["first", "second"]),
            ProbeStatus::Passed
        );
        assert!(matches!(
            validate_tool_calls(&message, "wrong", &["first", "second"]),
            ProbeStatus::Failed { .. }
        ));
    }
}
