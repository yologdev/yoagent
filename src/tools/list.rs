//! List files tool — directory exploration.

use crate::types::*;
use async_trait::async_trait;
use std::time::Duration;
use tokio::process::Command;

/// List files under a directory. Uses `find` for traversal.
pub struct ListFilesTool {
    pub max_results: usize,
    pub timeout: Duration,
    /// Allowed directory roots (empty = no restriction).
    ///
    /// Enforced against the *resolved* path, so `..` and symlinks cannot
    /// escape. See [`PathSandbox`](crate::tools::PathSandbox).
    pub allowed_paths: Vec<String>,
}

impl ListFilesTool {
    /// Restrict listing to these directory roots.
    pub fn with_allowed_paths(mut self, paths: Vec<String>) -> Self {
        self.allowed_paths = paths;
        self
    }
}

impl Default for ListFilesTool {
    fn default() -> Self {
        Self {
            max_results: 200,
            timeout: Duration::from_secs(10),
            allowed_paths: Vec::new(),
        }
    }
}

impl ListFilesTool {
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait]
impl AgentTool for ListFilesTool {
    fn name(&self) -> &str {
        "list_files"
    }

    fn label(&self) -> &str {
        "List Files"
    }

    fn description(&self) -> &str {
        "List files and directories. Optionally filter by glob pattern. Use to explore project structure before reading specific files."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Directory to list (default: current directory)"
                },
                "pattern": {
                    "type": "string",
                    "description": "Glob pattern to filter files, e.g. '*.rs' (optional)"
                },
                "max_depth": {
                    "type": "integer",
                    "description": "Maximum directory depth (default: 3)"
                }
            }
        })
    }

    async fn execute(
        &self,
        params: serde_json::Value,
        ctx: ToolContext,
    ) -> Result<ToolResult, ToolError> {
        let cancel = ctx.cancel;
        let path = params["path"].as_str().unwrap_or(".");
        let pattern = params["pattern"].as_str();
        let max_depth = params["max_depth"].as_u64().unwrap_or(3);

        if cancel.is_cancelled() {
            return Err(ToolError::Cancelled);
        }

        let io_path = crate::tools::PathSandbox::new(self.allowed_paths.clone()).io_path(path)?;

        // Check path exists
        if !io_path.exists() {
            return Err(ToolError::Failed(format!(
                "Directory not found: {}. Check the path and try again.",
                path
            )));
        }

        let mut cmd = Command::new("find");
        // A path starting with `-` would be read as a `find` expression.
        cmd.arg(not_an_option(io_path));
        cmd.args(["-maxdepth", &max_depth.to_string()]);

        if let Some(pat) = pattern {
            cmd.args(["-name", pat]);
        }

        // Exclude common noise
        cmd.args(["-not", "-path", "*/target/*"]);
        cmd.args(["-not", "-path", "*/.git/*"]);
        cmd.args(["-not", "-path", "*/node_modules/*"]);

        cmd.arg("-type").arg("f");
        cmd.stdout(std::process::Stdio::piped());
        cmd.stderr(std::process::Stdio::piped());
        // A timeout or cancel drops the output future; take `find` with it.
        cmd.kill_on_drop(true);

        let timeout = self.timeout;

        let result = tokio::select! {
            _ = cancel.cancelled() => return Err(ToolError::Cancelled),
            _ = tokio::time::sleep(timeout) => return Err(ToolError::Failed("Listing timed out".into())),
            result = cmd.output() => {
                result.map_err(|e| ToolError::Failed(format!("Failed to list: {}", e)))?
            }
        };

        let stdout = String::from_utf8_lossy(&result.stdout).to_string();
        let mut lines: Vec<&str> = stdout.lines().collect();
        lines.sort();

        // `find` keeps walking past what it can't read (a subdirectory without
        // permission, a file removed mid-walk), reports it on stderr and exits
        // non-zero. The files it did list are the answer, with those errors as
        // warnings, so a partial listing never reads as complete. Only a
        // listing with nothing to show is a failure.
        let stderr = String::from_utf8_lossy(&result.stderr);
        let stderr = stderr.trim();
        if !result.status.success() && lines.is_empty() && !stderr.is_empty() {
            return Err(ToolError::Failed(format!("Listing error: {}", stderr)));
        }
        let warnings = (!stderr.is_empty()).then(|| match stderr.char_indices().nth(2000) {
            Some((cut, _)) => format!("{}\n... (more warnings not shown)", &stderr[..cut]),
            None => stderr.to_string(),
        });

        let total = lines.len();
        let truncated = total > self.max_results;
        if truncated {
            lines.truncate(self.max_results);
        }

        let mut text = if lines.is_empty() {
            format!("No files found in {}", path)
        } else if truncated {
            format!(
                "{}\n\n... ({} files, showing first {})",
                lines.join("\n"),
                total,
                self.max_results
            )
        } else {
            format!("{}\n\n({} files)", lines.join("\n"), total)
        };

        if let Some(w) = &warnings {
            text.push_str(&format!(
                "\nWarnings (some paths could not be read; the listing may be incomplete):\n{w}"
            ));
        }

        Ok(ToolResult {
            content: vec![Content::Text { text }],
            details: serde_json::json!({
                "total": total,
                "truncated": truncated,
                "warnings": warnings,
            }),
        })
    }
}

/// Make a path unmistakable as an operand: `-x` becomes `./-x`.
pub(crate) fn not_an_option(path: std::path::PathBuf) -> std::path::PathBuf {
    if path.as_os_str().to_string_lossy().starts_with('-') {
        std::path::Path::new(".").join(path)
    } else {
        path
    }
}

#[cfg(test)]
mod option_guard {
    use super::not_an_option;
    use std::path::PathBuf;

    #[test]
    fn only_a_leading_dash_is_rewritten() {
        assert_eq!(not_an_option("-delete".into()), PathBuf::from("./-delete"));
        assert_eq!(not_an_option("a/-b".into()), PathBuf::from("a/-b"));
        assert_eq!(not_an_option("/abs".into()), PathBuf::from("/abs"));
        assert_eq!(not_an_option(".".into()), PathBuf::from("."));
    }
}
