# Installation

## Requirements

- Rust 1.86+ (2021 edition)
- Tokio async runtime

## Add to Cargo.toml

```toml
[dependencies]
yoagent = "0.21"
tokio = { version = "1", features = ["full"] }
```

## Dependencies

yoagent brings in these key dependencies automatically:

| Crate | Purpose |
|-------|---------|
| `tokio` | Async runtime (full features) |
| `serde` / `serde_json` | Serialization |
| `reqwest` | HTTP client for provider APIs |
| `reqwest-eventsource` | SSE streaming |
| `async-trait` | Async trait support |
| `tokio-util` | `CancellationToken` |
| `thiserror` | Error types |
| `tracing` | Logging |

## Feature Flags

All providers and built-in tools are included by default. Optional features:

| Feature | Dependencies | Description |
|---------|-------------|-------------|
| `native` (default) | Tokio `fs`/`process`, reqwest's default features (TLS, HTTP/2, system proxy) + SOCKS | Filesystem and shell tools, stdio MCP, disk-backed shared state and price cache. Disable it only to build for [WebAssembly / Cloudflare Workers](../guides/wasm-workers.md): a native build without it has no TLS, so HTTPS provider calls fail at runtime |
| `openapi` | `openapiv3`, `serde_yaml_ng` | Auto-generate tools from OpenAPI 3.0 specs |
| `gasp` | `yoagent-state` | Record runs into a [GASP](../concepts/gasp.md) agent repo |
| `decision` | none | [Decision models](../concepts/decision-models.md): typed yes/no, choice and score questions; advisory hints, the tool gate and the input guard. Nothing is sent until you pick a model |

Enable in `Cargo.toml`:

```toml
[dependencies]
yoagent = { version = "0.21", features = ["openapi", "decision"] }
```
