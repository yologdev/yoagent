//! File tools — read and write files with safety limits.

use crate::types::*;
use async_trait::async_trait;
use base64::Engine;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

/// 20 MB limit for image files
const MAX_IMAGE_SIZE_BYTES: u64 = 20 * 1024 * 1024;

fn is_image_file(path: &Path) -> bool {
    matches!(
        path.extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_lowercase())
            .as_deref(),
        Some("jpg" | "jpeg" | "png" | "webp" | "gif" | "bmp")
    )
}

fn get_image_mime_type(path: &Path) -> Option<&'static str> {
    match path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_lowercase())
        .as_deref()
    {
        Some("jpg" | "jpeg") => Some("image/jpeg"),
        Some("png") => Some("image/png"),
        Some("webp") => Some("image/webp"),
        Some("gif") => Some("image/gif"),
        Some("bmp") => Some("image/bmp"),
        _ => None,
    }
}

/// Default number of lines returned when the caller gives no explicit `limit`.
///
/// An unbounded read used to return whole files. In a real Rust codebase the
/// median source file runs well over a thousand lines, so a single unqualified
/// read could spend ~15K tokens — and reads are roughly a fifth of all tool
/// calls in a coding agent. That is what drives a session to its context budget
/// and triggers the compaction that rewrites history.
///
/// Paging is the right bound for a file: unlike head+tail truncation it is
/// lossless and directed — the header states the true total, and the agent asks
/// for the range it actually wants.
///
/// 500 comes from measurement against a fixed compaction target, where hit rate
/// is flat between a 300- and 500-line page and degrades monotonically above
/// it — making 500 the largest page that cost nothing extra.
///
/// With [`compact_headroom_turns`] on (the default) the choice matters much
/// less: the adaptive target compacts harder as growth rises, absorbing most of
/// the difference. What matters is that a bound exists at all.
///
/// [`compact_headroom_turns`]: crate::context::ContextConfig::compact_headroom_turns
pub const DEFAULT_READ_MAX_LINES: usize = 500;

/// What a path that is not a regular file is, for a refusal message.
fn non_regular_kind(file_type: std::fs::FileType) -> &'static str {
    if file_type.is_dir() {
        return "a directory";
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::FileTypeExt;
        if file_type.is_fifo() {
            return "a FIFO (named pipe)";
        }
        if file_type.is_char_device() {
            return "a character device";
        }
        if file_type.is_block_device() {
            return "a block device";
        }
        if file_type.is_socket() {
            return "a socket";
        }
    }
    "not a regular file"
}

/// Refuse anything but a regular file (after following symlinks, as
/// `metadata` does): a FIFO, a tty or `/dev/stdin` reports a length of 0, so
/// a size check passes and a read then blocks for ever, and `/dev/zero`
/// reads without end. Writing to one blocks or reaches a device.
pub(crate) fn require_regular_file(
    metadata: &std::fs::Metadata,
    path: &str,
) -> Result<(), ToolError> {
    if metadata.is_file() {
        return Ok(());
    }
    let kind = non_regular_kind(metadata.file_type());
    Err(ToolError::Failed(format!(
        "{path} is {kind}: only regular files can be read or written"
    )))
}

/// What one agent has read, shared by its [`ReadFileTool`] and
/// [`WriteFileTool`] so that `write_file` can refuse to overwrite a file the
/// agent has read only part of.
///
/// `read_file` pages (see [`DEFAULT_READ_MAX_LINES`]), and `write_file`
/// replaces the whole file. A model that read lines 1–500 of a 2,000-line
/// file and then writes "the file" back deletes lines 501–2,000 without ever
/// having seen them. With a shared log, that write is refused with an error
/// naming the lines read, and the model can read the rest or use `edit_file`.
///
/// The rule, for a `write_file` to a path that exists:
/// - refused when this agent's latest knowledge of the file is a partial read
///   (pages that, together, do not cover every line);
/// - allowed for a file read in full and unchanged since (in one read, or in
///   pages of the same version), whatever pages of that same version it reads
///   afterwards, and after its own successful `write_file`;
/// - allowed for a file it never read (this guard is about partial reads only).
///
/// Versions are told apart by length and modification time. A page of a
/// version other than the one last known in full or in part starts over: a
/// file that changed on disk after a read and is then read in part is refused
/// until read whole again, and earlier pages of an older version do not count
/// towards a later one. A file changed after the partial read is refused too.
/// A change after a full read, with no read since, is not re-checked. A page
/// with no lines (an `offset` past the end, `limit: 0`) records nothing.
/// `edit_file` is not affected.
///
/// Paths are compared after the same resolution the sandbox uses
/// ([`PathSandbox::check`](crate::tools::PathSandbox::check)), so `./a.rs`
/// and `a.rs` are one file.
///
/// [`default_tools`](crate::tools::default_tools) shares one log between its
/// `read_file` and `write_file`. Tools built on their own have none and keep
/// the unguarded behaviour; share one with `with_read_log`:
///
/// ```rust
/// use yoagent::tools::{ReadFileTool, ReadLog, WriteFileTool};
///
/// let log = ReadLog::new();
/// let read = ReadFileTool::new().with_read_log(log.clone());
/// let write = WriteFileTool::new().with_read_log(log);
/// ```
///
/// Give each agent its own log: a sub-agent sharing its parent's would count
/// the parent's reads as its own.
#[derive(Debug, Clone, Default)]
pub struct ReadLog {
    known: Arc<Mutex<HashMap<PathBuf, Known>>>,
}

/// What the agent last knew of a file.
#[derive(Debug)]
enum Known {
    /// Every line of this version (read whole, or written by the agent).
    Full(FileStamp),
    /// Part of it.
    Partial(PartialRead),
}

/// A file read in part: the lines seen (1-indexed, inclusive, merged) of the
/// version the pages came from.
#[derive(Debug)]
struct PartialRead {
    ranges: Vec<(usize, usize)>,
    total: usize,
    stamp: FileStamp,
}

/// What identifies a version of a file on disk, as far as cheaply checkable.
#[derive(Debug, Clone, PartialEq, Eq)]
struct FileStamp {
    len: u64,
    modified: Option<std::time::SystemTime>,
}

impl FileStamp {
    fn of(metadata: &std::fs::Metadata) -> Self {
        Self {
            len: metadata.len(),
            modified: metadata.modified().ok(),
        }
    }
}

/// Sort and merge line ranges, dropping empty ones.
fn merged(ranges: &[(usize, usize)]) -> Vec<(usize, usize)> {
    let mut sorted: Vec<(usize, usize)> = ranges.iter().copied().filter(|(a, b)| a <= b).collect();
    sorted.sort_unstable();
    let mut out: Vec<(usize, usize)> = Vec::new();
    for (a, b) in sorted {
        match out.last_mut() {
            Some(last) if a <= last.1.saturating_add(1) => last.1 = last.1.max(b),
            _ => out.push((a, b)),
        }
    }
    out
}

impl ReadLog {
    /// An empty log.
    pub fn new() -> Self {
        Self::default()
    }

    fn entries(&self) -> std::sync::MutexGuard<'_, HashMap<PathBuf, Known>> {
        self.known.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Every line of this version of the file is known (read in full, or
    /// written by this agent). `None`: the version could not be read, so
    /// nothing is remembered (a later page then starts over).
    fn record_full(&self, key: &Path, stamp: Option<FileStamp>) {
        let mut entries = self.entries();
        match stamp {
            Some(stamp) => {
                entries.insert(key.to_path_buf(), Known::Full(stamp));
            }
            None => {
                entries.remove(key);
            }
        }
    }

    /// Lines `first..=last` of a `total`-line file were read.
    fn record_page(&self, key: PathBuf, page: (usize, usize), total: usize, stamp: FileStamp) {
        if page.0 > page.1 {
            // No lines shown: nothing learned, nothing forgotten.
            return;
        }
        let mut entries = self.entries();
        let start_over = match entries.get(&key) {
            // A page of a version already known whole adds nothing.
            Some(Known::Full(known)) if *known == stamp => return,
            Some(Known::Partial(read)) => read.stamp != stamp || read.total != total,
            // Never read, or another version known whole: that says nothing
            // about this one.
            _ => true,
        };
        if start_over {
            let read = PartialRead {
                ranges: Vec::new(),
                total,
                stamp: stamp.clone(),
            };
            entries.insert(key.clone(), Known::Partial(read));
        }
        let Some(Known::Partial(read)) = entries.get_mut(&key) else {
            return;
        };
        read.ranges.push(page);
        read.ranges = merged(&read.ranges);
        if matches!(read.ranges.first(), Some(&(1, last)) if last >= total) {
            entries.insert(key, Known::Full(stamp));
        }
    }

    /// The refusal for overwriting the existing file at `key` (spelled `path`
    /// for the model), or `None` when the write may go ahead.
    fn overwrite_refusal(
        &self,
        key: &Path,
        path: &str,
        current: &std::fs::Metadata,
    ) -> Option<String> {
        let entries = self.entries();
        let Some(Known::Partial(read)) = entries.get(key) else {
            return None;
        };
        let ranges: Vec<String> = read
            .ranges
            .iter()
            .map(|(a, b)| format!("{a}-{b}"))
            .collect();
        let seen = format!("only lines {}", ranges.join(", "));
        let changed = if read.stamp == FileStamp::of(current) {
            ""
        } else {
            ", and it has changed on disk since"
        };
        Some(format!(
            "Refusing to overwrite {path}: you have read {seen} of its {total} lines{changed}, \
             so writing the whole file would delete lines you have not seen. Read the rest \
             with read_file (offset/limit), or change it in place with edit_file.",
            total = read.total,
        ))
    }
}

/// Read a file's contents. Supports line range for large files.
pub struct ReadFileTool {
    /// Max file size to read (prevents OOM)
    pub max_bytes: usize,
    /// Allowed directory roots (empty = no restriction).
    ///
    /// Enforced against the *resolved* path, so `..` and symlinks cannot
    /// escape. See [`PathSandbox`](crate::tools::PathSandbox).
    pub allowed_paths: Vec<String>,
    /// Lines returned when the call specifies no `limit`.
    /// Set to `usize::MAX` to restore unbounded reads.
    pub max_lines: usize,
    /// Where reads are recorded for a [`WriteFileTool`] sharing the same
    /// [`ReadLog`] (`None`, the default, records nothing).
    pub read_log: Option<ReadLog>,
}

impl Default for ReadFileTool {
    fn default() -> Self {
        Self {
            max_bytes: 1024 * 1024, // 1MB
            allowed_paths: Vec::new(),
            max_lines: DEFAULT_READ_MAX_LINES,
            read_log: None,
        }
    }
}

impl ReadFileTool {
    pub fn new() -> Self {
        Self::default()
    }

    /// Restrict reads to these directory roots.
    pub fn with_allowed_paths(mut self, paths: Vec<String>) -> Self {
        self.allowed_paths = paths;
        self
    }

    /// Record reads in `log`, so a [`WriteFileTool`] sharing it refuses to
    /// overwrite a file read only in part. See [`ReadLog`].
    pub fn with_read_log(mut self, log: ReadLog) -> Self {
        self.read_log = Some(log);
        self
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
        "Read a file's contents. Supports text files with optional offset/limit, and image files (jpg, png, webp, gif, bmp) which are returned as base64-encoded images. Long files are returned one page at a time — the header states the total line count, so pass offset/limit to read further."
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
        params: serde_json::Value,
        ctx: ToolContext,
    ) -> Result<ToolResult, ToolError> {
        let path = params["path"]
            .as_str()
            .ok_or_else(|| ToolError::InvalidArgs("missing 'path' parameter".into()))?;

        if ctx.cancel.is_cancelled() {
            return Err(ToolError::Cancelled);
        }

        let sandbox = crate::tools::PathSandbox::new(self.allowed_paths.clone());
        let io_path = sandbox.io_path(path)?;

        // Check file exists and size
        let metadata = tokio::fs::metadata(&io_path)
            .await
            .map_err(|e| ToolError::Failed(format!("Cannot access {}: {}", path, e)))?;
        require_regular_file(&metadata, path)?;
        // The resolved path the log knows this file by (`io_path` keeps the
        // caller's spelling when unrestricted).
        let logged = match &self.read_log {
            Some(log) => Some((log, sandbox.check(path)?)),
            None => None,
        };

        // Handle image files: read as binary, return base64-encoded Content::Image
        let file_path = Path::new(path);
        if is_image_file(file_path) {
            if metadata.len() > MAX_IMAGE_SIZE_BYTES {
                return Err(ToolError::Failed(format!(
                    "Image too large ({}MB, max 20MB)",
                    metadata.len() / (1024 * 1024)
                )));
            }
            let mime_type = get_image_mime_type(file_path)
                .ok_or_else(|| ToolError::Failed("Unknown image format".into()))?;
            let bytes = tokio::fs::read(&io_path)
                .await
                .map_err(|e| ToolError::Failed(format!("Cannot read {}: {}", path, e)))?;
            let data = base64::engine::general_purpose::STANDARD.encode(&bytes);
            if let Some((log, key)) = &logged {
                log.record_full(key, Some(FileStamp::of(&metadata)));
            }
            return Ok(ToolResult {
                content: vec![Content::Image {
                    data,
                    mime_type: mime_type.to_string(),
                }],
                details: serde_json::json!({ "path": path, "bytes": bytes.len() }),
            });
        }

        // Text files: check size limit and apply line offset/limit
        if metadata.len() as usize > self.max_bytes {
            return Err(ToolError::Failed(format!(
                "File too large ({} bytes, max {}). Use offset/limit for partial reads.",
                metadata.len(),
                self.max_bytes
            )));
        }

        let content = tokio::fs::read_to_string(&io_path)
            .await
            .map_err(|e| ToolError::Failed(format!("Cannot read {}: {}", path, e)))?;

        let offset = params["offset"].as_u64().map(|v| v.max(1) as usize);
        let limit = params["limit"].as_u64().map(|v| v as usize);

        // Always show line numbers — helps agent reference exact lines for edit_file
        let lines: Vec<&str> = content.lines().collect();
        let total = lines.len();

        // No explicit limit falls back to `max_lines` rather than the whole
        // file. The header below reports the true total, so a partial read is
        // visible and the agent can page with offset/limit.
        let effective_limit = limit.unwrap_or(self.max_lines);
        let (start, end) = match offset {
            Some(off) => {
                let s = (off - 1).min(total);
                (s, s.saturating_add(effective_limit).min(total))
            }
            None => (0, effective_limit.min(total)),
        };

        let numbered: Vec<String> = lines[start..end]
            .iter()
            .enumerate()
            .map(|(i, line)| format!("{:>4} | {}", start + i + 1, line))
            .collect();

        let partial = start > 0 || end < total;
        if let Some((log, key)) = logged {
            if partial {
                log.record_page(key, (start + 1, end), total, FileStamp::of(&metadata));
            } else {
                log.record_full(&key, Some(FileStamp::of(&metadata)));
            }
        }

        let header = if partial {
            format!(
                "[Lines {}-{} of {} — use offset/limit to read the rest]",
                start + 1,
                end,
                total
            )
        } else {
            format!("[{} lines]", total)
        };

        let output = format!("{}\n{}", header, numbered.join("\n"));

        Ok(ToolResult {
            content: vec![Content::Text { text: output }],
            details: serde_json::json!({ "path": path }),
        })
    }
}

// ---------------------------------------------------------------------------

/// Write content to a file. Creates parent directories if needed.
pub struct WriteFileTool {
    /// Allowed directory roots (empty = no restriction).
    ///
    /// Enforced against the *resolved* path, so `..` and symlinks cannot
    /// escape. See [`PathSandbox`](crate::tools::PathSandbox).
    pub allowed_paths: Vec<String>,
    /// The [`ReadLog`] shared with this agent's [`ReadFileTool`]: when set, an
    /// existing file it has read only in part is not overwritten. `None` (the
    /// default) overwrites without checking.
    pub read_log: Option<ReadLog>,
}

impl Default for WriteFileTool {
    fn default() -> Self {
        Self::new()
    }
}

impl WriteFileTool {
    pub fn new() -> Self {
        Self {
            allowed_paths: Vec::new(),
            read_log: None,
        }
    }

    /// Restrict writes to these directory roots.
    pub fn with_allowed_paths(mut self, paths: Vec<String>) -> Self {
        self.allowed_paths = paths;
        self
    }

    /// Refuse to overwrite an existing file that the [`ReadFileTool`] sharing
    /// `log` has read only in part. See [`ReadLog`].
    pub fn with_read_log(mut self, log: ReadLog) -> Self {
        self.read_log = Some(log);
        self
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
        params: serde_json::Value,
        ctx: ToolContext,
    ) -> Result<ToolResult, ToolError> {
        let path = params["path"]
            .as_str()
            .ok_or_else(|| ToolError::InvalidArgs("missing 'path' parameter".into()))?;
        let content = params["content"]
            .as_str()
            .ok_or_else(|| ToolError::InvalidArgs("missing 'content' parameter".into()))?;

        if ctx.cancel.is_cancelled() {
            return Err(ToolError::Cancelled);
        }

        let sandbox = crate::tools::PathSandbox::new(self.allowed_paths.clone());
        let io_path = sandbox.io_path(path)?;
        let logged = match &self.read_log {
            Some(log) => Some((log, sandbox.check(path)?)),
            None => None,
        };

        // An existing path must be a regular file: writing to a FIFO blocks
        // until something reads it, and a device is not a file to overwrite.
        if let Ok(metadata) = tokio::fs::metadata(&io_path).await {
            require_regular_file(&metadata, path)?;
            // Overwriting a file read only in part deletes the unseen lines.
            if let Some((log, key)) = &logged {
                if let Some(refusal) = log.overwrite_refusal(key, path, &metadata) {
                    return Err(ToolError::Failed(refusal));
                }
            }
        }

        // Create parent directories
        if let Some(parent) = io_path.parent() {
            if !parent.exists() {
                tokio::fs::create_dir_all(parent)
                    .await
                    .map_err(|e| ToolError::Failed(format!("Cannot create directory: {}", e)))?;
            }
        }

        tokio::fs::write(&io_path, content)
            .await
            .map_err(|e| ToolError::Failed(format!("Cannot write {}: {}", path, e)))?;
        // The agent wrote every line: it knows the whole file now.
        if let Some((log, key)) = &logged {
            let stamp = tokio::fs::metadata(&io_path)
                .await
                .ok()
                .map(|m| FileStamp::of(&m));
            log.record_full(key, stamp);
        }

        let bytes = content.len();
        Ok(ToolResult {
            content: vec![Content::Text {
                text: format!("Wrote {} bytes to {}", bytes, path),
            }],
            details: serde_json::json!({ "path": path, "bytes": bytes }),
        })
    }
}
