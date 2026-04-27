//! Shared key-value state for sub-agent communication.
//!
//! `SharedState` wraps an `Arc<RwLock<HashMap<String, String>>>` so multiple
//! sub-agents (and the parent) can read/write named variables without re-pasting
//! large artifacts into every prompt.
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
use std::sync::Arc;
use tokio::sync::RwLock;

/// Default capacity: 10 MB.
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

/// A shared string key-value store for sub-agent communication.
///
/// Cheaply cloneable (wraps `Arc`). All methods acquire and release locks
/// within a single call — no holding across await points.
#[derive(Clone)]
pub struct SharedState {
    inner: Arc<RwLock<HashMap<String, String>>>,
    max_bytes: usize,
}

impl SharedState {
    /// Create a new store with the default 10 MB capacity.
    pub fn new() -> Self {
        Self {
            inner: Arc::new(RwLock::new(HashMap::new())),
            max_bytes: DEFAULT_MAX_BYTES,
        }
    }

    /// Create a new store with a custom byte capacity.
    pub fn with_max_bytes(max_bytes: usize) -> Self {
        Self {
            inner: Arc::new(RwLock::new(HashMap::new())),
            max_bytes,
        }
    }

    /// Get a value by key. Returns `None` if the key doesn't exist.
    pub async fn get(&self, key: &str) -> Option<String> {
        self.inner.read().await.get(key).cloned()
    }

    /// Store a value. Returns `Err(CapacityError)` if the total size
    /// (excluding the old value for this key, if any) would exceed capacity.
    pub async fn set(&self, key: &str, value: String) -> Result<(), CapacityError> {
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
            });
        }

        map.insert(key.to_string(), value);
        Ok(())
    }

    /// Remove a key. Returns `true` if the key existed.
    pub async fn remove(&self, key: &str) -> bool {
        self.inner.write().await.remove(key).is_some()
    }

    /// List all keys (sorted).
    pub async fn keys(&self) -> Vec<String> {
        let map = self.inner.read().await;
        let mut keys: Vec<String> = map.keys().cloned().collect();
        keys.sort();
        keys
    }

    /// Human-readable summary of stored variables (key names + byte sizes).
    /// Suitable for injecting into a system prompt.
    pub async fn summary(&self) -> String {
        let map = self.inner.read().await;
        if map.is_empty() {
            return "(empty)".to_string();
        }
        let mut entries: Vec<_> = map.iter().map(|(k, v)| (k.clone(), v.len())).collect();
        entries.sort_by(|a, b| a.0.cmp(&b.0));

        entries
            .iter()
            .map(|(k, size)| format_entry(k, *size))
            .collect::<Vec<_>>()
            .join(", ")
    }
}

impl Default for SharedState {
    fn default() -> Self {
        Self::new()
    }
}

fn format_entry(key: &str, bytes: usize) -> String {
    if bytes >= 1024 * 1024 {
        format!("{} ({:.1}MB)", key, bytes as f64 / (1024.0 * 1024.0))
    } else if bytes >= 1024 {
        format!("{} ({:.1}KB)", key, bytes as f64 / 1024.0)
    } else {
        format!("{} ({}B)", key, bytes)
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
        assert!(s.contains("B)"));
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
}
