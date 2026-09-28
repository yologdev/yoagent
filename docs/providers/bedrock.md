# Amazon Bedrock Provider

`BedrockProvider` implements the AWS Bedrock ConverseStream API.

## Usage

```rust
use yoagent::provider::{ApiProtocol, ModelConfig};

// Bedrock has no dedicated ModelConfig preset — build one with `custom`.
// Credentials come from the environment (see Authentication below).
let agent = Agent::from_config(ModelConfig::custom(
    ApiProtocol::BedrockConverseStream,
    "bedrock",
    "https://bedrock-runtime.us-east-1.amazonaws.com",
    "anthropic.claude-opus-4-8",
    "Claude Opus 4.8",
));

// Or pass credentials explicitly:
//   .with_api_key(bedrock_api_key)                       // Bedrock API key
//   .with_api_key("ACCESS_KEY_ID:SECRET_ACCESS_KEY")      // IAM, SigV4-signed
//   .with_api_key("ACCESS_KEY_ID:SECRET_ACCESS_KEY:SESSION_TOKEN")
```

## Authentication

Two AWS mechanisms are supported:

- **Bedrock API keys** — a bearer token, sent as
  `Authorization: Bearer <key>` (see AWS's
  [Bedrock API keys](https://docs.aws.amazon.com/bedrock/latest/userguide/api-keys.html)
  guide). AWS's environment variable for it is `AWS_BEARER_TOKEN_BEDROCK`.
- **IAM credentials** — an access key id, secret access key and optional
  session token. The request is signed with
  [Signature Version 4](https://docs.aws.amazon.com/IAM/latest/UserGuide/reference_sigv4.html):
  the secret key never leaves the process; the request carries only the
  access key id, an HMAC signature, `x-amz-date`, `x-amz-content-sha256` (the
  hash of the exact body sent) and, with a session token,
  `x-amz-security-token`.

**Which is used**, first match wins:

1. An `authorization` header in `ModelConfig.headers` (any case). It is sent
   as given and nothing else is added — for pre-computed auth or a signing
   proxy.
2. The API key (`with_api_key`, or what `Agent::from_config` resolved from the
   environment when the agent was built):
   - a value **without `:`** is a Bedrock API key (bearer);
   - `access_key_id:secret_access_key` or
     `access_key_id:secret_access_key:session_token` is IAM credentials
     (SigV4). A value with `:` but an empty key id or secret is an error, as
     is a bare access key id (`AKIA…`/`ASIA…`).
3. With no API key, the environment at request time:
   `AWS_BEARER_TOKEN_BEDROCK`, then `AWS_ACCESS_KEY_ID` +
   `AWS_SECRET_ACCESS_KEY` (+ `AWS_SESSION_TOKEN`).

`Agent::from_config` resolves the key the same way
(`provider::resolve_api_key("bedrock")`): `AWS_BEARER_TOKEN_BEDROCK` first,
then the IAM variables composed as `access:secret[:token]`. An explicit
`with_api_key` always wins over the environment. With no credentials at all
the provider returns `ProviderError::Auth` without sending anything.

Other headers in `ModelConfig.headers` are sent but not signed. Credentials
are not refreshed: temporary credentials from the environment are read when
the agent is built (`from_config`) or on each request (empty API key). There
is no support for profiles, `~/.aws/credentials`, SSO or instance roles —
export the variables (for example with `aws configure export-credentials
--format env`) or pass them explicitly.

### Region (SigV4)

The signing region is, in order:

1. the region in the endpoint host — `bedrock-runtime.<region>.amazonaws.com`,
   `bedrock-runtime-fips.<region>.amazonaws.com`, `….amazonaws.com.cn`, or a
   VPC endpoint `vpce-….bedrock-runtime.<region>.vpce.amazonaws.com`;
2. `AWS_REGION`;
3. `AWS_DEFAULT_REGION`.

The host wins because a signature for any other region than the endpoint's is
rejected. A custom endpoint (proxy, private DNS) needs `AWS_REGION`; without a
region, SigV4 fails with `ProviderError::Auth` before sending. The signing
name is `bedrock` (Bedrock Runtime's signing name, not the `bedrock-runtime`
endpoint prefix). Bearer tokens need no region.

The model id is percent-encoded in the request path as the AWS SDKs send it
(`anthropic.claude-sonnet-5-v1:0` → `anthropic.claude-sonnet-5-v1%3A0`, and the
`/` in an inference-profile ARN → `%2F`), and encoded once more in the
signature's canonical request, as SigV4 requires for every service but S3.

The signer is checked against AWS's published SigV4 test vectors and against
a mock server that recomputes each signature from the request it received.
Like the rest of this provider, it has not been run against a live Bedrock
endpoint.

## API Details

- **Endpoint**: `{base_url}/model/{model}/converse-stream` (model id percent-encoded)
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
is kept in `Content::Thinking::redacted` (with `redacted_protocol` set to
`BedrockConverseStream`) and sent back as `redactedContent`. Encrypted
reasoning from another API is not: AWS does not document `redactedContent` as
the same bytes as the Anthropic API's `redacted_thinking` data, so an Anthropic
redacted block is skipped here with a warning (and Bedrock's is skipped by the
Anthropic provider).
Reasoning without a signature is sent back as `reasoningText {text}` with no
signature key (the signature is optional in `ReasoningTextBlock`, and Bedrock's
non-Claude reasoning models such as gpt-oss never sign). The exception is a
Claude model: Claude verifies signatures, so unsigned reasoning (which can only
come from another provider after a model switch) is not replayed to it. A
signature is never sent as `""`. Signature deltas are appended.

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
