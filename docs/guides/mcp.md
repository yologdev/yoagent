# MCP Integration

## What is MCP?

The [Model Context Protocol (MCP)](https://modelcontextprotocol.io) is a JSON-RPC 2.0 protocol that lets AI agents discover and call tools from external servers. It defines a standard way for agents to connect to tool providers over two transports:

- **Stdio** — spawn a child process, communicate via stdin/stdout (newline-delimited JSON)
- **HTTP** — POST JSON-RPC requests to an HTTP endpoint, including the
  request/response subset of Streamable HTTP

## Connecting to MCP Servers

### Stdio Transport

> Needs the default `native` feature: not available with
> `default-features = false` or on wasm32.

Use `with_mcp_server_stdio()` to spawn an MCP server process and register its tools:

```rust
use yoagent::Agent;
use yoagent::provider::ModelConfig;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut agent = Agent::from_config(ModelConfig::anthropic("claude-sonnet-5", "Claude Sonnet 5"))
        .with_system_prompt("You are a helpful assistant with file access.")
        .with_mcp_server_stdio(
            "npx",
            &["-y", "@modelcontextprotocol/server-filesystem", "/tmp"],
            None,
        )
        .await?;

    let rx = agent.prompt("List files in /tmp").await;
    // handle events...
    Ok(())
}
```

You can pass environment variables to the server process:

```rust
use std::collections::HashMap;

let mut env = HashMap::new();
env.insert("API_TOKEN".into(), "secret".into());

let agent = Agent::from_config(ModelConfig::anthropic("claude-sonnet-5", "Claude Sonnet 5"))
    .with_mcp_server_stdio("my-mcp-server", &["--port", "0"], Some(env))
    .await?;
```

How the stdio transport behaves:

- Each request waits for the response carrying its own id. Messages the server
  sends in between are handled: notifications (`notifications/message`,
  progress, `list_changed`) are skipped, a `ping` is answered, and any other
  server request (sampling, roots) is declined with "method not found" (logged
  at `WARN`) so the server does not wait. A response for another id (a call
  that timed out earlier) is dropped. Server messages are only read while a
  call is in progress, so a `ping` sent between calls is answered with the
  next call.
- A line on stdout that is not JSON (a banner, a debug print) is skipped and
  logged at `WARN`; if the server then closes, the error mentions it.
- `notifications/initialized` is sent as a notification, without an id, and not
  awaited. If it cannot be delivered the connect fails. The whole handshake is
  bounded at 60 seconds.
- The server's stderr is read continuously, whatever its encoding, and logged
  at `DEBUG` (target `yoagent::mcp::stderr`), so a server that logs heavily
  cannot block. The last few KB are kept: when the server exits, the error
  carries its exit status and that stderr, which is usually where it says why.
- Dropping the client ends the server process.

### HTTP Transport

For remote MCP servers exposed over HTTP. This works on every target,
including wasm32 / Cloudflare Workers (over the host's `fetch`):

```rust
let agent = Agent::from_config(ModelConfig::anthropic("claude-sonnet-5", "Claude Sonnet 5"))
    .with_mcp_server_http("http://localhost:8080/mcp")
    .await?;
```

`HttpTransport` handles both the plain JSON-RPC-over-POST shape and the
**request/response subset of Streamable HTTP** — servers that answer a POST with
an SSE-framed response, whether or not they then close the stream:

- Responses framed as `text/event-stream` are parsed out of their SSE frames,
  joining each event's `data:` lines as the SSE spec requires.
- A server may interleave `notifications/progress` and `notifications/message`
  frames ahead of the result — that is how it reports progress during a
  `tools/call`. Those are skipped: a frame is this request's response only if it
  carries no `method`, carries a `result` or an `error`, and its id matches.
- Requests advertise `Accept: application/json, text/event-stream`, letting the
  server pick its framing.
- An `Mcp-Session-Id` returned by the server is captured and replayed on every
  later request, and released with a `DELETE` on `McpClient::close()`. Servers
  that reject `DELETE` are tolerated — teardown is best-effort. A `404` on a
  session-bearing request clears the session and reports that it expired, so a
  caller can rebuild the client.
- `202 Accepted` (or `204`) with an empty body — how a notification is
  acknowledged — is a success, not a parse failure. Any *other* empty 2xx is
  reported as an error, since it usually means a proxy answered instead of the
  MCP server.
- The body is parsed incrementally, so a call returns at the blank-line-terminated
  frame carrying its response rather than at end-of-stream. A server that holds
  the POST stream open after answering does not block it. Two trade-offs come
  with that: returning mid-body forgoes connection reuse on native targets (a
  fresh connection, and TLS handshake, on the next call to such a server), and a plain JSON-RPC
  body has no frames to return early at, so it is read to the end as before.
- A stalled server — one that accepts the POST then sends nothing — is bounded
  by an idle read timeout (120s) rather than hanging. The timer resets on every
  read, so a long `tools/call` streaming progress frames is never cut off.
  (Native targets only. On wasm32 the fetch-based client has no read timeout;
  the host's own request limits apply.)

**Not supported:** the `GET` server→client stream and `Last-Event-ID`
resumability. `McpTransport` is `send`/`close` only, with nowhere to deliver a
server-initiated message — supporting them would mean growing the trait an
inbound channel. Notifications arriving on the POST stream *before* the response
are read and skipped; any that trail it are not, since the call has already
returned — so a server that blocks awaiting a reply to a `sampling/createMessage`
it sent on this stream will time out rather than be answered. Note also that the
handshake still negotiates `protocolVersion: 2024-11-05` (the revision predating
Streamable HTTP), which servers generally accept.

`McpClient::close()` is what sends the `DELETE`. `Agent::with_mcp_server_http`
does not call it, so sessions opened that way are released by the server's own
timeout rather than explicitly.

## How MCP Tools Work

When you call `with_mcp_server_stdio()` or `with_mcp_server_http()`, yoagent:

1. Connects to the MCP server and performs the `initialize` handshake
2. Calls `tools/list` to discover available tools
3. Wraps each MCP tool as an `AgentTool` via `McpToolAdapter`
4. Adds them to the agent's tool list

MCP tools appear alongside built-in tools. The LLM sees them with their original names, descriptions, and JSON Schema parameters — it can call them just like any other tool.

Each call ends when the run is cancelled. Over **stdio** it is also bounded at
five minutes (`McpToolAdapter::DEFAULT_CALL_TIMEOUT`), counted from when the
call is sent, not while it waits behind another call to the same server. Over
**HTTP** there is no whole-call bound — the transport's idle timeout already
ends a stalled call, and a long one that keeps streaming must not be cut off.
For a different bound, build the client yourself:

```rust
use std::time::Duration;
use yoagent::mcp::{McpClient, McpToolAdapter};

let client = McpClient::connect_stdio("my-server", &[], None)
    .await?
    .with_call_timeout(Some(Duration::from_secs(30 * 60)));
let tools = McpToolAdapter::from_client(Arc::new(Mutex::new(client))).await?;
let agent = agent.with_tools(tools.into_iter().map(|t| Box::new(t) as _).collect());
```

A call that times out may still be carried out by the server; the error says
so. Content kinds this crate does not model reach the model as text: a text
`resource` as its own text, anything else (`audio`, a `blob` resource, …) as
its JSON with large base64 payloads replaced by their size.

## Mixing Built-in and MCP Tools

```rust
use yoagent::tools::default_tools;

let agent = Agent::from_config(ModelConfig::anthropic("claude-sonnet-5", "Claude Sonnet 5"))
    .with_tools(default_tools())  // bash, read, write, edit, list, search
    .with_mcp_server_stdio("my-db-server", &[], None)
    .await?;
// Agent now has both built-in coding tools AND MCP database tools
```

## Using the MCP Client Directly

For lower-level control, use `McpClient` directly:

```rust
use yoagent::mcp::{McpClient, McpToolAdapter};
use std::sync::Arc;
use tokio::sync::Mutex;

let client = McpClient::connect_stdio("my-server", &[], None).await?;
let tools = client.list_tools().await?;

for tool in &tools {
    println!("{}: {}", tool.name, tool.description.as_deref().unwrap_or(""));
}

// Call a tool directly
let result = client.call_tool("read_file", serde_json::json!({"path": "/tmp/test.txt"})).await?;

// Or wrap as AgentTool adapters
let client = Arc::new(Mutex::new(client));
let adapters = McpToolAdapter::from_client(client).await?;
```

## Error Handling

MCP operations return `McpError`:

- `McpError::Transport` — connection or I/O failure
- `McpError::Protocol` — unexpected response format
- `McpError::JsonRpc` — server returned a JSON-RPC error
- `McpError::Serialization` — a message could not be (de)serialized as JSON
- `McpError::Io` — an I/O error, e.g. on the stdio pipes
- `McpError::ConnectionClosed` — the connection ended (the stdio transport
  now reports a server that exited as `McpError::Transport("Connection closed:
  MCP server '…' exited with …; last stderr: …")`, carrying the reason)

When an MCP tool returns `isError: true`, the adapter converts it to a `ToolError::Failed`, which the agent loop sends back to the LLM with `is_error: true` so it can self-correct.
