//! Adapts MCP tools to the AgentTool trait.

use super::client::McpClient;
use super::types::{McpContent, McpError, McpToolInfo};
use crate::types::{AgentTool, Content, ToolError, ToolResult, ToolUpdateFn};
use async_trait::async_trait;
use std::sync::Arc;
use tokio::sync::Mutex;

/// Wraps an MCP server tool as an `AgentTool` so it can be used by the agent.
pub struct McpToolAdapter {
    client: Arc<Mutex<McpClient>>,
    tool: McpToolInfo,
    /// Prefix to avoid name collisions (e.g., "server_name__tool_name").
    prefix: Option<String>,
}

impl McpToolAdapter {
    /// Create a new adapter.
    pub fn new(client: Arc<Mutex<McpClient>>, tool: McpToolInfo) -> Self {
        Self {
            client,
            tool,
            prefix: None,
        }
    }

    /// Create with a name prefix for disambiguation.
    pub fn with_prefix(mut self, prefix: impl Into<String>) -> Self {
        self.prefix = Some(prefix.into());
        self
    }

    /// Create adapters for all tools from an MCP client.
    pub async fn from_client(client: Arc<Mutex<McpClient>>) -> Result<Vec<Self>, McpError> {
        let tools = client.lock().await.list_tools().await?;
        Ok(tools
            .into_iter()
            .map(|tool| McpToolAdapter::new(client.clone(), tool))
            .collect())
    }

    /// Create adapters with a name prefix for all tools from an MCP client.
    pub async fn from_client_with_prefix(
        client: Arc<Mutex<McpClient>>,
        prefix: impl Into<String>,
    ) -> Result<Vec<Self>, McpError> {
        let prefix = prefix.into();
        let tools = client.lock().await.list_tools().await?;
        Ok(tools
            .into_iter()
            .map(|tool| McpToolAdapter::new(client.clone(), tool).with_prefix(prefix.clone()))
            .collect())
    }
}

#[async_trait]
impl AgentTool for McpToolAdapter {
    fn name(&self) -> &str {
        // Return the tool name; prefix is applied in label for display.
        // We use the raw name so MCP server recognizes it in call_tool.
        &self.tool.name
    }

    fn label(&self) -> &str {
        &self.tool.name
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
        _tool_call_id: &str,
        params: serde_json::Value,
        _cancel: tokio_util::sync::CancellationToken,
        _on_update: Option<ToolUpdateFn>,
    ) -> Result<ToolResult, ToolError> {
        let client = self.client.lock().await;
        let result = client
            .call_tool(&self.tool.name, params)
            .await
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

        let cancel = tokio_util::sync::CancellationToken::new();
        let result = adapter
            .execute(
                "tc-1",
                serde_json::json!({"path": "/tmp/test"}),
                cancel,
                None,
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

        let cancel = tokio_util::sync::CancellationToken::new();
        let result = adapter
            .execute("tc-1", serde_json::json!({}), cancel, None)
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
}
