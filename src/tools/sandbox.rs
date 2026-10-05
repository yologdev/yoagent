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
    /// checked, so the I/O targets exactly what was checked (as of the check:
    /// something changing the tree in between, such as a parallel `bash`
    /// call, is not covered).
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
        let resolved = resolve(Path::new(path), 0);
        if self.is_unrestricted() {
            return Ok(resolved.unwrap_or_else(|| PathBuf::from(path)));
        }
        let Some(resolved) = resolved else {
            return Err(ToolError::Failed(format!(
                "path '{path}' could not be resolved (symlink loop?)"
            )));
        };
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

/// How many symlinks [`resolve`] follows by hand before giving up (the OS
/// limit is similar; past it the path is refused, not guessed).
const MAX_SYMLINK_HOPS: u8 = 40;

/// Resolve a path against the filesystem, falling back to lexical
/// normalization for the part that does not exist yet. `None` when it cannot
/// be resolved safely (a symlink loop, or an unreadable link).
fn resolve(path: &Path, hops: u8) -> Option<PathBuf> {
    // Anchor a relative path at the working directory first. Otherwise a walk
    // that finds no existing prefix (`a/../link/x`, `a` missing) ends at ""
    // and falls back to lexical normalization, skipping the symlink checks.
    let anchored;
    let path = if path.is_relative() {
        anchored = std::env::current_dir().ok()?.join(path);
        anchored.as_path()
    } else {
        path
    };
    if let Ok(c) = std::fs::canonicalize(path) {
        return Some(c);
    }
    // Walk up to the nearest existing ancestor, canonicalize that, and
    // re-append the trailing components. This is what makes a not-yet-created
    // file under a symlinked directory resolve to its real location.
    let mut trailing: Vec<std::ffi::OsString> = Vec::new();
    let mut climbs = false;
    let mut current = path;
    loop {
        // `current` could not be canonicalized. If it exists anyway, it is a
        // dangling (or looping) symlink: writing through it creates the
        // target, so follow it rather than treat its name as a plain file.
        if std::fs::symlink_metadata(current).is_ok_and(|m| m.file_type().is_symlink()) {
            if hops >= MAX_SYMLINK_HOPS {
                return None;
            }
            let target = std::fs::read_link(current).ok()?;
            let mut next = match current.parent() {
                Some(parent) if target.is_relative() => parent.join(target),
                _ => target,
            };
            for part in trailing.iter().rev() {
                next.push(part);
            }
            return resolve(&next, hops + 1);
        }
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
                    // Collapse the trailing `..` lexically. Sandboxed tools do
                    // their I/O on this collapsed path (`io_path`), so the
                    // check and the I/O agree. A component left after
                    // collapsing may be an existing symlink
                    // (`root/new/../link/x`), so resolve once more; there is
                    // no `..` left, so this ends.
                    let out = normalize_lexically(&out);
                    return if climbs {
                        resolve(&out, hops + 1)
                    } else {
                        Some(out)
                    };
                }
                current = parent;
            }
            None => return Some(normalize_lexically(path)),
        }
    }
}

/// Collapse `.` and `..` textually, without touching the filesystem.
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

    #[cfg(unix)]
    #[test]
    fn a_dangling_symlink_out_of_the_root_is_rejected() {
        // `link` points at a file that does not exist yet, outside the root.
        // canonicalize fails, so the name used to pass as a plain new file —
        // and a write through it created the target outside.
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("workspace");
        std::fs::create_dir_all(&root).unwrap();
        std::os::unix::fs::symlink(tmp.path().join("pwned.txt"), root.join("link")).unwrap();
        std::os::unix::fs::symlink("../elsewhere/x", root.join("rel")).unwrap();
        let s = PathSandbox::new(vec![root.to_string_lossy().to_string()]);

        for name in ["link", "rel", "rel/deeper.txt"] {
            assert!(
                s.check(root.join(name).to_str().unwrap()).is_err(),
                "{name} must not reach outside the root"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn a_dangling_symlink_inside_the_root_resolves_to_its_target() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("workspace");
        std::fs::create_dir_all(&root).unwrap();
        std::os::unix::fs::symlink("later.txt", root.join("link")).unwrap();
        let s = PathSandbox::new(vec![root.to_string_lossy().to_string()]);

        let io = s.io_path(root.join("link").to_str().unwrap()).unwrap();
        assert_eq!(io, std::fs::canonicalize(&root).unwrap().join("later.txt"));
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_loop_is_refused() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("workspace");
        std::fs::create_dir_all(&root).unwrap();
        std::os::unix::fs::symlink("b", root.join("a")).unwrap();
        std::os::unix::fs::symlink("a", root.join("b")).unwrap();
        let s = PathSandbox::new(vec![root.to_string_lossy().to_string()]);

        let err = s.check(root.join("a").to_str().unwrap()).unwrap_err();
        assert!(err.to_string().contains("could not be resolved"), "{err}");
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
