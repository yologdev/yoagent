//! [`DecisionError`]: everything a decision-model call can fail with.

use std::time::Duration;

/// Why a decision-model call failed.
///
/// `Clone`, so one error can be both logged and returned (the same policy as
/// [`PriceError`](crate::provider::PriceError)). `#[non_exhaustive]`, as is
/// each struct variant: new failure kinds and new fields are minor changes.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
#[non_exhaustive]
pub enum DecisionError {
    /// The server answered with a non-success status that is neither a rate
    /// limit nor a validation failure (401, 5xx other than 529, ...).
    #[error("decision model returned HTTP {status}: {body}")]
    #[non_exhaustive]
    Http {
        /// HTTP status code.
        status: u16,
        /// Response body, truncated.
        body: String,
    },
    /// 429 Too Many Requests or 529 Overloaded, after retries were spent.
    #[error("decision model rate limited or overloaded (HTTP {status}){}", .retry_after.map(|d| format!(", retry after {:.1}s", d.as_secs_f64())).unwrap_or_default())]
    #[non_exhaustive]
    RateLimited {
        /// 429 or 529.
        status: u16,
        /// The server's `retry-after`, when it sent one.
        retry_after: Option<Duration>,
    },
    /// The call did not finish within the configured timeout.
    #[error("decision model call timed out after {:.1}s", .0.as_secs_f64())]
    Timeout(Duration),
    /// The request is invalid: rejected client-side before sending, or by the
    /// server with 422. The text names the offending field.
    #[error("invalid decision request: {0}")]
    Invalid(String),
    /// The backend cannot answer this kind of question. Never emulated.
    #[error("unsupported by this decision backend: {0}")]
    Unsupported(String),
    /// Connection, TLS or body-read failure.
    #[error("decision model transport error: {0}")]
    Transport(String),
    /// A preset's API key environment variable is unset or empty. Names the
    /// variable, never a value.
    #[error("decision model API key missing: set {0}")]
    MissingApiKey(String),
    /// A custom backend's own failure, in its words.
    #[error("decision backend error: {0}")]
    Backend(String),
    /// The answer could not be used: not JSON, a missing answer or one of
    /// the wrong type, a probability or confidence that is not finite or not
    /// in `[0, 1]`, a choice that is not one of the options. Checked for
    /// every backend.
    #[error("decision model returned an unusable response: {0}")]
    BadResponse(String),
}

impl DecisionError {
    /// Whether retrying the same request may succeed: rate limits, overload,
    /// and transport failures.
    pub fn is_retryable(&self) -> bool {
        matches!(self, Self::RateLimited { .. } | Self::Transport(_))
    }

    /// The server-specified retry delay, if any.
    pub fn retry_after(&self) -> Option<Duration> {
        match self {
            Self::RateLimited { retry_after, .. } => *retry_after,
            _ => None,
        }
    }

    /// An [`Http`](Self::Http) error. The variant is `#[non_exhaustive]`, so
    /// custom backends build it here.
    pub fn http(status: u16, body: impl Into<String>) -> Self {
        Self::Http {
            status,
            body: body.into(),
        }
    }

    /// A [`RateLimited`](Self::RateLimited) error, for custom backends.
    pub fn rate_limited(status: u16, retry_after: Option<Duration>) -> Self {
        Self::RateLimited {
            status,
            retry_after,
        }
    }
}
