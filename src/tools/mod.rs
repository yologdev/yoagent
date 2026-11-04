//! Built-in tools. The filesystem and shell tools need a native host (the
//! `native` feature); `SharedStateTool` works everywhere.

#[cfg(feature = "native")]
#[cfg_attr(docsrs, doc(cfg(feature = "native")))]
pub mod bash;
#[cfg(feature = "native")]
#[cfg_attr(docsrs, doc(cfg(feature = "native")))]
pub mod edit;
#[cfg(feature = "native")]
#[cfg_attr(docsrs, doc(cfg(feature = "native")))]
pub mod file;
#[cfg(feature = "native")]
#[cfg_attr(docsrs, doc(cfg(feature = "native")))]
pub mod list;
pub mod sandbox;
#[cfg(feature = "native")]
#[cfg_attr(docsrs, doc(cfg(feature = "native")))]
pub mod search;
pub mod shared_state_tool;

#[cfg(feature = "native")]
#[cfg_attr(docsrs, doc(cfg(feature = "native")))]
pub use bash::BashTool;
#[cfg(feature = "native")]
#[cfg_attr(docsrs, doc(cfg(feature = "native")))]
pub use edit::EditFileTool;
#[cfg(feature = "native")]
#[cfg_attr(docsrs, doc(cfg(feature = "native")))]
pub use file::{ReadFileTool, ReadLog, WriteFileTool, DEFAULT_READ_MAX_LINES};
#[cfg(feature = "native")]
#[cfg_attr(docsrs, doc(cfg(feature = "native")))]
pub use list::ListFilesTool;
pub use sandbox::PathSandbox;
#[cfg(feature = "native")]
#[cfg_attr(docsrs, doc(cfg(feature = "native")))]
pub use search::SearchTool;
pub use shared_state_tool::SharedStateTool;

#[cfg(feature = "native")]
use crate::types::AgentTool;

/// Get the standard set of coding agent tools (native hosts only).
///
/// `read_file` and `write_file` share a fresh [`ReadLog`], so `write_file`
/// refuses to overwrite a file that `read_file` returned only part of. Call
/// this once per agent: each call has its own log.
#[cfg(feature = "native")]
#[cfg_attr(docsrs, doc(cfg(feature = "native")))]
pub fn default_tools() -> Vec<Box<dyn AgentTool>> {
    let read_log = ReadLog::new();
    vec![
        Box::new(BashTool::default()),
        Box::new(ReadFileTool::default().with_read_log(read_log.clone())),
        Box::new(WriteFileTool::new().with_read_log(read_log)),
        Box::new(EditFileTool::new()),
        Box::new(ListFilesTool::default()),
        Box::new(SearchTool::default()),
    ]
}
