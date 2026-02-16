//! File tools — read and write files with safety limits.

use crate::types::*;
use async_trait::async_trait;

/// Read a file's contents. Supports line range for large files.
pub struct ReadFileTool {
    /// Max file size to read (prevents OOM)
    pub max_bytes: usize,
}

impl Default for ReadFileTool {
    fn default() -> Self {
        Self {
            max_bytes: 1024 * 1024, // 1MB
        }
    }
}

impl ReadFileTool {
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait]
impl AgentTool for ReadFileTool {
    fn name(&self) -> &str {
        "read_file"
    }

    fn label(&self) -> &str {
        "Read File"
    }

    fn description(&self) -> &str {
        "Read a file's contents. Optionally specify offset (1-indexed line) and limit (number of lines) for large files."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "File path to read"
                },
                "offset": {
                    "type": "integer",
                    "description": "Starting line number (1-indexed, optional)"
                },
                "limit": {
                    "type": "integer",
                    "description": "Maximum number of lines to return (optional)"
                }
            },
            "required": ["path"]
        })
    }

    async fn execute(
        &self,
        _tool_call_id: &str,
        params: serde_json::Value,
        cancel: tokio_util::sync::CancellationToken,
    ) -> Result<ToolResult, ToolError> {
        let path = params["path"]
            .as_str()
            .ok_or_else(|| ToolError::InvalidArgs("missing 'path' parameter".into()))?;

        if cancel.is_cancelled() {
            return Err(ToolError::Cancelled);
        }

        // Check file exists and size
        let metadata = tokio::fs::metadata(path)
            .await
            .map_err(|e| ToolError::Failed(format!("Cannot access {}: {}", path, e)))?;

        if metadata.len() as usize > self.max_bytes {
            return Err(ToolError::Failed(format!(
                "File too large ({} bytes, max {}). Use offset/limit for partial reads.",
                metadata.len(),
                self.max_bytes
            )));
        }

        let content = tokio::fs::read_to_string(path)
            .await
            .map_err(|e| ToolError::Failed(format!("Cannot read {}: {}", path, e)))?;

        let offset = params["offset"].as_u64().map(|v| v.max(1) as usize);
        let limit = params["limit"].as_u64().map(|v| v as usize);

        let output = match (offset, limit) {
            (Some(off), Some(lim)) => {
                let lines: Vec<&str> = content.lines().collect();
                let start = (off - 1).min(lines.len());
                let end = (start + lim).min(lines.len());
                let total = lines.len();
                let slice = lines[start..end].join("\n");
                format!("[Lines {}-{} of {}]\n{}", start + 1, end, total, slice)
            }
            (Some(off), None) => {
                let lines: Vec<&str> = content.lines().collect();
                let start = (off - 1).min(lines.len());
                let total = lines.len();
                let slice = lines[start..].join("\n");
                format!("[Lines {}-{} of {}]\n{}", start + 1, total, total, slice)
            }
            (None, Some(lim)) => {
                let lines: Vec<&str> = content.lines().collect();
                let end = lim.min(lines.len());
                let total = lines.len();
                let slice = lines[..end].join("\n");
                format!("[Lines 1-{} of {}]\n{}", end, total, slice)
            }
            (None, None) => content,
        };

        Ok(ToolResult {
            content: vec![Content::Text { text: output }],
            details: serde_json::json!({ "path": path }),
        })
    }
}

// ---------------------------------------------------------------------------

/// Write content to a file. Creates parent directories if needed.
pub struct WriteFileTool;

impl WriteFileTool {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl AgentTool for WriteFileTool {
    fn name(&self) -> &str {
        "write_file"
    }

    fn label(&self) -> &str {
        "Write File"
    }

    fn description(&self) -> &str {
        "Write content to a file. Creates the file if it doesn't exist, overwrites if it does. Creates parent directories automatically."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "File path to write"
                },
                "content": {
                    "type": "string",
                    "description": "Content to write to the file"
                }
            },
            "required": ["path", "content"]
        })
    }

    async fn execute(
        &self,
        _tool_call_id: &str,
        params: serde_json::Value,
        cancel: tokio_util::sync::CancellationToken,
    ) -> Result<ToolResult, ToolError> {
        let path = params["path"]
            .as_str()
            .ok_or_else(|| ToolError::InvalidArgs("missing 'path' parameter".into()))?;
        let content = params["content"]
            .as_str()
            .ok_or_else(|| ToolError::InvalidArgs("missing 'content' parameter".into()))?;

        if cancel.is_cancelled() {
            return Err(ToolError::Cancelled);
        }

        // Create parent directories
        if let Some(parent) = std::path::Path::new(path).parent() {
            if !parent.exists() {
                tokio::fs::create_dir_all(parent)
                    .await
                    .map_err(|e| ToolError::Failed(format!("Cannot create directory: {}", e)))?;
            }
        }

        tokio::fs::write(path, content)
            .await
            .map_err(|e| ToolError::Failed(format!("Cannot write {}: {}", path, e)))?;

        let bytes = content.len();
        Ok(ToolResult {
            content: vec![Content::Text {
                text: format!("Wrote {} bytes to {}", bytes, path),
            }],
            details: serde_json::json!({ "path": path, "bytes": bytes }),
        })
    }
}
