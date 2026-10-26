//! An MCP server's tools as a [`ToolSource`]: asked again mid-run when the
//! server announces that its tool list changed.

use std::sync::Arc;

use tokio::sync::Mutex;

use super::{ExitWatch, McpClient, McpToolAdapter, ServerNotifications};
use crate::tool_source::ToolSource;
use crate::types::AgentTool;

/// An MCP server's tools as a [`ToolSource`], for a server
/// whose tools change: it lists them at the start of each run and, whenever
/// the server sends `notifications/tools/list_changed`, again before the
/// run's next model request (see [`tool_source`](crate::tool_source)).
///
/// Install with [`Agent::with_tool_source`](crate::Agent::with_tool_source)
/// (or a [`SubAgentTool`](crate::SubAgentTool)'s). Unlike
/// [`Agent::with_mcp_server_stdio`](crate::Agent::with_mcp_server_stdio),
/// whose tools are listed once and fixed, the tools follow the server.
///
/// **When a change is seen.** The transport reads the server's notifications
/// while it waits for a call's answer — so a change a tool call caused (a
/// server that loads more tools when asked, the usual case), announced before
/// that call's answer, is seen when the call returns, and its tools are
/// offered from the next request. One announced after the answer is seen at
/// the next call (stdio) or not mid-run (HTTP):
/// - Over **stdio**, one the server sends while no call is open is counted at
///   the next call to it.
/// - Over **HTTP**, only one that arrives on a call's SSE response, ahead of
///   its answer, is counted: the transport opens no standalone `GET` stream,
///   so a change announced while idle — or by a server that answers in plain
///   JSON — is not seen mid-run (the next run lists the tools again).
/// - A **custom** transport is seen only if it implements
///   [`McpTransport::report_notifications`](crate::mcp::McpTransport::report_notifications);
///   otherwise the source logs it when built and lists once per run.
///
/// **A server that exits.** Over stdio, a server that exits on its own —
/// a crash, between calls or during one — is seen at the next turn: its
/// tools are withdrawn (the source gives none from then on, logged once),
/// so the model stops calling a dead server. Restarting it is the host's
/// job ([`McpClient::exited`] tells why it went); a new client needs a new
/// source. Over HTTP there is no process to watch.
///
/// **Failures.** The listing is bounded by the client's
/// [`call_timeout`](McpClient::call_timeout) — five minutes over stdio; over
/// HTTP by the transport's idle read timeout instead; a custom transport's
/// by nothing but the run's cancel. A listing that fails or times
/// out (logged) keeps the last list that succeeded — the server's tools stay
/// offered, and a call to one reports the server's real error — or, with no
/// earlier list, gives none. It is asked again at the server's next change.
///
/// ```no_run
/// # #[cfg(feature = "native")]
/// # async fn example() -> Result<(), yoagent::mcp::McpError> {
/// use yoagent::mcp::{McpClient, McpToolSource};
/// let client = McpClient::connect_stdio("my-mcp-server", &[], None).await?;
/// # let provider = yoagent::provider::MockProvider::text("ok");
/// let agent = yoagent::Agent::from_provider(provider, yoagent::provider::ModelConfig::mock())
///     .with_tool_source(McpToolSource::new(client).with_prefix("srv"));
/// # let _ = agent;
/// # Ok(())
/// # }
/// ```
pub struct McpToolSource {
    client: Arc<Mutex<McpClient>>,
    notifications: ServerNotifications,
    exit: Option<ExitWatch>,
    /// The exit was logged (once).
    exit_logged: std::sync::atomic::AtomicBool,
    prefix: Option<String>,
    /// The last list that succeeded, kept when a later listing fails.
    last: std::sync::Mutex<Vec<Arc<dyn AgentTool>>>,
}

impl McpToolSource {
    /// A source over `client`'s tools, each bound by the client's
    /// [`call_timeout`](McpClient::call_timeout).
    ///
    /// A client whose transport does not report notifications
    /// ([`McpClient::reports_notifications`] is `false`, a custom transport)
    /// is logged: its tools are then listed once per run, never mid-run.
    pub fn new(client: McpClient) -> Self {
        if !client.reports_notifications() {
            tracing::warn!(
                "MCP tool source: the client's transport does not report server \
                 notifications; its tools are listed once per run, never mid-run"
            );
        }
        let notifications = client.notifications();
        let exit = client.exit_watch();
        Self {
            client: Arc::new(Mutex::new(client)),
            notifications,
            exit,
            exit_logged: std::sync::atomic::AtomicBool::new(false),
            prefix: None,
            last: std::sync::Mutex::new(Vec::new()),
        }
    }

    /// Name each tool `<prefix>__<tool>`, as
    /// [`McpToolAdapter::with_prefix`] does.
    pub fn with_prefix(mut self, prefix: impl Into<String>) -> Self {
        self.prefix = Some(prefix.into());
        self
    }

    /// Why the server has gone, if it has (see [`McpClient::exited`]).
    fn exited(&self) -> Option<String> {
        self.exit.as_ref()?.exited()
    }

    /// The client, shared with the tools this source gives. Use it to call
    /// the server directly; do not replace the client behind the lock — the
    /// source keeps reading the notifications of the one it was built with,
    /// so a swapped-in client's changes would never be seen.
    pub fn client(&self) -> &Arc<Mutex<McpClient>> {
        &self.client
    }
}

#[cfg_attr(target_arch = "wasm32", async_trait::async_trait(?Send))]
#[cfg_attr(not(target_arch = "wasm32"), async_trait::async_trait)]
impl ToolSource for McpToolSource {
    async fn tools(&self) -> Vec<Arc<dyn AgentTool>> {
        if let Some(why) = self.exited() {
            if !self
                .exit_logged
                .swap(true, std::sync::atomic::Ordering::SeqCst)
            {
                tracing::warn!(
                    "MCP tool source: the server has gone; its tools are withdrawn: {why}"
                );
            }
            // Not the last good list: its tools would only fail.
            self.last.lock().unwrap_or_else(|p| p.into_inner()).clear();
            return Vec::new();
        }
        let (listed, timeout) = {
            let client = self.client.lock().await;
            let timeout = client.call_timeout();
            let listed = match timeout {
                // Asked between turns: a server that stops answering must not
                // hang the run.
                Some(limit) => crate::rt::timeout(limit, client.list_tools())
                    .await
                    .unwrap_or_else(|_| {
                        Err(super::McpError::Transport(format!(
                            "tools/list did not answer within {limit:?}"
                        )))
                    }),
                None => client.list_tools().await,
            };
            (listed, timeout)
        };
        let tools: Vec<Arc<dyn AgentTool>> = match listed {
            Ok(tools) => tools
                .into_iter()
                .map(|tool| {
                    let adapter =
                        McpToolAdapter::new(self.client.clone(), tool).with_call_timeout(timeout);
                    let adapter = match &self.prefix {
                        Some(prefix) => adapter.with_prefix(prefix.clone()),
                        None => adapter,
                    };
                    Arc::new(adapter) as Arc<dyn AgentTool>
                })
                .collect(),
            Err(e) => {
                let last = self.last.lock().unwrap_or_else(|p| p.into_inner()).clone();
                tracing::warn!(
                    kept = last.len(),
                    "MCP tool source: tools/list failed; keeping the last list that succeeded: {e}"
                );
                return last;
            }
        };
        *self.last.lock().unwrap_or_else(|p| p.into_inner()) = tools.clone();
        tools
    }

    /// The server's `notifications/tools/list_changed` so far, plus one
    /// once it has exited (its tools are then withdrawn).
    fn generation(&self) -> u64 {
        self.notifications.tools_list_changed() + u64::from(self.exited().is_some())
    }
}
