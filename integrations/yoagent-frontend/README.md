# yoagent-frontend (experimental)

One frontend protocol for yoagent agents: a terminal UI and any number of
browsers can drive the same agent session, plugins can ask the user things,
and plugins can add their own UI to the browser.

> **Experimental.** Not published (`publish = false`), no API promise.
> yoagent's core and yoagent-rutis's API do not depend on it.

```
 terminal UI plugin ─┐                       ┌─ pi adapter  (ctx.ui dialogs → `ui`)
 (pi-tui, rutis)     ├─ `frontend` service ─┐ │
 browser (WebSocket) ┘                      ├─ Session ── yoagent loop
 UI plugins (ES modules) ── addUiPlugin ────┘ │
                                              └─ `ui` service ← any plugin asks the user
```

## The protocol

JSON, tagged by `type`, camelCase — the same over a rutis call and a WebSocket.

| From a frontend | |
|---|---|
| `prompt {text}` | start a run (queued while one runs) |
| `steer {text}` / `followUp {text}` | guidance for the run in progress; when idle, start a run |
| `abort` | stop the run in progress |
| `reset` | stop any run, drop queued prompts, forget the conversation |
| `quit` | end the session (a host may ignore it; the example's browser mode does) |
| `uiResponse {id, value}` | the answer to a `uiRequest` — the first one that fits wins (`confirm`: a boolean; `select`: one of the options or `null`, with `multiple` a list of distinct options; `input`: a string or `null`) |

| To every frontend, in order, none dropped | |
|---|---|
| `hello {running, uiPlugins, uiRequests}` | first message on a connection; open questions included, so a reloaded page can still answer them |
| `runStart {run, prompt}` / `runEnd {run, outcome, error, stats, totalCostUsd}` | always paired, whatever happened between — an agent task or the session's driver failing included; `outcome` is `completed`, `aborted`, `rejected` or `error` |
| `event {run, event}` | yoagent's `AgentEvent`; text and thinking deltas merged, held at most 30 ms |
| `uiRequest {id, request}` / `uiResolved {id, reason}` | a plugin's question (`confirm`, `select` — `multiple` for several choices; options sent once each —, `input` — with an editor's prefill in `value` — or `notify`; each may carry a `message` shown with its title); resolved = close the dialog, `reason` `answered`, `timedOut` or `withdrawn` |
| `notice {level, message}` | something outside a run (an agent task that failed, a message the session did not understand) |
| `uiPlugins {uiPlugins}`, `closed` | browser components changed; the session ended (nothing sent after it is heard) |

The enums grow: a frontend skips a `type` (or a question `kind`) it does
not know.

With no frontend attached, or no answer in time (5 minutes, or the plugin's
own timeout), a question gets the safe answer: `false`, no choice, no text.
Frontends show questions one at a time, oldest first. A question whose asker
gives up (the run stopped) is withdrawn and its dialog closes: dropped
in-process, or `ui.withdraw(key)` from a plugin (rutis 0.7 cannot cancel a
plugin's call to the host). Keys must be unique (a UUID); a withdrawal that
overtakes its own question still counts.

## Pieces

- **`Session` / `Driver`** — one agent, any number of frontends. Each
  frontend gets its own unbounded channel, so `runEnd` is never lost behind
  streamed text. `Session::send` returns `false` once the session ended.
- **`services::provide`** — rutis host services:
  `frontend.connect(client)` (`client.receive(message)` called in order; a
  `receive` that throws disconnects that frontend), `frontend.send(message)`
  (rejects once the session ended), `frontend.addUiPlugin(info, module)`
  (its disposer removes that offer only), `ui.request(request, timeoutMs?)`
  (a `key` field makes it withdrawable), `ui.withdraw(key)` and
  `ui.frontends()` (how many are attached, for an asker with a fallback of
  its own).
- **`web::serve`** — the browser frontend: the page, `/ws`, and
  `/ui-plugins/<name>.js`; serving stops when the returned `Served` is
  stopped or dropped. **`/ws` needs the random token** in the URL it
  prints (`/?t=…`): browsers do not apply cross-origin rules to WebSockets,
  so without it any page you have open could drive an agent that has `bash`.
  Share the URL only with whoever may drive the agent.
- **`host::PluginHost`** — Node runtimes, a loader and shared services in a
  few lines; each row names its runtime (routing is by row name: two rows
  loading the same name must name the same runtime); `load` waits until
  plugins run, and a batch that fails is dropped again. While the agent
  runs, `unload(id)` removes a plugin (its tools, handlers, services and UI
  plugin offers go with it) and `reload(id)` restarts one with its edited
  file — all or nothing: new code that does not load is refused and the
  running version stays. A run in progress keeps the tools it started with;
  the next one sees the change. No rebuild of the host.

## UI plugins

A rutis plugin offers the browser an ES module:

```ts
frontend.addUiPlugin({ name: 'search-links', tools: ['advanced_search'], panel: true }, MODULE)
```

```js
// MODULE — loaded by the page from /ui-plugins/search-links.js
export function renderTool({ toolName, args, result, isError }, element) { /* draw under the tool line */ }
export function mountPanel(element, { send }) { /* a side panel; send protocol messages */ }
```

See [`plugins/search-links.ts`](plugins/search-links.ts). **UI plugins are
trusted code:** a module runs in the page with its privileges — it can send
any protocol message, answers to questions included. Load only plugins you
would let drive the agent.

## Tool cards

A successful tool result whose `details` carries `view = {call?, result?}`
is drawn as a card in the browser, unless a UI plugin draws that tool: the
vocabulary is DSH's (`generic`, `terminal`, `diff`, `search`, `read`, `web`;
see `@deepseek-ai/dsh-tools/presentation`), so DSH tools that present
themselves show up right, and any other tool can use it. The tool line takes
the view's title — in the browser only when no UI plugin draws the tool,
always in the terminal UI. Links in a card are http(s) only; each block shows
at most 200 lines, a web card at most 20 sources. Error results carry no
card (the bridge reports them by their text).

## pi dialogs

pi extensions' `ctx.ui.select`, `confirm`, `input`, `editor` and `notify` reach the
user when the host provides `ui` and loads
[`yoagent-rutis/plugins/pi/host-ui.ts`](../yoagent-rutis/plugins/pi/host-ui.ts)
in the pi runtime (rutis shows a host service only to plugins that inject it,
and the adapter cannot require one). The adapter is then in pi's RPC mode
(`hasUI` true). Without them it stays in print mode, as before. A question
asked from a tool policy waits as long as the policy hook may run:
yoagent-rutis's default is 60 s, so the example raises it
(`with_policy_timeout`) to the question timeout.

## DSH dialogs

An approval a DSH tool policy asks for, and DSH's `ask_user_question`, reach
the user when
[`yoagent-rutis/plugins/dsh/host-dialogs.ts`](../yoagent-rutis/plugins/dsh/host-dialogs.ts)
is loaded in the DSH runtime with `ui` shared into it (`.share(services::UI)`;
`@deepseek-ai/dsh-user-questions` and `@deepseek-ai/dsh-tool-ask-user` for
the tool). An approval is a `confirm` showing the reason and the arguments:
no, or no answer in time, denies the call. A question is a `select` (plus
"Other" for typed text), a `select` with `multiple`, or an `input`; one left
unanswered fails the tool with that reason. Without a frontend attached,
DSH answers as on its own (an `ask` is denied, the tool finds no answerer).

## Example

```bash
(cd plugins && npm ci) && (cd ../yoagent-rutis/plugins/pi && npm ci) && (cd ../yoagent-rutis/plugins/dsh && npm ci)
cargo run --example coding_agent                     # terminal frontend, scripted model
cargo run --example coding_agent -- --live           # DeepSeek (DEEPSEEK_API_KEY)
cargo run --example coding_agent -- --web --live     # browser: open the printed http://127.0.0.1:8787/?t=… URL
cargo run --example coding_agent -- --demo "clean the build"   # headless (CI)
cargo run --example coding_agent -- --web --watch    # edit a plugin file: it reloads, the page is told
```

The agent: yoagent's tools, a pi extension asking before dangerous commands
(`plugins/pi-extensions/confirm-dangerous.ts`), DSH's web search, and the
`search-links` UI plugin. DSH's `ask_user_question` is loaded too. Tried live with DeepSeek in Chrome: DSH search,
results rendered as links by the UI plugin, pi's question answered in the page.

Logs (`RUST_LOG`) go to stderr with `--web` / `--demo`, else to
`$TMPDIR/yoagent-coding-agent.log`, since the terminal UI owns the screen.

## Tests

```bash
cargo test                                            # protocol, session, web, services
YOAGENT_RUTIS_REQUIRE_RUNTIMES=1 cargo test           # pi's and DSH's dialogs need their packages (CI)
node --test web/lib.test.js                           # Markdown, question queue, run lines, tool views, diffs, links, typed answers
```

## Limits

- The terminal frontend reads the keyboard from `/dev/tty` (rutis starts Node
  with stdin closed): macOS / Linux.
- A TypeScript plugin reads as running before its async start-up finishes:
  wait for the handlers you depend on (the example does).
- pi's dialogs that cross: `select`, `confirm`, `input`, `editor` (an input
  with a prefill) and `notify`. pi's TUI-only UI (custom components, widgets) does not cross. DSH's
  React web client (its `dsh-client-ui-*` plugins) runs only on DSH's own
  Host; what crosses from DSH is its approvals, `ask_user_question` and tool
  cards.
- Three Node runtimes in the example (the UI's, pi's and DSH's packages).
- `reload` re-imports the plugin's own file, not the modules it imports (an
  edit there needs its runtime restarted); a pi extension is the exception,
  since pi loads extensions afresh when the adapter starts. A `.ts` plugin
  must sit in a `"type": "module"` package (tsx loads it as CommonJS
  otherwise, and that cache keeps the old code). After a reload, as after
  `load`, wait for the handlers you depend on before the next run.
