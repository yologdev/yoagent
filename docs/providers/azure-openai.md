# Azure OpenAI Provider

`AzureOpenAiProvider` implements the OpenAI Responses API format with Azure-specific authentication and URL patterns.

## Usage

```rust
use yo_agent::provider::AzureOpenAiProvider;

let agent = Agent::new(AzureOpenAiProvider)
    .with_model("gpt-4o")
    .with_api_key(std::env::var("AZURE_OPENAI_API_KEY").unwrap());
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
- **Streaming**: SSE with event types:
  - `response.output_text.delta` — Text content
  - `response.function_call_arguments.start` — Tool call start
  - `response.function_call_arguments.delta` — Tool call arguments
  - `response.completed` — Final usage data

## Message Format

Uses the Responses API input format:

| yo-agent | Azure Responses API |
|----------|-------------------|
| User message | `{"role": "user", "content": "..."}` |
| Assistant text | `{"type": "message", "role": "assistant", "content": [{"type": "output_text", ...}]}` |
| Tool call | `{"type": "function_call", "call_id": "...", "name": "...", "arguments": "..."}` |
| Tool result | `{"type": "function_call_output", "call_id": "...", "output": "..."}` |
| System prompt | `instructions` field |
