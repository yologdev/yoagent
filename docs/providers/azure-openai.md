# Azure OpenAI Provider

`AzureOpenAiProvider` calls the Responses API on Azure OpenAI's **v1 API**, the
surface Microsoft documents for Responses today.

## Usage

```rust
use yoagent::provider::{ApiProtocol, ModelConfig};

// Azure has no dedicated ModelConfig preset — build one with `custom`.
// The provider ("azure") resolves the key from AZURE_OPENAI_API_KEY.
let agent = Agent::from_config(ModelConfig::custom(
    ApiProtocol::AzureOpenAiResponses,
    "azure",
    "https://{resource}.openai.azure.com/openai/v1",
    "{deployment}", // the model id is your *deployment name*
    "GPT-5.5",
));
```

## URL Format

Every request is:

```
POST https://{resource}.openai.azure.com/openai/v1/responses
```

with no `api-version` query parameter. Azure's v1 API guide: "`api-version` is
no longer a required parameter with the v1 GA API." The deployment is named by
`model` in the request body, not in the path, so the model id must be the
**deployment name** (Azure's troubleshooting note: "**404**: Confirm `model`
matches your deployment name.").

`ModelConfig.base_url` can take any of these forms; each resolves to the URL
above:

| `base_url` | Notes |
|---|---|
| `https://{resource}.openai.azure.com` | resource endpoint |
| `https://{resource}.openai.azure.com/openai/v1` | the base URL Azure's SDK samples use (trailing `/` fine) |
| `https://{resource}.services.ai.azure.com/openai/v1` | Foundry endpoint form, also accepted by Azure |
| `https://{resource}.openai.azure.com/openai/deployments/{deployment}` | legacy form, see below |

### Migrating from the deployment-scoped URL

Earlier versions documented
`https://{resource}.openai.azure.com/openai/deployments/{deployment}` and sent
`{base_url}/responses?api-version=2025-01-01-preview`. Azure never served
Responses there: the API arrived in `2025-03-01-preview`, at
`/openai/responses`, not under `/deployments/`. Those requests failed.

A deployment-scoped `base_url` still works. The provider sends it to
`/openai/v1/responses` and puts `{deployment}` from the URL in the body's
`model`, overriding the configured model id. New configs should use the
`/openai/v1` form with the deployment name as the model id.

## Authentication

With an API key, the provider sends the `api-key` header, as in Azure's REST
samples:

```
api-key: {your_api_key}
```

For Microsoft Entra ID, leave the key empty (don't set
`AZURE_OPENAI_API_KEY`) and pass the token in `ModelConfig.headers`:

```rust
config.headers.insert("Authorization".into(), format!("Bearer {token}"));
```

When the key is empty, the provider sends no `api-key` header.

## API Details

- **Protocol**: `ApiProtocol::AzureOpenAiResponses`
- **Format**: OpenAI Responses API (not Chat Completions)
- **Streaming**: the Responses API event stream, parsed by the same code as the
  [OpenAI Responses provider](openai-responses.md#streaming-events) (tool calls
  start on `response.output_item.added`; usage splits cached and cache-written
  tokens out of `input`; refusals and `content_filter` stops become
  `StopReason::Refusal` with `error_message` set)
- **Errors**: a mid-stream `too_many_requests` / `no_capacity` error is
  `RateLimited` and retried, not a context overflow
- **Structured outputs**: `prompt_structured` schemas are ignored with a warning

## Thinking

`ThinkingLevel` becomes `reasoning.effort`, mapped exactly as the
[OpenAI Responses provider](openai-responses.md#thinking) maps it. A `custom`
config has `compat: None`, so the ceiling is `high`. `Off` always omits the
effort (the deployed model then runs at its default, not without reasoning).
Declare the deployed model's capability on `compat` — copying it from the
matching OpenAI preset is simplest:

```rust
let mut config = ModelConfig::custom(
    ApiProtocol::AzureOpenAiResponses,
    "azure",
    "https://{resource}.openai.azure.com/openai/v1",
    "{deployment}", // a deployment of gpt-6-sol
    "GPT-6 Sol",
);
config.compat = ModelConfig::gpt_6_sol().compat; // ceiling `max`
```

Azure's reasoning guide: "max works only with GPT-6 or GPT-5.6 models and the
Responses API. xhigh works only with GPT-6, GPT-5.6, GPT-5.5, GPT-5.4, and
gpt-5.1-codex-max models." See the
[`ThinkingLevel` table](../reference/configuration.md#thinkinglevel).

## Message Format

Uses the Responses API input format:

| yoagent | Azure Responses API |
|----------|-------------------|
| User message | `{"role": "user", "content": "..."}` |
| Assistant text | `{"type": "message", "role": "assistant", "content": [{"type": "output_text", ...}]}` |
| Tool call | `{"type": "function_call", "call_id": "...", "name": "...", "arguments": "..."}` |
| Tool result | `{"type": "function_call_output", "call_id": "...", "output": "..."}` |
| System prompt | `instructions` field |
