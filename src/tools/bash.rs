//! Bash tool — execute shell commands with timeout and output capture.
//!
//! # This is not a sandbox
//!
//! `BashTool` runs whatever the model asks through `bash -c`, with the
//! process's own environment and filesystem access. [`deny_patterns`] is a
//! substring check that catches *typos and obvious mistakes* — it is trivially
//! bypassed by any command that means to (`rm  -rf /` with two spaces, a
//! base64-decoded pipe, an equivalent `find -delete`). Treat it as a guardrail,
//! never as a security boundary.
//!
//! Real isolation belongs outside this tool: run the agent in a container or
//! VM, or gate every call through
//! [`ToolMiddleware`], which sees the arguments
//! before execution and can deny them. For credentials specifically, see
//! [`BashTool::env_allowlist`].
//!
//! [`deny_patterns`]: BashTool::deny_patterns

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
    /// Max bytes captured from each of stdout and stderr. Reading stops
    /// keeping bytes past this (the rest is drained and discarded), so memory
    /// stays bounded however much a command prints.
    pub max_output_bytes: usize,
    /// Substrings that block a command outright.
    ///
    /// A convenience guardrail against typos, **not** a security control: a
    /// substring match is bypassed by whitespace, quoting, or encoding. See
    /// the module docs.
    pub deny_patterns: Vec<String>,
    /// When set, the child process receives only these environment variables
    /// (plus `PATH`, `HOME`, `PWD` if present).
    ///
    /// Commands otherwise inherit the agent's whole environment, including
    /// any `*_API_KEY` the process holds — so a model-authored command can
    /// read them. An allowlist is the cheap mitigation when running commands
    /// the model composed.
    pub env_allowlist: Option<Vec<String>>,
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
            env_allowlist: None,
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

    /// Pass only these environment variables to commands (plus `PATH`,
    /// `HOME`, `PWD`). Use when the model composes the command.
    pub fn with_env_allowlist(mut self, vars: Vec<String>) -> Self {
        self.env_allowlist = Some(vars);
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
        params: serde_json::Value,
        ctx: ToolContext,
    ) -> Result<ToolResult, ToolError> {
        let cancel = ctx.cancel;
        let command = params["command"]
            .as_str()
            .ok_or_else(|| ToolError::InvalidArgs("missing 'command' parameter".into()))?;

        // Guardrail only — see the module docs; this is not a security boundary.
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

        // Keep credentials out of model-authored commands when configured.
        if let Some(ref allow) = self.env_allowlist {
            let keep: Vec<(String, String)> = std::env::vars()
                .filter(|(k, _)| {
                    allow.iter().any(|a| a == k) || matches!(k.as_str(), "PATH" | "HOME" | "PWD")
                })
                .collect();
            cmd.env_clear();
            for (k, v) in keep {
                cmd.env(k, v);
            }
        }

        if let Some(ref cwd) = self.cwd {
            cmd.current_dir(cwd);
        }

        // No stdin: a command that reads input gets EOF at once instead of
        // the agent's terminal (`spawn` inherits stdin; `output` did not).
        cmd.stdin(std::process::Stdio::null());
        // Capture output
        cmd.stdout(std::process::Stdio::piped());
        cmd.stderr(std::process::Stdio::piped());
        // A timeout or cancel must not leave the command running. This kills
        // the `bash` process only: what it started — pipeline stages,
        // commands in a `;` / `&&` list, background jobs — is not in that
        // kill and can keep running. Run the agent in a container if that
        // matters.
        cmd.kill_on_drop(true);

        let timeout = self.timeout;
        let max_bytes = self.max_output_bytes;

        if cancel.is_cancelled() {
            return Err(ToolError::Cancelled);
        }
        let mut child = cmd
            .spawn()
            .map_err(|e| ToolError::Failed(format!("Failed to execute: {}", e)))?;
        let (Some(child_out), Some(child_err)) = (child.stdout.take(), child.stderr.take()) else {
            return Err(ToolError::Failed("Failed to capture output".into()));
        };

        // The captures live outside the race, so a timeout still has what the
        // command printed before it, and whether it was cut.
        let mut out = Capture::default();
        let mut err = Capture::default();
        let outcome = {
            let run = async {
                tokio::join!(
                    out.read(child_out, max_bytes),
                    err.read(child_err, max_bytes),
                );
                child.wait().await
            };
            tokio::select! {
                _ = cancel.cancelled() => None,
                _ = tokio::time::sleep(timeout) => Some(Err(())),
                status = run => Some(Ok(status)),
            }
        };
        let status = match outcome {
            None => return Err(ToolError::Cancelled),
            Some(Err(())) => {
                let _ = child.start_kill();
                return Err(ToolError::Failed(format!(
                    "Command timed out after {}s. Output so far:\n{}",
                    timeout.as_secs(),
                    render_output(&out, &err)
                )));
            }
            Some(Ok(status)) => {
                status.map_err(|e| ToolError::Failed(format!("Failed to execute: {}", e)))?
            }
        };
        let exit_code = status.code().unwrap_or(-1);
        let output = format!("Exit code: {}\n{}", exit_code, render_output(&out, &err));

        // Return output even on failure — LLMs need error output to self-correct
        Ok(ToolResult {
            content: vec![Content::Text { text: output }],
            details: serde_json::json!({ "exit_code": exit_code, "success": exit_code == 0 }),
        })
    }
}

/// One output stream: the bytes kept, whether more were discarded, and a
/// read error if the stream ended on one.
#[derive(Default)]
struct Capture {
    bytes: Vec<u8>,
    cut: bool,
    read_error: Option<String>,
}

impl Capture {
    /// Read `from` to the end, keeping at most `cap` bytes; the rest is
    /// drained and discarded so the child never blocks on a full pipe.
    async fn read<R>(&mut self, mut from: R, cap: usize)
    where
        R: tokio::io::AsyncRead + Unpin,
    {
        use tokio::io::AsyncReadExt;
        let mut chunk = [0u8; 8192];
        loop {
            match from.read(&mut chunk).await {
                Ok(0) => return,
                Err(e) => {
                    self.read_error = Some(e.to_string());
                    return;
                }
                Ok(n) => {
                    let room = cap.saturating_sub(self.bytes.len());
                    self.bytes.extend_from_slice(&chunk[..n.min(room)]);
                    self.cut |= n > room;
                }
            }
        }
    }

    /// The text, with a note when bytes were discarded or reading failed.
    /// Bytes are cut before decoding, so a multi-byte character split by the
    /// cap becomes U+FFFD, never a panic.
    fn text(&self) -> String {
        let mut s = String::from_utf8_lossy(&self.bytes).into_owned();
        if self.cut {
            s.push_str("\n... (output truncated)");
        }
        if let Some(e) = &self.read_error {
            s.push_str(&format!("\n... (output incomplete: read error: {e})"));
        }
        s
    }

    fn is_empty(&self) -> bool {
        self.bytes.is_empty() && !self.cut && self.read_error.is_none()
    }
}

/// Stdout alone, or both streams labelled.
fn render_output(out: &Capture, err: &Capture) -> String {
    if err.is_empty() {
        out.text()
    } else {
        format!("STDOUT:\n{}\nSTDERR:\n{}", out.text(), err.text())
    }
}
