# yoagent-rutis

Extend a [yoagent](https://crates.io/crates/yoagent) `Agent` at runtime with
[rutis](https://crates.io/crates/rutis) plugins.

rutis (a Rust port of the [Cordis](https://github.com/shigma/cordis) plugin
kernel) loads, unloads, reloads and hot-updates plugins, and tears down
everything a plugin registered when it goes. With this bridge such plugins
can contribute to a live agent:

| A plugin contributes | through rutis | into yoagent |
|---|---|---|
| tools | the `ToolRegistry` service (entries owned by the plugin) | `ToolSource` — resolved at each run start |
| tool policy: allow / deny / rewrite args | a `waterfall` of `ToolCallEvent` | `ToolMiddleware` |
| turn notes | a `waterfall` of `TurnEvent` | `TurnHook` |
| input rejection | a `serial` of `InputEvent` | `AsyncInputFilter` |
| observing agent events | `emit` of `AgentEventEmitted` | the `*_with_sender` event channel |

yoagent does not depend on rutis; this crate uses only yoagent's public
builder methods. It pins `rutis = "=0.5.0"`, because rutis is 0.x and its
API moves between minor releases.

## Host

```rust
use rutis::Ctx;
use yoagent_rutis::{AgentRutisExt, RutisBridge};

let root = Ctx::root()?;
let bridge = RutisBridge::install(&root)?;           // once per rutis root
let mut agent = Agent::from_config(config).with_rutis(&bridge);

root.plugin(MyPlugin);                                // any time

let (tx, forwarder) = bridge.event_sender(Some(ui_tx)); // optional: events onto the bus
agent.prompt_with_sender("hello", tx).await;
forwarder.await?;
```

`yoagent_rutis::attach(agent, &ctx)` is the one-call form.
`bridge.attach_sub_agent(sub)` (or `sub.with_rutis(&bridge)`) wires a
`SubAgentTool` the same way.

## Plugin

```rust
use yoagent_rutis::{AgentPlugin, ToolVerdict};

root.plugin(
    AgentPlugin::new("text-tools")
        .with_tool(WordCount)
        .with_policy(|call| match call.tool_name() {
            "bash" => ToolVerdict::deny("shell access is disabled"),
            _ => ToolVerdict::Allow,
        }),
);
```

For per-generation state or config, implement `rutis::Plugin` (or
`PluginFactory`) and use the `PluginCtxExt` methods on the plugin's `Ctx`:
`provide_tool`, `on_tool_call`, `on_tool_call_async`, `on_turn`, `on_input`,
`on_agent_event`. Declare `TypeKey::of::<ToolRegistry>()` in `injects` so a
tool plugin waits for the bridge.

Runnable offline: `cargo run --example policy_plugin` (a tool plugin plus a
policy plugin that denies a tool by name and caps calls per tool).

## Semantics

### Tools

- **Tools change at run boundaries.** An agent asks for plugin tools once per
  run (`prompt*`, `continue_loop*`). A plugin unloaded mid-run leaves its tools
  offered until the run ends. Each tool is bound to the plugin generation that
  provided it: a call that *starts* after that generation began unloading
  fails as "no longer available", and a call *in flight* is abandoned and fails
  with "plugin unloaded during the call" (the plugin's cancellation token is
  cancelled before its cleanup runs).
- **Restart / config update mid-run**: the run keeps the old generation's tool
  and gets that error; the new generation's tool — whose schema may differ —
  is offered from the next run. The bridge never silently rebinds a call.
- **Everything a plugin registers goes when it goes** — on `dispose`, restart,
  config `update`, and dependency-driven eviction (and comes back on reload).
- **Tool names are unique across plugins.** Registering a name a live plugin
  tool holds is refused with `CordisError::ServiceExists` and logged
  (`warn!`, naming the tool and the holder). A plugin that `?`s it fails to
  load, and is **not retried** when the holder later unloads. The agent's own
  tools win over plugin tools.

### Policy, input, turn notes

- **Every policy must pass.** Any `Deny` wins; a denying listener does not
  call `next`, so later policies (a rate counter, say) never see a denied
  call. A raw `WaterfallListener` that returns `Allow` without calling `next`
  would skip later policies; the bridge treats that as a denial.
- **Fail closed** — a listener that errors or panics, a host that has shut
  down (`root.shutdown()`), or a chain past its timeout denies the call /
  rejects the prompt.
- **…except the empty chain.** No policy listener allows, no input listener
  passes. That includes the window **while a policy plugin reloads**
  (restart, config update, dependency-driven eviction drain the old listener
  before the new generation registers) and **before it first becomes
  active**. If a policy plugin is load-bearing, build the bridge with
  `.require_policy()`: a call no policy judged is then denied.
- **Turn notes fail open**: a failing or slow turn chain keeps the notes
  added so far. Notes are recomputed for every request. A raw turn listener
  that does not call `next` drops the notes of every later listener.
- **Finite default timeouts** per chain: policy 60 s (then deny), input
  30 s (then reject), turn notes 5 s (then keep notes so far). yoagent awaits
  these hooks without watching the run's cancel token, so `Agent::abort()`
  cannot unstick a hung plugin. `with_policy_timeout(None)` (and the input /
  turn equivalents) removes a bound — you then own liveness, e.g. for an
  approval prompt in an `on_tool_call_async` policy.

### Events

- **Events never block the agent.** `emit` only queues; each listener sees an
  agent's events in the order the agent produced them.
- **Cost**: every published event with a listener spawns a task, and
  streaming yields one `MessageUpdate` event per text delta.
- **One queue for the whole bus**: a slow listener delays the delivery of
  *every* agent's later events (not the agents) and can build an unbounded
  backlog. Label each agent's events with `bridge.event_sender_labeled("a", ..)`
  to tell agents apart (`AgentEventEmitted::label`).
- Your own consumer (`forward`) gets each event before the bus does; if it
  goes away, publishing continues.

### Host

- **One bridge per rutis root**: every attached agent shares its plugins.
- **Route rutis's `ErrorSink`** (default: `eprintln!`) into your logging with
  `Ctx::root_with_sink` — listener failures on `emit` and cleanup errors are
  reported there, not to the agent.

## Publishing

`publish = false` for now: the crate needs the yoagent release that ships
`ToolSource`. Flip it (and bump the `yoagent` requirement) after that release.

## License

MIT
