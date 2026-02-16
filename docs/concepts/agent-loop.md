# The Agent Loop

The agent loop is the core of yo-agent. It implements the fundamental cycle:

```
User prompt → LLM call → Tool execution → LLM call → ... → Final response
```

## How It Works

```
┌──────────────────────────────────────────────┐
│                  agent_loop()                │
│                                              │
│  1. Add prompts to context                   │
│  2. Emit AgentStart + TurnStart              │
│                                              │
│  ┌─────────── Inner Loop ──────────────┐     │
│  │  • Check steering messages          │     │
│  │  • Check execution limits           │     │
│  │  • Compact context (if configured)  │     │
│  │  • Stream LLM response              │     │
│  │  • Extract tool calls               │     │
│  │  • Execute tools (with steering)    │     │
│  │  • Emit TurnEnd                     │     │
│  │  • Continue if tool_calls or steer  │     │
│  └─────────────────────────────────────┘     │
│                                              │
│  3. Check follow-up messages                 │
│  4. If follow-ups exist, loop again          │
│  5. Emit AgentEnd                            │
└──────────────────────────────────────────────┘
```

## Entry Points

### `agent_loop()`

Starts a new agent run with prompt messages:

```rust
pub async fn agent_loop(
    prompts: Vec<AgentMessage>,
    context: &mut AgentContext,
    config: &AgentLoopConfig<'_>,
    tx: mpsc::UnboundedSender<AgentEvent>,
    cancel: CancellationToken,
) -> Vec<AgentMessage>
```

The prompts are added to context, then the loop runs. Returns all new messages generated during the run.

### `agent_loop_continue()`

Resumes from existing context (e.g., after an error or retry):

```rust
pub async fn agent_loop_continue(
    context: &mut AgentContext,
    config: &AgentLoopConfig<'_>,
    tx: mpsc::UnboundedSender<AgentEvent>,
    cancel: CancellationToken,
) -> Vec<AgentMessage>
```

Requires that the last message in context is **not** an assistant message.

## AgentLoopConfig

```rust
pub struct AgentLoopConfig<'a> {
    pub provider: &'a dyn StreamProvider,
    pub model: String,
    pub api_key: String,
    pub thinking_level: ThinkingLevel,
    pub max_tokens: Option<u32>,
    pub temperature: Option<f32>,
    pub convert_to_llm: Option<ConvertToLlmFn>,
    pub transform_context: Option<TransformContextFn>,
    pub get_steering_messages: Option<GetMessagesFn>,
    pub get_follow_up_messages: Option<GetMessagesFn>,
    pub context_config: Option<ContextConfig>,
    pub execution_limits: Option<ExecutionLimits>,
}
```

| Field | Purpose |
|-------|---------|
| `provider` | The `StreamProvider` implementation to use |
| `model` | Model identifier (e.g., `"claude-sonnet-4-20250514"`) |
| `api_key` | API key for the provider |
| `thinking_level` | `Off`, `Minimal`, `Low`, `Medium`, `High` |
| `convert_to_llm` | Custom `AgentMessage[] → Message[]` conversion |
| `transform_context` | Pre-processing hook for context pruning |
| `get_steering_messages` | Returns user interruptions during tool execution |
| `get_follow_up_messages` | Returns queued work after agent would stop |
| `context_config` | Token budget and compaction settings |
| `execution_limits` | Max turns, tokens, duration |

## Steering & Follow-Ups

**Steering messages** interrupt the agent between tool executions. If a steering message arrives while tools are running, remaining tool calls are skipped with "Skipped due to queued user message."

**Follow-up messages** are checked after the agent would normally stop (no more tool calls). If follow-ups exist, the loop continues with them as new input.

Both are provided via callback functions that return `Vec<AgentMessage>`.
