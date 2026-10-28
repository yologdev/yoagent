//! High-level MCP client.

use super::transport::{ExitWatch, HttpTransport, McpTransport, ServerNotifications};
#[cfg(feature = "native")]
use super::transport::{StdioOptions, StdioTransport};
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
    /// The server notifications the transport has read.
    notifications: ServerNotifications,
    /// Whether the transport reports them at all.
    reports_notifications: bool,
    /// Tells that the server has gone, without the transport's lock.
    exit: Option<ExitWatch>,
}

impl McpClient {
    /// The one place a client is built: hands the transport its
    /// [`ServerNotifications`] handle.
    fn build(mut transport: Box<dyn McpTransport>, call_timeout: Option<Duration>) -> Self {
        let notifications = ServerNotifications::new();
        let reports_notifications = transport.report_notifications(notifications.clone());
        let exit = transport.exit_watch();
        Self {
            transport: Arc::new(Mutex::new(transport)),
            server_info: None,
            capabilities: None,
            call_timeout,
            notifications,
            reports_notifications,
            exit,
        }
    }

    /// Connect to an MCP server via stdio (spawn a child process). Native hosts only.
    #[cfg(feature = "native")]
    #[cfg_attr(docsrs, doc(cfg(feature = "native")))]
    pub async fn connect_stdio(
        command: &str,
        args: &[&str],
        env: Option<HashMap<String, String>>,
    ) -> Result<Self, McpError> {
        Self::connect_stdio_with(command, args, env, StdioOptions::default()).await
    }

    /// [`connect_stdio`](Self::connect_stdio), started as `options` says: a
    /// process group of its own ([`StdioOptions::own_process_group`]), say.
    #[cfg(feature = "native")]
    #[cfg_attr(docsrs, doc(cfg(feature = "native")))]
    pub async fn connect_stdio_with(
        command: &str,
        args: &[&str],
        env: Option<HashMap<String, String>>,
        options: StdioOptions,
    ) -> Result<Self, McpError> {
        let transport = StdioTransport::with_options(command, args, env, options).await?;
        // Nothing else bounds a stdio call: a server that stops answering
        // would hang the agent.
        let mut client = Self::build(
            Box::new(transport),
            Some(crate::mcp::McpToolAdapter::DEFAULT_CALL_TIMEOUT),
        );
        client.handshake().await?;
        Ok(client)
    }

    /// Connect to an MCP server via HTTP.
    pub async fn connect_http(url: &str) -> Result<Self, McpError> {
        Self::connect_http_with(HttpTransport::new(url)?).await
    }

    /// Connect over a configured [`HttpTransport`] — with request headers
    /// ([`HttpTransport::with_header`]), say. The handshake and timeouts are
    /// those of [`connect_http`](Self::connect_http).
    pub async fn connect_http_with(transport: HttpTransport) -> Result<Self, McpError> {
        // The HTTP transport's idle read timeout already ends a stalled call;
        // a long one that keeps streaming must not be cut off.
        let mut client = Self::build(Box::new(transport), None);
        client.handshake().await?;
        Ok(client)
    }

    /// Create from an existing transport (useful for testing).
    pub fn from_transport(transport: Box<dyn McpTransport>) -> Self {
        Self::build(transport, None)
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

    /// How many times the server has announced that its tool list changed
    /// (`notifications/tools/list_changed`), as read so far. The transport
    /// reads the server's notifications while it waits for a call's answer:
    /// over stdio, one sent while no call is open is counted at the next call;
    /// over HTTP, only one on a call's SSE response ahead of its answer is
    /// counted (no standalone `GET` stream); a custom transport counts only
    /// if it implements [`McpTransport::report_notifications`]. Also the
    /// generation of a [`McpToolSource`](crate::mcp::McpToolSource) on this
    /// client.
    pub fn tools_list_changes(&self) -> u64 {
        self.notifications.tools_list_changed()
    }

    /// Whether the transport reports the server's notifications — `false`
    /// for a custom transport that does not implement
    /// [`McpTransport::report_notifications`], whose tool-list changes are
    /// then never seen.
    pub fn reports_notifications(&self) -> bool {
        self.reports_notifications
    }

    /// Whether the server has gone, without waiting: `None` while it runs —
    /// and always over HTTP or a custom transport without an
    /// [`exit_watch`](McpTransport::exit_watch) — else why, from the first
    /// poll that sees it. Over stdio the text settles within 0.5 s of being
    /// first seen (it gains the last of the server's stderr), then stays the
    /// same:
    /// - `… exited with <status>` (a code, or the signal that killed it),
    ///   then the last of its stderr;
    /// - `… closed its output (its process is still running)`: a call found
    ///   its output closed, and the process still running half a second
    ///   later (a wrapper outliving the server);
    /// - `… has gone (exit status unknown: …)`: the process cannot be waited
    ///   on (`SIGCHLD` ignored, or reaped elsewhere);
    /// - `… was closed (`close`)` after [`close`](Self::close): not a crash,
    ///   so a host that restarts crashed servers can tell.
    ///
    /// It never waits behind a call in flight, so a host can poll it — to
    /// tell its user a server stopped between calls, or to restart it.
    pub fn exited(&self) -> Option<String> {
        self.exit.as_ref()?.exited()
    }

    pub(crate) fn exit_watch(&self) -> Option<ExitWatch> {
        self.exit.clone()
    }

    pub(crate) fn notifications(&self) -> ServerNotifications {
        self.notifications.clone()
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
