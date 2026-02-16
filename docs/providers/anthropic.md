# Anthropic Provider

`AnthropicProvider` implements the Anthropic Messages API with SSE streaming.

## Usage

```rust
use yo_agent::provider::AnthropicProvider;

let agent = Agent::new(AnthropicProvider)
    .with_model("claude-sonnet-4-20250514")
    .with_api_key(std::env::var("ANTHROPIC_API_KEY").unwrap());
```

## Features

### Streaming SSE

Uses `reqwest-eventsource` to parse Anthropic's SSE stream. Events handled:

- `message_start` — Input token usage, cache stats
- `content_block_start` — Text, thinking, or tool_use block
- `content_block_delta` — Text, thinking, input JSON, or signature deltas
- `content_block_stop` — Block complete
- `message_delta` — Stop reason, output usage
- `message_stop` — Stream complete

### Extended Thinking

Set `thinking_level` to enable thinking with a token budget:

| Level | Budget Tokens |
|-------|--------------|
| `Minimal` | 128 |
| `Low` | 512 |
| `Medium` | 2,048 |
| `High` | 8,192 |

Thinking content is streamed as `Content::Thinking` with a cryptographic `signature` for verification.

### Cache Control

Automatic prompt caching via `cache_control` markers:

- **System prompt**: Always cached with `{"type": "ephemeral"}`
- **Second-to-last message**: Gets `cache_control` on its last content block, creating a cache breakpoint

This means on repeated calls, only the latest message is processed at full price.

## Configuration

| Setting | Value |
|---------|-------|
| API URL | `https://api.anthropic.com/v1/messages` |
| API Version | `2023-06-01` |
| Auth Header | `x-api-key` |
| Default Max Tokens | 8,192 |

## Environment Variables

| Variable | Purpose |
|----------|---------|
| `ANTHROPIC_API_KEY` | API key |
