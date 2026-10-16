//! Adapts MCP tools to the AgentTool trait.

use super::client::McpClient;
use super::types::{McpContent, McpError, McpToolInfo};
use crate::types::{AgentTool, Content, ToolContext, ToolError, ToolResult};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;

/// Wraps an MCP server tool as an `AgentTool` so it can be used by the agent.
pub struct McpToolAdapter {
    client: Arc<Mutex<McpClient>>,
    tool: McpToolInfo,
    /// The name the agent and the model see: the server's tool name, or
    /// `<prefix>__<tool>` with [`with_prefix`](Self::with_prefix). Calls to
    /// the server always use the server's own name.
    name: String,
    call_timeout: Option<Duration>,
}

impl McpToolAdapter {
    /// How long one call may take by default, waiting for the server
    /// included: five minutes. A stalled server fails the call instead of
    /// hanging the agent.
    pub const DEFAULT_CALL_TIMEOUT: Duration = Duration::from_secs(300);

    /// Create a new adapter, with no call timeout (the run's cancellation
    /// still ends a call). [`from_client`](Self::from_client) applies the
    /// client's default instead.
    pub fn new(client: Arc<Mutex<McpClient>>, tool: McpToolInfo) -> Self {
        Self {
            client,
            name: tool.name.clone(),
            tool,
            call_timeout: None,
        }
    }

    /// Bound each call (`None`: no bound). The clock starts once the call is
    /// sent, not while it waits behind another call to the same server. The
    /// run's cancellation ends a call either way.
    pub fn with_call_timeout(mut self, timeout: Option<Duration>) -> Self {
        self.call_timeout = timeout;
        self
    }

    /// Name the tool `<prefix>__<tool>` for the agent and the model (as
    /// `OpenApiConfig::with_name_prefix` does for OpenAPI tools), so two
    /// servers offering a tool of the same name, or a server tool named like
    /// a built-in, do not collide. The server is still called with its own
    /// tool name. A second call replaces the prefix. Provider limits on tool
    /// names apply to the result: keep the prefix to letters, digits, `_` and
    /// `-`, and short (several providers cap names at 64 characters); neither
    /// the prefix nor the server's name is checked here.
    pub fn with_prefix(mut self, prefix: impl Into<String>) -> Self {
        self.name = format!("{}__{}", prefix.into(), self.tool.name);
        self
    }

    /// Create adapters for all tools from an MCP client.
    ///
    /// Each adapter gets the client's call timeout
    /// ([`McpClient::call_timeout`]).
    pub async fn from_client(client: Arc<Mutex<McpClient>>) -> Result<Vec<Self>, McpError> {
        let (tools, timeout) = {
            let c = client.lock().await;
            (c.list_tools().await?, c.call_timeout())
        };
        Ok(tools
            .into_iter()
            .map(|tool| McpToolAdapter::new(client.clone(), tool).with_call_timeout(timeout))
            .collect())
    }

    /// Create adapters for all tools from an MCP client, each named
    /// `<prefix>__<tool>` ([`with_prefix`](Self::with_prefix)).
    pub async fn from_client_with_prefix(
        client: Arc<Mutex<McpClient>>,
        prefix: impl Into<String>,
    ) -> Result<Vec<Self>, McpError> {
        let prefix = prefix.into();
        let (tools, timeout) = {
            let c = client.lock().await;
            (c.list_tools().await?, c.call_timeout())
        };
        Ok(tools
            .into_iter()
            .map(|tool| {
                McpToolAdapter::new(client.clone(), tool)
                    .with_prefix(prefix.clone())
                    .with_call_timeout(timeout)
            })
            .collect())
    }
}

#[cfg_attr(target_arch = "wasm32", async_trait::async_trait(?Send))]
#[cfg_attr(not(target_arch = "wasm32"), async_trait::async_trait)]
impl AgentTool for McpToolAdapter {
    fn name(&self) -> &str {
        // Prefixed when a prefix was given; `execute` calls the server with
        // the server's own name (`self.tool.name`).
        &self.name
    }

    fn label(&self) -> &str {
        &self.name
    }

    fn description(&self) -> &str {
        self.tool
            .description
            .as_deref()
            .unwrap_or("MCP tool (no description)")
    }

    fn parameters_schema(&self) -> serde_json::Value {
        if self.tool.input_schema.is_null() {
            serde_json::json!({"type": "object", "properties": {}})
        } else {
            self.tool.input_schema.clone()
        }
    }

    async fn execute(
        &self,
        params: serde_json::Value,
        ctx: ToolContext,
    ) -> Result<ToolResult, ToolError> {
        // Wait for the client (another call to this server may hold it): only
        // cancellation ends the wait, the call timeout has not started.
        let client = tokio::select! {
            _ = ctx.cancel.cancelled() => return Err(ToolError::Cancelled),
            client = self.client.lock() => client,
        };
        let call = client.call_tool(&self.tool.name, params);
        let bounded = async {
            match self.call_timeout {
                Some(limit) => crate::rt::timeout(limit, call).await.map_err(|_| {
                    ToolError::Failed(format!(
                        "MCP call '{}' got no answer within {limit:?}. The request was sent, so the server may still carry it out: check before retrying anything that is not safe to repeat.",
                        self.tool.name
                    ))
                }),
                None => Ok(call.await),
            }
        };
        let result = tokio::select! {
            _ = ctx.cancel.cancelled() => return Err(ToolError::Cancelled),
            outcome = bounded => outcome?,
        }
        .map_err(|e| ToolError::Failed(format!("MCP call failed: {}", e)))?;

        if result.is_error {
            let error_text = result
                .content
                .iter()
                .filter_map(|c| match c {
                    McpContent::Text { text } => Some(text.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("\n");
            return Err(ToolError::Failed(error_text));
        }

        let content: Vec<Content> = result
            .content
            .into_iter()
            .map(|c| match c {
                McpContent::Text { text } => Content::Text { text },
                McpContent::Image { data, mime_type } => Content::Image { data, mime_type },
            })
            .collect();

        Ok(ToolResult {
            content,
            details: serde_json::Value::Null,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mcp::transport::McpTransport;
    use crate::mcp::types::*;
    use async_trait::async_trait;

    /// A mock transport that returns predefined responses.
    struct MockTransport {
        responses: std::sync::Mutex<Vec<JsonRpcResponse>>,
    }

    impl MockTransport {
        fn new(responses: Vec<JsonRpcResponse>) -> Self {
            Self {
                responses: std::sync::Mutex::new(responses),
            }
        }
    }

    #[async_trait]
    impl McpTransport for MockTransport {
        async fn send(&self, _request: JsonRpcRequest) -> Result<JsonRpcResponse, McpError> {
            let mut responses = self.responses.lock().unwrap();
            if responses.is_empty() {
                Err(McpError::ConnectionClosed)
            } else {
                Ok(responses.remove(0))
            }
        }

        async fn close(&self) -> Result<(), McpError> {
            Ok(())
        }
    }

    fn ok_response(id: u64, result: serde_json::Value) -> JsonRpcResponse {
        JsonRpcResponse {
            jsonrpc: "2.0".into(),
            id: Some(id),
            result: Some(result),
            error: None,
        }
    }

    #[tokio::test]
    async fn test_tool_adapter_wraps_mcp_tool() {
        let tool_info = McpToolInfo {
            name: "read_file".into(),
            description: Some("Read a file from disk".into()),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string"}
                },
                "required": ["path"]
            }),
        };

        // Mock: initialize response, initialized notification response, tools/call response
        let transport = MockTransport::new(vec![
            // tools/call response
            ok_response(
                1,
                serde_json::json!({
                    "content": [{"type": "text", "text": "file contents"}],
                    "isError": false
                }),
            ),
        ]);

        let client = McpClient::from_transport(Box::new(transport));
        let client = Arc::new(Mutex::new(client));

        let adapter = McpToolAdapter::new(client, tool_info);

        assert_eq!(adapter.name(), "read_file");
        assert_eq!(adapter.description(), "Read a file from disk");

        let schema = adapter.parameters_schema();
        assert_eq!(schema["type"], "object");

        let result = adapter
            .execute(
                serde_json::json!({"path": "/tmp/test"}),
                ToolContext {
                    tool_call_id: "tc-1".into(),
                    tool_name: "read_file".into(),
                    cancel: tokio_util::sync::CancellationToken::new(),
                    on_update: None,
                    on_progress: None,
                    sub_agent_report: None,
                    delegation: Default::default(),
                },
            )
            .await
            .unwrap();

        assert_eq!(result.content.len(), 1);
        if let Content::Text { text } = &result.content[0] {
            assert_eq!(text, "file contents");
        } else {
            panic!("Expected text content");
        }
    }

    #[tokio::test]
    async fn test_tool_adapter_handles_error() {
        let tool_info = McpToolInfo {
            name: "fail_tool".into(),
            description: None,
            input_schema: serde_json::Value::Null,
        };

        let transport = MockTransport::new(vec![ok_response(
            1,
            serde_json::json!({
                "content": [{"type": "text", "text": "something went wrong"}],
                "isError": true
            }),
        )]);

        let client = McpClient::from_transport(Box::new(transport));
        let client = Arc::new(Mutex::new(client));

        let adapter = McpToolAdapter::new(client, tool_info);
        assert_eq!(adapter.description(), "MCP tool (no description)");

        let result = adapter
            .execute(
                serde_json::json!({}),
                ToolContext {
                    tool_call_id: "tc-1".into(),
                    tool_name: "fail_tool".into(),
                    cancel: tokio_util::sync::CancellationToken::new(),
                    on_update: None,
                    on_progress: None,
                    sub_agent_report: None,
                    delegation: Default::default(),
                },
            )
            .await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_from_client_creates_adapters() {
        // Mock: list_tools response
        let transport = MockTransport::new(vec![ok_response(
            1,
            serde_json::json!({
                "tools": [
                    {"name": "tool_a", "description": "Tool A", "inputSchema": {"type": "object"}},
                    {"name": "tool_b", "description": "Tool B", "inputSchema": {"type": "object"}}
                ]
            }),
        )]);

        let client = McpClient::from_transport(Box::new(transport));
        let client = Arc::new(Mutex::new(client));

        let adapters = McpToolAdapter::from_client(client).await.unwrap();
        assert_eq!(adapters.len(), 2);
        assert_eq!(adapters[0].name(), "tool_a");
        assert_eq!(adapters[1].name(), "tool_b");
    }

    /// Records each request and answers from a queue.
    struct RecordingTransport {
        sent: Arc<std::sync::Mutex<Vec<JsonRpcRequest>>>,
        responses: std::sync::Mutex<Vec<JsonRpcResponse>>,
    }

    #[async_trait]
    impl McpTransport for RecordingTransport {
        async fn send(&self, request: JsonRpcRequest) -> Result<JsonRpcResponse, McpError> {
            self.sent.lock().unwrap().push(request);
            let mut responses = self.responses.lock().unwrap();
            if responses.is_empty() {
                Err(McpError::ConnectionClosed)
            } else {
                Ok(responses.remove(0))
            }
        }

        async fn close(&self) -> Result<(), McpError> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn a_prefix_names_the_tool_and_the_server_still_gets_its_own_name() {
        // Two servers offering a tool of the same name.
        let mut adapters = Vec::new();
        let mut sent = Vec::new();
        for server in ["docs", "web"] {
            let log = Arc::new(std::sync::Mutex::new(Vec::new()));
            let transport = RecordingTransport {
                sent: log.clone(),
                responses: std::sync::Mutex::new(vec![
                    ok_response(
                        1,
                        serde_json::json!({ "tools": [{ "name": "search", "inputSchema": {"type": "object"} }] }),
                    ),
                    ok_response(
                        2,
                        serde_json::json!({ "content": [{"type": "text", "text": server}], "isError": false }),
                    ),
                ]),
            };
            let client = McpClient::from_transport(Box::new(transport))
                .with_call_timeout(Some(Duration::from_secs(7)));
            adapters.extend(
                McpToolAdapter::from_client_with_prefix(Arc::new(Mutex::new(client)), server)
                    .await
                    .unwrap(),
            );
            sent.push(log);
        }
        let names: Vec<&str> = adapters.iter().map(|a| a.name()).collect();
        assert_eq!(names, ["docs__search", "web__search"]);
        // The client's call timeout is carried over, prefix or not.
        assert!(adapters
            .iter()
            .all(|a| a.call_timeout == Some(Duration::from_secs(7))));
        assert_eq!(adapters[1].label(), "web__search");

        let ctx = ToolContext::new("tc-1", "web__search");
        let result = adapters[1]
            .execute(serde_json::json!({"q": "x"}), ctx)
            .await
            .unwrap();
        assert!(matches!(&result.content[0], Content::Text { text } if text == "web"));
        // The server was called with its own tool name, not the prefixed one.
        let calls = sent[1].lock().unwrap();
        let call = calls
            .iter()
            .find(|r| r.method == "tools/call")
            .expect("a tools/call");
        assert_eq!(call.params.as_ref().unwrap()["name"], "search");
        // ...and only the server it belongs to.
        assert!(!sent[0]
            .lock()
            .unwrap()
            .iter()
            .any(|r| r.method == "tools/call"));

        // Without a prefix, the server's name.
        let tool = McpToolInfo {
            name: "search".into(),
            description: None,
            input_schema: serde_json::Value::Null,
        };
        let client = Arc::new(Mutex::new(McpClient::from_transport(Box::new(
            MockTransport::new(vec![]),
        ))));
        let plain = McpToolAdapter::new(client, tool);
        assert_eq!(plain.name(), "search");
        // A second prefix replaces the first; prefixes never stack.
        assert_eq!(plain.with_prefix("a").with_prefix("b").name(), "b__search");
    }
}
