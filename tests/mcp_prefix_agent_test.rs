//! Two MCP servers offering a tool of the same name, prefixed, in one agent:
//! the model sees two distinct names, and a call reaches only its server,
//! under the server's own tool name.

use std::sync::{Arc, Mutex as StdMutex};
use tokio::sync::Mutex;
use yoagent::mcp::types::{JsonRpcRequest, JsonRpcResponse};
use yoagent::mcp::{McpClient, McpError, McpToolAdapter, McpTransport};
use yoagent::provider::mock::{MockResponse, MockToolCall};
use yoagent::provider::{MockProvider, ModelConfig};
use yoagent::*;

/// Records each request; answers `tools/list` with one `search` tool and
/// `tools/call` with this server's name.
struct Server {
    name: &'static str,
    sent: Arc<StdMutex<Vec<JsonRpcRequest>>>,
}

#[async_trait::async_trait]
impl McpTransport for Server {
    async fn send(&self, request: JsonRpcRequest) -> Result<JsonRpcResponse, McpError> {
        let result = match request.method.as_str() {
            "tools/list" => serde_json::json!({
                "tools": [{ "name": "search", "inputSchema": { "type": "object" } }]
            }),
            "tools/call" => serde_json::json!({
                "content": [{ "type": "text", "text": self.name }], "isError": false
            }),
            other => panic!("unexpected {other}"),
        };
        let id = request.id;
        self.sent.lock().unwrap().push(request);
        Ok(JsonRpcResponse {
            jsonrpc: "2.0".into(),
            id: Some(id),
            result: Some(result),
            error: None,
        })
    }

    async fn close(&self) -> Result<(), McpError> {
        Ok(())
    }
}

#[tokio::test]
async fn two_prefixed_servers_in_one_agent() {
    let mut tools: Vec<Box<dyn AgentTool>> = Vec::new();
    let mut logs = Vec::new();
    for name in ["docs", "web"] {
        let sent = Arc::new(StdMutex::new(Vec::new()));
        let client = McpClient::from_transport(Box::new(Server {
            name,
            sent: sent.clone(),
        }));
        for tool in McpToolAdapter::from_client_with_prefix(Arc::new(Mutex::new(client)), name)
            .await
            .unwrap()
        {
            tools.push(Box::new(tool));
        }
        logs.push(sent);
    }
    let names: Vec<&str> = tools.iter().map(|t| t.name()).collect();
    assert_eq!(names, ["docs__search", "web__search"]);

    let provider = MockProvider::new(vec![
        MockResponse::ToolCalls(vec![MockToolCall {
            name: "web__search".into(),
            arguments: serde_json::json!({ "q": "x" }),
            provider_metadata: None,
        }]),
        MockResponse::Text("done".into()),
    ]);
    let mut agent = Agent::from_provider(provider, ModelConfig::mock()).with_tools(tools);
    let mut rx = agent.prompt("search the web").await;
    let mut started = Vec::new();
    while let Some(event) = rx.recv().await {
        if let AgentEvent::ToolExecutionStart { tool_name, .. } = event {
            started.push(tool_name);
        }
    }
    agent.finish().await;
    assert_eq!(started, ["web__search"]);

    // The result came from the web server...
    let result = agent
        .messages()
        .iter()
        .find_map(|m| match m {
            AgentMessage::Llm(Message::ToolResult { content, .. }) => match content.first() {
                Some(Content::Text { text }) => Some(text.clone()),
                _ => None,
            },
            _ => None,
        })
        .expect("a tool result");
    assert_eq!(result, "web");
    // ...which was called with its own tool name; the docs server never was.
    let calls = |i: usize| {
        logs[i]
            .lock()
            .unwrap()
            .iter()
            .filter(|r| r.method == "tools/call")
            .map(|r| r.params.as_ref().unwrap()["name"].clone())
            .collect::<Vec<_>>()
    };
    assert_eq!(calls(1), [serde_json::json!("search")]);
    assert!(calls(0).is_empty());
}
