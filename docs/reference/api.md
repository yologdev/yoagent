# API Reference

## Top-Level Functions

### `agent_loop()`

```rust
pub async fn agent_loop(
    prompts: Vec<AgentMessage>,
    context: &mut AgentContext,
    config: &AgentLoopConfig,
    tx: mpsc::UnboundedSender<AgentEvent>,
    cancel: CancellationToken,
) -> Vec<AgentMessage>
```

Start an agent loop with new prompt messages. Returns all messages generated during the run.

### `agent_loop_continue()`

```rust
pub async fn agent_loop_continue(
    context: &mut AgentContext,
    config: &AgentLoopConfig,
    tx: mpsc::UnboundedSender<AgentEvent>,
    cancel: CancellationToken,
) -> Vec<AgentMessage>
```

Resume from existing context. The last message must not be an assistant message.

### `default_tools()`

*(feature `native`, on by default)*

```rust
pub fn default_tools() -> Vec<Box<dyn AgentTool>>
```

Returns: `BashTool`, `ReadFileTool`, `WriteFileTool`, `EditFileTool`, `ListFilesTool`, `SearchTool`.

## Agent Struct

High-level stateful wrapper around the agent loop.

### Construction

```rust
let agent = Agent::from_config(ModelConfig::anthropic("claude-sonnet-5", "Claude Sonnet 5"));
```

| Signature | Description |
|-----------|-------------|
| `Agent::from_config(config: ModelConfig) -> Self` | Build from a `ModelConfig` — auto-selects the built-in provider for the config's protocol and resolves the API key from the provider's conventional env var (primary constructor) |
| `Agent::from_provider(provider: impl StreamProvider + 'static, config: ModelConfig) -> Self` | Build from an explicit provider plus its `ModelConfig` (custom providers and test doubles — pair with `ModelConfig::mock()`) |
| `Agent::from_config_with(registry: &ProviderRegistry, config: ModelConfig) -> Result<Self, AgentBuildError>` | Like `from_config`, but resolves the provider from a caller-supplied registry |

`Agent::new(provider)` with `with_model` / `with_model_config` (and the same on `SubAgentTool`) still compiles but is deprecated since 0.10.0 and hidden from the docs since 0.25; it will be removed in 1.0.

### Builder Methods

All return `Self` for chaining (unless noted as `Result`).

**Core**

| Method | Description |
|--------|-------------|
| `with_system_prompt(prompt) -> Self` | Set the system prompt |
| `with_api_key(key) -> Self` | Override the env-resolved API key |
| `with_thinking(level: ThinkingLevel) -> Self` | Set thinking level (`Off`, `Minimal`, `Low`, `Medium`, `High`, `XHigh`, `Max`) |
| `with_max_tokens(max: u32) -> Self` | Set max output tokens |
| `with_temperature(t: f32) -> Self` | Set the sampling temperature (the newest reasoning models reject it) |

**Tools & Integrations**

| Method | Description |
|--------|-------------|
| `with_tools(tools: Vec<Box<dyn AgentTool>>) -> Self` | Set tools (replaces existing) |
| `with_tool_source(source: impl ToolSource) -> Self` | Add a tool source consulted once at the start of every run; its tools are offered for that run only (the agent's own tools win on name collisions). See [Tools](../concepts/tools.md#tools-that-change-at-runtime-toolsource) |
| `with_tool_middleware(m: impl ToolMiddleware) -> Self` | Add an approve/modify/deny hook gating every tool call |
| `with_sub_agent(sub: SubAgentTool) -> Self` | Add a sub-agent tool |
| `with_shared_state(state: SharedState) -> Self` | Register the `shared_state` tool and stash truncated tool output under a retrievable key |
| `with_skills(skills: SkillSet) -> Self` | Load skills and append their index to the system prompt |
| `async with_mcp_server_stdio(command, args, env) -> Result<Self, McpError>` | Connect to MCP server via stdio and add its tools *(feature `native`)* |
| `async with_mcp_server_http(url) -> Result<Self, McpError>` | Connect to MCP server via HTTP and add its tools |
| `async with_openapi_file(path, config, filter) -> Result<Self, OpenApiError>` | Load tools from an OpenAPI spec file *(requires `openapi` feature)* |
| `async with_openapi_url(url, config, filter) -> Result<Self, OpenApiError>` | Fetch spec from URL and add tools *(requires `openapi` feature)* |
| `with_openapi_spec(spec_str, config, filter) -> Result<Self, OpenApiError>` | Parse spec string and add tools *(requires `openapi` feature)* |

**Context & Limits**

| Method | Description |
|--------|-------------|
| `with_context_config(config: ContextConfig) -> Self` | Set context compaction config |
| `with_execution_limits(limits: ExecutionLimits) -> Self` | Set execution limits (max turns, tokens, duration) |
| `with_compaction_strategy(strategy: impl CompactionStrategy) -> Self` | Set a custom compaction strategy |
| `without_context_management() -> Self` | Disable automatic context compaction and execution limits |

**Behavior**

| Method | Description |
|--------|-------------|
| `with_messages(msgs: Vec<AgentMessage>) -> Self` | Pre-load message history |
| `with_cache_config(config: CacheConfig) -> Self` | Set prompt caching configuration |
| `with_tool_execution(strategy: ToolExecutionStrategy) -> Self` | Set tool execution strategy (`Parallel`, `Sequential`, `Batched`) |
| `with_retry_config(config: RetryConfig) -> Self` | Set retry configuration |
| `with_input_filter(filter: impl InputFilter) -> Self` | Add an input filter (runs on user messages before LLM call) |
| `with_async_input_filter(filter: impl AsyncInputFilter) -> Self` | Add an input filter that awaits (same list and semantics) |
| `with_turn_hook(hook: impl TurnHook) -> Self` | Add an async per-request hook that may append one transient note to the latest user turn |
| `with_decision_model(model: DecisionModel) -> Self` | *(feature `decision`)* Advisory skill/tool hints from a decision model; never blocks; needs skills or 40+ tools |
| `with_decision_advisory(advisory: Advisory) -> Self` | *(feature `decision`)* The same, with explicit thresholds and timeout |
| `with_tool_gate(gate: ToolGate) -> Self` | *(feature `decision`)* Deny destructive, unrequested tool calls; runs last; fails closed |
| `with_input_guard(guard: InputGuard) -> Self` | *(feature `decision`)* Reject injection / harmful prompts; fails closed; panics if the guard has no checks |

**Callbacks**

| Method | Description |
|--------|-------------|
| `on_before_turn(f: Fn(&[AgentMessage], usize) -> bool) -> Self` | Called before each LLM call; return `false` to abort |
| `on_after_turn(f: Fn(&[AgentMessage], &Usage)) -> Self` | Called after each LLM response and tool execution |
| `on_error(f: Fn(&str)) -> Self` | Called when the LLM returns `StopReason::Error` (not for cancellations, which end `StopReason::Aborted` since 0.23) |

### Prompting

| Method | Description |
|--------|-------------|
| `async prompt(text) -> UnboundedReceiver<AgentEvent>` | Send a text prompt; spawns the loop concurrently and returns the event stream immediately for real-time consumption |
| `async prompt_messages(messages) -> UnboundedReceiver<AgentEvent>` | Send messages as prompt; spawns concurrently, returns event stream immediately |
| `async prompt_with_sender(text, tx: UnboundedSender<AgentEvent>)` | Send a text prompt, streaming events to a caller-provided sender; blocks until the loop finishes |
| `async prompt_messages_with_sender(messages, tx)` | Send messages, streaming events to a caller-provided sender; blocks until the loop finishes |
| `async prompt_structured::<T>(text, schema: serde_json::Value) -> Result<T, StructuredPromptError>` | Run to completion and parse a schema-constrained reply into `T`. See [Structured Outputs](../concepts/structured-outputs.md) |
| `async continue_loop() -> UnboundedReceiver<AgentEvent>` | Resume from current context; spawns concurrently, returns event stream immediately |
| `async continue_loop_with_sender(tx: UnboundedSender<AgentEvent>)` | Resume from current context, streaming events to a caller-provided sender; blocks until the loop finishes |
| `async finish()` | Await a pending spawned loop and restore tools/messages/state. Called automatically at the start of each prompt method |

### State Access

| Method | Description |
|--------|-------------|
| `messages() -> &[AgentMessage]` | Get the full message history |
| `is_streaming() -> bool` | Whether the agent is currently running |
| `session_cost_usd() -> Option<f64>` | Cost of the current history at the current rates (excludes sub-agents); `None` = unpriced |
| `sub_agent_spend() -> &SubAgentSpend` | What sub-agents spent on this agent's behalf since construction or `reset()` |
| `total_cost_usd() -> Option<f64>` | Everything this agent's runs spent, sub-agents included |
| `total_usage() -> Usage` | Token usage over the same window as `total_cost_usd()` |

### State Mutation

| Method | Description |
|--------|-------------|
| `set_tools(tools: Vec<Box<dyn AgentTool>>)` | Replace the tool set |
| `set_model(config: ModelConfig)` | Switch model mid-session; re-resolves the env key, re-selects the provider only if it was not supplied explicitly |
| `reprice()` | Re-run the price lookup for the current `ModelConfig` |
| `clear_messages()` | Clear all messages |
| `append_message(msg: AgentMessage)` | Add a message to history |
| `replace_messages(msgs: Vec<AgentMessage>)` | Replace all messages |
| `save_messages() -> Result<String, serde_json::Error>` | Serialize message history to JSON |
| `restore_messages(json: &str) -> Result<(), serde_json::Error>` | Restore message history from JSON |

### Steering & Follow-Up Queues

| Method | Description |
|--------|-------------|
| `steer(msg: AgentMessage)` | Queue a steering message (interrupts mid-tool-execution) |
| `follow_up(msg: AgentMessage)` | Queue a follow-up message (processed after agent finishes) |
| `clear_steering_queue()` | Clear pending steering messages |
| `clear_follow_up_queue()` | Clear pending follow-up messages |
| `clear_all_queues()` | Clear both queues |
| `steer_all(msgs: Vec<AgentMessage>)` | Queue multiple steering messages under one lock |
| `follow_up_all(msgs: Vec<AgentMessage>)` | Queue multiple follow-up messages under one lock |
| `steering_queue_snapshot() -> Vec<AgentMessage>` | Copy of pending steering messages (does not consume) |
| `follow_up_queue_snapshot() -> Vec<AgentMessage>` | Copy of pending follow-up messages |
| `steering_queue_len() -> usize` | Number of pending steering messages |
| `follow_up_queue_len() -> usize` | Number of pending follow-up messages |
| `take_steering_queue() -> Vec<AgentMessage>` | Atomically drain and return pending steering messages (messages already picked up by the loop are not included) |
| `take_follow_up_queue() -> Vec<AgentMessage>` | Atomically drain and return pending follow-up messages |
| `set_steering_mode(mode: QueueMode)` | Set delivery mode: `OneAtATime` or `All` |
| `set_follow_up_mode(mode: QueueMode)` | Set delivery mode: `OneAtATime` or `All` |

### Control

| Method | Description |
|--------|-------------|
| `abort()` | Cancel the current run via `CancellationToken`. During an LLM call (or a retry's backoff) the turn ends with `StopReason::Aborted`; while tools run or between turns, the run ends with a `[Agent stopped: cancelled]` user message |
| `async reset()` | Cancel any pending loop, recover tools, clear all state (messages, queues, streaming flag) |

## SubAgentTool

Delegates tasks to a child agent loop.

### Construction

```rust
// Provider auto-selected from the config's protocol; env key resolved automatically:
let sub = SubAgentTool::from_config("name", ModelConfig::anthropic("claude-sonnet-5", "Claude Sonnet 5"));

// Or resolve against a custom registry: SubAgentTool::from_config_with(&registry, "name", config) -> Result<_, AgentBuildError>

// Or pass an explicit provider Arc (custom providers, or a shared handle across sub-agents):
let sub = SubAgentTool::from_provider("name", Arc::new(provider), ModelConfig::anthropic("claude-sonnet-5", "Claude Sonnet 5"));
```

### Builder Methods

All return `Self` for chaining.

| Method | Description |
|--------|-------------|
| `with_description(desc) -> Self` | What the parent LLM sees (helps it decide when to delegate) |
| `with_system_prompt(prompt) -> Self` | The sub-agent's own instructions |
| `with_api_key(key) -> Self` | Override the env-resolved API key |
| `with_tools(tools: Vec<Arc<dyn AgentTool>>) -> Self` | Tools available to the sub-agent |
| `with_tool_source(source: impl ToolSource) -> Self` | Add a tool source, consulted once per delegation |
| `with_tool_middleware(m: impl ToolMiddleware) -> Self` | Gate the sub-agent's own tool calls |
| `with_turn_hook(hook: impl TurnHook) -> Self` | Per-request hook for the sub-agent's own LLM requests |
| `with_async_input_filter(f: impl AsyncInputFilter) -> Self` | Filter the task the parent hands over; a rejection fails the tool call |
| `with_skills(skills: SkillSet) -> Self` | Append the skills index to the sub-agent's system prompt |
| `with_shared_state(state: SharedState) -> Self` | Attach a shared key-value store (injects `shared_state` tool automatically) |
| `with_scoped_shared_state(state: SharedState, scope) -> Self` | Same, restricted to keys under `scope` |
| `with_context_config(config: ContextConfig) -> Self` | Give the child loop its own context management (truncation, compaction) |
| `with_max_turns(N) -> Self` | Turn limit (default: 10) |
| `with_thinking(level: ThinkingLevel) -> Self` | Enable extended thinking |
| `with_max_tokens(max: u32) -> Self` | Set max output tokens |
| `with_temperature(t: f32) -> Self` | Set the sampling temperature |
| `with_cache_config(config: CacheConfig) -> Self` | Prompt caching settings |
| `with_tool_execution(strategy: ToolExecutionStrategy) -> Self` | Tool execution strategy (`Parallel`, `Sequential`, `Batched`) |
| `with_retry_config(config: RetryConfig) -> Self` | Custom retry configuration |
| `with_turn_delay(delay: Duration) -> Self` | Inter-turn delay to throttle API calls (skips first turn) |
| `with_decision_model` / `with_decision_advisory` / `with_tool_gate` / `with_input_guard` | *(feature `decision`)* Mirror the `Agent` methods for the sub-agent's own turns |
| `reprice() -> Self` | Re-run the price lookup for the sub-agent's `ModelConfig` |

## SharedState

Pluggable key-value store for sub-agent communication. Backed by a `SharedStateBackend` trait.

### Construction

```rust
use yoagent::shared_state::{SharedState, FileBackend};

let state = SharedState::new();                              // MemoryBackend, 10MB cap
let state = SharedState::with_max_bytes(50 * 1024 * 1024);  // MemoryBackend, 50MB cap
let state = SharedState::with_backend(FileBackend::new("./state-dir")); // FileBackend (feature `native`)
```

### Methods

| Method | Description |
|--------|-------------|
| `async get(key) -> Option<String>` | Read a value by key |
| `async set(key, value) -> Result<(), SharedStateError>` | Store a value |
| `async remove(key) -> bool` | Delete a key, returns whether it existed |
| `async keys() -> Vec<String>` | List all keys |
| `async summary() -> String` | Human-readable summary of keys and sizes |
| `async prompt_summary() -> String` | Like `summary()`, minus truncation stashes (for system prompts) |
| `scoped(scope) -> SharedState` | A handle restricted to keys under `scope` |
| `scope() -> Option<&str>` | This handle's scope, if any |

### Built-in Backends

| Backend | Description |
|---------|-------------|
| `MemoryBackend` | In-memory `HashMap` with byte capacity limit (default) |
| `FileBackend` | One file per key, percent-encoded filenames, persistent *(feature `native`)* |

### Custom Backends

Implement the `SharedStateBackend` trait:

```rust
#[cfg_attr(target_arch = "wasm32", async_trait::async_trait(?Send))]
#[cfg_attr(not(target_arch = "wasm32"), async_trait::async_trait)]
pub trait SharedStateBackend: yoagent::rt::MaybeSend + yoagent::rt::MaybeSync {
    async fn get(&self, key: &str) -> Result<Option<String>, SharedStateError>;
    async fn set(&self, key: &str, value: String) -> Result<(), SharedStateError>;
    async fn remove(&self, key: &str) -> Result<bool, SharedStateError>;
    async fn keys(&self) -> Result<Vec<String>, SharedStateError>;
    async fn summary(&self) -> Result<String, SharedStateError>;
}
```

`MaybeSend + MaybeSync` is exactly `Send + Sync` on native targets, so a native-only implementation can keep plain `#[async_trait]`. See [WebAssembly & Cloudflare Workers](../guides/wasm-workers.md).

## Retry

`yoagent::retry` — see [Retry](../concepts/retry.md).

| Item | Description |
|------|-------------|
| `RetryConfig` | Retry count and backoff (`max_retries`, `initial_delay_ms`, `backoff_multiplier`, `max_delay_ms`); `RetryConfig::none()` disables retries |
| `retry_safe_events(rx) -> rx` | Wraps an event receiver so each provider attempt's events are held until it succeeds; a retried attempt leaves only its `ProviderRetry` |
| `RetrySafeEvents` | The same filter for your own event loop: `new()`, `push(event) -> Vec<AgentEvent>`, `finish() -> Vec<AgentEvent>` (also resets it) |

## Re-exports

The crate re-exports key types from `lib.rs`:

```rust
pub use agent::{Agent, AgentBuildError, StructuredPromptError};
pub use agent_loop::{agent_loop, agent_loop_continue};
pub use context::{CompactionStrategy, DefaultCompaction};
pub use llm_compaction::LlmCompaction;
pub use retry::RetryConfig;
pub use session::{Session, SessionEntry, SessionError};
pub use shared_state::SharedState;
pub use skills::SkillSet;
pub use sub_agent::SubAgentTool;
pub use tool_source::ToolSource;
pub use types::*;  // Message, Content, AgentMessage, AgentEvent, etc.
```

Runtime shims (`spawn`, `sleep`, `timeout`, `JoinHandle`, `Instant`, `MaybeSend`, `MaybeSync`) live in `yoagent::rt`: Tokio's own on native targets, the host executor on wasm32.
