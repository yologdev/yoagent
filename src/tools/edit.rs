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

        let sandbox = crate::tools::PathSandbox::new(self.allowed_paths.clone());
        let io_path = sandbox.io_path(path)?;
        // From the read to the write: another call's edit of this file in
        // between would be overwritten.
        let _held = crate::tools::file::hold_file(&sandbox.check(path)?).await;
        // A run cancelled while this call waited for the file does not act.
        if cancel.is_cancelled() {
            return Err(ToolError::Cancelled);
        }

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

        // Find the old text: as given, else with its line breaks spelled
        // `\r\n` (a model writes `\n` and cannot see a CRLF file's `\r`).
        let mut old_used = std::borrow::Cow::Borrowed(old_text);
        let mut match_count = content.matches(old_text).count();
        if match_count == 0 {
            let old_crlf = bare_lf_to_crlf(old_text);
            if old_crlf != old_text {
                let crlf_count = content.matches(old_crlf.as_str()).count();
                if crlf_count > 0 {
                    match_count = crlf_count;
                    old_used = old_crlf.into();
                }
            }
        }

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

        // Write `new_text` with the line endings of the place it goes, so a
        // CRLF file never gains bare `\n` (an LF region is left as given).
        let at = content.find(old_used.as_ref()).unwrap_or_default();
        let new_used = if region_uses_crlf(&content, at, old_used.len()) {
            std::borrow::Cow::Owned(bare_lf_to_crlf(new_text))
        } else {
            std::borrow::Cow::Borrowed(new_text)
        };
        let crlf = old_used != old_text || new_used != new_text;
        let (old_text, new_text) = (old_used.as_ref(), new_used.as_ref());

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
                "crlf": crlf,
            }),
        })
    }
}

/// Whether the text at `content[at..at + len]` sits among CRLF line
/// endings: when it contains line breaks, all of them are `\r\n`; when it
/// has none, the line break that ends its line is (or, on the last line,
/// the one before it). No line break anywhere: no.
fn region_uses_crlf(content: &str, at: usize, len: usize) -> bool {
    let region = &content[at..at + len];
    let breaks = region.matches('\n').count();
    if breaks > 0 {
        return region.matches("\r\n").count() == breaks;
    }
    let bytes = content.as_bytes();
    let next = content[at + len..].find('\n').map(|i| at + len + i);
    match next.or_else(|| content[..at].rfind('\n')) {
        Some(i) => i > 0 && bytes[i - 1] == b'\r',
        None => false,
    }
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
/// output, each capped at [`SNIPPET_MAX_LINE_CHARS`] characters. Longer
/// than [`SNIPPET_MAX_LINES`] lines, it shows a window of that many around
/// the first line that differs from `target`, with a count of the lines
/// left out above and below.
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
    let start = lines
        .iter()
        .position(|line| line.contains(first_line_trimmed))?;
    let target_lines: Vec<&str> = target_trimmed.lines().collect();
    let end = (start + target_lines.len() + 1).min(lines.len());
    // The first line of the file that is not the target's (the first one
    // is only required to contain it).
    let differs = (1..end - start)
        .find(|&k| target_lines.get(k) != Some(&lines[start + k]))
        .unwrap_or(0);
    let from = (start + differs)
        .saturating_sub(SNIPPET_MAX_LINES / 2)
        .clamp(start, end.saturating_sub(SNIPPET_MAX_LINES).max(start));
    let to = end.min(from + SNIPPET_MAX_LINES);

    let mut snippet = Vec::new();
    if from > start {
        snippet.push(format!("... ({} lines above)", from - start));
    }
    snippet.extend(
        lines[from..to]
            .iter()
            .enumerate()
            .map(|(k, line)| format!("{:>4} | {}", from + k + 1, cap_line(line))),
    );
    if to < end {
        snippet.push(format!("... ({} more lines)", end - to));
    }
    Some(snippet.join("\n"))
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
    fn the_region_decides_the_line_ending() {
        let crlf = "alpha\r\nbeta\r\ngamma\r\n";
        // Line breaks inside the region.
        assert!(region_uses_crlf(crlf, 0, "alpha\r\nbeta".len()));
        // None inside: the break ending its line, or (last line) before it.
        assert!(region_uses_crlf(crlf, 0, "alpha".len()));
        assert!(region_uses_crlf("a\r\nlast", 3, 4));
        assert!(!region_uses_crlf("alpha\nbeta\n", 0, 5));
        assert!(!region_uses_crlf("no breaks", 0, 2));
        // A mixed file: the region's own style.
        let mixed = "a\r\nb\r\nc\nd\n";
        assert!(region_uses_crlf(mixed, 0, 1));
        assert!(!region_uses_crlf(mixed, 7, 1));
        assert!(!region_uses_crlf(mixed, 3, "b\r\nc\nd".len()));
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

    /// A 40-line old_text that matches the file up to its line 30: the
    /// window shows line 30, with what it leaves out counted on both sides.
    #[test]
    fn snippet_window_centres_on_the_first_difference() {
        let content = (1..=100).map(|i| format!("l{i}\n")).collect::<String>();
        let target = (1..=40)
            .map(|i| {
                if i == 30 {
                    "changed\n".into()
                } else {
                    format!("l{i}\n")
                }
            })
            .collect::<String>();
        let snippet = find_similar_text(&content, &target).unwrap();
        let lines: Vec<&str> = snippet.lines().collect();
        assert_eq!(lines.len(), SNIPPET_MAX_LINES + 2, "{snippet}");
        assert_eq!(lines[0], "... (19 lines above)");
        assert_eq!(lines[1], "  20 | l20");
        assert!(lines.contains(&"  30 | l30"), "{snippet}");
        assert_eq!(lines[SNIPPET_MAX_LINES], "  39 | l39");
        assert_eq!(lines[SNIPPET_MAX_LINES + 1], "... (2 more lines)");
    }
}
