# Architecture Overview

## Module Layout

```
yoagent/
├── src/
│   ├── lib.rs              # Public re-exports
│   ├── types.rs            # Core types: Message, Content, AgentTool, AgentEvent
│   ├── agent.rs            # Agent struct (stateful wrapper)
│   ├── agent_loop.rs       # Core loop: prompt → LLM → tools → repeat
│   ├── context.rs          # Token estimation, compaction, limits
│   ├── provider/
│   │   ├── mod.rs          # Provider re-exports
│   │   ├── traits.rs       # StreamProvider trait, StreamEvent, ProviderError
│   │   ├── model.rs        # ModelConfig, ApiProtocol, OpenAiCompat
│   │   ├── registry.rs     # ProviderRegistry (protocol → provider map)
│   │   ├── anthropic.rs    # Anthropic Messages API
│   │   ├── openai_compat.rs # OpenAI Chat Completions (multi-provider)
│   │   ├── openai_responses.rs # OpenAI Responses API
│   │   ├── google.rs       # Google Generative AI
│   │   ├── google_vertex.rs # Google Vertex AI
│   │   ├── bedrock.rs      # AWS Bedrock ConverseStream
│   │   ├── azure_openai.rs # Azure OpenAI Responses
│   │   ├── mock.rs         # Mock provider for testing
│   │   └── sse.rs          # SSE utilities
│   └── tools/
│       ├── mod.rs          # default_tools(), re-exports
│       ├── bash.rs         # BashTool
│       ├── file.rs         # ReadFileTool, WriteFileTool
│       ├── edit.rs         # EditFileTool
│       ├── list.rs         # ListFilesTool
│       └── search.rs       # SearchTool
```

## Data Flow

```
                    ┌─────────────┐
                    │   Caller    │
                    └──────┬──────┘
                           │ prompt / prompt_messages
                    ┌──────▼──────┐
                    │    Agent    │  Stateful wrapper
                    │  (agent.rs) │  Manages queues, tools, state
                    └──────┬──────┘
                           │
                    ┌──────▼──────┐
                    │ agent_loop  │  Core loop
                    │             │  Prompt → LLM → Tools → Repeat
                    └──┬───────┬──┘
                       │       │
              ┌────────▼──┐ ┌──▼────────┐
              │ Provider  │ │   Tools   │
              │ .stream() │ │ .execute()│
              └────────┬──┘ └──┬────────┘
                       │       │
              ┌────────▼──┐ ┌──▼────────┐
              │ LLM API   │ │ OS / FS   │
              │ (HTTP)    │ │ (shell)   │
              └───────────┘ └───────────┘

Events flow back via mpsc::UnboundedSender<AgentEvent>
```

## How Providers Plug In

1. Implement `StreamProvider` trait
2. Register with `ProviderRegistry` under an `ApiProtocol`
3. Set `ModelConfig.api` to match that protocol
4. The registry dispatches `stream()` calls to the right provider

Each provider translates between yoagent's `Message`/`Content` types and the provider's native API format. All providers emit `StreamEvent`s through the channel for real-time updates.

## How Tools Plug In

1. Implement `AgentTool` trait
2. Add to the tools vec (via `default_tools()` or custom)
3. The agent loop converts tools to `ToolDefinition` (name, description, schema) for the LLM
4. When the LLM returns `Content::ToolCall`, the loop finds the matching tool and calls `execute()`
5. Results are wrapped in `Message::ToolResult` and added to context

Tools receive a `CancellationToken` child token — they should check it for cooperative cancellation during long operations.
