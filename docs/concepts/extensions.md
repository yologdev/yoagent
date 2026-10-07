# Extensions

An **extension** packages one feature (a budget, a policy, redaction, an audit log, a verifier) as one object that hooks into the agent loop wherever it needs to. yoagent defines the contract and calls it; your extensions live in your code or in their own crates.

```rust
use yoagent::extension::*;
use yoagent::provider::ModelConfig;
use yoagent::*;

#[derive(Clone)]
struct NoRm;

#[async_trait::async_trait]
impl RunHooks for NoRm {
    async fn before_tool(&self, call: &ToolCallRequest<'_>) -> ToolDecision {
        if call.tool_name == "bash" && call.args.to_string().contains("rm -rf") {
            ToolDecision::Deny("rm -rf is not allowed".into())
        } else {
            ToolDecision::Allow
        }
    }
}

let agent = Agent::from_config(ModelConfig::claude_sonnet_5())
    .with_extension(ClonedHooks::new("no-rm", NoRm));
```

`ClonedHooks` gives every run a clone of the hooks; `.required()`, `.filters_tool_output()` and `.rechecks_modified_calls()` declare the rest. An extension that builds its hooks per run implements `Extension` itself:

```rust
struct TurnCap { max_turns: usize }

struct TurnCapRun { turns: usize, max: usize }

#[async_trait::async_trait]
impl Extension for TurnCap {
    fn name(&self) -> &str { "turn-cap" }
    async fn start_run(&self, _run: &RunContext<'_>) -> Result<Box<dyn RunHooks>, ExtensionError> {
        Ok(Box::new(TurnCapRun { turns: 0, max: self.max_turns }))
    }
}

#[async_trait::async_trait]
impl RunHooks for TurnCapRun {
    async fn before_model(&mut self, _turn: &TurnContext<'_>) -> TurnDecision {
        self.turns += 1;
        if self.turns > self.max {
            TurnDecision::Stop("turn cap reached".into())
        } else {
            TurnDecision::Continue
        }
    }
}
```

An `Arc<dyn Extension>` (or any `Arc<E>`) is an extension too: install one on several agents, or keep a handle to read its state.

## Runs and state

A **run** is one `prompt*` / `continue_loop*` call, or one delegation for a `SubAgentTool`. `start_run` is called at the start of every run and returns that run's hooks, so state for one run (a run's spend, a verifier's attempts) starts fresh each time and is isolated between concurrent runs and between agents sharing an extension. State that spans runs (a session budget) belongs in the `Extension` itself, behind its own lock or atomics.

`tools`, `on_input`, `before_model`, `on_stop` and `finish` take `&mut self` and run one at a time. `before_tool`, `after_tool` and `on_event` take `&self`: the calls of one response may be judged concurrently, so state they change needs interior mutability (a `Mutex` or atomics) even within one run.

`RunContext` gives each run a unique `run_id`, the host's `label` (`Agent::with_run_label`, a session id say; delegated runs keep their parent's), the run's prompts, its delegation `depth`, for a delegated run the calling run's `parent_run_id` and the `delegated_by` tool call, and its cancel token.

## The hooks

Every `RunHooks` method has a no-op default; implement only what you need.

| Hook | When | With several extensions |
| --- | --- | --- |
| `tools` | Once per run, at its start | The agent's own tools win a name clash, then the earlier extension; sorted by name |
| `on_input` | On a prompted run's input, after the input filters (`continue_loop` has no input). Sees the text the filters saw, without their warnings | First `Reject` wins |
| `before_model` | Before each model request (a retried attempt is not judged again) | Notes appended in order to the latest user turn, never stored; the first `Stop`, or a required extension's `Fail`, ends the run |
| `before_tool` | Before each tool call, after any `ToolMiddleware` | `Deny` wins, `Modify` feeds the next |
| `after_tool` | After each call that ran, errors and panics included, before truncation, storage and `ToolExecutionEnd`. Gets a `ToolOutput` (`result`, `is_error`) to edit | In order, each sees the previous edit |
| `on_stop` | When the model ends with `StopReason::Stop` and nothing is queued (not for an answer cut off at the output limit) | A required extension's `Fail` ends the run; otherwise every `Continue` is sent, one line each |
| `on_event` | Every `AgentEvent` of the run, in order, before the consumer gets it | All |
| `take_failure` | At every point the loop acts on failures: before each turn, before a response's tools run, before `on_stop`, and when the run ends | Each extension asked; a failure returned counts as that extension's |
| `finish` | When the run ends, however it ends (not if the run's future is dropped); bounded by `FINISH_TIMEOUT` | All |

`finish` gets a `RunOutcome`: `end()` is a `RunEnd` (`Completed`, `Stopped { reason }`, `Rejected { reason }`, `Cancelled`, `Failed { error, extension }`), and `stop_reason()` the last assistant message's.

- **`TurnDecision::Stop(reason)`** ends the run like an execution limit: an `[Agent stopped: <reason>]` marker, partial success.
- **`StopDecision::Continue(message)`** appends `[Extension message: <name>] <message>` as a user message (one line per extension when several continue) and runs another turn, at most `max_stop_continues` times per run (default 3, `Agent::with_max_stop_continues`). Continues also count against the execution limits. That message is recognized by `is_loop_injected`, so it is never taken for the user's own request (the tool gate and `user_request()` skip it).
- **A policy whose judgement must survive later rewrites** returns `true` from `Extension::rechecks_modified_calls`. When a later extension modifies a call's arguments, it judges the final arguments again, and a `Deny` there wins (a `Modify` on the recheck is ignored, and `before_tool` may run twice for one call).
- **Report a failure you cannot raise in place with `take_failure`.** `on_event` must not block, and a panic there switches off that extension's observation for the rest of the run. Record the failure instead (from `on_event` or your own background work) and return it from `take_failure`: the loop takes it at its next boundary, the run's end included, so a required extension's failure is never lost.
- **`on_event` stays current.** Before each turn, before a response's tools run, and before `finish`, the loop waits until every event sent so far has been observed. A failure on the run's last events (`AgentEnd` itself) comes too late to change its outcome and is only logged. An `on_event` that panics is not called again for the rest of the run.
- **Hooks watch cancellation.** A hook still awaiting when the run is cancelled is abandoned: a pending input check lets the run start (it ends cancelled before any request; a cancel is not a rejection), a pending `before_tool` denies, a pending `after_tool` withholds the result, `before_model` lets the request go (it then ends aborted).
- **`on_event` can wait behind a running hook.** The observer calls `on_event` once any `&mut self` hook of the same extension has returned, so a slow `before_model` delays the events behind it (never reorders them).

## Failures: advisory and required

`Extension::mode` decides what a failure (a panic, an `Err`, a `Fail` decision, or `start_run` failing) means:

| | Advisory (default) | Required |
| --- | --- | --- |
| `start_run` fails | The extension sits the run out | The run fails |
| `tools`, `before_model`, `on_stop`, `on_event` fail | Logged, hook skipped | The run fails |
| `on_input` fails | The input is rejected | Rejected (the run does not fail) |
| `before_tool` fails | The call is denied | Denied (the run does not fail) |
| `after_tool` fails | The result is replaced by an error naming the extension | Same, and the run fails |
| `on_stop` keeps asking to continue past the cap | The answer is accepted without its approval, with a warning | The run fails |
| `finish` fails | Logged | Logged (too late to change the outcome) |

An extension that **filters tool output** and cannot start fails the run whatever its mode: running without it would let unfiltered output through.

A required failure is never lost. Tool calls not started yet when it happens are answered with an error and never run, and however the run ends (a limit, a cancel, a stop, a provider error), a failure the loop has not acted on yet fails it. A run fails once: a failure recorded while the first is being reported is only logged.

A failed run ends with an assistant message whose stop reason is `Error` and whose `error_message` starts with `EXTENSION_FAILED_PREFIX` (`[Extension failed: <name>] <reason>`). `on_error` is called, and a sub-agent's delegation reports it as a failure.

Panics are contained with `catch_unwind`, which does nothing where panics abort (wasm32 builds, `panic = "abort"`).

## What a hook cannot take back

- **Streamed text** reaches consumers as it arrives. No hook can revoke it, and `on_stop` runs after the final answer has streamed. The run's outcome is `AgentEnd`.
- **Partial tool output** (`ToolExecutionUpdate`, `ProgressMessage`) is sent while a tool runs, before `after_tool` sees the result. While any installed extension returns `true` from `Extension::filters_tool_output`, the loop withholds it, so only the filtered final result is sent, stored and appended. Without such an extension, partial output is delivered unfiltered.
- **Tool arguments and the model's text** are not filtered. An extension using `before_tool` or `on_event` sees them, whatever else it is allowed.

## Sub-agents

Where you install an extension decides what child runs see:

| Install with | Applies to |
| --- | --- |
| `with_extension` | This agent's runs only |
| `with_tree_extension` | This agent's runs **and every run they delegate to**, at any depth, ahead of the child's own extensions. A child cannot remove it. Its `tools` are not offered to child runs |

Use tree extensions for host policy: permissions, deny rules, redaction, audit, and a budget across the whole tree (keep the total in the `Extension`, which every run shares).

A delegation tool you write yourself passes the tree on with `Agent::delegated_from(&ctx)` (when its child is an `Agent`) or `AgentLoopConfig::delegated_from(&ctx)` (when it calls `agent_loop` directly): the child then runs the calling run's tree extensions, at the right depth and under its label, without their tools. `SubAgentTool` does this itself.

## Budget

`extension::Budget` checks a dollar limit before each model request and stops the run once spend has reached it, with `[Agent stopped: budget of $… spent …]`:

```rust
use yoagent::extension::Budget;
use yoagent::provider::{prices, ModelConfig};

// Nothing is priced by default: opt in once, before building configs
// (or give the budget a price yourself with `Budget::usd`).
prices::enable_bundled();
let model = ModelConfig::claude_sonnet_5();
// `None` for an unpriced model: a budget without a price is no limit.
let budget = Budget::for_model(2.0, &model).expect("a priced model");
let agent = Agent::from_config(model).with_extension(budget);
```

- **Per run** by default, sub-agents included: what a sub-agent reports spending (at its own price, or this one if it has none) counts toward the run that delegated.
- **`.across_runs()`** makes it one total for every run the extension serves: all of a session's runs, or, with `with_tree_extension`, a whole delegation tree, where each run's own messages are counted once. Read the total with `spent_usd()` through an `Arc<Budget>` you keep.
- A **per-run budget installed as a tree extension** gives every run of the tree its own limit.
- Messages are priced at the one rate given (`Budget::usd(max, CostConfig)` to choose it). `Budget::for_model` takes the model's `cost`, which is `None` until the process opts in to prices ([Model Pricing](pricing.md#enabling-pricing)). A negative or NaN limit panics; `with_name` tells several budgets apart.
- The check is before each request, so the request that crosses the limit still completes. A provider attempt that fails mid-stream reports no usage, so its billed input tokens are not counted.

## Built on extensions

The decision features are extensions themselves:

- **`with_tool_gate`** (a `before_tool`) and **`with_decision_model`** (a `before_model` note) are appended after the agent's own extensions, so the gate judges the final arguments. Installed with `with_tree_extension`, the gate runs first instead and rechecks rewritten calls (a second decision request).
- **`with_input_guard`** (an `on_input`) is an ordinary extension at its installation position: it screens after every input filter.

So is the [`yoagent-rutis`](https://github.com/yologdev/yoagent/tree/main/integrations/yoagent-rutis) bridge (a separate crate, not yet on crates.io): `RutisBridge::extension()` is one extension over the handlers that [rutis](https://crates.io/crates/rutis) plugins register and unregister at runtime, snapshotted at each run's start. The host keeps the decisions plugins must not make: `.required()`, `.filters_tool_output()`, `.rechecks_modified_calls()`.

## Order with the older hooks

`ToolMiddleware`, `InputFilter`, `TurnHook`, `ToolSource` and the `on_*` closures keep working unchanged. Input filters run before `on_input`, and middleware before `before_tool`. Notes from `before_model` come before a `TurnHook`'s (turn hooks run inside the provider call, once per attempt).

## Examples

Six runnable examples, one feature each. They run offline on a scripted `MockProvider` and check their own results (CI runs them, so they stay current); pass `-- --live` with `DEEPSEEK_API_KEY` or `ANTHROPIC_API_KEY` set to drive a real model instead.

| Example | Shows |
| --- | --- |
| [`extension_policy`](https://github.com/yologdev/yoagent/blob/main/examples/extension_policy.rs) | `before_tool` allowing, rewriting and denying calls with `ClonedHooks`; `rechecks_modified_calls` catching a rewrite by a later extension |
| [`extension_redact`](https://github.com/yologdev/yoagent/blob/main/examples/extension_redact.rs) | An `after_tool` redactor, and why it declares `filters_tool_output` (partial output leaks without it) |
| [`extension_verifier`](https://github.com/yologdev/yoagent/blob/main/examples/extension_verifier.rs) | A per-run `Extension` whose `on_stop` sends the model back, capped by `with_max_stop_continues`; `finish` reading `RunOutcome::end()`, advisory versus required at the cap |
| [`extension_budget`](https://github.com/yologdev/yoagent/blob/main/examples/extension_budget.rs) | `Budget` per run, and `.across_runs()` read through an `Arc` with `spent_usd()` |
| [`extension_tree`](https://github.com/yologdev/yoagent/blob/main/examples/extension_tree.rs) | A host policy installed with `with_tree_extension` judging a `SubAgentTool`'s child and a hand-written delegation tool's (`Agent::delegated_from`, forwarding the parent's cancel and reporting the child's spend with `report_delegated_run`) |
| [`extension_audit`](https://github.com/yologdev/yoagent/blob/main/examples/extension_audit.rs) | `on_event` + `finish` writing a JSON-lines audit log, including a run another extension's `on_input` rejected |

```bash
cargo run --example extension_policy
cargo run --example extension_policy -- --live   # a real model; the checks are skipped
```
