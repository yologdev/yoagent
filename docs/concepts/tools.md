# Tools

## The AgentTool Trait

Every tool implements `AgentTool`:

```rust
#[cfg_attr(target_arch = "wasm32", async_trait(?Send))]
#[cfg_attr(not(target_arch = "wasm32"), async_trait)]
pub trait AgentTool: rt::MaybeSend + rt::MaybeSync {
    fn name(&self) -> &str;
    fn label(&self) -> &str;
    fn description(&self) -> &str;
    fn parameters_schema(&self) -> serde_json::Value;
    async fn execute(
        &self,
        params: serde_json::Value,
        ctx: ToolContext,
    ) -> Result<ToolResult, ToolError>;
}
```

`rt::MaybeSend + rt::MaybeSync` is exactly `Send + Sync` on native targets, so
a plain `#[async_trait]` impl is unchanged there. On wasm32 the bound is empty;
see [WebAssembly & Cloudflare Workers](../guides/wasm-workers.md).

| Method | Purpose |
|--------|---------|
| `name()` | Unique ID sent to LLM (e.g., `"bash"`) |
| `label()` | Human-readable name for UI (e.g., `"Run Command"`) |
| `description()` | Tells the LLM what the tool does |
| `parameters_schema()` | JSON Schema for the tool's parameters |
| `execute()` | Runs the tool, returns `ToolResult` or `ToolError`. Receives a `ToolContext` with cancellation, update, and progress callbacks. |

## ToolContext

All execution context is bundled into a single struct, making the trait easier to extend in the future:

```rust
pub struct ToolContext {
    pub tool_call_id: String,
    pub tool_name: String,
    pub cancel: CancellationToken,
    pub on_update: Option<ToolUpdateFn>,
    pub on_progress: Option<ProgressFn>,
}
```

| Field | Purpose |
|-------|---------|
| `tool_call_id` | Unique ID for this tool call (for correlating events) |
| `tool_name` | Name of the tool being executed |
| `cancel` | Cancellation token — check `ctx.cancel.is_cancelled()` in long-running tools |
| `on_update` | Callback for streaming partial `ToolResult` updates to the UI (emits `ToolExecutionUpdate`) |
| `on_progress` | Callback for emitting user-facing progress messages (emits `ProgressMessage`) |

`ToolContext` implements `Clone` and `Debug`. It is `#[non_exhaustive]`: outside
the loop (tests, or driving a tool directly) build one with
`ToolContext::new(id, name)` and its `with_*` builders. Delegation tools report
a child run's stats with `ctx.report_delegated_run(..)`.

## ToolResult

```rust
pub struct ToolResult {
    pub content: Vec<Content>,
    pub details: serde_json::Value,
}
```

The `content` is sent back to the LLM. The `details` field holds metadata (not sent to the LLM) for UI/logging.

## ToolError

```rust
pub enum ToolError {
    Failed(String),
    NotFound(String),
    InvalidArgs(String),
    Cancelled,
}
```

Errors are converted to `ToolResult` with `is_error: true` and sent back to the LLM so it can recover.

## Implementing a Custom Tool

```rust
use yoagent::types::*;
use async_trait::async_trait;

pub struct WeatherTool;

#[async_trait]
impl AgentTool for WeatherTool {
    fn name(&self) -> &str { "get_weather" }
    fn label(&self) -> &str { "Weather" }
    fn description(&self) -> &str {
        "Get current weather for a city."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "city": {
                    "type": "string",
                    "description": "City name"
                }
            },
            "required": ["city"]
        })
    }

    async fn execute(
        &self,
        params: serde_json::Value,
        _ctx: ToolContext,
    ) -> Result<ToolResult, ToolError> {
        let city = params["city"].as_str()
            .ok_or(ToolError::InvalidArgs("missing city".into()))?;

        // Call weather API...
        Ok(ToolResult {
            content: vec![Content::Text {
                text: format!("Weather in {}: 72°F, sunny", city),
            }],
            details: serde_json::Value::Null,
        })
    }
}
```

> The built-in tools (`BashTool`, `ReadFileTool`, `WriteFileTool`,
> `EditFileTool`, `ListFilesTool`, `SearchTool`) and `default_tools()` need the
> default `native` feature. They do not exist with `default-features = false`
> or on wasm32.

Register custom tools alongside defaults:

```rust
use yoagent::tools::default_tools;

let mut tools = default_tools();
tools.push(Box::new(WeatherTool));
let agent = Agent::from_config(ModelConfig::anthropic("claude-sonnet-5", "Claude Sonnet 5")).with_tools(tools);
```

## Sandboxing the built-in file tools

`ReadFileTool`, `WriteFileTool`, `EditFileTool`, `ListFilesTool` and `SearchTool` accept an allowlist of directory roots. Empty (the default) means unrestricted:

```rust
let roots = vec!["/srv/workspace".to_string()];

let tools: Vec<Box<dyn AgentTool>> = vec![
    Box::new(ReadFileTool::new().with_allowed_paths(roots.clone())),
    Box::new(WriteFileTool::new().with_allowed_paths(roots.clone())),
    Box::new(EditFileTool::new().with_allowed_paths(roots)),
];
```

Enforcement is against the **resolved** path, not the string, so neither `..` nor a symlink pointing outside can escape. That includes a dangling symlink, which a write would follow to create its target; a symlink loop is refused. A file that does not exist yet still resolves through its real parent, so writes are checked too, and the tool then does its I/O on the path it checked. Before 0.24.2, a `..` after a not-yet-existing directory, or a dangling symlink, could get past the check. Something changing the tree between the check and the I/O, such as a parallel `bash` call, is not covered. The rejection message deliberately does not echo the allowed roots, since tool results reach the model's transcript.

## `BashTool` is not a sandbox

`BashTool` runs whatever the model asks through `bash -c`, with the agent process's own environment and filesystem access.

`deny_patterns` is a substring check that catches typos and obvious mistakes — `rm  -rf /` with two spaces, a base64-decoded pipe, or an equivalent `find -delete` all sail past it. Treat it as a guardrail, never as a security boundary.

Real isolation belongs outside the tool: run the agent in a container or VM, or gate calls through [`ToolMiddleware`](#permissions-tool-middleware), which sees the arguments before execution and can deny them.

For credentials specifically, commands inherit every environment variable the agent process holds — including any `*_API_KEY`. When the model composes the command, restrict what it can read:

```rust
let bash = BashTool::default()
    .with_env_allowlist(vec!["RUST_LOG".to_string()]);  // plus PATH, HOME, PWD
```

## Error Handling

**Return `Err(ToolError)` on failure, not `Ok` with error text.** When a tool returns `Err`, the agent loop converts it to a `Message::ToolResult` with `is_error: true` and sends it to the LLM. The LLM sees the error and can self-correct — retry with different arguments, try a different approach, or explain the failure to the user.

```rust
async fn execute(&self, params: serde_json::Value, _ctx: ToolContext) -> Result<ToolResult, ToolError> {
    let path = params["path"].as_str()
        .ok_or(ToolError::InvalidArgs("missing 'path'".into()))?;

    let content = std::fs::read_to_string(path)
        .map_err(|e| ToolError::Failed(format!("Cannot read {}: {}", path, e)))?;

    Ok(ToolResult {
        content: vec![Content::Text { text: content }],
        details: serde_json::Value::Null,
    })
}
```

**Exception: BashTool.** The built-in `BashTool` returns `Ok` even on non-zero exit codes, with both stdout and stderr in the result. This is intentional — the LLM needs to see the actual error output (compilation errors, test failures, etc.) to diagnose and fix issues. Only failures outside the command return `Err`: a matched deny pattern, a refused confirmation, a timeout (its message carries the output printed so far), cancellation, or `bash` failing to start. A command that does not exist is an ordinary non-zero exit (127). A timeout or cancel kills the `bash` process; what it started (pipeline stages, commands in a `&&` list, background jobs) can keep running.

**A panicking tool** is contained by the loop, the same as a panicking middleware. The call ends as an error result (`tool '<name>' panicked: …`) that the model sees, and the run continues.

## Tool Execution Flow

1. LLM returns `Content::ToolCall` blocks in its response
2. Agent loop emits `ToolExecutionStart` for each
3. Tool's `execute()` is called with parsed arguments
4. Result (or error) is wrapped in `Message::ToolResult`
5. `ToolExecutionEnd` is emitted
6. All tool results are added to context
7. Loop continues with another LLM call

## Streaming Tool Output

Long-running tools can stream progress updates to the UI via the `on_update` callback. Each call emits a `ToolExecutionUpdate` event. Partial results are **for UI/logging only** — they are not sent to the LLM. Only the final `ToolResult` returned from `execute()` becomes part of the conversation.

### The `ToolUpdateFn` type

```rust
pub type ToolUpdateFn = Arc<dyn Fn(ToolResult) + Send + Sync>;
```

### Basic usage

Call `on_update` whenever you have progress to report:

```rust
use yoagent::types::*;

struct DataProcessorTool;

#[async_trait]
impl AgentTool for DataProcessorTool {
    // ... name, label, description, parameters_schema ...

    async fn execute(
        &self,
        params: serde_json::Value,
        ctx: ToolContext,
    ) -> Result<ToolResult, ToolError> {
        let rows = fetch_rows(&params)?;
        let total = rows.len();

        for (i, row) in rows.iter().enumerate() {
            // Check for cancellation
            if ctx.cancel.is_cancelled() {
                return Err(ToolError::Cancelled);
            }

            process_row(row);

            // Stream progress every 100 rows
            if i % 100 == 0 {
                if let Some(ref cb) = &ctx.on_update {
                    cb(ToolResult {
                        content: vec![Content::Text {
                            text: format!("Processed {}/{} rows", i, total),
                        }],
                        details: serde_json::json!({"progress": i as f64 / total as f64}),
                    });
                }
            }
        }

        Ok(ToolResult {
            content: vec![Content::Text {
                text: format!("Processed all {} rows", total),
            }],
            details: serde_json::Value::Null,
        })
    }
}
```

### Consuming updates in your UI

Updates arrive as `AgentEvent::ToolExecutionUpdate` events on the same event stream as all other agent events:

```rust
while let Some(event) = rx.recv().await {
    match event {
        AgentEvent::ToolExecutionStart { tool_name, .. } => {
            println!("⏳ {} started", tool_name);
        }
        AgentEvent::ToolExecutionUpdate { tool_name, partial_result, .. } => {
            // Show progress in your UI
            if let Some(Content::Text { text }) = partial_result.content.first() {
                println!("  📊 {}: {}", tool_name, text);
            }
        }
        AgentEvent::ToolExecutionEnd { tool_name, is_error, .. } => {
            println!("{} {}", if is_error { "❌" } else { "✅" }, tool_name);
        }
        AgentEvent::ProgressMessage { tool_name, text, .. } => {
            println!("  💬 {}: {}", tool_name, text);
        }
        _ => {}
    }
}
```

### Progress Messages

In addition to `on_update` (which streams partial `ToolResult` values), tools can emit lightweight text-only progress messages via `ctx.on_progress`. These appear as `AgentEvent::ProgressMessage` events:

```rust
async fn execute(&self, params: serde_json::Value, ctx: ToolContext) -> Result<ToolResult, ToolError> {
    if let Some(ref progress) = &ctx.on_progress {
        progress("Starting analysis...".into());
    }

    // ... do work ...

    if let Some(ref progress) = &ctx.on_progress {
        progress("Almost done...".into());
    }

    Ok(ToolResult { /* ... */ })
}
```

Use `on_progress` for simple status text. Use `on_update` when you need structured data (progress percentages, partial results).

### Guidelines

- **Call `on_update` as often as useful** — there's no rate limit. The callback is synchronous and cheap.
- **Always check `ctx.on_update.is_some()`** before building the `ToolResult`. If `None`, the loop isn't interested in updates (e.g., testing).
- **Use `details` for structured data** — `content` is for human-readable text, `details` can carry progress percentages, byte counts, etc.
- **Don't rely on updates reaching the LLM** — they won't. Only the final return value is added to context.
- **Simple tools don't need it** — if your tool completes in <1 second, just ignore `ctx` (prefix with `_ctx` to suppress the warning).

### End-to-end example

Here's a complete example: a CLI agent with a deploy tool that streams progress. The human sees real-time output while the LLM only gets the final result.

```rust
use yoagent::agent::Agent;
use yoagent::provider::ModelConfig;
use yoagent::types::*;

/// A tool that deploys an app and streams each step.
struct DeployTool;

#[async_trait]
impl AgentTool for DeployTool {
    fn name(&self) -> &str { "deploy" }
    fn label(&self) -> &str { "Deploy App" }
    fn description(&self) -> &str { "Deploy the application to production." }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "env": { "type": "string", "description": "Target environment" }
            },
            "required": ["env"]
        })
    }

    async fn execute(
        &self,
        params: serde_json::Value,
        ctx: ToolContext,
    ) -> Result<ToolResult, ToolError> {
        let env = params["env"].as_str().unwrap_or("staging");

        let steps = ["Building image", "Running tests", "Pushing to registry", "Rolling out"];
        for (i, step) in steps.iter().enumerate() {
            if ctx.cancel.is_cancelled() {
                return Err(ToolError::Cancelled);
            }

            // Stream each step to the UI
            if let Some(ref cb) = &ctx.on_update {
                cb(ToolResult {
                    content: vec![Content::Text {
                        text: format!("[{}/{}] {}...", i + 1, steps.len(), step),
                    }],
                    details: serde_json::json!({
                        "step": i + 1,
                        "total": steps.len(),
                        "phase": step,
                    }),
                });
            }

            // Simulate work (`yoagent::rt::sleep` works on native and wasm32)
            yoagent::rt::sleep(std::time::Duration::from_secs(2)).await;
        }

        // Only this final result is sent to the LLM
        Ok(ToolResult {
            content: vec![Content::Text {
                text: format!("Successfully deployed to {}", env),
            }],
            details: serde_json::json!({"env": env, "status": "success"}),
        })
    }
}

#[tokio::main]
async fn main() {
    let mut agent = Agent::from_config(ModelConfig::anthropic("claude-sonnet-5", "Claude Sonnet 5"))
        .with_system_prompt("You are a deployment assistant.")
        .with_tools(vec![Box::new(DeployTool)]);

    let mut rx = agent.prompt("Deploy to production").await;

    while let Some(event) = rx.recv().await {
        match event {
            // LLM text streaming
            AgentEvent::MessageUpdate {
                delta: StreamDelta::Text { delta }, ..
            } => print!("{}", delta),

            // Tool progress streaming
            AgentEvent::ToolExecutionStart { tool_name, .. } => {
                println!("\n🚀 Starting {}...", tool_name);
            }
            AgentEvent::ToolExecutionUpdate { partial_result, .. } => {
                if let Some(Content::Text { text }) = partial_result.content.first() {
                    println!("  {}", text);
                }
            }
            AgentEvent::ToolExecutionEnd { tool_name, is_error, .. } => {
                if is_error {
                    println!("  ❌ {} failed", tool_name);
                } else {
                    println!("  ✅ {} complete", tool_name);
                }
            }
            AgentEvent::ProgressMessage { text, .. } => {
                println!("  💬 {}", text);
            }

            AgentEvent::AgentEnd { .. } => break,
            _ => {}
        }
    }
}
```

Running this produces:

```
🚀 Starting deploy...
  [1/4] Building image...
  [2/4] Running tests...
  [3/4] Pushing to registry...
  [4/4] Rolling out...
  ✅ deploy complete
Successfully deployed to production. The deployment completed all 4 stages.
```

The human sees each step as it happens. The LLM only sees "Successfully deployed to production" and can continue the conversation from there.

### How agents benefit

When an AI agent (like a coding assistant) uses yoagent, streaming tool output helps in two ways:

1. **Human oversight** — The human watching the agent work sees real-time progress instead of waiting for a tool to finish. A bash command running `cargo build` can stream compiler output as it happens, so the human can interrupt early if something is wrong.

2. **Agent UIs** — Tools like web dashboards, IDE extensions, or chat interfaces can render live progress bars, log tails, or status indicators. The `details` field in `ToolResult` carries structured data (progress percentage, byte counts, etc.) that UIs can render however they want.

The LLM itself doesn't see updates — it works with final results only. This is intentional: partial output would waste context tokens and confuse the model. The streaming is purely a **human-facing** feature.

## Tools That Change at Runtime: `ToolSource`

`with_tools` fixes an agent's tool list until you replace it. When the set of
tools changes while the agent lives — plugins loaded and unloaded, an MCP
server that reconnects with a different list, a feature flag — add a
`ToolSource` instead:

```rust
use std::sync::{Arc, Mutex};
use yoagent::{Agent, AgentTool, ToolSource};

struct Swappable(Mutex<Vec<Arc<dyn AgentTool>>>);

#[async_trait::async_trait]
impl ToolSource for Swappable {
    async fn tools(&self) -> Vec<Arc<dyn AgentTool>> {
        self.0.lock().unwrap().clone()
    }
}

let agent = Agent::from_provider(provider, config)
    .with_tools(my_static_tools)
    .with_tool_source(Arc::new(Swappable(Mutex::new(vec![]))));
```

The contract:

- **Consulted once per run.** Every `prompt*` and `continue_loop*` call asks
  each source for its tools before the first request; the list is then fixed
  for that run. A tool withdrawn mid-run stays offered until the run ends (make
  it fail cleanly if its backend is gone); a tool added mid-run is offered from
  the next run. Changing the list turn to turn could strand a call the model
  already made.
- **Prompt caching.** Tool definitions open the provider's cached prefix, so
  *any* change to the offered set rewrites the whole cache. Per-run
  consultation bounds that cost to run boundaries — it does not prevent it: a
  run whose set differs from the previous run's pays a full prefix rewrite.
  Order alone never costs anything: sourced tools are sorted by name after
  collisions are resolved (the agent's own tools keep their order).
- **No timeout.** The run waits for every source. On an `Agent`, dropping the
  prompt future while it waits is the escape (sources are consulted before
  any agent state is touched); a `SubAgentTool` stops waiting when the parent
  run is cancelled.
- **For that run only.** Sourced tools are appended after the agent's own
  tools and dropped again when the run ends; they never join `with_tools`'
  list. A model that calls a tool that is no longer offered gets the usual
  `Tool … not found` error result.
- **Name collisions.** Providers reject two tools with one name, so exactly
  one survives: the agent's own tools (including the injected `shared_state`
  tool) win, then earlier sources in installation order, then the earlier
  tool within one source. Each dropped duplicate is logged with
  `tracing::warn!`; the run is never refused.
- **Failures.** A source that panics — while building its future or while it
  runs — is contained, logged with the panic message, and contributes no
  tools to that run.

Several sources may be added. `SubAgentTool::with_tool_source` mirrors it —
consulted once per delegation. The [rutis bridge](https://github.com/yologdev/yoagent/tree/main/integrations/yoagent-rutis)
(`yoagent-rutis`) is built on it: plugins contribute tools, and an agent sees
exactly the tools of the currently loaded plugins at each run start.

## Execution Strategies

When the LLM returns multiple tool calls in a single response (e.g., "read file A, read file B, run bash C"), `ToolExecutionStrategy` controls how they run:

| Strategy | Behavior |
|----------|----------|
| `Sequential` | One at a time. Steering checked between each tool. Use for debugging or tools with shared mutable state. |
| **`Parallel`** (default) | All tool calls run concurrently via `futures::join_all`. Steering checked after all complete. Best latency for independent tools. |
| `Batched { size }` | Run in groups of N. Steering checked between batches. Balances speed with human-in-the-loop control. |

### Configuration

```rust
use yoagent::agent::Agent;
use yoagent::types::ToolExecutionStrategy;

// Default — parallel (fastest)
let agent = Agent::from_config(ModelConfig::anthropic("claude-sonnet-5", "Claude Sonnet 5"));

// Sequential (debug / shared state)
let agent = Agent::from_config(ModelConfig::anthropic("claude-sonnet-5", "Claude Sonnet 5"))
    .with_tool_execution(ToolExecutionStrategy::Sequential);

// Batched — 3 at a time
let agent = Agent::from_config(ModelConfig::anthropic("claude-sonnet-5", "Claude Sonnet 5"))
    .with_tool_execution(ToolExecutionStrategy::Batched { size: 3 });
```

### When to use each

- **Parallel** (default): Most tool calls are independent — file reads, searches, API calls. Running them concurrently can cut latency dramatically (3 tools × 50ms = ~50ms instead of ~150ms).
- **Sequential**: When tools have side effects that depend on order, or when you need fine-grained steering control between each tool.
- **Batched**: When you want parallelism but also want steering checkpoints. For example, `Batched { size: 3 }` runs 3 tools concurrently, checks for user interrupts, then runs the next 3.

Steering messages are always checked between execution units (between each tool in Sequential, after all tools in Parallel, between batches in Batched). If a user interrupts, remaining tools are skipped.

## Permissions: Tool Middleware

Every tool call can be gated by an async **middleware chain** — the mechanism
behind permission prompts, policy engines, and argument rewriting. yoagent
ships the hook, not a policy: with no middleware installed, every call runs.

```rust
use yoagent::{ToolCallRequest, ToolDecision, ToolMiddleware};

struct ReadOnlyPolicy;

#[async_trait::async_trait]
impl ToolMiddleware for ReadOnlyPolicy {
    async fn before_tool(&self, call: &ToolCallRequest<'_>) -> ToolDecision {
        match call.tool_name {
            "write_file" | "edit_file" | "bash" => {
                ToolDecision::Deny("read-only session".into())
            }
            _ => ToolDecision::Allow,
        }
    }
}

let agent = Agent::from_config(ModelConfig::anthropic("claude-sonnet-5", "Sonnet 5"))
    .with_tools(default_tools())
    .with_tool_middleware(ReadOnlyPolicy);
```

Semantics:

- **`Allow`** — the call proceeds (with the current arguments).
- **`Modify(args)`** — the call proceeds with replacement arguments (e.g.
  rewrite a path into a sandbox). Later middleware in the chain see the
  rewritten arguments; the `ToolExecutionStart` event carries what actually
  runs.
- **`Deny(reason)`** — the call never executes. The reason is returned to the
  LLM as an **error tool result** (`"Tool call denied: ..."`), so the model can
  adapt — pick another tool, ask the user — and the loop continues. A denial
  never aborts the run.

A middleware that panics is contained: the call is denied (reason `"tool
middleware panicked"`) and the loop continues — a buggy policy can't kill the
run.

The hook is `async`, so an interactive app can prompt a human before deciding.
Note that under the default `Parallel` execution strategy, middleware for
parallel tool calls run concurrently — if you need one-at-a-time approval UX,
serialize inside your middleware (e.g. a `tokio::sync::Mutex`) or switch to
`ToolExecutionStrategy::Sequential`.

Sub-agents gate their own tool calls the same way via
`SubAgentTool::with_tool_middleware`.

Middleware can also see the conversation: `call.messages` is the loop's
history (before `transform_context`, including the assistant message carrying
the call), and `call.run_prompts` holds the user messages this run was given,
which compaction cannot remove. For "what did the user ask?", use
`call.user_request()`: it skips loop-injected messages
(`yoagent::is_loop_injected`), never looks back past a compaction boundary,
falls back to the run's prompts, and returns `None` when nothing says what the
user wants — enough for policies such as "was this destructive call actually
requested?". The [decision-model tool gate](decision-models.md#blocking-with_tool_gate)
is one. `call.user_request_parts()` gives the same selection as structured
`UserRequestParts` (`latest`, `reply`, `source`, `run_prompts`); the prose of
`user_request()` is not a stable format.

To unit-test a middleware without running an agent, build the request
yourself:

```rust
let args = serde_json::json!({"path": "/tmp/x"});
let prompts = [Message::user("delete /tmp/x")];
let call = ToolCallRequest::new("call-1", "rm", &args)
    .with_run_prompts(&prompts); // or .with_messages(&history)
assert!(matches!(MyPolicy.before_tool(&call).await, ToolDecision::Allow));
```
