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

## Not yet supported

Structured outputs (`output_schema`) are ignored with a warning.
