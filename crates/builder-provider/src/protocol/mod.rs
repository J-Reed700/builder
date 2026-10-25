//! Wire protocols. Each one owns exactly three concerns for its API: how a
//! credential becomes request headers, how the conversation becomes a request
//! body, and how a response (streamed or not) becomes a validated [`Message`].
//! Transport, retries, and capability probes stay in [`crate::Endpoint`].
mod anthropic;
mod openai;

use crate::{Event, failure::Failure};
use anyhow::Result;
use builder_core::config::{Api, Profile};
use builder_core::protocol::Message;
use futures_util::StreamExt;
use reqwest::header::HeaderMap;
use serde_json::Value;

/// Upper bound on one response, streamed or buffered.
pub(crate) const RESPONSE_LIMIT: usize = 16 * 1024 * 1024;

/// Per-request choices made by the endpoint, independent of wire format.
pub(crate) struct Options {
    pub max_output_tokens: usize,
    /// Constrain the reply to one JSON object where the protocol can.
    pub json_mode: bool,
    /// Spend no output budget on hidden reasoning (memory extraction).
    pub suppress_thinking: bool,
    /// The server answered the llama.cpp `/props` probe.
    pub llama_cpp: bool,
}

/// An encoded request and the size of its conversation payload.
pub(crate) struct Request {
    pub body: Value,
    pub prompt_bytes: usize,
}

/// The wire protocol a profile's endpoint speaks, with any per-connection
/// state the protocol derives from its credential.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Protocol {
    OpenAi,
    Anthropic { oauth: bool },
}

impl Protocol {
    /// Select the protocol for `profile` and add its authentication headers.
    /// Explicit `Authorization` headers already in `headers` always win.
    pub(crate) fn connect(profile: &Profile, headers: &mut HeaderMap) -> Result<Self> {
        Ok(match profile.api {
            Api::OpenAi => {
                openai::authorize(profile, headers)?;
                Self::OpenAi
            }
            Api::Anthropic => Self::Anthropic {
                oauth: anthropic::authorize(profile, headers)?,
            },
        })
    }

    /// Generation path relative to the profile's base URL.
    pub(crate) fn generate_path(self) -> &'static str {
        match self {
            Self::OpenAi => "chat/completions",
            Self::Anthropic { .. } => "messages",
        }
    }

    /// OpenAI-compatible servers may be llama.cpp and expose `/props` and
    /// `/embeddings`; hosted Claude has neither.
    pub(crate) fn has_server_extensions(self) -> bool {
        matches!(self, Self::OpenAi)
    }

    pub(crate) fn encode(
        self,
        profile: &Profile,
        messages: &[Message],
        tools: &[Value],
        options: &Options,
    ) -> Result<Request> {
        match self {
            Self::OpenAi => openai::encode(profile, messages, tools, options),
            Self::Anthropic { oauth } => {
                anthropic::encode(profile, messages, tools, options, oauth)
            }
        }
    }

    pub(crate) async fn decode(
        self,
        response: reqwest::Response,
        stream: bool,
        emit: &mut dyn FnMut(Event),
    ) -> std::result::Result<Message, Failure> {
        match (self, stream) {
            (Self::OpenAi, true) => openai::stream(response, emit).await,
            (Self::Anthropic { .. }, true) => anthropic::stream(response, emit).await,
            (Self::OpenAi, false) => openai::parse(&read_json(response).await?, emit),
            (Self::Anthropic { .. }, false) => anthropic::parse(&read_json(response).await?, emit),
        }
    }
}

/// Buffer a whole non-streaming response under [`RESPONSE_LIMIT`].
async fn read_json(response: reqwest::Response) -> std::result::Result<Value, Failure> {
    let mut stream = response.bytes_stream();
    let mut bytes = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| Failure::transient(e.without_url().to_string()))?;
        if bytes.len().saturating_add(chunk.len()) > RESPONSE_LIMIT {
            return Err(Failure::permanent("Response exceeds 16 MiB"));
        }
        bytes.extend_from_slice(&chunk);
    }
    serde_json::from_slice(&bytes)
        .map_err(|_| Failure::transient("Endpoint returned incomplete or invalid JSON"))
}

/// Mark a header value sensitive so it never appears in debug output.
fn sensitive(value: &str, what: &str) -> Result<reqwest::header::HeaderValue> {
    let mut value = reqwest::header::HeaderValue::from_str(value)
        .map_err(|_| anyhow::anyhow!("Invalid {what} header value"))?;
    value.set_sensitive(true);
    Ok(value)
}
