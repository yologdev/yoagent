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

yoagent currently has no optional feature flags — all providers and tools are included by default.
