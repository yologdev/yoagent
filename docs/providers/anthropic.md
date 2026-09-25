# Anthropic Provider

`AnthropicProvider` implements the Anthropic Messages API with SSE streaming.

## Usage

```rust
use yoagent::provider::ModelConfig;

let agent = Agent::from_config(ModelConfig::anthropic("claude-sonnet-5", "Claude Sonnet 5"));
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

### Thinking

Set `thinking_level` to enable thinking. By default the provider sends
**adaptive thinking** (`thinking: {"type": "adaptive"}`), which the current
model generation requires (Claude Fable 5/5.1, Opus 5.5, Opus 5, Opus 4.7/4.8,
Sonnet 5 reject budget-based thinking with a 400). The level maps to an
`output_config.effort` hint:

| Level | Effort |
|-------|--------|
| `Minimal`, `Low` | `low` |
| `Medium` | `medium` |
| `High` | `high` |
| `XHigh` | `xhigh` |
| `Max` | `max` |

Effort is passed through, not clamped: the ladder varies by model and the
crate has no per-model table of it. `xhigh` arrived with Opus 4.7, so Opus 4.6
and Sonnet 4.6 accept `max` but reject `xhigh`.

**`ThinkingLevel::Off` does not always mean no thinking.** `Off` omits the
`thinking` field; it never sends `{"type": "disabled"}`. On Claude Opus 5.5 and
Fable 5.1 thinking is always on and cannot be disabled (the API rejects
`disabled` and budget-based thinking), so an `Off` request still thinks at the
model's default effort: `medium` on Opus 5.5, `high` on Fable 5.1. Opus 5 also
thinks whenever the field is absent. The thinking tokens count against
`max_tokens` and bill as output. To think less, pick a low level (`Low` sends
`effort: low`) rather than `Off`.

Pre-4.6 models (Sonnet 4.5, Opus 4.5, Haiku 4.5 and earlier Claude 4) accept
only budget-based thinking and reject `{"type": "adaptive"}` with a 400.
`ModelConfig::claude_haiku_4_5()` already selects it; for other pre-4.6
models, opt into legacy budget-based thinking via `AnthropicCompat::legacy()`:

```rust
let mut config = ModelConfig::anthropic("claude-sonnet-4-5", "Claude Sonnet 4.5");
config.anthropic = Some(AnthropicCompat::legacy());
```

Legacy budgets: `Minimal`/`Low` 1,024 (the API minimum), `Medium` 2,048,
`High` 8,192, `XHigh` 16,384, `Max` 30,720. On this provider `max_tokens` is
automatically raised to budget + 1,024 when needed (`Max` stops at 30,720 so
that budget + 1,024 still fits Opus 4/4.1's 32,000-token output ceiling).
Bedrock uses the same budgets but does **not** raise `max_tokens`: there the
caller must set `max_tokens` above the budget (for `Max`, above 30,720).

Thinking content is streamed as `Content::Thinking` with a cryptographic `signature` for verification.

### Structured Outputs

`Agent::prompt_structured` has two paths on this provider, selected by
`AnthropicCompat::native_structured_output`:

- **Native** (flag on, which every `ModelConfig::claude_*` preset sets): the
  schema is sent as `output_config.format = {"type": "json_schema", "schema": ...}`,
  in the same `output_config` object as the thinking `effort`. No tool is
  added or forced, thinking stays as requested, and the reply's text block is
  the JSON. Required on Claude Fable 5.1 and Opus 5.5, which reject forced
  `tool_choice` (`any` / `tool`) with a 400.
- **Tool-forcing** (flag off, the default for `ModelConfig::anthropic(..)`): a
  synthetic tool built from the schema is appended and forced with
  `tool_choice`, thinking is dropped for that request, and the agent loop
  unwraps the forced call into text. On the native path nothing is unwrapped:
  a call to a tool named after the schema is a real tool call and executes.

The flag is off by default because gateways that speak the Messages protocol
may not accept `output_config.format`. See
[Structured Outputs](../concepts/structured-outputs.md) for the schema rules.

### Claude Opus 5.5

`ModelConfig::claude_opus_5_5()` is not a drop-in replacement for Opus 5. The
API rejects, with a 400:

- `thinking: {"type": "disabled"}` and budget-based thinking. Thinking is
  always on (see above).
- forced `tool_choice` (`any` / `tool`). The preset uses native structured
  outputs, so `prompt_structured` works; nothing else in the crate forces a
  tool.
- `temperature`, `top_p` or `top_k` other than the default. Leave
  `StreamConfig::temperature` unset.

Thinking blocks are tied to the model and the conversation prefix. The
provider sends them back unmodified, which the API requires, but switching a
session to or from Opus 5.5 with `Agent::set_model` carries blocks the other
model cannot read. Opus 5.5 reads Opus 5 and earlier Opus, Sonnet and Haiku
thinking, but not Fable's.

### Stop Reasons

Every documented Anthropic `stop_reason` maps explicitly:

| Wire value | `StopReason` | Notes |
|---|---|---|
| `end_turn`, `stop_sequence` | `Stop` | |
| `tool_use` | `ToolUse` | |
| `max_tokens` | `Length` | |
| `refusal` | `Refusal` | Sets `error_message`; see below |
| `model_context_window_exceeded` | `Error` | In-stream overflow; keeps the phrase `Message::is_context_overflow()` matches, so compaction-retry hooks still fire |
| `pause_turn` | `Error` | The model stopped mid-turn expecting the conversation to be re-sent. This transport cannot resume, so reporting it as a normal stop would return a truncated answer as though it were complete |

Anything unrecognized maps to `Stop` and is logged at `warn`, so a stop reason
added by Anthropic later is visible rather than silently treated as a finish.

**Refusals.** Models with safety classifiers (e.g. Claude Fable 5) can decline a
request with `stop_reason: "refusal"`. The agent loop stops the turn like a
normal `Stop`, and callers can match on the variant to retry on a fallback model.

### Tool Calls With Unusable Arguments

A tool call's arguments arrive as `input_json_delta` fragments and are assembled
at `content_block_stop`. When that assembly cannot happen — the accumulated text
is not valid JSON, or the `content_block_stop` event itself is unusable — the
turn fails with `StopReason::Error` and an `error_message` naming each affected
tool and quoting its input.

This matters because the alternative is silent: a tool executed with empty
arguments falls back to its defaults, so `list_files` asked for `/etc` would
list the working directory instead, with nothing in the response indicating the
model's actual input was dropped.

`agent_loop` returns on `StopReason::Error` before extracting tool calls, so
nothing executes. Every tool call in the message is replaced with a text block —
not just the unusable one — because the turn runs none of them, and a `tool_use`
block with no matching `tool_result` is rejected by the API on the *next*
request.

### Cache Control

Automatic prompt caching via `cache_control` markers:

- **System prompt**: Always cached with `{"type": "ephemeral"}`
- **Second-to-last message**: Gets `cache_control` on its last content block, creating a cache breakpoint

This means on repeated calls, only the latest message is processed at full price.

## Configuration

| Setting | Value |
|---------|-------|
| API URL | `{base_url}/messages` (default `https://api.anthropic.com/v1/messages`) |
| API Version | `2023-06-01` |
| Auth Header | `x-api-key` (or `Authorization: Bearer` with `AnthropicCompat::bearer_auth` set / a custom `authorization` header in `ModelConfig.headers`) |
| Default Max Tokens | request `max_tokens`, else `ModelConfig.max_tokens`, else 8,192 |

Setting `ModelConfig.base_url` retargets the provider at any gateway that
speaks the Anthropic Messages protocol (e.g. OpenCode Zen/Go — see
[OpenCode Zen & Go](opencode.md)).

## Environment Variables

| Variable | Purpose |
|----------|---------|
| `ANTHROPIC_API_KEY` | API key |
