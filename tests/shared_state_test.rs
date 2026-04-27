//! Tests for SharedState and its integration with SubAgentTool.

use std::sync::Arc;
use tokio_util::sync::CancellationToken;
use yoagent::provider::mock::*;
use yoagent::provider::MockProvider;
use yoagent::shared_state::SharedState;
use yoagent::sub_agent::SubAgentTool;
use yoagent::*;

// ---------------------------------------------------------------------------
// Integration: parent stores a value, sub-agent reads it via shared_state tool
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_sub_agent_reads_shared_state() {
    let state = SharedState::new();
    state
        .set("artifact", "LINE1: build failed\nLINE2: exit code 1".into())
        .await
        .unwrap();

    // Sub-agent mock: first call issues shared_state get, second returns text
    let sub_provider = Arc::new(MockProvider::new(vec![
        MockResponse::ToolCalls(vec![MockToolCall {
            name: "shared_state".into(),
            provider_metadata: None,
            arguments: serde_json::json!({"action": "get", "key": "artifact"}),
        }]),
        MockResponse::Text("The build failed with exit code 1".into()),
    ]));

    let sub_agent = SubAgentTool::new("analyzer", sub_provider)
        .with_description("Analyzes artifacts")
        .with_system_prompt("Analyze the artifact.")
        .with_model("mock")
        .with_api_key("test")
        .with_shared_state(state.clone());

    let result = sub_agent
        .execute(
            serde_json::json!({"task": "What happened in the build?"}),
            ToolContext {
                tool_call_id: "tc-1".into(),
                tool_name: "analyzer".into(),
                cancel: CancellationToken::new(),
                on_update: None,
                on_progress: None,
            },
        )
        .await
        .expect("sub-agent should succeed");

    let text = match &result.content[0] {
        Content::Text { text } => text.as_str(),
        _ => panic!("Expected text content"),
    };
    assert!(text.contains("build failed"));
}

// ---------------------------------------------------------------------------
// Integration: sub-agent writes a value, parent reads it back
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_sub_agent_writes_shared_state() {
    let state = SharedState::new();

    // Sub-agent mock: sets a value then responds with text
    let sub_provider = Arc::new(MockProvider::new(vec![
        MockResponse::ToolCalls(vec![MockToolCall {
            name: "shared_state".into(),
            provider_metadata: None,
            arguments: serde_json::json!({
                "action": "set",
                "key": "summary",
                "value": "Root cause: OOM in test runner"
            }),
        }]),
        MockResponse::Text("Done, wrote summary.".into()),
    ]));

    let sub_agent = SubAgentTool::new("writer", sub_provider)
        .with_description("Writes summaries")
        .with_system_prompt("Summarize findings.")
        .with_model("mock")
        .with_api_key("test")
        .with_shared_state(state.clone());

    sub_agent
        .execute(
            serde_json::json!({"task": "Summarize"}),
            ToolContext {
                tool_call_id: "tc-1".into(),
                tool_name: "writer".into(),
                cancel: CancellationToken::new(),
                on_update: None,
                on_progress: None,
            },
        )
        .await
        .expect("sub-agent should succeed");

    // Parent reads back the value the sub-agent stored
    let summary = state.get("summary").await.expect("summary should exist");
    assert_eq!(summary, "Root cause: OOM in test runner");
}

// ---------------------------------------------------------------------------
// Integration: two parallel sub-agents share state
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_parallel_sub_agents_share_state() {
    let state = SharedState::new();
    state.set("input", "shared data".into()).await.unwrap();

    // Agent A reads then writes result_a
    let provider_a = Arc::new(MockProvider::new(vec![
        MockResponse::ToolCalls(vec![MockToolCall {
            name: "shared_state".into(),
            provider_metadata: None,
            arguments: serde_json::json!({"action": "get", "key": "input"}),
        }]),
        MockResponse::ToolCalls(vec![MockToolCall {
            name: "shared_state".into(),
            provider_metadata: None,
            arguments: serde_json::json!({"action": "set", "key": "result_a", "value": "from A"}),
        }]),
        MockResponse::Text("A done".into()),
    ]));

    // Agent B reads then writes result_b
    let provider_b = Arc::new(MockProvider::new(vec![
        MockResponse::ToolCalls(vec![MockToolCall {
            name: "shared_state".into(),
            provider_metadata: None,
            arguments: serde_json::json!({"action": "get", "key": "input"}),
        }]),
        MockResponse::ToolCalls(vec![MockToolCall {
            name: "shared_state".into(),
            provider_metadata: None,
            arguments: serde_json::json!({"action": "set", "key": "result_b", "value": "from B"}),
        }]),
        MockResponse::Text("B done".into()),
    ]));

    let agent_a = SubAgentTool::new("agent_a", provider_a)
        .with_system_prompt("You are agent A.")
        .with_model("mock")
        .with_api_key("test")
        .with_shared_state(state.clone());

    let agent_b = SubAgentTool::new("agent_b", provider_b)
        .with_system_prompt("You are agent B.")
        .with_model("mock")
        .with_api_key("test")
        .with_shared_state(state.clone());

    let ctx = || ToolContext {
        tool_call_id: "tc".into(),
        tool_name: "test".into(),
        cancel: CancellationToken::new(),
        on_update: None,
        on_progress: None,
    };

    // Run in parallel
    let (ra, rb) = tokio::join!(
        agent_a.execute(serde_json::json!({"task": "process"}), ctx()),
        agent_b.execute(serde_json::json!({"task": "process"}), ctx()),
    );
    ra.unwrap();
    rb.unwrap();

    assert_eq!(state.get("result_a").await, Some("from A".into()));
    assert_eq!(state.get("result_b").await, Some("from B".into()));
    assert_eq!(state.get("input").await, Some("shared data".into()));
}

// ---------------------------------------------------------------------------
// SubAgentTool without shared_state works as before
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_sub_agent_without_shared_state_unchanged() {
    let sub_provider = Arc::new(MockProvider::text("hello"));

    let sub_agent = SubAgentTool::new("plain", sub_provider)
        .with_system_prompt("You are plain.")
        .with_model("mock")
        .with_api_key("test");
    // No .with_shared_state() — existing behavior

    let result = sub_agent
        .execute(
            serde_json::json!({"task": "say hi"}),
            ToolContext {
                tool_call_id: "tc-1".into(),
                tool_name: "plain".into(),
                cancel: CancellationToken::new(),
                on_update: None,
                on_progress: None,
            },
        )
        .await
        .expect("should work without shared state");

    let text = match &result.content[0] {
        Content::Text { text } => text.as_str(),
        _ => panic!("Expected text"),
    };
    assert_eq!(text, "hello");
}

// ---------------------------------------------------------------------------
// System prompt includes shared state summary
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_shared_state_summary_in_system_prompt() {
    let state = SharedState::new();
    state.set("log", "x".repeat(2048)).await.unwrap();

    // We can't inspect the system prompt directly from outside, but we can
    // verify the sub-agent gets the shared_state tool by having it call list
    let sub_provider = Arc::new(MockProvider::new(vec![
        MockResponse::ToolCalls(vec![MockToolCall {
            name: "shared_state".into(),
            provider_metadata: None,
            arguments: serde_json::json!({"action": "list"}),
        }]),
        MockResponse::Text("Listed state".into()),
    ]));

    let sub_agent = SubAgentTool::new("lister", sub_provider)
        .with_system_prompt("List state.")
        .with_model("mock")
        .with_api_key("test")
        .with_shared_state(state);

    let result = sub_agent
        .execute(
            serde_json::json!({"task": "list"}),
            ToolContext {
                tool_call_id: "tc-1".into(),
                tool_name: "lister".into(),
                cancel: CancellationToken::new(),
                on_update: None,
                on_progress: None,
            },
        )
        .await
        .unwrap();

    let text = match &result.content[0] {
        Content::Text { text } => text.as_str(),
        _ => panic!("Expected text"),
    };
    assert_eq!(text, "Listed state");
}
