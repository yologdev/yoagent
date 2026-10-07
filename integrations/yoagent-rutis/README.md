# yoagent-rutis

Extend [yoagent](https://crates.io/crates/yoagent) agents at runtime with
[rutis](https://crates.io/crates/rutis) plugins — in Rust, TypeScript or
Python — through one yoagent `Extension`.

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
| `on_event` | observe the run's events (opt-in and filtered by type for TypeScript / Python) |

Every event is also published on the rutis bus as `AgentEventEmitted`.

yoagent does not depend on rutis; this crate uses only yoagent's public API.
It depends on `rutis = "0.6"` (0.x caret: any 0.6.x, never 0.7) and, with a
language feature, `rutis-bridge = "0.7"` (the matching release train). rutis
types are part of this crate's API, so every rutis or rutis-bridge minor bump
is a yoagent-rutis minor bump.

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
| `.required()` | a failing `tools`, `before_model`, `after_tool`, `on_stop` or `on_event` fails the run (default: logged, the handler skipped). A `tools` / `on_event` failure fails it at the next decision point (tool calls are denied meanwhile); one in the run's last events (`TurnEnd`, `AgentEnd`) is only logged. A handler whose plugin unloads mid-run never fails the run: an unloaded `after_tool` still withholds the result |
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

## TypeScript and Python plugins

With a language feature, `RutisBridge::install` also provides the registry
to plugins in other languages, through
[rutis-bridge](https://crates.io/crates/rutis-bridge) 0.7, as the host
service `yoagent`:

| Feature | Adds (rutis-bridge feature of the same name) |
|---|---|
| *(default)* | Rust plugins only |
| `node` | TypeScript / JavaScript plugins in local Node runtimes (Node 24+, Linux/macOS) |
| `python` | Python plugins in local Python runtimes (Python 3.12+, Linux/macOS) |
| `websocket` | runtimes and nodes on other machines, over `wss` |

```toml
yoagent-rutis = { git = "https://github.com/yologdev/yoagent", features = ["node", "python"] }
```

A plugin injects `yoagent` and registers a handler: an object (in Python, a
class instance or a dict) of async functions, passed by reference — rutis
calls them in the plugin's own process. `register(name, handler, options?)`
returns a function that unregisters it; pass it to `ctx.effect` so the
handler goes when the plugin unloads.

```ts
import { definePlugin } from '@arcships/rutis'
import type { Yoagent } from './yoagent.d.ts'

export default definePlugin({
  inject: ['yoagent'],
  apply(ctx) {
    const yoagent = ctx.use<Yoagent>('yoagent')
    ctx.effect(yoagent.register('no-shell', {
      async before_tool(call) {
        if (call.tool === 'bash') return { deny: 'shell access is disabled' }
      },
      async after_tool(call, output) {
        if (output.text.includes('sk-')) return { text: output.text.replace(/sk-\S+/g, '[key]') }
      },
    }))
  },
})
```

```python
class Handler:
    async def before_tool(self, call):
        if call["tool"] == "rm":
            return {"deny": "rm is disabled"}

inject = ["yoagent"]

def apply(ctx, config):
    ctx.effect(ctx.use("yoagent").register("no-rm", Handler()))
```

- **The handler shape** — every hook, its plain-JSON argument
  (`{tool, call_id, args, user_request, run_id, label, depth, ...}`) and what
  it returns — is in [`plugins/yoagent.d.ts`](plugins/yoagent.d.ts); copy it
  into your plugin (there is no SDK package yet). Python uses the same names
  and shapes. Examples: [`plugins/ts/example.ts`](plugins/ts/example.ts),
  [`plugins/python/yoagent_example.py`](plugins/python/yoagent_example.py).
- **Every hook is async.** rutis warns that synchronous calls across
  runtimes can deadlock. `register` itself is synchronous: it reads the
  handler's members back while the plugin waits.
- **`on_event` is opt-in and filtered**: one cross-process call per event,
  so only the types in `options.events` (`["toolExecutionEnd", "agentEnd"]`)
  are sent, in order, asynchronously (the run never waits for them; events
  sent before `finish` reach the plugin before it, `agentEnd` comes after).
- **A crashed runtime withdraws its handlers.** When a runtime process exits,
  its session closes and every handler it registered is removed; a run that
  still holds one sees it as unavailable (its tool calls fail, its
  `before_tool` denies, its `on_input` rejects, its `after_tool` withholds).
- **The same rules as Rust handlers**: one registry and one name space for
  every language, registration order, fail closed on errors and timeouts.
- **An abandoned JavaScript hook keeps running.** When the bridge stops
  waiting for a hook — its timeout passed, the run was cancelled
  (`Agent::abort()`), its plugin unloaded — the answer is discarded, but a
  JavaScript function is not stopped: no `AbortSignal` is passed, so it runs
  to completion. That includes **`call_tool` side effects after a run was
  cancelled** (a write, a request, a payment): make tools idempotent, or check
  your own deadline before acting. A Python coroutine is cancelled
  (`asyncio.CancelledError` at its next `await`). (rutis-bridge 0.7 can pass
  an `AbortSignal`, but only as an extra positional argument, which a Python
  method with a fixed signature would refuse; the bridge cannot tell the two
  apart, so it passes none.)
- **The bridge never loads plugins**: the host does, typically with
  [rutis-loader](https://crates.io/crates/rutis-loader) rows, and must share
  `yoagent` in the loader's catalog (`catalog.register_shared("yoagent")` or
  `share_by_name()`). [`examples/language_plugins.rs`](examples/language_plugins.rs)
  is a complete host: `npm ci` in `plugins/`, a Python 3.12+ with
  `rutis==0.7.0` in `plugins/.venv`, then
  `cargo run --features node,python --example language_plugins`.

### Remote handlers: latency and trust

With `websocket`, a handler can live on another machine: a runtime there
(`RuntimePlugin::remote`), or a node the host exports `yoagent` to. It then
sits inside every run of every agent using the extension:

- **Latency.** Each hook is a network round trip, and `before_tool` runs for
  **every** tool call — the agent's own tools too — once per handler, before
  the tool starts. A slow link slows every tool call. The default timeouts
  (60 s policy, 30 s input, 5 s turn) are generous for a remote handler;
  lower them with `with_policy_timeout` & co. A timeout denies the call
  (`before_tool`), rejects the input (`on_input`) or withholds the result
  (`after_tool`).
- **Trust.** A remote handler sees what its hooks receive: tool names and
  arguments, the user's request, tool output (`after_tool`), and with
  `on_event` whatever events it subscribes to (messages included). It can
  deny, rewrite arguments and edit output. Exporting `yoagent` to a peer lets
  that peer register handlers: treat it as part of the agent's trust
  boundary, use `wss` with a per-peer token (see rutis's guide to nodes), and
  set `.filters_tool_output()` only if you trust it to redact.
- **Registration** with an object handler reads each hook name back with a
  synchronous call (about ten round trips per `register`; a Python dict
  needs none).
- **Which session.** A handler is removed when the session that registered
  it closes. For a plugin behind a link, that is the link: a remote runtime
  that crashes while the link stays up leaves its handlers registered and
  failing (every tool call denied) until the plugin's own `ctx.effect`
  cleanup or the link goes, and its name stays taken.
- **Availability.** An unreachable handler fails closed for that call, but a
  peer that disconnects has its handlers removed: later runs have no policy
  from it, and allow, unless the extension has `.require_policy()`. Use it
  when a remote policy is load-bearing.

The end-to-end tests cover local Node and Python runtimes; a remote node over
`wss` is not tested here.

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
