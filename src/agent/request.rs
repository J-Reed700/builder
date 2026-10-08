//! Build a bounded request without changing original conversation messages.
use super::{Agent, AgentEvent, estimate_tokens};
use anyhow::{Result, ensure};
use builder_core::{
    protocol::{Message, Role},
    research::Phase,
    store::Store,
};
use builder_provider::Provider;
use serde_json::Value;

pub(super) struct RequestContext<'a> {
    pub messages: Vec<Message>,
    pub definitions: &'a [Value],
    pub parallel_notice: Option<&'a Message>,
    pub remaining: usize,
    pub phase: &'a Phase,
    pub guidance: Option<Message>,
    pub continuation: Option<Message>,
    pub todo_packet: Option<Message>,
}

impl<P: Provider> Agent<P> {
    pub(super) async fn prepare_request(
        &self,
        store: &mut Store,
        context: RequestContext<'_>,
        output_budget: usize,
        emit: &mut dyn FnMut(AgentEvent),
    ) -> Result<(Vec<Message>, usize)> {
        let RequestContext {
            mut messages,
            definitions,
            parallel_notice,
            remaining,
            phase,
            guidance,
            continuation,
            todo_packet,
        } = context;
        let mut memory_packet = if let Some(memory) = &self.memory {
            match memory
                .packet_with_todos(
                    store,
                    &self.session,
                    &self.workspace,
                    self.profile.tools && self.profile.pipeline.todos,
                )
                .await
            {
                Ok(packet) => Some(packet),
                Err(error) => {
                    emit(AgentEvent::MemoryNotice(format!(
                        "Memory unavailable; continuing without retrieval: {error}"
                    )));
                    None
                }
            }
        } else {
            None
        };
        let mut code_packet = match crate::code_index::packet(
            store,
            &self.session,
            &self.workspace,
            &self.profile.pipeline,
        ) {
            Ok(packet) => packet,
            Err(error) => {
                emit(AgentEvent::MemoryNotice(format!(
                    "Automatic code context unavailable; continuing with normal tools: {error}"
                )));
                None
            }
        };
        let research_packet = if self.profile.tools && self.profile.pipeline.enabled {
            Some(crate::research::packet_with_settings_for_phase(
                store,
                &self.session,
                &self.workspace,
                self.memory.is_some(),
                &self.profile.pipeline,
                phase,
            )?)
        } else {
            None
        };
        let budget_notice = (remaining <= 5).then(|| Message::text(Role::System, format!(
                "Run budget: {remaining} model rounds remain, including this one. Prioritize the next necessary action and verification. Do not claim completion without evidence or reduce the requested scope to meet this budget. Unfinished work remains saved for continuation."
            )));
        let capacity_tokens =
            parallel_notice.map_or(0, |m| estimate_tokens(std::slice::from_ref(m)));
        let mut schemas = capacity_tokens
            + budget_notice
                .as_ref()
                .map_or(0, |m| estimate_tokens(std::slice::from_ref(m)))
            + research_packet
                .as_ref()
                .map_or(0, |m| estimate_tokens(std::slice::from_ref(m)))
            + memory_packet
                .as_ref()
                .map_or(0, |m| estimate_tokens(std::slice::from_ref(m)))
            + code_packet
                .as_ref()
                .map_or(0, |m| estimate_tokens(std::slice::from_ref(m)))
            + continuation
                .as_ref()
                .map_or(0, |m| estimate_tokens(std::slice::from_ref(m)))
            + todo_packet
                .as_ref()
                .map_or(0, |m| estimate_tokens(std::slice::from_ref(m)))
            + serde_json::to_vec(definitions)?.len() / 2
            + guidance
                .as_ref()
                .map_or(0, |m| estimate_tokens(std::slice::from_ref(m)));
        let threshold =
            self.profile.context_tokens * usize::from(self.profile.compact_at_percent) / 100;
        if self.profile.auto_compact
            && estimate_tokens(&messages) + schemas + output_budget >= threshold
        {
            emit(AgentEvent::AutoCompact {
                messages: estimate_tokens(&messages),
                overhead: schemas,
                reserved: output_budget,
                threshold,
                context_tokens: self.profile.context_tokens,
            });
            self.compact(store, emit).await?;
            messages = store.messages(&self.session)?;
        }
        if estimate_tokens(&messages) + schemas + output_budget > self.profile.context_tokens
            && let Some(packet) = code_packet.take()
        {
            schemas -= estimate_tokens(std::slice::from_ref(&packet));
            emit(AgentEvent::MemoryNotice("Automatic code-index reference omitted for this request to fit the configured context budget; the index and code_search tool remain available".into()));
        }
        if estimate_tokens(&messages) + schemas + output_budget > self.profile.context_tokens
            && let Some(packet) = memory_packet.take()
        {
            schemas -= estimate_tokens(std::slice::from_ref(&packet));
            emit(AgentEvent::MemoryNotice("Memory reference omitted for this request to fit the context budget; stored notes retained".into()));
        }
        let estimate = estimate_tokens(&messages) + schemas;
        ensure!(
            estimate + output_budget <= self.profile.context_tokens,
            "Context budget reached (estimated {estimate} input + {} output / {}). History is intact. Use /compact, reduce the latest prompt, or verify context_tokens against the server's actual capacity.",
            output_budget,
            self.profile.context_tokens
        );
        if let Some(notice) = parallel_notice {
            messages.insert(0, notice.clone());
        }
        if let Some(notice) = budget_notice {
            messages.insert(0, notice);
        }
        if let Some(packet) = todo_packet {
            messages.insert(0, packet);
        }
        if let Some(packet) = research_packet {
            messages.insert(0, packet);
        }
        if let Some(packet) = memory_packet {
            messages.insert(0, packet);
        }
        if let Some(packet) = code_packet {
            messages.insert(0, packet);
        }
        if let Some(guidance) = guidance {
            // Request-only runtime policy, regenerated from durable evidence.
            // Keep runtime policy before task data; the labelled continuation
            // below reinforces the next action without changing durable history.
            messages.insert(0, guidance);
        }
        if let Some(continuation) = continuation {
            messages.push(continuation);
        }
        Ok((messages, estimate))
    }
}
