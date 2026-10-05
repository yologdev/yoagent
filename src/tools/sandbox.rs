//! Path sandboxing for the built-in file tools.
//!
//! A tool configured with allowed roots must reject every path outside them —
//! including paths that *reach* outside via `..` or a symlink. Resolving the
//! path lexically is not enough: `allowed/../../etc/passwd` and a symlink from
//! `allowed/link` to `/etc` both look fine as strings.
//!
//! [`PathSandbox`] resolves the path against the filesystem first, then
//! compares. An empty sandbox allows everything, which is the default for all
//! built-in tools — sandboxing is opt-in, but once opted into it is enforced.
//!
//! # Example
//!
//! ```rust
//! use yoagent::tools::sandbox::PathSandbox;
//!
//! let sandbox = PathSandbox::new(vec!["/srv/workspace".to_string()]);
//! assert!(sandbox.check("/etc/passwd").is_err());
//! ```

use crate::types::ToolError;
use std::path::{Component, Path, PathBuf};

/// Allowed filesystem roots for a tool. Empty means unrestricted.
#[derive(Debug, Clone, Default)]
pub struct PathSandbox {
    roots: Vec<PathBuf>,
}

impl PathSandbox {
    /// Build a sandbox from allowed root paths.
    ///
    /// Roots are canonicalized when they exist; a root that does not exist is
    /// kept as given (normalized) so a workspace created later still matches.
    pub fn new(roots: Vec<String>) -> Self {
        let roots = roots
            .iter()
            .map(|r| {
                let p = PathBuf::from(r);
                std::fs::canonicalize(&p).unwrap_or_else(|_| normalize_lexically(&p))
            })
            .collect();
        Self { roots }
    }

    /// [`check`](Self::check) for a tool about to touch `path`: the path to do
    /// the I/O on. Unrestricted, that is `path` as given, so output keeps the
    /// caller's spelling. Restricted, it is the resolved path that was
    /// checked, so the OS cannot reach anything the check did not see.
    pub fn io_path(&self, path: &str) -> Result<PathBuf, ToolError> {
        let resolved = self.check(path)?;
        Ok(if self.is_unrestricted() {
            PathBuf::from(path)
        } else {
            resolved
        })
    }

    /// Whether any restriction applies.
    pub fn is_unrestricted(&self) -> bool {
        self.roots.is_empty()
    }

    /// Resolve `path` and verify it falls inside an allowed root.
    ///
    /// Returns the resolved path on success. For paths that do not exist yet
    /// (a file about to be written), the nearest existing ancestor is resolved
    /// and the remainder appended — so a write through a symlinked parent is
    /// still checked against the symlink's target.
    pub fn check(&self, path: &str) -> Result<PathBuf, ToolError> {
        let resolved = resolve(Path::new(path));
        if self.is_unrestricted() {
            return Ok(resolved);
        }
        if self.roots.iter().any(|root| resolved.starts_with(root)) {
            Ok(resolved)
        } else {
            // Deliberately does not echo the allowed roots: the model does not
            // need the sandbox layout, and tool results reach the transcript.
            Err(ToolError::Failed(format!(
                "path '{path}' is outside the allowed directories"
            )))
        }
    }
}

/// Resolve a path against the filesystem, falling back to lexical
/// normalization for the part that does not exist yet.
fn resolve(path: &Path) -> PathBuf {
    if let Ok(c) = std::fs::canonicalize(path) {
        return c;
    }
    // Walk up to the nearest existing ancestor, canonicalize that, and
    // re-append the trailing components. This is what makes a not-yet-created
    // file under a symlinked directory resolve to its real location.
    let mut trailing: Vec<std::ffi::OsString> = Vec::new();
    let mut climbs = false;
    let mut current = path;
    loop {
        match current.parent() {
            Some(parent) => {
                // Keep every component, `..` included: `file_name()` is `None`
                // for `..`, and dropping it made `root/new/../../x` resolve to
                // `root/new/x` — inside the root — while the OS wrote `x` one
                // level above it.
                if let Some(last) = current.components().next_back() {
                    climbs |= last == Component::ParentDir;
                    trailing.push(last.as_os_str().to_owned());
                }
                if let Ok(base) = std::fs::canonicalize(parent) {
                    let mut out = base;
                    for part in trailing.iter().rev() {
                        out.push(part);
                    }
                    let out = normalize_lexically(&out);
                    // The missing components will be created as real
                    // directories, so collapsing their `..` lexically matches
                    // what the OS does. What follows may name an existing
                    // symlink (`root/new/../link/x`), so resolve the collapsed
                    // path again; it has no `..` left, so this ends.
                    return if climbs { resolve(&out) } else { out };
                }
                current = parent;
            }
            None => return normalize_lexically(path),
        }
    }
}

/// Collapse `.` and `..` textually. Only used when the filesystem cannot
/// resolve the path at all.
fn normalize_lexically(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    if out.as_os_str().is_empty() {
        // A purely relative path that collapsed to nothing resolves against cwd.
        std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
    } else if out.is_relative() {
        std::env::current_dir()
            .map(|cwd| cwd.join(&out))
            .unwrap_or(out)
    } else {
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn empty_sandbox_allows_anything() {
        let s = PathSandbox::default();
        assert!(s.is_unrestricted());
        assert!(s.check("/etc/passwd").is_ok());
    }

    #[test]
    fn inside_allowed_root_passes_outside_is_rejected() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("ok.txt"), "x").unwrap();
        let s = PathSandbox::new(vec![tmp.path().to_string_lossy().to_string()]);

        assert!(s.check(tmp.path().join("ok.txt").to_str().unwrap()).is_ok());
        assert!(s.check("/etc/passwd").is_err());
    }

    #[test]
    fn traversal_out_of_the_root_is_rejected() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("workspace");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(tmp.path().join("secret.txt"), "s").unwrap();
        let s = PathSandbox::new(vec![root.to_string_lossy().to_string()]);

        // Lexically this starts with the root; it must still be rejected.
        let escape = root.join("../secret.txt");
        assert!(
            s.check(escape.to_str().unwrap()).is_err(),
            "`..` must not escape the sandbox"
        );
    }

    #[test]
    fn traversal_through_a_missing_directory_is_rejected() {
        // `x` does not exist: the walk-up used to drop the `..` components
        // and resolve this to `root/x/escaped.txt`.
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("workspace");
        std::fs::create_dir_all(&root).unwrap();
        let s = PathSandbox::new(vec![root.to_string_lossy().to_string()]);

        for escape in ["x/../../escaped.txt", "x/y/../../../escaped.txt"] {
            let p = root.join(escape);
            assert!(
                s.check(p.to_str().unwrap()).is_err(),
                "{escape} must not escape the sandbox"
            );
        }
        // Climbing back inside the root is fine.
        let inside = root.join("x/../ok.txt");
        assert!(s.check(inside.to_str().unwrap()).is_ok());
    }

    #[cfg(unix)]
    #[test]
    fn traversal_through_a_missing_directory_into_a_symlink_is_rejected() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("workspace");
        std::fs::create_dir_all(&root).unwrap();
        let outside = tmp.path().join("outside");
        std::fs::create_dir_all(&outside).unwrap();
        std::os::unix::fs::symlink(&outside, root.join("link")).unwrap();
        let s = PathSandbox::new(vec![root.to_string_lossy().to_string()]);

        let p = root.join("new/../link/x.txt");
        assert!(
            s.check(p.to_str().unwrap()).is_err(),
            "a `..` that lands on a symlink out of the root must be rejected"
        );
    }

    #[test]
    fn io_path_is_the_checked_path_only_when_restricted() {
        assert_eq!(
            PathSandbox::default().io_path("rel/a.txt").unwrap(),
            PathBuf::from("rel/a.txt")
        );
        let tmp = TempDir::new().unwrap();
        let s = PathSandbox::new(vec![tmp.path().to_string_lossy().to_string()]);
        let p = tmp.path().join("a/../b.txt");
        assert_eq!(
            s.io_path(p.to_str().unwrap()).unwrap(),
            std::fs::canonicalize(tmp.path()).unwrap().join("b.txt")
        );
    }

    #[test]
    fn nonexistent_file_under_allowed_root_is_permitted() {
        // Writes target paths that do not exist yet — they must still resolve.
        let tmp = TempDir::new().unwrap();
        let s = PathSandbox::new(vec![tmp.path().to_string_lossy().to_string()]);
        let new_file = tmp.path().join("nested/deep/new.txt");
        assert!(s.check(new_file.to_str().unwrap()).is_ok());
    }

    #[test]
    fn nonexistent_path_outside_root_is_rejected() {
        let tmp = TempDir::new().unwrap();
        let s = PathSandbox::new(vec![tmp.path().join("ws").to_string_lossy().to_string()]);
        assert!(s.check("/nonexistent-elsewhere/x.txt").is_err());
    }

    #[cfg(unix)]
    #[test]
    fn symlink_out_of_the_root_is_rejected() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("workspace");
        std::fs::create_dir_all(&root).unwrap();
        let outside = tmp.path().join("outside");
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("secret.txt"), "s").unwrap();
        std::os::unix::fs::symlink(&outside, root.join("link")).unwrap();

        let s = PathSandbox::new(vec![root.to_string_lossy().to_string()]);
        assert!(
            s.check(root.join("link/secret.txt").to_str().unwrap())
                .is_err(),
            "a symlink pointing out of the sandbox must not grant access"
        );
    }

    #[test]
    fn error_does_not_disclose_the_allowed_roots() {
        let s = PathSandbox::new(vec!["/srv/secret-workspace-name".to_string()]);
        let err = s.check("/etc/passwd").unwrap_err().to_string();
        assert!(
            !err.contains("secret-workspace-name"),
            "leaked roots: {err}"
        );
    }
}
