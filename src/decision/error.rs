//! [`DecisionError`]: everything a decision-model call can fail with.

use std::sync::Arc;
use std::time::Duration;

/// Why a decision-model call failed.
///
/// `Clone`, so one error can be both logged and returned (the same policy as
/// [`PriceError`](crate::provider::PriceError)). `#[non_exhaustive]`, as is
/// each struct variant: new failure kinds and new fields are minor changes.
/// Match with `matches!(e, DecisionError::Timeout(_))` and friends; there is
/// no `PartialEq`, because an underlying source error cannot be compared.
#[derive(Debug, Clone, thiserror::Error)]
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
    /// Connection, TLS or request failure before a response arrived.
    /// Retried by [`SystemOneBackend`](super::SystemOneBackend).
    #[error("decision model transport error: {message}")]
    #[non_exhaustive]
    Transport {
        /// What failed.
        message: String,
        /// The underlying error, when there is one (shared, so the error
        /// stays `Clone`).
        #[source]
        source: Option<Arc<dyn std::error::Error + Send + Sync + 'static>>,
    },
    /// A preset's API key environment variable is unset or empty. Names the
    /// variable, never a value.
    #[error("decision model API key missing: set {0}")]
    MissingApiKey(String),
    /// A custom backend's own failure, in its words.
    #[error("decision backend error: {message}")]
    #[non_exhaustive]
    Backend {
        /// What failed.
        message: String,
        /// The underlying error, when there is one (shared, so the error
        /// stays `Clone`).
        #[source]
        source: Option<Arc<dyn std::error::Error + Send + Sync + 'static>>,
    },
    /// The answer could not be used: not JSON or unreadable, a missing
    /// answer or one of the wrong type, a probability or confidence that is
    /// not finite or not in `[0, 1]`, a distribution that does not cover
    /// every option or does not sum to 1, a choice that is not one of the
    /// options. Checked for every backend.
    #[error("decision model returned an unusable response: {0}")]
    BadResponse(String),
    /// Every model of a fallback chain
    /// ([`DecisionModel::or`](super::DecisionModel::or)) failed or was
    /// skipped for its limits. Lists each member, in the order they were
    /// tried. (When the chain's overall time ran out, the error is
    /// [`Timeout`](Self::Timeout) instead.)
    #[error("every decision model in the fallback chain failed: {}", fmt_attempts(.attempts))]
    #[non_exhaustive]
    AllFailed {
        /// One per member, in order.
        attempts: Vec<FallbackAttempt>,
    },
}

/// One member's outcome in [`DecisionError::AllFailed`].
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct FallbackAttempt {
    model: String,
    error: DecisionError,
    sent: bool,
}

impl FallbackAttempt {
    pub(crate) fn new(model: String, error: DecisionError, sent: bool) -> Self {
        Self { model, error, sent }
    }

    /// The model id this member asked for.
    pub fn model(&self) -> &str {
        &self.model
    }

    /// Why it failed — or, when it was not sent, why it was skipped.
    pub fn error(&self) -> &DecisionError {
        &self.error
    }

    /// Whether a request was sent. `false` when the member was skipped
    /// because the request exceeded its capabilities.
    pub fn was_sent(&self) -> bool {
        self.sent
    }
}

fn fmt_attempts(attempts: &[FallbackAttempt]) -> String {
    attempts
        .iter()
        .map(|a| {
            let skipped = if a.sent { "" } else { " (skipped)" };
            format!("[{}{skipped}] {}", a.model, a.error)
        })
        .collect::<Vec<_>>()
        .join("; ")
}

impl DecisionError {
    /// Whether retrying the same request may succeed: rate limits, overload,
    /// and transport failures — and [`AllFailed`](Self::AllFailed) when any
    /// member's error was one of those.
    pub fn is_retryable(&self) -> bool {
        match self {
            Self::RateLimited { .. } | Self::Transport { .. } => true,
            Self::AllFailed { attempts } => attempts.iter().any(|a| a.error.is_retryable()),
            _ => false,
        }
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

    /// A [`Transport`](Self::Transport) error without a source.
    pub fn transport(message: impl Into<String>) -> Self {
        Self::Transport {
            message: message.into(),
            source: None,
        }
    }

    /// A [`Transport`](Self::Transport) error carrying its cause.
    pub fn transport_with_source(
        message: impl Into<String>,
        source: impl std::error::Error + Send + Sync + 'static,
    ) -> Self {
        Self::Transport {
            message: message.into(),
            source: Some(Arc::new(source)),
        }
    }

    /// A [`Backend`](Self::Backend) error without a source.
    pub fn backend(message: impl Into<String>) -> Self {
        Self::Backend {
            message: message.into(),
            source: None,
        }
    }

    /// A [`Backend`](Self::Backend) error carrying its cause.
    pub fn backend_with_source(
        message: impl Into<String>,
        source: impl std::error::Error + Send + Sync + 'static,
    ) -> Self {
        Self::Backend {
            message: message.into(),
            source: Some(Arc::new(source)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::error::Error;

    #[test]
    fn sources_are_wired() {
        let io = std::io::Error::other("disk on fire");
        let e = DecisionError::backend_with_source("lookup failed", io);
        assert_eq!(e.to_string(), "decision backend error: lookup failed");
        assert_eq!(e.source().unwrap().to_string(), "disk on fire");
        assert!(DecisionError::backend("x").source().is_none());
        let t = DecisionError::transport_with_source("connect", std::io::Error::other("refused"));
        assert!(t.is_retryable());
        assert_eq!(t.source().unwrap().to_string(), "refused");
        // Clone keeps the source.
        assert!(t.clone().source().is_some());
    }
}
