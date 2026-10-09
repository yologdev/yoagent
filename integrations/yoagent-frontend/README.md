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
| `steer {text}` / `followUp {text}` | guidance for the run in progress |
| `abort`, `reset`, `quit` | stop the run, forget the conversation, end (terminal only) |
| `uiResponse {id, value}` | the answer to a `uiRequest` — the first answer wins |

| To every frontend, in order, none dropped | |
|---|---|
| `hello {running, uiPlugins}` | first message on a connection |
| `runStarted {run, prompt}` / `runEnded {run, stats, error}` | always paired, whatever happened to the events between |
| `event {run, event}` | yoagent's `AgentEvent`; text deltas merged (30 ms) |
| `uiRequest {id, request}` / `uiResolved {id}` | a plugin's question (`confirm`, `select`, `input`, `notify`); resolved = close the dialog |
| `uiPlugins {uiPlugins}`, `closed` | browser components changed; the session ended |

With no frontend attached, or no answer in time (5 minutes, or the plugin's
own timeout), a question gets the safe answer: `false`, no choice, no text.

## Pieces

- **`Session` / `Driver`** — one agent, any number of frontends. Each
  frontend gets its own unbounded channel, so `runEnded` is never lost behind
  streamed text.
- **`services::provide`** — rutis host services:
  `frontend.connect(client)` (`client.receive(message)` called in order),
  `frontend.send(message)`, `frontend.addUiPlugin(info, module)`, and
  `ui.request(request, timeoutMs?)`.
- **`web::serve`** — the browser frontend: the page, `/ws`, and
  `/ui-plugins/<name>.js`. No authentication: bind to localhost.
- **`host::PluginHost`** — Node runtimes, a loader and shared services in a
  few lines; each row names its runtime; `load` waits until plugins run.

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

See [`plugins/search-links.ts`](plugins/search-links.ts).

## pi dialogs

pi extensions' `ctx.ui.select`, `confirm`, `input` and `notify` reach the
user when the host provides `ui` and loads
[`yoagent-rutis/plugins/pi/host-ui.ts`](../yoagent-rutis/plugins/pi/host-ui.ts)
in the pi runtime (rutis shows a host service only to plugins that inject it,
and the adapter cannot require one). The adapter is then in pi's RPC mode
(`hasUI` true). Without them it stays in print mode, as before.

## Example

```bash
(cd plugins && npm ci) && (cd ../yoagent-rutis/plugins/pi && npm ci) && (cd ../yoagent-rutis/plugins/dsh && npm ci)
cargo run --example coding_agent                     # terminal frontend, scripted model
cargo run --example coding_agent -- --live           # DeepSeek (DEEPSEEK_API_KEY)
cargo run --example coding_agent -- --web --live     # browser at http://127.0.0.1:8787
cargo run --example coding_agent -- --demo "clean the build"   # headless (CI)
```

The agent: yoagent's tools, a pi extension asking before dangerous commands
(`plugins/pi-extensions/confirm-dangerous.ts`), DSH's web search, and the
`search-links` UI plugin. Tried live with DeepSeek in Chrome: DSH search,
results rendered as links by the UI plugin, pi's question answered in the page.

## Limits

- The terminal frontend reads the keyboard from `/dev/tty` (rutis starts Node
  with stdin closed): macOS / Linux.
- A TypeScript plugin reads as running before its async start-up finishes:
  wait for the handlers you depend on (the example does).
- pi's TUI-only UI (custom components, widgets) does not cross; DSH's web
  client UI plugins need a shim of DSH's client APIs (not built).
- Three Node runtimes in the example (the UI's, pi's and DSH's packages).
