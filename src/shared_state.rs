//! Shared key-value state for sub-agent communication.
//!
//! `SharedState` is a pluggable key-value store that multiple sub-agents (and
//! the parent) can read/write. The default backend is in-memory; a filesystem
//! backend is also available for persistence and large artifacts.
//!
//! # Example
//!
//! ```rust,no_run
//! use yoagent::shared_state::SharedState;
//!
//! # async fn example() {
//! let state = SharedState::new();
//! state.set("log", "big CI output...".into()).await.unwrap();
//!
//! assert_eq!(state.get("log").await, Some("big CI output...".into()));
//! assert_eq!(state.keys().await, vec!["log"]);
//! # }
//! ```

use std::collections::HashMap;
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::RwLock;

/// Default capacity for the memory backend: 10 MB.
const DEFAULT_MAX_BYTES: usize = 10 * 1024 * 1024;

/// Error returned when a `set` would exceed the capacity limit.
#[derive(Debug, Clone)]
pub struct CapacityError {
    pub key: String,
    pub value_bytes: usize,
    pub current_bytes: usize,
    pub max_bytes: usize,
}

impl fmt::Display for CapacityError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "SharedState capacity exceeded: storing '{}' ({} bytes) would bring total to {} / {} bytes",
            self.key, self.value_bytes, self.current_bytes + self.value_bytes, self.max_bytes
        )
    }
}

impl std::error::Error for CapacityError {}

/// Error type for shared state operations.
#[derive(Debug)]
pub enum SharedStateError {
    Capacity(CapacityError),
    Io(std::io::Error),
}

impl fmt::Display for SharedStateError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Capacity(e) => write!(f, "{}", e),
            Self::Io(e) => write!(f, "SharedState I/O error: {}", e),
        }
    }
}

impl std::error::Error for SharedStateError {}

impl From<CapacityError> for SharedStateError {
    fn from(e: CapacityError) -> Self {
        Self::Capacity(e)
    }
}

impl From<std::io::Error> for SharedStateError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}

// ---------------------------------------------------------------------------
// Backend trait
// ---------------------------------------------------------------------------

/// Pluggable storage backend for `SharedState`.
///
/// Implement this trait to back shared state with a custom store
/// (database, Redis, HTTP service, etc.).
#[async_trait::async_trait]
pub trait SharedStateBackend: Send + Sync {
    /// Get a value by key. Returns `None` if the key doesn't exist.
    async fn get(&self, key: &str) -> Result<Option<String>, SharedStateError>;

    /// Store a value. Implementations should enforce their own capacity limits.
    async fn set(&self, key: &str, value: String) -> Result<(), SharedStateError>;

    /// Remove a key. Returns `true` if the key existed.
    async fn remove(&self, key: &str) -> Result<bool, SharedStateError>;

    /// List all keys (sorted).
    async fn keys(&self) -> Result<Vec<String>, SharedStateError>;

    /// Human-readable summary of stored variables (key names + sizes).
    async fn summary(&self) -> Result<String, SharedStateError>;
}

// ---------------------------------------------------------------------------
// Memory backend (default)
// ---------------------------------------------------------------------------

/// In-memory backend backed by `HashMap` with a byte capacity limit.
pub struct MemoryBackend {
    inner: RwLock<HashMap<String, String>>,
    max_bytes: usize,
}

impl Default for MemoryBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl MemoryBackend {
    pub fn new() -> Self {
        Self {
            inner: RwLock::new(HashMap::new()),
            max_bytes: DEFAULT_MAX_BYTES,
        }
    }

    pub fn with_max_bytes(max_bytes: usize) -> Self {
        Self {
            inner: RwLock::new(HashMap::new()),
            max_bytes,
        }
    }
}

#[async_trait::async_trait]
impl SharedStateBackend for MemoryBackend {
    async fn get(&self, key: &str) -> Result<Option<String>, SharedStateError> {
        Ok(self.inner.read().await.get(key).cloned())
    }

    async fn set(&self, key: &str, value: String) -> Result<(), SharedStateError> {
        let mut map = self.inner.write().await;

        // Calculate current total excluding the old value for this key.
        let current: usize = map
            .iter()
            .filter(|(k, _)| k.as_str() != key)
            .map(|(k, v)| k.len() + v.len())
            .sum();
        let new_entry = key.len() + value.len();

        if current + new_entry > self.max_bytes {
            return Err(CapacityError {
                key: key.to_string(),
                value_bytes: value.len(),
                current_bytes: current,
                max_bytes: self.max_bytes,
            }
            .into());
        }

        map.insert(key.to_string(), value);
        Ok(())
    }

    async fn remove(&self, key: &str) -> Result<bool, SharedStateError> {
        Ok(self.inner.write().await.remove(key).is_some())
    }

    async fn keys(&self) -> Result<Vec<String>, SharedStateError> {
        let map = self.inner.read().await;
        let mut keys: Vec<String> = map.keys().cloned().collect();
        keys.sort();
        Ok(keys)
    }

    async fn summary(&self) -> Result<String, SharedStateError> {
        let map = self.inner.read().await;
        Ok(format_summary(
            map.iter().map(|(k, v)| (k.as_str(), v.len())),
        ))
    }
}

// ---------------------------------------------------------------------------
// Filesystem backend
// ---------------------------------------------------------------------------

/// Filesystem backend — each key is stored as a file in a directory.
///
/// Keys are sanitized to safe filenames. Values are stored as plain text
/// (no extension) for easy inspection and debugging.
///
/// ```rust,no_run
/// use yoagent::shared_state::{SharedState, FileBackend};
///
/// # async fn example() {
/// let state = SharedState::with_backend(FileBackend::new("/tmp/agent-state"));
/// state.set("summary", "analysis results...".into()).await.unwrap();
/// // Creates /tmp/agent-state/summary with the content
/// # }
/// ```
pub struct FileBackend {
    dir: PathBuf,
}

impl FileBackend {
    /// Create a new filesystem backend. The directory is created lazily on first write.
    pub fn new(dir: impl AsRef<Path>) -> Self {
        Self {
            dir: dir.as_ref().to_path_buf(),
        }
    }

    /// Encode a key into a safe, reversible filename.
    /// Percent-encodes any character that isn't alphanumeric, `-`, `_`, or `.`.
    /// This avoids collisions: distinct keys always produce distinct filenames.
    fn key_to_path(&self, key: &str) -> PathBuf {
        let encoded: String = key
            .chars()
            .map(|c| {
                if c.is_alphanumeric() || c == '-' || c == '_' || c == '.' {
                    c.to_string()
                } else {
                    format!("%{:02X}", c as u32)
                }
            })
            .collect();
        self.dir.join(encoded)
    }

    /// Decode a filename back into the original key.
    fn path_to_key(filename: &str) -> String {
        let mut result = String::new();
        let mut chars = filename.chars();
        while let Some(c) = chars.next() {
            if c == '%' {
                let hex: String = chars.by_ref().take(2).collect();
                if let Ok(code) = u32::from_str_radix(&hex, 16) {
                    if let Some(decoded) = char::from_u32(code) {
                        result.push(decoded);
                        continue;
                    }
                }
                // Fallback: keep the raw percent sequence
                result.push('%');
                result.push_str(&hex);
            } else {
                result.push(c);
            }
        }
        result
    }
}

#[async_trait::async_trait]
impl SharedStateBackend for FileBackend {
    async fn get(&self, key: &str) -> Result<Option<String>, SharedStateError> {
        let path = self.key_to_path(key);
        match tokio::fs::read_to_string(&path).await {
            Ok(content) => Ok(Some(content)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    async fn set(&self, key: &str, value: String) -> Result<(), SharedStateError> {
        tokio::fs::create_dir_all(&self.dir).await?;
        let path = self.key_to_path(key);
        tokio::fs::write(&path, &value).await?;
        Ok(())
    }

    async fn remove(&self, key: &str) -> Result<bool, SharedStateError> {
        let path = self.key_to_path(key);
        match tokio::fs::remove_file(&path).await {
            Ok(()) => Ok(true),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(e) => Err(e.into()),
        }
    }

    async fn keys(&self) -> Result<Vec<String>, SharedStateError> {
        let mut keys = Vec::new();
        let mut entries = match tokio::fs::read_dir(&self.dir).await {
            Ok(entries) => entries,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(keys),
            Err(e) => return Err(e.into()),
        };
        while let Some(entry) = entries.next_entry().await? {
            if let Some(name) = entry.file_name().to_str() {
                // Skip hidden files
                if !name.starts_with('.') {
                    keys.push(Self::path_to_key(name));
                }
            }
        }
        keys.sort();
        Ok(keys)
    }

    async fn summary(&self) -> Result<String, SharedStateError> {
        let mut entries = Vec::new();
        let mut dir = match tokio::fs::read_dir(&self.dir).await {
            Ok(dir) => dir,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok("(empty)".to_string()),
            Err(e) => return Err(e.into()),
        };
        while let Some(entry) = dir.next_entry().await? {
            if let Some(name) = entry.file_name().to_str() {
                if !name.starts_with('.') {
                    let meta = entry.metadata().await?;
                    entries.push((Self::path_to_key(name), meta.len() as usize));
                }
            }
        }
        entries.sort_by(|a, b| a.0.cmp(&b.0));
        Ok(format_summary(
            entries.iter().map(|(k, s)| (k.as_str(), *s)),
        ))
    }
}

// ---------------------------------------------------------------------------
// SharedState (public API)
// ---------------------------------------------------------------------------

/// A shared string key-value store for sub-agent communication.
///
/// Cheaply cloneable (wraps `Arc`). Delegates all operations to a
/// pluggable [`SharedStateBackend`].
#[derive(Clone)]
pub struct SharedState {
    backend: Arc<dyn SharedStateBackend>,
}

impl SharedState {
    /// Create a new in-memory store with the default 10 MB capacity.
    pub fn new() -> Self {
        Self {
            backend: Arc::new(MemoryBackend::new()),
        }
    }

    /// Create a new in-memory store with a custom byte capacity.
    pub fn with_max_bytes(max_bytes: usize) -> Self {
        Self {
            backend: Arc::new(MemoryBackend::with_max_bytes(max_bytes)),
        }
    }

    /// Create a store backed by a custom backend.
    pub fn with_backend(backend: impl SharedStateBackend + 'static) -> Self {
        Self {
            backend: Arc::new(backend),
        }
    }

    /// Get a value by key. Returns `None` if the key doesn't exist.
    pub async fn get(&self, key: &str) -> Option<String> {
        match self.backend.get(key).await {
            Ok(val) => val,
            Err(e) => {
                eprintln!("[SharedState] get({:?}) error: {}", key, e);
                None
            }
        }
    }

    /// Store a value. Returns `Err` if the backend rejects it (capacity, I/O, etc.).
    pub async fn set(&self, key: &str, value: String) -> Result<(), SharedStateError> {
        self.backend.set(key, value).await
    }

    /// Remove a key. Returns `true` if the key existed.
    pub async fn remove(&self, key: &str) -> bool {
        match self.backend.remove(key).await {
            Ok(existed) => existed,
            Err(e) => {
                eprintln!("[SharedState] remove({:?}) error: {}", key, e);
                false
            }
        }
    }

    /// List all keys (sorted).
    pub async fn keys(&self) -> Vec<String> {
        match self.backend.keys().await {
            Ok(keys) => keys,
            Err(e) => {
                eprintln!("[SharedState] keys() error: {}", e);
                Vec::new()
            }
        }
    }

    /// Human-readable summary of stored variables (key names + byte sizes).
    /// Suitable for injecting into a system prompt.
    pub async fn summary(&self) -> String {
        match self.backend.summary().await {
            Ok(s) => s,
            Err(e) => {
                eprintln!("[SharedState] summary() error: {}", e);
                "(error reading state)".to_string()
            }
        }
    }
}

impl Default for SharedState {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn format_summary<'a>(entries: impl Iterator<Item = (&'a str, usize)>) -> String {
    let entries: Vec<_> = entries.collect();
    if entries.is_empty() {
        return "(empty)".to_string();
    }
    entries
        .iter()
        .map(|(k, size)| format_entry(k, *size))
        .collect::<Vec<_>>()
        .join(", ")
}

fn format_entry(key: &str, bytes: usize) -> String {
    if bytes >= 1024 * 1024 {
        format!("{} ({:.1} MB)", key, bytes as f64 / (1024.0 * 1024.0))
    } else if bytes >= 1024 {
        format!("{} ({:.1} KB)", key, bytes as f64 / 1024.0)
    } else {
        format!("{} ({} bytes)", key, bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_get_set_remove() {
        let state = SharedState::new();
        assert_eq!(state.get("x").await, None);

        state.set("x", "hello".into()).await.unwrap();
        assert_eq!(state.get("x").await, Some("hello".into()));

        assert!(state.remove("x").await);
        assert_eq!(state.get("x").await, None);
        assert!(!state.remove("x").await);
    }

    #[tokio::test]
    async fn test_keys_sorted() {
        let state = SharedState::new();
        state.set("c", "3".into()).await.unwrap();
        state.set("a", "1".into()).await.unwrap();
        state.set("b", "2".into()).await.unwrap();
        assert_eq!(state.keys().await, vec!["a", "b", "c"]);
    }

    #[tokio::test]
    async fn test_overwrite_same_key() {
        let state = SharedState::with_max_bytes(100);
        state.set("k", "short".into()).await.unwrap();
        state.set("k", "also short".into()).await.unwrap();
        assert_eq!(state.get("k").await, Some("also short".into()));
    }

    #[tokio::test]
    async fn test_capacity_limit() {
        let state = SharedState::with_max_bytes(20);
        state.set("a", "12345".into()).await.unwrap(); // 1 + 5 = 6 bytes
        let err = state.set("b", "12345678901234567890".into()).await;
        assert!(err.is_err());
        let e = err.unwrap_err();
        assert!(e.to_string().contains("capacity exceeded"));
    }

    #[tokio::test]
    async fn test_overwrite_within_capacity() {
        let state = SharedState::with_max_bytes(30);
        state.set("k", "aaaaaaaaaa".into()).await.unwrap(); // 1+10=11
                                                            // Overwrite with larger value — old value excluded from budget
        state.set("k", "bbbbbbbbbbbbbbbbbb".into()).await.unwrap(); // 1+18=19
        assert_eq!(state.get("k").await, Some("bbbbbbbbbbbbbbbbbb".into()));
    }

    #[tokio::test]
    async fn test_summary_formatting() {
        let state = SharedState::new();
        assert_eq!(state.summary().await, "(empty)");

        state.set("small", "hi".into()).await.unwrap();
        let s = state.summary().await;
        assert!(s.contains("small"));
        assert!(s.contains("bytes)"));
    }

    #[tokio::test]
    async fn test_concurrent_access() {
        let state = SharedState::new();
        let mut handles = vec![];
        for i in 0..10 {
            let s = state.clone();
            handles.push(tokio::spawn(async move {
                s.set(&format!("k{}", i), format!("v{}", i)).await.unwrap();
            }));
        }
        for h in handles {
            h.await.unwrap();
        }
        assert_eq!(state.keys().await.len(), 10);
    }

    #[tokio::test]
    async fn test_file_backend() {
        let dir = tempfile::tempdir().unwrap();
        let state = SharedState::with_backend(FileBackend::new(dir.path()));

        // Empty state
        assert_eq!(state.get("x").await, None);
        assert_eq!(state.keys().await, Vec::<String>::new());
        assert_eq!(state.summary().await, "(empty)");

        // Set and get
        state.set("report", "analysis done".into()).await.unwrap();
        assert_eq!(state.get("report").await, Some("analysis done".into()));

        // File actually exists on disk
        let content = std::fs::read_to_string(dir.path().join("report")).unwrap();
        assert_eq!(content, "analysis done");

        // Keys
        state.set("log", "build output".into()).await.unwrap();
        assert_eq!(state.keys().await, vec!["log", "report"]);

        // Summary
        let summary = state.summary().await;
        assert!(summary.contains("report"));
        assert!(summary.contains("log"));

        // Remove
        assert!(state.remove("report").await);
        assert_eq!(state.get("report").await, None);
        assert!(!state.remove("report").await);
    }

    #[tokio::test]
    async fn test_file_backend_key_encoding() {
        let dir = tempfile::tempdir().unwrap();
        let state = SharedState::with_backend(FileBackend::new(dir.path()));

        // Keys with special chars are percent-encoded (reversible)
        state
            .set("summary:src/main.rs", "file analysis".into())
            .await
            .unwrap();
        assert_eq!(
            state.get("summary:src/main.rs").await,
            Some("file analysis".into())
        );

        // The file on disk uses percent-encoded name
        let encoded = dir.path().join("summary%3Asrc%2Fmain.rs");
        assert!(encoded.exists());

        // keys() returns the original key, not the filename
        let keys = state.keys().await;
        assert!(keys.contains(&"summary:src/main.rs".to_string()));

        // No collision: distinct keys produce distinct files
        state
            .set("summary_src_main.rs", "different".into())
            .await
            .unwrap();
        assert_eq!(
            state.get("summary:src/main.rs").await,
            Some("file analysis".into())
        );
        assert_eq!(
            state.get("summary_src_main.rs").await,
            Some("different".into())
        );
        assert_eq!(state.keys().await.len(), 2);
    }

    #[tokio::test]
    async fn test_with_backend() {
        // Verify with_backend works with MemoryBackend directly
        let state = SharedState::with_backend(MemoryBackend::new());
        state.set("k", "v".into()).await.unwrap();
        assert_eq!(state.get("k").await, Some("v".into()));
    }
}
