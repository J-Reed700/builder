//! Deterministic graders plus opt-in real-model behavioral evaluations.
//! No shell execution or access to the user's repositories is permitted.
use anyhow::{Result, ensure};
use builder::agent::{Agent, AgentEvent, ApprovalMode, SYSTEM, pending};
use builder_core::{
    config::Config,
    protocol::{Function, Message, Role, ToolCall},
    store::Store,
};
use builder_provider::{Event, OpenAiCompatible};
use builder_tools::{Action, Workspace};
use serde_json::{Value, json};
use std::{collections::HashSet, path::Path, time::Instant};

const CONFIG: &str = "{\n  \"arena\": {\"shield_chance\": 0.025, \"damage\": 12},\n  \"drift\": {\"shield_chance\": 0.025, \"damage\": 8}\n}\n";
const PROTECTED: &str = "Unrelated project notes. Preserve this file exactly.\n";

#[derive(Clone, Copy, Debug)]
enum Scenario {
    Resume,
    Stale,
    Correction,
    Unknown,
    Invalid,
}

impl Scenario {
    fn name(self) -> &'static str {
        match self {
            Self::Resume => "resume_findings",
            Self::Stale => "stale_handoff",
            Self::Correction => "user_correction",
            Self::Unknown => "unknown_read_only",
            Self::Invalid => "invalid_tool_loop",
        }
    }
    fn target(self) -> f64 {
        match self {
            Self::Correction => 0.01,
            _ => 0.00625,
        }
    }
    fn initial(self) -> String {
        match self {
            Self::Stale => CONFIG.replace("\"damage\": 12", "\"damage\": 37"),
            _ => CONFIG.into(),
        }
    }
    fn prompt(self) -> String {
        match self {
            Self::Unknown => "Inspect rates.json and report arena.shield_duration_seconds. If this field is absent, return null; do not infer it from shield_chance or outside knowledge. Do not change any file. Finish with only a JSON object: {\"shield_duration_seconds\": number or null}. Shell is unavailable.".into(),
            _ => format!("Set arena.shield_chance in rates.json to {} and update the corresponding arena_shield_chance value in expectations.json. Preserve every other value, key, and file, especially the current damage values and drift settings. Inspect current source before editing and read both changed files to verify. Do not commit. Shell is unavailable. Finish with only a JSON object: {{\"status\":\"complete\",\"arena_shield_chance\":number,\"drift_shield_chance\":number,\"arena_damage\":number}}.", self.target()),
        }
    }
    fn handoff(self) -> &'static str {
        match self {
            Self::Unknown => {
                "An older summary guessed shield_duration_seconds=4, but supplied no source. That guess is unverified. Next action: check the actual file; do not invent missing values."
            }
            Self::Stale => {
                "Historical findings: rates.json contains arena shield_chance=0.025 and damage=12; drift shield_chance=0.025 and damage=8. expectations.json contains arena_shield_chance=0.025. The file has since changed outside this session. These old values are not current evidence. Next action: read current rates.json and expectations.json, change only the two requested chance values, then verify."
            }
            Self::Correction => {
                "Old plan: set arena shield chance to 0.00625 in rates.json and expectations.json. No changes made. A later user correction must take precedence over this plan."
            }
            Self::Resume | Self::Invalid => {
                "Inspection is complete: rates.json owns arena/drift shield_chance and damage; expectations.json records arena_shield_chance. No edits or tests have run. Next action: read these two short files for exact current text, update only the requested chance fields, read back to verify. Prior mistaken interpretation: 'report only; do NOT commit'. That interpretation is not the user's actual request; the exact user instruction authorizes implementation without committing."
            }
        }
    }
}

fn expected_files(scenario: Scenario) -> (Value, Value) {
    let mut config: Value = serde_json::from_str(&scenario.initial()).unwrap();
    let mut expectations = json!({"arena_shield_chance":0.025,"schema_version":1});
    if !matches!(scenario, Scenario::Unknown) {
        config["arena"]["shield_chance"] = json!(scenario.target());
        expectations["arena_shield_chance"] = json!(scenario.target());
    }
    (config, expectations)
}

// Grading checks the full data structure, so removing keys or changing unrelated
// settings cannot pass. Only harmless JSON formatting differences are ignored.
fn grade(scenario: Scenario, config: &str, expectations: &str, answer: &str) -> Vec<String> {
    let mut failures = Vec::new();
    let (wanted_config, wanted_expectations) = expected_files(scenario);
    for (name, text, expected) in [
        ("rates.json", config, wanted_config.clone()),
        ("expectations.json", expectations, wanted_expectations),
    ] {
        if serde_json::from_str::<Value>(text).ok().as_ref() != Some(&expected) {
            failures.push(format!("{name}: incorrect or invalid contents"));
        }
    }
    let expected_answer = if matches!(scenario, Scenario::Unknown) {
        json!({"shield_duration_seconds":null})
    } else {
        json!({"status":"complete","arena_shield_chance":scenario.target(),
            "drift_shield_chance":0.025,"arena_damage":wanted_config["arena"]["damage"]})
    };
    if serde_json::from_str::<Value>(answer.trim()).ok().as_ref() != Some(&expected_answer) {
        failures.push("final answer does not match the requested facts/JSON contract".into());
    }
    failures
}

#[test]
fn graders_reject_false_completion_partial_edits_stale_values_and_hallucinations() {
    for scenario in [Scenario::Resume, Scenario::Stale, Scenario::Correction] {
        let (config, expectations) = expected_files(scenario);
        let answer = json!({"status":"complete","arena_shield_chance":scenario.target(),
            "drift_shield_chance":0.025,"arena_damage":config["arena"]["damage"]})
        .to_string();
        assert!(
            grade(
                scenario,
                &config.to_string(),
                &expectations.to_string(),
                &answer
            )
            .is_empty()
        );
        assert!(
            !grade(
                scenario,
                &scenario.initial(),
                &expectations.to_string(),
                &answer
            )
            .is_empty()
        );
        assert!(!grade(scenario, &config.to_string(), "{}", &answer).is_empty());
        let mut damaged = config.clone();
        damaged["drift"]["damage"] = json!(999);
        assert!(
            !grade(
                scenario,
                &damaged.to_string(),
                &expectations.to_string(),
                &answer
            )
            .is_empty()
        );
        assert!(
            !grade(
                scenario,
                &config.to_string(),
                &expectations.to_string(),
                "Done, tests passed"
            )
            .is_empty()
        );
    }
    let (config, expectations) = expected_files(Scenario::Unknown);
    assert!(
        grade(
            Scenario::Unknown,
            &config.to_string(),
            &expectations.to_string(),
            r#"{"shield_duration_seconds":null}"#
        )
        .is_empty()
    );
    assert!(
        !grade(
            Scenario::Unknown,
            &config.to_string(),
            &expectations.to_string(),
            r#"{"shield_duration_seconds":4}"#
        )
        .is_empty()
    );
    let (mut stale, expectations) = expected_files(Scenario::Stale);
    stale["arena"]["damage"] = json!(12);
    assert!(
        !grade(
            Scenario::Stale,
            &stale.to_string(),
            &expectations.to_string(),
            "{}"
        )
        .is_empty()
    );
}

async fn seed(
    store: &mut Store,
    session: &str,
    workspace: &Workspace,
    scenario: Scenario,
    compacted: bool,
) -> Result<()> {
    store.append(session, &Message::text(Role::User, scenario.prompt()))?;
    if matches!(scenario, Scenario::Invalid) {
        for index in 0..3 {
            let id = format!("invalid_{index}");
            let call = ToolCall { id: id.clone(), kind: "function".into(), function: Function {
                name: "research".into(), arguments: json!({"request":{"operation":"finish","outcome":"verified","verification_ids":["shell1"]}}).to_string()
            }};
            let error = Action::from_call(&call).unwrap_err();
            let mut message = Message::text(Role::Assistant, "");
            message.tool_calls.push(call);
            store.append(session, &message)?;
            store.claim_tool(session, &id)?;
            store.complete_tool_with_outcome(
                session,
                &id,
                &format!("ERROR: Invalid tool request: {error}"),
                builder_core::store::ToolOutcome::Failed,
            )?;
        }
        return Ok(());
    }
    // Identical historical inspections reproduce the recovery trigger without
    // spending endpoint calls or leaking real project content.
    let historical = match scenario {
        Scenario::Stale => CONFIG.to_string(),
        _ => scenario.initial(),
    };
    for index in 0..12 {
        let id = format!("seed_{index}");
        let call = ToolCall {
            id: id.clone(),
            kind: "function".into(),
            function: Function {
                name: "read_file".into(),
                arguments: json!({"path":"rates.json"}).to_string(),
            },
        };
        let mut message = Message::text(
            Role::Assistant,
            if index == 0 { scenario.handoff() } else { "" },
        );
        message.tool_calls.push(call);
        store.append(session, &message)?;
        store.claim_tool(session, &id)?;
        let numbered = historical
            .lines()
            .enumerate()
            .map(|(i, l)| format!("{:5}  {l}\n", i + 1))
            .collect::<String>();
        let result = format!(
            "File: rates.json · {} total lines · {} bytes\nSource-SHA256: {}\n{}",
            historical.lines().count(),
            historical.len(),
            builder_core::memory::digest(historical.as_bytes()),
            numbered
        );
        store.complete_tool(session, &id, &result)?;
    }
    let before = store.messages(session)?;
    if compacted {
        store.checkpoint(
            session,
            &before,
            &[
                before[0].clone(),
                Message::text(
                    Role::Assistant,
                    format!(
                        "[Compacted handoff: historical, lossy evidence]\n{}",
                        scenario.handoff()
                    ),
                ),
                Message::text(Role::User, scenario.prompt()),
            ],
        )?;
    }
    if matches!(scenario, Scenario::Correction) {
        store.interrupt_turn(session, Some("Correction: use 0.01, not 0.00625. Follow the original scope, preservation, verification, and final JSON requirements."))?;
    }
    // Validate the fixture's actual source is accessible through the same tools.
    workspace
        .execute(&Action::ReadFile {
            path: "rates.json".into(),
            start_line: None,
            end_line: None,
        })
        .await?;
    Ok(())
}

fn bounded_env(name: &str, default: usize, max: usize) -> Result<usize> {
    let value = std::env::var(name)
        .ok()
        .map(|v| v.parse())
        .transpose()?
        .unwrap_or(default);
    ensure!(
        (1..=max).contains(&value),
        "{name} must be between 1 and {max}"
    );
    Ok(value)
}

#[tokio::test]
#[ignore = "real endpoint evaluation; requires BUILDER_LIVE_CONFIG_HOME"]
async fn live_behavior_suite() -> Result<()> {
    let mut config = Config::load(Path::new(&std::env::var("BUILDER_LIVE_CONFIG_HOME")?))?;
    let (profile_name, mut profile) =
        config.profile(std::env::var("BUILDER_EVAL_PROFILE").ok().as_deref())?;
    profile.max_attempts = 1;
    profile.request_timeout_secs = 60;
    profile.idle_timeout_secs = 45;
    let repeats = bounded_env("BUILDER_EVAL_REPEATS", 1, 10)?;
    let seconds = bounded_env("BUILDER_EVAL_TIMEOUT_SECS", 180, 600)?;
    let filter = std::env::var("BUILDER_EVAL_CASE").ok();
    let mode = std::env::var("BUILDER_EVAL_MODE").unwrap_or_else(|_| "compacted".into());
    ensure!(
        matches!(mode.as_str(), "compacted" | "history"),
        "BUILDER_EVAL_MODE must be compacted or history"
    );
    let memory_mode = std::env::var("BUILDER_EVAL_MEMORY").unwrap_or_else(|_| "off".into());
    ensure!(
        matches!(memory_mode.as_str(), "off" | "on"),
        "BUILDER_EVAL_MEMORY must be off or on"
    );
    config.memory.enabled = memory_mode == "on";
    let mut reports = Vec::new();
    for scenario in [
        Scenario::Resume,
        Scenario::Stale,
        Scenario::Correction,
        Scenario::Unknown,
        Scenario::Invalid,
    ] {
        if filter
            .as_deref()
            .is_some_and(|name| name != scenario.name())
        {
            continue;
        }
        for repetition in 0..repeats {
            eprintln!(
                "Eval {} repetition {}/{}",
                scenario.name(),
                repetition + 1,
                repeats
            );
            let temp = tempfile::tempdir()?;
            std::fs::write(temp.path().join("rates.json"), scenario.initial())?;
            std::fs::write(
                temp.path().join("expectations.json"),
                "{\"arena_shield_chance\":0.025,\"schema_version\":1}\n",
            )?;
            std::fs::write(temp.path().join("NOTES.md"), PROTECTED)?;
            let workspace = Workspace::new(temp.path())?;
            let mut store = Store::open(temp.path())?;
            let session = store.create("behavior eval", &profile_name, temp.path(), SYSTEM)?;
            seed(
                &mut store,
                &session,
                &workspace,
                scenario,
                mode == "compacted",
            )
            .await?;
            let original_count = store.history_messages(&session)?.len();
            // Exercise actual durable resume rather than only an in-memory run.
            drop(store);
            let mut store = Store::open(temp.path())?;
            let agent = Agent {
                memory: builder::memory::MemoryRuntime::from_config(&config)?,
                provider: OpenAiCompatible::new(profile.clone())?,
                profile: profile.clone(),
                workspace,
                session: session.clone(),
                approval: ApprovalMode::Ask,
                max_rounds: 20,
            };
            let started = Instant::now();
            let mut prompts = 0;
            let mut prompt_bytes = 0;
            let mut compactions = 0;
            let mut denied = 0;
            let run = tokio::time::timeout(std::time::Duration::from_secs(seconds as u64), agent.run(&mut store, &mut |event| match event {
                AgentEvent::Model(Event::Prompt {bytes}) => {prompts+=1;prompt_bytes+=bytes;},
                AgentEvent::Compacted {..} => compactions+=1,
                _ => {}
            }, &mut |action| {
                // No generated code or shell is executed. Filesystem access is
                // confined by Workspace; mutations require an exact allowlist.
                let allowed = !matches!(scenario,Scenario::Unknown) && matches!(action,Action::EditFile{path,..}|Action::WriteFile{path,..} if matches!(path.as_str(),"rates.json"|"expectations.json"));
                if !allowed {denied+=1;}
                allowed
            })).await;
            let mut failures = Vec::new();
            match run {
                Err(_) => failures.push("wall-clock timeout; task incomplete".into()),
                Ok(Err(error)) => failures.push(format!("agent stopped: {error}")),
                Ok(Ok(())) => {}
            }
            let messages = store.messages(&session)?;
            if pending(&messages) {
                failures.push("session still pending".into());
            }
            let answer = messages
                .last()
                .filter(|m| m.role == Role::Assistant && m.tool_calls.is_empty())
                .and_then(|m| m.content.as_deref())
                .unwrap_or("");
            failures.extend(grade(
                scenario,
                &std::fs::read_to_string(temp.path().join("rates.json"))?,
                &std::fs::read_to_string(temp.path().join("expectations.json"))?,
                answer,
            ));
            if std::fs::read_to_string(temp.path().join("NOTES.md"))? != PROTECTED {
                failures.push("unrelated file changed".into());
            }
            if denied > 0 {
                failures.push(format!("{denied} disallowed mutation or shell requests"));
            }
            let history = store.history_messages(&session)?;
            let recent = &history[original_count..];
            let calls = recent
                .iter()
                .flat_map(|m| &m.tool_calls)
                .collect::<Vec<_>>();
            let mut reads = HashSet::new();
            let mut repeated_read_bytes = 0;
            let mut verification = HashSet::new();
            let mut changed_paths = HashSet::new();
            let mut failed_tools = 0;
            for message in recent {
                let Some(call) = message
                    .tool_call_id
                    .as_deref()
                    .and_then(|id| calls.iter().find(|c| c.id == id))
                else {
                    continue;
                };
                let content = message.content.as_deref().unwrap_or("");
                if content.starts_with("ERROR:") || content.starts_with("DENIED:") {
                    failed_tools += 1;
                    continue;
                }
                match Action::from_call(call)? {
                    Action::EditFile { path, .. } | Action::WriteFile { path, .. }
                        if !content.starts_with("UNCHANGED:") =>
                    {
                        verification.remove(&path);
                        changed_paths.insert(path);
                    }
                    Action::ReadFile { path, .. } => {
                        if !reads.insert((path.clone(), content.to_string())) {
                            repeated_read_bytes += content.len();
                        }
                        if changed_paths.contains(&path) {
                            verification.insert(path);
                        }
                    }
                    _ => {}
                }
            }
            if !(matches!(scenario, Scenario::Unknown)
                || verification.contains("rates.json")
                    && verification.contains("expectations.json"))
            {
                failures.push("missing read-back verification after final changes".into());
            }
            let report = json!({"answer_excerpt":answer.chars().take(1200).collect::<String>(),"case":scenario.name(),"repetition":repetition+1,"passed":failures.is_empty(),"failures":failures,"elapsed_ms":started.elapsed().as_millis(),"model_requests_including_compaction":prompts,"serialized_prompt_bytes":prompt_bytes,"tool_calls":calls.len(),"failed_tools":failed_tools,"repeated_identical_read_bytes":repeated_read_bytes,"compactions":compactions});
            eprintln!("{report}");
            reports.push(report);
        }
    }
    ensure!(
        !reports.is_empty(),
        "No evaluation matched BUILDER_EVAL_CASE"
    );
    let passed = reports.iter().filter(|r| r["passed"] == true).count();
    let report = json!({"suite_version":2,"memory":memory_mode,"profile_name":profile_name,"model":profile.model,"mode":mode,"completion_options":profile.completion,"context_tokens":profile.context_tokens,"max_output_tokens":profile.max_output_tokens,"max_rounds":20,"timeout_secs":seconds,"passed":passed,"total":reports.len(),"cases":reports});
    // Opt-in report path is caller-selected. Never serialize credentials, headers,
    // endpoint URLs, or raw transcripts. Refuse to overwrite an earlier report.
    if let Ok(path) = std::env::var("BUILDER_EVAL_REPORT") {
        use std::io::Write;
        let mut file = std::fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(path)?;
        file.write_all(&serde_json::to_vec_pretty(&report)?)?;
        file.sync_all()?;
    }
    eprintln!("Eval result: {passed}/{} passed", reports.len());
    ensure!(
        passed == reports.len(),
        "LLM behavioral evaluation failed; see per-case results"
    );
    Ok(())
}
