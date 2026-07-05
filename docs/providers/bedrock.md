# Amazon Bedrock Provider

`BedrockProvider` implements the AWS Bedrock ConverseStream API.

## Usage

```rust
use yoagent::provider::{ApiProtocol, ModelConfig};

// Bedrock has no dedicated ModelConfig preset — build one with `custom`.
let agent = Agent::from_config(ModelConfig::custom(
    ApiProtocol::BedrockConverseStream,
    "bedrock",
    "https://bedrock-runtime.us-east-1.amazonaws.com",
    "anthropic.claude-opus-4-8",
    "Claude Opus 4.8",
))
    .with_api_key("ACCESS_KEY:SECRET_KEY");  // or ACCESS_KEY:SECRET_KEY:SESSION_TOKEN
```

## Authentication

The `api_key` field uses a colon-separated format:

```
{access_key_id}:{secret_access_key}
{access_key_id}:{secret_access_key}:{session_token}
```

Alternatively, provide pre-computed auth headers via `ModelConfig.headers` or use an IAM proxy that handles SigV4 signing.

## API Details

- **Endpoint**: `{base_url}/model/{model}/converse-stream`
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

## Stream Events

Bedrock's ConverseStream returns these event types:

- `contentBlockStart` — New content block (text or tool use)
- `contentBlockDelta` — Text or tool use input delta
- `contentBlockStop` — Block complete
- `messageStop` — Stop reason (`end_turn`, `max_tokens`, `tool_use`)
- `metadata` — Token usage
