# Prompt Caching

yoagent automatically optimizes API costs through prompt caching. For providers that support it, stable content (system prompts, tool definitions, conversation history) is cached between turns, giving you up to **90% savings** on input tokens.

## How It Works

In a multi-turn agent loop, each request sends the full context: system prompt + tools + conversation history. Without caching, you pay full price for all of it every turn. With caching, the provider reuses previously processed prefixes.

### Provider Support

| Provider | Caching Type | Savings | Framework Action |
|----------|-------------|---------|-----------------|
| **Anthropic** | Explicit (cache breakpoints) | 90% on hits | ✅ Auto-placed |
| **OpenAI** | Automatic (>1024 tokens) | 50% on hits | None needed |
| **DeepSeek** | Automatic prefix cache | Varies by model | None needed |
| **Google Gemini** | Implicit (automatic) | Varies | None needed |
| **Azure OpenAI** | Automatic (same as OpenAI) | 50% on hits | None needed |
| **Amazon Bedrock** | None (no automatic caching) | — | Not supported |

### What Gets Cached (Anthropic)

yoagent places up to 3 cache breakpoints automatically:

1. **System prompt** — stable across all turns
2. **Tool definitions** — rarely change between turns
3. **Conversation history** — second-to-last message, so the growing prefix is cached

This means on a typical multi-turn conversation, only the latest user message and the new assistant response cost full price.

### DeepSeek

DeepSeek's API manages context caching automatically. yoagent does not send
Anthropic-style `cache_control` markers for DeepSeek; instead, keep stable
prefixes stable (system prompt, tool definitions, and earlier messages) and
monitor DeepSeek's `prompt_cache_hit_tokens` / `prompt_cache_miss_tokens`
usage fields through `Usage.cache_read` and `Usage.input`.

## Configuration

Caching is **enabled by default** with automatic breakpoint placement. No configuration needed for optimal behavior.

### Disable Explicit Cache Hints

```rust
use yoagent::{CacheConfig, CacheStrategy};

let agent = Agent::from_config(ModelConfig::anthropic("claude-sonnet-5", "Claude Sonnet 5"))
    .with_cache_config(CacheConfig {
        enabled: false,
        ..Default::default()
    });
```

This disables yoagent-managed cache hints for providers such as Anthropic. It
does not turn off automatic server-side caching for providers such as DeepSeek
or OpenAI.

### Fine-Grained Control

```rust
let agent = Agent::from_config(ModelConfig::anthropic("claude-sonnet-5", "Claude Sonnet 5"))
    .with_cache_config(CacheConfig {
        enabled: true,
        strategy: CacheStrategy::Manual {
            cache_system: true,
            cache_tools: true,
            cache_messages: false, // Don't cache conversation history
        },
    });
```

## Monitoring Cache Usage

Every `Usage` struct includes cache statistics:

```rust
// After a response:
let usage = message.usage(); // from assistant message
println!("Cache read: {} tokens", usage.cache_read);
println!("Cache write: {} tokens", usage.cache_write);
println!("Cache hit rate: {:.1}%", usage.cache_hit_rate() * 100.0);
```

- **`cache_read`** — tokens served from cache (cheap)
- **`cache_write`** — tokens written to cache when the provider reports that metric
- **`cache_hit_rate()`** — fraction of input tokens from cache (0.0–1.0)

## Cost Impact

For a typical 10-turn agent conversation with Anthropic Claude:

| Without Caching | With Caching (auto) |
|-----------------|-------------------|
| ~500K input tokens billed at full price | ~50K at full price + ~450K at 10% price |
| **$1.50** (Claude Sonnet 5) | **$0.29** (Claude Sonnet 5) |

That's an **~80% cost reduction** with zero configuration.

## Best Practices

1. **Keep system prompts stable** — changing the system prompt between turns invalidates the cache
2. **Don't shuffle tools** — tool order matters for cache prefix matching
3. **Let it work automatically** — the default `CacheStrategy::Auto` is optimal for most use cases
4. **Monitor `cache_hit_rate()`** — if it's consistently low, check if your system prompt or tools are changing unexpectedly
