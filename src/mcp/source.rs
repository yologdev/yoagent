//! An MCP server's tools as a [`ToolSource`]: asked again mid-run when the
//! server announces that its tool list changed.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use tokio::sync::Mutex;

use super::{McpClient, McpToolAdapter};
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
/// server that loads more tools when asked, the usual case) is seen right
/// after that call, and its tools are offered from the next request. One the
/// server sends while no call is open is counted at the next call to it.
///
/// A `tools/list` that fails gives no tools (logged): the server's tools are
/// withdrawn until a later listing succeeds.
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
    changed: Arc<AtomicU64>,
    prefix: Option<String>,
}

impl McpToolSource {
    /// A source over `client`'s tools, each bound by the client's
    /// [`call_timeout`](McpClient::call_timeout).
    pub fn new(client: McpClient) -> Self {
        let changed = client.tools_changed_counter();
        Self {
            client: Arc::new(Mutex::new(client)),
            changed,
            prefix: None,
        }
    }

    /// Name each tool `<prefix>__<tool>`, as
    /// [`McpToolAdapter::with_prefix`] does.
    pub fn with_prefix(mut self, prefix: impl Into<String>) -> Self {
        self.prefix = Some(prefix.into());
        self
    }

    /// The client, shared with the tools this source gives.
    pub fn client(&self) -> &Arc<Mutex<McpClient>> {
        &self.client
    }
}

#[cfg_attr(target_arch = "wasm32", async_trait::async_trait(?Send))]
#[cfg_attr(not(target_arch = "wasm32"), async_trait::async_trait)]
impl ToolSource for McpToolSource {
    async fn tools(&self) -> Vec<Arc<dyn AgentTool>> {
        let (listed, timeout) = {
            let client = self.client.lock().await;
            (client.list_tools().await, client.call_timeout())
        };
        match listed {
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
                tracing::warn!(
                    "MCP tool source: tools/list failed, offering none of its tools: {e}"
                );
                Vec::new()
            }
        }
    }

    fn generation(&self) -> u64 {
        self.changed.load(Ordering::SeqCst)
    }
}
