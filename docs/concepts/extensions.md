# Extensions

An **extension** packages one feature (a budget, a policy, redaction, an audit log, a verifier) as one object that hooks into the agent loop wherever it needs to. yoagent defines the contract and calls it; your extensions live in your code or in their own crates.

```rust
use yoagent::extension::*;
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

let agent = Agent::from_config(ModelConfig::deepseek("deepseek-v4-pro", "DeepSeek V4 Pro"))
    .with_extension(Stateless::new("no-rm", NoRm));
```

`Stateless` gives every run a clone of the hooks. An extension with state for one run implements `Extension` itself:

```rust
struct Budget { max_turns: usize }

struct BudgetRun { turns: usize, max: usize }

#[async_trait::async_trait]
impl Extension for Budget {
    fn name(&self) -> &str { "budget" }
    async fn start_run(&self, _run: &RunContext<'_>) -> Result<Box<dyn RunHooks>, ExtensionError> {
        Ok(Box::new(BudgetRun { turns: 0, max: self.max_turns }))
    }
}

#[async_trait::async_trait]
impl RunHooks for BudgetRun {
    async fn before_model(&mut self, _turn: &TurnContext<'_>) -> TurnDecision {
        self.turns += 1;
        if self.turns > self.max {
            TurnDecision::Stop("turn budget spent".into())
        } else {
            TurnDecision::Continue
        }
    }
}
```

## Runs and state

A **run** is one `prompt*` / `continue_loop*` call, or one delegation for a `SubAgentTool`. `start_run` is called at the start of every run and returns that run's hooks, so state for one run (a run's spend, a verifier's attempts) starts fresh each time and is isolated between concurrent runs and between agents sharing an extension. State that spans runs (a session budget) belongs in the `Extension` itself, behind its own lock or atomics. The per-call hooks `before_tool` and `after_tool` take `&self`, because the calls of one response may be judged concurrently: state they change needs interior mutability (a `Mutex` or atomics) even within one run.

`RunContext` gives each run a unique `run_id`, the host's `label` (`Agent::with_run_label`, for example a session id), the run's prompts, its delegation `depth`, and its cancel token.

## The hooks

Every `RunHooks` method has a no-op default; implement only what you need.

| Hook | When | With several extensions |
| --- | --- | --- |
| `tools` | Once per run, at its start | The agent's own tools win a name clash, then the earlier extension; sorted by name |
| `on_input` | On a prompted run's input, after the input filters (`continue_loop` has no input) | First `Reject` wins |
| `before_model` | Before each model request (a retried attempt is not judged again) | Notes appended in order to the latest user turn, never stored; first `Stop` or `Fail` ends the run |
| `before_tool` | Before each tool call, after any `ToolMiddleware`. Takes `&self`: the calls of one response are judged concurrently under parallel execution | `Deny` wins, `Modify` feeds the next |
| `after_tool` | After each call that ran, errors and panics included, before truncation and `ToolExecutionEnd`. Gets a `ToolOutput` (`result`, `is_error`) to edit | In order, each sees the previous edit |
| `on_stop` | When the model ends with `StopReason::Stop` and nothing is queued | First `Fail` wins, else first `Continue` |
| `finish` | When the run ends, however it ends (not if the run's future is dropped) | All |

`Extension::on_event` observes every `AgentEvent` of the run, in order, before the event reaches the consumer. Before each turn and before `finish`, the loop waits until every event sent so far has been observed, so what `on_event` recorded is current when the hooks run. A failure on the run's last events (`AgentEnd` itself) comes too late to change its outcome and is only logged.

- **`TurnDecision::Stop(reason)`** ends the run like an execution limit: an `[Agent stopped: <reason>]` marker, partial success.
- **`StopDecision::Continue(message)`** appends `[Extension <name>] <message>` as a user message (one line per extension when several continue) and runs another turn, at most `max_stop_continues` times per run (default 3, `Agent::with_max_stop_continues`). That message is recognized by `is_loop_injected`, so it is never taken for the user's own request (the tool gate and `user_request()` skip it).
- **A policy whose judgement must survive later rewrites** returns `true` from `Extension::rechecks_modified_calls`. When a later extension modifies a call's arguments, it judges the final arguments again, and a `Deny` there wins (a `Modify` on the recheck is ignored, and `before_tool` may run twice for one call).

## Failures: advisory and required

`Extension::mode` decides what a failure (a panic, an `Err`, a `Fail` decision, or `start_run` failing) means:

| | Advisory (default) | Required |
| --- | --- | --- |
| `start_run` fails | The extension sits the run out | The run fails |
| `tools`, `before_model`, `on_stop`, `on_event` fail | Logged, hook skipped | The run fails |
| `on_input` fails | The input is rejected | Rejected |
| `before_tool` fails | The call is denied | Denied |
| `after_tool` fails | The result is replaced by an error naming the extension | Same, and the run fails |
| `on_stop` keeps asking to continue past the cap | The answer is accepted without its approval, with a warning | The run fails |

An extension that **filters tool output** and cannot start fails the run whatever its mode: running without it would let unfiltered output through. A required failure is never lost: however the run ends (a limit, a cancel, a stop, a provider error), a failure the loop has not acted on yet fails it. And a run fails once: a failure recorded while the first is being reported is only logged.

A failed run ends with an assistant message whose stop reason is `Error` and whose `error_message` starts with `EXTENSION_FAILED_PREFIX` (`[Extension failed: <name>] <reason>`). `on_error` is called, and a sub-agent's delegation reports it as a failure.

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

Use tree extensions for host policy: permissions, deny rules, redaction, audit, and a budget across the whole tree (keep the total in the `Extension`, which every run shares). A child run keeps the parent's run label. A custom delegation tool honours all of this through `ToolContext::tree_extensions()`, `ToolContext::delegation_depth()` and `ToolContext::run_label()`.

## Budget

`extension::Budget` checks a dollar limit before each model request and stops the run once spend has reached it, with `[Agent stopped: budget of $… spent …]`:

```rust
use yoagent::extension::Budget;

let model = ModelConfig::claude_sonnet_5();
// `None` for an unpriced model: a budget without a price is no limit.
let budget = Budget::for_model(2.0, &model).expect("a priced model");
let agent = Agent::from_config(model).with_extension(budget);
```

The limit is per run by default. `.across_runs()` makes it one total for every run the extension serves: all of a session's runs, or, with `with_tree_extension`, a whole delegation tree. Every message is priced at the one rate given (`Budget::usd(max, CostConfig)` to choose it), so sub-agents on other models are priced approximately. The check is before each request, so the request that crosses the limit still completes. A provider attempt that fails mid-stream reports no usage, so its billed input tokens are not counted. `spent_usd()` gives an across-runs budget's total.

## Built on extensions

The decision features are extensions themselves: `with_tool_gate` (`before_tool`), `with_input_guard` (`on_input`) and `with_decision_model` (a `before_model` note). They are appended after the agent's own extensions, so the gate judges the final arguments.

## Order with the older hooks

`ToolMiddleware`, `InputFilter`, `TurnHook`, `ToolSource` and the `on_*` closures keep working unchanged. At each point the older hook runs first: input filters before `on_input`, middleware before `before_tool`. Notes from `before_model` are appended before a `TurnHook`'s (turn hooks run inside the provider call, once per attempt).
