# Installation

## Requirements

- Rust 2021 edition (1.56+, recommended 1.75+)
- Tokio async runtime

## Add to Cargo.toml

```toml
[dependencies]
yoagent = "0.5"
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
| `openapi` | `openapiv3`, `serde_yaml` | Auto-generate tools from OpenAPI 3.0 specs |

Enable in `Cargo.toml`:

```toml
[dependencies]
yoagent = { version = "0.5", features = ["openapi"] }
```
