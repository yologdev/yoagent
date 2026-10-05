//! Retry with exponential backoff and jitter for provider calls.

use crate::provider::ProviderError;
use std::time::Duration;
use tracing::warn;

/// Configuration for automatic retry of transient provider errors.
///
/// Defaults: 3 retries, 1s initial delay, 2x backoff, 30s max delay.
/// Use `RetryConfig::none()` to disable retries entirely.
#[derive(Debug, Clone)]
pub struct RetryConfig {
    /// Maximum number of retry attempts (0 = no retries).
    pub max_retries: usize,
    /// Initial delay before the first retry (milliseconds).
    pub initial_delay_ms: u64,
    /// Multiplier applied to the delay after each attempt.
    pub backoff_multiplier: f64,
    /// Maximum delay between retries (milliseconds).
    pub max_delay_ms: u64,
}

impl Default for RetryConfig {
    fn default() -> Self {
        Self {
            max_retries: 3,
            initial_delay_ms: 1000,
            backoff_multiplier: 2.0,
            max_delay_ms: 30_000,
        }
    }
}

impl RetryConfig {
    /// No retries — fail immediately on any error.
    pub fn none() -> Self {
        Self {
            max_retries: 0,
            ..Default::default()
        }
    }

    /// Calculate the delay for a given attempt (1-indexed).
    /// Uses exponential backoff with ±20% jitter.
    pub fn delay_for_attempt(&self, attempt: usize) -> Duration {
        // `saturating_sub`, not `attempt - 1`: this is a public method whose
        // 1-indexed contract is easy to miss, and `usize` underflow panics in
        // debug. `llm_compaction.rs` passed it 0-indexed and died on the first
        // retry — on a *detached* task, so the summarization simply vanished
        // and compaction fell back deterministically with nothing logged.
        // A misuse should cost the backoff, not the task.
        let base_ms = self.initial_delay_ms as f64
            * self
                .backoff_multiplier
                .powi(attempt.saturating_sub(1) as i32);
        let capped_ms = base_ms.min(self.max_delay_ms as f64);

        // Jitter: ±20% (multiply by 0.8–1.2)
        let jitter = 0.8 + rand::random::<f64>() * 0.4;
        Duration::from_millis((capped_ms * jitter) as u64)
    }
}

impl ProviderError {
    /// Whether this error is safe to retry.
    ///
    /// Retryable: rate limits (429) and network/transient errors.
    /// Not retryable: auth errors, API errors (bad request), cancellation.
    pub fn is_retryable(&self) -> bool {
        matches!(self, Self::RateLimited { .. } | Self::Network(_))
    }

    /// If this is a rate limit with a server-specified retry delay, return it.
    pub fn retry_after(&self) -> Option<Duration> {
        match self {
            Self::RateLimited {
                retry_after_ms: Some(ms),
            } => Some(Duration::from_millis(*ms)),
            _ => None,
        }
    }
}

/// Log a retry attempt.
pub(crate) fn log_retry(attempt: usize, max: usize, delay: &Duration, error: &ProviderError) {
    warn!(
        "Provider error (attempt {}/{}), retrying in {:.1}s: {}",
        attempt,
        max,
        delay.as_secs_f64(),
        error
    );
}

// ---------------------------------------------------------------------------
// Retry-safe event streams
// ---------------------------------------------------------------------------

/// A filter that holds back each provider attempt's streamed output until the
/// attempt succeeds, so a consumer never sees the text of an attempt that was
/// retried.
///
/// The loop retries a provider attempt that fails with a retryable error
/// (`RateLimited`, `Network`), and by then that attempt may already have
/// streamed part of its answer. A consumer that can rewind handles this with
/// the events it gets: the failed attempt ends with an error `MessageEnd`,
/// followed by [`AgentEvent::ProviderRetry`](crate::AgentEvent::ProviderRetry).
/// A consumer writing to an **append-only sink** — stdout on a pipe, a log, a
/// stream to a client — cannot take text back. Feed its events through this
/// filter instead:
///
/// - An assistant attempt's `MessageStart` and `MessageUpdate`s are held and
///   released together with its `MessageEnd` when it succeeds (any stop reason
///   but `Error` or `Aborted`).
/// - An attempt that is retried disappears: only its `ProviderRetry` passes.
///   That includes the last one when the run is aborted during the retry's
///   backoff: the turn's final state is then only in `TurnEnd` / `AgentEnd`.
/// - An attempt that fails for good passes as its `MessageStart` and its
///   `Error` or `Aborted` `MessageEnd` (whose `content` keeps what it
///   produced), without its deltas.
/// - An open attempt is dropped, never released, if the stream ends, a
///   `ProviderRetry` arrives or another attempt starts before it ends.
/// - Every other event passes through at once, so it can overtake an attempt
///   being held. Use one filter per agent's stream.
///
/// It covers this agent's own assistant messages. A sub-agent's text reaches
/// the parent as `ToolExecutionUpdate`s, which pass through unfiltered.
///
/// The cost is incremental output: an attempt's text arrives all at once when
/// it finishes, rather than as it is generated. That suits non-interactive
/// output. A consumer that can rewind should keep the live stream, and one a
/// person watches can instead abort when a retry follows streamed text (see
/// [Interactive terminals](https://yologdev.github.io/yoagent/concepts/retry.html#interactive-terminals)).
///
/// ```
/// use yoagent::retry::RetrySafeEvents;
/// # fn forward(_: yoagent::AgentEvent) {}
/// # fn run(events: Vec<yoagent::AgentEvent>) {
/// let mut filter = RetrySafeEvents::new();
/// for event in events {
///     for ready in filter.push(event) {
///         forward(ready);
///     }
/// }
/// for ready in filter.finish() {
///     forward(ready);
/// }
/// # }
/// ```
///
/// For a receiver from [`Agent::prompt`](crate::Agent::prompt), see
/// [`retry_safe_events`].
#[derive(Debug, Default)]
pub struct RetrySafeEvents {
    held: Held,
}

/// What the filter is holding.
// One per filter, never stored in bulk: boxing the events would only add
// allocations. (The lint fires on wasm32, where the sizes differ.)
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Default)]
enum Held {
    #[default]
    Nothing,
    /// An assistant attempt in progress: its `MessageStart` and updates.
    Open {
        start: crate::AgentEvent,
        updates: Vec<crate::AgentEvent>,
    },
    /// An attempt that ended with `Error` or `Aborted`, waiting on the next
    /// event: a `ProviderRetry` means it was retried, anything else that it
    /// was final. `start` is `None` for an end that arrived without one.
    Failed {
        start: Option<crate::AgentEvent>,
        end: crate::AgentEvent,
    },
}

impl RetrySafeEvents {
    /// An empty filter.
    pub fn new() -> Self {
        Self::default()
    }

    /// Take one event; returns the events to forward now, in order.
    #[must_use = "the returned events are the filter's output; dropping them loses them"]
    pub fn push(&mut self, event: crate::AgentEvent) -> Vec<crate::AgentEvent> {
        use crate::AgentEvent;
        let mut out = Vec::new();
        match std::mem::take(&mut self.held) {
            Held::Nothing => {}
            Held::Failed { start, end } => {
                if matches!(event, AgentEvent::ProviderRetry { .. }) {
                    // Retried: as far as the consumer is concerned, the
                    // failed attempt never happened.
                    out.push(event);
                    return out;
                }
                out.extend(start);
                out.push(end);
            }
            Held::Open { start, updates } => match event {
                AgentEvent::MessageUpdate { .. } => {
                    let mut updates = updates;
                    updates.push(event);
                    self.held = Held::Open { start, updates };
                    return out;
                }
                AgentEvent::MessageEnd { ref message } if is_assistant(message) => {
                    if ended_without_answer(message) {
                        self.held = Held::Failed {
                            start: Some(start),
                            end: event,
                        };
                    } else {
                        out.push(start);
                        out.extend(updates);
                        out.push(event);
                    }
                    return out;
                }
                AgentEvent::MessageStart { ref message } if is_assistant(message) => {
                    // The loop always closes an attempt before the next one;
                    // if it did not, this one's fate is unknown, so it is
                    // dropped rather than risk showing retried text.
                    tracing::warn!("retry-safe events: an attempt started before the previous one ended; dropping the unfinished one");
                    self.held = Held::Open {
                        start: event,
                        updates: Vec::new(),
                    };
                    return out;
                }
                AgentEvent::ProviderRetry { .. } => {
                    // Retried without being closed: drop it, keep the marker.
                    tracing::warn!("retry-safe events: a retry arrived inside an open attempt; dropping the attempt");
                    out.push(event);
                    return out;
                }
                other => {
                    // Passes at once, ahead of the attempt still held.
                    self.held = Held::Open { start, updates };
                    out.push(other);
                    return out;
                }
            },
        }
        match event {
            AgentEvent::MessageStart { ref message } if is_assistant(message) => {
                self.held = Held::Open {
                    start: event,
                    updates: Vec::new(),
                };
            }
            AgentEvent::MessageEnd { ref message }
                if is_assistant(message) && ended_without_answer(message) =>
            {
                self.held = Held::Failed {
                    start: None,
                    end: event,
                };
            }
            other => out.push(other),
        }
        out
    }

    /// The stream ended: returns whatever is still held, and resets the
    /// filter so it can be reused for another stream. A failed attempt with
    /// nothing after it was final, so its start and end are released; an
    /// attempt the stream ended inside is dropped (and logged).
    #[must_use = "the returned events are the filter's output; dropping them loses them"]
    pub fn finish(&mut self) -> Vec<crate::AgentEvent> {
        match std::mem::take(&mut self.held) {
            Held::Nothing => Vec::new(),
            Held::Open { updates, .. } => {
                tracing::warn!(
                    updates = updates.len(),
                    "retry-safe events: the stream ended inside an attempt; dropping it"
                );
                Vec::new()
            }
            Held::Failed { start, end } => start.into_iter().chain([end]).collect(),
        }
    }
}

fn is_assistant(message: &crate::AgentMessage) -> bool {
    matches!(
        message,
        crate::AgentMessage::Llm(crate::Message::Assistant { .. })
    )
}

fn ended_without_answer(message: &crate::AgentMessage) -> bool {
    matches!(
        message,
        crate::AgentMessage::Llm(crate::Message::Assistant {
            stop_reason: crate::StopReason::Error | crate::StopReason::Aborted,
            ..
        })
    )
}

/// [`RetrySafeEvents`] over a receiver: returns a receiver that yields the
/// same events with each provider attempt held back until it succeeds.
///
/// ```no_run
/// # async fn demo(mut agent: yoagent::Agent) {
/// let mut rx = yoagent::retry::retry_safe_events(agent.prompt("hi").await);
/// while let Some(event) = rx.recv().await {
///     // Only successful attempts' text reaches here.
/// }
/// agent.finish().await;
/// # }
/// ```
///
/// The filtering runs on a task spawned with [`crate::rt::spawn`], so this
/// needs a runtime (Tokio natively; the host's executor on wasm32). Dropping
/// the returned receiver ends the task, and the run carries on unobserved, as
/// it would if the original receiver were dropped.
#[must_use = "the returned receiver carries the run's events"]
pub fn retry_safe_events(
    mut rx: tokio::sync::mpsc::UnboundedReceiver<crate::AgentEvent>,
) -> tokio::sync::mpsc::UnboundedReceiver<crate::AgentEvent> {
    let (tx, out) = tokio::sync::mpsc::unbounded_channel();
    crate::rt::spawn(async move {
        let mut filter = RetrySafeEvents::new();
        while let Some(event) = rx.recv().await {
            for ready in filter.push(event) {
                if tx.send(ready).is_err() {
                    return;
                }
            }
        }
        for ready in filter.finish() {
            let _ = tx.send(ready);
        }
    });
    out
}

#[cfg(test)]
mod retry_safe {
    use super::RetrySafeEvents;
    use crate::{AgentEvent, AgentMessage, Message, StopReason, StreamDelta, Usage};

    /// `model` tells otherwise identical events apart, so order assertions
    /// can see which attempt an event belongs to.
    fn assistant(model: &str, stop: StopReason) -> AgentMessage {
        AgentMessage::Llm(
            Message::assistant(vec![], stop, model, "p", Usage::default()).with_timestamp(0),
        )
    }
    fn start(model: &str) -> AgentEvent {
        AgentEvent::MessageStart {
            message: assistant(model, StopReason::Stop),
        }
    }
    fn update(model: &str, delta: StreamDelta) -> AgentEvent {
        AgentEvent::MessageUpdate {
            message: assistant(model, StopReason::Stop),
            delta,
        }
    }
    fn text(model: &str, t: &str) -> AgentEvent {
        update(model, StreamDelta::Text { delta: t.into() })
    }
    fn end(model: &str, stop: StopReason) -> AgentEvent {
        AgentEvent::MessageEnd {
            message: assistant(model, stop),
        }
    }
    fn retry(attempt: usize) -> AgentEvent {
        AgentEvent::ProviderRetry {
            attempt,
            max_attempts: 3,
            error: "cut".into(),
            delay_ms: 1,
        }
    }
    fn progress() -> AgentEvent {
        AgentEvent::ProgressMessage {
            tool_call_id: "t".into(),
            tool_name: "n".into(),
            text: "working".into(),
        }
    }
    fn feed(events: Vec<AgentEvent>) -> Vec<AgentEvent> {
        let mut filter = RetrySafeEvents::new();
        let mut out: Vec<AgentEvent> = events.into_iter().flat_map(|e| filter.push(e)).collect();
        out.extend(filter.finish());
        out
    }

    #[test]
    fn a_successful_attempt_is_released_whole_at_its_end() {
        let mut filter = RetrySafeEvents::new();
        assert!(filter.push(start("a")).is_empty());
        assert!(filter.push(text("a", "x")).is_empty());
        assert_eq!(
            filter.push(end("a", StopReason::Stop)),
            vec![start("a"), text("a", "x"), end("a", StopReason::Stop)]
        );
    }

    #[test]
    fn a_retried_attempt_leaves_only_its_marker() {
        let out = feed(vec![
            start("a"),
            text("a", "PARTIAL_"),
            end("a", StopReason::Error),
            retry(1),
            start("b"),
            text("b", "PONG"),
            end("b", StopReason::Stop),
        ]);
        assert_eq!(
            out,
            vec![
                retry(1),
                start("b"),
                text("b", "PONG"),
                end("b", StopReason::Stop)
            ]
        );
    }

    #[test]
    fn two_retries_in_a_row_leave_only_their_markers() {
        let out = feed(vec![
            start("a"),
            text("a", "A"),
            end("a", StopReason::Error),
            retry(1),
            start("b"),
            text("b", "B"),
            end("b", StopReason::Error),
            retry(2),
            start("c"),
            text("c", "C"),
            end("c", StopReason::Stop),
        ]);
        assert_eq!(
            out,
            vec![
                retry(1),
                retry(2),
                start("c"),
                text("c", "C"),
                end("c", StopReason::Stop)
            ]
        );
    }

    #[test]
    fn every_kind_of_delta_is_held_and_dropped_with_its_attempt() {
        let out = feed(vec![
            start("a"),
            update("a", StreamDelta::Thinking { delta: "t".into() }),
            update("a", StreamDelta::ToolCallDelta { delta: "{".into() }),
            text("a", "x"),
            end("a", StopReason::Error),
            retry(1),
        ]);
        assert_eq!(out, vec![retry(1)]);
    }

    #[test]
    fn a_final_failure_is_released_without_its_text() {
        // Followed by another event…
        let out = feed(vec![
            start("a"),
            text("a", "x"),
            end("a", StopReason::Error),
            AgentEvent::TurnStart,
        ]);
        assert_eq!(
            out,
            vec![
                start("a"),
                end("a", StopReason::Error),
                AgentEvent::TurnStart
            ]
        );
        // …or by the end of the stream.
        let out = feed(vec![
            start("a"),
            text("a", "x"),
            end("a", StopReason::Aborted),
        ]);
        assert_eq!(out, vec![start("a"), end("a", StopReason::Aborted)]);
    }

    #[test]
    fn an_error_end_without_a_start_is_held_like_any_failure() {
        assert_eq!(
            feed(vec![end("a", StopReason::Error), retry(1)]),
            vec![retry(1)]
        );
        assert_eq!(
            feed(vec![end("a", StopReason::Error)]),
            vec![end("a", StopReason::Error)]
        );
    }

    #[test]
    fn an_attempt_the_stream_ends_inside_is_dropped() {
        assert!(feed(vec![start("a"), text("a", "x")]).is_empty());
    }

    #[test]
    fn an_unclosed_attempt_is_dropped_when_another_starts() {
        let out = feed(vec![
            start("a"),
            text("a", "x"),
            start("b"),
            end("b", StopReason::Stop),
        ]);
        assert_eq!(out, vec![start("b"), end("b", StopReason::Stop)]);
    }

    #[test]
    fn an_unclosed_attempt_is_dropped_when_a_retry_follows() {
        let out = feed(vec![
            start("a"),
            text("a", "x"),
            retry(1),
            start("b"),
            end("b", StopReason::Stop),
        ]);
        assert_eq!(out, vec![retry(1), start("b"), end("b", StopReason::Stop)]);
    }

    #[test]
    fn other_events_overtake_a_held_attempt() {
        let out = feed(vec![
            start("a"),
            text("a", "x"),
            progress(),
            end("a", StopReason::Stop),
        ]);
        assert_eq!(
            out,
            vec![
                progress(),
                start("a"),
                text("a", "x"),
                end("a", StopReason::Stop)
            ]
        );
    }

    #[test]
    fn updates_outside_an_attempt_pass_through() {
        assert_eq!(feed(vec![text("a", "x")]), vec![text("a", "x")]);
    }

    #[test]
    fn finish_resets_the_filter_for_another_stream() {
        let mut filter = RetrySafeEvents::new();
        let _ = filter.push(start("a"));
        let _ = filter.push(end("a", StopReason::Error));
        assert_eq!(
            filter.finish(),
            vec![start("a"), end("a", StopReason::Error)]
        );
        assert!(filter.finish().is_empty());
        assert_eq!(
            filter.push(AgentEvent::TurnStart),
            vec![AgentEvent::TurnStart]
        );
    }
}

#[cfg(test)]
mod attempt_indexing {
    use super::RetryConfig;

    /// A zero attempt must not panic.
    ///
    /// `delay_for_attempt` documents 1-indexed and computed `attempt - 1`, so a
    /// 0-indexed caller hit `usize` underflow — a debug panic. `llm_compaction`
    /// did exactly that, and because the retry runs on a detached task the
    /// panic was invisible: the summarization vanished and compaction fell back
    /// deterministically, which is one of the behaviours #150 was filed about.
    #[test]
    fn a_zero_attempt_does_not_panic() {
        let cfg = RetryConfig {
            initial_delay_ms: 1000,
            backoff_multiplier: 2.0,
            max_delay_ms: 60_000,
            ..RetryConfig::default()
        };
        // Reaching this line at all is the point — the old code panicked here.
        let zero = cfg.delay_for_attempt(0).as_millis();
        // Jitter is +/-20% of the base 1000ms, so 0 degrades into the same band
        // as attempt 1 rather than to something wild.
        assert!(
            (800..=1200).contains(&zero),
            "attempt 0 must degrade to the base delay, got {zero}ms"
        );
    }

    /// Backoff still grows for the documented 1-indexed usage.
    #[test]
    fn backoff_grows_with_the_attempt_number() {
        let cfg = RetryConfig {
            initial_delay_ms: 1000,
            backoff_multiplier: 2.0,
            max_delay_ms: 60_000,
            ..RetryConfig::default()
        };
        // Jitter is +/-20%, so compare with margin rather than exactly.
        let first = cfg.delay_for_attempt(1).as_millis();
        let third = cfg.delay_for_attempt(3).as_millis();
        assert!(
            third > first * 2,
            "attempt 3 must back off well beyond attempt 1, got {first}ms then {third}ms"
        );
    }
}
