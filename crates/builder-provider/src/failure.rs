use crate::OutputLimit;
use crate::reply::MissingResponse;

/// Why one generation attempt failed, and whether another attempt may succeed.
/// Shared by every wire protocol so retry policy lives in one place.
#[derive(Debug)]
pub(crate) struct Failure {
    pub(crate) reason: String,
    pub(crate) retry: bool,
    pub(crate) retry_after: Option<u64>,
    pub(crate) output_limit: bool,
}
impl Failure {
    /// A complete response that failed durable validation. Only a missing
    /// answer is worth regenerating; malformed tool calls are not.
    pub(crate) fn validation(error: anyhow::Error) -> Self {
        if error.is::<MissingResponse>() {
            Self::transient(error.to_string())
        } else {
            Self::permanent(error.to_string())
        }
    }
    /// A response whose finish/stop reason rules out using it.
    pub(crate) fn finish(error: anyhow::Error) -> Self {
        Self {
            output_limit: error.is::<OutputLimit>(),
            ..Self::permanent(error.to_string())
        }
    }
    /// A non-success HTTP status. Never carries the body: proxies can echo
    /// credentials.
    pub(crate) fn status(status: reqwest::StatusCode, retry_after: Option<u64>) -> Self {
        Self {
            reason: format!("Endpoint returned HTTP {status}"),
            // 529: Anthropic overloaded.
            retry: matches!(status.as_u16(), 408 | 429 | 500 | 502 | 503 | 504 | 529),
            retry_after,
            output_limit: false,
        }
    }
    pub(crate) fn transient(reason: impl Into<String>) -> Self {
        Self {
            reason: reason.into(),
            retry: true,
            retry_after: None,
            output_limit: false,
        }
    }
    pub(crate) fn permanent(reason: impl Into<String>) -> Self {
        Self {
            reason: reason.into(),
            retry: false,
            retry_after: None,
            output_limit: false,
        }
    }
}
