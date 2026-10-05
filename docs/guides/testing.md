# Testing Your Agent

An agent built on yoagent can be tested without a network or an API key:
`MockProvider` plays the model, returning scripted responses one turn at a
time, and everything else (the loop, your tools, middleware, events) runs for
real. The examples below are compiled and run as
[`tests/testing_guide_test.rs`](https://github.com/yologdev/yoagent/blob/main/tests/testing_guide_test.rs).

## A scripted run

Give `MockProvider` the turns you expect, pair it with `ModelConfig::mock()`,
and assert on the events or the final history:

```rust
use yoagent::provider::mock::{MockResponse, MockToolCall};
use yoagent::provider::{MockProvider, ModelConfig};
use yoagent::*;

#[tokio::test]
async fn the_agent_calls_the_tool_then_answers() {
    // Each response is one model turn, in order.
    let provider = MockProvider::new(vec![
        MockResponse::ToolCalls(vec![MockToolCall {
            name: "echo".into(),
            arguments: serde_json::json!({"text": "hi"}),
            provider_metadata: None,
        }]),
        MockResponse::Text("The tool said hi.".into()),
    ]);
    let mut agent =
        Agent::from_provider(provider, ModelConfig::mock()).with_tools(vec![Box::new(Echo)]);

    let mut rx = agent.prompt("say hi through the tool").await;
    let mut tool_output = None;
    while let Some(event) = rx.recv().await {
        if let AgentEvent::ToolExecutionEnd { result, .. } = event {
            tool_output = result.content.first().cloned();
        }
    }
    agent.finish().await;

    assert!(matches!(tool_output, Some(Content::Text { text }) if text == "hi"));
}
```

`Echo` is an ordinary `AgentTool`; the test file has it in full. For a single
text answer, `MockProvider::text("…")` is enough.

`MockProvider` also checks that every request pairs tool calls with their
results (no unanswered call, no stray result, nothing in between), the shape a
real provider would reject. On a violation it panics with an explanation;
under `Agent::prompt` that panic is in the spawned loop task, so the run ends
early and your assertions on its output fail. When a malformed sequence is
the point of the test, opt out:
`MockProvider::new(responses).without_transcript_validation()`.

## A middleware, without an agent

`ToolCallRequest::new(id, name, &args)` builds the request a
[`ToolMiddleware`](../concepts/tools.md) receives, so a policy can be tested
on its own:

```rust
struct NoRm;

#[async_trait::async_trait]
impl ToolMiddleware for NoRm {
    async fn before_tool(&self, call: &ToolCallRequest<'_>) -> ToolDecision {
        let command = call.args["command"].as_str().unwrap_or("");
        if call.tool_name == "bash" && command.contains("rm ") {
            ToolDecision::Deny("no deleting".into())
        } else {
            ToolDecision::Allow
        }
    }
}

#[tokio::test]
async fn the_middleware_denies_rm() {
    let args = serde_json::json!({"command": "rm -rf build"});
    let call = ToolCallRequest::new("call-1", "bash", &args);
    assert!(matches!(NoRm.before_tool(&call).await, ToolDecision::Deny(_)));
}
```

When the policy depends on what the user asked, give the request that
context: `.with_run_prompts(&prompts)` for the run's prompts, or
`.with_messages(&history)` for the whole conversation. A policy should read
`call.user_request_parts()`; `user_request()` is prose whose format may
change.

## Abort

`abort()` cancels the run; `finish()` waits for it and leaves the agent
usable. This shows the call pattern — with an instant mock the run may well
finish before the abort lands:

```rust
#[tokio::test]
async fn an_abort_ends_the_run() {
    let mut agent = Agent::from_provider(MockProvider::text("hello"), ModelConfig::mock());
    let mut rx = agent.prompt("hi").await;
    agent.abort();
    while rx.recv().await.is_some() {}
    agent.finish().await;
    assert!(!agent.is_streaming());
}
```

## Beyond the mock

- **Streaming and provider quirks** are tested against a local HTTP server
  (`wiremock`) serving hand-built bodies in each provider's wire format (SSE;
  binary event stream for Bedrock) — see `tests/*_stream_test.rs` in the
  repository.
- **Live runs** belong in an example run on demand, not in `cargo test`: the
  repository's `examples/release_smoke.rs` checks a real provider end to end.
