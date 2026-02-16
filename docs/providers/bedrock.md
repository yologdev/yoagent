# Amazon Bedrock Provider

`BedrockProvider` implements the AWS Bedrock ConverseStream API.

## Usage

```rust
use yoagent::provider::BedrockProvider;

let agent = Agent::new(BedrockProvider)
    .with_model("anthropic.claude-3-sonnet-20240229-v1:0")
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
