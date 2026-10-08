# Design Philosophy

Status: current. Date: 2026-10-09.

**yoagent is a loop library: the smallest correct agent loop, and one contract to extend it.** It is not an agent, not a framework and not an app. Agents, frameworks and apps are built on it.

This page explains that starting point, the shape it leads to, what it costs, and the principles that follow. The mechanisms themselves are in the concept pages and the [architecture overview](architecture/overview.md).

## 1. An agent is a loop; the loop is the hard part to get right

An agent sends messages to a model, gets text and tool calls back, runs the tools, and repeats until the model stops. Everyone can write that loop in an afternoon. Almost everyone gets the same things wrong in it:

- **Streams that end badly.** A provider stream that drops mid-answer, a capacity error disguised as a context overflow, a retried attempt whose partial text already reached the user.
- **Tools that misbehave.** A tool that panics, hangs, floods its output, or escapes its sandbox.
- **Context that runs out.** History that outgrows the window, compaction that destroys the cached prefix, a summary that loses the one fact that mattered.
- **Runs that never end, or end silently.** Loops that repeat a failing call forever, limits that are checked once, a cancel that leaves a half-written state.
- **Cost nobody can account for.** Spend from retries, sub-agents, decision models and summaries that never shows up in the total.

yoagent exists so that this loop is written **once, correctly, with every guarantee pinned by a test** — and then reused by every agent built on it. That is why `agent_loop` is a stateless free function, why [`Agent`](concepts/agent-loop.md) is only an optional wrapper around it, and why most of the codebase is the loop's failure handling rather than its happy path.

## 2. A library, not an agent

yoagent deliberately stops at the loop. What a product needs beyond it belongs to the product:

| Belongs to yoagent | Belongs to the app built on it |
|---|---|
| streaming, tool execution, retries, cancellation | the UI: terminal, web, chat channels |
| context management, limits, loop detection | commands, settings, dialogs |
| cost accounting, structured outputs, telemetry spans | sessions as a product: listing, resuming, sharing |
| the provider protocols (seven) | which models a user may pick, and their keys |
| one extension contract | which plugins and ecosystems a user may install |

The line is drawn by one question: **does every agent need it, the same way?** Retry logic does; a command palette does not. When the answer is "only some agents", the capability is built as an extension or a companion crate, never in the loop.

The same restraint applies to providers. yoagent integrates model APIs — Anthropic, OpenAI's two protocols, Azure, Gemini, Vertex, Bedrock, and the OpenAI-compatible long tail — and does not host or wrap models. If a model has no API, yoagent does not support it.

## 3. One contract to extend the loop

```text
            app (yo, yoyo, your agent)
                     │  builds, configures, owns the UI
                     ▼
   ┌───────────── yoagent ──────────────┐
   │  the loop  ◀── Extension / RunHooks │  tools, on_input, before_model,
   │                                     │  before_tool, after_tool, on_stop,
   │                                     │  on_event, finish
   └───────────────────┬─────────────────┘
                       │  an Extension like any other
                       ▼
   yoagent-rutis (companion crate): a plugin host
   Rust · TypeScript · Python handlers, images, logs
                       │
        ┌──────────────┼──────────────┐
        ▼              ▼              ▼
       DSH        rutis-agent        pi        (ecosystem adapters)
```

Every way to change what the loop does goes through [`Extension`](concepts/extensions.md): offer tools, judge input, add a note before a model request, allow, deny or rewrite a tool call, edit a result, verify an answer, observe events, clean up. The contract has a few properties that matter more than its list of hooks:

- **It is run-scoped.** An extension is a factory; every run gets fresh hooks. A run's state cannot leak into the next one, and a sub-agent's run gets its own.
- **Failure has a meaning.** An extension is *advisory* (its failure is logged and the run goes on) or *required* (its failure ends the run). The host decides, not the extension.
- **The loop's own features use it.** The tool gate, the input guard, the decision advisor and the dollar [`Budget`](concepts/extensions.md#budget) are extensions. If the contract were not enough for them, it would not be enough for anyone.
- **It is not a UI.** A UI must exist before and between runs; an extension exists only inside one. That is why commands, dialogs and history stay with the app.

## 4. Fail closed

An agent acts in the world, so an extension point that can fail must fail in the safe direction. The rule is the same everywhere:

- **A policy that cannot run denies.** A `before_tool` hook that panics, times out or answers nonsense denies the call; a cancelled run's pending policy denies too.
- **A redaction that fails withholds.** An `after_tool` hook that fails never lets the raw output through.
- **A tool that panics is an error result**, carrying its message, not a crashed loop.
- **Nothing is enforced that cannot be.** A plugin feature the host cannot honour — rewriting the conversation, say — refuses to load rather than silently not applying. The [adapter contract](https://github.com/yologdev/yoagent/tree/main/integrations/yoagent-rutis#writing-an-ecosystem-adapter) writes this down for every ecosystem adapter.

Fail-closed costs some convenience: a plugin that would have half-worked elsewhere refuses to load here. That is the intended trade.

## 5. Plugins and other ecosystems, outside the core

The core never depends on a plugin system. [yoagent-rutis](https://github.com/yologdev/yoagent/tree/main/integrations/yoagent-rutis), a separate crate, makes [rutis](https://github.com/arcships/rutis) plugins — Rust in-process, TypeScript and Python in their own runtimes — into **one** `Extension`. A plugin registers a handler; the bridge turns it into hooks; the loop does not know the difference.

That bridge is how other agent ecosystems reach yoagent. DSH's tool plugins, rutis-agent's tools and pi's extensions plug in through small adapters, with **no change to the loop**. Each adapter maps what the loop can honour (tools, policies, notes, images, logs) and refuses or reports the rest.

Two consequences:

- **The contract is the claim, not the adapters.** That three foreign ecosystems fit through one `Extension` is the evidence the contract is general. The adapters themselves follow other projects' releases and need app services (commands, dialogs, sessions) to be complete; they live in the companion crate, marked experimental, and are expected to move to the app that hosts them.
- **What crosses the bridge is what every ecosystem gets:** images in tool results both ways, plugin logs in the host's `tracing` (and, opt-in, in the [GASP](concepts/gasp.md) record of their run), and the fail-closed rules.

## 6. Correct over clever — and honest about cost and cache

A few principles run through the whole loop:

- **The prompt cache is a correctness concern.** The cached prefix is what makes long agent runs affordable, so nothing rewrites it behind the user's back: per-turn notes go on the latest user turn, never into the system prompt; sourced tools are sorted by name so an equal set never reorders; compaction is tiered and measured against real usage.
- **Cost is reported, never guessed.** Pricing is opt-in data, not literals in code; an unpriced call is `None`, not zero; retries, sub-agents, decision models and summaries each have their bucket and roll into one total.
- **Everything is testable without a network.** `MockProvider` drives the loop deterministically; every guarantee here has a test, and the important ones are mutation-checked.
- **It runs where agents run.** The same loop builds for `wasm32` (Cloudflare Workers) through a small runtime shim, so an agent does not have to change when it moves to the edge.

## 7. What it costs

- **You assemble the product.** yoagent gives you no UI, no command system and no session product. A finished agent app is more work than starting from a framework that has them.
- **Some things are deliberately impossible in an extension.** An extension cannot rewrite history, replace the system prompt or hold a UI open between runs. Plugins that need those parts of another ecosystem (pi's plan mode, DSH's web apps) only partly work here.
- **Fail-closed denies more.** A safety policy that cannot be honoured stops the load or the call. That is safer, and occasionally inconvenient.
- **Plugins across processes cost latency.** A TypeScript or Python handler is a call into another process for every hook it implements; hot paths belong in Rust.
- **Ecosystem adapters are partial and move fast.** They cover what the loop can honour and follow other projects' releases.

## 8. Principles and the test for a new capability

### Principles

1. **The loop is written once, in yoagent**, and every guarantee is pinned by a test.
2. **Everything else extends it through one contract**, `Extension`; new hooks are added narrowly, for a concrete user.
3. **Fail closed** at every extension point that can fail.
4. **Never rewrite the cached prefix** behind the caller's back.
5. **Report cost honestly**: unknown is `None`, never zero.
6. **The core depends on nothing optional**: plugin systems, recorders and decision models are features or companion crates.
7. **Integrate APIs, don't host models.**
8. **Write down what is not guaranteed**, not only what is.

### Should a capability be in yoagent?

1. Does every agent need it, in the same way?
2. Can it be an extension, a feature or a companion crate instead of loop code?
3. Does it keep the prompt cache intact?
4. Does it fail closed?
5. Can its guarantee be tested without a network?
6. Is its cost accounted for?

If the answer to the first question is no, the answer is almost always "build it as an extension" — and if it cannot be, the contract may be missing a hook, which is a change worth discussing on its own.

## 9. Open questions

- **App services for plugins.** Commands, dialogs and session history are app concerns, but plugins need them. The current direction: the attached client provides them as plugin-level services, outside the loop.
- **Trust for plugins.** Plugins run with their process's permissions. Finer grants — which plugin may offer or judge which tools — are not designed yet.
- **A stable 1.0 contract.** `Extension` and the bridge's handler shape are young; their stability promise comes with 1.0.

## Related

- [The Agent Loop](concepts/agent-loop.md) · [Extensions](concepts/extensions.md) · [Context Management](concepts/context-management.md) · [Prompt Caching](concepts/prompt-caching.md) · [Model Pricing](concepts/pricing.md) · [GASP](concepts/gasp.md)
- [Architecture overview](architecture/overview.md) · [WebAssembly & Cloudflare Workers](guides/wasm-workers.md) · [Testing Your Agent](guides/testing.md)
- [yoagent-rutis](https://github.com/yologdev/yoagent/tree/main/integrations/yoagent-rutis) · [rutis design philosophy](https://github.com/arcships/rutis/blob/main/docs/design-philosophy.md)
