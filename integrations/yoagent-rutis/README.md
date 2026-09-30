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

- **Tools change at run boundaries.** An agent asks for plugin tools once per
  run (`prompt*`, `continue_loop*`). A plugin unloaded mid-run leaves its tools
  offered until the run ends, but calling one then returns an error result
  instead of reaching the torn-down plugin.
- **Everything a plugin registers goes when it goes** — on `dispose`, restart,
  config `update`, and dependency-driven eviction (and comes back on reload).
- **Tool names are unique across plugins.** Registering a name a live plugin
  tool holds is refused with `CordisError::ServiceExists` (a plugin that `?`s
  it fails to load). The agent's own tools win over plugin tools.
- **Policy and input filtering fail closed**: a listener that errors or
  panics, a refused dispatch, or a chain past `RutisBridge::with_timeout`
  denies the call / rejects the prompt. **Turn notes fail open** (notes added
  before the failure are kept).
- **Events never block the agent.** `emit` only queues; listeners see events
  in the agent's order, one dispatch after another, so a slow listener builds
  an (unbounded) backlog but does not delay the run.
- **One bridge per rutis root**: every attached agent shares its plugins.

## License

MIT
