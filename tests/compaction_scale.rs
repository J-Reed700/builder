//! Synthetic multi-turn size measurements and opt-in factual recall evaluation.
use anyhow::{Result, ensure};
use builder::agent::{Agent, ApprovalMode, estimate_tokens};
use builder_core::{
    config::{Config, Profile},
    protocol::{Message, Role},
    store::Store,
};
use builder_provider::{Event, OpenAiCompatible, Provider};
use builder_tools::Workspace;
use serde_json::{Value, json};
use std::path::Path;

fn facts(index: usize) -> Value {
    json!({"timeout_ms":1100 + 17 * index,"retry_limit":index % 5 + 1,"audit_tag":format!("audit-{:08x}",index * 7919 + 31)})
}

fn seed(store: &mut Store, session: &str, rounds: usize) -> Result<Value> {
    store.append(session, &Message::text(Role::User, "Keep deployment frozen. When I ask for the final audit, return only a JSON object with services and deployment_frozen fields. Do not invent missing findings."))?;
    for index in 0..rounds {
        store.append(session, &Message::text(Role::User, format!(
            "Investigate synthetic service svc-{index:04}. What timeout does its worker use, how many retries are allowed, and what audit tag identifies the observed configuration? Explain how a transient failure differs from a permanent failure in this service. Check whether retries could duplicate work, whether queue ordering matters, and whether the timeout applies to one attempt or the entire job. Keep this service separate from earlier services; their findings must not be assumed to apply here. This round is informational: do not modify or deploy anything. Record the exact findings so we can compare selected services later."
        )))?;
        store.append(session, &Message::text(Role::Assistant, format!(
            "Synthetic inspection findings for svc-{index:04}: {}. These are the observed fixture values, not recommended replacements. Transient failures may be retried within the configured limit; a permanent validation failure should be reported. Retried jobs need idempotent processing. Queue ordering and the scope of the timeout need independent inspection; neither is established by these three findings. No implementation or deployment was performed. The audit tag identifies this service's observation only. We can compare this record with other services once the user selects which records matter.", facts(index)
        )))?;
    }
    let selected = [1, rounds / 2, rounds - 4];
    let mut services = serde_json::Map::new();
    for index in selected {
        services.insert(format!("svc-{index:04}"), facts(index));
    }
    store.append(session, &Message::text(Role::User, format!(
        "Now produce the final audit for svc-{:04}, svc-{:04}, and svc-{:04}. Use the recorded timeout_ms, retry_limit, and audit_tag for each service. Follow my original deployment constraint and final JSON format. The services field must map each selected service name to exactly those three recorded fields. Do not use tools or repeat investigations.", selected[0], selected[1], selected[2]
    )))?;
    Ok(json!({"services":services,"deployment_frozen":true}))
}

fn measurements(messages: &[Message]) -> Value {
    let user_bytes: usize = messages
        .iter()
        .filter(|m| m.role == Role::User)
        .map(|m| m.content.as_deref().unwrap_or("").len())
        .sum();
    let handoff_bytes: usize = messages
        .iter()
        .filter_map(|m| m.content.as_deref())
        .filter(|s| s.starts_with("[Compacted handoff"))
        .map(str::len)
        .sum();
    json!({"estimated_context_tokens":estimate_tokens(messages),"user_text_bytes":user_bytes,"handoff_bytes":handoff_bytes})
}

struct SmallSummary;
impl Provider for SmallSummary {
    async fn complete(
        &self,
        messages: &[Message],
        tools: &[Value],
        _: &mut dyn FnMut(Event),
    ) -> Result<Message> {
        assert!(tools.is_empty());
        if messages[0]
            .content
            .as_deref()
            .unwrap_or("")
            .starts_with("Extract active user instructions")
        {
            let input: Value = serde_json::from_str(messages[1].content.as_deref().unwrap())?;
            let add = input["sources"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|v| {
                    v["text"]
                        .as_str()
                        .unwrap()
                        .starts_with("Keep deployment frozen.")
                })
                .map(|v| json!({"seq":v["seq"],"quote":v["text"]}))
                .collect::<Vec<_>>();
            return Ok(Message::text(
                Role::Assistant,
                json!({"add":add,"retire":[]}).to_string(),
            ));
        }
        Ok(Message::text(
            Role::Assistant,
            "Synthetic service inspection completed. Consult the original user requirements.",
        ))
    }
}

#[tokio::test]
async fn many_user_turns_keep_active_context_bounded() -> Result<()> {
    // Optimistic size-only oracle, deliberately not an accuracy evaluation.
    for rounds in [16, 64, 256, 512, 1024] {
        let temp = tempfile::tempdir()?;
        let mut store = Store::open(temp.path())?;
        let session = store.create(
            "scale fixture",
            "synthetic",
            temp.path(),
            "Answer the user's audit questions.",
        )?;
        seed(&mut store, &session, rounds)?;
        let before = store.messages(&session)?;
        let agent = Agent {
            provider: SmallSummary,
            memory: None,
            profile: Profile {
                context_tokens: 163840,
                max_output_tokens: 16384,
                ..Profile::default()
            },
            workspace: Workspace::new(temp.path())?,
            session: session.clone(),
            approval: ApprovalMode::ReadOnly,
            max_rounds: 1,
        };
        let result = agent.compact(&mut store, &mut |_| {}).await;
        let after = store.messages(&session)?;
        eprintln!(
            "{}",
            json!({"rounds":rounds,"before":measurements(&before),"after":measurements(&after),"compacted":result.is_ok(),"error":result.as_ref().err().map(|e|format!("{e:#}"))})
        );
        assert_eq!(store.history_messages(&session)?, before);
        assert!(result?);
        assert!(estimate_tokens(&after) < estimate_tokens(&before));
        assert!(
            estimate_tokens(&after) < 6000,
            "active context grows with archived user text"
        );
        assert!(after.iter().any(|m| {
            m.content
                .as_deref()
                .is_some_and(|s| s.contains("Keep deployment frozen."))
        }));
    }
    Ok(())
}

#[tokio::test]
#[ignore = "real endpoint; requires BUILDER_LIVE_CONFIG_HOME"]
async fn live_many_turns_preserve_early_middle_and_recent_findings() -> Result<()> {
    let config = Config::load(Path::new(&std::env::var("BUILDER_LIVE_CONFIG_HOME")?))?;
    let (profile_name, mut profile) =
        config.profile(std::env::var("BUILDER_EVAL_PROFILE").ok().as_deref())?;
    profile.tools = false;
    profile.auto_compact = false;
    profile.pipeline.enabled = false;
    profile.pipeline.code_index = false;
    profile.max_attempts = 1;
    profile.request_timeout_secs = 300;
    profile.idle_timeout_secs = 90;
    let rounds = 128;
    let temp = tempfile::tempdir()?;
    let mut store = Store::open(temp.path())?;
    let session = store.create("many-turn recall", &profile_name, temp.path(), "Answer from recorded synthetic findings. Follow user constraints and report uncertainty honestly.")?;
    let expected = seed(&mut store, &session, rounds)?;
    let before = store.messages(&session)?;
    let agent = Agent {
        provider: OpenAiCompatible::new(profile.clone())?,
        memory: None,
        profile,
        workspace: Workspace::new(temp.path())?,
        session: session.clone(),
        approval: ApprovalMode::ReadOnly,
        max_rounds: 2,
    };
    let started = std::time::Instant::now();
    let compact = agent.compact(&mut store, &mut |_| {}).await;
    let mut failures = Vec::new();
    match compact {
        Ok(true) => {}
        Ok(false) => failures.push("no checkpoint produced".to_owned()),
        Err(error) => failures.push(format!("compaction failed: {error:#}")),
    }
    store = Store::open(temp.path())?;
    let compacted = store.messages(&session)?;
    ensure!(
        store.history_messages(&session)? == before,
        "original history changed"
    );
    ensure!(
        compacted.iter().any(|m| m
            .content
            .as_deref()
            .is_some_and(|s| s.starts_with("[Source-backed user memory v1]"))),
        "missing bounded memory"
    );
    ensure!(
        estimate_tokens(&compacted) < 12000,
        "active context is not bounded"
    );
    eprintln!(
        "{}",
        json!({"rounds":rounds,"before":measurements(&before),"after":measurements(&compacted)})
    );
    if failures.is_empty() {
        if let Err(error) = agent.run(&mut store, &mut |_| {}, &mut |_| false).await {
            failures.push(format!("continuation failed: {error:#}"));
        }
    }
    let active = store.messages(&session)?;
    let answer = active
        .last()
        .and_then(|m| m.content.as_deref())
        .unwrap_or("");
    if serde_json::from_str::<Value>(answer).ok().as_ref() != Some(&expected) {
        failures.push("incorrect facts or JSON output".to_owned());
    }
    let report = json!({"rounds":rounds,"passed":failures.is_empty(),"failures":failures,"before":measurements(&before),"after":measurements(&compacted),"expected":expected,"answer":answer,"memory":compacted.iter().filter_map(|m|m.content.as_deref()).find(|s|s.starts_with("[Source-backed user memory v1]")),"handoff":compacted.iter().filter_map(|m|m.content.as_deref()).find(|s|s.starts_with("[Compacted handoff")),"elapsed_ms":started.elapsed().as_millis()});
    eprintln!("{report}");
    if let Ok(path) = std::env::var("BUILDER_EVAL_REPORT") {
        use std::io::Write;
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)?;
        file.write_all(&serde_json::to_vec_pretty(&report)?)?;
    }
    ensure!(failures.is_empty(), "many-turn compaction failed");
    Ok(())
}
