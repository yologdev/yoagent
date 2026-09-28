# Amazon Bedrock Provider

`BedrockProvider` implements the AWS Bedrock ConverseStream API.

## Usage

```rust
use yoagent::provider::{ApiProtocol, ModelConfig};

// Bedrock has no dedicated ModelConfig preset — build one with `custom`.
let agent = Agent::from_config(ModelConfig::custom(
    ApiProtocol::BedrockConverseStream,
    "bedrock",
    "https://bedrock-runtime.us-east-1.amazonaws.com",
    "anthropic.claude-opus-4-8",
    "Claude Opus 4.8",
))
    .with_api_key("ACCESS_KEY:SECRET_KEY");  // or ACCESS_KEY:SECRET_KEY:SESSION_TOKEN
```

## Authentication

The `api_key` field uses a colon-separated format:

```
{access_key_id}:{secret_access_key}
{access_key_id}:{secret_access_key}:{session_token}
```

Alternatively, provide pre-computed auth headers via `ModelConfig.headers` or use an IAM proxy that handles SigV4 signing.

> **Caution:** yoagent does not SigV4-sign requests. When `ModelConfig.headers`
> has no `authorization` entry, the provider sends `Authorization: Bearer
> {api_key}` — the whole `access:secret` string — which Bedrock rejects and
> which puts the secret key in a header. Until this is fixed (tracked in
> [#174](https://github.com/yologdev/yoagent/issues/174)), supply your own
> `authorization` header or route through a signing proxy.

## API Details

- **Endpoint**: `{base_url}/model/{model}/converse-stream`
- **Default base URL**: `https://bedrock-runtime.us-east-1.amazonaws.com`
- **Protocol**: `ApiProtocol::BedrockConverseStream`

## Message Format

Bedrock uses its own content block format:

| yoagent | Bedrock API |
|----------|-------------|
| `Content::Text` | `{"text": "..."}` |
| `Content::Image` | `{"image": {"format": "...", "source": {"bytes": "..."}}}` |
| `Content::ToolCall` | `{"toolUse": {"toolUseId": "...", "name": "...", "input": ...}}` |
| `Message::ToolResult` | `{"toolResult": {"toolUseId": "...", "content": [...], "status": "success"}}` |
| System prompt | `system` array of text blocks |
| Tools | `toolConfig.tools[].toolSpec` |
| Max tokens | `inferenceConfig.maxTokens` |

## Thinking

`ThinkingLevel` is sent as Anthropic's legacy budget-based thinking
(`additionalModelRequestFields.thinking.budget_tokens`); budgets per level are in
the [`ThinkingLevel` table](../reference/configuration.md#thinkinglevel). Bedrock does not raise `maxTokens` above the
budget, so set `max_tokens` higher yourself (for `Max`, above 30,720). Claude
4.7+ models (Opus 4.7/4.8 and the generation after; see the
[Anthropic provider page](anthropic.md#thinking)) reject budget-based thinking,
so thinking is not usable with them on this provider yet.

## Stream Events

ConverseStream answers with binary `application/vnd.amazon.eventstream`
frames. Each frame carries a length prelude and a trailing checksum (both
CRC-32 verified), binary headers and a JSON payload. The event type is the
`:event-type` header; the payload is the event itself, with no wrapper key.
The provider buffers bytes across network chunks and decodes only complete
frames.

| Event | Payload | What yoagent does |
|-------|---------|-------------------|
| `messageStart` | `{role}` | — |
| `contentBlockStart` | `{contentBlockIndex, start: {toolUse: {toolUseId, name}}}` | Opens a tool call (`ToolCallStart`) |
| `contentBlockDelta` | `{contentBlockIndex, delta: {text} \| {toolUse: {input}} \| {reasoningContent: {text} \| {signature} \| {redactedContent}}}` | Text, thinking and tool-input deltas, accumulated per block index |
| `contentBlockStop` | `{contentBlockIndex}` | Parses the tool call's accumulated input (`ToolCallEnd`) |
| `messageStop` | `{stopReason}` | Sets the stop reason (below) |
| `metadata` | `{usage: {inputTokens, outputTokens, totalTokens, cacheReadInputTokens?, cacheWriteInputTokens?}, metrics}` | Usage, including cache reads and writes |

Stop reasons: `end_turn` / `stop_sequence` → `Stop`, `tool_use` → `ToolUse`,
`max_tokens` → `Length`, `guardrail_intervened` / `content_filtered` →
`Refusal`, `model_context_window_exceeded` → `Error` (detected as a context
overflow), `malformed_model_output` / `malformed_tool_use` → `Error`.

Tool input that does not parse (cut off at the token limit) and tool blocks
that never receive `contentBlockStop` are not run: the loop answers them with an
error tool result. Server-side tool blocks and image, citation and tool-result
content are not surfaced; each is dropped with a warning (once per block), as
is any union member or event type added after this was written.

Reasoning is kept for replay: a signed reasoning block is sent back as
`reasoningText {text, signature}`, and encrypted reasoning (`redactedContent`)
is kept in `Content::Thinking::redacted` and sent back as `redactedContent`.
Reasoning without a signature (for example from another provider after a model
switch) is skipped on replay, with a warning. Signature deltas are appended.

If the stream ends after `messageStop` without a `metadata` event, the turn
reports zero tokens (as the other providers do when usage never arrives) and a
warning with `usage_missing = true` is logged inside the `llm_stream` span.

Errors are never an empty successful turn:

- An exception frame (`:message-type: exception`) is classified by the HTTP
  status AWS documents for it: `throttlingException` is a retryable rate
  limit, a `validationException` whose message reports an over-long input is
  a context overflow, and the rest (`modelStreamErrorException`,
  `internalServerException`, `serviceUnavailableException`, …) are API
  errors.
- A checksum mismatch or malformed frame is an error.
- A dropped connection, a body that ends inside a frame, or a stream that ends
  without `messageStop` is a retryable network error — unless `messageStop` and
  `metadata` have both arrived, in which case the response is complete and is
  kept (with a warning) rather than retried and billed again.
- A `200` whose `content-type` is not `application/vnd.amazon.eventstream` is
  an error carrying an excerpt of the body.

This is tested with mock frames built to AWS's documented format, not against a
live Bedrock endpoint.
