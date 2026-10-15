//! How the `search` tool picks and runs its engine: found on `PATH` without
//! `which`, the grep fallback with `GREP_OPTIONS` (#315), and a search that
//! ends abnormally reported as an error, never as "No matches". Its own
//! binary: each test points `PATH` at a folder of its own.
#![cfg(all(feature = "native", unix))]

use std::path::{Path, PathBuf};
use yoagent::tools::SearchTool;
use yoagent::types::*;

/// `PATH` is process-wide: one test at a time.
static PATH_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

fn ctx() -> ToolContext {
    ToolContext::new("c1", "search")
}

/// A `bin/` holding only `tools` (name → target or script), as `PATH`.
fn bin(root: &Path, tools: &[(&str, Tool)]) -> PathBuf {
    let dir = root.join("bin");
    std::fs::create_dir_all(&dir).unwrap();
    for (name, tool) in tools {
        let at = dir.join(name);
        match tool {
            Tool::Link(target) => std::os::unix::fs::symlink(target, &at).unwrap(),
            Tool::Script(body) => {
                use std::os::unix::fs::PermissionsExt;
                std::fs::write(&at, format!("#!/bin/sh\n{body}\n")).unwrap();
                std::fs::set_permissions(&at, std::fs::Permissions::from_mode(0o755)).unwrap();
            }
        }
    }
    dir
}

enum Tool {
    Link(PathBuf),
    Script(&'static str),
}

/// ok/a.txt, deny/s.txt, and ok/link → deny.
fn fixture(root: &Path) -> PathBuf {
    let (ok, deny) = (root.join("ok"), root.join("deny"));
    std::fs::create_dir_all(&ok).unwrap();
    std::fs::create_dir_all(&deny).unwrap();
    std::fs::write(ok.join("a.txt"), "TOKEN=allowed\n").unwrap();
    std::fs::write(deny.join("s.txt"), "TOKEN=secret\n").unwrap();
    std::os::unix::fs::symlink(&deny, ok.join("link")).unwrap();
    ok
}

async fn search(path_dir: &Path, root: &Path) -> Result<ToolResult, ToolError> {
    // SAFETY: PATH_LOCK is held by the caller; nothing else reads the
    // environment while it is.
    unsafe { std::env::set_var("PATH", path_dir) };
    SearchTool::new()
        .with_root(root.to_str().unwrap())
        .with_allowed_paths(vec![root.to_str().unwrap().to_string()])
        .execute(serde_json::json!({"pattern": "TOKEN"}), ctx())
        .await
}

fn text(r: &ToolResult) -> &str {
    match &r.content[0] {
        Content::Text { text } => text,
        _ => panic!("expected text"),
    }
}

fn system_grep() -> PathBuf {
    ["/usr/bin/grep", "/bin/grep"]
        .iter()
        .map(PathBuf::from)
        .find(|p| p.exists())
        .expect("a system grep")
}

#[tokio::test]
async fn the_grep_fallback_ignores_grep_options() {
    let _lock = PATH_LOCK.lock().await;
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let ok = fixture(&root);
    // No rg: the fallback. BSD grep (macOS) honours GREP_OPTIONS: `-h` drops
    // the paths and `-S` follows the symlink out of `ok/`.
    let path = bin(&root, &[("grep", Tool::Link(system_grep()))]);
    // SAFETY: PATH_LOCK is held.
    unsafe { std::env::set_var("GREP_OPTIONS", "-S -h") };
    let result = search(&path, &ok).await;
    unsafe { std::env::remove_var("GREP_OPTIONS") };
    let result = result.unwrap();
    assert_eq!(result.details["engine"], "grep");
    let text = text(&result);
    assert!(
        !text.contains("TOKEN=secret"),
        "escaped allowed_paths: {text}"
    );
    assert!(
        text.contains(&format!("{}:1:TOKEN=allowed", ok.join("a.txt").display())),
        "{text}"
    );
}

#[tokio::test]
async fn rg_is_found_without_which() {
    let _lock = PATH_LOCK.lock().await;
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let ok = fixture(&root);
    // Only a fake rg on PATH: no `which`, no grep.
    let path = bin(
        &root,
        &[("rg", Tool::Script("echo \"$PWD/x.txt:1:TOKEN=fake\""))],
    );
    let result = search(&path, &ok).await.unwrap();
    assert_eq!(result.details["engine"], "rg");
    assert!(text(&result).contains("TOKEN=fake"));
}

#[tokio::test]
async fn a_search_killed_by_a_signal_is_an_error_not_no_matches() {
    let _lock = PATH_LOCK.lock().await;
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let ok = fixture(&root);
    let path = bin(&root, &[("rg", Tool::Script("kill -9 $$"))]);
    let err = search(&path, &ok).await.unwrap_err();
    assert!(format!("{err}").contains("ended abnormally"), "{err}");
}

#[tokio::test]
async fn an_unexpected_exit_code_is_an_error_not_no_matches() {
    let _lock = PATH_LOCK.lock().await;
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let ok = fixture(&root);
    let path = bin(&root, &[("rg", Tool::Script("exit 3"))]);
    let err = search(&path, &ok).await.unwrap_err();
    assert!(format!("{err}").contains("ended abnormally"), "{err}");
}

#[tokio::test]
async fn matches_before_an_abnormal_end_come_with_a_warning() {
    let _lock = PATH_LOCK.lock().await;
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let ok = fixture(&root);
    let path = bin(
        &root,
        &[(
            "rg",
            Tool::Script("echo \"$PWD/x.txt:1:TOKEN=partial\"; kill -9 $$"),
        )],
    );
    let result = search(&path, &ok).await.unwrap();
    let text = text(&result);
    assert!(text.contains("TOKEN=partial"), "{text}");
    assert!(text.contains("results may be incomplete"), "{text}");
    assert!(result.details["warnings"]
        .as_str()
        .unwrap()
        .contains("ended abnormally"));
}
