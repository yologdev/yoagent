# Telemetry

yoagent instruments the loop with [`tracing`](https://docs.rs/tracing) **spans**
— structured, timed, nested units your observability stack can consume. With
no subscriber installed they compile to no-ops; there is zero overhead unless
you opt in.

## The span tree

```
agent_loop                (model)
└─ llm_stream             (turn, model, tokens_in, tokens_out, tokens_cached, cost_usd)
└─ tool                   (tool, tool_call_id, is_error)
```

- `llm_stream` — one per turn, wrapping the provider call. Token counts are
  recorded from real usage; `cost_usd` is recorded when the `ModelConfig` has
  pricing configured (`CostConfig`).
- `tool` — one per tool execution, with the tool name and error status;
  duration comes free with the span.

## Local: print spans

```rust
tracing_subscriber::fmt()
    .with_span_events(tracing_subscriber::fmt::format::FmtSpan::CLOSE)
    .init();
```

Run `cargo run --example telemetry` to see it.

## Production: OpenTelemetry

The OTel bridge is **application-side** — yoagent needs no OTel dependency
(that's the point of `tracing`). Install the
[`tracing-opentelemetry`](https://docs.rs/tracing-opentelemetry) layer and the
same spans flow to any OTLP backend — Datadog, Grafana Tempo, Honeycomb,
Jaeger:

```rust
use opentelemetry_otlp::WithExportConfig;
use tracing_subscriber::layer::SubscriberExt;

let tracer = opentelemetry_otlp::new_pipeline()
    .tracing()
    .with_exporter(opentelemetry_otlp::new_exporter().tonic())
    .install_batch(opentelemetry_sdk::runtime::Tokio)?;

let subscriber = tracing_subscriber::registry()
    .with(tracing_opentelemetry::layer().with_tracer(tracer));
tracing::subscriber::set_global_default(subscriber)?;
```

Because these are ordinary `tracing` spans, an agent call nests inside your
app's existing request traces (e.g. an axum handler span) automatically.

## What it buys you

- **Cost attribution** — dollars per turn/model in your dashboards, from the
  same `CostConfig` data as [`session_cost_usd`](../reference/api.md).
- **Latency diagnosis** — "40s request: 8s provider, 30s in one bash tool."
- **Audit** — which tools ran, with what outcome, per session.
