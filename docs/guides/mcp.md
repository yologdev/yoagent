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
  progress) are skipped — `notifications/tools/list_changed` is counted (see
  *Servers Whose Tools Change*) — a `ping` is answered, and any other
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
- Dropping the client ends the server process — the command itself. What it
  started (`npx` → `node`) lives on if it ignores its closed input, unless the
  server runs in a process group of its own:
  `McpClient::connect_stdio_with(command, args, env, StdioOptions::new().own_process_group(true))`
  (Unix). Then `close()` and drop kill the whole group, and a Ctrl+C in the
  host's terminal no longer reaches the server; but a host that dies without
  dropping its clients leaves it running.

### HTTP Transport

For remote MCP servers exposed over HTTP. This works on every target,
including wasm32 / Cloudflare Workers (over the host's `fetch`):

```rust
let agent = Agent::from_config(ModelConfig::anthropic("claude-sonnet-5", "Claude Sonnet 5"))
    .with_mcp_server_http("http://localhost:8080/mcp")
    .await?;
```

#### Servers that need headers

A server that wants a token, an API key or a `User-Agent` gets them from
`HttpTransport::with_header`, then connects with
`Agent::with_mcp_server_http_transport` (or `McpClient::connect_http_with`):

```rust
use yoagent::mcp::HttpTransport;

let token = std::env::var("MCP_TOKEN")?;
let transport = HttpTransport::new("https://mcp.example.com/mcp")?
    .with_header("authorization", format!("Bearer {token}"))?
    .with_header("user-agent", "my-app/1.0")?;
let agent = agent.with_mcp_server_http_transport(transport).await?;
```

- The headers go on every request of the session: `initialize`, the
  `initialized` notification, tool calls and the closing `DELETE`.
- Setting a name again replaces its value. Values are marked sensitive, so they
  stay out of debug output, and a refused value is never echoed in the error.
- An invalid name, or a value that is not visible ASCII, is an error from
  `with_header`, before anything is sent. So is a header the transport or the
  HTTP client sets itself: `Accept`, `Content-Type`, `Mcp-Session-Id`,
  `Content-Length`, `Transfer-Encoding`, `Connection`, `Host`.
- On wasm32 the headers ride on the host's `fetch`. Cloudflare Workers send
  them; a browser may drop some, such as `User-Agent`.
- The headers are fixed for the transport's lifetime. A token that expires
  during a session (OAuth refresh) needs a new transport and client.

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
resumability: the transport reads the server only while a call is answered.
Notifications arriving on the POST stream *before* the response are read —
`notifications/tools/list_changed` is counted (see *Servers Whose Tools
Change*), the rest skipped; any that trail it are not, since the call has already
returned — so a server that blocks awaiting a reply to a `sampling/createMessage`
it sent on this stream will time out rather than be answered. Note also that the
handshake still negotiates `protocolVersion: 2024-11-05` (the revision predating
Streamable HTTP), which servers generally accept.

`McpClient::close()` is what sends the `DELETE`. `Agent::with_mcp_server_http`
does not call it, so sessions opened that way are released by the server's own
timeout rather than explicitly.

## Servers Whose Tools Change

`with_mcp_server_*` lists a server's tools once, when you connect. For a
server whose tools change — one that loads more tools when asked, or adds and
removes them as a session goes on — install it as a
[`ToolSource`](../concepts/tools.md#tools-that-change-at-runtime-toolsource)
instead, with `McpToolSource`:

```rust,no_run
use yoagent::mcp::{McpClient, McpToolSource};

let client = McpClient::connect_stdio("my-mcp-server", &[], None).await?;
let agent = Agent::from_config(config)
    .with_tool_source(McpToolSource::new(client).with_prefix("srv"));
```

It lists the server's tools at the start of every run and, whenever the
server sends `notifications/tools/list_changed`, again before the run's next
model request — the same run sees the new tools. The transports read the
server's notifications while they wait for a call's answer, so a change a
tool call caused (the usual case), announced before that call's answer, is
seen when the call returns (`McpClient::tools_list_changes` tells how many
were read). One announced at another time:

- over **stdio**, a change announced while no call is open is counted at the
  next call to the server;
- over **HTTP**, only a notification that arrives on a call's SSE response,
  ahead of its answer, is counted. The transport opens no standalone `GET`
  stream, so a change announced while idle — or by a server that answers in
  plain JSON — is not seen mid-run; the next run lists the tools again;
- over a **custom** `McpTransport`, only if it implements
  `report_notifications` (record each notification's method on the
  `ServerNotifications` handle it is given, and return `true`). By default it
  does not: `McpClient::reports_notifications()` is `false`, `McpToolSource`
  logs a warning when built, and the tools are listed once per run.

The listing is bounded by the client's call timeout over stdio (five
minutes), by the idle read timeout over HTTP, and by nothing but the run's
cancel over a custom transport. One that fails or times out keeps the last
list that succeeded (logged), so the server's tools stay offered and a call
reports the server's real error.

## Servers That Exit

A stdio server can crash between calls. `McpClient::exited()` tells, without
waiting — `None` while the server runs; once it has gone, its exit status
and the last of its stderr:

```rust,no_run
if let Some(why) = client.exited() {
    eprintln!("MCP server stopped: {why}");
}
```

It never waits behind a call in flight, so a host can poll it (to show a
notice, or to restart the server — yoagent does not restart servers). It
answers from the first poll that sees the exit; the text settles within half
a second of that (it gains the last of the server's stderr), then stays the
same. It also
reports a server whose output closed while a wrapper process lives on, one
that no longer reads its input (a write to it failed), and one that cannot be
waited on. A server stopped with `close()` reads as
"was closed", not as a crash, so a restart policy can tell the two apart.

An `McpToolSource` uses it on its own: a server that exits is seen at the next
turn and its tools are withdrawn (logged once, naming the server and its
prefix), so the model stops calling a dead server — a later call to one of
its tools is answered as an unknown tool. A new client needs a new source.

`with_mcp_server_stdio` gets neither: it lists the tools once and keeps no
client handle. Connect an `McpClient` yourself and install it with
`McpToolSource` to have the tools withdrawn and `exited()` to poll.

Over HTTP there is no process to watch: `exited()` is always `None`. A custom
`McpTransport` can provide one by implementing `exit_watch` (returning an
`ExitWatch::new(check)`, where `check` must not block).

## How MCP Tools Work

When you call `with_mcp_server_stdio()` or `with_mcp_server_http()`, yoagent:

1. Connects to the MCP server and performs the `initialize` handshake
2. Calls `tools/list` to discover available tools
3. Wraps each MCP tool as an `AgentTool` via `McpToolAdapter`
4. Adds them to the agent's tool list

MCP tools appear alongside built-in tools. The LLM sees them with their original names, descriptions, and JSON Schema parameters — it can call them just like any other tool.

**Two servers with the same tool name.** The agent's tools are not de-duplicated, and a provider rejects two tools with one name — so two servers that both offer `search`, or a server tool named like a built-in (`bash`, `read_file`), break the request. Give such a server's tools a prefix by wrapping them yourself (the `with_mcp_server_*` helpers add none):

```rust
use std::sync::Arc;
use tokio::sync::Mutex;
use yoagent::mcp::{McpClient, McpToolAdapter};
use yoagent::AgentTool;

let docs = Arc::new(Mutex::new(McpClient::connect_stdio("docs-server", &[], None).await?));
let mut tools = yoagent::tools::default_tools();
for tool in McpToolAdapter::from_client_with_prefix(docs, "docs").await? {
    // The model sees `docs__search`; the server is still called with `search`.
    tools.push(Box::new(tool) as Box<dyn AgentTool>);
}
// `with_tools` replaces the agent's list: pass every tool in one call.
let agent = agent.with_tools(tools);
```

Each call ends when the run is cancelled. Over **stdio** it is also bounded at
five minutes (`McpToolAdapter::DEFAULT_CALL_TIMEOUT`), counted from when the
call is sent, not while it waits behind another call to the same server. Over
**HTTP** there is no whole-call bound — the transport's idle timeout already
ends a stalled call, and a long one that keeps streaming must not be cut off.
For a different bound, build the client yourself:

```rust
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;
use yoagent::mcp::{McpClient, McpToolAdapter};
use yoagent::tools::default_tools;
use yoagent::AgentTool;

let client = McpClient::connect_stdio("my-server", &[], None)
    .await?
    .with_call_timeout(Some(Duration::from_secs(30 * 60)));
let mcp_tools = McpToolAdapter::from_client(Arc::new(Mutex::new(client))).await?;

// `with_tools` replaces the agent's tools, so pass everything it should have.
let mut tools = default_tools();
tools.extend(mcp_tools.into_iter().map(|t| Box::new(t) as Box<dyn AgentTool>));
let agent = agent.with_tools(tools);
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

If a server's tools are named like the built-ins (`bash`, `read_file`, `search`, …), prefix them (see above).

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
