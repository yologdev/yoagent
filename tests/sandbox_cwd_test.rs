//! `PathSandbox` with relative paths. This binary changes the process's
//! working directory, which is global, so it holds a single test.
#![cfg(feature = "native")]

use yoagent::tools::sandbox::PathSandbox;
use yoagent::tools::*;
use yoagent::types::*;

/// Relative paths are anchored at the working directory before the walk.
/// Without that, a path whose first component is missing (`a/../link/x`)
/// was collapsed lexically and never checked for symlinks — escaping through
/// `link` — and a plain new file (`newdir/a.txt`) was rejected as outside.
#[cfg(unix)]
#[tokio::test]
async fn relative_paths_are_checked_like_absolute_ones() {
    let tmp = tempfile::TempDir::new().unwrap();
    let ws = tmp.path().join("workspace");
    let outside = tmp.path().join("outside");
    std::fs::create_dir_all(&ws).unwrap();
    std::fs::create_dir_all(&outside).unwrap();
    std::os::unix::fs::symlink("../outside", ws.join("link")).unwrap();
    std::os::unix::fs::symlink("../outside/pwn", ws.join("dlink")).unwrap();
    std::env::set_current_dir(&ws).unwrap();

    let sandbox = PathSandbox::new(vec![".".into()]);
    for escape in ["a/../link/x", "a/../dlink", "link/x"] {
        assert!(sandbox.check(escape).is_err(), "{escape} must be rejected");
    }
    assert!(
        sandbox.check("newdir/a.txt").is_ok(),
        "a new file inside is fine"
    );

    let write = WriteFileTool::new().with_allowed_paths(vec![".".into()]);
    let ctx = ToolContext::new("t1", "write_file");
    assert!(write
        .execute(
            serde_json::json!({"path": "a/../link/x", "content": "pwned"}),
            ctx.clone()
        )
        .await
        .is_err());
    assert!(!outside.join("x").exists());
    write
        .execute(
            serde_json::json!({"path": "newdir/a.txt", "content": "ok"}),
            ctx,
        )
        .await
        .unwrap();
    assert_eq!(
        std::fs::read_to_string(ws.join("newdir/a.txt")).unwrap(),
        "ok"
    );
}
