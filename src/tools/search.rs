//! Search tool — grep/ripgrep-style search across files.

use crate::types::*;
use async_trait::async_trait;
use std::time::Duration;
use tokio::process::Command;

/// Search files using grep (or ripgrep if available).
pub struct SearchTool {
    /// Root directory to search in
    pub root: Option<String>,
    /// Max results to return
    pub max_results: usize,
    /// Timeout
    pub timeout: Duration,
    /// Allowed directory roots (empty = no restriction).
    ///
    /// Enforced against the *resolved* path, so `..` and symlinks cannot
    /// escape. See [`PathSandbox`](crate::tools::PathSandbox).
    pub allowed_paths: Vec<String>,
}

impl SearchTool {
    /// Restrict searching to these directory roots.
    pub fn with_allowed_paths(mut self, paths: Vec<String>) -> Self {
        self.allowed_paths = paths;
        self
    }
}

impl Default for SearchTool {
    fn default() -> Self {
        Self {
            root: None,
            max_results: 50,
            timeout: Duration::from_secs(30),
            allowed_paths: Vec::new(),
        }
    }
}

impl SearchTool {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_root(mut self, root: impl Into<String>) -> Self {
        self.root = Some(root.into());
        self
    }
}

#[async_trait]
impl AgentTool for SearchTool {
    fn name(&self) -> &str {
        "search"
    }

    fn label(&self) -> &str {
        "Search Files"
    }

    fn description(&self) -> &str {
        "Search for a pattern across files using grep. Returns matching lines with file paths and line numbers. Supports regex patterns."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "pattern": {
                    "type": "string",
                    "description": "Search pattern (regex supported)"
                },
                "path": {
                    "type": "string",
                    "description": "Directory or file to search in (optional, defaults to working directory)"
                },
                "include": {
                    "type": "string",
                    "description": "File glob pattern to include, e.g. '*.rs' (optional)"
                },
                "case_sensitive": {
                    "type": "boolean",
                    "description": "Case sensitive search (default: false)"
                }
            },
            "required": ["pattern"]
        })
    }

    async fn execute(
        &self,
        params: serde_json::Value,
        ctx: ToolContext,
    ) -> Result<ToolResult, ToolError> {
        let cancel = ctx.cancel;
        let pattern = params["pattern"]
            .as_str()
            .ok_or_else(|| ToolError::InvalidArgs("missing 'pattern' parameter".into()))?;

        let search_path = params["path"]
            .as_str()
            .map(|s| s.to_string())
            .or_else(|| self.root.clone())
            .unwrap_or_else(|| ".".into());

        let include = params["include"].as_str();
        let case_sensitive = params["case_sensitive"].as_bool().unwrap_or(false);

        if cancel.is_cancelled() {
            return Err(ToolError::Cancelled);
        }

        let io_path = crate::tools::PathSandbox::new(self.allowed_paths.clone())
            .io_path(&search_path)?
            .to_string_lossy()
            .into_owned();

        // Try ripgrep first, fall back to grep
        let (cmd_name, args) = if which_exists("rg") {
            build_rg_args(pattern, &io_path, include, case_sensitive)
        } else {
            build_grep_args(pattern, &io_path, include, case_sensitive)
        };

        let mut cmd = Command::new(&cmd_name);
        cmd.args(&args);
        // `spawn` inherits stdin; the search never needs it.
        cmd.stdin(std::process::Stdio::null());
        cmd.stdout(std::process::Stdio::piped());
        cmd.stderr(std::process::Stdio::piped());
        // A timeout or cancel drops the run; take the search with it.
        cmd.kill_on_drop(true);

        let timeout = self.timeout;
        let max = self.max_results;

        let mut child = cmd
            .spawn()
            .map_err(|e| ToolError::Failed(format!("Search failed: {}", e)))?;
        let (Some(child_out), Some(child_err)) = (child.stdout.take(), child.stderr.take()) else {
            return Err(ToolError::Failed("Search failed: no output pipes".into()));
        };

        // Keep the first `max` match lines and stop the search at the next
        // one: memory stays bounded and a huge result set costs nothing more.
        let run = async {
            use tokio::io::{AsyncBufReadExt, AsyncReadExt};
            let read_matches = async {
                let mut reader = tokio::io::BufReader::new(child_out);
                let mut lines = Vec::new();
                let mut more = false;
                let mut line = Vec::new();
                while matches!(reader.read_until(b'\n', &mut line).await, Ok(n) if n > 0) {
                    if lines.len() == max {
                        more = true;
                        break;
                    }
                    lines.push(String::from_utf8_lossy(&line).trim_end().to_string());
                    line.clear();
                }
                (lines, more)
            };
            let read_errors = async {
                let mut buf = Vec::new();
                let mut err = child_err;
                let _ = (&mut err).take(16 * 1024).read_to_end(&mut buf).await;
                let _ = tokio::io::copy(&mut err, &mut tokio::io::sink()).await;
                String::from_utf8_lossy(&buf).into_owned()
            };
            // Kill as soon as there are more matches than shown: a search
            // walking a large tree writes nothing for a while, so it would not
            // notice the closed pipe. Stderr keeps draining meanwhile.
            let read_and_stop = async {
                let found = read_matches.await;
                if found.1 {
                    let _ = child.start_kill();
                }
                found
            };
            let ((lines, more), stderr) = tokio::join!(read_and_stop, read_errors);
            (lines, more, stderr, child.wait().await)
        };

        let (lines, more, stderr, status) = tokio::select! {
            _ = cancel.cancelled() => {
                return Err(ToolError::Cancelled);
            }
            _ = tokio::time::sleep(timeout) => {
                return Err(ToolError::Failed("Search timed out".into()));
            }
            done = run => done,
        };
        let status = status.map_err(|e| ToolError::Failed(format!("Search failed: {}", e)))?;

        // grep returns exit code 1 for "no matches" — that's not an error. A
        // search stopped early (more matches than shown) was killed: its
        // status says nothing. rg and grep exit 2 when *any* file errors
        // (permission denied, a broken symlink, a file removed mid-walk), even
        // with matches found: those matches are the answer, with the errors
        // as warnings. Only a search with nothing to show is a failure.
        let errored =
            !more && (status.code() == Some(2) || (!stderr.is_empty() && status.code() != Some(1)));
        if errored && lines.is_empty() {
            return Err(ToolError::Failed(format!("Search error: {}", stderr)));
        }
        let warnings = (!more && !stderr.trim().is_empty()).then(|| {
            let text = stderr.trim();
            match text.char_indices().nth(2000) {
                Some((cut, _)) => format!("{}\n... (more warnings not shown)", &text[..cut]),
                None => text.to_string(),
            }
        });

        if lines.is_empty() {
            return Ok(ToolResult {
                content: vec![Content::Text {
                    text: format!("No matches found for '{}'", pattern),
                }],
                details: serde_json::json!({ "matches": 0 }),
            });
        }

        let shown = lines.len();
        let mut text = if more {
            format!(
                "{}\n... (showing the first {} matches; there are more. Narrow the pattern, path or include.)",
                lines.join("\n"),
                shown
            )
        } else {
            format!("{}\n({} matches)", lines.join("\n"), shown)
        };
        if let Some(w) = &warnings {
            text.push_str(&format!(
                "\nWarnings (some files could not be searched):\n{w}"
            ));
        }

        Ok(ToolResult {
            content: vec![Content::Text { text }],
            details: serde_json::json!({
                "matches": shown,
                "truncated": more,
                "warnings": warnings.is_some(),
            }),
        })
    }
}

fn which_exists(name: &str) -> bool {
    std::process::Command::new("which")
        .arg(name)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn build_rg_args(
    pattern: &str,
    path: &str,
    include: Option<&str>,
    case_sensitive: bool,
) -> (String, Vec<String>) {
    let mut args = vec!["--line-number".into(), "--no-heading".into()];

    if !case_sensitive {
        args.push("--ignore-case".into());
    }

    if let Some(glob) = include {
        args.push(format!("--glob={}", glob));
    }

    // `--regexp=` and `--` keep a pattern or path that starts with `-` from
    // being read as a flag (`--pre=<cmd>` would run a command).
    args.push(format!("--regexp={pattern}"));
    args.push("--".into());
    args.push(path.into());

    ("rg".into(), args)
}

fn build_grep_args(
    pattern: &str,
    path: &str,
    include: Option<&str>,
    case_sensitive: bool,
) -> (String, Vec<String>) {
    let mut args = vec!["-r".into(), "-n".into()];

    if !case_sensitive {
        args.push("-i".into());
    }

    if let Some(glob) = include {
        args.push(format!("--include={}", glob));
    }

    args.push("-e".into());
    args.push(pattern.into());
    args.push("--".into());
    args.push(path.into());

    ("grep".into(), args)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The pattern and the path can never be read as flags, whichever
    /// backend runs: `--pre=<cmd>` would make ripgrep run a command.
    #[test]
    fn pattern_and_path_are_never_flags() {
        let (_, rg) = build_rg_args("--pre=sh", "-x", Some("*.rs"), false);
        assert_eq!(rg[rg.len() - 3..], ["--regexp=--pre=sh", "--", "-x"]);
        assert!(rg.contains(&"--glob=*.rs".to_string()));

        let (_, grep) = build_grep_args("--pre=sh", "-x", Some("*.rs"), false);
        assert_eq!(grep[grep.len() - 4..], ["-e", "--pre=sh", "--", "-x"]);
        assert!(grep.contains(&"--include=*.rs".to_string()));
    }
}
