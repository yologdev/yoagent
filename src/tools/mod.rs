//! Built-in tools. The filesystem and shell tools need a native host (the
//! `native` feature); `SharedStateTool` works everywhere.

#[cfg(feature = "native")]
pub mod bash;
#[cfg(feature = "native")]
pub mod edit;
#[cfg(feature = "native")]
pub mod file;
#[cfg(feature = "native")]
pub mod list;
pub mod sandbox;
#[cfg(feature = "native")]
pub mod search;
pub mod shared_state_tool;

#[cfg(feature = "native")]
pub use bash::BashTool;
#[cfg(feature = "native")]
pub use edit::EditFileTool;
#[cfg(feature = "native")]
pub use file::{ReadFileTool, WriteFileTool, DEFAULT_READ_MAX_LINES};
#[cfg(feature = "native")]
pub use list::ListFilesTool;
pub use sandbox::PathSandbox;
#[cfg(feature = "native")]
pub use search::SearchTool;
pub use shared_state_tool::SharedStateTool;

#[cfg(feature = "native")]
use crate::types::AgentTool;

/// Get the standard set of coding agent tools (native hosts only).
#[cfg(feature = "native")]
pub fn default_tools() -> Vec<Box<dyn AgentTool>> {
    vec![
        Box::new(BashTool::default()),
        Box::new(ReadFileTool::default()),
        Box::new(WriteFileTool::new()),
        Box::new(EditFileTool::new()),
        Box::new(ListFilesTool::default()),
        Box::new(SearchTool::default()),
    ]
}
