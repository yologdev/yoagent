---
name: yoagent
description: Use when building, extending or upgrading an agent on the yoagent Rust crate. Maps each job (tools, policies, verifiers, budgets, sub-agents, plugins from other ecosystems, retries, structured output, tests) to the yoagent feature for it, with tips, pitfalls and what changed per version.
---

# yoagent: which feature for which job

Written for **yoagent 0.25.2** (and yoagent-rutis 0.3.0); the recipes
below are compiled by `tests/skill_test.rs`, line for line. Check what you
build against: `cargo tree -p yoagent` or `Cargo.lock`. If you are on an
older version, read [What changed](#what-changed) first.

yoagent is a loop library. It runs the loop (stream a reply, run the tools
it asks for, feed results back, repeat) and its guarantees: retries,
cancellation, limits, compaction, cost accounting. **Everything that
changes what the loop does goes through one contract, `Extension`.**
Before writing loop code of your own, find the hook below.

## Pick the feature

| You want to… | Use | Read |
|---|---|---|
| build an agent | `Agent::from_config(ModelConfig::…)` (provider and API key follow the config) | `docs/getting-started/quick-start.md` |
| give it tools | `impl AgentTool`; `.with_tools(…)` (built-ins: `tools::default_tools()`) | `docs/concepts/tools.md` |
| allow / rewrite / deny tool calls | `RunHooks::before_tool` in an `Extension` | `docs/concepts/extensions.md` |
| redact or edit a tool's output | `RunHooks::after_tool` (+ `.filters_tool_output()`) | same |
| refuse an input before the run | `RunHooks::on_input` | same |
| add a note to every model request | `RunHooks::before_model` → `TurnDecision::Note` | same |
| check the final answer, send the agent back | `RunHooks::on_stop` → `StopDecision::Continue(why)` | same |
| count, log or record what happened | `RunHooks::on_event` (sync, cheap), `finish` | same |
| cap spend in dollars | `extension::Budget` | `docs/concepts/extensions.md#budget` |
| cap turns / tokens / time | `ExecutionLimits` | `docs/concepts/context-management.md` |
| delegate to a child agent | `SubAgentTool` (own model, tools, limits) | `docs/concepts/sub-agents.md` |
| apply one policy to every sub-agent | `.with_tree_extension(…)` | `docs/concepts/extensions.md` |
| offer tools that change between runs | `ToolSource` (consulted once per run) | `docs/concepts/tools.md` |
| load plugins at runtime (Rust, TS on Node/Bun, Python, Go), or pi / DeepSeek Harness plugins unchanged | the **yoagent-rutis** crate (`RutisBridge`, one `Extension`) | [yoagent-rutis README](https://github.com/yologdev/yoagent/tree/main/integrations/yoagent-rutis) |
| use MCP servers | `.with_mcp_server_stdio(…)` / `_http(…)` / `_http_transport(…)` | `docs/guides/mcp.md` |
| get typed JSON back | `agent.prompt_structured::<T>(text, schema)` | `docs/concepts/structured-outputs.md` |
| steer a running agent | `agent.steer(msg)` (between tool batches), `follow_up(msg)` | `docs/concepts/agent-loop.md` |
| show streamed text without retry duplicates | `retry::retry_safe_events(rx)` | `docs/concepts/retry.md` |
| branch / restore history | `session::Session` (a tree, JSONL) | `docs/concepts/session-trees.md` |
| know what it cost | `agent.total_cost_usd()`, `SessionStats` buckets | `docs/concepts/pricing.md` |
| test without a network | `MockProvider` + `Agent::from_provider` | `docs/guides/testing.md` |
| run on Cloudflare Workers | `default-features = false` (wasm32) | `docs/guides/wasm-workers.md` |

## Recipes

### A policy (deny, rewrite)

```rust
use yoagent::extension::{ClonedHooks, RunHooks};
use yoagent::{ToolCallRequest, ToolDecision};

#[derive(Clone)]
struct ProtectGuards;

#[async_trait::async_trait]
impl RunHooks for ProtectGuards {
    async fn before_tool(&self, call: &ToolCallRequest<'_>) -> ToolDecision {
        let path = call.args["path"].as_str().unwrap_or_default();
        if path.starts_with(".github/") {
            ToolDecision::Deny("CI files are off limits".into())
        } else {
            ToolDecision::Allow
        }
    }
}

let agent = agent.with_extension(ClonedHooks::new("protect-guards", ProtectGuards).required());
```

`ClonedHooks` clones the hooks per run, so state starts fresh every run.
`.required()` makes a failing hook fail the run (default: advisory, the
failure is logged and that hook skipped).

### A verifier (don't accept "done" without evidence)

```rust
use yoagent::extension::{RunHooks, StopContext, StopDecision};
use yoagent::{AgentMessage, Content, Message};

#[derive(Clone)]
struct RequireTests;

#[async_trait::async_trait]
impl RunHooks for RequireTests {
    async fn on_stop(&mut self, stop: &StopContext<'_>) -> StopDecision {
        let ran_tests = stop.messages.iter().any(|m| match m {
            AgentMessage::Llm(Message::Assistant { content, .. }) => content.iter().any(|c| {
                matches!(c, Content::ToolCall { name, arguments, .. }
                    if name == "bash" && arguments["command"].as_str().is_some_and(|cmd| cmd.contains("cargo test")))
            }),
            _ => false,
        });
        if ran_tests {
            StopDecision::Accept
        } else {
            StopDecision::Continue("Run `cargo test` before saying you are done.".into())
        }
    }
}
```

Read the evidence from `stop.messages` (the conversation so far). Don't
try to set a flag from `on_event`: it takes `&self`, and a shared
`Arc<AtomicBool>` would carry over between runs, since `ClonedHooks`
clones the `Arc`. Continues are capped per run (`max_stop_continues`,
default 3); a required extension that still continues at the cap fails the
run.

### A dollar budget

```rust
use yoagent::extension::Budget;

yoagent::provider::prices::enable_bundled(); // pricing is opt-in since 0.25
let config = ModelConfig::claude_sonnet_5();
if let Some(budget) = Budget::for_model(2.00, &config) { // None when unpriced
    agent = agent.with_extension(budget.across_runs()); // one total for the session
}
```

It is checked before every model call: once the spend has reached the
limit, the run stops there (one call may cross it). It counts sub-agents,
decision models and compaction too.

### A sub-agent with its own limits, under the parent's policy

```rust
use yoagent::context::ExecutionLimits;

let child = SubAgentTool::from_config("researcher", ModelConfig::claude_sonnet_5())
    .with_system_prompt("Research, then report.")
    .with_execution_limits(ExecutionLimits::default().with_max_turns(20));
let agent = agent
    .with_tools(vec![Box::new(child)])
    .with_tree_extension(ClonedHooks::new("protect-guards", ProtectGuards).required());
```

### Plugins from other ecosystems (yoagent-rutis)

Plugins register a handler with the bridge; the bridge is one `Extension`.
pi extensions and DeepSeek Harness (DSH) plugins load unchanged through
adapters, as far as yoagent can honour them; what it cannot (UI, commands)
is ignored, and what would decide things yoagent cannot enforce is
**refused**. Measured: of the 20 most-downloaded packages, pi 12 and DSH 4
are usable (`docs/evals/plugin-compatibility.md`). Start from the
[yoagent-rutis README](https://github.com/yologdev/yoagent/tree/main/integrations/yoagent-rutis)
and its examples.

## Tips and pitfalls

- **Pricing is opt-in (0.25).** Call `prices::enable_bundled()` once,
  *before* building configs, on a path tests reach too (tests never run
  `main`). Without it every cost is `None`, `Budget::for_model` is `None`.
- **`with_tools` replaces the list.** An agent starts with no tools; the
  built-ins come from `tools::default_tools()`. `with_mcp_server_*` adds to
  that list, so call `with_tools` first — a later `with_tools` drops the
  MCP tools.
- **Policies fail closed.** A `before_tool` that panics, or is still
  running when the run is cancelled, denies the call (a plugin policy
  through yoagent-rutis also denies on an error or its timeout); an
  `after_tool` that fails withholds the output. A deny is a normal tool
  result the model reads and adapts to.
- **`before_tool`, `after_tool`, `on_event` take `&self`** and run
  concurrently for parallel calls; keep shared state in a `Mutex`/atomic.
- **Never rewrite the system prompt per turn.** It breaks the prompt cache.
  Per-turn context goes in `before_model` notes (added to the latest user
  turn, never stored).
- **Cancelling**: a cancel during a model call ends that message as
  `StopReason::Aborted`; a cancel while tools run or between turns appends
  `[Agent stopped: cancelled]`. Either way `on_error` is not called. A
  cancelled sub-agent arrives as `ToolError::Cancelled` (cancelled before
  its first answer), `Sub-agent '<name>' failed: [Agent stopped: cancelled]`,
  or `Sub-agent '<name>' failed: Cancelled` (mid-stream) — key on the cancel
  token, not the text.
- **`bash` runs in its own process group (0.25.2)**: the terminal's Ctrl+C
  does not reach a running command any more. A terminal host must catch
  Ctrl+C and call `agent.abort()`; the cancel then kills the whole group.
- **Retries emit `AgentEvent::ProviderRetry`.** A streamed attempt that
  failed was already shown; look at the next event before treating an error
  `MessageEnd` as final, or use `retry_safe_events`.
- **Limits are per run** and `max_total_tokens` counts the whole prompt
  every turn, so it fills faster than the visible conversation.
- **Spend is bucketed**: the agent's own, `sub_agents`, `decision`,
  `compaction`; `total_cost_usd()` adds them. Unknown cost is `None`,
  never zero.
- **Plugin messages are capped at 16 MiB** (rutis 0.9 closes the channel
  above that). Large images in a conversation reach plugins as text.
- **Tests**: script whole multi-turn tool conversations with `MockProvider`
  (`Agent::from_provider(MockProvider::new(…), ModelConfig::mock())`); it
  honours cancellation, so abort and steering paths are testable.
- **The older hooks still work** (`ToolMiddleware`, input filters,
  `TurnHook`, callbacks), but new work should use `Extension`: one place,
  per-run state, sub-agent coverage.

## What changed

Newest first; the full list is `CHANGELOG.md`.

- **0.25.2** — MCP `HttpTransport::with_header`; `bash` kills its process
  group (Ctrl+C: see above).
- **0.25.1** — GASP records plugin log lines on their run
  (`GaspRecorder::extension()` + `plugin_log_layer()`).
- **0.25.0** — `Extension` / `RunHooks` (the contract above), `Budget`;
  pricing opt-in (`enable_bundled`); `AgentMessage::Extension` renamed
  `Custom`; `ToolGate` / `InputGuard` run as extensions; encrypted
  reasoning opt-in for the Responses API. **Breaking for event consumers:**
  every message now gets its `MessageStart` / `MessageEnd` (a cancel during
  a retry backoff emits an `Aborted` end), every `TurnStart` a `TurnEnd`,
  and a cancelled run no longer runs its pending tool calls. **Breaking for
  free-function users:** `AgentLoopConfig` is `#[non_exhaustive]` — build it
  with `AgentLoopConfig::new(…)`; `provider::sse` is private.
- **0.24.3** — `SubAgentTool::with_execution_limits`; 503 / 529 and
  in-stream overloads retried; the `[Agent stopped: cancelled]` marker.
- **0.24.2** — built-in tool sandbox fixes (path escapes, `search` flag
  injection, Gemini key out of the URL, tool panics contained).
- **0.24.1** — `retry_safe_events`.
- **yoagent-rutis 0.3.0** — rutis 0.9 (Rust 1.88); Bun and Go plugins;
  the 16 MiB guard.

## More

The book: <https://yologdev.github.io/yoagent/> (source `docs/`), the
design philosophy (`docs/design-philosophy.md`), the API reference
(<https://docs.rs/yoagent>), and `llms.txt` at the repository root for a
map of every page.
