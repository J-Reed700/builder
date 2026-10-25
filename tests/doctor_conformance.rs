//! `builder doctor` probes an endpoint through the provider contract. These
//! script every answer an endpoint can give and check how each is judged.
use builder::doctor::{ConformanceReport, ProbeStatus, conformance};
use builder_core::protocol::{Function, Message, Role, ToolCall};
use builder_provider::{Event, Provider};
use serde_json::{Value, json};
use std::sync::Mutex;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Script {
    Faithful,
    EmptyText,
    ToolCallInProse,
    ProseJson,
    ArrayJson,
    WrongName,
    BadArguments,
    WrongNonce,
    MissingSlot,
    OneCallOnly,
    DuplicateSlot,
    Failing,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Probe {
    Text,
    Json,
    Tools { nonce: String, slots: Vec<String> },
}

struct Scripted {
    script: Script,
    seen: Mutex<Vec<Probe>>,
}

impl Scripted {
    fn new(script: Script) -> Self {
        Self {
            script,
            seen: Mutex::new(Vec::new()),
        }
    }
    fn probes(&self) -> Vec<Probe> {
        self.seen.lock().unwrap().clone()
    }
}

fn probe_call(id: &str, arguments: Value) -> ToolCall {
    ToolCall {
        id: id.into(),
        kind: "function".into(),
        function: Function {
            name: "builder_capability_probe".into(),
            arguments: arguments.to_string(),
        },
    }
}

fn assistant(content: Option<&str>, tool_calls: Vec<ToolCall>) -> Message {
    Message {
        role: Role::Assistant,
        content: content.map(Into::into),
        reasoning: None,
        tool_calls,
        tool_call_id: None,
    }
}

impl Provider for Scripted {
    async fn complete(
        &self,
        _messages: &[Message],
        tools: &[Value],
        _emit: &mut dyn FnMut(Event),
    ) -> anyhow::Result<Message> {
        if self.script == Script::Failing {
            anyhow::bail!("endpoint exploded\r\nline two {}", "x".repeat(400));
        }
        if tools.is_empty() {
            self.seen.lock().unwrap().push(Probe::Text);
            return Ok(match self.script {
                Script::EmptyText => assistant(Some("   "), vec![]),
                Script::ToolCallInProse => {
                    assistant(Some("ok"), vec![probe_call("stray", json!({}))])
                }
                _ => assistant(Some("Acknowledged."), vec![]),
            });
        }
        let parameters = &tools[0]["function"]["parameters"]["properties"];
        let nonce = parameters["nonce"]["enum"][0].as_str().unwrap().to_owned();
        let slots = parameters["slot"]["enum"]
            .as_array()
            .unwrap()
            .iter()
            .map(|slot| slot.as_str().unwrap().to_owned())
            .collect::<Vec<_>>();
        self.seen.lock().unwrap().push(Probe::Tools {
            nonce: nonce.clone(),
            slots: slots.clone(),
        });
        let faithful = |index: usize, slot: &str| {
            probe_call(
                &format!("call-{index}"),
                json!({"nonce": nonce, "slot": slot}),
            )
        };
        let each = |make: &dyn Fn(usize, &str) -> ToolCall| {
            slots
                .iter()
                .enumerate()
                .map(|(index, slot)| make(index, slot))
                .collect::<Vec<_>>()
        };
        Ok(match self.script {
            Script::WrongName => {
                let mut calls = each(&faithful);
                calls[0].function.name = "other_tool".into();
                assistant(None, calls)
            }
            Script::BadArguments => {
                let mut calls = each(&faithful);
                calls[0].function.arguments = "{nonce".into();
                assistant(None, calls)
            }
            Script::WrongNonce => assistant(
                None,
                each(&|index, slot| {
                    probe_call(
                        &format!("call-{index}"),
                        json!({"nonce":"stale","slot":slot}),
                    )
                }),
            ),
            Script::MissingSlot => assistant(
                None,
                each(&|index, _| probe_call(&format!("call-{index}"), json!({"nonce": nonce}))),
            ),
            Script::OneCallOnly => assistant(None, vec![faithful(0, &slots[0])]),
            Script::DuplicateSlot => assistant(None, each(&|index, _| faithful(index, &slots[0]))),
            _ => assistant(None, each(&faithful)),
        })
    }

    async fn complete_json(
        &self,
        _messages: &[Message],
        _max_output_tokens: usize,
        _emit: &mut dyn FnMut(Event),
    ) -> anyhow::Result<Message> {
        if self.script == Script::Failing {
            anyhow::bail!("json endpoint exploded");
        }
        self.seen.lock().unwrap().push(Probe::Json);
        Ok(match self.script {
            Script::ProseJson => assistant(Some("sure: {\"ok\":true}"), vec![]),
            Script::ArrayJson => assistant(Some("[true]"), vec![]),
            _ => assistant(Some(r#"{"ok":true}"#), vec![]),
        })
    }
}

fn failed(reason: &str) -> ProbeStatus {
    ProbeStatus::Failed {
        reason: reason.into(),
    }
}

#[tokio::test]
async fn a_faithful_endpoint_passes_every_probe_and_each_tool_probe_has_its_own_nonce() {
    let provider = Scripted::new(Script::Faithful);
    let report = conformance(&provider, true).await;
    assert_eq!(
        report,
        ConformanceReport {
            normal_generation: ProbeStatus::Passed,
            json_object: ProbeStatus::Passed,
            native_tool_call: ProbeStatus::Passed,
            parallel_tool_calls: ProbeStatus::Passed,
        }
    );
    assert!(report.required_failures(true, 8).is_empty());
    let probes = provider.probes();
    assert_eq!(probes.len(), 4);
    assert_eq!(probes[0], Probe::Text);
    assert_eq!(probes[1], Probe::Json);
    let (
        Probe::Tools {
            nonce: single,
            slots: one,
        },
        Probe::Tools {
            nonce: parallel,
            slots: two,
        },
    ) = (&probes[2], &probes[3])
    else {
        panic!("tool probes follow the text probes: {probes:?}");
    };
    assert_eq!(one, &["only"]);
    assert_eq!(two, &["first", "second"]);
    assert_ne!(single, parallel, "a nonce is never reused across probes");
    assert!(uuid::Uuid::parse_str(single).is_ok());
}

#[tokio::test]
async fn disabled_tools_skip_the_tool_probes_without_sending_them() {
    let provider = Scripted::new(Script::WrongName);
    let report = conformance(&provider, false).await;
    let skipped = ProbeStatus::Skipped {
        reason: "tools are disabled for this profile".into(),
    };
    assert_eq!(report.normal_generation, ProbeStatus::Passed);
    assert_eq!(report.json_object, ProbeStatus::Passed);
    assert_eq!(report.native_tool_call, skipped);
    assert_eq!(report.parallel_tool_calls, skipped);
    assert_eq!(provider.probes(), [Probe::Text, Probe::Json]);
    assert!(report.required_failures(false, 8).is_empty());
}

#[tokio::test]
async fn empty_or_tool_bearing_text_and_non_object_json_fail_their_probes() {
    let empty = "response was empty or unexpectedly contained tool calls";
    for script in [Script::EmptyText, Script::ToolCallInProse] {
        let report = conformance(&Scripted::new(script), true).await;
        assert_eq!(report.normal_generation, failed(empty), "{script:?}");
        assert_eq!(report.json_object, ProbeStatus::Passed, "{script:?}");
        assert_eq!(report.required_failures(false, 1), ["normal_generation"]);
    }
    let not_object = "response was not one parseable JSON object";
    for script in [Script::ProseJson, Script::ArrayJson] {
        let report = conformance(&Scripted::new(script), true).await;
        assert_eq!(report.normal_generation, ProbeStatus::Passed, "{script:?}");
        assert_eq!(report.json_object, failed(not_object), "{script:?}");
        assert_eq!(report.native_tool_call, ProbeStatus::Passed, "{script:?}");
        assert_eq!(report.required_failures(false, 1), ["json_object"]);
    }
}

#[tokio::test]
async fn transport_errors_become_single_line_bounded_reasons() {
    let report = conformance(&Scripted::new(Script::Failing), true).await;
    let ProbeStatus::Failed { reason } = &report.normal_generation else {
        panic!("{report:?}");
    };
    assert_eq!(reason.len(), 300, "long errors are truncated");
    assert!(!reason.contains(['\n', '\r']), "{reason:?}");
    assert!(
        reason.starts_with("endpoint exploded  line two x"),
        "{reason:?}"
    );
    assert_eq!(report.json_object, failed("json endpoint exploded"));
    assert!(matches!(
        report.native_tool_call,
        ProbeStatus::Failed { .. }
    ));
    assert!(matches!(
        report.parallel_tool_calls,
        ProbeStatus::Failed { .. }
    ));
    assert_eq!(
        report.required_failures(true, 8),
        [
            "normal_generation",
            "json_object",
            "native_tool_call",
            "parallel_tool_calls"
        ]
    );
}

#[tokio::test]
async fn tool_probes_require_the_exact_name_nonce_and_slot_coverage() {
    for (script, reason) in [
        (Script::WrongName, "endpoint returned the wrong tool name"),
        (Script::BadArguments, "tool arguments were not valid JSON"),
        (
            Script::WrongNonce,
            "tool arguments did not preserve the probe nonce",
        ),
        (
            Script::MissingSlot,
            "tool arguments omitted the required slot",
        ),
    ] {
        let report = conformance(&Scripted::new(script), true).await;
        assert_eq!(report.native_tool_call, failed(reason), "{script:?}");
        assert_eq!(report.parallel_tool_calls, failed(reason), "{script:?}");
        assert_eq!(report.required_failures(true, 1), ["native_tool_call"]);
        assert_eq!(
            report.required_failures(true, 2),
            ["native_tool_call", "parallel_tool_calls"]
        );
    }

    let report = conformance(&Scripted::new(Script::OneCallOnly), true).await;
    assert_eq!(report.native_tool_call, ProbeStatus::Passed);
    assert_eq!(
        report.parallel_tool_calls,
        failed("expected 2 tool call(s), received 1")
    );
    assert!(report.required_failures(true, 1).is_empty());
    assert_eq!(report.required_failures(true, 2), ["parallel_tool_calls"]);

    let report = conformance(&Scripted::new(Script::DuplicateSlot), true).await;
    assert_eq!(report.native_tool_call, ProbeStatus::Passed);
    assert_eq!(
        report.parallel_tool_calls,
        failed("tool calls did not cover the requested slots exactly once")
    );
}
