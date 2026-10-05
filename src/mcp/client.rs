//! High-level MCP client.

#[cfg(feature = "native")]
use super::transport::StdioTransport;
use super::transport::{HttpTransport, McpTransport};
use super::types::*;
#[cfg(feature = "native")]
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;

/// How long `connect_*` waits for the `initialize` handshake.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(60);

/// High-level MCP client that manages connection lifecycle and protocol.
pub struct McpClient {
    transport: Arc<Mutex<Box<dyn McpTransport>>>,
    server_info: Option<ServerInfo>,
    capabilities: Option<ServerCapabilities>,
    call_timeout: Option<Duration>,
}

impl McpClient {
    /// Connect to an MCP server via stdio (spawn a child process). Native hosts only.
    #[cfg(feature = "native")]
    #[cfg_attr(docsrs, doc(cfg(feature = "native")))]
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
            // Nothing else bounds a stdio call: a server that stops answering
            // would hang the agent.
            call_timeout: Some(crate::mcp::McpToolAdapter::DEFAULT_CALL_TIMEOUT),
        };
        client.handshake().await?;
        Ok(client)
    }

    /// Connect to an MCP server via HTTP.
    pub async fn connect_http(url: &str) -> Result<Self, McpError> {
        let transport = HttpTransport::new(url)?;
        let mut client = Self {
            transport: Arc::new(Mutex::new(Box::new(transport))),
            server_info: None,
            capabilities: None,
            // The HTTP transport's idle read timeout already ends a stalled
            // call; a long one that keeps streaming must not be cut off.
            call_timeout: None,
        };
        client.handshake().await?;
        Ok(client)
    }

    /// Create from an existing transport (useful for testing).
    pub fn from_transport(transport: Box<dyn McpTransport>) -> Self {
        Self {
            transport: Arc::new(Mutex::new(transport)),
            server_info: None,
            capabilities: None,
            call_timeout: None,
        }
    }

    /// The bound tool adapters built from this client apply to each call
    /// ([`McpToolAdapter::from_client`](crate::mcp::McpToolAdapter::from_client)):
    /// five minutes over stdio, none over HTTP (its idle timeout already ends
    /// a stalled call) or a custom transport.
    pub fn call_timeout(&self) -> Option<Duration> {
        self.call_timeout
    }

    /// Change the bound adapters built from this client apply to each call
    /// (`None`: no bound; the run's cancellation still ends a call).
    pub fn with_call_timeout(mut self, timeout: Option<Duration>) -> Self {
        self.call_timeout = timeout;
        self
    }

    /// `initialize`, bounded: a server that never answers fails the connect
    /// instead of hanging it.
    async fn handshake(&mut self) -> Result<ServerInfo, McpError> {
        crate::rt::timeout(HANDSHAKE_TIMEOUT, self.initialize())
            .await
            .unwrap_or_else(|_| {
                Err(McpError::Transport(format!(
                    "MCP server did not complete the initialize handshake within {}s",
                    HANDSHAKE_TIMEOUT.as_secs()
                )))
            })
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

        // The handshake ends with a notification, which gets no response.
        // (It used to be sent as a request and awaited: a spec-compliant
        // server never answers it, so connecting over stdio could hang.)
        // A server that never got it has not finished the handshake, so a
        // failure here fails the connect (over stdio it means the server is
        // gone) instead of surfacing later as a puzzling tools/list error.
        let initialized = JsonRpcNotification::new("notifications/initialized", None);
        self.transport.lock().await.notify(initialized).await?;

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
