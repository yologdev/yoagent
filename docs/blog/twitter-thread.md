# Twitter Thread — yoagent v0.1.0 Launch

---

**Tweet 1 (hook)**

We just released yoagent — an agent loop in Rust inspired by @badlogicgames' pi-agent-core.

Same philosophy: the loop IS the product. No planning layers, no RAG. Just prompt → stream → tools → loop.

But we didn't just port it. We improved it. 🧵

---

**Tweet 2 (parallel tools)**

🔀 Parallel Tool Execution

pi-agent-core runs tools sequentially. When the LLM asks to "read file A, read file B, search for X" — that's 3 serial waits.

yoagent runs them concurrently by default. 3 tools × 50ms = ~50ms, not 150ms.

Sequential and batched modes available too.

---

**Tweet 3 (batteries included)**

🔋 Batteries Included

pi-agent-core is deliberately minimal — no providers, no tools. Clean, but you wire everything yourself.

yoagent ships with:
• 7 API protocols, 20+ LLM providers
• 6 built-in tools (bash, file I/O, search)
• MCP client (stdio + HTTP)
• Prompt caching (Anthropic)

One crate. Zero extra deps.

---

**Tweet 4 (retry)**

🔄 Automatic Retry

Rate limited? Network hiccup? yoagent retries automatically.

• Exponential backoff with jitter
• Respects retry-after headers
• Only retries transient errors (not auth failures)
• Enabled by default, zero config

pi-agent-core delegates this to the provider layer.

---

**Tweet 5 (context management)**

🧠 Built-in Context Management

• Token estimation per message
• Smart truncation (keep first + last, drop middle)
• Execution limits: max turns, tokens, duration
• Prevents runaway loops automatically

pi-agent-core gives you a hook. yoagent gives you the implementation.

---

**Tweet 6 (Rust advantage)**

🦀 Why Rust?

• Single binary — no Node.js, no node_modules
• True parallelism (futures::join_all, not JS cooperative scheduling)
• Memory safety without GC pauses
• Enums > strings (compiler catches mistakes)

The 210-line CLI example is a working coding agent. Try it.

---

**Tweet 7 (fair comparison)**

⚖️ What pi-agent-core does better:

• Battle-tested (v0.52, extensively iterated)
• More natural streaming (async iterators vs event collection)
• Dynamic API key rotation per-call
• TypeScript ecosystem compatibility

We respect the original. We just wanted more out of the box.

---

**Tweet 8 (CTA)**

yoagent v0.1.0 is live:

📦 cargo add yoagent
🐙 github.com/yologdev/yoagent
📖 yologdev.github.io/yoagent/
📝 Full comparison blog post: [link]

Thanks @badlogicgames for pi-agent-core — the design that proved thin loop + good model > complex frameworks.

---
