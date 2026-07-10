# Telemetry

yoagent instruments the loop with [`tracing`](https://docs.rs/tracing) **spans**
— structured, timed, nested units your observability stack can consume. With
no subscriber installed the overhead is near-zero (a cached per-callsite
interest check); nothing is exported unless you opt in.

## The span tree

```
agent_loop                (model)
└─ llm_stream             (turn, model, tokens_in, tokens_out, tokens_cached, cost_usd, error)
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
// opentelemetry_otlp 0.27+, opentelemetry_sdk 0.27+, tracing-opentelemetry 0.28+
use opentelemetry::trace::TracerProvider as _;
use tracing_subscriber::layer::SubscriberExt;

let exporter = opentelemetry_otlp::SpanExporter::builder()
    .with_tonic()
    .build()?;
let provider = opentelemetry_sdk::trace::SdkTracerProvider::builder()
    .with_batch_exporter(exporter)
    .build();

let subscriber = tracing_subscriber::registry()
    .with(tracing_opentelemetry::layer().with_tracer(provider.tracer("yoagent-app")));
tracing::subscriber::set_global_default(subscriber)?;
```

(The OTel crates rework their builder APIs between releases — if this snippet
drifts, the authoritative wiring is the
[`tracing-opentelemetry` docs](https://docs.rs/tracing-opentelemetry); yoagent
only emits standard `tracing` spans and does not depend on OTel.)

Because these are ordinary `tracing` spans, an agent call nests inside your
app's existing request traces (e.g. an axum handler span) automatically.

## What it buys you

- **Cost attribution** — dollars per turn/model in your dashboards, from the
  same `CostConfig` data as [`session_cost_usd`](../reference/api.md).
- **Latency diagnosis** — "40s request: 8s provider, 30s in one bash tool."
- **Audit** — which tools ran, with what outcome, per session.
