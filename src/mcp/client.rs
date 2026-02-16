//! High-level MCP client.

use super::transport::{HttpTransport, McpTransport, StdioTransport};
use super::types::*;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::Mutex;

/// High-level MCP client that manages connection lifecycle and protocol.
pub struct McpClient {
    transport: Arc<Mutex<Box<dyn McpTransport>>>,
    server_info: Option<ServerInfo>,
    capabilities: Option<ServerCapabilities>,
}

impl McpClient {
    /// Connect to an MCP server via stdio (spawn a child process).
    pub async fn connect_stdio(
        command: &str,
        args: &[&str],
        env: Option<HashMap<String, String>>,
    ) -> Result<Self, McpError> {
        let transport = StdioTransport::new(command, args, env).await?;
        let mut client = Self {
            transport: Arc::new(Mutex::new(Box::new(transport))),
            server_info: None,
            capabilities: None,
        };
        client.initialize().await?;
        Ok(client)
    }

    /// Connect to an MCP server via HTTP.
    pub async fn connect_http(url: &str) -> Result<Self, McpError> {
        let transport = HttpTransport::new(url)?;
        let mut client = Self {
            transport: Arc::new(Mutex::new(Box::new(transport))),
            server_info: None,
            capabilities: None,
        };
        client.initialize().await?;
        Ok(client)
    }

    /// Create from an existing transport (useful for testing).
    pub fn from_transport(transport: Box<dyn McpTransport>) -> Self {
        Self {
            transport: Arc::new(Mutex::new(transport)),
            server_info: None,
            capabilities: None,
        }
    }

    /// Initialize the MCP connection (handshake).
    pub async fn initialize(&mut self) -> Result<ServerInfo, McpError> {
        let params = serde_json::json!({
            "protocolVersion": "2024-11-05",
            "capabilities": {},
            "clientInfo": ClientInfo::default()
        });

        let request = JsonRpcRequest::new("initialize", Some(params));
        let response = self.send_request(request).await?;

        let result: InitializeResult = serde_json::from_value(response)?;
        self.server_info = Some(result.server_info.clone());
        self.capabilities = Some(result.capabilities);

        // Send initialized notification (no response expected, but we send it as a request
        // since our transport is request/response. Some servers ignore the id on notifications.)
        let notify = JsonRpcRequest::new("notifications/initialized", None);
        // Best-effort: ignore errors on the notification
        let _ = self.send_request(notify).await;

        Ok(result.server_info)
    }

    /// List available tools from the server.
    pub async fn list_tools(&self) -> Result<Vec<McpToolInfo>, McpError> {
        let request = JsonRpcRequest::new("tools/list", Some(serde_json::json!({})));
        let response = self.send_request(request).await?;

        let result: ToolsListResult = serde_json::from_value(response)?;
        Ok(result.tools)
    }

    /// Call a tool on the server.
    pub async fn call_tool(
        &self,
        name: &str,
        arguments: serde_json::Value,
    ) -> Result<McpToolCallResult, McpError> {
        let params = serde_json::json!({
            "name": name,
            "arguments": arguments
        });

        let request = JsonRpcRequest::new("tools/call", Some(params));
        let response = self.send_request(request).await?;

        let result: McpToolCallResult = serde_json::from_value(response)?;
        Ok(result)
    }

    /// Close the connection.
    pub async fn close(&self) -> Result<(), McpError> {
        self.transport.lock().await.close().await
    }

    /// Get server info (available after initialize).
    pub fn server_info(&self) -> Option<&ServerInfo> {
        self.server_info.as_ref()
    }

    /// Send a request and extract the result, handling errors.
    async fn send_request(&self, request: JsonRpcRequest) -> Result<serde_json::Value, McpError> {
        let transport = self.transport.lock().await;
        let response = transport.send(request).await?;

        if let Some(error) = response.error {
            return Err(McpError::JsonRpc {
                code: error.code,
                message: error.message,
            });
        }

        response
            .result
            .ok_or_else(|| McpError::Protocol("Response has neither result nor error".into()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Integration test would require a running MCP server.
    // Unit tests for the client logic are covered via mock transport in tool_adapter tests.

    #[test]
    fn test_client_info_default() {
        let info = ClientInfo::default();
        assert_eq!(info.name, "yoagent");
        assert!(!info.version.is_empty());
    }
}
