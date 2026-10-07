# OpenAI Responses

`OpenAiResponsesProvider` speaks OpenAI's Responses API (`POST /v1/responses`),
OpenAI's native interface for reasoning models. `ModelConfig::openai()` targets
Chat Completions instead; see [OpenAI Compatible](openai-compat.md).

## Usage

```rust
use yoagent::agent::Agent;
use yoagent::provider::ModelConfig;

// Provider auto-selected (OpenAiResponsesProvider), key from OPENAI_API_KEY.
let agent = Agent::from_config(ModelConfig::openai_responses("gpt-5.5", "GPT-5.5"));
```

`openai_responses` is priced when the id is listed in the price data
(`gpt-5.5`, the GPT-6 models — see [Model Pricing](../concepts/pricing.md))
and unpriced (`cost: None`) otherwise. For an unlisted model, set `cost` to a
`CostConfig` if you want `session_cost_usd` and the telemetry `cost_usd`
field.

The GPT-6 presets — `ModelConfig::gpt_6_astra()`, `gpt_6_sol()`,
`gpt_6_luna()` — are built on `openai_responses` and priced, including their
272K context tier. They use Responses because Chat Completions does not
support function calling with GPT-6 Astra, and on Sol/Luna allows it only at
reasoning effort `none`. This provider does not enforce `prompt_structured`
schemas (see [Not yet supported](#not-yet-supported)), so neither do the GPT-6
presets.

```rust
let agent = Agent::from_config(ModelConfig::gpt_6_sol());
```

## Streaming events

The provider (and [Azure OpenAI](azure-openai.md), which shares its parser)
handles these server-sent events:

| Event | Becomes |
|-------|---------|
| `response.output_text.delta` | `Content::Text` |
| `response.reasoning_summary_text.delta`, `response.reasoning_text.delta` | `Content::Thinking` |
| `response.output_item.added` with `item.type == "function_call"` | start of a `Content::ToolCall` (`call_id`, `name`) |
| `response.output_item.done` with `item.type == "reasoning"` | its `encrypted_content` kept on the item's `Content::Thinking` for replay (see [Encrypted reasoning](#encrypted-reasoning)); a summary sent only here becomes the thinking text |
| `response.function_call_arguments.delta` | argument text, routed by `output_index` / `item_id` |
| `response.function_call_arguments.done`, `response.output_item.done` | final arguments (source of truth) |
| `response.refusal.delta` / `.done`, a `refusal` content part | refusal text as `Content::Text`, `StopReason::Refusal` |
| `response.completed` | usage; `status: "incomplete"` → `StopReason::Length` (`Refusal` when `incomplete_details.reason` is `content_filter`) |
| `response.incomplete` | usage, `StopReason::Length` (`Refusal` for `content_filter`) |
| `response.failed`, `error` | `ProviderError` |

A refusal or content-filter stop also sets the assistant message's
`error_message` (on both this provider and Azure), and a refusal is never
relabelled `ToolUse`. A token count sent as explicit `null` in `usage` reads as
0 rather than failing the terminal event. A mid-stream error whose `type` /
`code` says rate limit or capacity (`too_many_requests`, `no_capacity`,
`rate_limit_exceeded`, …) is classified `RateLimited` and retried, not treated
as a context overflow.

Parallel function calls each get their own buffer. Argument text that does not
parse as a JSON object (for example, cut off by `response.incomplete`) is kept
under the `__partial_json` marker, and the agent loop answers that call with an
error instead of running it.

## Usage and cost

`usage.input_tokens` counts the whole prompt. The provider splits it:

| `Usage` field | Source |
|---------------|--------|
| `cache_read` | `input_tokens_details.cached_tokens` |
| `cache_write` | `input_tokens_details.cache_write_tokens` |
| `input` | `input_tokens` − cached − written |
| `output` | `output_tokens` |

This is the same convention as the Anthropic provider, so `CostConfig` prices
each bucket at its own rate (`with_cache_read`, `with_cache_write`), and
context-tier selection still uses the full prompt size.

## Thinking

`ThinkingLevel` becomes `reasoning.effort`. How high it goes is the model's
declared capability, read from `ModelConfig::compat`:
`OpenAiCompat::max_reasoning_effort` (`High` / `XHigh` / `Max`). This provider
reads that one field and ignores the rest of `OpenAiCompat`.

`ThinkingLevel::Off` omits `reasoning` entirely, on every model. That runs a
reasoning model at its default effort (`medium` on most OpenAI models), not
without reasoning; the crate never sends OpenAI's `none` rung, which several
models (GPT-6 Astra, gpt-5, the o-series) reject with HTTP 400.

`openai_responses(..)` sets `compat: None` — ceiling `high`, so `XHigh` and
`Max` are sent as `high` (logged once per process with `tracing::warn!`). The
GPT-6 presets declare `max`, so `XHigh` sends `xhigh` and `Max` sends `max`.
For another model, declare it:

```rust
use yoagent::provider::{ModelConfig, OpenAiCompat, ReasoningEffortCeiling};

let mut config = ModelConfig::openai_responses("gpt-5.6-sol", "GPT-5.6 Sol");
let mut compat = OpenAiCompat::openai();
compat.max_reasoning_effort = ReasoningEffortCeiling::Max;
config.compat = Some(compat);
```

See the [`ThinkingLevel` table](../reference/configuration.md#thinkinglevel).

`temperature` is rejected by GPT-6 while the effort is anything but `none`,
and this crate never sends `none`: leave `StreamConfig::temperature` unset.

## Encrypted reasoning

A reasoning model's reasoning is carried across turns, including across tool
calls, as OpenAI recommends ("pass back any reasoning items returned with the
last function call"):

- **Requested.** For a reasoning model — `ModelConfig::reasoning`, or any
  request that sends a reasoning effort — the body carries
  `include: ["reasoning.encrypted_content"]`. Non-reasoning models get no
  `include`: they have no reasoning to return, and OpenAI rejects it there
  with a 400 ("Encrypted content is not supported with this model").
  `openai_responses(id, ..)` infers `reasoning` from the id: `false` for ids
  starting with `gpt-3`, `gpt-4` (`gpt-4o`, `gpt-4.1`, …) or `chatgpt-`, and
  for any id containing `-chat` (`gpt-5-chat-latest`); `true` otherwise (the
  o-series, `gpt-5*`, unknown ids). The GPT-6 presets set `true`. Set
  `config.reasoning` to override.
- **Kept.** The finished reasoning item (`response.output_item.done`) is stored
  on its `Content::Thinking` block: `thinking` is the readable summary (empty
  when no summary was requested, which is OpenAI's default), and `redacted`
  holds the item's `id`, `summary` and `encrypted_content` as a JSON object,
  with `redacted_protocol = OpenAiResponses`. The same object records the ids
  of the output items that followed it: `call_ids` (each function call's
  `call_id` → its item `id`, `fc_…`) and `message_id` (the first message's
  `msg_…`).
- **Replayed in place.** The next request sends it back as
  `{"type": "reasoning", "id", "summary", "encrypted_content"}`, before the
  function call or message it led to, and that call or message carries its
  item `id` again: OpenAI refuses a reasoning item sent without its paired
  item ("Item 'rs_…' of type 'reasoning' was provided without its required
  following item"). Items with no replayed reasoning before them are sent
  without ids. A reasoning item with no output after it in its turn (a
  cut-off response) is not sent.
- **Only to the API that produced it.** Encrypted reasoning from Azure,
  Anthropic (`redacted_thinking`), Bedrock or of unknown origin is skipped, and
  this provider's blocks are skipped by every other provider — including Azure,
  which is a different service with its own keys. Plain thinking text from
  another provider is not sent either; the Responses API has no input for it.

`store` is not sent, so OpenAI's default (`true`) applies. yoagent resends the
whole history on every request and never uses `previous_response_id`, which is
the stateless pattern the encrypted content exists for; OpenAI returns
`encrypted_content` by default only in stateless mode (`store: false`, or Zero
Data Retention) and otherwise on this `include`.

## Prompt caching

The request carries `prompt_cache_key`: `CacheConfig::session_key` when set,
otherwise a key derived from the system prompt (`yo-` + a hash), stable for a
session. `CacheConfig::disabled()` (or any configuration with caching hints
off) sends none. See [Prompt Caching](../concepts/prompt-caching.md#openai).

## Not yet supported

Structured outputs (`output_schema`) are ignored with a warning.

## Live test

`tests/integration_openai_responses.rs` runs a tool call, the continuation that
replays the encrypted reasoning, and a follow-up turn against the real API. It
is `#[ignore]`d and skips (passing) without a key:

```bash
OPENAI_API_KEY=... cargo test --test integration_openai_responses -- --ignored --nocapture
```

It uses `gpt-6-luna` unless `YOAGENT_RESPONSES_MODEL` names another Responses
model. The key is read from the environment and never printed.
