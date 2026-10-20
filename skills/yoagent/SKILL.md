---
name: yoagent
description: Use when building, extending or upgrading an agent on the yoagent Rust crate. Maps each job (tools, policies, verifiers, budgets, sub-agents, plugins from other ecosystems, retries, structured output, tests) to the yoagent feature for it, with tips, pitfalls and what changed per version.
---

# yoagent: which feature for which job

Written for **yoagent 0.25.11** (and yoagent-rutis 0.7.1); the recipes
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
**refused**. Measured (2026-10-16): of the 20 most-downloaded packages, pi 12 and
DSH 3 are usable (`docs/evals/plugin-compatibility.md`). Start from the
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

- **0.25.11** — `tool_output_max_lines` is one budget per tool result,
  shared by its text blocks (a many-block MCP result reached the model as
  blocks × the budget): short blocks stay whole, long ones split the rest,
  each cut with its own marker and stash key. Older multi-block results
  over the budget are cut once more at their first level-1 compaction.
- **0.25.10** — `ModelConfig::presets()` / `preset(provider, id)` /
  `generic(provider, id)`: for a model given as text, write
  `preset(p, id).or_else(|| generic(p, id))` and tell the user when the
  fallback ran (its limits are the provider's defaults, not the model's).
  `RunContext::spend_handle()` → `SpendHandle::add(&usage, &model)`: an
  extension's own model calls (a host service's, for a plugin) count in
  `Budget` and `SessionStats::extensions`. Steering and follow-ups taken
  before a cancel are kept in the history (they were lost);
  `Batched { size: 0 }` no longer answers a steered call twice.
- **0.25.9** — a tool result with several text blocks reaches the model
  whole (Anthropic, OpenAI-compatible, Responses, Gemini and Vertex sent
  only the first block; an MCP server answering in blocks was seen as its
  intro line). Upgrade if you use MCP tools.
- **0.25.8** — eleven priced presets with real context windows and
  output limits (`claude_sonnet_5_5`, `claude_haiku_5_5`, `gpt_6_1_sol`,
  `gemini_4_argon`, `gemini_3_8_flash`, `grok_4_7`, `mistral_large_4`,
  `glm_5_3`, `glm_5_3_flash`, `qwen_3_8_max`, `qwen_3_8_flash`; the three
  on promotion carry the regular rate). Gemini thinking tokens and
  OpenAI-compatible reasoning tokens reported beside `completion_tokens`
  now count as output (cost, context tracking, `max_total_tokens`). The
  file tools refuse a FIFO, device, socket or directory. **Behaviour
  change:** `bash` returns when `bash` exits; a background job still
  holding its output is left running with that output discarded
  (`NOTE:` line, `details.output_still_open`) instead of holding the call
  until the timeout killed it — redirect a job's output to keep it.
- **0.25.7** — `McpToolAdapter::with_prefix` / `from_client_with_prefix`
  name the tool `<prefix>__<tool>` (the prefix was ignored); the server is
  still called with its own name. The `with_mcp_server_*` helpers add no
  prefix: wrap a server yourself when two offer the same tool name.
- **0.25.6** — DeepSeek is priced (after `prices::enable_bundled()`):
  `deepseek-flash`, `deepseek-v4-pro`, `deepseek-v4-flash`, at the **peak**
  rate (off-peak is half; a flat price has no time of day), so its costs are
  an upper bound and a `Budget` stops early rather than late. Install the
  off-peak rates as an override to count those (`docs/concepts/pricing.md`).
- **0.25.5** — the built-in `search` tool ignores the user's ripgrep
  config (`--no-config`) and `GREP_OPTIONS`, finds `rg` without `which`
  (`details.engine` says `"rg"` or `"grep"`), and reports a search that
  ended abnormally as an error, never as "No matches". The experimental
  frontend and its examples moved to
  [yobuild](https://github.com/yologdev/yobuild).
- **0.25.4** — `ToolGate` (feature `decision`) sends a call's arguments
  whole when they fit (`with_max_argument_chars`, default 48,000, and the
  decision backend's token limits) and otherwise cuts only the longest
  strings, listing them under `tool_call.omitted`. **Behaviour change:**
  `bash`'s command is a full view (`with_full_view`; drop it with
  `without_default_full_views`), so a command too large to review whole is
  denied instead of judged from its head and tail.
- **0.25.3** — this skill and `llms.txt` ship in the crate; no code changes.
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
- **yoagent-rutis 0.7.1** — pi adapter: `model` / `models` give
  extensions `ctx.model` (default: the run's model id under provider
  `yoagent`) and a read-only `ctx.modelRegistry`; `execFor` grants
  `pi.exec` per extension file. pi observer events (`agent_end`,
  `turn_end`, `tool_execution_*`, …) now fire. Opt-in DSH host
  (`adapters::dsh_host_rows`) gives DSH plugins a stand-in DSH agent.
- **yoagent-rutis 0.7.0** — pi adapter: **`pi.exec` is closed by
  default** (row config `exec`: `true`, or an exact command list; only the
  command is checked, so granting `git`/`sh`/`node` grants anything; a
  refusal is code 126, never a throw). Fixed answers are logged as
  `[pi] fixed answer: <question> → <answer>`; `projectTrusted` sets
  `ctx.isProjectTrusted()` (default false). Loads write wrappers to
  `$TMPDIR/yoagent-pi-wrappers-<uid>`.
- **yoagent-rutis 0.6.1** — `rows::wait_running` (feature `loader`): a
  host's plugin rows can wait for a service nobody provides, and `reconcile`
  still reports success; this waits until they run and names, per waiting
  row, the services it lacks.
- **yoagent-rutis 0.6.0** — rutis 0.12 (move the npm / PyPI / Go runtimes
  to 0.12 with it; undeclared `ctx.provide`d services now reach rutis, and
  `ServiceCatalog::new()` shares every name). A TypeScript, Bun, Python or
  Go plugin's handlers are owned by its row (`offered_by().owner()` is the
  row id, seen by the host) and go when the row unloads or restarts —
  declare `yoagent` in the plugin's `inject` to be attributed.
- **yoagent-rutis 0.5.1** — pi adapter fix: a run calls only the tools it
  was offered (top-level and nested), so `offered_by.source` names what
  runs; `nestedCalls` (`"own-extension"`, `"off"`) narrows what a pi tool
  can call through `ctx.executeTool`.
- **yoagent-rutis 0.5.0** — rutis 0.11 (move the npm / PyPI / Go
  runtimes to 0.11 with it: an integer beyond ±(2^53−1) now crosses as a
  big integer, which older peers refuse); `Registry::offered_by(run_id,
  tool)` names the plugin handler (and, for pi, the extension file) that
  offered a tool, for policies that trust tools per plugin; per-handler hook
  timing (`Registry::timings()` / `take_timings()`).
- **yoagent-rutis 0.4.0** — `adapters()` (the pi and DSH adapter files the
  crate ships: copy `root`, `npm ci` the copy) and the `probe` feature: vet
  a pi extension or DSH plugin without running it as an agent; each
  report's `problem()` is the verdict.
- **yoagent-rutis 0.3.0** — rutis 0.9 (Rust 1.88); Bun and Go plugins;
  the 16 MiB guard.

## More

The book: <https://yologdev.github.io/yoagent/> (source `docs/`), the
design philosophy (`docs/design-philosophy.md`), the API reference
(<https://docs.rs/yoagent>), and `llms.txt` at the repository root for a
map of every page.
