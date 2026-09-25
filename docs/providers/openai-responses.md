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

`openai_responses` is unpriced (`cost: None`). Set `cost` to a `CostConfig`
for the model you use if you want `session_cost_usd` and the telemetry
`cost_usd` field.

The GPT-6 presets — `ModelConfig::gpt_6_astra()`, `gpt_6_sol()`,
`gpt_6_luna()` — are built on `openai_responses` and priced, including their
272K context tier. They use Responses because Chat Completions does not
support function calling with GPT-6 Astra, and on Sol/Luna allows it only at
reasoning effort `none`.

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
| `response.completed` | usage; `status: "incomplete"` → `StopReason::Length` |
| `response.incomplete` | usage, `StopReason::Length` |
| `response.failed`, `error` | `ProviderError` |

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

`ThinkingLevel` becomes `reasoning.effort`. How high it goes, and whether
`Off` is sent as `none`, is the model's declared capability, read from
`ModelConfig::compat`: `OpenAiCompat::max_reasoning_effort` (`High` /
`XHigh` / `Max`) and `OpenAiCompat::supports_effort_none`. This provider reads
those two fields and ignores the rest of `OpenAiCompat`.

`openai_responses(..)` sets `compat: None` — ceiling `high`, `Off` omitted,
exactly what this provider always sent. The GPT-6 presets set it for you:

| Preset | Ceiling | `Off` sends |
|--------|---------|-------------|
| `gpt_6_astra()` | `max` | nothing — `none` is an HTTP 400 on Astra |
| `gpt_6_sol()`, `gpt_6_luna()` | `max` | `none` |

For another model, declare it:

```rust
use yoagent::provider::{ModelConfig, OpenAiCompat, ReasoningEffortCeiling};

let mut config = ModelConfig::openai_responses("gpt-5.6-sol", "GPT-5.6 Sol");
let mut compat = OpenAiCompat::openai();
compat.max_reasoning_effort = ReasoningEffortCeiling::Max;
compat.supports_effort_none = true;
config.compat = Some(compat);
```

Omitting the effort runs a reasoning model at its default (`medium`), not
without reasoning, which is why `Off` sends `none` where the model has it. See
the [`ThinkingLevel` table](../reference/configuration.md#thinkinglevel).

`temperature` is rejected by GPT-6 while the effort is anything
but `none`; leave `StreamConfig::temperature` unset unless you run at `Off` on
a model with `none`.

## Not yet supported

Structured outputs (`output_schema`) are ignored with a warning.
