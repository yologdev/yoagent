# Azure OpenAI Provider

`AzureOpenAiProvider` implements the OpenAI Responses API format with Azure-specific authentication and URL patterns.

## Usage

```rust
use yoagent::provider::{ApiProtocol, ModelConfig};

// Azure has no dedicated ModelConfig preset — build one with `custom`.
// The provider ("azure") resolves the key from AZURE_OPENAI_API_KEY.
let agent = Agent::from_config(ModelConfig::custom(
    ApiProtocol::AzureOpenAiResponses,
    "azure",
    "https://{resource}.openai.azure.com/openai/deployments/{deployment}",
    "gpt-5.5",
    "GPT-5.5",
));
```

## Authentication

Uses the `api-key` header (not `Authorization: Bearer`):

```
api-key: {your_api_key}
```

Additional headers can be set via `ModelConfig.headers` (e.g., for Azure AD Bearer tokens).

## URL Format

```
https://{resource}.openai.azure.com/openai/deployments/{deployment}
```

Set this as `ModelConfig.base_url`. The provider appends `/responses?api-version=2025-01-01-preview`.

## API Details

- **Protocol**: `ApiProtocol::AzureOpenAiResponses`
- **Format**: OpenAI Responses API (not Chat Completions)
- **Streaming**: the Responses API event stream, parsed by the same code as the
  [OpenAI Responses provider](openai-responses.md#streaming-events) (tool calls
  start on `response.output_item.added`; usage splits cached and cache-written
  tokens out of `input`)

## Thinking

`ThinkingLevel` becomes `reasoning.effort`, mapped exactly as the
[OpenAI Responses provider](openai-responses.md#thinking) maps it. A `custom`
config has `compat: None`, so the ceiling is `high` and `Off` omits the effort.
Declare the deployed model's capability on `compat` — copying it from the
matching OpenAI preset is simplest:

```rust
let mut config = ModelConfig::custom(
    ApiProtocol::AzureOpenAiResponses,
    "azure",
    "https://{resource}.openai.azure.com/openai/deployments/{deployment}",
    "gpt-6-sol",
    "GPT-6 Sol",
);
config.compat = ModelConfig::gpt_6_sol().compat; // ceiling `max`, `Off` → `none`
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
