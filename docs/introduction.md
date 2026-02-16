# yoagent

**Simple, effective agent loop in Rust.**

yoagent is a library for building LLM-powered agents that can use tools. It provides the core loop — prompt the model, execute tool calls, feed results back — and gets out of your way.

## Philosophy

**The loop is the product.** An agent is just a loop: send messages to an LLM, get back text and tool calls, execute the tools, repeat until the model stops. yoagent implements this loop with streaming, cancellation, context management, and multi-provider support — so you don't have to.

## Features

- **Streaming events** — Real-time `AgentEvent` stream for UI updates (text deltas, thinking, tool execution)
- **Multi-provider** — Anthropic, OpenAI, Google Gemini, Amazon Bedrock, Azure OpenAI, and any OpenAI-compatible API
- **Tool system** — `AgentTool` trait with built-in coding tools (bash, file read/write/edit, search)
- **Context management** — Automatic token estimation, tiered compaction (truncate tool outputs → summarize → drop old messages)
- **Execution limits** — Max turns, tokens, and wall-clock time
- **Steering & follow-ups** — Interrupt the agent mid-run or queue work for after it finishes
- **Cancellation** — `CancellationToken`-based abort at any point
- **Builder pattern** — Ergonomic `Agent` struct with chainable configuration

## Ecosystem

yoagent is part of the [Yolog](https://github.com/yologdev) ecosystem. It powers the agent backend for Yolog applications.

- **Repository:** [github.com/yologdev/yoagent](https://github.com/yologdev/yoagent)
- **License:** MIT
