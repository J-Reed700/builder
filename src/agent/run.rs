//! Model-round orchestration. Persist only complete, validated responses.
use super::guard::{
    ToolGuardViolation, blocked_outcome_name, failed_tool_streak, invalid_tool_call_id,
    no_progress_streak, structurally_valid_tool_call, tool_guard_state, tool_guard_violation,
    tool_phase,
};
use super::progress::{
    Nudge, ReadLog, planning_reminder, progress_guidance, progress_nudge, todo_continuation,
    todo_packet,
};
use super::{Agent, AgentEvent, ApprovalMode, pending, request::RequestContext, subagent_slots};
use anyhow::{Result, bail, ensure};
use builder_core::{
    protocol::{Message, Role},
    store::{AttemptOutcome, Store},
};
use builder_provider::{Event, OutputLimit, Provider};
use builder_tools::Action;

impl<P: Provider> Agent<P> {
    pub async fn run(
        &self,
        store: &mut Store,
        emit: &mut dyn FnMut(AgentEvent),
        approve: &mut dyn FnMut(&Action) -> bool,
    ) -> Result<()> {
        if !pending(&store.messages(&self.session)?) {
            return Ok(());
        }
        if let Some(memory) = &self.memory {
            memory.reset_network();
        }
        let capacity = if self.profile.tools && self.profile.pipeline.subagents {
            self.provider.parallel_capacity().await
        } else {
            None
        };
        let parallel_notice = (self.profile.tools && self.profile.pipeline.subagents).then(|| {
            let slots = subagent_slots(&self.profile, capacity);
            let source = capacity.map_or_else(
                || "The server does not report its parallel capacity".to_owned(),
                |n| format!("The current model server reports {n} parallel inference slots"),
            );
            Message::text(Role::System, format!(
                "Subagent capacity: {source}. Builder can run up to {slots} research subagents concurrently with the current settings. The main agent waits while they run, so it does not consume an additional inference slot. Delegate independent, substantial investigations together in one tool response to gather information faster and preserve your context; only their reports return. Give each a bounded question and required evidence, then use the reports to continue the task. Avoid delegation for trivial reads or dependent work. Slots are shared with other server clients and are not guaranteed to be idle."
            ))
        });
        let mut output_budget = self.profile.max_output_tokens;
        let mut recovered_output = false;
        let mut exploration_rounds = 0;
        let mut verification_recoveries = 0;
        let mut failure_recoveries = 0;
        let mut conclusion_rejections = 0;
        let mut nudged = 0;
        for round in 0..self.max_rounds {
            self.recover_tools(store, emit, approve).await?;
            if !pending(&store.messages(&self.session)?) {
                return Ok(());
            }
            let messages = store.messages(&self.session)?;
            if !pending(&messages) {
                return Ok(());
            }
            // Use original, unrewound history: compaction and process restarts
            // must not make a long investigation look like a fresh task.
            let history = store.history_messages(&self.session)?;
            let outcomes = store.tool_outcomes(&self.session)?;
            let phase = tool_phase(&history, &outcomes);
            let no_progress_calls = no_progress_streak(&history, &outcomes);
            let progress_check_calls = self.profile.pipeline.progress_check_calls;
            let progress_recovery_rounds = self.profile.pipeline.progress_recovery_rounds;
            let max_no_progress_calls = self.profile.pipeline.max_no_progress_calls();
            let identical_shell_calls = self.profile.pipeline.identical_shell_calls;
            let tool_guards = tool_guard_state(&history, &outcomes);
            let repeated = tool_guards
                .repetitions
                .values()
                .filter(|repetition| repetition.count >= identical_shell_calls)
                .max_by_key(|repetition| repetition.count);
            let failed_calls = failed_tool_streak(&history, &outcomes);
            let todos = (self.profile.tools && self.profile.pipeline.todos)
                .then(|| builder_core::todo::List::latest(&history, &outcomes))
                .flatten()
                .filter(|list| !list.is_finished());
            let reads = ReadLog::new(&history, &outcomes, &messages);
            let can_edit = self.approval != ApprovalMode::ReadOnly;
            let mut force_conclusion = false;
            let mut guidance = if no_progress_calls >= max_no_progress_calls {
                force_conclusion = true;
                exploration_rounds = progress_recovery_rounds;
                Some(progress_guidance(
                    no_progress_calls,
                    true,
                    conclusion_rejections > 0,
                ))
            } else if failed_calls >= self.profile.pipeline.failure_check_calls
                && exploration_rounds == 0
            {
                ensure!(
                    failure_recoveries < self.profile.pipeline.failure_recovery_rounds,
                    "Tool recovery exhausted after repeated failures. Session remains pending; inspect the recorded errors before /retry."
                );
                if failure_recoveries == 0 {
                    emit(AgentEvent::ExplorationRecovery {
                        calls: failed_calls,
                    });
                }
                failure_recoveries += 1;
                Some(Message::text(
                    Role::System,
                    "Runtime tool failure recovery: repeated calls have failed. Read the recorded error and the operation-specific schema before trying again. For a failed exact-text edit, use current source from a targeted read to rebuild a minimal replacement; historical snippets are not current source. Preserve unrelated current values. A successful read does not erase earlier edit failures. Do not repeat identical invalid arguments. For research finish, supply only operation, outcome and explanation; verification_ids belong to review/learn. If no research plan is active, report completed work and actual results directly. A tool error is not successful evidence. If unable to correct the request, report unfinished work and the concrete blocker. Denials and uncertain outcomes remain binding.",
                ))
            } else if no_progress_calls >= progress_check_calls {
                // Tell the user once per multiple of the check, including
                // after /retry, so a stalling run stays visible.
                let bucket = no_progress_calls / progress_check_calls;
                if bucket > nudged {
                    nudged = bucket;
                    emit(AgentEvent::ProgressNudge {
                        calls: no_progress_calls,
                        step: todos
                            .as_ref()
                            .and_then(|list| list.current().map(|(n, _)| (n, list.items.len()))),
                        repeated_reads: reads.repeats(),
                        planning: todos.is_none()
                            && self.profile.tools
                            && self.profile.pipeline.todos
                            && can_edit,
                    });
                }
                force_conclusion = no_progress_calls >= max_no_progress_calls
                    || exploration_rounds + 1 >= progress_recovery_rounds;
                exploration_rounds = exploration_rounds
                    .saturating_add(1)
                    .min(progress_recovery_rounds);
                Some(progress_guidance(
                    no_progress_calls,
                    force_conclusion,
                    conclusion_rejections > 0,
                ))
            } else {
                exploration_rounds = 0;
                None
            };
            if let Some(repeated) = repeated {
                emit(AgentEvent::RepetitionNotice {
                    name: repeated.name.clone(),
                    count: repeated.count,
                    limit: identical_shell_calls,
                });
                let warning = format!(
                    "Runtime repetition check: the identical {} request already completed {} times since the latest user instruction or successful file edit. Do not request it again. Use its recorded result, choose a materially different action, or answer the user. A further identical request will be rejected before execution.",
                    repeated.name, repeated.count
                );
                guidance = Some(match guidance {
                    Some(mut message) => {
                        let content = message.content.get_or_insert_with(String::new);
                        content.push_str("\n\n");
                        content.push_str(&warning);
                        message
                    }
                    None => Message::text(Role::System, warning),
                });
            }
            // The liveness boundary must be mechanical. A text instruction to
            // stop using tools is not enough for a model that keeps calling
            // them, and pausing first makes /retry repeat the same failure.
            let restrict_discovery = !force_conclusion && no_progress_calls >= progress_check_calls;
            let definitions = if force_conclusion {
                vec![]
            } else if self.profile.tools {
                let mut tool_pipeline = self.profile.pipeline.clone();
                if self.memory.is_none() {
                    tool_pipeline.procedures = false;
                }
                let mut tools = builder_tools::definitions_with_pipeline_and_phase(
                    &tool_pipeline,
                    tool_pipeline.phase_routing.then_some(&phase),
                );
                if self.memory.is_some() {
                    tools.extend(crate::memory::definitions());
                }
                if restrict_discovery {
                    tools.retain(|definition| {
                        !matches!(
                            definition["function"]["name"].as_str(),
                            Some("list_files" | "search" | builder_tools::SUBAGENT_TOOL)
                        )
                    });
                }
                tools
            } else {
                vec![]
            };
            let planning = self.profile.tools && self.profile.pipeline.todos && can_edit;
            // Everything that changes from round to round goes last, so an
            // endpoint's prompt cache survives a long recovery.
            let continuation = if guidance.is_some() {
                let mut text = if force_conclusion {
                    "[Builder runtime continuation, not a new user request] Conclude the original task now using only the recorded evidence. Tools are unavailable in this request. Honor the user's requested final-answer format, including JSON-only when requested: do not output tool-call tags, function syntax, XML control markup, or a proposed tool request. State honestly what was completed, what remains unresolved, and any concrete blocker.".to_owned()
                } else if restrict_discovery {
                    progress_nudge(&Nudge {
                        calls: no_progress_calls,
                        check: progress_check_calls,
                        todos: todos.as_ref(),
                        planning,
                        can_edit,
                        reads: &reads,
                    })
                } else {
                    "[Builder runtime continuation, not a new user request] Continue the unfinished task from the handoff and existing results. The original user instruction above is preserved verbatim; it is not a request to start the investigation again. Follow its actual constraints, not contradictory interpretations in a lossy summary. Take the single next action identified in the handoff. If exact source is needed, read only that location, then apply the justified change. All configured tools remain available, including reads and verification. Never make placeholder or unrelated edits merely to demonstrate progress. Preserve the original user's requested final-answer format. If you cannot proceed, explain the specific blocker within that format.".to_owned()
                };
                if let Some(list) = &todos
                    && (force_conclusion || !restrict_discovery)
                {
                    text.push_str(&todo_continuation(list, force_conclusion));
                }
                Some(Message::text(Role::User, text))
            } else if planning
                && todos.is_none()
                && no_progress_calls >= (progress_check_calls / 2).max(1)
            {
                Some(Message::text(
                    Role::User,
                    planning_reminder(no_progress_calls, progress_check_calls, &reads),
                ))
            } else {
                None
            };
            let todo_packet = todos
                .as_ref()
                .map(|list| todo_packet(list, force_conclusion));
            let remaining = self.max_rounds - round;
            let (messages, estimate) = self
                .prepare_request(
                    store,
                    RequestContext {
                        messages,
                        definitions: &definitions,
                        parallel_notice: parallel_notice.as_ref(),
                        remaining,
                        phase: &phase,
                        guidance,
                        continuation,
                        todo_packet,
                    },
                    output_budget,
                    emit,
                )
                .await?;
            let attempt = store.begin_attempt(&self.session)?;
            // The final recovery response is buffered until validation. Otherwise
            // a model that prints tool-control syntax as text can leak it into the
            // terminal even though the response is subsequently rejected.
            let result = self
                .provider
                .complete_with_budget(&messages, &definitions, output_budget, &mut |event| {
                    if force_conclusion {
                        match event {
                            Event::Delta(_) | Event::Reasoning(_) | Event::ToolProgress { .. } => {}
                            event => emit(AgentEvent::Model(event)),
                        }
                    } else {
                        emit(AgentEvent::Model(event));
                    }
                })
                .await;
            match result {
                Ok(message) => {
                    if force_conclusion
                        && (!message.tool_calls.is_empty()
                            || message
                                .content
                                .as_deref()
                                .is_some_and(looks_like_tool_control_markup))
                    {
                        store.finish_attempt(
                            attempt,
                            AttemptOutcome::Failed,
                            "Enforced conclusion returned tool-control syntax",
                        )?;
                        conclusion_rejections += 1;
                        if conclusion_rejections == 1 && remaining > 1 {
                            emit(AgentEvent::MemoryNotice(
                                "The model returned a tool request during the tool-free conclusion; it was not shown or executed. Retrying once for plain prose."
                                    .into(),
                            ));
                            continue;
                        }
                        let fallback = Message::text(
                            Role::Assistant,
                            "Builder stopped this response because the model kept requesting tools after the no-progress safety boundary. No calls from the rejected response were executed. I could not verify that the requested task was completed.",
                        );
                        if let Some(content) = fallback.content.clone() {
                            emit(AgentEvent::Model(Event::Delta(content)));
                        }
                        store.append(&self.session, &fallback)?;
                        return Ok(());
                    }
                    if message.tool_calls.len() > self.profile.pipeline.tool_calls_per_response {
                        store.finish_attempt(
                            attempt,
                            AttemptOutcome::Failed,
                            "Model returned an oversized tool batch",
                        )?;
                        bail!(
                            "Model returned {} tool calls in one response; the configured limit is {}. The entire batch was rejected before execution. Session remains pending; send a follow-up or /retry.",
                            message.tool_calls.len(),
                            self.profile.pipeline.tool_calls_per_response
                        );
                    }
                    if message
                        .tool_calls
                        .iter()
                        .any(|call| !structurally_valid_tool_call(call))
                    {
                        store.finish_attempt(
                            attempt,
                            AttemptOutcome::Failed,
                            "Model returned a malformed tool call",
                        )?;
                        bail!(
                            "Model returned a malformed tool call. The entire batch was rejected before persistence or execution. Session remains pending; send a follow-up or /retry."
                        );
                    }
                    if message.tool_calls.iter().any(|call| {
                        !definitions.iter().any(|d| {
                            d["function"]["name"].as_str() == Some(call.function.name.as_str())
                        })
                    }) {
                        store.finish_attempt(
                            attempt,
                            AttemptOutcome::Failed,
                            "Model requested an unavailable tool",
                        )?;
                        bail!(
                            "Model requested a tool unavailable for this step. No calls from that response were executed. Session is paused; send a follow-up or /retry."
                        );
                    }
                    if let Some(id) = invalid_tool_call_id(&message.tool_calls) {
                        store.finish_attempt(
                            attempt,
                            AttemptOutcome::Failed,
                            "Model used an empty or duplicate tool call ID within one response",
                        )?;
                        bail!(
                            "Model used an empty or duplicate tool call ID {id:?} within one response. The entire batch was rejected before persistence or execution. Session remains pending; send a follow-up or /retry."
                        );
                    }
                    // Never accept reuse of a call ID from an earlier generation.
                    let old_ids = store.used_call_ids(&self.session)?;
                    if message.tool_calls.iter().any(|c| old_ids.contains(&c.id)) {
                        store.finish_attempt(
                            attempt,
                            AttemptOutcome::Failed,
                            "Reused tool call ID",
                        )?;
                        bail!("Model reused a previous tool call ID; refusing ambiguous execution");
                    }
                    if !message.tool_calls.is_empty() && no_progress_calls >= max_no_progress_calls
                    {
                        store.finish_attempt(
                            attempt,
                            AttemptOutcome::Failed,
                            "Durable no-progress tool execution limit reached",
                        )?;
                        bail!(
                            "Stopped tool execution after {no_progress_calls} calls without new inspection evidence or a successful file edit since the latest user instruction. The configured limit is {max_no_progress_calls}; the entire response was rejected before execution. Existing evidence is preserved. Retry only to let the model answer without tools, or send a new instruction to continue a narrowed task."
                        );
                    }
                    if let Some(violation) = tool_guard_violation(
                        message.tool_calls.iter(),
                        &tool_guards,
                        identical_shell_calls,
                    ) {
                        match violation {
                            ToolGuardViolation::Blocked { call, outcome } => {
                                store.finish_attempt(
                                    attempt,
                                    AttemptOutcome::Failed,
                                    "Model tried to replay a denied or uncertain tool request",
                                )?;
                                bail!(
                                    "Refused to replay {} call {} because an identical execute or mutation request has a {} outcome since the latest user instruction. The entire response was rejected before execution. Inspect recorded side effects and send a new explicit instruction before requesting that action again.",
                                    call.function.name,
                                    call.id,
                                    blocked_outcome_name(outcome)
                                );
                            }
                            ToolGuardViolation::Repeated { call, prior_count } => {
                                store.finish_attempt(
                                    attempt,
                                    AttemptOutcome::Failed,
                                    "Model repeated an identical completed tool request",
                                )?;
                                bail!(
                                    "Stopped a repeated {} loop: call {} would exceed the configured limit of {} identical requests since the latest user instruction or successful file edit ({prior_count} already completed or present earlier in this response). The entire response was rejected before execution. Session remains pending; give a new instruction, or use /retry after changing the model/settings.",
                                    call.function.name,
                                    call.id,
                                    identical_shell_calls
                                );
                            }
                        }
                    }
                    if message.tool_calls.is_empty()
                        && self.profile.tools
                        && self.profile.pipeline.enabled
                        && self.profile.pipeline.planning
                        && self.profile.pipeline.completion_gate
                    {
                        let state = crate::research::status_with_settings(
                            store,
                            &self.session,
                            &self.workspace,
                            &self.profile.pipeline,
                        )?;
                        if state["workflow_active"] == true && state["finish_valid"] != true {
                            // Rejected final proposals remain in the attempt audit. They
                            // are not accepted conversation completions or new user turns.
                            store.finish_attempt(
                                attempt,
                                AttemptOutcome::Failed,
                                &format!(
                                    "Verification gate rejected final proposal: {}",
                                    serde_json::to_string(&message)?
                                ),
                            )?;
                            verification_recoveries += 1;
                            emit(AgentEvent::MemoryNotice("Completion needs a research finish record: verify and review the criteria, or explicitly finish unverified/blocked.".into()));
                            ensure!(
                                verification_recoveries <= self.profile.pipeline.completion_retries,
                                "Completion remains unverified after {} corrective rounds. Session is pending; inspect research status and finish with an honest outcome.",
                                self.profile.pipeline.completion_retries
                            );
                            continue;
                        }
                    }
                    if force_conclusion && let Some(content) = message.content.clone() {
                        emit(AgentEvent::Model(Event::Delta(content)));
                    }
                    store.append(&self.session, &message)?;
                    store.finish_attempt(attempt, AttemptOutcome::Complete, "")?;
                    if message.tool_calls.is_empty() {
                        // Final answers return immediately. Optional extraction/indexing
                        // resumes on a later pending turn, never after completion.
                        return Ok(());
                    }
                }
                Err(error) => {
                    store.finish_attempt(attempt, AttemptOutcome::Failed, &error.to_string())?;
                    // One bounded retry with more generation room. Partial
                    // text and tool calls from the failed attempt stay discarded.
                    let larger = output_budget
                        .saturating_mul(2)
                        .min(32768)
                        .min(self.profile.context_tokens.saturating_sub(estimate + 1024));
                    if error.is::<OutputLimit>() && !recovered_output && larger > output_budget {
                        recovered_output = true;
                        output_budget = larger;
                        emit(AgentEvent::OutputRecovery { budget: larger });
                        continue;
                    }
                    return Err(error);
                }
            }
        }
        bail!(
            "Reached the configured budget of {} model rounds for this run. Work remains pending and saved. Adjust Agent rounds per run in /settings, then /retry to continue.",
            self.max_rounds
        )
    }
}
fn looks_like_tool_control_markup(content: &str) -> bool {
    let content = content.trim_start();
    (content.starts_with("<tool_call>") || content.starts_with("<function="))
        && content.contains("<function=")
}
