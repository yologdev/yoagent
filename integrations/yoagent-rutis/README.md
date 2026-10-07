# yoagent-rutis

Extend [yoagent](https://crates.io/crates/yoagent) agents at runtime with
[rutis](https://crates.io/crates/rutis) plugins, through one yoagent
`Extension`.

**Status:** 0.1.0, not yet on crates.io; needs yoagent's `Extension` (0.25).

rutis (a Rust port of the [Cordis](https://github.com/shigma/cordis) plugin
kernel) loads, unloads, reloads and hot-updates plugins, and tears down
everything a plugin registered when it goes. With this bridge such plugins
contribute to live agents:

1. `RutisBridge::install(&root)` provides the `yoagent` registry service.
2. A plugin registers a **handler**: a name plus whichever hooks it
   implements. The registration ends when the plugin unloads.
3. `bridge.extension()` is one yoagent `Extension`. Each run snapshots the
   handlers registered when it starts.

| Hook | A handler can |
|---|---|
| `tools` / `with_tool` | offer tools (static ones are checked for unique names) |
| `before_tool` | allow, deny or rewrite **every** tool call, the agent's own tools too |
| `after_tool` | edit a tool's output before the model sees it (redaction) |
| `before_model` | add a note to the request, or stop the run |
| `on_input` | reject a prompt |
| `on_stop` | send the model back with a message (a verifier) |
| `finish` | see how the run ended |
| `on_event` | observe the run's events (Rust handlers) |

Every event is also published on the rutis bus as `AgentEventEmitted`.

yoagent does not depend on rutis; this crate uses only yoagent's public API.
It depends on `rutis = "0.6"` (0.x caret: any 0.6.x, never 0.7). rutis types
are part of this crate's API, so every rutis minor bump is a yoagent-rutis
minor bump.

## Host

```rust
use rutis::Ctx;
use yoagent_rutis::RutisBridge;

let root = Ctx::root()?;
let bridge = RutisBridge::install(&root)?;            // on the root, once
let mut agent = Agent::from_config(config)
    .with_extension(bridge.extension());               // or with_tree_extension

root.plugin(MyPlugin);                                 // any time
agent.prompt("hello").await;
```

The host decides how much it trusts its plugins:

| `bridge.extension()` option | Effect |
|---|---|
| `.required()` | a failing `tools`, `before_model`, `after_tool`, `on_stop` or `on_event` fails the run (default: logged, the handler skipped). A `tools` / `on_event` failure fails it at the next decision point (tool calls are denied meanwhile); when the run ends before that point comes (an execution limit, a final answer that is not a plain stop, a cancel, a failure on `AgentEnd`), it is only logged. A handler whose plugin unloads mid-run never fails the run: an unloaded `after_tool` still withholds the result |
| `.filters_tool_output()` | plugins redact output: yoagent withholds partial tool output, so only the filtered result is sent |
| `.rechecks_modified_calls()` | plugin policy judges a call again when an extension installed later rewrote it |
| `.require_policy()` | a run that starts with no `before_tool` handler denies every tool call |
| `.with_policy_timeout(..)`, `.with_input_timeout(..)`, `.with_turn_timeout(..)`, `.with_timeout(..)` | per-call bounds (below) |

**`with_extension` or `with_tree_extension`.** Installed with
`with_tree_extension`, plugin policy, input checks, redaction and notes also
cover every sub-agent run at any depth, and a child cannot remove them;
yoagent does not offer a tree extension's tools to child runs. To give a
`SubAgentTool` the plugin tools too, install the extension on it with
`with_extension` (its runs then go through the handlers twice if the parent
also installed it for its tree).

**Install on the root.** A bridge installed on a plugin's context is bound to
that plugin's generation: once the plugin unloads or reloads, the bridge reads
as stopped for good — every tool call denied, every prompt rejected.

## Plugin

```rust
use yoagent::ToolDecision;
use yoagent_rutis::{AgentPlugin, Handler};

root.plugin(AgentPlugin::new(
    Handler::new("text-tools")
        .with_tool(WordCount)
        .with_before_tool(|call| match call.tool.as_str() {
            "bash" => ToolDecision::Deny("shell access is disabled".into()),
            _ => ToolDecision::Allow,
        })
        .with_after_tool(|_call, output| {
            redact(output);
            Ok(())
        }),
));
```

Every hook has a synchronous form (`with_before_tool`) and an `_async` one
(`with_before_tool_async`) whose closure takes the owned argument and returns
a future of `Result<_, ExtensionError>`. Arguments are plain data:
`ToolCall { tool, call_id, args, user_request, latest_user_text, run }`,
`Turn`, `Input`, `Stop`, each with the `RunInfo { run_id, label, depth, .. }`;
decisions are yoagent's (`ToolDecision`, `TurnDecision`, `InputDecision`,
`StopDecision`).

For per-generation state or config, implement `rutis::Plugin` (or
`PluginFactory`), build the `Handler` in `apply`, and register it with
`ctx.register_handler(handler)` (`PluginCtxExt`; also `provide_tool` and
`on_agent_event`). Declare `TypeKey::of::<Registry>()` in `injects` so the
plugin waits for the bridge.

Runnable offline: `cargo run --example policy_plugin` from this directory, or
with `--manifest-path integrations/yoagent-rutis/Cargo.toml` from the repo
root (a tool plugin, a policy plugin that denies a tool by name and caps
calls per tool, and a redactor).

## Semantics

### Runs and plugin lifecycles

- **A run uses the handlers registered when it started.** Plugins loaded,
  unloaded or reloaded during a run change the next run, not this one.
- **A handler whose plugin unloads mid-run is unavailable** for the rest of
  that run (the plugin's cancellation token is cancelled before its cleanup
  runs): its tool calls fail ("no longer available", or "plugin unloaded
  during the call" for one in flight), its `before_tool` denies every call,
  its `on_input` rejects, its `after_tool` withholds the result (fail closed,
  but not a failure: a `required()` run goes on), and its `before_model`,
  `on_stop`, `on_event` and `finish` are skipped. A restart
  or config update is the same: the new generation serves the next run. The
  bridge never rebinds a run to a newer generation.
- **Everything a plugin registers goes when it goes** — on `dispose`,
  restart, config `update`, and dependency-driven eviction (and comes back on
  reload).
- **Names are unique across plugins**: handler names, and static tool names.
  A clash is refused with `CordisError::ServiceExists` and logged (`warn!`,
  naming the holder). A plugin that `?`s it fails to load, and is **not
  retried** when the holder later unloads. Per-run tools (`with_tools`) are
  not checked at registration: on a clash the earlier handler's tool wins
  (logged). The agent's own tools win over every plugin tool.

### How handlers combine

Handlers run in registration order (a reloaded plugin registers again, after
the others).

| Hook | Combination | A handler that errors, panics or times out |
|---|---|---|
| `tools` | static tools, then per-run ones | contributes nothing (`required()`: the run fails) |
| `on_input` | the first `Reject` wins | rejects the input |
| `before_model` | notes joined one per line; the first `Stop` ends the run | skipped (`required()`: the run fails) |
| `before_tool` | a `Deny` wins (later handlers never see the call); a `Modify` feeds the next handler | denies the call |
| `after_tool` | each sees the previous edit | withholds the result (`required()`: the run fails too) |
| `on_stop` | every `Continue` message is sent, joined | skipped (`required()`: the run fails) |
| `on_event` | all, in order, synchronously | that handler is switched off for the run; the others and bus publishing go on (`required()`: the run fails at its next decision point) |
| `finish` | all, concurrently | logged |

- **No policy means allow.** A run that starts with no `before_tool` handler
  allows every call — including while a policy plugin reloads (the old
  handler is removed before the new generation registers) and before it first
  becomes active. If a policy plugin is load-bearing, use
  `.require_policy()`. Input checks have no such switch.
- **A host that is not running** (`root.shutdown()`, or a root disposed or
  restarted since the bridge was installed):
  every plugin went with it, so the bridge denies every tool call (checked per
  call), rejects every prompt and adds no notes, rather than reading the empty
  registry as "nobody objected".
- **Timeouts** per handler call: policy 60 s (`before_tool`, `after_tool`,
  `on_stop`), input 30 s (`on_input`), turn 5 s (`before_model`, `tools`,
  `finish`). Past it the call counts as failed (see the table). yoagent also
  abandons a hook when the run is cancelled (`Agent::abort()`), so `None`
  (no bound) is safe for a policy that waits on a human.
- **`rechecks_modified_calls()`** means handlers' `before_tool` may run twice
  for one call.

### Events

- **Every event of a run is published on the bus** as `AgentEventEmitted`
  (`event()`, `run_id()`, `label()` — the host's `Agent::with_run_label` —
  and `depth()`), from the extension's `on_event`. Publishing never blocks the
  agent: `emit` only queues, and does nothing without listeners. Each
  listener sees a run's events in order; a listener may see an event before
  or after the run's own consumer does.
- **Cost**: each event is cloned once and spawns a task while any bus
  listener is registered, and streaming yields one `MessageUpdate` per text
  delta.
- **One queue for the whole bus**: a slow listener delays the delivery of
  *every* run's later events (not the runs) and can build an unbounded
  backlog. A handler's `with_on_event` runs synchronously inside the run
  instead: keep it cheap.

### Host

- **One bridge per rutis root**: every agent using its extension shares its
  plugins.
- **Route rutis's `ErrorSink`** (default: `eprintln!`) into your logging with
  `Ctx::root_with_sink` — bus listener failures and cleanup errors are
  reported there, not to the agent.

## Publishing

Not yet on crates.io (`publish = false`). It needs the first yoagent release
with `Extension` (0.25); raise the `yoagent` requirement to it and flip
`publish`. yoagent types are in its API too, so a yoagent minor bump is a
yoagent-rutis minor bump. Until then, depend on it by git, and take yoagent
from the same git source: a crates.io `yoagent` next to a git `yoagent-rutis`
is two `yoagent` crates whose types do not match.

```toml
yoagent = { git = "https://github.com/yologdev/yoagent" }
yoagent-rutis = { git = "https://github.com/yologdev/yoagent" }
```

## License

MIT
