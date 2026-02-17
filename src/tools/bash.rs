//! Bash tool — execute shell commands with timeout and output capture.

use crate::types::*;

/// Type alias for command confirmation callback.
pub type ConfirmFn = Box<dyn Fn(&str) -> bool + Send + Sync>;
use async_trait::async_trait;
use std::time::Duration;
use tokio::process::Command;

/// Execute shell commands. Captures stdout + stderr.
pub struct BashTool {
    /// Working directory for commands
    pub cwd: Option<String>,
    /// Max execution time per command
    pub timeout: Duration,
    /// Max output bytes to capture (prevents OOM on huge outputs)
    pub max_output_bytes: usize,
    /// Commands/patterns that are always blocked (e.g., "rm -rf /")
    pub deny_patterns: Vec<String>,
    /// Optional callback for confirming dangerous commands
    pub confirm_fn: Option<ConfirmFn>,
}

impl Default for BashTool {
    fn default() -> Self {
        Self {
            cwd: None,
            timeout: Duration::from_secs(120),
            max_output_bytes: 256 * 1024, // 256KB
            deny_patterns: vec![
                "rm -rf /".into(),
                "rm -rf /*".into(),
                "mkfs".into(),
                "dd if=".into(),
                ":(){:|:&};:".into(), // fork bomb
            ],
            confirm_fn: None,
        }
    }
}

impl BashTool {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_cwd(mut self, cwd: impl Into<String>) -> Self {
        self.cwd = Some(cwd.into());
        self
    }

    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    pub fn with_deny_patterns(mut self, patterns: Vec<String>) -> Self {
        self.deny_patterns = patterns;
        self
    }

    pub fn with_confirm(mut self, f: impl Fn(&str) -> bool + Send + Sync + 'static) -> Self {
        self.confirm_fn = Some(Box::new(f));
        self
    }
}

#[async_trait]
impl AgentTool for BashTool {
    fn name(&self) -> &str {
        "bash"
    }

    fn label(&self) -> &str {
        "Execute Command"
    }

    fn description(&self) -> &str {
        "Execute a bash command and return stdout/stderr. Use for running scripts, installing packages, checking system state, etc."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "command": {
                    "type": "string",
                    "description": "The bash command to execute"
                }
            },
            "required": ["command"]
        })
    }

    async fn execute(
        &self,
        _tool_call_id: &str,
        params: serde_json::Value,
        cancel: tokio_util::sync::CancellationToken,
        _on_update: Option<ToolUpdateFn>,
    ) -> Result<ToolResult, ToolError> {
        let command = params["command"]
            .as_str()
            .ok_or_else(|| ToolError::InvalidArgs("missing 'command' parameter".into()))?;

        // Check deny patterns
        for pattern in &self.deny_patterns {
            if command.contains(pattern.as_str()) {
                return Err(ToolError::Failed(format!(
                    "Command blocked by safety policy: contains '{}'. This pattern is denied for safety.",
                    pattern
                )));
            }
        }

        // Check confirmation callback
        if let Some(ref confirm) = self.confirm_fn {
            if !confirm(command) {
                return Err(ToolError::Failed(
                    "Command was not confirmed by the user.".into(),
                ));
            }
        }

        let mut cmd = Command::new("bash");
        cmd.arg("-c").arg(command);

        if let Some(ref cwd) = self.cwd {
            cmd.current_dir(cwd);
        }

        // Capture output
        cmd.stdout(std::process::Stdio::piped());
        cmd.stderr(std::process::Stdio::piped());

        let timeout = self.timeout;
        let max_bytes = self.max_output_bytes;

        // Run with timeout and cancellation
        let result = tokio::select! {
            _ = cancel.cancelled() => {
                return Err(ToolError::Cancelled);
            }
            _ = tokio::time::sleep(timeout) => {
                return Err(ToolError::Failed(format!(
                    "Command timed out after {}s",
                    timeout.as_secs()
                )));
            }
            result = cmd.output() => {
                result.map_err(|e| ToolError::Failed(format!("Failed to execute: {}", e)))?
            }
        };

        let mut stdout = String::from_utf8_lossy(&result.stdout).to_string();
        let mut stderr = String::from_utf8_lossy(&result.stderr).to_string();

        // Truncate if too large
        if stdout.len() > max_bytes {
            stdout.truncate(max_bytes);
            stdout.push_str("\n... (output truncated)");
        }
        if stderr.len() > max_bytes {
            stderr.truncate(max_bytes);
            stderr.push_str("\n... (output truncated)");
        }

        let exit_code = result.status.code().unwrap_or(-1);

        let output = if stderr.is_empty() {
            format!("Exit code: {}\n{}", exit_code, stdout)
        } else {
            format!(
                "Exit code: {}\nSTDOUT:\n{}\nSTDERR:\n{}",
                exit_code, stdout, stderr
            )
        };

        // Return output even on failure — LLMs need error output to self-correct
        Ok(ToolResult {
            content: vec![Content::Text { text: output }],
            details: serde_json::json!({ "exit_code": exit_code, "success": exit_code == 0 }),
        })
    }
}
