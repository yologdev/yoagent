# The handoff agent example

`handoff_agent.rs` is a personal handoff agent. You give it a task (research, writing, planning or code) and walk away. It plans, works and asks you only what is yours to decide, without waiting for the answer. Every run ends with a report and the facts the host measured.

```bash
mkdir research && cd research && git init
DEEPSEEK_API_KEY=… path/to/yoagent/scripts/try-handoff.sh --live            # then open the printed URL
DEEPSEEK_API_KEY=… path/to/yoagent/scripts/try-handoff.sh --live --phone    # open it on your phone too (same Wi-Fi)
cargo run --example handoff_agent -- --demo                                 # scripted, no key, no browser (CI)
```

The other two examples here are smaller. `minimal_browser.rs` is one agent and the browser page, with no plugins. `coding_agent.rs` is the interactive showcase, with a terminal or browser frontend.

## The idea

Most agents assume you are watching them. A handoff agent assumes you are not:

- **It asks without waiting.** `ask_owner` puts a question to every frontend and returns at once, so the agent carries on with whatever does not depend on the answer.
  - The question stays open while nobody is connected, and a page opened later shows it.
  - The answer, whenever it comes, goes in as a steer message. It is picked up mid-run, or it starts the next run if the agent had stopped.
  - Dismissing a question means "decide yourself"; the agent does, and says so.
- **It cannot do damage while you are away.** Anything that would delete or overwrite what you already have asks first, and with nobody there it is refused:
  - `rm`, `git reset --hard`, `git clean`, forced pushes;
  - a write, `mv` or `>` over an existing file.

  Files it created itself, and its own scratch folder, stay free, so it can keep its deliverable up to date.
- **What it says is checked.**
  - Every run must end with a four-part report (Done, Waiting on you, Decisions, Next); the agent is sent back until it writes one.
  - The host then adds facts the model cannot make up: what changed since the handoff (against your working tree as it was), commits, new files, tokens, tool calls, time, and open questions.
- **You can look whenever you like.** A page opened late replays the runs so far. Questions are docked, not modal, so the page stays readable.

## The stack

One Rust process holds the agent. Three Node processes host the plugins, each a [rutis](https://github.com/arcships/rutis) runtime:

```
Rust process (21 MB)
├─ yoagent ──────────── the loop: model streaming, tool calls, limits, extensions
│    ├─ tools ───────── yoagent's built-ins (bash, files, search) + ask_owner (this example)
│    └─ extensions ──── Handoff (report check, facts), Budget (when the model is priced),
│                       the rutis bridge (every plugin below, as one extension)
├─ yoagent-rutis ────── the bridge: plugins' tools, policies, prompt notes → yoagent hooks
├─ yoagent-frontend ─── Session (one agent, many frontends), protocol, web server (axum + WebSocket)
└─ model ───────────── DeepSeek (deepseek-flash) over its OpenAI-compatible API

Node runtime "ui"  ─── todo-plan             draws the plan in the page
Node runtime "pi"  ─── pi adapter + host-ui  runs pi extensions unchanged
                       ├─ @juicesharp/rpiv-todo   the plan (`todo` tool)
                       └─ confirm-dangerous       the approval gate
Node runtime "dsh" ─── DSH adapter + host-dialogs, DSH's core packages
                       └─ dsh-free-search         web search (no API key)
```

The browser page is plain HTML and JavaScript, served by the Rust process. Any number of tabs and phones can share one session.

## Plugins, by ecosystem

| Plugin | Ecosystem | Version | Changed? | What the agent gets |
|---|---|---|---|---|
| [`@juicesharp/rpiv-todo`](https://www.npmjs.com/package/@juicesharp/rpiv-todo) | pi (npm, ~207k downloads a month) | 2.12.0 | unchanged | the `todo` tool, its plan |
| `plugins/pi-extensions/confirm-dangerous.ts` | pi (written here, in pi's extension format) | — | ours | the approval gate for destructive commands and overwrites |
| [`dsh-free-search`](https://www.npmjs.com/package/dsh-free-search) | DSH (npm) | 0.8.1 | unchanged | 9 search tools (`multi_search`, `research`, …) and prompt guidance |
| `@deepseek-ai/dsh-web`, `dsh-system-prompt`, `dsh-tools` | DSH core | 0.2.0-rc.2 | unchanged | the services DSH plugins expect |
| `plugins/todo-plan.ts` | rutis (yoagent-frontend UI plugin) | — | ours | the plan drawn as a checklist in the page |
| pi adapter, `host-ui.ts`, DSH adapter, `host-dialogs.ts` | rutis (yoagent-rutis) | — | ours | pi and DSH plugins as yoagent tools, policies and dialogs |
| `ask_owner`, `Handoff` | yoagent (Rust, in the example) | — | ours | asking without waiting; the report check and the facts |

Those from pi and DSH are installed from npm and loaded as published. The adapters translate:
- pi's `tool_call` handlers become yoagent's `before_tool`;
- DSH's tool registry becomes yoagent tools;
- pi's and DSH's dialogs reach the page.

What does not map (pi's `/todos` command, a keyboard shortcut) is reported at info level and left out. A plugin hooking something yoagent cannot enforce is refused, not half-run. See [plugin compatibility](../../../docs/evals/plugin-compatibility.md) for how the 20 most-downloaded pi and DSH packages fare.

## How a tool call travels

1. The model asks for `write_file` on `notes.md`.
2. yoagent's loop calls `before_tool` on every extension. The rutis bridge forwards it to the pi adapter in its Node runtime.
3. The adapter runs pi's `tool_call` handlers. `confirm-dangerous` sees a new file, which it allows; for an existing file it would ask you first.
4. The loop runs the tool in Rust. The result goes back through `after_tool`, then to the page, then to the next model request.

A pi tool such as `todo` makes the same trip, and then runs in the pi runtime.

## Performance

Measured on 2026-10-11 on an Apple M2 (16 GB) with macOS 15.5, Node 24.7, a release build (`cargo build --release`) and DeepSeek `deepseek-flash`. The example prints these itself: `plugins ready in …` at start, and the time of each tool call in `--demo`.

**Startup**, until the plugins are ready (5 warm runs):

| | Startup |
|---|---|
| 3 Node runtimes (ui, pi, dsh) | 1.7–2.3 s |
| 2 Node runtimes (`--no-dsh`) | 1.5–1.7 s |
| First run after boot | ~9 s (macOS reading Node's modules from disk) |

**A tool call, as the loop runs it** (scripted model; 5 runs, medians). Times include the trip to the pi adapter's policy in Node and back:

| Call | Path | Time |
|---|---|---|
| `write_file` | Rust tool, judged by the pi policy | 0.7 ms |
| `todo` (repeat calls) | pi tool: policy + run in the pi runtime | 1.3–1.5 ms |
| `ask_owner` | Rust tool, judged by the pi policy | 2.0–2.5 ms |
| `todo` (first call) | the same, first time | 5–6 ms |
| `list_files` | Rust tool that spawns `find` | ~10 ms |

**A live research run.** The task: "the three most-recommended noise-cancelling headphones under $300, written to picks.md":

| | Time |
|---|---|
| Whole run | 30.5 s |
| The model (6 requests, streaming) | 15.7 s |
| Web search tools (5 calls) | 2.7–6.7 s each |
| The agent's own tool calls (`todo`, `write_file`) | 1.7–3.6 ms each |

Tools ran in parallel and overlap the model's time, so subtracting leaves nothing to attribute to the loop at this resolution. Its cost is the few milliseconds per call measured above, against seconds per model request and per web search. For the ten calls in this run that is tens of milliseconds, about 0.1% of the 30.5 s.

**Memory** during that run:

| Process | Resident memory |
|---|---|
| The Rust process (loop, bridge, session, web server) | 21 MB |
| Each Node runtime | 137–244 MB |

The Node runtimes are the cost of running other ecosystems' plugins unchanged. An agent with Rust tools only is the 21 MB process. yoagent's loop alone also runs in a Cloudflare Worker: about 4 ms to start, and a median of about 45 ms of CPU per agent run there (see the main README).

**Cost.** DeepSeek's off-peak price is $0.15 per million input tokens on a cache miss and $0.003 on a hit. Peak hours cost double. A run like the one above (a few hundred thousand tokens in, mostly cache hits) costs cents. The example caps each run at 4M tokens (`--max-tokens`), 30 minutes and 80 turns. `--budget-usd` only applies to models in yoagent's price table, which DeepSeek is not yet.

## Perspective

- **A small loop, big ecosystems.** The whole example is about 850 lines of Rust on top of yoagent's loop (a quarter of it is the scripted demo). The plan, the web search and the dialogs come from plugins that thousands of people already use. The adapters let them run unchanged instead of being rewritten.
- **Safety by contract, not by trust.**
  - An adapter that cannot enforce a plugin's hook refuses it.
  - A policy that fails denies the call.
  - The approval gate refuses when nobody is there.
  - The facts come from git and the event stream, not from the model.
- **Walk-away first.** Questions that wait, a page that replays, answers that start the next run, a report on every run. These are what make "hand it off" work. Each is a small, tested piece of yoagent-frontend (`Session::ask_later`, history replay, late steering), not special code in the agent.
- **Not done yet.**
  - The session lives in memory, so a restart starts fresh; your files and git history are the durable record.
  - There are no push notifications.
  - Most of the runtime memory is Node's. Fewer runtimes (`--no-dsh`), or plugins written in Rust, shrink it.
