# OpenAPI Tool Adapter

Auto-generate `AgentTool` implementations from OpenAPI 3.0 specs. Point an agent at any API spec and it instantly gets callable tools for every operation.

> **Feature-gated** — add `features = ["openapi"]` to your `Cargo.toml`.

## Quick Start

```rust
use yoagent::Agent;
use yoagent::openapi::{OpenApiToolAdapter, OpenApiConfig, OperationFilter};
use yoagent::provider::AnthropicProvider;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let config = OpenApiConfig::new()
        .with_bearer_token("sk-...");

    let agent = Agent::new(AnthropicProvider)
        .with_system_prompt("You are an API assistant.")
        .with_model("claude-sonnet-4-20250514")
        .with_api_key(std::env::var("ANTHROPIC_API_KEY")?)
        .with_openapi_file("petstore.yaml", config, &OperationFilter::All)
        .await?;

    Ok(())
}
```

## Loading Specs

Three ways to load an OpenAPI spec:

```rust
// From a file
let agent = agent.with_openapi_file("spec.yaml", config, &filter).await?;

// From a URL
let agent = agent.with_openapi_url("https://api.example.com/openapi.json", config, &filter).await?;

// From a string (sync)
let agent = agent.with_openapi_spec(&spec_string, config, &filter)?;
```

Or create adapters directly for more control:

```rust
let adapters = OpenApiToolAdapter::from_str(&spec, config, &OperationFilter::All)?;
let tools: Vec<Box<dyn AgentTool>> = adapters.into_iter().map(|a| Box::new(a) as _).collect();
```

## Configuration

`OpenApiConfig` controls auth, headers, timeouts, and response limits:

```rust
let config = OpenApiConfig::new()
    .with_base_url("https://api.staging.example.com") // Override spec's servers
    .with_bearer_token("sk-...")                       // Bearer auth
    .with_header("X-Custom", "value")                  // Extra headers
    .with_timeout_secs(60)                             // Request timeout
    .with_max_response_bytes(128 * 1024)               // Truncate large responses
    .with_name_prefix("github");                       // Tool names: github__listRepos
```

### Authentication

```rust
// Bearer token
let config = OpenApiConfig::new().with_bearer_token("token");

// API key in a custom header
let config = OpenApiConfig::new().with_api_key("X-API-Key", "key-value");

// No auth
let config = OpenApiConfig::new(); // default
```

## Filtering Operations

Most API specs have dozens or hundreds of operations. Use `OperationFilter` to select which ones become tools:

```rust
// All operations (default)
let filter = OperationFilter::All;

// Specific operations by ID
let filter = OperationFilter::ByOperationId(vec![
    "listRepos".into(),
    "getRepo".into(),
    "createIssue".into(),
]);

// All operations with a specific tag
let filter = OperationFilter::ByTag(vec!["repos".into()]);

// All operations under a path prefix
let filter = OperationFilter::ByPathPrefix("/repos".into());
```

## How It Works

Each OpenAPI operation becomes one `AgentTool`:

| AgentTool method | Mapped from |
|-----------------|-------------|
| `name()` | `operationId` (with optional prefix) |
| `label()` | `summary` or `operationId` |
| `description()` | `description` or `summary` |
| `parameters_schema()` | Combined JSON Schema from path/query/header params + request body |

When the LLM calls a tool, the adapter:

1. Substitutes path parameters in the URL (`/pets/{petId}` → `/pets/123`)
2. Adds query parameters as `?key=value`
3. Adds header parameters
4. Applies auth from config
5. Sends the request body as JSON (if the operation has one)
6. Returns the response text (with status code) to the LLM

Non-2xx responses are **not** treated as errors — they're returned as text so the LLM can reason about them and retry or adjust.

## Mixing with Other Tools

OpenAPI tools work alongside built-in tools and MCP tools:

```rust
use yoagent::tools::default_tools;

let agent = Agent::new(provider)
    .with_tools(default_tools())
    .with_openapi_file("github.yaml", github_config, &github_filter).await?
    .with_mcp_server_stdio("db-server", &[], None).await?;
```

## Limitations (v1)

- OpenAPI 3.0.x only (not 3.1.x)
- JSON request/response bodies only (no multipart/form-data)
- No OAuth2 or token refresh (pass tokens via `OpenApiConfig`)
- Operations without `operationId` are skipped
- Path-level `$ref` items are skipped
