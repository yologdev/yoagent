//! Telemetry example: yoagent emits `tracing` spans for the loop, each LLM
//! stream (with token/cost fields), and each tool execution.
//!
//! Run with: cargo run --example telemetry
//!
//! Here spans are printed with the fmt subscriber; in production, install a
//! `tracing-opentelemetry` layer instead and the same spans flow to any OTLP
//! backend (Datadog, Grafana Tempo, Honeycomb, Jaeger, ...).

use yoagent::provider::{MockProvider, ModelConfig};
use yoagent::*;

#[tokio::main]
async fn main() {
    // Print spans with timing on close. Swap this for an OTel layer in prod.
    tracing_subscriber::fmt()
        .with_span_events(tracing_subscriber::fmt::format::FmtSpan::CLOSE)
        .with_target(false)
        .init();

    let mut agent = Agent::from_provider(
        MockProvider::text("Here is the answer."),
        ModelConfig::mock(),
    );

    let mut rx = agent.prompt("What is the answer?").await;
    while rx.recv().await.is_some() {}
    agent.finish().await;

    println!("done — spans above show agent_loop / llm_stream timings");
}
