//! File tools — read and write files with safety limits.

use crate::types::*;
use async_trait::async_trait;
use base64::Engine;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

/// The largest image `read_file` sends by default: 3.75 MB (3,750,000
/// bytes), which base64 makes 5,000,000. Anthropic refuses an image over
/// 5 MiB encoded, Bedrock one over 3.75 MB; see
/// [`ReadFileTool::max_image_bytes`].
pub const DEFAULT_MAX_IMAGE_BYTES: u64 = 3_750_000;

/// The longest side `read_file` sends: Anthropic and Bedrock refuse an image
/// wider or taller than 8000 px. (Anthropic allows only 2000 px once a
/// request carries more than 20 images; that depends on the conversation,
/// not the file, and is not checked.)
const MAX_IMAGE_SIDE: u32 = 8000;

/// Extensions `read_file` sends as images: the formats every built-in
/// provider with image input takes.
const IMAGE_EXTENSIONS: &[&str] = &["jpg", "jpeg", "png", "webp"];
/// Extensions refused with a conversion hint rather than read as text that
/// is not UTF-8. GIF: Gemini does not list it, OpenAI takes only still ones.
const REFUSED_IMAGE_EXTENSIONS: &[&str] = &[
    "gif", "bmp", "tif", "tiff", "heic", "heif", "avif", "ico", "jxl",
];

fn image_extension(path: &Path) -> Option<String> {
    let ext = path.extension()?.to_str()?.to_lowercase();
    (IMAGE_EXTENSIONS.contains(&ext.as_str()) || REFUSED_IMAGE_EXTENSIONS.contains(&ext.as_str()))
        .then_some(ext)
}

/// An image format `read_file` sends.
#[derive(Debug, Clone, Copy, PartialEq)]
enum ImageKind {
    Png,
    Jpeg,
    Webp,
}

impl ImageKind {
    /// From the file's first bytes: a file named `.png` may hold a JPEG, and
    /// a provider refuses a block whose stated type is not its own.
    fn sniff(bytes: &[u8]) -> Option<Self> {
        if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
            Some(Self::Png)
        } else if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
            Some(Self::Jpeg)
        } else if bytes.len() >= 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
            Some(Self::Webp)
        } else {
            None
        }
    }

    fn mime(self) -> &'static str {
        match self {
            Self::Png => "image/png",
            Self::Jpeg => "image/jpeg",
            Self::Webp => "image/webp",
        }
    }
}

/// What an image's header says.
#[derive(Debug, PartialEq)]
enum ImageHeader {
    Size {
        width: u32,
        height: u32,
    },
    /// A JPEG coded in a way decoders seldom read: lossless, hierarchical,
    /// arithmetic-coded, or more than 8 bits a sample.
    UnusualJpeg,
    /// An animated WebP: providers that take images take still ones.
    Animated,
}

/// `N` bytes of `b` at `i`, if there are that many.
fn bytes_at<const N: usize>(b: &[u8], i: usize) -> Option<[u8; N]> {
    b.get(i..i + N)?.try_into().ok()
}

/// The image's header, or `None` when its size cannot be read (the bytes end
/// before it, or an unknown WebP chunk).
fn image_header(bytes: &[u8], kind: ImageKind) -> Option<ImageHeader> {
    let be16 = |i| bytes_at(bytes, i).map(|b| u16::from_be_bytes(b) as u32);
    let le16 = |i| bytes_at(bytes, i).map(|b| u16::from_le_bytes(b) as u32);
    let le24 = |i| bytes_at::<3>(bytes, i).map(|[a, b, c]| u32::from_le_bytes([a, b, c, 0]));
    let (width, height) = match kind {
        // IHDR comes first.
        ImageKind::Png => (
            u32::from_be_bytes(bytes_at(bytes, 16)?),
            u32::from_be_bytes(bytes_at(bytes, 20)?),
        ),
        ImageKind::Webp => match bytes.get(12..16)? {
            b"VP8 " => (le16(26)? & 0x3FFF, le16(28)? & 0x3FFF),
            b"VP8L" => {
                let bits = u32::from_le_bytes(bytes_at(bytes, 21)?);
                ((bits & 0x3FFF) + 1, ((bits >> 14) & 0x3FFF) + 1)
            }
            b"VP8X" => {
                if bytes.get(20)? & 0x02 != 0 {
                    return Some(ImageHeader::Animated);
                }
                (le24(24)? + 1, le24(27)? + 1)
            }
            _ => return None,
        },
        ImageKind::Jpeg => {
            // Walk the segments to the first start-of-frame.
            let mut at = 2;
            loop {
                if *bytes.get(at)? != 0xFF {
                    // Padding some writers leave between segments; decoders
                    // skip it.
                    at += 1;
                    continue;
                }
                let marker = *bytes.get(at + 1)?;
                match marker {
                    // Fill byte, or a marker with no length.
                    0xFF => at += 1,
                    0x00 | 0x01 | 0xD0..=0xD7 => at += 2,
                    // Baseline, extended and progressive, 8 bits a sample.
                    0xC0..=0xC2 if *bytes.get(at + 4)? == 8 => {
                        break (be16(at + 7)?, be16(at + 5)?)
                    }
                    0xC0..=0xCF if !matches!(marker, 0xC4 | 0xC8 | 0xCC) => {
                        return Some(ImageHeader::UnusualJpeg);
                    }
                    _ => at += 2 + be16(at + 2)? as usize,
                }
            }
        }
    };
    Some(ImageHeader::Size { width, height })
}

fn convert_hint(path: &str) -> String {
    format!("Convert it first, e.g. `magick '{path}' out.png`, or on macOS `sips -s format png '{path}' --out out.png`.")
}

fn scale_hint(path: &str) -> String {
    format!(
        "Scale it down first, e.g. `magick '{path}' -resize 2000x2000 smaller.jpg`, \
         or on macOS `sips -Z 2000 '{path}' --out smaller.jpg`."
    )
}

/// Why `bytes` (from `path`) must not be sent as an image, or its type.
///
/// `bytes` may be only the first bytes of a file too large to send
/// (`whole: false`): a header that does not fit in them is then not a fault,
/// and `Ok` lets the caller say "too large".
fn check_image(path: &str, ext: &str, bytes: &[u8], whole: bool) -> Result<ImageKind, ToolError> {
    let refuse = |why: String| Err(ToolError::Failed(why));
    let Some(kind) = ImageKind::sniff(bytes) else {
        return refuse(if IMAGE_EXTENSIONS.contains(&ext) {
            format!(
                "{path} is not a PNG, JPEG or WebP image, whatever its name says. If it is \
                 another image format, {}",
                lowercase_first(&convert_hint(path))
            )
        } else {
            format!(
                "{path}: {} images are not sent (what every model API takes is PNG, JPEG or \
                 WebP). {}",
                ext.to_uppercase(),
                convert_hint(path)
            )
        });
    };
    match image_header(bytes, kind) {
        None if !whole => Ok(kind),
        None => refuse(format!(
            "{path}: cannot read the image's size from its header (cut off or damaged?). \
             If it opens in an image viewer, {}",
            lowercase_first(&convert_hint(path))
        )),
        Some(ImageHeader::UnusualJpeg) => refuse(format!(
            "{path} is a JPEG coded in a way model APIs may not decode (lossless, \
             hierarchical, arithmetic-coded or over 8 bits a sample). {}",
            convert_hint(path)
        )),
        Some(ImageHeader::Animated) => refuse(format!(
            "{path} is an animated WebP; images are sent still. Take one frame first, \
             e.g. `magick '{path}[0]' frame.png`."
        )),
        Some(ImageHeader::Size { width, height }) if width == 0 || height == 0 => refuse(format!(
            "{path}: its header gives no width or height ({width}×{height}), so it is not \
             sent. {}",
            convert_hint(path)
        )),
        Some(ImageHeader::Size { width, height })
            if width > MAX_IMAGE_SIDE || height > MAX_IMAGE_SIDE =>
        {
            refuse(format!(
                "{path} is {width}×{height} px; images are sent at most {MAX_IMAGE_SIDE} px \
                 a side. {}",
                scale_hint(path)
            ))
        }
        Some(ImageHeader::Size { .. }) => Ok(kind),
    }
}

fn lowercase_first(s: &str) -> String {
    let mut chars = s.chars();
    chars
        .next()
        .map(|c| c.to_lowercase().chain(chars).collect())
        .unwrap_or_default()
}

fn image_too_large(path: &str, len: u64, max: u64) -> ToolError {
    ToolError::Failed(format!(
        "{path}: image too large ({:.1} MB; images are sent up to {:.2} MB). {}",
        len as f64 / 1e6,
        max as f64 / 1e6,
        scale_hint(path)
    ))
}

/// How much of a file too large to send is read to judge it: a non-image or
/// a refused one is told so rather than "too large". A JPEG's size can come
/// later (a camera's metadata runs past it); "too large" is then right anyway.
const IMAGE_HEAD_BYTES: u64 = 64 * 1024;

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
/// Other processes are not held off; this is not a file lock. Each call
/// holds one file; a tool that held two (a move, a copy) would need an order
/// to take them in.
///
/// A run cancelled while the call waited for the file is `Cancelled`: the
/// call must not act.
///
/// [`PathSandbox::check`]: crate::tools::PathSandbox::check
pub(crate) async fn hold_file(
    key: &Path,
    cancel: &tokio_util::sync::CancellationToken,
) -> Result<tokio::sync::OwnedMutexGuard<()>, ToolError> {
    use std::sync::{OnceLock, Weak};
    type Locks = HashMap<PathBuf, Weak<tokio::sync::Mutex<()>>>;
    static LOCKS: OnceLock<Mutex<Locks>> = OnceLock::new();
    let lock = {
        let mut locks = LOCKS
            .get_or_init(Default::default)
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        // Entries nobody holds or waits for any more are dropped here.
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
    let held = lock.lock_owned().await;
    if cancel.is_cancelled() {
        return Err(ToolError::Cancelled);
    }
    Ok(held)
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
        "Read a file's contents. Supports text files with optional offset/limit, and images (PNG, JPEG, WebP; up to 8000 px a side), which are returned as images. Long files are returned one page at a time — the header states the total line count, so pass offset/limit to read further."
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
        let _held = hold_file(&key, &ctx.cancel).await?;

        // Check file exists and size
        let metadata = tokio::fs::metadata(&io_path)
            .await
            .map_err(|e| ToolError::Failed(format!("Cannot access {}: {}", path, e)))?;
        require_regular_file(&metadata, path)?;
        let logged = self.read_log.as_ref().map(|log| (log, key));

        // Images: sent only in a form every provider takes. A block one
        // refuses stays in the history, and every later request fails too.
        if let Some(ext) = image_extension(Path::new(path)) {
            let read_error =
                |e: std::io::Error| ToolError::Failed(format!("Cannot read {path}: {e}"));
            if metadata.len() > self.max_image_bytes {
                use tokio::io::AsyncReadExt;
                let mut head = Vec::new();
                tokio::fs::File::open(&io_path)
                    .await
                    .map_err(read_error)?
                    .take(IMAGE_HEAD_BYTES)
                    .read_to_end(&mut head)
                    .await
                    .map_err(read_error)?;
                // Not an image, or one refused anyway: say that, not "too large".
                check_image(path, &ext, &head, false)?;
                return Err(image_too_large(path, metadata.len(), self.max_image_bytes));
            }
            let bytes = tokio::fs::read(&io_path).await.map_err(read_error)?;
            // Grown since `metadata` (another process: the lock holds off
            // only this one's tools).
            let len = bytes.len() as u64;
            if len > self.max_image_bytes {
                return Err(image_too_large(path, len, self.max_image_bytes));
            }
            let kind = check_image(path, &ext, &bytes, true)?;
            let data = base64::engine::general_purpose::STANDARD.encode(&bytes);
            if let Some((log, key)) = &logged {
                log.record_full(key, Some(FileStamp::of(&metadata)));
            }
            return Ok(ToolResult {
                content: vec![Content::Image {
                    data,
                    mime_type: kind.mime().to_string(),
                }],
                details: serde_json::json!({ "path": path, "bytes": bytes.len() }),
            });
        }

        let offset = params["offset"].as_u64().map(|v| v.max(1) as usize);
        let limit = params["limit"].as_u64().map(|v| v as usize);
        let paged = offset.is_some() || limit.is_some();

        // Text files: check size limit and apply line offset/limit. A file
        // over the limit can still be read a page at a time (the error says
        // so): it is streamed, keeping only the requested lines.
        if metadata.len() as usize > self.max_bytes && !paged {
            return Err(ToolError::Failed(format!(
                "File too large ({} bytes, max {}). Read it in pages with offset/limit \
                 (e.g. offset 1, limit 500).",
                metadata.len(),
                self.max_bytes
            )));
        }

        // No explicit limit falls back to `max_lines` rather than the whole
        // file. The header below reports the true total, so a partial read is
        // visible and the agent can page with offset/limit.
        let effective_limit = limit.unwrap_or(self.max_lines);
        let first = offset.map_or(0, |off| off - 1);
        let (start, end, total, lines) = if metadata.len() as usize > self.max_bytes {
            read_page(
                &io_path,
                path,
                first,
                effective_limit,
                self.max_bytes,
                &ctx.cancel,
            )
            .await?
        } else {
            let content = tokio::fs::read_to_string(&io_path)
                .await
                .map_err(|e| ToolError::Failed(format!("Cannot read {}: {}", path, e)))?;
            let all: Vec<&str> = content.lines().collect();
            let total = all.len();
            let start = first.min(total);
            let end = start.saturating_add(effective_limit).min(total);
            let lines = all[start..end].iter().map(|l| l.to_string()).collect();
            (start, end, total, lines)
        };

        // Always show line numbers — helps agent reference exact lines for edit_file
        let numbered: Vec<String> = lines
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

/// Replace `io_path`'s content with `bytes` whole: write a temp file in the
/// same directory, flush it to disk, and rename it over the target. A crash, a
/// full disk or a write error then leaves the old file intact instead of a
/// truncated one, and a reader never sees a half-written file. (Writing in
/// place truncated first: a failure part-way lost the original.)
///
/// What an in-place write did is kept:
/// - a file that could not be written in place (read-only, another user's) is
///   refused, not replaced — a rename needs only the directory's permission;
/// - a symlink is followed: its target is replaced and the link stays; a
///   dangling one is written through in place, creating its target;
/// - the file's mode is kept, and on Unix its owner and group (best effort);
///   the temp file is created with that mode, so new content is never more
///   readable than the old;
/// - a directory where no file can be created, or a rename it refuses (a
///   sticky directory), falls back to writing in place.
///
/// A rename gives the file a new inode: a hard link to the old one keeps the
/// old content, and ACLs and extended attributes are not carried over.
pub(crate) async fn write_atomic(io_path: &std::path::Path, bytes: &[u8]) -> std::io::Result<()> {
    use tokio::io::AsyncWriteExt;
    let link = tokio::fs::symlink_metadata(io_path)
        .await
        .is_ok_and(|m| m.file_type().is_symlink());
    let target = match tokio::fs::canonicalize(io_path).await {
        Ok(resolved) => resolved,
        // A dangling symlink: write through it, creating what it names.
        Err(_) if link => return tokio::fs::write(io_path, bytes).await,
        Err(_) => io_path.to_path_buf(),
    };
    let existing = tokio::fs::metadata(&target).await.ok();
    if existing.is_some() {
        // Replace only what could be written in place.
        tokio::fs::OpenOptions::new()
            .write(true)
            .open(&target)
            .await?;
    }
    let dir = match target.parent() {
        Some(d) if !d.as_os_str().is_empty() => d.to_path_buf(),
        _ => std::path::PathBuf::from("."),
    };
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.subsec_nanos());
    // Not derived from the file's name: a long one would make it too long.
    let temp = dir.join(format!(".yoagent-{}-{nonce}.tmp", std::process::id()));

    let mut options = tokio::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    if let Some(meta) = &existing {
        use std::os::unix::fs::PermissionsExt;
        options.mode(meta.permissions().mode() & 0o7777);
    }
    let mut file = match options.open(&temp).await {
        Ok(f) => f,
        // No new file can go here (a read-only directory holding a writable
        // file): write in place, as before.
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
            return tokio::fs::write(&target, bytes).await;
        }
        Err(e) => return Err(e),
    };
    let written = async {
        file.write_all(bytes).await?;
        file.sync_all().await?;
        #[cfg(unix)]
        if let Some(meta) = &existing {
            use std::os::unix::fs::MetadataExt;
            let std_file = file.into_std().await;
            // Best effort: only root can give a file to another user.
            let _ = std::os::unix::fs::fchown(&std_file, Some(meta.uid()), Some(meta.gid()));
        } else {
            drop(file);
        }
        #[cfg(not(unix))]
        drop(file);
        if let Some(meta) = &existing {
            // Exact mode: the create was narrowed by the umask.
            tokio::fs::set_permissions(&temp, meta.permissions()).await?;
        }
        match tokio::fs::rename(&temp, &target).await {
            // A sticky directory refuses to replace another user's file the
            // process may still write: write in place, as before.
            Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
                let _ = tokio::fs::remove_file(&temp).await;
                tokio::fs::write(&target, bytes).await
            }
            other => other,
        }
    }
    .await;
    if written.is_err() {
        let _ = tokio::fs::remove_file(&temp).await;
    }
    written
}

/// Lines `[first, first + limit)` of a file too large to load whole, with the
/// file's line count: `(start, end, total, lines)` as the in-memory path
/// computes them. Read in fixed chunks, so memory stays bounded whatever the
/// file: lines before the page are only counted, a kept line is cut at
/// `max_bytes` (a minified file can be one huge line), and the page stops
/// once it holds `max_bytes`, which the header's `end` then shows. Checks
/// `cancel` as it goes (counting the lines reads the whole file).
async fn read_page(
    io_path: &std::path::Path,
    path: &str,
    first: usize,
    limit: usize,
    max_bytes: usize,
    cancel: &tokio_util::sync::CancellationToken,
) -> Result<(usize, usize, usize, Vec<String>), ToolError> {
    use tokio::io::AsyncReadExt;
    let fail = |e: std::io::Error| ToolError::Failed(format!("Cannot read {}: {}", path, e));
    let mut file = tokio::fs::File::open(io_path).await.map_err(fail)?;
    let mut buf = vec![0u8; 64 * 1024];
    let (mut line_no, mut bytes) = (0usize, 0usize);
    let mut lines: Vec<String> = Vec::new();
    let mut full = false;
    let (mut cur, mut cur_cut, mut open) = (Vec::new(), false, false);

    // End the current line: keep it if it is in the page and fits.
    let mut finish = |cur: &mut Vec<u8>, cut: &mut bool, line_no: usize| -> Result<(), ToolError> {
        let collecting = line_no >= first && !full && lines.len() < limit;
        if collecting {
            if cur.last() == Some(&b'\r') && !*cut {
                cur.pop();
            }
            if !lines.is_empty() && bytes + cur.len() > max_bytes {
                full = true;
            } else {
                let text = if *cut {
                    format!(
                        "{} … (line cut at {max_bytes} bytes)",
                        String::from_utf8_lossy(cur)
                    )
                } else {
                    String::from_utf8(std::mem::take(cur)).map_err(|_| {
                        ToolError::Failed(format!(
                            "Cannot read {path}: line {} is not valid UTF-8",
                            line_no + 1
                        ))
                    })?
                };
                bytes += text.len();
                lines.push(text);
            }
        }
        cur.clear();
        *cut = false;
        Ok(())
    };
    let mut chunks = 0usize;
    loop {
        if chunks % 64 == 0 && cancel.is_cancelled() {
            return Err(ToolError::Cancelled);
        }
        chunks += 1;
        let n = file.read(&mut buf).await.map_err(fail)?;
        if n == 0 {
            break;
        }
        let mut data = &buf[..n];
        while !data.is_empty() {
            let wanted = line_no >= first && line_no < first.saturating_add(limit);
            let (piece, ends) = match data.iter().position(|&b| b == b'\n') {
                Some(i) => (&data[..i], Some(i + 1)),
                None => (data, None),
            };
            if wanted {
                let room = max_bytes.saturating_sub(cur.len());
                cur.extend_from_slice(&piece[..piece.len().min(room)]);
                cur_cut |= piece.len() > room;
            }
            match ends {
                Some(next) => {
                    finish(&mut cur, &mut cur_cut, line_no)?;
                    line_no += 1;
                    open = false;
                    data = &data[next..];
                }
                None => {
                    open = true;
                    data = &[];
                }
            }
        }
    }
    if open {
        finish(&mut cur, &mut cur_cut, line_no)?;
        line_no += 1;
    }
    let total = line_no;
    let start = first.min(total);
    Ok((start, start + lines.len(), total, lines))
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
        let _held = hold_file(&key, &ctx.cancel).await?;
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

        write_atomic(&io_path, content.as_bytes())
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

#[cfg(test)]
mod tests {
    use super::*;
    use tokio_util::sync::CancellationToken;

    /// A call that waited for the file while its run was cancelled does not
    /// act once it gets the file.
    #[tokio::test]
    async fn a_call_cancelled_while_it_waited_for_the_file_does_not_act() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("f.txt");
        std::fs::write(&path, "before").unwrap();
        let p = path.to_str().unwrap().to_owned();
        let key = crate::tools::PathSandbox::default().check(&p).unwrap();
        let held = hold_file(&key, &CancellationToken::new()).await.unwrap();

        let cancel = CancellationToken::new();
        let ctx = ToolContext::new("t", "write_file").with_cancel(cancel.clone());
        let write = tokio::spawn(async move {
            WriteFileTool::new()
                .execute(serde_json::json!({"path": p, "content": "after"}), ctx)
                .await
        });
        // The write is past its first cancel check, waiting for the file.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert!(!write.is_finished());
        cancel.cancel();
        drop(held);
        assert!(matches!(write.await.unwrap(), Err(ToolError::Cancelled)));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "before");
    }
}
