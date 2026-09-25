use serde::{Deserialize, Serialize};
use std::fmt;
use std::sync::Arc;

// ---------------------------------------------------------------------------
// Content types
// ---------------------------------------------------------------------------

/// Content block of a message.
///
/// Exhaustiveness policy (two separate levers):
/// - The **enum** is `#[non_exhaustive]`: new content kinds may be added in
///   minor releases, so downstream `match` arms need a wildcard.
/// - The `ToolCall` and `Thinking` **variants** are separately
///   `#[non_exhaustive]`: their fields grow with provider features (PR #32
///   added `provider_metadata`), so downstream constructs them via the
///   `Content::tool_call*` / `Content::thinking*` constructors and uses `..`
///   in patterns. `Text` and `Image` stay literally constructible — they are
///   user-facing shapes that do not grow.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type")]
#[non_exhaustive]
pub enum Content {
    #[serde(rename = "text")]
    Text { text: String },
    #[serde(rename = "image")]
    Image {
        data: String,
        #[serde(rename = "mimeType")]
        mime_type: String,
    },
    #[serde(rename = "thinking")]
    #[non_exhaustive]
    Thinking {
        thinking: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        signature: Option<String>,
    },
    #[serde(rename = "toolCall")]
    #[non_exhaustive]
    ToolCall {
        id: String,
        name: String,
        arguments: serde_json::Value,
        /// Provider-specific metadata (e.g. Gemini thought signatures).
        /// Not passed to tool execution; used by providers when building
        /// the next request.
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            rename = "providerMetadata",
            alias = "provider_metadata"
        )]
        provider_metadata: Option<serde_json::Value>,
    },
}

impl Content {
    /// Construct a tool-call content block.
    ///
    /// The `ToolCall` variant is `#[non_exhaustive]` so provider-specific
    /// fields can be added without breaking downstream crates — use this
    /// constructor instead of a struct literal.
    pub fn tool_call(
        id: impl Into<String>,
        name: impl Into<String>,
        arguments: serde_json::Value,
    ) -> Self {
        Self::ToolCall {
            id: id.into(),
            name: name.into(),
            arguments,
            provider_metadata: None,
        }
    }

    /// Construct a thinking content block without a signature.
    pub fn thinking(text: impl Into<String>) -> Self {
        Self::Thinking {
            thinking: text.into(),
            signature: None,
        }
    }

    /// Construct a thinking content block with a provider signature.
    pub fn thinking_signed(text: impl Into<String>, signature: impl Into<String>) -> Self {
        Self::Thinking {
            thinking: text.into(),
            signature: Some(signature.into()),
        }
    }

    /// Construct a tool-call content block carrying provider metadata
    /// (e.g. a Gemini thought signature).
    pub fn tool_call_with_metadata(
        id: impl Into<String>,
        name: impl Into<String>,
        arguments: serde_json::Value,
        provider_metadata: serde_json::Value,
    ) -> Self {
        Self::ToolCall {
            id: id.into(),
            name: name.into(),
            arguments,
            provider_metadata: Some(provider_metadata),
        }
    }
}

// ---------------------------------------------------------------------------
// Messages
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "role")]
pub enum Message {
    #[serde(rename = "user")]
    User {
        content: Vec<Content>,
        timestamp: u64,
    },
    #[serde(rename = "assistant")]
    #[non_exhaustive]
    Assistant {
        content: Vec<Content>,
        #[serde(rename = "stopReason")]
        stop_reason: StopReason,
        model: String,
        provider: String,
        usage: Usage,
        timestamp: u64,
        #[serde(
            skip_serializing_if = "Option::is_none",
            rename = "errorMessage",
            alias = "error_message"
        )]
        error_message: Option<String>,
    },
    #[serde(rename = "toolResult")]
    ToolResult {
        #[serde(rename = "toolCallId")]
        tool_call_id: String,
        #[serde(rename = "toolName")]
        tool_name: String,
        content: Vec<Content>,
        #[serde(rename = "isError")]
        is_error: bool,
        timestamp: u64,
    },
}

impl Message {
    pub fn user(text: impl Into<String>) -> Self {
        Self::User {
            content: vec![Content::Text { text: text.into() }],
            timestamp: now_ms(),
        }
    }

    /// Construct an assistant message.
    ///
    /// The `Assistant` variant is `#[non_exhaustive]` — its fields grow with
    /// provider features (`error_message` was itself a later addition), so
    /// custom `StreamProvider` implementations construct it here instead of
    /// with a struct literal. `timestamp` is set to now and `error_message`
    /// to `None`; use [`Message::with_error_message`] /
    /// [`Message::with_timestamp`] to override.
    pub fn assistant(
        content: Vec<Content>,
        stop_reason: StopReason,
        model: impl Into<String>,
        provider: impl Into<String>,
        usage: Usage,
    ) -> Self {
        Self::Assistant {
            content,
            stop_reason,
            model: model.into(),
            provider: provider.into(),
            usage,
            timestamp: now_ms(),
            error_message: None,
        }
    }

    /// Set the error message (no-op on non-assistant messages).
    pub fn with_error_message(mut self, msg: impl Into<String>) -> Self {
        if let Self::Assistant { error_message, .. } = &mut self {
            *error_message = Some(msg.into());
        }
        self
    }

    /// Override the timestamp (applies to all message kinds).
    pub fn with_timestamp(mut self, ts: u64) -> Self {
        match &mut self {
            Self::User { timestamp, .. }
            | Self::Assistant { timestamp, .. }
            | Self::ToolResult { timestamp, .. } => *timestamp = ts,
        }
        self
    }

    pub fn role(&self) -> &str {
        match self {
            Self::User { .. } => "user",
            Self::Assistant { .. } => "assistant",
            Self::ToolResult { .. } => "toolResult",
        }
    }

    /// Check if this assistant message represents a context overflow error.
    ///
    /// Some providers (SSE-based: Anthropic, OpenAI) return overflow as a
    /// `StopReason::Error` message rather than an HTTP error. This method
    /// checks the `error_message` field against known overflow patterns.
    pub fn is_context_overflow(&self) -> bool {
        match self {
            Self::Assistant {
                stop_reason: StopReason::Error,
                error_message: Some(msg),
                ..
            } => crate::provider::is_context_overflow_message(msg),
            _ => false,
        }
    }
}

// ---------------------------------------------------------------------------
// AgentMessage — LLM messages + extensible custom types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExtensionMessage {
    pub role: String,
    pub kind: String,
    pub data: serde_json::Value,
}

impl ExtensionMessage {
    pub fn new(kind: impl Into<String>, data: impl Serialize) -> Self {
        Self {
            role: "extension".into(),
            kind: kind.into(),
            data: serde_json::to_value(data).unwrap_or(serde_json::Value::Null),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum AgentMessage {
    /// Standard LLM message
    Llm(Message),
    /// App-specific message (UI-only, notifications, etc.)
    Extension(ExtensionMessage),
}

impl AgentMessage {
    pub fn role(&self) -> &str {
        match self {
            Self::Llm(m) => m.role(),
            Self::Extension(ext) => &ext.role,
        }
    }

    pub fn as_llm(&self) -> Option<&Message> {
        match self {
            Self::Llm(m) => Some(m),
            Self::Extension(_) => None,
        }
    }
}

impl From<Message> for AgentMessage {
    fn from(m: Message) -> Self {
        Self::Llm(m)
    }
}

// ---------------------------------------------------------------------------
// Stop reasons & usage
// ---------------------------------------------------------------------------

/// Why the model stopped generating.
///
/// `#[non_exhaustive]` since 0.17.0: stop reasons grow with provider features,
/// so every addition was otherwise a breaking release. Downstream `match` arms
/// need a `_ =>` wildcard; inside this crate the enum is still exhaustive, so
/// adding a variant remains a compile error where it matters most.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub enum StopReason {
    Stop,
    Length,
    ToolUse,
    Error,
    Aborted,
    /// The provider's safety system declined the request. The stream completes
    /// normally (HTTP 200) but with `stop_reason: refusal` and empty or partial
    /// content; `error_message` carries an explanation. Currently emitted by
    /// Anthropic models that support the `refusal` stop reason (e.g. Claude
    /// Fable 5). The agent loop does not special-case it (the turn ends like a
    /// normal `Stop`); callers can match on it to retry on a fallback model.
    Refusal,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct Usage {
    pub input: u64,
    pub output: u64,
    // camelCase on the wire (AgentEvent contract); `alias` keeps session
    // files written by yoagent < 0.13 loadable.
    #[serde(default, rename = "cacheRead", alias = "cache_read")]
    pub cache_read: u64,
    #[serde(default, rename = "cacheWrite", alias = "cache_write")]
    pub cache_write: u64,
    #[serde(default, rename = "totalTokens", alias = "total_tokens")]
    pub total_tokens: u64,
}

impl Usage {
    /// Fraction of input tokens served from cache (0.0–1.0).
    /// Returns 0.0 if no input tokens were processed.
    pub fn cache_hit_rate(&self) -> f64 {
        let total_input = self.input + self.cache_read + self.cache_write;
        if total_input == 0 {
            return 0.0;
        }
        self.cache_read as f64 / total_input as f64
    }
}

// ---------------------------------------------------------------------------
// Cache configuration
// ---------------------------------------------------------------------------

/// Controls yoagent-managed prompt caching hints.
///
/// By default, caching is enabled with automatic breakpoint placement. What
/// that produces on the wire depends on the protocol — see [`CacheStrategy`]
/// for the per-provider table. `enabled: false` suppresses every hint yoagent
/// would otherwise send; it cannot switch off a provider's *automatic*
/// server-side caching, which is not under client control.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct CacheConfig {
    /// Master switch — set to false to disable all caching hints.
    /// Default: true.
    pub enabled: bool,
    /// How cache breakpoints are placed.
    pub strategy: CacheStrategy,
    /// Stable identifier for this conversation, for providers that route cache
    /// lookups by key rather than by explicit breakpoints (OpenAI's
    /// `prompt_cache_key`).
    ///
    /// Leave `None` and one is derived from the request's stable head — the
    /// system prompt plus the first user message. That derivation is correct
    /// for the common case and costs nothing, but two sessions opening with
    /// identical text share a key. Set this explicitly when sessions must be
    /// routed apart, or when the head is not distinctive.
    ///
    /// Ignored everywhere except the key-routed path — by Anthropic, which
    /// takes explicit breakpoints, and by Google, Vertex, Bedrock, Azure and
    /// Responses, which yoagent sends no cache hints to at all.
    ///
    /// Note for [`crate::SubAgentTool`]: it holds one `CacheConfig` and clones
    /// it into every invocation, so a key set there is shared by every run the
    /// tool performs rather than identifying one conversation.
    #[serde(default)]
    pub session_key: Option<String>,
}

impl CacheConfig {
    /// Caching enabled with automatic breakpoint placement.
    ///
    /// Identical to [`Default::default`]; provided because this struct is
    /// `#[non_exhaustive]` and `new()` is where callers look first.
    pub fn new() -> Self {
        Self::default()
    }

    /// All caching hints suppressed.
    pub fn disabled() -> Self {
        Self {
            enabled: false,
            ..Self::default()
        }
    }

    /// Set the session key used by key-routed providers.
    ///
    /// A blank key is treated as unset — sending `prompt_cache_key: ""` would
    /// route every caller who did that onto one cache, which is worse than
    /// sending nothing. `with_session_key(format!("tenant-{id}"))` with an
    /// empty `id` is the ordinary way to arrive here.
    pub fn with_session_key(mut self, key: impl Into<String>) -> Self {
        let key = key.into();
        self.session_key = if key.trim().is_empty() {
            None
        } else {
            Some(key)
        };
        self
    }

    /// Set the breakpoint-placement strategy.
    ///
    /// Needed because this struct is `#[non_exhaustive]`: downstream crates
    /// cannot use a struct literal, so `Manual { .. }` would otherwise be
    /// unreachable from outside.
    pub fn with_strategy(mut self, strategy: CacheStrategy) -> Self {
        self.strategy = strategy;
        self
    }

    /// Whether yoagent should emit any caching hint at all.
    ///
    /// Three configurations mean "no": `enabled: false`, `Disabled`, and
    /// `Manual` with every flag off. The third is easy to miss — Anthropic
    /// honours it correctly by placing no breakpoints, so a key-routed
    /// provider that still sent a key would be reading the same value as
    /// "yes". One predicate, so every protocol agrees on what off means.
    pub fn hints_enabled(&self) -> bool {
        if !self.enabled {
            return false;
        }
        !matches!(
            self.strategy,
            CacheStrategy::Disabled
                | CacheStrategy::Manual {
                    cache_system: false,
                    cache_tools: false,
                    cache_messages: false,
                }
        )
    }
}

impl Default for CacheConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            strategy: CacheStrategy::Auto,
            session_key: None,
        }
    }
}

// ---------------------------------------------------------------------------
// Tool execution strategy
// ---------------------------------------------------------------------------

/// Controls how multiple tool calls from a single LLM response are executed.
///
/// When the LLM returns multiple tool calls (e.g., "read file A, read file B,
/// run bash C"), this determines whether they run sequentially or in parallel.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum ToolExecutionStrategy {
    /// Run tools one at a time, check steering between each.
    /// Use for debugging or tools with shared mutable state.
    Sequential,
    /// Run all tool calls concurrently, check steering after all complete.
    /// Default — most tool calls are independent and this gives the best latency.
    #[default]
    Parallel,
    /// Run in batches of N, check steering between batches.
    /// Balances speed with human-in-the-loop control.
    Batched { size: usize },
}

/// Strategy for prompt caching.
///
/// Providers expose caching in two different shapes, and this enum means
/// something different in each:
///
/// | provider | shape | what this enum controls |
/// |---|---|---|
/// | Anthropic | explicit breakpoints | where `cache_control` markers are placed |
/// | OpenAI (native) | key-routed | whether `prompt_cache_key` is sent |
/// | DeepSeek, Gemini, and other automatic backends | automatic, server-side | nothing |
/// | Azure, OpenAI Responses, Bedrock | supported but **not yet wired** | nothing *yet* |
///
/// **Explicit breakpoints** (Anthropic) are the model this enum was designed
/// around: the client chooses cache boundaries and pays a write premium for
/// them. [`Auto`](Self::Auto) and [`Manual`](Self::Manual) select which
/// boundaries.
///
/// **Key-routed** (OpenAI) caches automatically on prefixes of ~1024 tokens or
/// more; there are no breakpoints to place. `prompt_cache_key` only improves
/// *routing* — it steers requests from one conversation toward the same cache
/// — so the `Auto`/`Manual` distinction has nothing to act on and both send the
/// key. Only [`Disabled`](Self::Disabled) is meaningful. The key comes from
/// [`CacheConfig::session_key`], or is derived from the request head. Gated on
/// [`OpenAiCompat::supports_prompt_cache_key`](crate::provider::OpenAiCompat),
/// because the field is OpenAI's and a strict compat server may reject unknown
/// keys outright rather than ignore them.
///
/// **Automatic, server-side** (DeepSeek, Gemini) caches on its own with nothing
/// to configure. DeepSeek quantises hits to 64-token blocks and charges nothing
/// to populate; Gemini reports `cachedContentTokenCount` but its explicit
/// cached-content API is a separate create-then-reference resource with its own
/// TTL and billing, deliberately not driven from here. For these, this setting
/// is inert — including `Disabled`, which cannot switch off caching the client
/// never asked for.
///
/// **Not yet wired** is a separate row on purpose. Azure and the OpenAI
/// Responses API both accept `prompt_cache_key`, and Bedrock accepts explicit
/// `cachePoint` blocks; yoagent sends none of them today. That is a gap in this
/// crate, not a property of those vendors, and conflating the two would make
/// the omission read as a deliberate design decision.
///
/// Two practical consequences. A hit rate is not comparable across protocols
/// without knowing which shape produced it. And only Anthropic, the
/// OpenAI-compat path and Gemini populate [`Usage::cache_read`] at all — on
/// Azure, Responses and Bedrock there is no hit rate to read. See
/// `docs/concepts/prompt-caching.md`.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub enum CacheStrategy {
    /// Automatic placement (recommended).
    ///
    /// Anthropic: caches system prompt, tool definitions, and recent history.
    /// OpenAI: sends `prompt_cache_key`. Elsewhere: no effect.
    #[default]
    Auto,
    /// Send no caching hints at all.
    ///
    /// Does **not** disable a provider's automatic server-side caching — that
    /// is not client-controllable. On Anthropic this means paying full input
    /// price for every prefix, so reach for it only when a rewrite-heavy
    /// workload makes cache writes pure loss.
    Disabled,
    /// Fine-grained control over what gets cached.
    ///
    /// Anthropic-only in effect: no other protocol exposes placement. Treated
    /// as [`Auto`](Self::Auto) by key-routed providers, which have one knob
    /// rather than three.
    Manual {
        /// Cache the system prompt.
        cache_system: bool,
        /// Cache tool definitions.
        cache_tools: bool,
        /// Cache conversation history (second-to-last message).
        cache_messages: bool,
    },
}

// ---------------------------------------------------------------------------
// Thinking level
// ---------------------------------------------------------------------------

/// How hard the model should reason before answering.
///
/// A provider-neutral ladder. Each provider maps it onto its own knob, and
/// where a provider's ladder is shorter than this one the upper levels are
/// **clamped** to the highest value this crate knows the provider accepts
/// rather than sent as a value it would reject —
/// with one exception: Anthropic's adaptive `effort` is passed through
/// unclamped, so a model with a shorter effort ladder can reject it (see
/// below). What each level becomes, per provider:
///
/// | Level     | Anthropic (adaptive) | Anthropic legacy / Bedrock budget | OpenAI-compat `reasoning_effort`¹ | DeepSeek `reasoning_effort`² | OpenAI Responses / Azure `reasoning.effort` | Gemini 2.x / Vertex `thinkingBudget`³ |
/// |-----------|----------|--------|----------|--------|----------|--------|
/// | `Off`     | (no thinking) | (no thinking) | (omitted) | (omitted; `thinking: disabled`) | (omitted) | (omitted) |
/// | `Minimal` | `low`    | 1,024  | `low`    | `low`  | `low`    | 1,024  |
/// | `Low`     | `low`    | 1,024  | `low`    | `low`  | `low`    | 1,024  |
/// | `Medium`  | `medium` | 2,048  | `medium` | `medium` (DeepSeek rounds up to `high`) | `medium` | 8,192  |
/// | `High`    | `high`   | 8,192  | `high`   | `high` | `high`   | 24,576 |
/// | `XHigh`   | `xhigh`  | 16,384 | `high` *(clamped)* | `high` *(clamped)* | `high` *(clamped)* | 24,576 *(clamped)* |
/// | `Max`     | `max`    | 30,720 | `high` *(clamped)* | `max` | `high` *(clamped)* | 24,576 *(clamped)* |
///
/// ¹ Only when [`OpenAiCompat::supports_reasoning_effort`] is set; otherwise
/// no `reasoning_effort` is sent. Omitting it does not always mean "no
/// reasoning": xAI's Grok cannot disable reasoning, so `Off` leaves it at its
/// default (`high`).
///
/// ² "DeepSeek" means any OpenAI-compat provider with both
/// [`OpenAiCompat::supports_thinking_control`] and
/// [`OpenAiCompat::supports_reasoning_effort`] set. DeepSeek's
/// `reasoning_effort` accepts `low`/`high`/`max`
/// (<https://api-docs.deepseek.com/guides/thinking_mode>), and DeepSeek itself
/// maps a requested `xhigh` to `high`, so `XHigh` is sent as `high` and only
/// `Max` selects `max`. `Off` is sent as `thinking: {"type": "disabled"}`
/// rather than as an effort value.
///
/// ³ Gemini 3 and later take `thinkingLevel` instead (never both — Gemini
/// rejects a request carrying the two). The generation is read from the model
/// id's version (`gemini-3*`, `gemini-3.8-flash`, `models/…`, Vertex resource
/// paths); override with [`GoogleCompat::thinking_level`]:
///
/// | Level | Gemini 3+ / Vertex `thinkingLevel` |
/// |-------|-----------|
/// | `Off` | `MINIMAL`, or `LOW` where `MINIMAL` is not accepted (no `includeThoughts`) |
/// | `Minimal` | `MINIMAL`, or `LOW` *(clamped)* on 3.7 / 3.8 Flash, 3.x Pro and unlisted models |
/// | `Low` | `LOW` |
/// | `Medium` | `MEDIUM` |
/// | `High`, `XHigh`, `Max` | `HIGH` (`XHigh`/`Max` *clamped*) |
///
/// `Off` does **not** disable thinking on Gemini 3: Google documents
/// `MINIMAL` as matching "the "no thinking" setting for most queries" but
/// not guaranteeing it, and thinking cannot be turned off at all on 3 Pro /
/// 3.1 Pro.
///
/// Anthropic's adaptive `effort` is passed through as-is, so a model with a
/// shorter ladder rejects what it does not know: `xhigh` arrived with Opus
/// 4.7, so Opus 4.6 / Sonnet 4.6 accept `max` but not `xhigh`. The crate has
/// no per-model effort table; pick a level the model supports.
///
/// On Anthropic, `Off` omits the `thinking` field; it never sends
/// `disabled`. "(no thinking)" holds only for models that think on request:
/// Claude Opus 5.5 and Fable 5.1 always think (at their default effort), and
/// Opus 5 thinks whenever the field is absent.
///
/// Marked `#[non_exhaustive]` so the next rung a vendor adds is not a breaking
/// change: `match` on it from outside the crate needs a wildcard arm.
///
/// [`OpenAiCompat::supports_thinking_control`]: crate::provider::OpenAiCompat::supports_thinking_control
/// [`OpenAiCompat::supports_reasoning_effort`]: crate::provider::OpenAiCompat::supports_reasoning_effort
/// [`GoogleCompat::thinking_level`]: crate::provider::GoogleCompat::thinking_level
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
#[non_exhaustive]
pub enum ThinkingLevel {
    /// No reasoning requested.
    #[default]
    Off,
    /// Currently identical to `Low` on every provider.
    Minimal,
    Low,
    Medium,
    High,
    /// Above `High`, below `Max` — Anthropic's `xhigh`. Where a provider has
    /// no `xhigh` rung it is clamped down to that provider's `High` value
    /// (see the table above). Serializes as `"xhigh"`.
    XHigh,
    /// The highest setting this crate sends — Anthropic's and DeepSeek's
    /// `max`. Elsewhere it is clamped to the highest value this crate knows is
    /// accepted, which may be below the model's real ceiling (see the table
    /// above).
    Max,
}

// ---------------------------------------------------------------------------
// Tool definition
// ---------------------------------------------------------------------------

/// Callback for streaming partial results during tool execution.
///
/// Tools call this to emit progress updates (e.g., partial output, status messages)
/// that are forwarded as `AgentEvent::ToolExecutionUpdate` events for UI consumption.
/// Partial results are **not** sent to the LLM — only the final `ToolResult` is.
pub type ToolUpdateFn = Arc<dyn Fn(ToolResult) + Send + Sync>;

/// Callback for emitting user-facing progress messages during tool execution.
///
/// Each invocation emits an `AgentEvent::ProgressMessage` event. Unlike `ToolUpdateFn`,
/// these are simple text messages intended for user-facing display (e.g., status lines,
/// notifications), not structured tool results.
pub type ProgressFn = Arc<dyn Fn(String) + Send + Sync>;

/// Context passed to tool execution. Bundles all per-invocation state.
///
/// Using a struct instead of individual parameters future-proofs the trait —
/// adding fields to `ToolContext` is non-breaking, which `#[non_exhaustive]`
/// is what actually makes true. Tools receive this rather than build it, so
/// the attribute costs implementors nothing; it only stops a struct literal in
/// downstream test code from breaking on every new field.
#[non_exhaustive]
pub struct ToolContext {
    /// The ID of this tool call (for correlation).
    pub tool_call_id: String,
    /// The name of the tool being invoked.
    pub tool_name: String,
    /// Cancellation token — check `is_cancelled()` in long-running tools.
    pub cancel: tokio_util::sync::CancellationToken,
    /// Optional callback for streaming partial `ToolResult`s (UI/logging only).
    pub on_update: Option<ToolUpdateFn>,
    /// Optional callback for emitting user-facing progress messages.
    pub on_progress: Option<ProgressFn>,
    /// Set by the loop; delegation tools report their runs' stats here, via
    /// [`report_delegated_run`](Self::report_delegated_run), so they survive a
    /// failed delegation.
    pub(crate) sub_agent_report: Option<SubAgentReport>,
}

impl ToolContext {
    /// A context for one tool invocation, with no cancellation token and no
    /// callbacks.
    ///
    /// This struct is `#[non_exhaustive]`, so downstream crates build it here
    /// rather than with a struct literal. The loop constructs the real one;
    /// this is for tests and for callers driving a tool directly.
    pub fn new(tool_call_id: impl Into<String>, tool_name: impl Into<String>) -> Self {
        Self {
            tool_call_id: tool_call_id.into(),
            tool_name: tool_name.into(),
            cancel: tokio_util::sync::CancellationToken::new(),
            on_update: None,
            on_progress: None,
            sub_agent_report: None,
        }
    }

    /// Use the given cancellation token instead of a fresh one.
    pub fn with_cancel(mut self, cancel: tokio_util::sync::CancellationToken) -> Self {
        self.cancel = cancel;
        self
    }

    /// Stream partial [`ToolResult`]s to this callback (UI/logging only).
    pub fn with_on_update(mut self, on_update: ToolUpdateFn) -> Self {
        self.on_update = Some(on_update);
        self
    }

    /// Emit user-facing progress messages through this callback.
    pub fn with_on_progress(mut self, on_progress: ProgressFn) -> Self {
        self.on_progress = Some(on_progress);
        self
    }

    /// Report one delegated agent run's stats to the loop that invoked this
    /// tool, so its spend is counted in that loop's
    /// [`SessionStats::sub_agents`].
    ///
    /// [`SubAgentTool`](crate::SubAgentTool) calls this for you; a **custom**
    /// delegation tool — one that runs its own [`agent_loop`](crate::agent_loop())
    /// or [`Agent`](crate::Agent) — calls it once per run it started, with that
    /// run's stats as carried by its [`AgentEvent::AgentEnd`]. Report even
    /// when the run failed and the tool is about to return `Err`: the spend of
    /// a failed delegation is still spend, and this is the only way it reaches
    /// the parent.
    ///
    /// **Call it before [`execute`](AgentTool::execute) returns.** The loop
    /// collects reports as soon as the tool's future completes; a report made
    /// after that — from a task the tool spawned and did not await, say — is
    /// silently lost. Clones of this context report to the same place, so a
    /// tool that fans out to several runs can hand each a clone.
    ///
    /// A no-op on a context the loop did not build (e.g. one from
    /// [`ToolContext::new`]): there is no parent to report to.
    pub fn report_delegated_run(&self, stats: SessionStats) {
        if let Some(report) = &self.sub_agent_report {
            report.lock().unwrap_or_else(|e| e.into_inner()).push(stats);
        }
    }
}

impl Clone for ToolContext {
    fn clone(&self) -> Self {
        Self {
            tool_call_id: self.tool_call_id.clone(),
            tool_name: self.tool_name.clone(),
            cancel: self.cancel.clone(),
            on_update: self.on_update.clone(),
            on_progress: self.on_progress.clone(),
            sub_agent_report: self.sub_agent_report.clone(),
        }
    }
}

impl std::fmt::Debug for ToolContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ToolContext")
            .field("tool_call_id", &self.tool_call_id)
            .field("tool_name", &self.tool_name)
            .field("cancel", &self.cancel)
            .field("on_update", &self.on_update.as_ref().map(|_| "<callback>"))
            .field(
                "on_progress",
                &self.on_progress.as_ref().map(|_| "<callback>"),
            )
            .finish()
    }
}

/// A tool the agent can call. Implement this trait for your tools.
#[async_trait::async_trait]
pub trait AgentTool: Send + Sync {
    /// Unique tool name (used in LLM tool_use)
    fn name(&self) -> &str;
    /// Human-readable label for UI
    fn label(&self) -> &str;
    /// Description for the LLM
    fn description(&self) -> &str;
    /// JSON Schema for parameters
    fn parameters_schema(&self) -> serde_json::Value;
    /// Execute the tool.
    ///
    /// The `ctx` parameter provides per-invocation context:
    /// - `ctx.tool_call_id` / `ctx.tool_name` — for correlation and logging
    /// - `ctx.cancel` — cancellation token; check `is_cancelled()` in long-running tools
    /// - `ctx.on_update` — optional callback for streaming partial `ToolResult`s (UI/logging only)
    /// - `ctx.on_progress` — optional callback for user-facing progress text (`ProgressMessage`)
    async fn execute(
        &self,
        params: serde_json::Value,
        ctx: ToolContext,
    ) -> Result<ToolResult, ToolError>;
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolResult {
    pub content: Vec<Content>,
    #[serde(default)]
    pub details: serde_json::Value,
}

#[derive(Debug, thiserror::Error)]
pub enum ToolError {
    #[error("{0}")]
    Failed(String),
    #[error("Tool not found: {0}")]
    NotFound(String),
    #[error("Invalid arguments: {0}")]
    InvalidArgs(String),
    #[error("Cancelled")]
    Cancelled,
}

// ---------------------------------------------------------------------------
// Agent events (for streaming UI updates)
// ---------------------------------------------------------------------------

/// Events emitted by the agent loop for streaming UI updates.
///
/// # Wire format (stability contract)
///
/// `AgentEvent` and [`StreamDelta`] serialize as internally-tagged JSON —
/// `{"type": "<camelCase variant>", ...camelCase fields}` — so external
/// frontends (websocket fanout servers, TypeScript clients, JSONL pipes) can
/// consume the event stream directly:
///
/// ```json
/// {"type":"messageUpdate","message":{...},"delta":{"type":"text","delta":"hi"}}
/// {"type":"toolExecutionEnd","toolCallId":"tc_1","toolName":"bash","result":{...},"isError":false}
/// ```
///
/// This shape is a **public contract**: variant tags, field names, and the
/// internal tagging are frozen by snapshot tests. Renaming a variant or field
/// is a breaking change for wire clients, not just for Rust callers.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(
    tag = "type",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
#[non_exhaustive]
pub enum AgentEvent {
    AgentStart,
    /// The run finished. Carries the messages it produced and a
    /// [`SessionStats`] rollup of what they cost.
    ///
    /// The variant is `#[non_exhaustive]` — the payload is expected to grow.
    /// Match with `..`.
    #[non_exhaustive]
    AgentEnd {
        messages: Vec<AgentMessage>,
        /// `#[serde(default)]`: `AgentEvent` is a frozen wire format, and
        /// archived streams predate this field.
        #[serde(default)]
        stats: SessionStats,
    },
    TurnStart,
    TurnEnd {
        message: AgentMessage,
        tool_results: Vec<Message>,
    },
    MessageStart {
        message: AgentMessage,
    },
    MessageUpdate {
        message: AgentMessage,
        delta: StreamDelta,
    },
    MessageEnd {
        message: AgentMessage,
    },
    ToolExecutionStart {
        tool_call_id: String,
        tool_name: String,
        args: serde_json::Value,
    },
    ToolExecutionUpdate {
        tool_call_id: String,
        tool_name: String,
        partial_result: ToolResult,
    },
    ToolExecutionEnd {
        tool_call_id: String,
        tool_name: String,
        result: ToolResult,
        is_error: bool,
    },
    ProgressMessage {
        tool_call_id: String,
        tool_name: String,
        text: String,
    },
    InputRejected {
        reason: String,
    },
    /// A tool was called repeatedly with identical arguments.
    ///
    /// Emitted on both escalations: the first trip steers the model and
    /// continues, a later trip on the same signature stops the run. `aborted`
    /// distinguishes them, so a caller can tell a nudge from a stop and an
    /// audit can record why a run ended.
    #[non_exhaustive]
    LoopDetected {
        tool_name: String,
        repetitions: usize,
        aborted: bool,
    },
    /// History was compacted before a turn.
    ///
    /// Emitted by [`LlmCompaction`](crate::LlmCompaction) on both of its paths
    /// — the spliced summary and the deterministic fallback — so a consumer can
    /// tell which one ran and what it cost. The built-in
    /// [`DefaultCompaction`](crate::context::DefaultCompaction) does not emit
    /// this; it has no event channel and never issues a request.
    ContextCompacted {
        /// Which compaction path produced this result.
        method: CompactionMethod,
        messages_before: usize,
        messages_after: usize,
        tokens_before: usize,
        tokens_after: usize,
        /// What the summarization request produced and cost, when one was
        /// made. `None` on a purely deterministic compaction.
        ///
        /// Present as one optional payload rather than three sibling fields so
        /// the cost, the span it bought, and the fact that a request happened
        /// cannot disagree with each other.
        summary: Option<SummaryStats>,
    },
}

impl AgentEvent {
    /// Construct an [`AgentEvent::AgentEnd`].
    ///
    /// The variant is `#[non_exhaustive]` — its payload grows — so downstream
    /// crates (and tests) build it here rather than with a struct literal.
    pub fn agent_end(messages: Vec<AgentMessage>, stats: SessionStats) -> Self {
        Self::AgentEnd { messages, stats }
    }

    /// Construct an [`AgentEvent::LoopDetected`].
    ///
    /// `aborted` separates the two escalations: `false` is a steer the model
    /// can recover from, `true` means the run stopped.
    pub fn loop_detected(tool_name: impl Into<String>, repetitions: usize, aborted: bool) -> Self {
        Self::LoopDetected {
            tool_name: tool_name.into(),
            repetitions,
            aborted,
        }
    }
}

/// Session-level rollup carried by [`AgentEvent::AgentEnd`].
///
/// The per-turn numbers already existed — `Usage` on every assistant message,
/// `tokens_cached` on the `llm_stream` span, `cache_read` in the GASP record —
/// but nothing summed them, so answering "what was this run's cache hit rate"
/// meant replaying the whole event stream. Any change that moves caching
/// (breakpoint placement, compaction strategy, model choice) had to be judged
/// by hand-built harnesses instead of a number the library reports.
///
/// ```
/// # use yoagent::{SessionStats, Usage};
/// # let stats = SessionStats::default();
/// // Reading your cache hit rate:
/// println!("{:.1}% cached over {} turns", stats.cache_hit_rate() * 100.0, stats.turns);
/// ```
///
/// **Hit rate is `cache_read / (input + cache_read + cache_write)`** — cache
/// writes count against you, because they are prompt tokens the provider
/// processed and billed. Counting only `input` shrinks the denominator and so
/// **overstates** the rate for a write-charging provider: Anthropic books a
/// re-processed prefix to `cache_write`, and an `input`-only metric makes it
/// look roughly ten times cheaper than it is. See
/// `docs/evals/llm-compaction-live.md`, where that error was made and caught.
///
/// Read a rate against its session length, not against 100%: every turn's new
/// content is necessarily a miss, so the ceiling is about `(n-1)/(n+1)` — ~88%
/// at 15 turns, ~96% only past 49.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct SessionStats {
    /// Provider usage summed over every LLM turn in this run.
    ///
    /// `total_tokens` is **not** summed and stays 0 — see [`record_turn`] for
    /// why. Derive a total from the four components instead.
    ///
    /// [`record_turn`]: SessionStats::record_turn
    #[serde(default)]
    pub usage: Usage,
    /// LLM turns taken. Tool executions are not turns; an errored turn counts,
    /// because the provider billed it, and provider retries within one turn do
    /// not appear separately.
    #[serde(default)]
    pub turns: u32,
    /// Dollar cost of [`usage`](Self::usage), when the model's rates are
    /// known (see [`ModelConfig::cost`](crate::provider::ModelConfig::cost)).
    ///
    /// `None` is never "free", but it means one of two things: the spend
    /// **cannot be priced** (a turn with non-zero usage came from a model with
    /// `cost: None`, which is what the generic constructors — custom, local,
    /// `deepseek`, … — return), or there was **nothing to price** (a run that
    /// took no turns, or whose turns reported no usage).
    /// [`is_unpriced`](Self::is_unpriced) tells them apart. A model configured
    /// as free (`Some` with zero rates) reports `Some(0.0)`. Unpriced is
    /// sticky: once any turn cannot be priced this stays `None`, because a
    /// sum that silently skips the unpriced part under-reports.
    ///
    /// Scope: this run's own turns. What [`SubAgentTool`](crate::SubAgentTool)s
    /// spent is kept apart in [`sub_agents`](Self::sub_agents); use
    /// [`total_cost_usd`](Self::total_cost_usd) for the whole bill.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost_usd: Option<f64>,
    /// Times the loop observed compaction rewrite history.
    ///
    /// Counts any turn where the strategy returned a different message count or
    /// a different *estimated* token total ([`context::total_tokens`]), so
    /// in-place tool-output truncation is included alongside reshaping. A
    /// compaction that runs and reclaims nothing is indistinguishable from no
    /// compaction at all.
    ///
    /// [`context::total_tokens`]: crate::context::total_tokens
    ///
    /// Deliberately **not** split into spliced-summary vs deterministic
    /// fallback, and carrying no summarization spend. That detail exists — on
    /// [`AgentEvent::ContextCompacted`], with its [`SummaryStats`] — but
    /// [`CompactionStrategy::compact`](crate::context::CompactionStrategy::compact)
    /// is synchronous and has no event channel, so the loop cannot see it. Wire
    /// `LlmCompaction::with_event_sender` to the same channel and aggregate the
    /// two together — the events *describe* the same compactions this counts,
    /// with the breakdown attached, so do not sum the two. Folding a guess in
    /// here would be worse than the gap.
    #[serde(default)]
    pub compactions: u32,
    /// What this run's [`SubAgentTool`](crate::SubAgentTool) delegations
    /// spent, summed over the whole delegation tree — a sub-agent's nested
    /// sub-agents included.
    ///
    /// A **separate bucket**: [`usage`](Self::usage), [`turns`](Self::turns)
    /// and [`cost_usd`](Self::cost_usd) stay this agent's own, so delegation
    /// remains attributable. Sub-agents run their own loop on a private
    /// channel, and before this field existed their spend reached the parent
    /// nowhere, so every total silently under-reported delegation. Read
    /// [`total_usage`](Self::total_usage) / [`total_cost_usd`](Self::total_cost_usd)
    /// for the whole bill.
    ///
    /// Omitted from the wire when nothing was delegated, so a run without
    /// sub-agents serializes exactly as it did before.
    #[serde(default, skip_serializing_if = "SubAgentSpend::is_empty")]
    pub sub_agents: SubAgentSpend,
}

impl SessionStats {
    /// A rollup with the given figures.
    ///
    /// This struct is `#[non_exhaustive]`, so downstream crates cannot use a
    /// struct literal; without this the only construction path would be
    /// `Default::default()` plus field assignment.
    pub fn new(usage: Usage, turns: u32, cost_usd: Option<f64>, compactions: u32) -> Self {
        Self {
            usage,
            turns,
            cost_usd,
            compactions,
            sub_agents: SubAgentSpend::default(),
        }
    }

    /// Whether this run's **own** spend cannot be priced: non-zero
    /// [`usage`](Self::usage) with no [`cost_usd`](Self::cost_usd). `false`
    /// when there is simply nothing to price. Delegated spend is judged
    /// separately by [`SubAgentSpend::is_unpriced`].
    pub fn is_unpriced(&self) -> bool {
        is_unpriced(&self.usage, self.cost_usd)
    }

    /// This run's own usage plus everything its sub-agents spent.
    ///
    /// `total_tokens` stays 0, as in [`usage`](Self::usage), and for the same
    /// reason: providers disagree on what it counts.
    pub fn total_usage(&self) -> Usage {
        add_usage(&self.usage, &self.sub_agents.usage)
    }

    /// Dollar cost of this run plus its sub-agents, each priced at its **own**
    /// model's rates.
    ///
    /// `None` when any part of that spend cannot be priced — an unpriced
    /// sub-agent makes the whole figure unknown rather than silently low — and
    /// also when nothing at all was spent. Spend of zero tokens needs no
    /// price, so a zero-usage part never poisons the rest.
    pub fn total_cost_usd(&self) -> Option<f64> {
        combine_cost(
            &self.usage,
            self.cost_usd,
            &self.sub_agents.usage,
            self.sub_agents.cost_usd,
        )
    }

    /// Fold another run's stats into this one — own figures into own, the
    /// delegated bucket into the delegated bucket — with the same unpriced
    /// rule as [`SubAgentSpend::merge`].
    pub(crate) fn merge(&mut self, other: &SessionStats) {
        self.cost_usd = combine_cost(&self.usage, self.cost_usd, &other.usage, other.cost_usd);
        self.usage = add_usage(&self.usage, &other.usage);
        self.turns = self.turns.saturating_add(other.turns);
        self.compactions = self.compactions.saturating_add(other.compactions);
        self.sub_agents.merge(&other.sub_agents);
    }

    /// The [`SessionStats`] of a sub-agent run, if `result` came from a
    /// [`SubAgentTool`](crate::SubAgentTool).
    ///
    /// Present on the tool's own return value and on
    /// [`AgentEvent::ToolExecutionEnd`] — including when the sub-agent
    /// **failed**: the loop attaches what it spent before failing to the error
    /// result. Its [`usage`](Self::usage) is the sub-agent's own spend and its
    /// [`sub_agents`](Self::sub_agents) what *it* delegated, so own and nested
    /// spend stay distinguishable; [`total_usage`](Self::total_usage) covers the
    /// subtree.
    ///
    /// A custom tool that reported **several** runs from one call (see
    /// [`ToolContext::report_delegated_run`]) gets their combination: `usage`,
    /// `turns` and `cost_usd` summed over those runs' own turns, `sub_agents`
    /// over what they in turn delegated. The subtree totals stay exact; only
    /// the split between the individual runs is not kept.
    ///
    /// Do not add these to [`AgentEvent::AgentEnd`]'s stats as well: the loop
    /// already folds every delegation into that run's
    /// [`sub_agents`](Self::sub_agents).
    pub fn from_sub_agent_result(result: &ToolResult) -> Option<SessionStats> {
        serde_json::from_value(result.details.get(SUB_AGENT_STATS_KEY)?.clone()).ok()
    }

    /// Fraction of prompt tokens served from cache across the whole session
    /// (0.0–1.0). Delegates to [`Usage::cache_hit_rate`] so there is one
    /// definition of the metric rather than two that can drift.
    pub fn cache_hit_rate(&self) -> f64 {
        self.usage.cache_hit_rate()
    }

    /// Fold one turn's usage into the rollup, costing it when rates are known.
    ///
    /// `total_tokens` is deliberately not summed. It is a per-response provider
    /// report, and the providers disagree on it: `anthropic.rs` never sets it
    /// at all, `bedrock.rs` computes `input + output` and so excludes cache,
    /// and the rest pass through a payload value that includes cached tokens.
    /// Summing it would launder that inconsistency into a session-level number
    /// that reads as authoritative and is 0 for every Anthropic run. The four
    /// components sum cleanly; derive a total from those.
    ///
    /// Cost accrues per turn rather than once at the end. Today that is
    /// arithmetically identical — `CostConfig::cost_usd` is linear in every
    /// `Usage` field and `config.model_config` is fixed for the life of a run
    /// (`run_loop` holds `&AgentLoopConfig`, and `set_model` needs `&mut self`).
    /// It is written this way so a per-turn model override stays correct if one
    /// is ever introduced, and so `cost_usd` reflects whether any turn was
    /// priceable rather than requiring a separate check.
    ///
    /// Pricing follows [`combine_cost`]: a turn with non-zero usage and no
    /// configured rates makes [`cost_usd`](Self::cost_usd) `None` for the rest
    /// of the run, rather than leaving a partial sum that reads as the whole.
    pub(crate) fn record_turn(
        &mut self,
        usage: &Usage,
        cost: Option<&crate::provider::CostConfig>,
    ) {
        let turn_cost = cost.map(|c| c.cost_usd(usage));
        self.cost_usd = combine_cost(&self.usage, self.cost_usd, usage, turn_cost);
        self.usage = add_usage(&self.usage, usage);
        self.turns = self.turns.saturating_add(1);
    }
}

/// Key under which a sub-agent's [`SessionStats`] ride in
/// [`ToolResult::details`]. Read it with
/// [`SessionStats::from_sub_agent_result`] rather than by hand.
pub const SUB_AGENT_STATS_KEY: &str = "sub_agent_stats";

fn add_usage(a: &Usage, b: &Usage) -> Usage {
    Usage {
        input: a.input.saturating_add(b.input),
        output: a.output.saturating_add(b.output),
        cache_read: a.cache_read.saturating_add(b.cache_read),
        cache_write: a.cache_write.saturating_add(b.cache_write),
        // Not summed — see `SessionStats::record_turn`.
        total_tokens: 0,
    }
}

fn usage_is_zero(u: &Usage) -> bool {
    u.input == 0 && u.output == 0 && u.cache_read == 0 && u.cache_write == 0
}

/// Spend that happened but has no price: non-zero usage, no cost.
fn is_unpriced(usage: &Usage, cost: Option<f64>) -> bool {
    cost.is_none() && !usage_is_zero(usage)
}

/// The cost of two pieces of spend together — the one rule every rollup
/// shares ([`SessionStats::record_turn`], [`SessionStats::total_cost_usd`],
/// [`SubAgentSpend::merge`], `Agent::total_cost_usd`).
///
/// `None` if either piece is unpriced — sticky, so a later priced piece never
/// revives a sum that silently skipped part of the bill. A zero-usage piece
/// with no cost needs no price and does not poison the other. `None` too when
/// neither piece carries a cost at all (nothing was spent).
///
/// Both naive `Option` sums are wrong: `zip` drops a priced piece when the
/// other merely spent nothing, and `unwrap_or(0.0)` turns unpriced into free.
pub(crate) fn combine_cost(
    a_usage: &Usage,
    a_cost: Option<f64>,
    b_usage: &Usage,
    b_cost: Option<f64>,
) -> Option<f64> {
    if is_unpriced(a_usage, a_cost) || is_unpriced(b_usage, b_cost) {
        return None;
    }
    match (a_cost, b_cost) {
        (None, None) => None,
        (a, b) => Some(a.unwrap_or(0.0) + b.unwrap_or(0.0)),
    }
}

/// Spend of delegated [`SubAgentTool`](crate::SubAgentTool) runs, carried as
/// [`SessionStats::sub_agents`] and returned by
/// [`Agent::sub_agent_spend`](crate::Agent::sub_agent_spend).
///
/// Every figure covers the whole delegation tree: a sub-agent's own nested
/// sub-agents are included, so one top-level number accounts for all of it.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct SubAgentSpend {
    /// Provider usage summed over every delegated run. `total_tokens` stays 0,
    /// as in [`SessionStats::usage`].
    #[serde(default)]
    pub usage: Usage,
    /// Dollar cost of [`usage`](Self::usage), each run priced at its own
    /// model's rates — a sub-agent on a cheaper model is billed as such, never
    /// re-priced at the parent's.
    ///
    /// `None` is never "free", but it means one of two things: the delegated
    /// spend **cannot be priced** — sticky as soon as any delegated spend came
    /// from a model with `cost: None`, because a sum that silently
    /// skips the unpriced part is the under-report this field exists to
    /// remove — or there was **nothing to price** (no delegation, or runs that
    /// reported no usage). [`is_unpriced`](Self::is_unpriced) tells them
    /// apart.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost_usd: Option<f64>,
    /// Sub-agent invocations, nested ones included. Non-zero means delegation
    /// happened even when it cost nothing measurable.
    #[serde(default)]
    pub runs: u32,
}

impl SubAgentSpend {
    /// Whether nothing was delegated.
    pub fn is_empty(&self) -> bool {
        self.runs == 0 && usage_is_zero(&self.usage) && self.cost_usd.is_none()
    }

    /// Whether delegated spend happened that cannot be priced: non-zero
    /// [`usage`](Self::usage) with no [`cost_usd`](Self::cost_usd). `false`
    /// for an empty bucket, where `cost_usd` is `None` because there is
    /// nothing to price.
    pub fn is_unpriced(&self) -> bool {
        is_unpriced(&self.usage, self.cost_usd)
    }

    /// Fold another bucket into this one — for a caller accumulating across
    /// runs.
    ///
    /// Use this rather than adding fields by hand: it keeps
    /// [`cost_usd`](Self::cost_usd) `None` once any part is unpriced, which a
    /// plain `Option` sum gets wrong in both directions.
    pub fn merge(&mut self, other: &SubAgentSpend) {
        self.cost_usd = combine_cost(&self.usage, self.cost_usd, &other.usage, other.cost_usd);
        self.usage = add_usage(&self.usage, &other.usage);
        self.runs = self.runs.saturating_add(other.runs);
    }

    /// Fold in one sub-agent run, given that run's own stats — its own spend
    /// and, recursively, everything it delegated.
    pub(crate) fn record_run(&mut self, child: &SessionStats) {
        let mut subtree = SubAgentSpend {
            usage: child.usage.clone(),
            cost_usd: child.cost_usd,
            runs: 1,
        };
        subtree.merge(&child.sub_agents);
        self.merge(&subtree);
    }
}

/// Where a [`SubAgentTool`](crate::SubAgentTool) reports its run's stats to the
/// loop that invoked it. A side channel rather than the tool's return value
/// because a failed delegation returns `Err(ToolError)`, which has nowhere to
/// carry them — and the spend of a failed run is still spend.
///
/// A `Vec` because [`ToolContext`] is `Clone`: a custom tool may hand clones to
/// several sub-agents within one call, and each run must be counted.
pub(crate) type SubAgentReport = Arc<std::sync::Mutex<Vec<SessionStats>>>;

#[cfg(test)]
mod spend_rollup_tests {
    use super::*;
    use crate::provider::CostConfig;

    fn u(input: u64) -> Usage {
        Usage {
            input,
            ..Usage::default()
        }
    }

    /// $1 per million input tokens, so `u(1_000_000)` costs exactly $1.
    fn priced() -> CostConfig {
        CostConfig::new(1.0, 0.0)
    }

    /// An unpriced turn that spent tokens poisons the run's cost, and a
    /// priced turn after it must not revive a partial sum — the same rule as
    /// `SubAgentSpend::merge`.
    #[test]
    fn record_turn_unpriced_then_priced_is_unknown() {
        let mut stats = SessionStats::default();
        stats.record_turn(&u(1_000_000), None);
        assert_eq!(stats.cost_usd, None);
        stats.record_turn(&u(1_000_000), Some(&priced()));
        assert_eq!(stats.cost_usd, None, "a partial sum would under-report");
        assert!(stats.is_unpriced());
        assert_eq!(stats.usage, u(2_000_000));
        assert_eq!(stats.turns, 2);
    }

    #[test]
    fn record_turn_priced_then_unpriced_is_unknown() {
        let mut stats = SessionStats::default();
        stats.record_turn(&u(1_000_000), Some(&priced()));
        assert_eq!(stats.cost_usd, Some(1.0));
        stats.record_turn(&u(1), None);
        assert_eq!(stats.cost_usd, None);
    }

    /// A zero-rate config means free, not unknown: it prices at 0.0 and does
    /// not poison the sum.
    #[test]
    fn record_turn_free_model_prices_at_zero() {
        let mut stats = SessionStats::default();
        stats.record_turn(&u(1_000_000), Some(&priced()));
        stats.record_turn(&u(1_000_000), Some(&CostConfig::default()));
        assert_eq!(stats.cost_usd, Some(1.0));
        assert!(!stats.is_unpriced());
    }

    #[test]
    fn record_turn_sums_priced_turns_and_ignores_empty_unpriced_ones() {
        let mut stats = SessionStats::default();
        // No usage reported: nothing to price, so no poison.
        stats.record_turn(&Usage::default(), None);
        assert_eq!(stats.cost_usd, None);
        assert!(!stats.is_unpriced(), "nothing spent is not unpriced");
        stats.record_turn(&u(1_000_000), Some(&priced()));
        stats.record_turn(&u(2_000_000), Some(&priced()));
        assert_eq!(stats.cost_usd, Some(3.0));
        assert!(!stats.is_unpriced());
    }

    #[test]
    fn usage_sums_saturate_instead_of_overflowing() {
        let mut a = SubAgentSpend {
            usage: u(u64::MAX - 1),
            cost_usd: Some(1.0),
            runs: u32::MAX,
        };
        a.merge(&a.clone());
        assert_eq!(a.usage.input, u64::MAX);
        assert_eq!(a.runs, u32::MAX);
    }
}

/// What a summarization request produced, carried by
/// [`AgentEvent::ContextCompacted`].
///
/// Weigh [`usage`](Self::usage) against the event's `tokens_before -
/// tokens_after` to decide whether an LLM compaction strategy earns its keep.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct SummaryStats {
    /// Messages the briefing replaced.
    ///
    /// Zero when a briefing was produced but could not be kept — the request
    /// was still paid for, so the event still reports it, but `method` will be
    /// [`CompactionMethod::Deterministic`].
    pub messages_summarized: usize,
    /// Tokens the summarization request itself consumed.
    pub usage: Usage,
    /// Dollar cost of `usage`, when the summarization model's rates are
    /// configured (see [`CostConfig`](crate::provider::CostConfig)).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cost_usd: Option<f64>,
}

impl SummaryStats {
    /// A record of one summarization request.
    pub fn new(messages_summarized: usize, usage: Usage, cost_usd: Option<f64>) -> Self {
        Self {
            messages_summarized,
            usage,
            cost_usd,
        }
    }
}

/// Which compaction path produced an [`AgentEvent::ContextCompacted`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub enum CompactionMethod {
    /// History was replaced by an LLM-written summary.
    Summarized,
    /// Deterministic tiered compaction ran: truncate → summarize → drop.
    ///
    /// On [`LlmCompaction`](crate::LlmCompaction) this means no briefing made
    /// it into the result — none was ready, one was discarded as stale, or one
    /// was produced but could not be kept within the budget. The loop stayed
    /// unblocked; the compaction was lossy. Check `summary` to tell a free
    /// fallback from one that still paid for a request.
    Deterministic,
}

/// Incremental content delta carried by [`AgentEvent::MessageUpdate`].
///
/// Serializes internally tagged (`{"type":"text","delta":"..."}`); see the
/// wire-format contract on [`AgentEvent`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(
    tag = "type",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
#[non_exhaustive]
pub enum StreamDelta {
    Text { delta: String },
    Thinking { delta: String },
    ToolCallDelta { delta: String },
}

// ---------------------------------------------------------------------------
// Agent context (passed to the loop)
// ---------------------------------------------------------------------------

pub struct AgentContext {
    pub system_prompt: String,
    pub messages: Vec<AgentMessage>,
    pub tools: Vec<Box<dyn AgentTool>>,
}

// ---------------------------------------------------------------------------
// Input filtering
// ---------------------------------------------------------------------------

/// Result of applying an input filter to a user message.
#[derive(Debug, Clone)]
pub enum FilterResult {
    /// Message passes unchanged.
    Pass,
    /// Message passes, but append a warning to context for the LLM to see.
    Warn(String),
    /// Message is rejected. Agent loop returns immediately.
    Reject(String),
}

/// Synchronous filter applied to user input before the LLM call.
///
/// Implement this for injection detection, content moderation, PII redaction, etc.
/// Filters run in the hot path and must be fast — use `before_turn` callbacks
/// for async moderation (external API calls).
pub trait InputFilter: Send + Sync {
    fn filter(&self, text: &str) -> FilterResult;
}

// ---------------------------------------------------------------------------
// Tool middleware (permissions)
// ---------------------------------------------------------------------------

/// Decision returned by a [`ToolMiddleware`] before a tool executes.
///
/// Deliberately NOT `#[non_exhaustive]` (same policy as [`StopReason`]):
/// this is a control-flow enum — a new variant should be a compile error for
/// matchers, not a silent wildcard fallthrough. Interactive flows like
/// "ask the user" need no variant: the hook is `async`, so prompt inside the
/// middleware and return `Allow`/`Deny`.
#[derive(Debug, Clone)]
pub enum ToolDecision {
    /// Execute the tool with the current arguments.
    Allow,
    /// Execute the tool with replacement arguments (e.g. a sandboxed path).
    Modify(serde_json::Value),
    /// Block the call. The reason is returned to the LLM as an error tool
    /// result so it can adapt (pick another tool, ask the user, ...); the
    /// loop itself continues.
    Deny(String),
}

/// Borrowed view of a pending tool call, passed to
/// [`ToolMiddleware::before_tool`].
///
/// Marked `#[non_exhaustive]`: fields may be added in minor releases (turn
/// number, history access, ...) without breaking middleware implementations.
/// Constructed by the loop; middleware only reads it.
#[derive(Debug)]
#[non_exhaustive]
pub struct ToolCallRequest<'a> {
    /// Provider-assigned id of this tool call.
    pub tool_call_id: &'a str,
    /// Name of the tool the model wants to run.
    pub tool_name: &'a str,
    /// Arguments as the model provided them (possibly rewritten by earlier
    /// middleware in the chain).
    pub args: &'a serde_json::Value,
}

/// Async hook that gates every tool call — the mechanism behind permission
/// prompts, policy engines, and argument rewriting.
///
/// yoagent ships the mechanism, not a policy: install middleware via
/// [`Agent::with_tool_middleware`](crate::Agent::with_tool_middleware) (or
/// [`AgentLoopConfig::tool_middleware`](crate::agent_loop::AgentLoopConfig))
/// and decide per call. Middleware run in a chain: each may rewrite the
/// arguments seen by later ones; the first `Deny` wins. With no middleware
/// installed, every call is allowed — behavior is unchanged.
///
/// The hook is `async` so an interactive app can prompt a human. Under the
/// default [`ToolExecutionStrategy::Parallel`], middleware for parallel tool
/// calls runs concurrently — serialize approval prompts inside your
/// implementation (or use `Sequential`) if you need one-at-a-time UX.
///
/// Middleware never sees a call whose arguments failed to resolve to a JSON
/// object (cut off mid-stream, or not an object — see
/// [`parse_tool_arguments`](crate::provider::parse_tool_arguments)). The loop
/// answers such a call with an error tool result *before* the chain runs, so
/// there is no real call to approve, deny or rewrite.
#[async_trait::async_trait]
pub trait ToolMiddleware: Send + Sync {
    async fn before_tool(&self, call: &ToolCallRequest<'_>) -> ToolDecision;
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

impl fmt::Display for StopReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Stop => write!(f, "stop"),
            Self::Length => write!(f, "length"),
            Self::ToolUse => write!(f, "toolUse"),
            Self::Error => write!(f, "error"),
            Self::Aborted => write!(f, "aborted"),
            Self::Refusal => write!(f, "refusal"),
        }
    }
}

/// Freezes the serde wire contract for `AgentEvent` and `StreamDelta`.
///
/// Both enums are documented as a stable wire format for websocket fanout
/// servers, TypeScript clients and JSONL pipes, so a changed tag or a variant
/// that quietly serializes however serde happens to derive it are breaking
/// changes for consumers who never rebuild against this crate.
///
/// **Scope.** This freezes the `"type"` tag of every variant, that every
/// variant has a sample, and that each sample round-trips. It does **not**
/// freeze payload shape: a `#[serde(rename)]` on a field, a field added or
/// removed, or a changed field type all pass here. Field names are pinned
/// only where `tests/serialization_test.rs` asserts them by literal.
///
/// This lives in the defining crate on purpose. Both enums are
/// `#[non_exhaustive]`, so an integration test *cannot* match them
/// exhaustively — its match needs a `_` arm, and a wildcard turns "adding a
/// variant fails to compile" into "adding a variant is silently untested".
/// Inside this crate exhaustiveness still applies.
#[cfg(test)]
mod wire_tag_freeze {
    use super::*;
    use std::collections::BTreeSet;

    /// Declares the frozen tag **and** a sample value for every variant of a
    /// `#[serde(tag = "type")]` enum, from a single list.
    ///
    /// This is the fix for the class of bug that made the old guard useless: a
    /// hand-written sample list and a hand-written variant count could never
    /// notice a *new* variant, because a new variant appears in neither. Here
    /// the generated match has no wildcard, so adding a variant fails to
    /// compile — and the only way to fix that is to add a line below, which
    /// supplies the sample in the same breath.
    ///
    /// Three mistakes are caught by the compiler rather than by luck: a
    /// duplicated pattern makes the later arm `unreachable_patterns` (an error
    /// under CI's `-Dwarnings`), a sample of the wrong type does not compile,
    /// and a missing arm is a non-exhaustive match.
    ///
    /// The specifier is `pat_param`, not `pat`, and that is load-bearing: `pat`
    /// would accept an or-pattern, letting someone answer the compile error by
    /// widening an unrelated arm (`TurnStart | NewVariant => "turnStart"`)
    /// instead of adding a line — leaving the new variant with no sample and no
    /// coverage, which is exactly the hole this macro exists to close. No arm
    /// can legitimately need one, since two variants cannot share a tag.
    macro_rules! wire_freeze {
        ($ty:ty, $tag_of:ident, $samples:ident, $($pat:pat_param => $tag:literal = $sample:expr),+ $(,)?) => {
            /// The frozen `"type"` tag per variant. Changing one breaks every
            /// deployed wire client — do not edit casually.
            fn $tag_of(v: &$ty) -> &'static str {
                match v { $($pat => $tag,)+ }
            }

            /// One sample per variant, in declaration order. The tests re-derive
            /// each tag from its sample rather than trusting that order — see
            /// [`assert_frozen`].
            fn $samples() -> Vec<$ty> { vec![$($sample,)+] }
        };
    }

    /// A populated assistant message. Every field is deliberately non-default:
    /// a round-trip cannot detect a field that `#[serde(skip)]` drops if the
    /// value it reconstructs is the default anyway.
    fn msg() -> AgentMessage {
        AgentMessage::Llm(Message::Assistant {
            content: vec![Content::Text { text: "hi".into() }],
            stop_reason: StopReason::ToolUse,
            model: "mock-1".into(),
            provider: "mock".into(),
            usage: Usage {
                input: 11,
                output: 22,
                cache_read: 33,
                cache_write: 44,
                total_tokens: 110,
            },
            timestamp: 7,
            error_message: Some("boom".into()),
        })
    }

    fn tool_result() -> ToolResult {
        ToolResult {
            content: vec![Content::Text { text: "ok".into() }],
            details: serde_json::json!({"exitCode": 0}),
        }
    }

    fn tool_result_message() -> Message {
        Message::ToolResult {
            tool_call_id: "tc-1".into(),
            tool_name: "bash".into(),
            content: vec![Content::Text { text: "ok".into() }],
            is_error: false,
            timestamp: 9,
        }
    }

    wire_freeze! {
        AgentEvent, expected_event_tag, event_samples,
        AgentEvent::AgentStart => "agentStart" = AgentEvent::AgentStart,
        AgentEvent::AgentEnd { .. } => "agentEnd" = AgentEvent::agent_end(
            vec![msg()],
            SessionStats {
                usage: Usage {
                    input: 5,
                    output: 6,
                    cache_read: 7,
                    cache_write: 8,
                    total_tokens: 26,
                },
                turns: 3,
                cost_usd: Some(0.02),
                compactions: 1,
                sub_agents: SubAgentSpend {
                    usage: Usage {
                        input: 50,
                        output: 60,
                        cache_read: 70,
                        cache_write: 80,
                        total_tokens: 0,
                    },
                    cost_usd: Some(0.2),
                    runs: 2,
                },
            },
        ),
        AgentEvent::TurnStart => "turnStart" = AgentEvent::TurnStart,
        AgentEvent::TurnEnd { .. } => "turnEnd" = AgentEvent::TurnEnd {
            message: msg(),
            tool_results: vec![tool_result_message()],
        },
        AgentEvent::MessageStart { .. } => "messageStart"
            = AgentEvent::MessageStart { message: msg() },
        AgentEvent::MessageUpdate { .. } => "messageUpdate" = AgentEvent::MessageUpdate {
            message: msg(),
            delta: StreamDelta::Text { delta: "hi".into() },
        },
        AgentEvent::MessageEnd { .. } => "messageEnd"
            = AgentEvent::MessageEnd { message: msg() },
        AgentEvent::ToolExecutionStart { .. } => "toolExecutionStart"
            = AgentEvent::ToolExecutionStart {
                tool_call_id: "tc-1".into(),
                tool_name: "bash".into(),
                args: serde_json::json!({"command": "ls"}),
            },
        AgentEvent::ToolExecutionUpdate { .. } => "toolExecutionUpdate"
            = AgentEvent::ToolExecutionUpdate {
                tool_call_id: "tc-1".into(),
                tool_name: "bash".into(),
                partial_result: tool_result(),
            },
        AgentEvent::ToolExecutionEnd { .. } => "toolExecutionEnd"
            = AgentEvent::ToolExecutionEnd {
                tool_call_id: "tc-1".into(),
                tool_name: "bash".into(),
                result: tool_result(),
                is_error: false,
            },
        AgentEvent::ProgressMessage { .. } => "progressMessage"
            = AgentEvent::ProgressMessage {
                tool_call_id: "tc-1".into(),
                tool_name: "bash".into(),
                text: "50% done".into(),
            },
        AgentEvent::InputRejected { .. } => "inputRejected"
            = AgentEvent::InputRejected { reason: "injection detected".into() },
        AgentEvent::LoopDetected { .. } => "loopDetected"
            = AgentEvent::loop_detected("bash", 3, false),
        AgentEvent::ContextCompacted { .. } => "contextCompacted"
            = AgentEvent::ContextCompacted {
                method: CompactionMethod::Summarized,
                messages_before: 40,
                messages_after: 13,
                tokens_before: 96_500,
                tokens_after: 41_200,
                summary: Some(SummaryStats::new(
                    28,
                    Usage {
                        input: 54_000,
                        output: 900,
                        cache_read: 0,
                        cache_write: 0,
                        total_tokens: 54_900,
                    },
                    Some(0.17),
                )),
            },
    }

    wire_freeze! {
        StreamDelta, expected_delta_tag, delta_samples,
        StreamDelta::Text { .. } => "text" = StreamDelta::Text { delta: "hi".into() },
        StreamDelta::Thinking { .. } => "thinking"
            = StreamDelta::Thinking { delta: "hmm".into() },
        StreamDelta::ToolCallDelta { .. } => "toolCallDelta"
            = StreamDelta::ToolCallDelta { delta: "{}".into() },
    }

    /// Every key in `v`, recursively, as `path -> key` pairs.
    ///
    /// Recursive on purpose. Checking only the top level would pass a
    /// snake_case key one level down — `message.usage.total_tokens` reaches
    /// clients just as surely as `turnEnd.toolResults` does, and the nested
    /// payload structs carry their own `rename_all`, which nothing else here
    /// would notice going missing.
    fn all_keys(v: &serde_json::Value, path: &str, out: &mut Vec<(String, String)>) {
        match v {
            serde_json::Value::Object(map) => {
                for (k, child) in map {
                    out.push((path.to_string(), k.clone()));
                    all_keys(child, &format!("{path}.{k}"), out);
                }
            }
            serde_json::Value::Array(items) => {
                for (i, child) in items.iter().enumerate() {
                    all_keys(child, &format!("{path}[{i}]"), out);
                }
            }
            _ => {}
        }
    }

    /// Asserts the frozen contract for one sample: the declared tag is the one
    /// serde emits, no payload key anywhere in the value contains an
    /// underscore, and the value survives a JSON round-trip.
    ///
    /// `seen` collects tags so a mis-paired sample is caught **when it
    /// duplicates another variant** — which is the case that costs coverage,
    /// since some variant is then left unsampled. Two samples swapped between
    /// lines is invisible here, and harmless: the set of samples is unchanged
    /// and every tag is still checked against serde.
    fn assert_frozen<T>(sample: &T, declared: &'static str, seen: &mut BTreeSet<&'static str>)
    where
        T: serde::Serialize + serde::de::DeserializeOwned + std::fmt::Debug + PartialEq,
    {
        let v = serde_json::to_value(sample).expect("serialize");

        assert_eq!(
            v["type"], declared,
            "wire tag drifted: {sample:?} serializes as {} but wire_freeze! declares {declared}. \
             Changing a tag breaks every deployed client — if this is intentional it is a \
             breaking change, not a test fix",
            v["type"]
        );

        let mut keys = Vec::new();
        all_keys(&v, declared, &mut keys);
        for (path, key) in &keys {
            assert!(
                !key.contains('_'),
                "payload key {key:?} at {path} is not camelCase. Every struct on this wire \
                 carries rename_all = \"camelCase\" and TS clients hardcode these names, so a \
                 snake_case key here means a rename attribute is missing"
            );
        }

        let back: T = serde_json::from_value(v).expect("round-trip deserialize");
        assert_eq!(
            &back, sample,
            "{declared} did not survive a JSON round-trip"
        );

        assert!(
            seen.insert(declared),
            "two samples serialize as {declared} — a sample in wire_freeze! does not match \
             the pattern on its own line, so some variant has no sample at all"
        );
    }

    #[test]
    fn every_event_variant_is_frozen_tagged_and_round_trips() {
        let mut seen = BTreeSet::new();
        for sample in &event_samples() {
            assert_frozen(sample, expected_event_tag(sample), &mut seen);
        }
    }

    #[test]
    fn every_delta_variant_is_frozen_tagged_and_round_trips() {
        let mut seen = BTreeSet::new();
        for sample in &delta_samples() {
            assert_frozen(sample, expected_delta_tag(sample), &mut seen);
        }
    }
}

#[cfg(test)]
mod thinking_level_tests {
    use super::ThinkingLevel;

    #[test]
    fn serde_names_are_lowercase_and_old_values_still_load() {
        for (level, name) in [
            (ThinkingLevel::Off, "off"),
            (ThinkingLevel::Minimal, "minimal"),
            (ThinkingLevel::Low, "low"),
            (ThinkingLevel::Medium, "medium"),
            (ThinkingLevel::High, "high"),
            (ThinkingLevel::XHigh, "xhigh"),
            (ThinkingLevel::Max, "max"),
        ] {
            let json = serde_json::to_string(&level).unwrap();
            assert_eq!(json, format!("\"{name}\""));
            let back: ThinkingLevel = serde_json::from_str(&json).unwrap();
            assert_eq!(back, level);
        }
    }
}
