use super::{
    Agent, AgentEvent, ApprovalMode,
    guard::{
        ToolGuardViolation, blocked_outcome_name, invalid_tool_call_id, no_progress_streak,
        structurally_valid_tool_call, tool_guard_state, tool_guard_violation,
    },
    subagent_slots,
};
use anyhow::{Result, bail, ensure};
use builder_core::{
    protocol::{Role, ToolCall},
    store::{Store, ToolOutcome, ToolRunState},
};
use builder_provider::Provider;
use builder_tools::{Action, Risk};

impl<P: Provider> Agent<P> {
    pub(super) async fn recover_tools(
        &self,
        store: &mut Store,
        emit: &mut dyn FnMut(AgentEvent),
        approve: &mut dyn FnMut(&Action) -> bool,
    ) -> Result<()> {
        // An interrupted side-effect-free call (an inspection or a subagent)
        // changed nothing, so it closes as a retryable failure, never as an
        // uncertain outcome that would pause the session.
        if let Some(assistant) = store
            .messages(&self.session)?
            .into_iter()
            .rfind(|message| message.role == Role::Assistant)
        {
            for call in &assistant.tool_calls {
                store.close_interrupted_side_effect_free_tool(&self.session, &call.id)?;
            }
        }
        let messages = store.messages(&self.session)?;
        let Some(assistant_index) = messages
            .iter()
            .rposition(|message| message.role == Role::Assistant)
        else {
            return Ok(());
        };
        let assistant = &messages[assistant_index];
        // A durable started claim outranks every saved-message validation error:
        // execution may already have happened, so record and surface uncertainty
        // before malformed or ambiguous legacy data can disguise it.
        for call in &assistant.tool_calls {
            if store.tool_run_state(&self.session, &call.id)? == ToolRunState::Started {
                let result = store.tool_result(&self.session, &call.id)?.unwrap_or_else(|| "ERROR: Execution uncertain after interruption. This tool will NOT be rerun automatically. Ask the user to inspect any side effects before issuing further mutations.".into());
                store.complete_tool_with_outcome(
                    &self.session,
                    &call.id,
                    &result,
                    ToolOutcome::Uncertain,
                )?;
                bail!(
                    "An interrupted tool has an uncertain outcome. Its status is saved. Inspect the workspace before /retry."
                );
            }
        }
        if let Some(id) = invalid_tool_call_id(&assistant.tool_calls) {
            bail!(
                "Saved assistant response contains an empty or duplicate tool call ID {id:?}. No unclaimed call was executed. Session remains pending; send a new instruction or /cancel to discard the ambiguous batch."
            );
        }
        if assistant
            .tool_calls
            .iter()
            .any(|call| !structurally_valid_tool_call(call))
        {
            bail!(
                "Saved assistant response contains a malformed tool call. No unclaimed call was executed. Session remains pending; send a new instruction or /cancel to discard the ambiguous batch."
            );
        }
        let reused_ids = store.reused_call_ids(&self.session)?;
        if let Some(call) = assistant
            .tool_calls
            .iter()
            .find(|call| reused_ids.contains(&call.id))
        {
            bail!(
                "Saved assistant response reuses earlier tool call ID {:?}. No result was borrowed and no unclaimed call was executed. Session remains pending; send a new instruction or /cancel to discard the ambiguous batch.",
                call.id
            );
        }
        let completed: std::collections::HashSet<_> = messages[assistant_index + 1..]
            .iter()
            .filter_map(|message| message.tool_call_id.as_deref())
            .collect();
        let unresolved = assistant
            .tool_calls
            .iter()
            .filter(|call| !completed.contains(call.id.as_str()))
            .collect::<Vec<_>>();

        let mut finished = Vec::new();
        // A durable started claim means execution may already have happened.
        // Resolve that uncertainty before applying liveness policy so a new
        // guard can never disguise or replay an interrupted side effect.
        for call in &unresolved {
            match store.tool_run_state(&self.session, &call.id)? {
                ToolRunState::Unclaimed => {}
                ToolRunState::Started => {
                    let result = store.tool_result(&self.session, &call.id)?.unwrap_or_else(|| "ERROR: Execution uncertain after interruption. This tool will NOT be rerun automatically. Ask the user to inspect any side effects before issuing further mutations.".into());
                    store.complete_tool_with_outcome(
                        &self.session,
                        &call.id,
                        &result,
                        ToolOutcome::Uncertain,
                    )?;
                    bail!(
                        "An interrupted tool has an uncertain outcome. Its status is saved. Inspect the workspace before /retry."
                    );
                }
                ToolRunState::Finished => finished.push(*call),
            }
        }
        if !finished.is_empty() {
            let mut restored_uncertain = false;
            for call in finished {
                restored_uncertain |=
                    store.restore_finished_tool_message(&self.session, &call.id)?;
            }
            if restored_uncertain {
                bail!(
                    "A finished tool result was restored with an uncertain outcome. Its status is saved and no further tool or model request was dispatched. Inspect possible side effects and send a new instruction before continuing."
                );
            }
            // The current projection now contains the durable results. Do not
            // process the stale unresolved list from before the repair.
            return Ok(());
        }

        if !unresolved.is_empty()
            && assistant.tool_calls.len() > self.profile.pipeline.tool_calls_per_response
        {
            bail!(
                "Saved assistant response contains {} tool calls; the configured limit is {}. The unresolved batch was rejected before execution. Session remains pending; send a new instruction or /cancel to discard the saved calls.",
                assistant.tool_calls.len(),
                self.profile.pipeline.tool_calls_per_response
            );
        }
        let mut next = 0;
        while next < unresolved.len() {
            let admitted = self.admit(store, &unresolved[next..], emit)?;
            next += admitted.len();
            let results =
                if let Some(reason) = admitted.first().and_then(|entry| entry.refusal.clone()) {
                    vec![(reason, ToolOutcome::Failed)]
                } else if admitted
                    .first()
                    .is_some_and(|entry| self.side_effect_free(&entry.action))
                {
                    self.execute_side_effect_free(store, &admitted, emit).await
                } else {
                    let mut results = Vec::with_capacity(admitted.len());
                    for entry in &admitted {
                        results.push(self.execute(store, entry, approve).await);
                    }
                    results
                };
            for (entry, (result, outcome)) in admitted.into_iter().zip(results) {
                self.commit(store, entry, result, outcome, emit)?;
            }
            // Approval callbacks and bounded file tools can complete synchronously.
            // Let the owning adapter observe cancellation before another tool or
            // model request, after these results are safely committed.
            tokio::task::yield_now().await;
        }
        Ok(())
    }

    /// Answer a call without running it: the reason becomes its result, so
    /// the model can adapt instead of the run ending.
    pub(super) fn refuse_call(
        &self,
        store: &mut Store,
        call: &ToolCall,
        reason: String,
        emit: &mut dyn FnMut(AgentEvent),
    ) -> Result<()> {
        if !store.claim_side_effect_free_tool(&self.session, &call.id)? {
            return Ok(());
        }
        let detail = builder_tools::call_summary(&call.function.name, &call.function.arguments);
        emit(AgentEvent::ToolStarted {
            name: call.function.name.clone(),
            detail: detail.clone(),
        });
        let entry = Admitted {
            call,
            action: Action::from_call(call),
            detail,
            refusal: None,
        };
        self.commit(store, entry, reason, ToolOutcome::Failed, emit)
    }

    /// Whether a decoded call is guaranteed not to change the workspace.
    fn side_effect_free(&self, action: &Result<Action>) -> bool {
        match action {
            Ok(action) if action.is_inspection() => true,
            Ok(Action::Subagent { .. }) => self.profile.pipeline.subagents,
            _ => false,
        }
    }

    /// Claim the next call, or a run of consecutive side-effect-free calls
    /// that execute together. Admission reads durable state; the first call
    /// keeps the original stop-and-explain semantics, and a later call that
    /// fails a check simply waits to be the first call of the next group.
    fn admit<'a>(
        &self,
        store: &mut Store,
        calls: &[&'a ToolCall],
        emit: &mut dyn FnMut(AgentEvent),
    ) -> Result<Vec<Admitted<'a>>> {
        let history = store.history_messages(&self.session)?;
        let outcomes = store.tool_outcomes(&self.session)?;
        let no_progress = no_progress_streak(&history, &outcomes);
        let guards = tool_guard_state(&history, &outcomes);
        let max_no_progress_calls = self.profile.pipeline.max_no_progress_calls();
        let mut admitted: Vec<Admitted<'a>> = Vec::new();
        for call in calls {
            let action = Action::from_call(call);
            let side_effect_free = self.side_effect_free(&action);
            if let Some(first) = admitted.first() {
                let group_open = self.side_effect_free(&first.action)
                    && side_effect_free
                    && admitted.len() < self.profile.pipeline.parallel_tools
                    && no_progress + admitted.len() < max_no_progress_calls;
                if !group_open {
                    break;
                }
            } else {
                ensure!(
                    no_progress < max_no_progress_calls,
                    "Stopped before executing saved {} call {}: {no_progress} tool calls have completed without new inspection evidence or a successful file edit since the latest user instruction, reaching the configured limit of {max_no_progress_calls}. This call remains unclaimed. Existing evidence is preserved; send a new instruction to continue a narrowed task or /cancel to discard saved calls.",
                    call.function.name,
                    call.id
                );
                // A request the guards will not run is answered with the
                // reason instead of ending the run, so the model can adapt.
                if let Some(violation) = tool_guard_violation(
                    [*call],
                    &guards,
                    self.profile.pipeline.identical_shell_calls,
                ) && store.claim_side_effect_free_tool(&self.session, &call.id)?
                {
                    let detail =
                        builder_tools::call_summary(&call.function.name, &call.function.arguments);
                    emit(AgentEvent::ToolStarted {
                        name: call.function.name.clone(),
                        detail: detail.clone(),
                    });
                    return Ok(vec![Admitted {
                        call,
                        action,
                        detail,
                        refusal: Some(refusal(&violation)),
                    }]);
                }
            }
            let claimed = if side_effect_free {
                store.claim_side_effect_free_tool(&self.session, &call.id)?
            } else {
                store.claim_tool(&self.session, &call.id)?
            };
            if !claimed {
                if !admitted.is_empty() {
                    break;
                }
                let result = store.tool_result(&self.session, &call.id)?.unwrap_or_else(|| "ERROR: Execution uncertain after interruption. This tool will NOT be rerun automatically. Ask the user to inspect any side effects before issuing further mutations.".into());
                store.complete_tool_with_outcome(
                    &self.session,
                    &call.id,
                    &result,
                    ToolOutcome::Uncertain,
                )?;
                // Stop before the model can issue a replacement mutation.
                bail!(
                    "An interrupted tool has an uncertain outcome. Its status is saved. Inspect the workspace before /retry."
                );
            }
            let detail = builder_tools::call_summary(&call.function.name, &call.function.arguments);
            emit(AgentEvent::ToolStarted {
                name: call.function.name.clone(),
                detail: detail.clone(),
            });
            admitted.push(Admitted {
                call,
                action,
                detail,
                refusal: None,
            });
        }
        Ok(admitted)
    }

    /// Run claimed side-effect-free calls together. Inspections use blocking
    /// threads; subagents share the model through a bounded number of slots.
    async fn execute_side_effect_free(
        &self,
        store: &Store,
        admitted: &[Admitted<'_>],
        emit: &mut dyn FnMut(AgentEvent),
    ) -> Vec<(String, ToolOutcome)> {
        let emit = std::cell::RefCell::new(emit);
        let capacity = if admitted
            .iter()
            .any(|entry| matches!(&entry.action, Ok(Action::Subagent { .. })))
        {
            self.provider.parallel_capacity().await
        } else {
            None
        };
        let slots = tokio::sync::Semaphore::new(subagent_slots(&self.profile, capacity));
        let runs = admitted.iter().map(|entry| async {
            let result = match &entry.action {
                Ok(Action::Subagent {
                    description,
                    prompt,
                }) => match slots.acquire().await {
                    Ok(_slot) => {
                        self.delegate(store, entry.call, description, prompt, &emit)
                            .await
                    }
                    Err(error) => Err(error.into()),
                },
                Ok(action) => {
                    let workspace = self.workspace.clone();
                    let action = action.clone();
                    tokio::task::spawn_blocking(move || workspace.inspect(&action))
                        .await
                        .map_err(anyhow::Error::from)
                        .and_then(|result| result)
                }
                Err(error) => Err(anyhow::anyhow!("Invalid tool request: {error}")),
            };
            match result {
                Ok(text) => (text, ToolOutcome::Succeeded),
                Err(error) => (format!("ERROR: {error:#}"), ToolOutcome::Failed),
            }
        });
        futures_util::future::join_all(runs).await
    }

    /// Execute one claimed call that may change the workspace or needs the
    /// session store, after the approval policy allows it.
    async fn execute(
        &self,
        store: &mut Store,
        entry: &Admitted<'_>,
        approve: &mut dyn FnMut(&Action) -> bool,
    ) -> (String, ToolOutcome) {
        let action = match &entry.action {
            Ok(action) => action,
            Err(error) => {
                return (
                    format!("ERROR: Invalid tool request: {error}"),
                    ToolOutcome::Failed,
                );
            }
        };
        let allowed = match (action.risk(), self.approval) {
            (Risk::Read, _) | (_, ApprovalMode::Trust) => true,
            (_, ApprovalMode::ReadOnly) => false,
            (_, ApprovalMode::Ask) => approve(action),
        };
        if !allowed {
            return (
                "DENIED: The user did not authorize this tool. Do not repeat it.".into(),
                ToolOutcome::Denied,
            );
        }
        let mutation_path = match action {
            Action::WriteFile { path, .. }
            | Action::EditFile { path, .. }
            | Action::MultiEdit { path, .. } => Some(path.as_str()),
            _ => None,
        };
        let before = mutation_path.and_then(|path| self.workspace.source_hash(path).ok());
        let execution = match action {
            Action::Research { request } => {
                if self.memory.is_none()
                    && matches!(
                        request,
                        builder_core::research::Request::Learn { .. }
                            | builder_core::research::Request::Recall { .. }
                    )
                {
                    Err(anyhow::anyhow!(
                        "Memory is disabled; procedural learning/recall is unavailable"
                    ))
                } else {
                    crate::research::execute_with_settings(
                        &self.provider,
                        store,
                        &self.session,
                        &self.workspace,
                        request,
                        self.profile.context_tokens,
                        &self.profile.pipeline,
                    )
                    .await
                }
            }
            Action::CodeSearch { query, limit } => {
                crate::code_index::search(
                    store,
                    &self.session,
                    &self.workspace,
                    self.memory.as_ref().map(|memory| memory.embeddings()),
                    &self.profile.pipeline,
                    query,
                    *limit,
                )
                .await
            }
            Action::TodoWrite { .. } if !self.profile.pipeline.todos => Err(anyhow::anyhow!(
                "The todo list is disabled by pipeline configuration"
            )),
            Action::Subagent { .. } => Err(anyhow::anyhow!(
                "Subagents are disabled by pipeline configuration"
            )),
            action if crate::memory::is_memory(action) => match &self.memory {
                Some(memory) => {
                    memory
                        .execute_with_settings(
                            store,
                            &self.session,
                            &self.workspace,
                            action,
                            Some(&self.profile.pipeline),
                        )
                        .await
                }
                None => Err(anyhow::anyhow!("Memory is disabled")),
            },
            Action::Shell {
                command,
                timeout_secs,
            } => {
                let shell = Action::Shell {
                    command: command.clone(),
                    timeout_secs: self.profile.pipeline.command_timeout(*timeout_secs),
                };
                self.workspace.execute(&shell).await
            }
            action => self.workspace.execute(action).await,
        };
        match execution {
            Ok(result) => {
                let mut outcome = ToolOutcome::Succeeded;
                if matches!(
                    action,
                    Action::Research {
                        request: builder_core::research::Request::CandidateApply { .. }
                    }
                ) {
                    // Candidate application validates nonempty, changing patches.
                    outcome = ToolOutcome::Changed;
                }
                if let Some(path) = mutation_path {
                    let after = self.workspace.source_hash(path).ok();
                    if after.is_some() && after != before {
                        outcome = ToolOutcome::Changed;
                    }
                }
                (result, outcome)
            }
            Err(error) => {
                let outcome = match error.downcast_ref::<builder_tools::ShellFailure>() {
                    Some(failure)
                        if failure.kind != builder_tools::ShellFailureKind::NonZeroExit =>
                    {
                        ToolOutcome::Uncertain
                    }
                    _ => ToolOutcome::Failed,
                };
                (format!("ERROR: {error:#}"), outcome)
            }
        }
    }

    /// Durably record one result, in call order, then report it.
    fn commit(
        &self,
        store: &mut Store,
        entry: Admitted<'_>,
        result: String,
        outcome: ToolOutcome,
        emit: &mut dyn FnMut(AgentEvent),
    ) -> Result<()> {
        let Admitted {
            call,
            action,
            detail,
            ..
        } = entry;
        let is_research = matches!(&action, Ok(Action::Research { .. }));
        let result = if is_research {
            match serde_json::from_str::<serde_json::Value>(&result) {
                Ok(mut value) if value["builder_research"] == 1 => {
                    value["record_id"] = serde_json::json!(format!("{}:{}", self.session, call.id));
                    serde_json::to_string(&value)?
                }
                _ => result,
            }
        } else {
            result
        };
        store.complete_tool_with_outcome(&self.session, &call.id, &result, outcome)?;
        emit(AgentEvent::ToolFinished {
            name: call.function.name.clone(),
            detail,
            note: builder_tools::result_note(&call.function.name, &result),
            failed: matches!(
                outcome,
                ToolOutcome::Failed | ToolOutcome::Denied | ToolOutcome::Uncertain
            ) || (is_research
                && serde_json::from_str::<serde_json::Value>(&result).is_ok_and(|value| {
                    value["passed"] == false || value["verdict"] == "needs_work"
                })),
        });
        if outcome == ToolOutcome::Succeeded
            && let Ok(Action::TodoWrite { todos }) = action
        {
            emit(AgentEvent::TodosUpdated(todos));
        }
        Ok(())
    }
}

/// A claimed call awaiting execution.
struct Admitted<'a> {
    call: &'a ToolCall,
    action: Result<Action>,
    detail: String,
    /// Why the guards answer this call instead of running it.
    refusal: Option<String>,
}

/// The result recorded for a call the guards will not run. The run goes on:
/// the model reads this and chooses something else.
fn refusal(violation: &ToolGuardViolation<'_>) -> String {
    match violation {
        ToolGuardViolation::Blocked {
            call,
            outcome: ToolOutcome::Denied,
        } => format!(
            "NOT RUN: the user denied this exact {} request earlier, and it stays denied until their next message. Choose a different action or ask the user.",
            call.function.name
        ),
        ToolGuardViolation::Blocked { call, outcome } => format!(
            "NOT RUN: an identical {} request already ran and its outcome is {} (it timed out or was interrupted after starting). Check what it changed before relying on it. The identical request stays blocked until the user's next message; a different or corrected request runs normally.",
            call.function.name,
            blocked_outcome_name(*outcome)
        ),
        ToolGuardViolation::Repeated { call, prior_count } => format!(
            "NOT RUN: this identical {} request already ran {prior_count} times since the latest user message or file change, so its result would not change. Use the recorded result, change something first, or answer the user.",
            call.function.name
        ),
    }
}
