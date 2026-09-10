//! Provider boundary and OpenAI-compatible transport. No UI or filesystem tools.
pub mod local_embedding;
mod openai;
mod sse;
use anyhow::Result;
use builder_core::protocol::Message;
pub use openai::OpenAiCompatible;
use serde_json::Value;
pub use sse::SseDecoder;
use std::future::Future;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Activity {
    Connected,
    Thinking,
    PreparingTools,
}

#[derive(Debug)]
pub enum Event {
    Prompt { bytes: usize },
    Activity(Activity),
    Attempt { number: u32, maximum: u32 },
    Delta(String),
    Retry { delay_ms: u64, reason: String },
}

/// A successful response is complete and validated, never a partial generation.
pub trait Provider {
    fn complete_with_budget(
        &self,
        messages: &[Message],
        tools: &[Value],
        _max_output_tokens: usize,
        emit: &mut dyn FnMut(Event),
    ) -> impl Future<Output = Result<Message>> {
        self.complete(messages, tools, emit)
    }
    /// A tool-free response constrained to one JSON object when the endpoint
    /// supports response_format; otherwise an ordinary completion.
    fn complete_json(
        &self,
        messages: &[Message],
        max_output_tokens: usize,
        emit: &mut dyn FnMut(Event),
    ) -> impl Future<Output = Result<Message>> {
        self.complete_with_budget(messages, &[], max_output_tokens, emit)
    }
    fn complete(
        &self,
        messages: &[Message],
        tools: &[Value],
        emit: &mut dyn FnMut(Event),
    ) -> impl Future<Output = Result<Message>>;
}

/// A response exhausted its generation budget. Partial output is never usable.
#[derive(Debug)]
pub struct OutputLimit;
impl std::fmt::Display for OutputLimit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Model hit its output limit (not the context limit); incomplete output was not committed. Increase max_output_tokens or request a smaller response.")
    }
}
impl std::error::Error for OutputLimit {}
