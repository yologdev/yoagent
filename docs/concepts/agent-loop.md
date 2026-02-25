# The Agent Loop

The agent loop is the core of yoagent. It implements the fundamental cycle:

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
    pub cache_config: CacheConfig,
    pub tool_execution: ToolExecutionStrategy,
    pub retry_config: RetryConfig,
    pub before_turn: Option<BeforeTurnFn>,
    pub after_turn: Option<AfterTurnFn>,
    pub on_error: Option<OnErrorFn>,
    pub input_filters: Vec<Arc<dyn InputFilter>>,
    pub compaction_strategy: Option<Arc<dyn CompactionStrategy>>,
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
| `cache_config` | Prompt caching behavior (see [Prompt Caching](prompt-caching.md)) |
| `tool_execution` | Parallel, Sequential, or Batched (see [Tools](tools.md#execution-strategies)) |
| `retry_config` | Retry behavior for transient errors (see [Retry](retry.md)) |
| `before_turn` | Called before each LLM call; return `false` to abort (see [Callbacks](callbacks.md)) |
| `after_turn` | Called after each turn with messages and usage (see [Callbacks](callbacks.md)) |
| `on_error` | Called on `StopReason::Error` with the error string (see [Callbacks](callbacks.md)) |
| `input_filters` | Input filters applied to user messages before the LLM call (see [Tools](tools.md)) |
| `compaction_strategy` | Custom compaction strategy (see [Custom Compaction](#custom-compaction) below) |

## Steering & Follow-Ups

### Steering

**Steering messages** interrupt the agent between tool executions. When the agent is executing multiple tool calls from a single LLM response, steering is checked after each tool completes. If a steering message is found:

1. The current tool finishes normally
2. All remaining tool calls are **skipped** with `is_error: true` and "Skipped due to queued user message"
3. The steering message is injected into context
4. The loop continues with a new LLM call that sees the interruption

```rust
// While agent is running tools, redirect it:
agent.steer(AgentMessage::Llm(Message::user("Stop that. Instead, explain what you found.")));
```

### Follow-Ups

**Follow-up messages** are checked after the agent would normally stop (no more tool calls, no steering). If follow-ups exist, the loop continues with them as new input — the agent doesn't need to be re-prompted.

```rust
// Queue work for after the agent finishes its current task:
agent.follow_up(AgentMessage::Llm(Message::user("Now run the tests.")));
agent.follow_up(AgentMessage::Llm(Message::user("Then commit the changes.")));
```

### Queue Modes

Both queues support two delivery modes:

| Mode | Behavior |
|------|----------|
| `QueueMode::OneAtATime` | Delivers one message per turn (default) |
| `QueueMode::All` | Delivers all queued messages at once |

```rust
agent.set_steering_mode(QueueMode::All);
agent.set_follow_up_mode(QueueMode::OneAtATime);
```

### Queue Management

```rust
agent.clear_steering_queue();   // Drop all pending steers
agent.clear_follow_up_queue();  // Drop all pending follow-ups
agent.clear_all_queues();       // Drop everything
```

### Low-Level API

When using `agent_loop()` directly, steering and follow-ups are provided via callback functions:

```rust
let config = AgentLoopConfig {
    get_steering_messages: Some(Box::new(|| {
        // Return Vec<AgentMessage> — checked between tool calls
        vec![]
    })),
    get_follow_up_messages: Some(Box::new(|| {
        // Return Vec<AgentMessage> — checked when agent would stop
        vec![]
    })),
    // ...
};
```

## Custom Compaction

By default, when context exceeds the token budget in `ContextConfig`, yoagent runs a 3-level compaction strategy: truncate tool outputs → summarize old turns → drop middle messages. You can replace this with your own `CompactionStrategy`:

```rust
use yoagent::context::{CompactionStrategy, ContextConfig, compact_messages};
use yoagent::types::*;

struct MyCompaction;

impl CompactionStrategy for MyCompaction {
    fn compact(
        &self,
        messages: Vec<AgentMessage>,
        config: &ContextConfig,
    ) -> Vec<AgentMessage> {
        // Your logic here — then optionally delegate to the default:
        compact_messages(messages, config)
    }
}

let agent = Agent::new(provider)
    .with_compaction_strategy(MyCompaction);
```

The strategy is called once per turn, right before the LLM call, whenever `context_config` is `Some`. When `compaction_strategy` is `None`, `DefaultCompaction` (which wraps `compact_messages()`) is used automatically.

### Use Cases

**Memory-aware compaction** — Index messages into a vector store before they're dropped, so the agent can recall them later via a search tool:

```rust
struct MemoryAwareCompaction {
    memory: Arc<dyn MemoryStore>,
}

impl CompactionStrategy for MemoryAwareCompaction {
    fn compact(
        &self,
        messages: Vec<AgentMessage>,
        config: &ContextConfig,
    ) -> Vec<AgentMessage> {
        let compacted = compact_messages(messages.clone(), config);

        // Index what was dropped
        let dropped: Vec<_> = messages.iter()
            .filter(|m| !compacted.contains(m))
            .collect();
        if !dropped.is_empty() {
            self.memory.index(dropped);
        }

        compacted
    }
}
```

**Semantic pointer compaction** — Replace dropped messages with a marker so the agent knows context was lost:

```rust
struct SemanticPointerCompaction;

impl CompactionStrategy for SemanticPointerCompaction {
    fn compact(
        &self,
        messages: Vec<AgentMessage>,
        config: &ContextConfig,
    ) -> Vec<AgentMessage> {
        let compacted = compact_messages(messages.clone(), config);
        let dropped_count = messages.len() - compacted.len();

        if dropped_count == 0 {
            return compacted;
        }

        // Insert a marker after the first kept messages
        let mut result = compacted;
        let insert_at = config.keep_first.min(result.len());
        result.insert(insert_at, AgentMessage::Extension(
            ExtensionMessage::new("compaction_marker", serde_json::json!({
                "dropped": dropped_count,
                "note": format!("{} earlier messages were compacted", dropped_count),
            }))
        ));
        result
    }
}
```

**Priority-preserving compaction** — Never drop messages containing important keywords:

```rust
struct PriorityPreservingCompaction {
    preserve_keywords: Vec<String>,
}

impl CompactionStrategy for PriorityPreservingCompaction {
    fn compact(
        &self,
        messages: Vec<AgentMessage>,
        config: &ContextConfig,
    ) -> Vec<AgentMessage> {
        let (priority, normal): (Vec<_>, Vec<_>) = messages.into_iter()
            .partition(|m| self.is_priority(m));

        let mut compacted = compact_messages(normal, config);

        // Re-insert priority messages — they're never dropped
        for msg in priority {
            compacted.push(msg);
        }
        compacted
    }
}
```
