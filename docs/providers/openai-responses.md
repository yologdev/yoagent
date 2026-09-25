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

`ThinkingLevel` becomes `reasoning.effort` (`XHigh`/`Max` clamp to `high`); see
the [`ThinkingLevel` table](../reference/configuration.md#thinkinglevel).

## Not yet supported

Structured outputs (`output_schema`) are ignored with a warning.
