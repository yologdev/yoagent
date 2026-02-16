# Google Gemini Provider

Two providers for Google's Gemini models:

- `GoogleProvider` — Google AI Studio (Generative AI API)
- `GoogleVertexProvider` — Google Cloud Vertex AI

## Google AI Studio

```rust
use yoagent::provider::GoogleProvider;

let agent = Agent::new(GoogleProvider)
    .with_model("gemini-2.0-flash")
    .with_api_key(std::env::var("GOOGLE_API_KEY").unwrap());
```

### API Details

- **Endpoint**: `{base_url}/v1beta/models/{model}:streamGenerateContent?alt=sse&key={api_key}`
- **Auth**: API key as query parameter
- **Default base URL**: `https://generativelanguage.googleapis.com`
- **Default context window**: 1,000,000 tokens

### Message Format

Google uses a different message format than OpenAI/Anthropic:

| yoagent | Google API |
|----------|-----------|
| `user` role | `user` role |
| `assistant` role | `model` role |
| `Content::Text` | `{"text": "..."}` |
| `Content::Image` | `{"inlineData": {...}}` |
| `Content::ToolCall` | `{"functionCall": {...}}` |
| `Message::ToolResult` | `{"functionResponse": {...}}` |
| System prompt | `systemInstruction` field |
| Tools | `tools[].functionDeclarations[]` |

### Streaming

Uses SSE format (`alt=sse`). Each chunk contains `candidates` with `content.parts` and optional `usageMetadata`.

## Google Vertex AI

`GoogleVertexProvider` uses the same message format but with Vertex AI authentication and endpoints.

- **Protocol**: `ApiProtocol::GoogleVertex`
- **Auth**: OAuth2 / service account credentials
- **Endpoint pattern**: `https://{region}-aiplatform.googleapis.com/v1/projects/{project}/locations/{region}/publishers/google/models/{model}:streamGenerateContent`
