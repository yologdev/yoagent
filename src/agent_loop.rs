//! The core agent loop: prompt → LLM stream → tool execution → repeat.
//!
//! This is the heart of yoagent:
//!
//! - `agent_loop()` starts with new prompt messages
//! - `agent_loop_continue()` resumes from existing context
//!
//! Both return a stream of `AgentEvent`s.

use crate::context::{
    self, CompactionStrategy, ContextConfig, ContextTracker, DefaultCompaction, ExecutionLimits,
    ExecutionTracker,
};
use crate::provider::{
    ModelConfig, ProviderError, StreamConfig, StreamEvent, StreamProvider, ToolDefinition,
};
use crate::types::*;
use std::sync::Arc;

/// Type alias for convert_to_llm callback.
pub type ConvertToLlmFn = Box<dyn Fn(&[AgentMessage]) -> Vec<Message> + Send + Sync>;
/// Type alias for transform_context callback.
pub type TransformContextFn = Box<dyn Fn(Vec<AgentMessage>) -> Vec<AgentMessage> + Send + Sync>;
/// Type alias for steering/follow-up message callbacks.
pub type GetMessagesFn = Box<dyn Fn() -> Vec<AgentMessage> + Send + Sync>;
/// Called before each LLM turn. Return `false` to abort the loop.
pub type BeforeTurnFn = Arc<dyn Fn(&[AgentMessage], usize) -> bool + Send + Sync>;
/// Called after each LLM turn with the current messages and the turn's usage.
pub type AfterTurnFn = Arc<dyn Fn(&[AgentMessage], &Usage) + Send + Sync>;
/// Called when the LLM returns a `StopReason::Error`.
pub type OnErrorFn = Arc<dyn Fn(&str) + Send + Sync>;
use tokio::sync::mpsc;
use tracing::warn;

/// Configuration for the agent loop.
///
/// Build one with [`AgentLoopConfig::new`], then set the fields you need: the
/// struct is `#[non_exhaustive]`, so new fields can be added in a minor release
/// without breaking callers.
///
/// ```
/// use std::sync::Arc;
/// use yoagent::agent_loop::AgentLoopConfig;
/// use yoagent::provider::MockProvider;
///
/// let mut config = AgentLoopConfig::new(Arc::new(MockProvider::text("hi")), "mock");
/// config.max_tokens = Some(1024);
/// ```
#[non_exhaustive]
pub struct AgentLoopConfig {
    pub provider: Arc<dyn StreamProvider>,
    pub model: String,
    pub api_key: String,
    pub thinking_level: ThinkingLevel,
    pub max_tokens: Option<u32>,
    pub temperature: Option<f32>,

    /// Optional model configuration for multi-provider support.
    /// When set, passed through to StreamConfig so providers can use
    /// base_url, headers, compat flags, etc.
    pub model_config: Option<ModelConfig>,

    /// Convert AgentMessage[] → Message[] before each LLM call.
    /// Default: keep only LLM-compatible messages.
    pub convert_to_llm: Option<ConvertToLlmFn>,

    /// Transform context before convert_to_llm (for pruning, compaction).
    pub transform_context: Option<TransformContextFn>,

    /// Get steering messages (user interruptions mid-run).
    pub get_steering_messages: Option<GetMessagesFn>,

    /// Get follow-up messages (queued work after agent finishes).
    pub get_follow_up_messages: Option<GetMessagesFn>,

    /// Context window configuration (auto-compaction).
    pub context_config: Option<ContextConfig>,

    /// Custom compaction strategy. When set, replaces the default
    /// `compact_messages()` call. Invoked when `context_config` is `Some`.
    pub compaction_strategy: Option<Arc<dyn CompactionStrategy>>,

    /// Execution limits (max turns, tokens, duration).
    pub execution_limits: Option<ExecutionLimits>,

    /// Prompt caching configuration.
    pub cache_config: CacheConfig,
    /// Where to stash the full text of a truncated tool result, so the model
    /// can retrieve what head-tail truncation elided.
    ///
    /// `None` (default) means truncation behaves exactly as before: the middle
    /// is gone and only the event stream, which the agent cannot read, still
    /// has it. Set it and the marker names a `shared_state` key instead.
    ///
    /// The pointer lives in the transcript, so it is lost when lossy compaction
    /// drops that turn — the stash entry outlives it and keeps consuming the
    /// backend's cap. Retrieval is best-effort by construction.
    pub tool_output_sink: Option<crate::shared_state::SharedState>,

    /// Tool execution strategy (sequential, parallel, or batched).
    pub tool_execution: ToolExecutionStrategy,

    /// Tool middleware chain — approve/deny/modify every tool call before it
    /// executes (see [`ToolMiddleware`]). Empty = allow all.
    pub tool_middleware: Vec<Arc<dyn ToolMiddleware>>,

    /// Structured-output constraint, passed through to the provider (see
    /// [`OutputSchema`](crate::provider::OutputSchema)). Usually set via
    /// [`Agent::prompt_structured`](crate::Agent::prompt_structured).
    pub output_schema: Option<crate::provider::OutputSchema>,

    /// Retry configuration for transient provider errors.
    pub retry_config: crate::retry::RetryConfig,

    /// Called before each LLM turn. Return `false` to abort the loop.
    pub before_turn: Option<BeforeTurnFn>,
    /// Called after each LLM turn with the current messages and the turn's usage.
    pub after_turn: Option<AfterTurnFn>,
    /// Called when the LLM returns a `StopReason::Error`.
    pub on_error: Option<OnErrorFn>,

    /// Input filters applied to user messages before the LLM call.
    /// Filters run in order; first `Reject` wins and discards any accumulated
    /// warnings. `Warn` messages accumulate and are appended to the user message.
    /// An [`AsyncFilter`] in the list is awaited (see [`InputFilter::as_async`]).
    pub input_filters: Vec<Arc<dyn InputFilter>>,

    /// Optional delay between turns. Useful for rate-limit-sensitive scenarios
    /// (e.g., OAuth tokens with low request-per-minute caps). Skipped on the
    /// first turn so the agent starts immediately.
    pub turn_delay: Option<std::time::Duration>,

    /// Extensions for this run (see [`crate::extension`]), in dispatch order
    /// after `tree_extensions`.
    pub extensions: Vec<Arc<dyn crate::extension::Extension>>,

    /// Extensions for this run and every run it delegates to, at any depth
    /// (host policy). They run before `extensions`.
    pub tree_extensions: Vec<Arc<dyn crate::extension::Extension>>,

    /// How many times per run an extension's `on_stop` may continue it.
    pub max_stop_continues: usize,

    /// The host's label for the run, passed to extensions as
    /// [`RunContext::label`](crate::extension::RunContext::label).
    pub run_label: Option<String>,

    /// Tree extensions a parent run passed down. They run first, and their
    /// tools are not offered.
    pub(crate) inherited_extensions: Vec<Arc<dyn crate::extension::Extension>>,

    /// 0 for a top-level run; a delegated run's depth.
    pub(crate) depth: usize,

    /// The tool call that started this run, for a delegated run.
    pub(crate) delegated_by: Option<String>,
    /// The calling run's extension run id, for a delegated run.
    pub(crate) parent_run_id: Option<String>,
}

impl AgentLoopConfig {
    /// A configuration for `model` on `provider`, with every other field at
    /// its default: no API key, thinking off, no limits beyond the provider's,
    /// no context management, no hooks, parallel tool execution and the
    /// default retry policy.
    pub fn new(provider: Arc<dyn StreamProvider>, model: impl Into<String>) -> Self {
        Self {
            provider,
            model: model.into(),
            api_key: String::new(),
            thinking_level: ThinkingLevel::Off,
            max_tokens: None,
            temperature: None,
            model_config: None,
            convert_to_llm: None,
            transform_context: None,
            get_steering_messages: None,
            get_follow_up_messages: None,
            context_config: None,
            compaction_strategy: None,
            execution_limits: None,
            cache_config: CacheConfig::default(),
            tool_output_sink: None,
            tool_execution: ToolExecutionStrategy::default(),
            tool_middleware: Vec::new(),
            output_schema: None,
            retry_config: crate::retry::RetryConfig::default(),
            before_turn: None,
            after_turn: None,
            on_error: None,
            input_filters: Vec::new(),
            turn_delay: None,
            extensions: Vec::new(),
            tree_extensions: Vec::new(),
            max_stop_continues: crate::extension::DEFAULT_MAX_STOP_CONTINUES,
            run_label: None,
            inherited_extensions: Vec::new(),
            depth: 0,
            delegated_by: None,
            parent_run_id: None,
        }
    }

    /// Whether any extension applies to this run.
    fn has_extensions(&self) -> bool {
        !(self.extensions.is_empty()
            && self.tree_extensions.is_empty()
            && self.inherited_extensions.is_empty())
    }

    /// Make this the configuration of a run delegated by the tool call
    /// `ctx` belongs to: the calling run's tree extensions apply here (ahead
    /// of this run's own, their tools not offered), at the delegation's
    /// depth, under the calling run's label. What a custom delegation tool
    /// calls before running its child; `SubAgentTool` does it itself.
    pub fn delegated_from(&mut self, ctx: &ToolContext) -> &mut Self {
        self.inherited_extensions = ctx.tree_extensions().to_vec();
        self.depth = ctx.delegation_depth();
        self.delegated_by = ctx.delegation.call_id.clone();
        self.parent_run_id = ctx.delegation.parent_run_id.clone();
        if self.run_label.is_none() {
            self.run_label = ctx.run_label().map(String::from);
        }
        self
    }

    /// The tree extensions a delegated run inherits: this run's inherited
    /// ones, then its own.
    pub(crate) fn tree_for_children(&self) -> Vec<Arc<dyn crate::extension::Extension>> {
        self.inherited_extensions
            .iter()
            .chain(&self.tree_extensions)
            .cloned()
            .collect()
    }
}

/// Default convert_to_llm: keep only user/assistant/toolResult messages.
fn default_convert_to_llm(messages: &[AgentMessage]) -> Vec<Message> {
    messages
        .iter()
        .filter_map(|m| m.as_llm().cloned())
        .collect()
}

/// Start an agent loop with new prompt messages.
/// Prefix of the message the loop appends when it stops a run itself.
///
/// A run stopped by a limit or by loop detection ends with a `Message::User`
/// carrying this prefix. The last *assistant* message still reports whatever
/// stop reason it had — `ToolUse` for a loop abort — so a consumer inspecting
/// only assistant messages cannot tell a stopped run from a finished one.
/// `SubAgentTool` uses this to avoid reporting an aborted delegation as a
/// bland success.
pub const AGENT_STOPPED_PREFIX: &str = "[Agent stopped:";

/// The stop marker for a run cancelled between provider calls — while tools
/// ran, or between turns. (A cancel during a provider call ends that turn's
/// message as `StopReason::Aborted` instead.) Unlike a limit, which leaves
/// real partial work, a cancelled run did not finish: `SubAgentTool` reports
/// it as a failure.
pub const CANCELLED_MARKER: &str = "[Agent stopped: cancelled]";

/// End a turn that stops before its model request, so its `TurnStart` is
/// paired. It made no assistant message and ran no tools: `TurnEnd` carries
/// the last message of the history (the stop marker, when there is one).
/// With an empty history (a run given no prompts at all) there is no message
/// to carry, and the turn is left open.
fn close_turn(tx: &mpsc::UnboundedSender<AgentEvent>, context: &AgentContext) {
    if let Some(last) = context.messages.last() {
        tx.send(AgentEvent::TurnEnd {
            message: last.clone(),
            tool_results: Vec::new(),
        })
        .ok();
    }
}

/// Append a stop marker as a user message: emitted, kept in context and in
/// the run's new messages.
fn push_stop_marker(
    text: String,
    tx: &mpsc::UnboundedSender<AgentEvent>,
    context: &mut AgentContext,
    new_messages: &mut Vec<AgentMessage>,
) {
    let marker = AgentMessage::Llm(Message::User {
        content: vec![Content::Text { text }],
        timestamp: now_ms(),
    });
    tx.send(AgentEvent::MessageStart {
        message: marker.clone(),
    })
    .ok();
    tx.send(AgentEvent::MessageEnd {
        message: marker.clone(),
    })
    .ok();
    context.messages.push(marker.clone());
    new_messages.push(marker);
}

/// Mark a run cancelled between provider calls — only when it did something,
/// so a run cancelled before it started leaves no trace.
fn mark_cancelled(
    tx: &mpsc::UnboundedSender<AgentEvent>,
    context: &mut AgentContext,
    new_messages: &mut Vec<AgentMessage>,
) {
    if new_messages
        .iter()
        .any(|m| matches!(m, AgentMessage::Llm(Message::Assistant { .. })))
    {
        push_stop_marker(CANCELLED_MARKER.to_string(), tx, context, new_messages);
    }
}

/// The stop marker for a run halted by loop detection specifically.
///
/// Distinct from the limit stops because the two mean opposite things to a
/// caller. Hitting `max_turns` is a bound: the work was cut short but what it
/// produced is real, and `SubAgentTool` returns it. A loop abort is a failure:
/// the model was emitting the same call forever and there is nothing to keep.
pub const LOOP_ABORT_PREFIX: &str = "[Agent stopped: repeated tool call —";

/// Prefix of the user-role nudge the loop injects when loop detection steers
/// a model that keeps repeating one call.
pub(crate) const LOOP_NUDGE_PREFIX: &str = "[You have called ";

/// Per-run state the loop shares with the hooks that run inside its task.
#[derive(Default)]
struct LoopScope {
    /// Decision-model spend, recorded by the decision integrations and
    /// folded into the run's `SessionStats`.
    decision: DecisionStats,
    /// The user messages this run was given — its prompts, steering and
    /// follow-ups — kept apart from the context so compaction cannot remove
    /// them. Exposed as `ToolCallRequest::run_prompts` /
    /// `TurnContext::run_prompts`.
    prompts: Vec<Message>,
}

tokio::task_local! {
    /// The running loop's [`LoopScope`]. Hooks run inside the loop's task; a
    /// nested sub-agent loop opens its own scope.
    static LOOP_SCOPE: std::cell::RefCell<LoopScope>;
}

/// Record decision-model spend into the enclosing loop's stats. A no-op
/// outside a loop.
#[cfg_attr(not(feature = "decision"), allow(dead_code))]
pub(crate) fn record_decision(f: impl FnOnce(&mut DecisionStats)) {
    let _ = LOOP_SCOPE.try_with(|cell| f(&mut cell.borrow_mut().decision));
}

/// The user messages the enclosing loop's run was given so far (empty
/// outside a loop).
pub(crate) fn run_prompts() -> Vec<Message> {
    LOOP_SCOPE
        .try_with(|cell| cell.borrow().prompts.clone())
        .unwrap_or_default()
}

/// The user-role LLM messages among `messages`.
fn user_messages(messages: &[AgentMessage]) -> Vec<Message> {
    messages
        .iter()
        .filter_map(|m| match m {
            AgentMessage::Llm(msg @ Message::User { .. }) => Some(msg.clone()),
            _ => None,
        })
        .collect()
}

/// Note messages handed to the run (steering, follow-ups) as run prompts.
fn note_run_prompts(messages: &[AgentMessage]) {
    let users = user_messages(messages);
    if !users.is_empty() {
        let _ = LOOP_SCOPE.try_with(|cell| cell.borrow_mut().prompts.extend(users));
    }
}

/// Run `fut` in a fresh [`LoopScope`] seeded with the run's prompts, and
/// return its output plus the decision spend recorded.
async fn with_loop_scope<T>(
    prompts: Vec<Message>,
    fut: impl std::future::Future<Output = T>,
) -> (T, DecisionStats) {
    let scope = LoopScope {
        prompts,
        ..Default::default()
    };
    LOOP_SCOPE
        .scope(std::cell::RefCell::new(scope), async {
            let out = fut.await;
            let recorded = LOOP_SCOPE.with(|cell| std::mem::take(&mut cell.borrow_mut().decision));
            (out, recorded)
        })
        .await
}

/// The error tool result given to each tool call in a response that ended as
/// [`StopReason::Refusal`] — the model declined, or a content filter stopped
/// the response. The call is never executed.
const REFUSAL_TOOL_RESULT_TEXT: &str = "Tool call not run: the response was stopped as a refusal \
     (declined by the model or stopped by the content filter).";

/// The error tool result given to a tool call whose run was already
/// cancelled when the call was reached. The call is never executed.
const CANCELLED_TOOL_RESULT_TEXT: &str = "Tool call not run: the run was cancelled.";

pub async fn agent_loop(
    prompts: Vec<AgentMessage>,
    context: &mut AgentContext,
    config: &AgentLoopConfig,
    tx: mpsc::UnboundedSender<AgentEvent>,
    cancel: tokio_util::sync::CancellationToken,
) -> Vec<AgentMessage> {
    agent_loop_with_stats(prompts, context, config, tx, cancel)
        .await
        .0
}

/// [`agent_loop`], also returning the [`SessionStats`] it sent on
/// `AgentEnd` — for callers inside the crate that must not depend on someone
/// draining the event channel (`Agent`'s sub-agent bucket, `SubAgentTool`'s
/// spend report).
pub(crate) async fn agent_loop_with_stats(
    prompts: Vec<AgentMessage>,
    context: &mut AgentContext,
    config: &AgentLoopConfig,
    tx: mpsc::UnboundedSender<AgentEvent>,
    cancel: tokio_util::sync::CancellationToken,
) -> (Vec<AgentMessage>, SessionStats) {
    let prompt_messages = user_messages(&prompts);
    let (exts, start_failure) = start_extensions(config, &prompt_messages, &cancel).await;
    let observer = EventObserver::new(&exts, tx);
    let tx = observer.sender();

    tx.send(AgentEvent::AgentStart).ok();

    // One scope for the whole run, input filters included, so decision
    // spend in an async filter is counted — also when it rejects.
    let (outcome, decision) = with_loop_scope(Vec::new(), async {
        // What the filters see, without the warnings they append.
        let text = if exts.is_empty() {
            String::new()
        } else {
            prompt_text(&prompts)
        };
        let prompts = apply_input_filters(prompts, config).await?;
        if !exts.is_empty() {
            exts.on_input(&crate::extension::InputContext::new(&text, &prompts))
                .await?;
        }
        note_run_prompts(&prompts);

        let mut new_messages: Vec<AgentMessage> = prompts.clone();

        // Add prompts to context
        for prompt in &prompts {
            context.messages.push(prompt.clone());
        }

        tx.send(AgentEvent::TurnStart).ok();

        // Emit events for each prompt message
        for prompt in &prompts {
            tx.send(AgentEvent::MessageStart {
                message: prompt.clone(),
            })
            .ok();
            tx.send(AgentEvent::MessageEnd {
                message: prompt.clone(),
            })
            .ok();
        }

        let stats = run_with_extensions(
            context,
            &mut new_messages,
            config,
            &tx,
            &cancel,
            &exts,
            start_failure,
        )
        .await;
        Ok::<_, String>((new_messages, stats))
    })
    .await;

    let (new_messages, mut stats) = match outcome {
        Ok(done) => done,
        Err(reason) => {
            tx.send(AgentEvent::InputRejected {
                reason: reason.clone(),
            })
            .ok();
            exts.finish(&crate::extension::RunOutcome::new(
                crate::extension::RunEnd::Rejected {
                    reason: reason.clone(),
                },
                None,
            ))
            .await;
            let stats = SessionStats {
                decision,
                ..Default::default()
            };
            tx.send(AgentEvent::AgentEnd {
                messages: vec![],
                stats: stats.clone(),
            })
            .ok();
            drop(tx);
            observer.finish().await;
            return (vec![], stats);
        }
    };
    stats.decision.merge(&decision);

    exts.finish(&run_outcome(&new_messages, &cancel)).await;
    tx.send(AgentEvent::AgentEnd {
        messages: new_messages.clone(),
        stats: stats.clone(),
    })
    .ok();
    drop(tx);
    observer.finish().await;
    (new_messages, stats)
}

/// Start the run's extensions. A required extension (or one that filters
/// tool output) that cannot start makes the run fail once it has begun; the
/// extensions that did start are returned with the failure, so they still
/// get `finish`.
async fn start_extensions(
    config: &AgentLoopConfig,
    prompts: &[Message],
    cancel: &tokio_util::sync::CancellationToken,
) -> (
    Arc<crate::extension::ActiveExtensions>,
    Option<crate::extension::Failure>,
) {
    use crate::extension::{ActiveExtensions, RunInfo};
    if !config.has_extensions() {
        return (Arc::new(ActiveExtensions::none()), None);
    }
    let info = RunInfo {
        label: config.run_label.as_deref(),
        prompts,
        depth: config.depth,
        delegated_by: config.delegated_by.as_deref(),
        parent_run_id: config.parent_run_id.as_deref(),
        cancel,
    };
    let own: Vec<_> = config
        .tree_extensions
        .iter()
        .chain(&config.extensions)
        .cloned()
        .collect();
    let (exts, failure) = ActiveExtensions::start(&config.inherited_extensions, &own, info).await;
    (Arc::new(exts), failure)
}

/// Run the loop with the run's extensions: offer their tools for this run
/// only, or fail the run when a required extension could not start.
#[allow(clippy::too_many_arguments)]
async fn run_with_extensions(
    context: &mut AgentContext,
    new_messages: &mut Vec<AgentMessage>,
    config: &AgentLoopConfig,
    tx: &mpsc::UnboundedSender<AgentEvent>,
    cancel: &tokio_util::sync::CancellationToken,
    exts: &crate::extension::ActiveExtensions,
    start_failure: Option<crate::extension::Failure>,
) -> SessionStats {
    if let Some(failure) = start_failure {
        fail_run(&failure, exts, config, tx, context, new_messages, true);
        return SessionStats::default();
    }
    let base_tools = context.tools.len();
    if !exts.is_empty() {
        let tools = exts.tools().await;
        if !tools.is_empty() {
            crate::tool_source::merge(&mut context.tools, tools);
        }
    }
    let stats = {
        use tracing::Instrument;
        run_loop(context, new_messages, config, tx, cancel, exts)
            .instrument(tracing::info_span!("agent_loop", model = %config.model))
            .await
    };
    // Extension tools belong to this run only.
    context.tools.truncate(base_tools);
    // However the loop ended (a limit, a cancel, a stop, a provider error), a
    // required extension's failure it did not act on still fails the run.
    if let Some(failure) = exts.settle().await {
        fail_run(&failure, exts, config, tx, context, new_messages, false);
    }
    stats
}

/// End the run because a required extension failed: an assistant error
/// message naming it, announced and appended, `on_error`, inside a turn. With
/// `in_turn`, the current turn is closed with it; otherwise it gets a turn of
/// its own, so every message stays between a `TurnStart` and a `TurnEnd`.
fn fail_run(
    failure: &crate::extension::Failure,
    exts: &crate::extension::ActiveExtensions,
    config: &AgentLoopConfig,
    tx: &mpsc::UnboundedSender<AgentEvent>,
    context: &mut AgentContext,
    new_messages: &mut Vec<AgentMessage>,
    in_turn: bool,
) {
    // The run fails once: a failure recorded while this one is reported
    // (an `on_event` that fails on these very events) is only logged.
    exts.latch_failed();
    let text = failure.message();
    tracing::error!("{text}");
    if !in_turn {
        tx.send(AgentEvent::TurnStart).ok();
    }
    let message = error_message(&config.model, text.clone());
    announce(tx, &message);
    let am: AgentMessage = message.into();
    context.messages.push(am.clone());
    new_messages.push(am.clone());
    if let Some(ref on_error) = config.on_error {
        on_error(&text);
    }
    tx.send(AgentEvent::TurnEnd {
        message: am,
        tool_results: Vec::new(),
    })
    .ok();
}

/// How a run ended, from its new messages.
fn run_outcome(
    new_messages: &[AgentMessage],
    cancel: &tokio_util::sync::CancellationToken,
) -> crate::extension::RunOutcome {
    use crate::extension::{failed_extension, RunEnd, RunOutcome};
    let stop_reason = new_messages.iter().rev().find_map(|m| match m {
        AgentMessage::Llm(Message::Assistant { stop_reason, .. }) => Some(stop_reason.clone()),
        _ => None,
    });
    let end = match new_messages.last() {
        // A stop marker ends the run: a limit, loop detection, a cancel
        // between turns, `on_before_turn`, an extension's `Stop`.
        Some(AgentMessage::Llm(Message::User { content, .. })) => match content.first() {
            Some(Content::Text { text }) if text == CANCELLED_MARKER => RunEnd::Cancelled,
            Some(Content::Text { text }) if text.starts_with(AGENT_STOPPED_PREFIX) => {
                RunEnd::Stopped {
                    reason: text.clone(),
                }
            }
            _ if cancel.is_cancelled() => RunEnd::Cancelled,
            _ => RunEnd::Completed,
        },
        Some(AgentMessage::Llm(Message::Assistant {
            stop_reason: StopReason::Aborted,
            ..
        })) => RunEnd::Cancelled,
        // The model finished (or stopped on its own): a cancel after that
        // changes nothing.
        Some(AgentMessage::Llm(Message::Assistant {
            stop_reason: StopReason::Stop | StopReason::Length | StopReason::Refusal,
            ..
        })) => RunEnd::Completed,
        Some(AgentMessage::Llm(Message::Assistant {
            stop_reason: StopReason::Error,
            error_message,
            ..
        })) => {
            let error = error_message.clone().unwrap_or_default();
            RunEnd::Failed {
                extension: failed_extension(&error),
                error,
            }
        }
        _ if cancel.is_cancelled() => RunEnd::Cancelled,
        _ => RunEnd::Completed,
    };
    RunOutcome::new(end, stop_reason)
}

/// Every event of a run with extensions passes through their `on_event`
/// before it reaches the run's consumer. Without extensions, the consumer's
/// sender is used as is. The loop waits on a flush before each failure check
/// and before `finish`, so what `on_event` recorded is current there.
/// Failures on the run's last events (`AgentEnd` itself) come too late to
/// change its outcome and are only logged. Events a stray sender clone sends
/// after the run ended are dropped.
struct EventObserver {
    tx: mpsc::UnboundedSender<AgentEvent>,
    exts: Arc<crate::extension::ActiveExtensions>,
    task: Option<(tokio::sync::oneshot::Sender<()>, crate::rt::JoinHandle<()>)>,
}

impl EventObserver {
    fn new(
        exts: &Arc<crate::extension::ActiveExtensions>,
        out: mpsc::UnboundedSender<AgentEvent>,
    ) -> Self {
        if exts.is_empty() {
            return Self {
                tx: out,
                exts: exts.clone(),
                task: None,
            };
        }
        let (tx, mut rx) = mpsc::unbounded_channel::<AgentEvent>();
        let (flush_tx, mut flush_rx) = mpsc::unbounded_channel::<crate::extension::FlushRequest>();
        exts.connect_observer(flush_tx);
        let (done_tx, mut done_rx) = tokio::sync::oneshot::channel::<()>();
        let observed = exts.clone();
        let task = crate::rt::spawn(async move {
            loop {
                tokio::select! {
                    biased;
                    event = rx.recv() => match event {
                        Some(event) => {
                            observed.on_event(&event).await;
                            out.send(event).ok();
                        }
                        None => break,
                    },
                    Some(ack) = flush_rx.recv() => {
                        // Every event sent before the request is queued.
                        while let Ok(event) = rx.try_recv() {
                            observed.on_event(&event).await;
                            out.send(event).ok();
                        }
                        let _ = ack.send(());
                    }
                    _ = &mut done_rx => {
                        // The run is over: deliver what is queued and stop,
                        // even if a stray sender clone is still alive.
                        while let Ok(event) = rx.try_recv() {
                            observed.on_event(&event).await;
                            out.send(event).ok();
                        }
                        break;
                    }
                }
            }
        });
        Self {
            tx,
            exts: exts.clone(),
            task: Some((done_tx, task)),
        }
    }

    fn sender(&self) -> mpsc::UnboundedSender<AgentEvent> {
        self.tx.clone()
    }

    /// Deliver every event sent so far, then stop observing.
    async fn finish(self) {
        let Self { tx, exts, task } = self;
        drop(tx);
        if let Some((done, task)) = task {
            let _ = done.send(());
            if let Err(e) = task.await {
                tracing::error!("extension event observer failed: {e}");
            }
        }
        if let Some(failure) = exts.take_failure() {
            tracing::error!(
                "{} (on the run's last events: too late to change its outcome)",
                failure.message()
            );
        }
    }
}

/// Every user text block of `prompts`, joined by newlines.
fn prompt_text(prompts: &[AgentMessage]) -> String {
    prompts
        .iter()
        .filter_map(|m| {
            if let AgentMessage::Llm(Message::User { content, .. }) = m {
                Some(
                    content
                        .iter()
                        .filter_map(|c| {
                            if let Content::Text { text } = c {
                                Some(text.as_str())
                            } else {
                                None
                            }
                        })
                        .collect::<Vec<_>>()
                        .join("\n"),
                )
            } else {
                None
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Run the input filters over the prompts: `Err(reason)` on the first
/// reject, otherwise the prompts with any warnings appended to the last
/// user message.
async fn apply_input_filters(
    prompts: Vec<AgentMessage>,
    config: &AgentLoopConfig,
) -> Result<Vec<AgentMessage>, String> {
    let prompts = if !config.input_filters.is_empty() {
        let user_text = prompt_text(&prompts);

        let mut warnings: Vec<String> = Vec::new();
        for filter in &config.input_filters {
            let verdict = match filter.as_async() {
                // A panicking async filter must not take the loop task (and
                // with it the agent's tools and history) down: contain it and
                // fail closed, as for middleware.
                Some(async_filter) => {
                    use futures::FutureExt;
                    std::panic::AssertUnwindSafe(async_filter.filter(&user_text))
                        .catch_unwind()
                        .await
                        .unwrap_or_else(|_| {
                            warn!("async input filter panicked; rejecting the input");
                            FilterResult::Reject("input filter panicked".into())
                        })
                }
                None => filter.filter(&user_text),
            };
            match verdict {
                FilterResult::Pass => {}
                FilterResult::Warn(w) => warnings.push(w),
                FilterResult::Reject(reason) => return Err(reason),
            }
        }

        // Append warnings to the last user message's content (avoids consecutive user messages)
        if !warnings.is_empty() {
            let warning_text = warnings
                .iter()
                .map(|w| format!("[Warning: {}]", w))
                .collect::<Vec<_>>()
                .join("\n");

            let mut modified = prompts;
            // Find and extend the last user message
            for msg in modified.iter_mut().rev() {
                if let AgentMessage::Llm(Message::User { content, .. }) = msg {
                    content.push(Content::Text { text: warning_text });
                    break;
                }
            }
            modified
        } else {
            prompts
        }
    } else {
        prompts
    };
    Ok(prompts)
}

/// Continue an agent loop from existing context (for retries).
pub async fn agent_loop_continue(
    context: &mut AgentContext,
    config: &AgentLoopConfig,
    tx: mpsc::UnboundedSender<AgentEvent>,
    cancel: tokio_util::sync::CancellationToken,
) -> Vec<AgentMessage> {
    agent_loop_continue_with_stats(context, config, tx, cancel)
        .await
        .0
}

/// [`agent_loop_continue`], also returning its [`SessionStats`].
pub(crate) async fn agent_loop_continue_with_stats(
    context: &mut AgentContext,
    config: &AgentLoopConfig,
    tx: mpsc::UnboundedSender<AgentEvent>,
    cancel: tokio_util::sync::CancellationToken,
) -> (Vec<AgentMessage>, SessionStats) {
    assert!(
        !context.messages.is_empty(),
        "Cannot continue: no messages in context"
    );

    if let Some(last) = context.messages.last() {
        assert!(
            last.role() != "assistant",
            "Cannot continue from assistant message"
        );
    }

    let (exts, start_failure) = start_extensions(config, &[], &cancel).await;
    let observer = EventObserver::new(&exts, tx);
    let tx = observer.sender();

    let mut new_messages: Vec<AgentMessage> = Vec::new();

    tx.send(AgentEvent::AgentStart).ok();
    tx.send(AgentEvent::TurnStart).ok();

    let stats = {
        let (mut stats, decision) = with_loop_scope(
            Vec::new(),
            run_with_extensions(
                context,
                &mut new_messages,
                config,
                &tx,
                &cancel,
                &exts,
                start_failure,
            ),
        )
        .await;
        stats.decision.merge(&decision);
        stats
    };

    exts.finish(&run_outcome(&new_messages, &cancel)).await;
    tx.send(AgentEvent::AgentEnd {
        messages: new_messages.clone(),
        stats: stats.clone(),
    })
    .ok();
    drop(tx);
    observer.finish().await;
    (new_messages, stats)
}

/// Main loop logic shared by agent_loop and agent_loop_continue.
///
/// Outer loop: continues when follow-up messages arrive after agent would stop.
/// Inner loop: process tool calls and steering messages.
async fn run_loop(
    context: &mut AgentContext,
    new_messages: &mut Vec<AgentMessage>,
    config: &AgentLoopConfig,
    tx: &mpsc::UnboundedSender<AgentEvent>,
    cancel: &tokio_util::sync::CancellationToken,
    exts: &crate::extension::ActiveExtensions,
) -> SessionStats {
    let mut stats = SessionStats::default();
    let mut first_turn = true;
    let mut turn_number: usize = 0;
    let mut stop_continues: usize = 0;
    // What the runs this one delegates to inherit.
    let delegation = Delegation {
        tree: config.tree_for_children(),
        depth: config.depth + 1,
        label: config.run_label.clone(),
        call_id: None,
        parent_run_id: (!exts.is_empty()).then(|| exts.run_id().to_string()),
    };
    // Rolling growth measurement feeding `compact_headroom_turns`.
    let mut last_context_tokens: Option<usize> = None;
    let mut growth_total: usize = 0;
    let mut growth_samples: usize = 0;
    // Blends real provider usage with estimation for compaction sizing.
    let mut context_tracker = ContextTracker::new();
    let mut tracker = config
        .execution_limits
        .as_ref()
        .map(|limits| ExecutionTracker::new(limits.clone()));

    // Check for steering messages at start
    let mut pending: Vec<AgentMessage> = config
        .get_steering_messages
        .as_ref()
        .map(|f| f())
        .unwrap_or_default();

    // Outer loop: follow-ups after agent would stop
    loop {
        if cancel.is_cancelled() {
            mark_cancelled(tx, context, new_messages);
            // The caller opened the first turn; nothing has closed it.
            if first_turn {
                close_turn(tx, context);
            }
            return stats;
        }

        let mut steering_after_tools: Option<Vec<AgentMessage>> = None;

        // Inner loop: runs at least once, then continues if tool calls or pending messages
        loop {
            if cancel.is_cancelled() {
                mark_cancelled(tx, context, new_messages);
                if first_turn {
                    close_turn(tx, context);
                }
                return stats;
            }

            if !first_turn {
                tx.send(AgentEvent::TurnStart).ok();
            } else {
                first_turn = false;
            }

            // Inject pending messages
            if !pending.is_empty() {
                note_run_prompts(&pending);
                for msg in pending.drain(..) {
                    tx.send(AgentEvent::MessageStart {
                        message: msg.clone(),
                    })
                    .ok();
                    tx.send(AgentEvent::MessageEnd {
                        message: msg.clone(),
                    })
                    .ok();
                    context.messages.push(msg.clone());
                    new_messages.push(msg);
                }
            }

            // A required extension that failed where it could not end the
            // run (its tools, after a tool, observing an event) ends it here,
            // ahead of a limit or `on_before_turn` that would otherwise end
            // it as a partial success.
            if let Some(failure) = exts.settle().await {
                fail_run(&failure, exts, config, tx, context, new_messages, true);
                return stats;
            }

            // Check execution limits
            if let Some(ref tracker) = tracker {
                if let Some(reason) = tracker.check_limits() {
                    warn!("Execution limit reached: {}", reason);
                    push_stop_marker(
                        format!("{AGENT_STOPPED_PREFIX} {}]", reason),
                        tx,
                        context,
                        new_messages,
                    );
                    close_turn(tx, context);
                    return stats;
                }
            }

            // before_turn callback — abort if it returns false
            if let Some(ref before_turn) = config.before_turn {
                if !before_turn(&context.messages, turn_number) {
                    close_turn(tx, context);
                    return stats;
                }
            }

            // Inter-turn delay — throttle API calls to stay under rate limits.
            // Skipped on the first turn so the agent starts immediately.
            if turn_number > 0 {
                if let Some(delay) = config.turn_delay {
                    crate::rt::sleep(delay).await;
                }
            }

            turn_number += 1;

            // Compact context if configured (tiered: tool outputs → summarize → drop).
            //
            // Calibration: the tracker's hybrid figure is anchored on provider
            // usage, which counts the FULL request (system prompt + tool
            // schemas + any estimation shortfall on messages), while
            // `total_tokens` counts messages only. The difference is a
            // measured overhead; subtracting it from the budget makes the
            // strategy's message-token checks equivalent to "true window
            // occupancy <= max_context_tokens". The static
            // `system_prompt_tokens` reserve is zeroed in the calibrated
            // config because the measured overhead already includes the real
            // system prompt. A floor of 10% of the configured budget
            // guarantees a mis-measured overhead can never wipe the history.
            if let Some(ref ctx_config) = config.context_config {
                let estimated = context::total_tokens(&context.messages);
                let hybrid = context_tracker.estimate_context_tokens(&context.messages);
                let overhead = hybrid.saturating_sub(estimated);

                // Growth since the last turn's compaction step, averaged. This
                // is what lets the headroom policy target an interval between
                // compactions instead of a fixed fraction of the budget: a
                // ratio cannot know how fast the session is growing, so the
                // room it leaves shrinks as history accumulates.
                if let Some(previous) = last_context_tokens {
                    growth_samples += 1;
                    growth_total += estimated.saturating_sub(previous);
                }
                let growth_per_turn = if growth_samples > 0 {
                    growth_total as f64 / growth_samples as f64
                } else {
                    0.0
                };

                let calibrated;
                let effective_config = if overhead > 0 {
                    let floor = ctx_config.max_context_tokens / 10;
                    calibrated = ContextConfig {
                        max_context_tokens: ctx_config
                            .max_context_tokens
                            .saturating_sub(overhead)
                            .max(floor),
                        system_prompt_tokens: 0,
                        ..ctx_config.clone()
                    };
                    tracing::debug!(
                        "compaction budget calibrated: {} -> {} (measured overhead: {} tokens)",
                        ctx_config.max_context_tokens,
                        calibrated.max_context_tokens,
                        overhead
                    );
                    &calibrated
                } else {
                    ctx_config
                };

                // Resolve the headroom policy against the calibrated budget.
                let with_headroom;
                let effective_config = {
                    let ratio = effective_config.effective_target_ratio(growth_per_turn);
                    if ratio != effective_config.compact_target_ratio {
                        tracing::debug!(
                            "compaction target adapted: ratio {} -> {:.3} \
                             ({} tokens/turn growth, {:?} turns of headroom)",
                            effective_config.compact_target_ratio,
                            ratio,
                            growth_per_turn as usize,
                            effective_config.compact_headroom_turns,
                        );
                        with_headroom = ContextConfig {
                            compact_target_ratio: ratio,
                            ..effective_config.clone()
                        };
                        &with_headroom
                    } else {
                        effective_config
                    }
                };

                let strategy: &dyn CompactionStrategy = config
                    .compaction_strategy
                    .as_deref()
                    .unwrap_or(&DefaultCompaction);
                let before_len = context.messages.len();
                // Already computed above over the same unmutated history.
                let before_tokens = estimated;
                context.messages =
                    strategy.compact(std::mem::take(&mut context.messages), effective_config);
                let after_tokens = context::total_tokens(&context.messages);
                // Length alone would miss level-1 truncation, which rewrites
                // tool outputs in place and leaves the count unchanged — and
                // that is exactly the case where the tracker's baseline goes
                // stale, so both the reset and the counter key off the same
                // test rather than the reset keying off length alone.
                if context.messages.len() != before_len || after_tokens != before_tokens {
                    // History moved; re-baseline from the next real usage.
                    context_tracker.reset();
                    stats.compactions += 1;
                }
                last_context_tokens = Some(after_tokens);
            }

            // Stream assistant response, under an llm_stream span that
            // records tokens and (when rates are configured) dollar cost.
            let llm_span = tracing::info_span!(
                "llm_stream",
                turn = turn_number,
                model = %config.model,
                tokens_in = tracing::field::Empty,
                tokens_out = tracing::field::Empty,
                tokens_cached = tracing::field::Empty,
                cost_usd = tracing::field::Empty,
                error = tracing::field::Empty,
            );
            let message = {
                use tracing::Instrument;
                stream_assistant_response(context, config, tx, cancel, exts)
                    .instrument(llm_span.clone())
                    .await
            };
            let message = match message {
                Ok(message) => message,
                // An extension's `before_model` ended the run before the
                // request was sent.
                Err(crate::extension::ModelHalt::Stop(reason)) => {
                    push_stop_marker(
                        format!("{AGENT_STOPPED_PREFIX} {reason}]"),
                        tx,
                        context,
                        new_messages,
                    );
                    close_turn(tx, context);
                    return stats;
                }
                Err(crate::extension::ModelHalt::Fail(failure)) => {
                    fail_run(&failure, exts, config, tx, context, new_messages, true);
                    return stats;
                }
            };
            if let Message::Assistant {
                usage, stop_reason, ..
            } = &message
            {
                llm_span.record("error", *stop_reason == StopReason::Error);
                llm_span.record("tokens_in", usage.input);
                llm_span.record("tokens_out", usage.output);
                llm_span.record("tokens_cached", usage.cache_read);
                // Unpriced models (`cost: None`) leave the field empty: unknown,
                // never $0. A free model (`Some`, all-zero) records 0.0.
                if let Some(cost) = config.model_config.as_ref().and_then(|mc| mc.cost.as_ref()) {
                    llm_span.record("cost_usd", cost.cost_usd(usage));
                }
            }
            // Tool-forcing providers (Anthropic) deliver structured output as
            // a forced tool call — unwrap it into plain text BEFORE tool-call
            // extraction, so the loop never tries to execute the synthetic tool.
            // Skipped on Anthropic's native path (`native_structured_output`):
            // no synthetic tool was offered there, so a call named after the
            // schema is a real user tool and must execute. A no-op for the
            // other natively-constraining providers (OpenAI-compat, Gemini)
            // unless a user tool shares the schema's name.
            let message = if structured_output_is_tool_forced(config.model_config.as_ref()) {
                unwrap_structured_tool_call(message, config.output_schema.as_ref())
            } else {
                message
            };

            let agent_msg: AgentMessage = message.clone().into();
            context.messages.push(agent_msg.clone());
            new_messages.push(agent_msg.clone());
            if let Message::Assistant { usage, .. } = &message {
                context_tracker.record_usage(usage, context.messages.len() - 1);
                stats.record_turn(
                    usage,
                    config.model_config.as_ref().and_then(|mc| mc.cost.as_ref()),
                );
            }

            // Check for error/abort
            if let Message::Assistant {
                ref stop_reason,
                ref error_message,
                ref usage,
                ..
            } = message
            {
                if *stop_reason == StopReason::Error || *stop_reason == StopReason::Aborted {
                    if *stop_reason == StopReason::Error {
                        if let Some(ref on_error) = config.on_error {
                            let err_str = error_message.as_deref().unwrap_or("Unknown error");
                            on_error(err_str);
                        }
                    }
                    // Call after_turn even on error/abort so callers tracking usage don't miss this turn
                    if let Some(ref after_turn) = config.after_turn {
                        after_turn(&context.messages, usage);
                    }
                    tx.send(AgentEvent::TurnEnd {
                        message: agent_msg,
                        tool_results: vec![],
                    })
                    .ok();
                    return stats;
                }
            }

            // A refusal (the model declined, or a content filter cut the
            // response) is terminal: nothing in it runs, and there is no
            // further LLM turn — the run ends as Error/Aborted end it, leaving
            // any queued steering/follow-up messages queued. A tool call in the
            // refused message may be complete and well-formed (a content filter
            // can land after it), but executing it would act on a response the
            // provider withdrew. Every call is still answered with an error
            // result so the transcript stays one the provider accepts on the
            // next prompt.
            if let Message::Assistant {
                stop_reason: StopReason::Refusal,
                ref content,
                ref usage,
                ..
            } = message
            {
                let mut tool_results: Vec<Message> = Vec::new();
                for c in content {
                    if let Content::ToolCall {
                        id,
                        name,
                        arguments,
                        ..
                    } = c
                    {
                        tracing::warn!(
                            tool = name.as_str(),
                            tool_call_id = id.as_str(),
                            "tool call not executed: the response was a refusal"
                        );
                        let (result, _) = unexecuted_tool_call(
                            id,
                            name,
                            arguments,
                            REFUSAL_TOOL_RESULT_TEXT.to_string(),
                            tx,
                        );
                        let am: AgentMessage = result.clone().into();
                        context.messages.push(am.clone());
                        new_messages.push(am);
                        tool_results.push(result);
                    }
                }
                if let Some(ref after_turn) = config.after_turn {
                    after_turn(&context.messages, usage);
                }
                tx.send(AgentEvent::TurnEnd {
                    message: agent_msg,
                    tool_results,
                })
                .ok();
                return stats;
            }

            // Extract tool calls
            let tool_calls: Vec<_> = match &message {
                Message::Assistant { content, .. } => content
                    .iter()
                    .filter_map(|c| match c {
                        Content::ToolCall {
                            id,
                            name,
                            arguments,
                            ..
                        } => Some((id.clone(), name.clone(), arguments.clone())),
                        _ => None,
                    })
                    .collect(),
                _ => vec![],
            };

            // A loop-detection nudge, held until the assistant's tool calls
            // have been answered. Injecting it before the `tool_result`s would
            // orphan them, which every provider rejects.
            let mut loop_nudge: Option<AgentMessage> = None;

            // Repetition check runs *before* execution, so a stuck model does
            // not also pay for the tool run it was never going to learn from.
            if !tool_calls.is_empty() {
                if let Some(ref mut tracker) = tracker {
                    let sigs: Vec<(String, serde_json::Value)> = tool_calls
                        .iter()
                        .map(|(_, name, args)| (name.clone(), args.clone()))
                        .collect();
                    match tracker.record_tool_calls(&sigs) {
                        context::LoopVerdict::Continue => {}
                        context::LoopVerdict::Steer {
                            tool_name,
                            repetitions,
                        } => {
                            warn!(
                                "loop detection: {tool_name} called {repetitions}x with identical arguments; steering"
                            );
                            tx.send(AgentEvent::LoopDetected {
                                tool_name: tool_name.clone(),
                                repetitions,
                                aborted: false,
                            })
                            .ok();
                            // A nudge, not a stop: a model repeating a call is
                            // often retrying something transient, and it
                            // usually recovers once told the result will not
                            // change.
                            let nudge = AgentMessage::Llm(Message::User {
                                content: vec![Content::Text {
                                    text: format!(
                                        "{LOOP_NUDGE_PREFIX}{tool_name} {repetitions} times with identical arguments. The result will not change — change approach, or say why the repetition is needed.]"
                                    ),
                                }],
                                timestamp: now_ms(),
                            });
                            // Deferred, not pushed here. The assistant's
                            // `tool_use` blocks are still unanswered at this
                            // point, and every provider rejects a transcript
                            // where anything but their `tool_result`s comes
                            // next. Appended after the results, below.
                            loop_nudge = Some(nudge);
                        }
                        context::LoopVerdict::Abort {
                            tool_name,
                            repetitions,
                        } => {
                            warn!("loop detection: {tool_name} repeated after steering; stopping");
                            tx.send(AgentEvent::LoopDetected {
                                tool_name: tool_name.clone(),
                                repetitions,
                                aborted: true,
                            })
                            .ok();
                            let stop = AgentMessage::Llm(Message::User {
                                content: vec![Content::Text {
                                    text: format!(
                                        "{LOOP_ABORT_PREFIX} {tool_name} was called repeatedly with identical \
                                         arguments after being asked to change approach.]"
                                    ),
                                }],
                                timestamp: now_ms(),
                            });
                            // The tools are deliberately not run — a stuck
                            // model should not also pay for a call it was
                            // never going to learn from. But the assistant's
                            // `tool_use` blocks must still be answered, or the
                            // transcript is one every provider rejects and the
                            // agent is unusable for any later prompt. Synthesize
                            // one error result per outstanding call.
                            for (id, name, _) in &tool_calls {
                                let denied = Message::ToolResult {
                                    tool_call_id: id.clone(),
                                    tool_name: name.clone(),
                                    content: vec![Content::Text {
                                        text: "Not executed: the run was stopped for repeating \
                                               this call with identical arguments."
                                            .into(),
                                    }],
                                    is_error: true,
                                    timestamp: now_ms(),
                                };
                                let am: AgentMessage = denied.into();
                                context.messages.push(am.clone());
                                new_messages.push(am);
                            }
                            tx.send(AgentEvent::MessageStart {
                                message: stop.clone(),
                            })
                            .ok();
                            tx.send(AgentEvent::MessageEnd {
                                message: stop.clone(),
                            })
                            .ok();
                            context.messages.push(stop.clone());
                            new_messages.push(stop);
                            // Match every other abort path in this function:
                            // callers tracking usage must not miss this turn,
                            // and a UI pairing TurnStart/TurnEnd must not be
                            // left with an unmatched TurnStart.
                            if let Some(after_turn) = config.after_turn.as_ref() {
                                let usage = match &message {
                                    Message::Assistant { usage, .. } => usage.clone(),
                                    _ => Usage::default(),
                                };
                                after_turn(&context.messages, &usage);
                            }
                            tx.send(AgentEvent::TurnEnd {
                                message: agent_msg,
                                tool_results: Vec::new(),
                            })
                            .ok();
                            return stats;
                        }
                    }
                }
            }

            let has_tool_calls = !tool_calls.is_empty();
            let mut tool_results: Vec<Message> = Vec::new();

            if has_tool_calls {
                // Extensions observe this turn's response before its tools
                // run, so a sub-agent started by one sees the spend so far
                // (a tree budget).
                exts.sync_events().await;
                let execution = execute_tool_calls(
                    &context.tools,
                    &tool_calls,
                    tx,
                    cancel,
                    config.get_steering_messages.as_ref(),
                    &config.tool_execution,
                    Gate {
                        middleware: &config.tool_middleware,
                        history: &context.messages,
                        extensions: exts,
                        delegation: &delegation,
                    },
                )
                .await;

                tool_results = execution.tool_results;
                steering_after_tools = execution.steering_messages;
                // Separate bucket: `usage`/`cost_usd` stay this agent's own.
                for child in &execution.sub_agent_stats {
                    stats.sub_agents.record_run(child);
                    // Decision spend has one bucket for the whole tree.
                    stats.decision.merge(&child.decision);
                }

                // Cap oversized output on the way in when configured, so
                // compaction never has to rewrite a tool result the provider
                // has already cached. Per-tool budgets apply, so a tool that
                // tolerates head+tail badly can opt out. The events emitted
                // above still carry the untruncated output — this is a context
                // concern only.
                let append_cap = config
                    .context_config
                    .as_ref()
                    .filter(|c| c.truncate_tool_output_on_append);

                for result in &tool_results {
                    let original: AgentMessage = result.clone().into();
                    let mut am = original.clone();
                    if let Some(ctx_config) = append_cap {
                        // First pass, unkeyed: tells us what the context will
                        // hold and which blocks carry a marker to name a key.
                        let (plain, marked) =
                            context::truncate_tool_output_keyed(original.clone(), ctx_config, None);
                        am = plain;

                        // Stash here and nowhere else. This is the one place
                        // the full text still exists — by the time compaction
                        // runs, the append-path truncation has already
                        // discarded the middle, so there would be nothing left
                        // to store.
                        //
                        // `marked` is the gate, not "did the text change": a
                        // budget too small for a marker still truncates, and
                        // stashing then would leave an entry no marker names
                        // and nothing can reach.
                        if let (Some(sink), false) = (&config.tool_output_sink, marked.is_empty()) {
                            if let Message::ToolResult { tool_call_id, .. } = result {
                                let blocks = context::block_texts(&original);
                                // One key per marked block. A single shared key
                                // made every marker resolve to all blocks
                                // concatenated, silently dropping any image
                                // between them, so what the model fetched was
                                // never the block whose marker it followed.
                                // Via the shared helper, so the hash input
                                // cannot drift from what `message_text`
                                // documents and silently move every key.
                                let base = context::tool_output_key(
                                    tool_call_id,
                                    &context::message_text(&original),
                                );
                                let mut all_stored = true;
                                let mut written: Vec<String> = Vec::new();
                                for idx in &marked {
                                    let Some((_, text)) = blocks.iter().find(|(i, _)| i == idx)
                                    else {
                                        // Unreachable — `marked` and `blocks`
                                        // both enumerate the same `original`.
                                        // Fail closed regardless: continuing
                                        // leaves `all_stored` true and emits a
                                        // marker for a block never stored,
                                        // which is the defect this feature
                                        // exists to prevent.
                                        debug_assert!(
                                            false,
                                            "marked block {idx} absent from block_texts"
                                        );
                                        all_stored = false;
                                        break;
                                    };
                                    let key = context::block_key(&base, *idx);
                                    match sink.set(&key, text.clone()).await {
                                        Ok(()) => written.push(key),
                                        Err(e) => {
                                            all_stored = false;
                                            warn!(
                                                tool_call_id = %tool_call_id,
                                                block = idx,
                                                bytes = text.len(),
                                                "could not stash tool output: {e}; no marker in \
                                                 this result will offer retrieval"
                                            );
                                            break;
                                        }
                                    }
                                }

                                // `Ok(())` per write is not proof the set
                                // survived. `FileBackend` evicts to fit and
                                // exempts only the key it just wrote, so a
                                // later sibling can evict an earlier one —
                                // and its (mtime, filename) ordering makes
                                // `-b0` lose to `-b2` deterministically. Naming
                                // a key that was deleted microseconds ago is
                                // exactly the marker-points-at-nothing failure
                                // the backend's own guard was added to stop.
                                if all_stored {
                                    for key in &written {
                                        if sink.get(key).await.is_none() {
                                            all_stored = false;
                                            warn!(
                                                tool_call_id = %tool_call_id,
                                                key = %key,
                                                stored = written.len(),
                                                "a stashed block did not survive storing its \
                                                 siblings — the backend cap cannot hold this \
                                                 result; no marker will offer retrieval"
                                            );
                                            break;
                                        }
                                    }
                                }

                                // Roll back whatever is left, keyed or not.
                                // Half a result in the store is unreachable
                                // bytes that still count against the cap, and
                                // on `FileBackend` they evict the caller's own
                                // artifacts to make room for debris.
                                if !all_stored {
                                    for key in &written {
                                        if !sink.remove(key).await {
                                            // `remove` already swallowed any backend error into
                                            // `false` + a warn. Surfacing it here too matters
                                            // because a failed rollback leaves a stashed value
                                            // no marker names, consuming cap quota for the rest
                                            // of the run.
                                            warn!("shared state: rollback could not remove {key}");
                                        }
                                    }
                                }
                                if all_stored {
                                    // Re-truncate from the original with the
                                    // base key, so each marker names its own
                                    // block only once every block is stored.
                                    am = context::truncate_tool_output_keyed(
                                        original.clone(),
                                        ctx_config,
                                        Some(&base),
                                    )
                                    .0;
                                }
                            }
                        }
                    }
                    context.messages.push(am.clone());
                    new_messages.push(am);
                }
            }

            // Track turn for execution limits
            if let Some(ref mut tracker) = tracker {
                let turn_tokens = match &message {
                    Message::Assistant { usage, .. } => {
                        (usage.input + usage.output + usage.cache_read + usage.cache_write) as usize
                    }
                    _ => context::message_tokens(&agent_msg),
                };
                tracker.record_turn(turn_tokens);
            }

            // after_turn callback
            if let Some(ref after_turn) = config.after_turn {
                let usage = match &message {
                    Message::Assistant { usage, .. } => usage.clone(),
                    _ => Usage::default(),
                };
                after_turn(&context.messages, &usage);
            }

            // Now that every `tool_result` is appended, the transcript is
            // well-formed again and the nudge can land.
            if let Some(nudge) = loop_nudge.take() {
                tx.send(AgentEvent::MessageStart {
                    message: nudge.clone(),
                })
                .ok();
                tx.send(AgentEvent::MessageEnd {
                    message: nudge.clone(),
                })
                .ok();
                context.messages.push(nudge.clone());
                new_messages.push(nudge);
            }

            tx.send(AgentEvent::TurnEnd {
                message: agent_msg,
                tool_results,
            })
            .ok();

            // Check steering after turn
            if let Some(steering) = steering_after_tools.take() {
                if !steering.is_empty() {
                    pending = steering;
                    continue;
                }
            }

            pending = config
                .get_steering_messages
                .as_ref()
                .map(|f| f())
                .unwrap_or_default();

            // Exit inner loop if no more tool calls and no pending messages
            if !has_tool_calls && pending.is_empty() {
                break;
            }
        }

        // Agent would stop. Check for follow-ups.
        let follow_ups = config
            .get_follow_up_messages
            .as_ref()
            .map(|f| f())
            .unwrap_or_default();

        if !follow_ups.is_empty() {
            pending = follow_ups;
            continue;
        }

        if !exts.is_empty() {
            if let Some(failure) = exts.settle().await {
                fail_run(&failure, exts, config, tx, context, new_messages, false);
                return stats;
            }
            if let Some(message) =
                check_stop(exts, config, tx, context, new_messages, &mut stop_continues).await
            {
                pending = vec![message];
                continue;
            }
            // A failure from `on_stop` ended the run there.
        }

        break;
    }

    stats
}

/// Run the extensions' `on_stop` over the model's answer. Returns the message
/// to continue the run with, or `None` to end it (accepted, or failed: the
/// failure is then already appended).
async fn check_stop(
    exts: &crate::extension::ActiveExtensions,
    config: &AgentLoopConfig,
    tx: &mpsc::UnboundedSender<AgentEvent>,
    context: &mut AgentContext,
    new_messages: &mut Vec<AgentMessage>,
    stop_continues: &mut usize,
) -> Option<AgentMessage> {
    use crate::extension::{Failure, StopContext, StopGate, EXTENSION_MESSAGE_PREFIX};
    // Only a finished answer is judged: not a cut-off (`Length`), a failure,
    // or a run that ended on a marker.
    let answer = match context.messages.last() {
        Some(AgentMessage::Llm(m @ Message::Assistant { stop_reason, .. }))
            if *stop_reason == StopReason::Stop =>
        {
            m.clone()
        }
        _ => return None,
    };
    let stop = StopContext::new(&answer, &context.messages, *stop_continues);
    match exts.on_stop(&stop).await {
        StopGate::Accept => None,
        StopGate::Fail(failure) => {
            fail_run(&failure, exts, config, tx, context, new_messages, false);
            None
        }
        StopGate::Continue { messages, required } => {
            if *stop_continues >= config.max_stop_continues {
                // A required extension that still has not accepted fails the
                // run, whichever extension's message would have been sent.
                if let Some(required) = required {
                    let failure = Failure {
                        name: required,
                        reason: format!(
                            "the answer was still not accepted after {} continues",
                            config.max_stop_continues
                        ),
                    };
                    fail_run(&failure, exts, config, tx, context, new_messages, false);
                } else {
                    let names: Vec<&str> = messages.iter().map(|(n, _)| n.as_str()).collect();
                    warn!(
                        extensions = ?names,
                        "extensions still ask to continue after max_stop_continues ({}); \
                         accepting the answer without their approval",
                        config.max_stop_continues
                    );
                }
                return None;
            }
            *stop_continues += 1;
            // Every extension that asked is heard: one line each.
            let text = messages
                .iter()
                .map(|(name, message)| format!("{EXTENSION_MESSAGE_PREFIX}{name}] {message}"))
                .collect::<Vec<_>>()
                .join("\n");
            Some(AgentMessage::Llm(Message::user(text)))
        }
    }
}

/// Stream an assistant response from the LLM.
async fn stream_assistant_response(
    context: &AgentContext,
    config: &AgentLoopConfig,
    tx: &mpsc::UnboundedSender<AgentEvent>,
    cancel: &tokio_util::sync::CancellationToken,
    exts: &crate::extension::ActiveExtensions,
) -> Result<Message, crate::extension::ModelHalt> {
    // Apply context transform
    let messages = if let Some(transform) = &config.transform_context {
        transform(context.messages.clone())
    } else {
        context.messages.clone()
    };

    // Convert to LLM messages
    let convert = config.convert_to_llm.as_ref();
    let llm_messages = match convert {
        Some(f) => f(&messages),
        None => default_convert_to_llm(&messages),
    };

    // Build tool definitions
    let tool_defs: Vec<ToolDefinition> = crate::extension::tool_definitions(&context.tools);

    // Extensions judge the request once per turn (a retried attempt is not
    // judged again). Their notes go on the latest user turn, before any turn
    // hook's (those run inside the provider wrapper, per attempt).
    let mut llm_messages = llm_messages;
    if !exts.is_empty() {
        let prompts = run_prompts();
        let turn = TurnContext::new(
            &context.system_prompt,
            &llm_messages,
            &tool_defs,
            &config.model,
        )
        .with_run_prompts(&prompts);
        match exts.before_model(&turn).await {
            Ok(notes) => {
                if !notes.is_empty() {
                    let latest_user = llm_messages.iter_mut().rev().find_map(|m| match m {
                        Message::User { content, .. } => Some(content),
                        _ => None,
                    });
                    match latest_user {
                        Some(content) => content.push(Content::Text {
                            text: notes.join("\n"),
                        }),
                        None => {
                            tracing::debug!("extension note dropped: the request has no user turn")
                        }
                    }
                }
            }
            Err(halt) => return Err(halt),
        }
    }

    // Retry loop for transient provider errors
    let retry = &config.retry_config;
    let mut attempt = 0;
    let result = loop {
        let stream_config = StreamConfig {
            model: config.model.clone(),
            system_prompt: context.system_prompt.clone(),
            messages: llm_messages.clone(),
            tools: tool_defs.clone(),
            thinking_level: config.thinking_level,
            api_key: config.api_key.clone(),
            max_tokens: config.max_tokens,
            temperature: config.temperature,
            model_config: config.model_config.clone(),
            cache_config: config.cache_config.clone(),
            output_schema: config.output_schema.clone(),
        };

        let (stream_tx, mut stream_rx) = mpsc::unbounded_channel();
        let provider_cancel = cancel.clone();

        // Spawn a task to forward events in real-time as the provider streams
        let event_tx = tx.clone();
        let model_for_events = config.model.clone();
        let forward_handle = crate::rt::spawn(async move {
            let mut partial_message: Option<AgentMessage> = None;
            let mut ended = false;
            while let Some(event) = stream_rx.recv().await {
                match &event {
                    StreamEvent::Start => {
                        let placeholder = AgentMessage::Llm(Message::Assistant {
                            content: Vec::new(),
                            stop_reason: StopReason::Stop,
                            model: model_for_events.clone(),
                            provider: String::new(),
                            usage: Usage::default(),
                            timestamp: now_ms(),
                            error_message: None,
                        });
                        partial_message = Some(placeholder.clone());
                        event_tx
                            .send(AgentEvent::MessageStart {
                                message: placeholder,
                            })
                            .ok();
                    }
                    StreamEvent::TextDelta { delta, .. } => {
                        if let Some(ref msg) = partial_message {
                            event_tx
                                .send(AgentEvent::MessageUpdate {
                                    message: msg.clone(),
                                    delta: StreamDelta::Text {
                                        delta: delta.clone(),
                                    },
                                })
                                .ok();
                        }
                    }
                    StreamEvent::ThinkingDelta { delta, .. } => {
                        if let Some(ref msg) = partial_message {
                            event_tx
                                .send(AgentEvent::MessageUpdate {
                                    message: msg.clone(),
                                    delta: StreamDelta::Thinking {
                                        delta: delta.clone(),
                                    },
                                })
                                .ok();
                        }
                    }
                    StreamEvent::ToolCallDelta { delta, .. } => {
                        if let Some(ref msg) = partial_message {
                            event_tx
                                .send(AgentEvent::MessageUpdate {
                                    message: msg.clone(),
                                    delta: StreamDelta::ToolCallDelta {
                                        delta: delta.clone(),
                                    },
                                })
                                .ok();
                        }
                    }
                    StreamEvent::Done { message } => {
                        let am: AgentMessage = message.clone().into();
                        // A provider that sends `Done` without `Start` still
                        // gets its message announced, as `Error` below does.
                        if partial_message.is_none() {
                            event_tx
                                .send(AgentEvent::MessageStart {
                                    message: am.clone(),
                                })
                                .ok();
                        }
                        partial_message = Some(am.clone());
                        ended = true;
                        event_tx.send(AgentEvent::MessageEnd { message: am }).ok();
                    }
                    StreamEvent::Error { message } => {
                        let am: AgentMessage = message.clone().into();
                        if partial_message.is_none() {
                            event_tx
                                .send(AgentEvent::MessageStart {
                                    message: am.clone(),
                                })
                                .ok();
                        }
                        partial_message = Some(am.clone());
                        ended = true;
                        event_tx.send(AgentEvent::MessageEnd { message: am }).ok();
                    }
                    _ => {}
                }
            }
            AttemptEvents {
                started: partial_message.is_some(),
                ended,
            }
        });

        // Provider streams concurrently — events are forwarded in real-time
        // When provider returns, stream_tx is dropped, ending the forwarder
        let result = config
            .provider
            .stream(stream_config, stream_tx, provider_cancel)
            .await;

        // The provider has returned, so its sender is gone and the forwarder
        // ends once it has forwarded what the attempt produced. Draining it
        // (rather than aborting it) makes what consumers see deterministic:
        // every event of every attempt, never a timing-dependent subset.
        let attempt_events = match forward_handle.await {
            Ok(events) => events,
            Err(e) => {
                warn!("event forwarder failed; this attempt's events may be incomplete: {e}");
                // Read as "sent nothing", so the turn's message is announced
                // again. That could repeat a `MessageStart` the forwarder sent
                // before failing; it only forwards, so this is not expected
                // in practice.
                AttemptEvents::default()
            }
        };

        match &result {
            Err(e) if e.is_retryable() && attempt < retry.max_retries && !cancel.is_cancelled() => {
                // Close the failed attempt's message so consumers can discard
                // whatever partial content it streamed before the retry. The
                // `ProviderRetry` that follows marks it as retried, not final.
                if attempt_events.needs_end() {
                    let failed = error_message(
                        &config.model,
                        format!(
                            "attempt {} of {} failed and will be retried: {e}",
                            attempt + 1,
                            retry.max_retries + 1
                        ),
                    );
                    tx.send(AgentEvent::MessageEnd {
                        message: failed.into(),
                    })
                    .ok();
                }
                attempt += 1;
                // Server-provided Retry-After wins over backoff, but is
                // clamped to max_delay_ms so a bad header can't stall the loop.
                let delay = e
                    .retry_after()
                    .map(|d| d.min(std::time::Duration::from_millis(retry.max_delay_ms)))
                    .unwrap_or_else(|| retry.delay_for_attempt(attempt));
                crate::retry::log_retry(attempt, retry.max_retries, &delay, e);
                tx.send(AgentEvent::provider_retry(
                    attempt,
                    retry.max_retries + 1,
                    e.to_string(),
                    delay,
                ))
                .ok();
                // Cancelling during the backoff ends the turn now rather than
                // after the delay (and a wasted request).
                let backoff = std::pin::pin!(crate::rt::sleep(delay));
                let cancelled = std::pin::pin!(cancel.cancelled());
                if let futures::future::Either::Right(_) =
                    futures::future::select(backoff, cancelled).await
                {
                    break (Err(ProviderError::Cancelled), AttemptEvents::default());
                }
                continue;
            }
            _ => break (result, attempt_events),
        }
    };

    let (result, attempt_events) = result;
    match result {
        Ok(msg) => {
            // A provider that returns without sending `Done` would leave its
            // message open; close it with the message it returned. One that
            // sent nothing at all gets the whole pair.
            if attempt_events.is_silent() {
                announce(tx, &msg);
            } else if attempt_events.needs_end() {
                tx.send(AgentEvent::MessageEnd {
                    message: msg.clone().into(),
                })
                .ok();
            }
            Ok(msg)
        }
        Err(e) => {
            warn!("Provider error: {}", e);
            let mut failed = error_message(&config.model, e.to_string());
            // A cancelled run is aborted, not failed: it does not reach
            // `on_error`, and consumers can tell the two apart.
            if matches!(e, ProviderError::Cancelled) || cancel.is_cancelled() {
                if let Message::Assistant { stop_reason, .. } = &mut failed {
                    *stop_reason = StopReason::Aborted;
                }
            }
            // Close the message the final attempt opened, with the same
            // error message this turn returns. An attempt that failed before
            // any output opened nothing: announce the message anyway, since it
            // is appended to the history and consumers that rebuild the
            // transcript from events must see it.
            if attempt_events.is_silent() {
                announce(tx, &failed);
            } else if attempt_events.needs_end() {
                tx.send(AgentEvent::MessageEnd {
                    message: failed.clone().into(),
                })
                .ok();
            }
            Ok(failed)
        }
    }
}

/// Send `MessageStart` and `MessageEnd` for a message no stream announced.
fn announce(tx: &mpsc::UnboundedSender<AgentEvent>, message: &Message) {
    tx.send(AgentEvent::MessageStart {
        message: message.clone().into(),
    })
    .ok();
    tx.send(AgentEvent::MessageEnd {
        message: message.clone().into(),
    })
    .ok();
}

/// What one provider attempt's forwarder emitted.
#[derive(Debug, Default, Clone, Copy)]
struct AttemptEvents {
    /// A `MessageStart` was sent for this attempt.
    started: bool,
    /// Its `MessageEnd` was sent too.
    ended: bool,
}

impl AttemptEvents {
    /// The attempt opened a message that nothing closed.
    fn needs_end(self) -> bool {
        self.started && !self.ended
    }

    /// The attempt sent no message event at all.
    fn is_silent(self) -> bool {
        !self.started && !self.ended
    }
}

/// The assistant message for a provider failure.
fn error_message(model: &str, error: String) -> Message {
    Message::Assistant {
        content: vec![Content::Text {
            text: String::new(),
        }],
        stop_reason: StopReason::Error,
        model: model.to_string(),
        provider: "unknown".into(),
        usage: Usage::default(),
        timestamp: now_ms(),
        error_message: Some(error),
    }
}

// ---------------------------------------------------------------------------
// Tool execution
// ---------------------------------------------------------------------------

struct ToolExecutionResult {
    tool_results: Vec<Message>,
    steering_messages: Option<Vec<AgentMessage>>,
    /// Stats of every sub-agent run these tool calls delegated to, failed
    /// ones included, for the run's `SessionStats::sub_agents`.
    sub_agent_stats: Vec<SessionStats>,
}

/// Whether a structured-output request may have been enforced by forcing a
/// synthetic tool call, which the loop must unwrap. False only on Anthropic's
/// native path (`AnthropicCompat::native_structured_output`), where the API
/// constrains the reply text and offers no synthetic tool. Without a model
/// config the provider falls back to its defaults (tool-forcing on Anthropic),
/// so unwrapping stays on.
fn structured_output_is_tool_forced(model_config: Option<&ModelConfig>) -> bool {
    !model_config.is_some_and(|mc| {
        mc.api == crate::provider::ApiProtocol::AnthropicMessages
            && mc
                .anthropic
                .as_ref()
                .is_some_and(|c| c.native_structured_output)
    })
}

/// Convert a forced structured-output tool call back into a plain-text
/// assistant message with `StopReason::Stop`. No-op unless a schema is set
/// and the message carries a tool call named after it.
fn unwrap_structured_tool_call(
    message: Message,
    schema: Option<&crate::provider::OutputSchema>,
) -> Message {
    let Some(schema) = schema else {
        return message;
    };
    let Message::Assistant {
        content,
        model,
        provider,
        usage,
        stop_reason,
        error_message,
        ..
    } = &message
    else {
        return message;
    };
    let Some(payload) = content.iter().find_map(|c| match c {
        Content::ToolCall {
            name, arguments, ..
        } if *name == schema.name => Some(arguments.clone()),
        _ => None,
    }) else {
        return message;
    };

    // Remove ONLY the synthetic call; any real tool calls (shouldn't occur
    // under forced tool_choice, but defensively) stay and execute normally.
    // The payload is appended AFTER any preamble text — consumers take the
    // last text block.
    let mut new_content: Vec<Content> = content
        .iter()
        .filter(|c| !matches!(c, Content::ToolCall { name, .. } if *name == schema.name))
        .cloned()
        .collect();
    new_content.push(Content::Text {
        text: payload.to_string(),
    });
    // ToolUse becomes Stop (the forced call was the "answer"); every other
    // stop reason (Length = truncated payload, Error, ...) is preserved so
    // truncation isn't laundered into success.
    let new_stop = if *stop_reason == StopReason::ToolUse {
        StopReason::Stop
    } else {
        stop_reason.clone()
    };
    let rebuilt = Message::assistant(
        new_content,
        new_stop,
        model.clone(),
        provider.clone(),
        usage.clone(),
    );
    // `Message::assistant` nulls `error_message`, so carry it across explicitly.
    // The stop reason is preserved a few lines up for the same reason; dropping
    // the explanation with it leaves the caller a bare `Error` and
    // `StructuredPromptError::Provider { message: "provider error (no detail)" }`.
    match error_message {
        Some(msg) => rebuilt.with_error_message(msg.clone()),
        None => rebuilt,
    }
}

/// What gates a tool call: the middleware chain, and the conversation it may
/// consult ([`ToolCallRequest::messages`]).
#[derive(Clone, Copy)]
struct Gate<'a> {
    middleware: &'a [Arc<dyn ToolMiddleware>],
    history: &'a [AgentMessage],
    /// The run's extensions (`before_tool` / `after_tool`).
    extensions: &'a crate::extension::ActiveExtensions,
    /// What a run this call delegates to inherits.
    delegation: &'a Delegation,
}

async fn execute_tool_calls(
    tools: &[Box<dyn AgentTool>],
    tool_calls: &[(String, String, serde_json::Value)],
    tx: &mpsc::UnboundedSender<AgentEvent>,
    cancel: &tokio_util::sync::CancellationToken,
    get_steering: Option<&GetMessagesFn>,
    strategy: &ToolExecutionStrategy,
    gate: Gate<'_>,
) -> ToolExecutionResult {
    match strategy {
        ToolExecutionStrategy::Sequential => {
            execute_sequential(tools, tool_calls, tx, cancel, get_steering, gate).await
        }
        ToolExecutionStrategy::Parallel => {
            execute_batch(tools, tool_calls, tx, cancel, get_steering, gate).await
        }
        ToolExecutionStrategy::Batched { size } => {
            let mut results: Vec<Message> = Vec::new();
            let mut steering_messages: Option<Vec<AgentMessage>> = None;
            let mut sub_agent_stats: Vec<SessionStats> = Vec::new();

            // `chunks(0)` panics, and `size` comes from config: treat 0 as 1.
            for (batch_idx, batch) in tool_calls.chunks((*size).max(1)).enumerate() {
                let batch_result = execute_batch(tools, batch, tx, cancel, None, gate).await;
                results.extend(batch_result.tool_results);
                sub_agent_stats.extend(batch_result.sub_agent_stats);

                // Check steering between batches
                if let Some(get_steering_fn) = get_steering {
                    let steering = get_steering_fn();
                    if !steering.is_empty() {
                        steering_messages = Some(steering);
                        // Skip remaining batches
                        let executed = (batch_idx + 1) * *size;
                        if executed < tool_calls.len() {
                            for (skip_id, skip_name, _) in &tool_calls[executed..] {
                                results.push(skip_tool_call(skip_id, skip_name, tx));
                            }
                        }
                        break;
                    }
                }
            }

            ToolExecutionResult {
                tool_results: results,
                steering_messages,
                sub_agent_stats,
            }
        }
    }
}

/// Execute tool calls one at a time, checking steering between each.
async fn execute_sequential(
    tools: &[Box<dyn AgentTool>],
    tool_calls: &[(String, String, serde_json::Value)],
    tx: &mpsc::UnboundedSender<AgentEvent>,
    cancel: &tokio_util::sync::CancellationToken,
    get_steering: Option<&GetMessagesFn>,
    gate: Gate<'_>,
) -> ToolExecutionResult {
    let mut results: Vec<Message> = Vec::new();
    let mut steering_messages: Option<Vec<AgentMessage>> = None;
    let mut sub_agent_stats: Vec<SessionStats> = Vec::new();

    for (index, (id, name, args)) in tool_calls.iter().enumerate() {
        let (result_msg, delegated) =
            execute_single_tool(tools, id, name, args, tx, cancel, gate).await;
        results.push(result_msg);
        sub_agent_stats.extend(delegated);

        // Check for steering — skip remaining tools if user interrupted
        if let Some(get_steering_fn) = get_steering {
            let steering = get_steering_fn();
            if !steering.is_empty() {
                steering_messages = Some(steering);
                for (skip_id, skip_name, _) in &tool_calls[index + 1..] {
                    results.push(skip_tool_call(skip_id, skip_name, tx));
                }
                break;
            }
        }
    }

    ToolExecutionResult {
        tool_results: results,
        steering_messages,
        sub_agent_stats,
    }
}

/// Execute a batch of tool calls concurrently using futures::join_all.
async fn execute_batch(
    tools: &[Box<dyn AgentTool>],
    tool_calls: &[(String, String, serde_json::Value)],
    tx: &mpsc::UnboundedSender<AgentEvent>,
    cancel: &tokio_util::sync::CancellationToken,
    get_steering: Option<&GetMessagesFn>,
    gate: Gate<'_>,
) -> ToolExecutionResult {
    use futures::future::join_all;

    let futures: Vec<_> = tool_calls
        .iter()
        .map(|(id, name, args)| execute_single_tool(tools, id, name, args, tx, cancel, gate))
        .collect();

    let batch_results = join_all(futures).await;

    let mut sub_agent_stats: Vec<SessionStats> = Vec::new();
    let results: Vec<Message> = batch_results
        .into_iter()
        .map(|(msg, delegated)| {
            sub_agent_stats.extend(delegated);
            msg
        })
        .collect();

    // Check steering after batch completes
    let steering_messages = if let Some(get_steering_fn) = get_steering {
        let steering = get_steering_fn();
        if steering.is_empty() {
            None
        } else {
            Some(steering)
        }
    } else {
        None
    };

    ToolExecutionResult {
        tool_results: results,
        steering_messages,
        sub_agent_stats,
    }
}

/// Execute a single tool call and emit events.
///
/// Also returns the stats of any sub-agent runs the tool reported, so the
/// caller can fold delegated spend into the run's rollup.
async fn execute_single_tool(
    tools: &[Box<dyn AgentTool>],
    id: &str,
    name: &str,
    args: &serde_json::Value,
    tx: &mpsc::UnboundedSender<AgentEvent>,
    cancel: &tokio_util::sync::CancellationToken,
    gate: Gate<'_>,
) -> (Message, Vec<SessionStats>) {
    // A call whose streamed arguments did not resolve to a JSON object (cut
    // off at the output token limit, in practice, or double-encoded as a
    // string) is answered with an error, never run: the provider kept the
    // raw text instead of substituting `{}`, and running the tool on its
    // defaults would silently replace what the model asked for. This sits
    // ahead of middleware — there is no real call to approve or rewrite.
    if let Some(raw) = crate::provider::unparsed_tool_arguments(args) {
        let (msg, _) = unparsed_arguments_tool_call(id, name, args, raw, tx);
        return (msg, Vec::new());
    }

    // A cancelled run starts no new tool calls: the user stopped it, and a
    // call the model asked for before the cancel must not still act. The call
    // is answered (the transcript stays valid) and the run ends at the top of
    // the next turn with the cancel marker. A tool already running sees the
    // cancel through its `ToolContext` token instead.
    if cancel.is_cancelled() {
        return (cancelled_tool_call(id, name, args, tx), Vec::new());
    }
    // Nor does a run a required extension has failed: it ends at the next
    // turn boundary.
    if gate.extensions.has_failure() {
        return (failed_run_tool_call(id, name, args, tx), Vec::new());
    }

    // Middleware chain runs next: each hook may rewrite the args seen by
    // later hooks; the first Deny short-circuits into an error tool result
    // (the LLM sees the reason and can adapt — the loop continues).
    let mut effective_args = args.clone();
    let prompts = if gate.middleware.is_empty() && gate.extensions.is_empty() {
        Vec::new()
    } else {
        run_prompts()
    };
    for mw in gate.middleware {
        let call = ToolCallRequest {
            tool_call_id: id,
            tool_name: name,
            args: &effective_args,
            messages: gate.history,
            run_prompts: &prompts,
        };
        // A panicking middleware must not kill the loop task (which would
        // strip the agent of its tools) — contain it and fail closed.
        let decision = {
            use futures::FutureExt;
            std::panic::AssertUnwindSafe(mw.before_tool(&call))
                .catch_unwind()
                .await
                .unwrap_or_else(|_| {
                    tracing::warn!(
                        tool = name,
                        tool_call_id = id,
                        "tool middleware panicked; denying the call"
                    );
                    ToolDecision::Deny("tool middleware panicked".into())
                })
        };
        match decision {
            ToolDecision::Allow => {}
            ToolDecision::Modify(new_args) => effective_args = new_args,
            ToolDecision::Deny(reason) => {
                let (msg, _) = denied_tool_call(id, name, &effective_args, &reason, tx);
                return (msg, Vec::new());
            }
        }
    }

    // Then the extensions' `before_tool`, after the legacy chain.
    if !gate.extensions.is_empty() {
        let call = ToolCallRequest {
            tool_call_id: id,
            tool_name: name,
            args: &effective_args,
            messages: gate.history,
            run_prompts: &prompts,
        };
        match gate.extensions.before_tool(call).await {
            Ok(final_args) => effective_args = final_args,
            Err(reason) => {
                let (msg, _) = denied_tool_call(id, name, &effective_args, &reason, tx);
                return (msg, Vec::new());
            }
        }
    }
    let args = &effective_args;

    // Middleware may await (an approval, a classifier), so the run can be
    // cancelled, or failed, while it decides.
    if cancel.is_cancelled() {
        return (cancelled_tool_call(id, name, args, tx), Vec::new());
    }
    if gate.extensions.has_failure() {
        return (failed_run_tool_call(id, name, args, tx), Vec::new());
    }

    let tool = tools.iter().find(|t| t.name() == name);

    // The Start event carries the effective (post-middleware) args — what
    // actually runs.
    tx.send(AgentEvent::ToolExecutionStart {
        tool_call_id: id.to_string(),
        tool_name: name.to_string(),
        args: args.clone(),
    })
    .ok();

    // An extension that filters tool output sees only the final result, so
    // partial output (sent while the tool runs) is withheld.
    let withhold = gate.extensions.filters_tool_output();
    let on_update: Option<ToolUpdateFn> = if withhold {
        None
    } else {
        let tx = tx.clone();
        let id = id.to_string();
        let name = name.to_string();
        Some(Arc::new(move |partial: ToolResult| {
            tx.send(AgentEvent::ToolExecutionUpdate {
                tool_call_id: id.clone(),
                tool_name: name.clone(),
                partial_result: partial,
            })
            .ok();
        }))
    };

    let on_progress: Option<ProgressFn> = if withhold {
        None
    } else {
        let tx = tx.clone();
        let id = id.to_string();
        let name = name.to_string();
        Some(Arc::new(move |text: String| {
            tx.send(AgentEvent::ProgressMessage {
                tool_call_id: id.clone(),
                tool_name: name.clone(),
                text,
            })
            .ok();
        }))
    };

    let sub_agent_report: SubAgentReport = Arc::default();
    let ctx = ToolContext {
        tool_call_id: id.to_string(),
        tool_name: name.to_string(),
        cancel: cancel.child_token(),
        on_update,
        on_progress,
        sub_agent_report: Some(sub_agent_report.clone()),
        delegation: Delegation {
            call_id: Some(id.to_string()),
            ..gate.delegation.clone()
        },
    };

    let tool_span = tracing::info_span!(
        "tool",
        tool = %name,
        tool_call_id = %id,
        is_error = tracing::field::Empty,
    );
    use tracing::Instrument;
    let (result, is_error) = match tool {
        Some(tool) => {
            // A panicking tool must not kill the loop task (which would strip
            // the agent of its tools and lose the run) — contain it and report
            // it to the model as a failed call.
            // The call is built inside the guarded block, so a panic while
            // creating the future (a hand-written `execute`) is caught too.
            let execution = {
                use futures::FutureExt;
                std::panic::AssertUnwindSafe(async { tool.execute(args.clone(), ctx).await })
                    .catch_unwind()
                    .instrument(tool_span.clone())
                    .await
                    .unwrap_or_else(|payload| {
                        let why = crate::tool_source::panic_message(&*payload);
                        tracing::error!(tool = name, tool_call_id = id, panic = %why, "tool panicked");
                        Err(ToolError::Failed(format!("tool '{name}' panicked: {why}")))
                    })
            };
            match execution {
                Ok(r) => (r, false),
                Err(e) => (
                    ToolResult {
                        content: vec![Content::Text {
                            text: e.to_string(),
                        }],
                        details: serde_json::Value::Null,
                    },
                    true,
                ),
            }
        }
        None => (
            ToolResult {
                content: vec![Content::Text {
                    text: format!("Tool {} not found", name),
                }],
                details: serde_json::Value::Null,
            },
            true,
        ),
    };

    tool_span.record("is_error", is_error);

    // Delegated spend travels out of band, so it is here even when the
    // sub-agent failed and the tool returned `Err`. Attach it to the result
    // too — a streaming consumer reads it off `ToolExecutionEnd` — but only
    // where the tool did not already: `SubAgentTool` sets it on success, and
    // a custom tool's own details are not ours to overwrite. A call that
    // reported several runs gets their combination, so the details never
    // show less than the rollup counted (see `from_sub_agent_result`).
    let delegated =
        std::mem::take(&mut *sub_agent_report.lock().unwrap_or_else(|e| e.into_inner()));
    let mut result = result;
    let mut is_error = is_error;
    // Extensions see (and may edit) the result of a call that ran, before it
    // is truncated, stored or sent.
    if tool.is_some() && !gate.extensions.is_empty() {
        let call = ToolCallRequest {
            tool_call_id: id,
            tool_name: name,
            args,
            messages: gate.history,
            run_prompts: &prompts,
        };
        let mut output = crate::extension::ToolOutput::new(result, is_error);
        gate.extensions.after_tool(&call, &mut output).await;
        result = output.result;
        is_error = output.is_error;
    }
    if let Some((first, rest)) = delegated.split_first() {
        let mut combined = first.clone();
        for stats in rest {
            combined.merge(stats);
        }
        attach_sub_agent_stats(&mut result.details, &combined);
    }

    tx.send(AgentEvent::ToolExecutionEnd {
        tool_call_id: id.to_string(),
        tool_name: name.to_string(),
        result: result.clone(),
        is_error,
    })
    .ok();

    let tool_result_msg = Message::ToolResult {
        tool_call_id: id.to_string(),
        tool_name: name.to_string(),
        content: result.content,
        is_error,
        timestamp: now_ms(),
    };

    tx.send(AgentEvent::MessageStart {
        message: tool_result_msg.clone().into(),
    })
    .ok();
    tx.send(AgentEvent::MessageEnd {
        message: tool_result_msg.clone().into(),
    })
    .ok();

    (tool_result_msg, delegated)
}

/// Put a sub-agent's stats under [`SUB_AGENT_STATS_KEY`] unless the details
/// already carry them. `Null` (every error result) becomes an object; any
/// other non-object is left alone rather than clobbered.
fn attach_sub_agent_stats(details: &mut serde_json::Value, stats: &SessionStats) {
    if details.is_null() {
        *details = serde_json::json!({});
    }
    if let Some(obj) = details.as_object_mut() {
        if !obj.contains_key(SUB_AGENT_STATS_KEY) {
            if let Ok(v) = serde_json::to_value(stats) {
                obj.insert(SUB_AGENT_STATS_KEY.to_string(), v);
            }
        }
    }
}

/// Emit events and build the error tool result for a middleware-denied call.
fn denied_tool_call(
    id: &str,
    name: &str,
    args: &serde_json::Value,
    reason: &str,
    tx: &mpsc::UnboundedSender<AgentEvent>,
) -> (Message, bool) {
    // Operator-visible signal: without this, a denial exists only in the
    // event stream / message history, invisible to telemetry.
    tracing::warn!(
        tool = name,
        tool_call_id = id,
        reason,
        "tool call denied by middleware"
    );
    unexecuted_tool_call(id, name, args, format!("Tool call denied: {}", reason), tx)
}

/// Emit events and build the error tool result for a call whose arguments
/// did not resolve to a JSON object: either they did not parse (cut off —
/// the response likely hit its output token limit) or they parsed to some
/// other JSON value (e.g. double-encoded as a string).
///
/// The text deliberately does not say "retry": the same call resent unchanged
/// would be cut off at the same point again.
fn unparsed_arguments_tool_call(
    id: &str,
    name: &str,
    args: &serde_json::Value,
    raw: &str,
    tx: &mpsc::UnboundedSender<AgentEvent>,
) -> (Message, bool) {
    let text = match serde_json::from_str::<serde_json::Value>(raw) {
        Err(e) => {
            tracing::warn!(
                tool = name,
                tool_call_id = id,
                len = raw.len(),
                parse_error = %e,
                "tool call not executed: arguments did not parse as JSON"
            );
            // Only an early end of input means the text was cut off. Complete
            // but malformed JSON (a trailing comma, two objects run together)
            // must not be answered with "make it smaller": the model would
            // resend the same syntax error in smaller pieces.
            if e.is_eof() {
                format!(
                    "The arguments for tool `{name}` were cut off before they were complete \
                     (the response likely hit the output token limit): {e}. The tool was not \
                     run. Do not resend the same call unchanged — make the arguments smaller \
                     (for example, split large content across several calls)."
                )
            } else {
                format!(
                    "The arguments for tool `{name}` are not valid JSON: {e}. The tool was not \
                     run. Send the arguments as a single valid JSON object."
                )
            }
        }
        Ok(v) => {
            let kind = match v {
                serde_json::Value::String(_) => "a string",
                serde_json::Value::Number(_) => "a number",
                serde_json::Value::Array(_) => "an array",
                serde_json::Value::Bool(_) => "a boolean",
                // Unreachable for a provider-built marker (`null` resolves to
                // `{}`, objects pass through), but a hand-built one could hold
                // either; say only what is known rather than invent a cause.
                serde_json::Value::Null | serde_json::Value::Object(_) => "",
            };
            tracing::warn!(
                tool = name,
                tool_call_id = id,
                kind,
                "tool call not executed: arguments were not delivered as a JSON object"
            );
            if kind.is_empty() {
                format!(
                    "The arguments for tool `{name}` were not delivered as a parsed JSON \
                     object. The tool was not run."
                )
            } else {
                format!(
                    "The arguments for tool `{name}` were not a JSON object (got {kind}). \
                     The tool was not run. Send the arguments as a single JSON object — \
                     not encoded as a string."
                )
            }
        }
    };
    unexecuted_tool_call(id, name, args, text, tx)
}

/// A tool call not executed because a required extension failed the run.
fn failed_run_tool_call(
    id: &str,
    name: &str,
    args: &serde_json::Value,
    tx: &mpsc::UnboundedSender<AgentEvent>,
) -> Message {
    tracing::debug!(
        tool = name,
        tool_call_id = id,
        "tool call not executed: a required extension failed the run"
    );
    unexecuted_tool_call(
        id,
        name,
        args,
        "Tool call not run: a required extension failed the run.".to_string(),
        tx,
    )
    .0
}

/// A tool call not executed because its run was cancelled.
fn cancelled_tool_call(
    id: &str,
    name: &str,
    args: &serde_json::Value,
    tx: &mpsc::UnboundedSender<AgentEvent>,
) -> Message {
    tracing::debug!(
        tool = name,
        tool_call_id = id,
        "tool call not executed: the run was cancelled"
    );
    unexecuted_tool_call(id, name, args, CANCELLED_TOOL_RESULT_TEXT.to_string(), tx).0
}

/// Emit events and build an error tool result for a call that was not run.
/// Start/End are both emitted so UI event pairing stays intact.
fn unexecuted_tool_call(
    id: &str,
    name: &str,
    args: &serde_json::Value,
    text: String,
    tx: &mpsc::UnboundedSender<AgentEvent>,
) -> (Message, bool) {
    tx.send(AgentEvent::ToolExecutionStart {
        tool_call_id: id.to_string(),
        tool_name: name.to_string(),
        args: args.clone(),
    })
    .ok();

    let result = ToolResult {
        content: vec![Content::Text { text }],
        details: serde_json::Value::Null,
    };

    tx.send(AgentEvent::ToolExecutionEnd {
        tool_call_id: id.to_string(),
        tool_name: name.to_string(),
        result: result.clone(),
        is_error: true,
    })
    .ok();

    let msg = Message::ToolResult {
        tool_call_id: id.to_string(),
        tool_name: name.to_string(),
        content: result.content,
        is_error: true,
        timestamp: now_ms(),
    };

    tx.send(AgentEvent::MessageStart {
        message: msg.clone().into(),
    })
    .ok();
    tx.send(AgentEvent::MessageEnd {
        message: msg.clone().into(),
    })
    .ok();

    (msg, true)
}

fn skip_tool_call(
    tool_call_id: &str,
    tool_name: &str,
    tx: &mpsc::UnboundedSender<AgentEvent>,
) -> Message {
    let result = ToolResult {
        content: vec![Content::Text {
            text: "Skipped due to queued user message.".into(),
        }],
        details: serde_json::Value::Null,
    };

    tx.send(AgentEvent::ToolExecutionStart {
        tool_call_id: tool_call_id.into(),
        tool_name: tool_name.into(),
        args: serde_json::Value::Null,
    })
    .ok();

    tx.send(AgentEvent::ToolExecutionEnd {
        tool_call_id: tool_call_id.into(),
        tool_name: tool_name.into(),
        result: result.clone(),
        is_error: true,
    })
    .ok();

    let msg = Message::ToolResult {
        tool_call_id: tool_call_id.into(),
        tool_name: tool_name.into(),
        content: result.content,
        is_error: true,
        timestamp: now_ms(),
    };

    tx.send(AgentEvent::MessageStart {
        message: msg.clone().into(),
    })
    .ok();
    tx.send(AgentEvent::MessageEnd {
        message: msg.clone().into(),
    })
    .ok();

    msg
}
