# Installation

## Requirements

- Rust 2021 edition (1.56+, recommended 1.75+)
- Tokio async runtime

## Add to Cargo.toml

```toml
[dependencies]
yo-agent = { git = "https://github.com/yologdev/yo-agent.git" }
```

## Dependencies

yo-agent brings in these key dependencies automatically:

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

yo-agent currently has no optional feature flags — all providers and tools are included by default.
