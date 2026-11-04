//! File tools — read and write files with safety limits.

use crate::types::*;
use async_trait::async_trait;
use base64::Engine;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

/// The largest image `read_file` sends by default: 3.75 MB, which base64
/// makes 5 MB. Anthropic refuses an image over 5 MB encoded, Bedrock one
/// over 3.75 MB; see [`ReadFileTool::max_image_bytes`].
pub const DEFAULT_MAX_IMAGE_BYTES: u64 = 3_932_160;

/// The longest side `read_file` sends: Anthropic and Bedrock refuse an image
/// wider or taller than 8000 px.
const MAX_IMAGE_SIDE: u32 = 8000;

/// What `read_file` treats as an image: the formats every provider with image
/// input takes, then ones none of them does (refused with a hint, rather than
/// read as text that is not UTF-8).
const IMAGE_EXTENSIONS: &[&str] = &["jpg", "jpeg", "png", "webp", "gif"];
const REFUSED_IMAGE_EXTENSIONS: &[&str] =
    &["bmp", "tif", "tiff", "heic", "heif", "avif", "ico", "jxl"];

fn image_extension(path: &Path) -> Option<String> {
    let ext = path.extension()?.to_str()?.to_lowercase();
    (IMAGE_EXTENSIONS.contains(&ext.as_str()) || REFUSED_IMAGE_EXTENSIONS.contains(&ext.as_str()))
        .then_some(ext)
}

/// The image's type, from its first bytes: a file named `.png` may hold a
/// JPEG, and a provider refuses a block whose stated type is not its own.
fn sniff_image(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some("image/png")
    } else if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        Some("image/jpeg")
    } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        Some("image/gif")
    } else if bytes.len() >= 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        Some("image/webp")
    } else {
        None
    }
}

/// Width and height from the image's header, or `None` when it is cut off
/// before them.
fn image_size(bytes: &[u8], mime: &str) -> Option<(u32, u32)> {
    let be16 = |at: usize| Some(u16::from_be_bytes(bytes.get(at..at + 2)?.try_into().ok()?) as u32);
    let le16 = |at: usize| Some(u16::from_le_bytes(bytes.get(at..at + 2)?.try_into().ok()?) as u32);
    let le24 = |at: usize| {
        let b = bytes.get(at..at + 3)?;
        Some(b[0] as u32 | (b[1] as u32) << 8 | (b[2] as u32) << 16)
    };
    let be32 = |at: usize| Some(u32::from_be_bytes(bytes.get(at..at + 4)?.try_into().ok()?));
    match mime {
        // IHDR comes first.
        "image/png" => Some((be32(16)?, be32(20)?)),
        "image/gif" => Some((le16(6)?, le16(8)?)),
        "image/webp" => match bytes.get(12..16)? {
            b"VP8 " => Some((le16(26)? & 0x3FFF, le16(28)? & 0x3FFF)),
            b"VP8L" => {
                let bits = u32::from_le_bytes(bytes.get(21..25)?.try_into().ok()?);
                Some(((bits & 0x3FFF) + 1, ((bits >> 14) & 0x3FFF) + 1))
            }
            b"VP8X" => Some((le24(24)? + 1, le24(27)? + 1)),
            _ => None,
        },
        "image/jpeg" => {
            // Walk the segments to the first start-of-frame.
            let mut at = 2;
            loop {
                if *bytes.get(at)? != 0xFF {
                    return None;
                }
                let marker = *bytes.get(at + 1)?;
                match marker {
                    // Fill byte, or a marker with no length.
                    0xFF => at += 1,
                    0x01 | 0xD0..=0xD7 => at += 2,
                    0xC0..=0xCF if !matches!(marker, 0xC4 | 0xC8 | 0xCC) => {
                        return Some((be16(at + 7)?, be16(at + 5)?));
                    }
                    _ => at += 2 + be16(at + 2)? as usize,
                }
            }
        }
        _ => None,
    }
}

/// Why `bytes` (from `path`) must not be sent as an image, or its type.
fn check_image(path: &str, ext: &str, bytes: &[u8]) -> Result<&'static str, ToolError> {
    const TAKEN: &str = "model APIs take PNG, JPEG, GIF or WebP";
    let Some(mime) = sniff_image(bytes) else {
        return Err(ToolError::Failed(if IMAGE_EXTENSIONS.contains(&ext) {
            format!("{path} is not a PNG, JPEG, GIF or WebP image, whatever its name says")
        } else {
            format!(
                "{path}: {} images are not sent ({TAKEN}). Convert it first, e.g. \
                 `magick {path} out.png` or, on macOS, `sips -s format png {path} --out out.png`.",
                ext.to_uppercase()
            )
        }));
    };
    let Some((w, h)) = image_size(bytes, mime) else {
        return Err(ToolError::Failed(format!(
            "{path}: cannot read the image's size from its header (cut off or damaged?)"
        )));
    };
    if w == 0 || h == 0 || w > MAX_IMAGE_SIDE || h > MAX_IMAGE_SIDE {
        return Err(ToolError::Failed(format!(
            "{path} is {w}×{h} px; images are sent at most {MAX_IMAGE_SIDE} px a side. \
             Scale it down first, e.g. `magick {path} -resize 2000x2000 smaller.png` \
             or `sips -Z 2000 {path} --out smaller.png`."
        )));
    }
    Ok(mime)
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

/// Hold the file at `key` (a resolved path, [`PathSandbox::check`]'s) for
/// one tool call: `read_file`, `write_file` and `edit_file` take turns on a
/// file, process-wide, so calls the loop runs at once (one response's tool
/// calls, sub-agents) cannot interleave. Two `edit_file`s of one file each
/// read it and write it back; interleaved, the last write wins and the other
/// edit is lost while both report success. A read during a write could see
/// the file half written.
///
/// Other processes are not held off; this is not a file lock.
///
/// [`PathSandbox::check`]: crate::tools::PathSandbox::check
pub(crate) async fn hold_file(key: &Path) -> tokio::sync::OwnedMutexGuard<()> {
    use std::sync::{OnceLock, Weak};
    type Locks = HashMap<PathBuf, Weak<tokio::sync::Mutex<()>>>;
    static LOCKS: OnceLock<Mutex<Locks>> = OnceLock::new();
    let lock = {
        let mut locks = LOCKS
            .get_or_init(Default::default)
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        // Entries go when the last call holding or waiting for one ends.
        locks.retain(|_, held| held.strong_count() > 0);
        match locks.get(key).and_then(Weak::upgrade) {
            Some(lock) => lock,
            None => {
                let lock = Arc::new(tokio::sync::Mutex::new(()));
                locks.insert(key.to_path_buf(), Arc::downgrade(&lock));
                lock
            }
        }
    };
    lock.lock_owned().await
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
///
/// The log lives as long as the tools, not the conversation:
/// `Agent::reset()`, compaction and a new `SubAgentTool` delegation do not
/// clear it (the tool `Arc`s are reused), and a read whose output is
/// truncated afterwards still counts as read. A fresh `default_tools()` (or
/// a fresh `ReadLog`) starts over.
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
    /// The largest image file sent, in bytes (default
    /// [`DEFAULT_MAX_IMAGE_BYTES`], what every provider takes). Raise it only
    /// for a provider known to take more: one it refuses fails every later
    /// request of the conversation.
    pub max_image_bytes: u64,
}

impl Default for ReadFileTool {
    fn default() -> Self {
        Self {
            max_bytes: 1024 * 1024, // 1MB
            allowed_paths: Vec::new(),
            max_lines: DEFAULT_READ_MAX_LINES,
            read_log: None,
            max_image_bytes: DEFAULT_MAX_IMAGE_BYTES,
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

    /// Send images up to `bytes` (see [`max_image_bytes`](Self::max_image_bytes)).
    pub fn with_max_image_bytes(mut self, bytes: u64) -> Self {
        self.max_image_bytes = bytes;
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
        "Read a file's contents. Supports text files with optional offset/limit, and images (PNG, JPEG, GIF, WebP; up to 8000 px a side), which are returned as images. Long files are returned one page at a time — the header states the total line count, so pass offset/limit to read further."
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
        // The resolved path the lock and the log know this file by (`io_path`
        // keeps the caller's spelling when unrestricted).
        let key = sandbox.check(path)?;
        let _held = hold_file(&key).await;

        // Check file exists and size
        let metadata = tokio::fs::metadata(&io_path)
            .await
            .map_err(|e| ToolError::Failed(format!("Cannot access {}: {}", path, e)))?;
        require_regular_file(&metadata, path)?;
        let logged = self.read_log.as_ref().map(|log| (log, key));

        // Images: sent only in a form every provider takes. A block one
        // refuses stays in the history, and every later request fails too.
        if let Some(ext) = image_extension(Path::new(path)) {
            if metadata.len() > self.max_image_bytes {
                return Err(ToolError::Failed(format!(
                    "{path}: image too large ({:.1} MB; images are sent up to {:.2} MB). \
                     Scale it down first, e.g. `magick {path} -resize 2000x2000 -quality 85 \
                     smaller.jpg` or `sips -Z 2000 {path} --out smaller.jpg`.",
                    metadata.len() as f64 / 1_048_576.0,
                    self.max_image_bytes as f64 / 1_048_576.0,
                )));
            }
            let bytes = tokio::fs::read(&io_path)
                .await
                .map_err(|e| ToolError::Failed(format!("Cannot read {}: {}", path, e)))?;
            let mime_type = check_image(path, &ext, &bytes)?;
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
        let key = sandbox.check(path)?;
        let _held = hold_file(&key).await;
        let logged = self.read_log.as_ref().map(|log| (log, key));

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
