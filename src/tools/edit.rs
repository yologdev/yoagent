//! Edit tool — surgical search/replace edits on files.
//!
//! This is the most important tool for coding agents. Instead of rewriting
//! entire files, the agent specifies exact text to find and replace.
//! Modeled after Claude Code's Edit tool and Aider's search/replace blocks.

use crate::types::*;
use async_trait::async_trait;

/// Surgical file editing via exact text search/replace.
pub struct EditFileTool {
    /// Allowed directory roots (empty = no restriction).
    ///
    /// Enforced against the *resolved* path, so `..` and symlinks cannot
    /// escape. See [`PathSandbox`](crate::tools::PathSandbox).
    pub allowed_paths: Vec<String>,
}

impl Default for EditFileTool {
    fn default() -> Self {
        Self::new()
    }
}

impl EditFileTool {
    pub fn new() -> Self {
        Self {
            allowed_paths: Vec::new(),
        }
    }

    /// Restrict edits to these directory roots.
    pub fn with_allowed_paths(mut self, paths: Vec<String>) -> Self {
        self.allowed_paths = paths;
        self
    }
}

#[async_trait]
impl AgentTool for EditFileTool {
    fn name(&self) -> &str {
        "edit_file"
    }

    fn label(&self) -> &str {
        "Edit File"
    }

    fn description(&self) -> &str {
        "Make a surgical edit to a file by specifying exact text to find and replace. The old_text must match exactly (including whitespace and indentation). For creating new files, use write_file instead."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "File path to edit"
                },
                "old_text": {
                    "type": "string",
                    "description": "Exact text to find (must match exactly, including whitespace)"
                },
                "new_text": {
                    "type": "string",
                    "description": "Text to replace it with"
                }
            },
            "required": ["path", "old_text", "new_text"]
        })
    }

    async fn execute(
        &self,
        params: serde_json::Value,
        ctx: ToolContext,
    ) -> Result<ToolResult, ToolError> {
        let cancel = ctx.cancel;
        let path = params["path"]
            .as_str()
            .ok_or_else(|| ToolError::InvalidArgs("missing 'path' parameter".into()))?;
        let old_text = params["old_text"]
            .as_str()
            .ok_or_else(|| ToolError::InvalidArgs("missing 'old_text' parameter".into()))?;
        let new_text = params["new_text"]
            .as_str()
            .ok_or_else(|| ToolError::InvalidArgs("missing 'new_text' parameter".into()))?;

        if cancel.is_cancelled() {
            return Err(ToolError::Cancelled);
        }

        let io_path = crate::tools::PathSandbox::new(self.allowed_paths.clone()).io_path(path)?;

        // Read existing file (a regular one: a FIFO or device would block or
        // read without end).
        if let Ok(metadata) = tokio::fs::metadata(&io_path).await {
            crate::tools::file::require_regular_file(&metadata, path)?;
        }
        let content = tokio::fs::read_to_string(&io_path).await.map_err(|e| {
            ToolError::Failed(format!(
                "Cannot read {}: {}. Use write_file to create new files.",
                path, e
            ))
        })?;

        // Find the old text: as given, else (in a CRLF file) with its line
        // breaks spelled `\r\n`, the way the model could not see them.
        let mut match_count = content.matches(old_text).count();
        let crlf = match match_count {
            0 => crlf_adapted(&content, old_text, new_text),
            _ => None,
        };
        let (old_text, new_text) = match &crlf {
            Some((old, new)) => {
                match_count = content.matches(old.as_str()).count();
                (old.as_str(), new.as_str())
            }
            None => (old_text, new_text),
        };

        if match_count == 0 {
            // Provide helpful error with context
            let suggestion = find_similar_text(&content, old_text);
            let hint = if let Some(similar) = suggestion {
                format!(
                    "\n\nDid you mean (line numbers are not part of the file):\n```\n{}\n```\nMake sure old_text matches exactly, including whitespace and indentation.",
                    similar
                )
            } else {
                "\n\nTip: Use read_file to see the current file contents, then copy the exact text you want to replace.".into()
            };

            return Err(ToolError::Failed(format!(
                "old_text not found in {}.{}",
                path, hint
            )));
        }

        if match_count > 1 {
            return Err(ToolError::Failed(format!(
                "old_text matches {} locations in {}. Include more surrounding context to make the match unique.",
                match_count, path
            )));
        }

        // Perform the replacement
        let new_content = content.replacen(old_text, new_text, 1);

        tokio::fs::write(&io_path, &new_content)
            .await
            .map_err(|e| ToolError::Failed(format!("Cannot write {}: {}", path, e)))?;

        // Show what changed
        let old_lines = old_text.lines().count();
        let new_lines = new_text.lines().count();
        let diff_summary = if old_text == new_text {
            "No changes (old_text == new_text)".into()
        } else {
            format!(
                "Replaced {} line{} with {} line{} in {}",
                old_lines,
                if old_lines == 1 { "" } else { "s" },
                new_lines,
                if new_lines == 1 { "" } else { "s" },
                path
            )
        };

        Ok(ToolResult {
            content: vec![Content::Text { text: diff_summary }],
            details: serde_json::json!({
                "path": path,
                "old_lines": old_lines,
                "new_lines": new_lines,
                "crlf": crlf.is_some(),
            }),
        })
    }
}

/// The edit with every bare `\n` of `old` and `new` spelled `\r\n`, when
/// `old` has one and `content` contains it spelled that way: a model writes
/// line breaks as `\n` and cannot see a CRLF file's `\r`. Only tried when
/// `old` is not found as given, so an exact match always wins and an LF
/// file is never touched.
fn crlf_adapted(content: &str, old: &str, new: &str) -> Option<(String, String)> {
    let old_crlf = bare_lf_to_crlf(old);
    if old_crlf == old || !content.contains(old_crlf.as_str()) {
        return None;
    }
    Some((old_crlf, bare_lf_to_crlf(new)))
}

/// `s` with every bare `\n` spelled `\r\n`; an existing `\r\n` is kept.
fn bare_lf_to_crlf(s: &str) -> String {
    s.replace("\r\n", "\n").replace('\n', "\r\n")
}

/// Most characters of one file line reproduced in a "did you mean" snippet:
/// the snippet is part of an error, which no output cap trims, and one line
/// of a minified file can be megabytes.
const SNIPPET_MAX_LINE_CHARS: usize = 400;

/// Most lines of a "did you mean" snippet.
const SNIPPET_MAX_LINES: usize = 20;

/// Try to find similar text in the file (fuzzy match for better error
/// messages): the lines from the first one containing `target`'s first
/// line, as many as `target` has plus one, numbered like `read_file`'s
/// output and capped at [`SNIPPET_MAX_LINES`] lines of
/// [`SNIPPET_MAX_LINE_CHARS`] characters each.
fn find_similar_text(content: &str, target: &str) -> Option<String> {
    let target_trimmed = target.trim();
    if target_trimmed.is_empty() {
        return None;
    }

    // Try to find the first line of target in the content
    let first_line = target_trimmed.lines().next()?;
    let first_line_trimmed = first_line.trim();

    if first_line_trimmed.is_empty() {
        return None;
    }

    // Search for lines containing the first line (case-sensitive)
    let lines: Vec<&str> = content.lines().collect();
    for (i, line) in lines.iter().enumerate() {
        if line.contains(first_line_trimmed) {
            // Return a few lines of context
            let start = i;
            let target_line_count = target_trimmed.lines().count();
            let end = (i + target_line_count + 1).min(lines.len());
            let shown = end.min(start + SNIPPET_MAX_LINES);
            let mut snippet: Vec<String> = lines[start..shown]
                .iter()
                .enumerate()
                .map(|(k, line)| format!("{:>4} | {}", start + k + 1, cap_line(line)))
                .collect();
            if shown < end {
                snippet.push(format!("... ({} more lines)", end - shown));
            }
            return Some(snippet.join("\n"));
        }
    }

    None
}

/// `line`, cut to [`SNIPPET_MAX_LINE_CHARS`] characters (on a character
/// boundary) with a marker when it was longer.
fn cap_line(line: &str) -> std::borrow::Cow<'_, str> {
    match line.char_indices().nth(SNIPPET_MAX_LINE_CHARS) {
        Some((at, _)) => format!("{}… (line truncated)", &line[..at]).into(),
        None => line.into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bare_lf_becomes_crlf_and_crlf_is_not_doubled() {
        assert_eq!(bare_lf_to_crlf("a\nb\r\nc\n"), "a\r\nb\r\nc\r\n");
        assert_eq!(bare_lf_to_crlf("one line"), "one line");
    }

    #[test]
    fn crlf_adaptation_needs_a_line_break_and_a_match() {
        let file = "alpha\r\nbeta\r\ngamma\r\n";
        assert_eq!(
            crlf_adapted(file, "alpha\nbeta", "A\nB\nC"),
            Some(("alpha\r\nbeta".into(), "A\r\nB\r\nC".into()))
        );
        assert_eq!(crlf_adapted(file, "alpha", "A"), None);
        assert_eq!(crlf_adapted(file, "alpha\ngamma", "x"), None);
        assert_eq!(crlf_adapted("alpha\nbeta\n", "alpha\nbeta", "x"), None);
    }

    #[test]
    fn snippet_lines_are_numbered_and_capped() {
        let long = "é".repeat(SNIPPET_MAX_LINE_CHARS + 50);
        let content = format!("zero\nfn target() {{ {long}\nnext\n");
        let snippet = find_similar_text(&content, "fn target() {\nnope").unwrap();
        let lines: Vec<&str> = snippet.lines().collect();
        assert_eq!(lines.len(), 2, "{snippet}");
        assert!(lines[0].starts_with("   2 | fn target() { é"), "{snippet}");
        assert!(lines[0].ends_with("é… (line truncated)"), "{snippet}");
        assert_eq!(
            lines[0].chars().count(),
            "   2 | ".len() + SNIPPET_MAX_LINE_CHARS + "… (line truncated)".chars().count()
        );
        assert_eq!(lines[1], "   3 | next");
    }

    #[test]
    fn snippet_has_at_most_max_lines() {
        let content = (1..=100).map(|i| format!("l{i}\n")).collect::<String>();
        let target = (2..=60).fold("l1\n".to_string(), |t, i| t + &format!("l{i}x\n"));
        let snippet = find_similar_text(&content, &target).unwrap();
        let lines: Vec<&str> = snippet.lines().collect();
        assert_eq!(lines.len(), SNIPPET_MAX_LINES + 1, "{snippet}");
        assert_eq!(lines[0], "   1 | l1");
        assert_eq!(lines[SNIPPET_MAX_LINES], "... (41 more lines)");
    }
}
