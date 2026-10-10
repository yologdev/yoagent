# yoagent-rutis

Extend [yoagent](https://crates.io/crates/yoagent) agents at runtime with
[rutis](https://crates.io/crates/rutis) plugins — in Rust, TypeScript or
Python — through one yoagent `Extension`.

**Status:** on [crates.io](https://crates.io/crates/yoagent-rutis); needs yoagent's `Extension` (0.25). The DSH and pi adapters below cover tools, tool policies, input checks, prompt additions and images; commands, dialogs and UI belong to the app and are not mapped.

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
It depends on `rutis = "0.8"` (0.x caret: any 0.8.x, never 0.9) and, with a
language feature, `rutis-bridge = "0.8"`: since 0.8 the packages in rutis's
release train share one version. rutis types are part of this crate's API,
so every rutis or rutis-bridge minor bump is a yoagent-rutis minor bump.

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
| `.required()` | a failing `tools`, `before_model`, `after_tool`, `on_stop` or `on_event` fails the run (default: logged, the handler skipped). A `tools` / `on_event` failure fails it at the next decision point (tool calls are denied meanwhile); yoagent takes it at its next boundary, the run's end included (only a failure on `AgentEnd` itself is just logged). A handler whose plugin unloads mid-run never fails the run: an unloaded `after_tool` still withholds the result |
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
[rutis-bridge](https://crates.io/crates/rutis-bridge) 0.8, as the host
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
- **Every hook gets a cancel handle: `signal`.** Each hook but `on_event`
  finds it in its first argument (`call.signal`, `turn.signal`, ...): a real
  `AbortSignal` in JavaScript, rutis's `Signal` in Python
  (`signal.cancelled`, `await signal.wait()`). It is aborted when the bridge
  stops waiting — the run was cancelled (`Agent::abort()`), the hook's
  timeout passed, its plugin unloaded mid-call — and never once the call
  completed. The answer of an abandoned call is discarded, but a JavaScript
  function that ignores its signal runs to completion, side effects and all:
  **pass `call.signal` on** (`fetch(url, { signal })`, a child process, a dsh
  tool's `execute`) or check `signal.aborted` before acting. A Python
  coroutine is also cancelled (`asyncio.CancelledError` at its next
  `await`). The handle is a field rather than an extra positional argument
  so that a Python method with a fixed signature still accepts the call;
  it is the one value that is not plain data (`json.dumps(call)` refuses
  it — drop `signal` first).

  ```ts
  async call_tool(call) {
    const res = await fetch(call.args.url, { signal: call.signal })
    return await res.text()
  }
  ```
- **Images, both ways.** A tool result and an `after_tool` edit carry
  `content` blocks in yoagent's JSON shape (pi's and MCP's too) instead of
  `text` — `{"type": "text", "text"}` and
  `{"type": "image", "data": <base64>, "mimeType": "image/…"}` — and
  `after_tool` sees the output's blocks as `output.content`, so a handler
  can return pictures, and keep, add or drop them when it edits a result.
  Text and blocks are exclusive in one answer (so return picked fields,
  not the `output` you were given). An image must be standard base64 of at
  most 10 MB, typed `image/png`, `image/jpeg`, `image/gif` or `image/webp`
  (what every provider takes); anything else in `content` fails the answer
  — except, in an `after_tool` edit, an image identical to one in
  `output.content`: keeping what yoagent let in (`read_file` takes bmp, up
  to 20 MB) never fails the edit.
  Stay under your provider's own limit too (Anthropic: 5 MB base64): an
  image it refuses sits in the history and fails every later request.

  ```ts
  async call_tool(call) {
    return { content: [{ type: 'text', text: 'the chart' }, { type: 'image', data: png.toString('base64'), mimeType: 'image/png' }] }
  }
  ```
- **Logs reach the host.** `yoagent.log(level, message, { run_id }?)`
  (`error`, `warn`, `info`, `debug`; over 8192 characters cut) writes to
  the host's `tracing` output under the target `yoagent_rutis::plugin`,
  where a terminal or service host shows it — a runtime process's own
  stderr may go nowhere. Pass `{ run_id }`, or the hook argument itself, to
  attribute the line to its run (the event's `run_id` field) — a host
  recording runs with yoagent's GASP recorder can then keep the line in the
  run's record (`GaspRecorder::plugin_log_layer`). It never
  rejects (any message, extra context fields ignored). Fire-and-forget in
  JavaScript; in Python `await yoagent.log(...)` — an un-awaited coroutine
  is never sent. On a host without it (yoagent-rutis 0.1.0) rutis's
  stand-in throws (Python: `AttributeError`): wrap the call and fall back
  to `console.warn` / `print`.
- **The bridge never loads plugins**: the host does, typically with
  [rutis-loader](https://crates.io/crates/rutis-loader) rows, and must share
  `yoagent` in the loader's catalog (`catalog.register_shared("yoagent")` or
  `share_by_name()`). [`examples/language_plugins.rs`](examples/language_plugins.rs)
  is a complete host: `npm ci` in `plugins/`, a Python 3.12+ with
  `rutis==0.8.0` in `plugins/.venv`, then
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

## Tools from other agent ecosystems

### dsh (DeepSeek Harness) tool plugins

dsh plugins are Cordis plugins, and rutis's Node runtime runs them
unchanged. [`plugins/dsh/dsh-tools-adapter.ts`](plugins/dsh/dsh-tools-adapter.ts)
offers every tool in dsh's tool registry (the `tools` service of
`@deepseek-ai/dsh-tools`) to yoagent agents:

| Hook | What the adapter does |
|---|---|
| `tools` | `tools.schemas()` → name, description, parameters (config `tools`: an allowlist) |
| `call_tool` | `tools.execute({callId, name, arguments, signal})` with the bridge's cancel handle as dsh's `signal`: cancelling the run aborts the dsh call. `isError` → an error tool result. Text blocks stay text; an image block — a reference into dsh's attachment store — becomes a yoagent image, its bytes read with the `attachments` service when one is loaded (looked up per image, so the adapter also runs without it). Without one, when a read fails, when dsh marked the image `offloaded`, or over 3.75 MB (under Anthropic's 5 MB once base64), the image is a text placeholder; an error result's images are not read; other blocks are named. The tool's own presenters (`presentCall` / `presentResult`, dsh's card vocabulary: generic, terminal, diff, search, read, web) go along with a successful result as `details.view = {call?, result?}`, for a frontend to draw (content blocks as text, other blocks as `[type block]`; over 100k JSON characters, or a presenter that throws, leaves the card out; an error result has none) |
| `before_model` | the system-prompt sections dsh plugins added (the harness identity and persona slots left out, sections whose variables are unset skipped), as one note, capped at `maxNoteChars` (2000) |

Load, as rows of one Node runtime whose `package.json` is `plugins/dsh/`'s:
`@deepseek-ai/dsh-system-prompt`, `@deepseek-ai/dsh-tools`, your dsh tool
plugins (and what they need, e.g. `@deepseek-ai/dsh-web`), then the adapter;
share `yoagent` in the loader's catalog. `plugins/dsh/package.json` pins
everything exactly (dsh 0.2.0-rc.2 is a release candidate) and is its own
install, so the language tests' stays small.

```sh
(cd plugins/dsh && npm ci)
cargo run --features node --example dsh_tools            # scripted model, real web search: needs network
cargo run --features node --example dsh_tools -- --live  # DeepSeek: DEEPSEEK_API_KEY
```

[`examples/dsh_tools.rs`](examples/dsh_tools.rs) loads `dsh-web`,
`dsh-system-prompt`, `dsh-tools`, the unchanged `dsh-free-search` plugin and
the adapter, runs one search, and checks that a dsh tool answered (and,
scripted, that free-search's prompt section reached the model).
`tests/dsh_test.rs` covers the adapter offline with a fixture dsh plugin
(`plugins/dsh/fixture-tools.ts`) and, for images, a stand-in attachment
store (`plugins/dsh/fixture-attachments.ts`).

**Asking the user.** On its own, a dsh tool call the adapter makes cannot
reach a person: an `ask` from a dsh tool policy is denied, and
`ask_user_question` (`@deepseek-ai/dsh-tool-ask-user`) finds no answerer.
With a host that provides a `ui` service (yoagent-frontend does), load
[`plugins/dsh/host-dialogs.ts`](plugins/dsh/host-dialogs.ts) in the same
runtime **and share `ui` in the loader's catalog** (yoagent-frontend's
`PluginHost`: `.share(services::UI)`). It is a plain Cordis plugin that
injects `ui` (and `yoagent`, for the host's log), so without a shared `ui`
it never starts:

- **approval** — an outermost `tools/pre-execute` listener, for the
  adapter's calls only: when dsh's guards together decide `ask`, the user
  confirms, seeing the reason and the arguments (in full up to 4000
  characters; past that, the head and the end). Yes allows the call. No, or
  no answer in time, denies it ("not approved"); a cancelled call is
  cancelled; a host that fails while asking denies it ("asking the user
  failed"). dsh's own approval service is not used: it needs a dsh agent and
  session, which these calls do not have (so no dsh audit entry).
- **questions** — an answerer on `user-questions/request` for requests
  without an agent, which is what `ask_user_question` sends from these
  calls. Each question becomes a select (its distinct option labels, plus
  "Other" — which then asks for typed text — unless an option already has
  that label), a multiple select, or a text input. A blank input or an empty
  multiple choice is a skip (`selected: []`); a question left unanswered
  (dismissed, timed out, no frontend) fails the request with that reason,
  so the model never sees a made-up answer. Load
  `@deepseek-ai/dsh-user-questions` and `@deepseek-ai/dsh-tool-ask-user` for
  the tool itself (its default `legacy` mode: the `timed` one needs a dsh
  agent).

With no frontend attached when a call starts (`ui.frontends()` is 0), both
step aside, so the result is dsh's own. A question whose call is cancelled
is withdrawn.

### rutis-agent tools

[rutis-agent](https://github.com/arcships/rutis/tree/v0.8.0/crates/rutis-agent)
keeps its tools in a `ToolRegistry` service.
[`examples/rutis-agent-tools/`](examples/rutis-agent-tools/src/main.rs) is a
Rust rutis plugin that maps every `ToolDef` to a yoagent tool per run
(results as text, failures as `ToolError`s, yoagent's cancel token passed as
rutis-agent's), shown with rutis-agent's `replace_text` and a tool registered
into the registry while the host runs, which the next run offers.
rutis-agent's results are text (a runner's JSON value is serialized), so
images use a convention of this adapter: a runner returning
`{"content": [blocks]}` in yoagent's block shape, with at least one image,
gives yoagent those blocks (`dot_picture` in the example); rutis-agent's
own agent still sees the JSON text. A tool that prints exactly such JSON
(an image included) as its text would be read as blocks too.

```sh
cargo run --manifest-path examples/rutis-agent-tools/Cargo.toml [-- --live]
```

> **Pinning caveat.** rutis matches services by Rust type, so rutis-agent and
> this bridge must share one `rutis` crate. crates.io's `rutis-agent` 0.2.0 is
> built on rutis 0.2; the one on rutis 0.8 (the rutis repository at tag
> v0.8.0) is unpublished. The example is therefore its own workspace: it takes
> rutis-agent from git at that tag and patches crates.io's `rutis` to the same
> tag (`[patch.crates-io]`), leaving one `rutis` in the graph (`cargo tree -d`
> shows none twice). Move the tag and the patch together.

### pi extensions (tools and tool policies)

> **Scope.** The adapter maps what an agent loop can honour: tools, tool
> policies, input checks, prompt additions and images. Commands, dialogs,
> UI and session history belong to the app hosting the agent and are not
> mapped (reported, or refused when they would decide something). It is
> pinned to pi 1.1.0 and tested weekly against the latest pi.

[pi](https://github.com/earendil-works/pi) extensions are TypeScript
modules written against pi's `ExtensionAPI`.
[`plugins/pi/pi-extensions-adapter.ts`](plugins/pi/pi-extensions-adapter.ts)
loads them unchanged with pi's own loader (pi's packages are installed in
`plugins/pi/`, so their imports — TypeBox, `defineTool`, pi's helpers — are
the real ones) and maps the part of the API that belongs to an agent loop
onto one handler, following pi 1.1.0's own runner and agent loop:

| pi | yoagent |
|---|---|
| `pi.registerTool` | a tool, while pi would activate it (exposure `direct` / `model-only`, `defaultActive` not false, in the `setActiveTools` allowlist if one was set; the first registration of a name wins). Arguments go through `prepareArguments` and pi's validation before any policy sees them, and `execute` runs only with the arguments the policies left — a call another handler rewrote afterwards is not run. A throw or `isError` is an error result; text and image blocks pass through (pi's shape is yoagent's) |
| `ctx.executeTool` | another pi tool, the way pi runs a nested call: prepared, validated, judged by `tool_call` (with `parentToolCallId`), its result through `tool_result`; never rejects (an unknown tool, a block or a throw is `isError`). yoagent's own tools are not reachable from it; `ctx.tools` lists the callable ones |
| `pi.setActiveTools` | an allowlist over the tools pi knows — yoagent's built-ins under pi's names and the extensions' tools: those outside it are not offered and their calls are denied. Tools pi does not know (MCP tools, sub-agents, the host's own) are not affected, as in pi. As in pi, a tool named in it is activated even when `defaultActive` is false (not when hidden), and a tool registered later that pi would activate joins it. `getActiveTools` / `getAllTools` answer from the same view |
| `on("tool_call")` | `before_tool` for every call. yoagent's built-ins are judged under pi's names (`bash`, `read_file`→`read`, `write_file`→`write`, `edit_file`→`edit` with `edits`, `search`→`grep` with `glob` / `ignoreCase` (yoagent's search is case-insensitive unless asked, pi's grep the reverse), `list_files`→`find`; config `toolNames` adds more), with a relative `path` resolved against `cwd` first so the tool acts where the policy looked (yoagent's built-ins only: another tool's `path` may be a repository's). `{ block }` denies (`terminate: true` also stops the run at its next model request — stricter than pi, which ends only a batch whose results all set it); in-place changes to `event.input` rewrite the arguments, and one the yoagent tool cannot take (a second edit, a `timeout` on `bash`) denies the call; a throwing handler blocks, as in pi |
| `on("tool_result")` | `after_tool`: content, details and isError edits, chained; only the fields a handler set are applied (a replaced content keeps its text and image blocks). A throwing handler **withholds the result** — unlike pi, which skips it: a failed redaction must not let the raw output through |
| `on("input")` | `on_input`: `handled` rejects the prompt (it never reaches the agent, as in pi); `transform` and a throwing handler reject it too (yoagent cannot rewrite a prompt) |
| `on("before_agent_start")` | `before_model`: the handlers run once per run, and the text they add around `event.systemPrompt` is a note on the latest user turn of every request of that run. A handler that throws, replaces the prompt or changes `systemPromptOptions` is skipped with a warning — even under `strict` (a run cannot refuse the load), so a policy that replaces the prompt (a "read-only mode") does not apply; the others still count. A returned `message` is dropped, the same handler's addition kept |
| `on("session_start")` / `on("session_shutdown")` | when the adapter loads / unloads; a `session_start` handler that throws refuses the load (its extension may be missing its policies) |

**What refuses the load** (fail closed, `strict` or not): an extension that
fails to load (nothing loads, not just the bad one); a `session_start`
failure; a handler for an event yoagent never fires that would decide or
rewrite what the agent does (`context`, `context_with_system`,
`message_end`, `before_provider_request`, `before_provider_headers`) unless
the host lists it in `allowUnmapped`; and a pi tool named exactly like a
yoagent built-in (pi's sandboxed `bash`) unless the host left that built-in
out and says so in `withoutBuiltins` — otherwise yoagent's own tool would
win the merge and run unsandboxed. Anything of this kind registered after
load (inside a handler or a tool) is caught at the next model request or
tool call and stops the adapter instead: that run stopped, every later call
denied, prompt rejected.

**Overrides.** An extension tool that replaces one of pi's built-ins under
another name than yoagent's (`read`, `write`, `edit`, `grep`, `find`) makes
the adapter deny yoagent's counterpart (`read_file`, ...), so the model
cannot go around it.

**What is only reported** (a warning in the host's `tracing` logs, through the bridge's `log`; config
`strict: true` makes it a load failure): every other unfired event —
observers such as `agent_end` or `tool_execution_*`, pi's session events,
the boundary events `turn_end` / `agent_before_settle` — commands,
shortcuts, flags, renderers, model providers, virtual models and MCP
servers. `pi.appendEntry` and `pi.sendMessage` are recorded in the
adapter's in-memory session — readable through `ctx.sessionManager`, a
displayed message also logged — so a tool that records or shows its result
this way (pi video tools do, after the paid work) does not fail; a message
never starts a turn or reaches the model (yoagent's history is written only
by its host). The session lives as long as the adapter: in a long-running
host it grows until the plugin reloads. Other runtime actions (`pi.sendUserMessage`, `pi.setModel`,
...) throw "not available in yoagent". There is no UI: `ctx.hasUI` is false and
`ctx.ui` behaves as in pi's print mode (`confirm` answers false), so a
policy that would ask the user denies. Commands, dialogs and session
history are planned as host-level plugin services — `ui` and `commands`
provided by the attached client, `session` by the host that owns the
session — which the adapter would route `ctx.ui.*`, `registerCommand` and
the session calls to; without them it stays in print mode (design: yo's
`docs/WEB-UI-DESIGN.md` §7, [yoyo-meme/yo#3](https://github.com/yoyo-meme/yo/pull/3); not built).

**Host setup.** Install the bridge's extension with `.require_policy()`, so
a run that starts before the adapter registered (or after it failed to
load) has every tool call denied rather than unjudged; and with
`.rechecks_modified_calls()` when other handlers can rewrite calls after
the adapter (pi tools are protected by the adapter itself; yoagent's
built-ins are not). Policies cover what the agent calls under the names pi
knows: a pi policy for `write` does not see a `bash` command that writes
the same file (as in pi), nor tools outside `TOOL_NAMES` / `toolNames`
(MCP tools, sub-agents, your own) under any but their own names.

Config: `extensions` (files, or directories with an `index.ts` /
`index.js`, loaded in order; relative paths resolve against `cwd`), `cwd`
(the project the extensions see; default the runtime's), `name`,
`toolNames`, `withoutBuiltins`, `allowUnmapped`, `strict`. Load it as a
row of a Node runtime whose `package.json` is `plugins/pi/`'s (pi 1.1.0,
pinned exactly: the adapter imports pi's loader by file, since the package
exports only `discoverAndLoadExtensions`, which also loads
`<cwd>/.pi/extensions` and `~/.pi/agent/extensions`; check it on every pi
upgrade), with `yoagent` shared in the loader's catalog.

```sh
(cd plugins/pi && npm ci)
cargo run --features node --example pi_extensions                          # the fixture extension, scripted
cargo run --features node --example pi_extensions -- hello.ts todo.ts ...  # your pi extensions
cargo run --features node --example pi_extensions -- --live --prompt "..." [--without bash] EXT.ts ...  # DeepSeek
```

The example runs yoagent's own tools in the temporary project the
extensions see (it is the process's working directory).

Tried (pi 1.1.0, October 2026) with eleven of pi's own examples, unchanged,
scripted and live with DeepSeek: `hello`, `todo`, `tool-override` (`read`;
yoagent's `read_file` is then denied), `truncated-tool` (`rg`) and
`dynamic-tools` offer working tools; `protected-paths` and
`permission-gate` judge yoagent's own `write_file` and `bash`;
`claude-rules` loads (its note needs a project with `.claude/rules/`, which
these runs did not have). `pirate` only acts after its `/pirate` command,
and `dirty-repo-guard` and `confirm-destructive` only on pi's session
events, so under the adapter they load and do nothing (reported).
`tests/pi_test.rs` covers the adapter offline with the fixture extensions
(`plugins/pi/fixture-extension.ts`, `plugins/pi/fixture-extra.ts`) and small
ones written per test.
## Writing an ecosystem adapter

An adapter makes another plugin system's plugins (DSH's, rutis-agent's,
pi's) into handlers. Their APIs rarely map one-to-one — an ecosystem has
commands, dialogs, sessions, model routing — so this is the contract an
adapter should follow, learned from the DSH, rutis-agent and pi adapters
above.

Why adapters at all: as of 2026-10-09, npm lists about 11.6k packages
tagged `pi-package` and about 7.3k tagged `dsh-plugin`. Those are tagged
packages, not plugins that work here: a pi package may be only skills,
prompts or a theme, and an extension that needs a UI only partly runs
through the adapter. (Counted with
`https://registry.npmjs.org/-/v1/search?text=keywords:<tag>`; result pages
sampled deep into each list all carried the tag, so the totals are not
loose text matches.)

The point is that **a plugin's safety policy never silently stops
applying**. Not every rule arises for every adapter: the DSH adapter maps
only tools and prompt sections, so it has nothing to refuse (2) or record
(6), and today relies on yoagent's own handling of a name clash (the host's
tool wins, with a warning) rather than refusing (4).

1. **Map only what the host can honour.** Tools, `before_tool` /
   `after_tool` policies, input checks, turn notes, verifiers and events
   have a yoagent hook; offer those.
2. **Refuse to load what would decide or rewrite unenforced.** A plugin
   handler for something the host never does — rewriting the conversation,
   rewriting provider requests — would be a policy that never runs. Fail
   the load (the host may accept it explicitly, by name), and when such a
   registration appears after load, stop: deny every call, reject input.
3. **A failing policy denies; a failing redaction withholds.** A thrown
   error, a timeout or a malformed answer in a `before_tool`-like hook is a
   denial; in an `after_tool`-like hook the result is withheld, never
   passed through raw. (The bridge already treats a handler's failures
   this way; an adapter must not catch them into an allow.)
4. **Name clashes refuse.** A plugin tool named like one of the host's
   tools would lose to it and leave its policy judging the wrong tool:
   refuse unless the host says it left its own out.
5. **Report what is ignored — through the host.** Commands, renderers,
   notifications and other app-level parts warn (or refuse with a strict
   option) through `yoagent.log`, never silently.
6. **Keep plugin state local, don't fail work.** A plugin that records its
   own state (a session entry, a result card) gets an adapter-local store
   rather than an error, so a tool does not fail after its work is done;
   what would inject input or start a turn still throws.
7. **No UI means "no".** With no one to ask, a dialog answers as a
   headless run does — a confirmation is declined — so a policy that would
   ask denies.

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

## Versions

yoagent-rutis 0.1 (crates.io) requires yoagent 0.25 (the first release with
`Extension`) and rutis 0.6. From 0.2 it requires rutis 0.8, whose release
train (`rutis`, `rutis-bridge`, `rutis-loader`, the npm and PyPI runtimes)
shares one version. yoagent and rutis types are in its API, so a minor bump of
either is a minor bump of yoagent-rutis. Take `yoagent` from the same source
as `yoagent-rutis`: a crates.io `yoagent` next to a git `yoagent-rutis` is two
`yoagent` crates whose types do not match.

```toml
yoagent = "0.25"
yoagent-rutis = "0.2"
```

## License

MIT
