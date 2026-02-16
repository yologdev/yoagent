//! MCP (Model Context Protocol) client support.
//!
//! Connect to MCP tool servers and use their tools seamlessly within yoagent.
//!
//! # Example
//!
//! ```rust,no_run
//! use yoagent::mcp::McpClient;
//!
//! # async fn example() -> Result<(), yoagent::mcp::McpError> {
//! // Connect to an MCP server via stdio
//! let client = McpClient::connect_stdio("npx", &["-y", "@modelcontextprotocol/server-filesystem", "/tmp"], None).await?;
//! # Ok(())
//! # }
//! ```

pub mod client;
pub mod tool_adapter;
pub mod transport;
pub mod types;

pub use client::McpClient;
pub use tool_adapter::McpToolAdapter;
pub use transport::{HttpTransport, McpTransport, StdioTransport};
pub use types::{McpContent, McpError, McpToolCallResult, McpToolInfo, ServerInfo};
