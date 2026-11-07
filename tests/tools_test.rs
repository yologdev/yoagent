//! Tests for built-in tools.

use base64::Engine;
use tokio_util::sync::CancellationToken;
use yoagent::tools::edit::EditFileTool;
use yoagent::tools::list::ListFilesTool;
use yoagent::tools::*;
use yoagent::types::*;

/// Helper to build a ToolContext for tests.
fn ctx(name: &str) -> ToolContext {
    ToolContext::new("t1", name)
}

fn ctx_with_cancel(name: &str, cancel: CancellationToken) -> ToolContext {
    ToolContext::new("t1", name).with_cancel(cancel)
}

#[tokio::test]
async fn test_bash_echo() {
    let tool = BashTool::new();
    let result = tool
        .execute(serde_json::json!({"command": "echo hello"}), ctx("bash"))
        .await
        .unwrap();

    let text = match &result.content[0] {
        Content::Text { text } => text,
        _ => panic!("expected text"),
    };
    assert!(text.contains("hello"));
    assert!(text.contains("Exit code: 0"));
}

#[tokio::test]
async fn test_bash_failure() {
    // Non-zero exit codes return Ok with exit code in output (for LLM self-correction)
    let tool = BashTool::new();
    let result = tool
        .execute(serde_json::json!({"command": "false"}), ctx("bash"))
        .await
        .unwrap();

    let text = match &result.content[0] {
        Content::Text { text } => text,
        _ => panic!("expected text"),
    };
    assert!(text.contains("Exit code: 1"));
}

#[tokio::test]
async fn test_bash_deny_pattern() {
    let tool = BashTool::new();
    let result = tool
        .execute(serde_json::json!({"command": "rm -rf /"}), ctx("bash"))
        .await;

    assert!(result.is_err());
    assert!(result.unwrap_err().to_string().contains("blocked"));
}

#[tokio::test]
async fn test_bash_timeout() {
    let tool = BashTool::new().with_timeout(std::time::Duration::from_millis(100));
    let result = tool
        .execute(serde_json::json!({"command": "sleep 10"}), ctx("bash"))
        .await;

    assert!(result.is_err());
    let err = result.unwrap_err().to_string();
    assert!(err.contains("timed out"));
}

/// Cutting output inside a multi-byte character must not panic (it used to:
/// `String::truncate` off a char boundary).
#[tokio::test]
async fn test_bash_truncation_inside_a_multibyte_character() {
    let mut tool = BashTool::new();
    // 9 bytes, cap 7: the head keeps "日" (3 bytes), the tail the last 4 —
    // the final byte of "本", cut mid-character, and "語".
    tool.max_output_bytes = 7;
    let result = tool
        .execute(
            serde_json::json!({"command": "printf '日本語'"}),
            ctx("bash"),
        )
        .await
        .unwrap();
    let Content::Text { text } = &result.content[0] else {
        panic!("expected text")
    };
    assert!(text.starts_with("Exit code: 0\n日"), "{text}");
    assert!(
        text.contains("(output truncated: 2 bytes omitted)"),
        "{text}"
    );
    assert!(text.ends_with("\u{FFFD}語"), "{text}");
}

/// Output beyond the cap is drained, not kept, and the command completes.
#[tokio::test]
async fn test_bash_large_output_is_capped() {
    // A short timeout: a regression that stops draining would deadlock.
    let mut tool = BashTool::new().with_timeout(std::time::Duration::from_secs(10));
    tool.max_output_bytes = 1000;
    let result = tool
        .execute(
            serde_json::json!({"command": "head -c 5000000 /dev/zero | tr '\\0' a"}),
            ctx("bash"),
        )
        .await
        .unwrap();
    let Content::Text { text } = &result.content[0] else {
        panic!("expected text")
    };
    assert!(text.len() < 1100, "{} bytes kept", text.len());
    assert!(text.contains("(output truncated"));
}

/// Past the cap, the end of the output is kept as well as the start: a build
/// or test log's verdict comes last.
#[tokio::test]
async fn test_bash_long_output_keeps_its_end() {
    let mut tool = BashTool::new().with_timeout(std::time::Duration::from_secs(10));
    tool.max_output_bytes = 1000;
    let result = tool
        .execute(
            serde_json::json!({"command": "seq 1 100000; echo 'error: the verdict'"}),
            ctx("bash"),
        )
        .await
        .unwrap();
    let Content::Text { text } = &result.content[0] else {
        panic!("expected text")
    };
    assert!(text.starts_with("Exit code: 0\n1\n2\n"), "{text}");
    assert!(
        text.ends_with("error: the verdict\n"),
        "the end must survive: {text}"
    );
    assert!(text.contains("bytes omitted"), "{text}");
    assert!(text.len() < 1200, "{} bytes kept", text.len());
}

/// A timeout returns what the command printed so far, and kills it.
#[tokio::test]
async fn test_bash_timeout_keeps_output_and_kills_the_command() {
    let tmp = tempfile::TempDir::new().unwrap();
    let marker = tmp.path().join("still-running");
    let tool = BashTool::new().with_timeout(std::time::Duration::from_millis(1000));
    let command = format!("echo started; sleep 2; touch '{}'", marker.display());
    let err = tool
        .execute(serde_json::json!({ "command": command }), ctx("bash"))
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("timed out"), "{err}");
    assert!(err.contains("started"), "{err}");

    // Well past when the command would have touched the marker.
    tokio::time::sleep(std::time::Duration::from_millis(2000)).await;
    assert!(!marker.exists(), "the timed-out command kept running");
}

/// Stderr is drained concurrently with stdout: a flood on stderr must not
/// block the command, and it is capped like stdout.
#[tokio::test]
async fn test_bash_stderr_flood_is_drained_and_capped() {
    let mut tool = BashTool::new().with_timeout(std::time::Duration::from_secs(10));
    tool.max_output_bytes = 1000;
    let result = tool
        .execute(
            serde_json::json!({"command": "head -c 300000 /dev/zero >&2; echo done"}),
            ctx("bash"),
        )
        .await
        .unwrap();
    let Content::Text { text } = &result.content[0] else {
        panic!("expected text")
    };
    assert!(
        text.contains("STDOUT:\ndone"),
        "{}",
        &text[..text.len().min(200)]
    );
    assert!(text.contains("(output truncated"), "stderr is cut");
}

/// A timeout that hit the cap still says the output was cut.
#[tokio::test]
async fn test_bash_timeout_keeps_the_truncation_note() {
    let mut tool = BashTool::new().with_timeout(std::time::Duration::from_millis(1500));
    tool.max_output_bytes = 100;
    let err = tool
        .execute(
            serde_json::json!({"command": "head -c 100000 /dev/zero | tr '\\0' a; sleep 10"}),
            ctx("bash"),
        )
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("timed out"), "{err}");
    assert!(err.contains("(output truncated"), "{err}");
}

/// Cancelling mid-run kills the command.
#[tokio::test]
async fn test_bash_cancel_mid_run_kills_the_command() {
    let tmp = tempfile::TempDir::new().unwrap();
    let marker = tmp.path().join("still-running");
    let cancel = CancellationToken::new();
    let trigger = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        trigger.cancel();
    });
    let command = format!("sleep 1.5; touch '{}'", marker.display());
    let result = BashTool::new()
        .execute(
            serde_json::json!({ "command": command }),
            ctx_with_cancel("bash", cancel),
        )
        .await;
    assert!(matches!(result, Err(ToolError::Cancelled)), "{result:?}");
    tokio::time::sleep(std::time::Duration::from_millis(2000)).await;
    assert!(!marker.exists(), "the cancelled command kept running");
}

/// An already-cancelled call never starts the command.
#[tokio::test]
async fn test_bash_already_cancelled_does_not_run() {
    let tmp = tempfile::TempDir::new().unwrap();
    let marker = tmp.path().join("ran");
    let cancel = CancellationToken::new();
    cancel.cancel();
    let command = format!("touch '{}'", marker.display());
    let result = BashTool::new()
        .execute(
            serde_json::json!({ "command": command }),
            ctx_with_cancel("bash", cancel),
        )
        .await;
    assert!(result.is_err());
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    assert!(!marker.exists());
}

/// A command that reads stdin gets EOF at once, never the agent's own input
/// (the tool spawns the child, and `spawn` would inherit stdin). Run from a
/// terminal, a regression hangs here until the timeout.
#[tokio::test]
async fn test_bash_stdin_is_empty() {
    let result = BashTool::new()
        .with_timeout(std::time::Duration::from_secs(5))
        .execute(
            serde_json::json!({"command": "cat; read line; echo \"got:[$line]\""}),
            ctx("bash"),
        )
        .await
        .unwrap();
    let Content::Text { text } = &result.content[0] else {
        panic!("expected text")
    };
    assert_eq!(text, "Exit code: 0\ngot:[]\n");
}

#[tokio::test]
async fn test_bash_cancel() {
    let tool = BashTool::new();
    let cancel = CancellationToken::new();
    cancel.cancel();

    let result = tool
        .execute(
            serde_json::json!({"command": "echo should not run"}),
            ctx_with_cancel("bash", cancel),
        )
        .await;

    assert!(result.is_err());
}

#[tokio::test]
async fn test_read_write_file() {
    let tmp = std::env::temp_dir().join("yoagent-test-rw.txt");
    let path = tmp.to_str().unwrap();

    // Write
    let write_tool = WriteFileTool::new();
    let result = write_tool
        .execute(
            serde_json::json!({"path": path, "content": "hello from yoagent"}),
            ctx("write_file"),
        )
        .await
        .unwrap();

    let text = match &result.content[0] {
        Content::Text { text } => text,
        _ => panic!("expected text"),
    };
    assert!(text.contains("Wrote"));

    // Read
    let read_tool = ReadFileTool::new();
    let result = read_tool
        .execute(serde_json::json!({"path": path}), ctx("read_file"))
        .await
        .unwrap();

    let text = match &result.content[0] {
        Content::Text { text } => text,
        _ => panic!("expected text"),
    };
    assert!(text.contains("hello from yoagent"));

    // Cleanup
    let _ = std::fs::remove_file(tmp);
}

#[tokio::test]
async fn test_read_file_with_offset_limit() {
    let tmp = std::env::temp_dir().join("yoagent-test-lines.txt");
    let path = tmp.to_str().unwrap();

    let content = (1..=20)
        .map(|i| format!("line {}", i))
        .collect::<Vec<_>>()
        .join("\n");
    std::fs::write(&tmp, &content).unwrap();

    let tool = ReadFileTool::new();
    let result = tool
        .execute(
            serde_json::json!({"path": path, "offset": 5, "limit": 3}),
            ctx("read_file"),
        )
        .await
        .unwrap();

    let text = match &result.content[0] {
        Content::Text { text } => text,
        _ => panic!("expected text"),
    };
    assert!(text.contains("line 5"));
    assert!(text.contains("line 7"));
    assert!(!text.contains("line 8"));

    let _ = std::fs::remove_file(tmp);
}

#[tokio::test]
async fn test_read_file_not_found() {
    let tool = ReadFileTool::new();
    let result = tool
        .execute(
            serde_json::json!({"path": "/nonexistent/file.txt"}),
            ctx("read_file"),
        )
        .await;

    assert!(result.is_err());
}

#[tokio::test]
async fn test_write_creates_directories() {
    let tmp = std::env::temp_dir().join("yoagent-test-nested/deep/dir/file.txt");
    let path = tmp.to_str().unwrap();

    let tool = WriteFileTool::new();
    let result = tool
        .execute(
            serde_json::json!({"path": path, "content": "nested!"}),
            ctx("write_file"),
        )
        .await;

    assert!(result.is_ok());
    assert!(tmp.exists());

    // Cleanup
    let _ = std::fs::remove_dir_all(std::env::temp_dir().join("yoagent-test-nested"));
}

#[tokio::test]
async fn test_search_pattern() {
    let tmp_dir = std::env::temp_dir().join("yoagent-test-search");
    let _ = std::fs::create_dir_all(&tmp_dir);
    std::fs::write(tmp_dir.join("a.txt"), "hello world\nfoo bar\nhello again").unwrap();
    std::fs::write(tmp_dir.join("b.txt"), "no match here\nhello there").unwrap();

    let tool = SearchTool::new().with_root(tmp_dir.to_str().unwrap());
    let result = tool
        .execute(serde_json::json!({"pattern": "hello"}), ctx("search"))
        .await
        .unwrap();

    let text = match &result.content[0] {
        Content::Text { text } => text,
        _ => panic!("expected text"),
    };
    assert!(text.contains("hello"));
    assert!(text.contains("3 matches") || text.contains("matches")); // 3 lines match

    let _ = std::fs::remove_dir_all(tmp_dir);
}

#[tokio::test]
async fn test_search_no_matches() {
    let tmp_dir = std::env::temp_dir().join("yoagent-test-search-empty");
    let _ = std::fs::create_dir_all(&tmp_dir);
    std::fs::write(tmp_dir.join("a.txt"), "nothing interesting").unwrap();

    let tool = SearchTool::new().with_root(tmp_dir.to_str().unwrap());
    let result = tool
        .execute(
            serde_json::json!({"pattern": "zzzznotfound"}),
            ctx("search"),
        )
        .await
        .unwrap();

    let text = match &result.content[0] {
        Content::Text { text } => text,
        _ => panic!("expected text"),
    };
    assert!(text.contains("No matches"));

    let _ = std::fs::remove_dir_all(tmp_dir);
}

/// A pattern that looks like a flag is searched for, never parsed as one.
/// (`--pre=<cmd>` would make ripgrep run a command on every file.)
#[tokio::test]
async fn test_search_pattern_that_looks_like_a_flag_is_literal() {
    let tmp = tempfile::TempDir::new().unwrap();
    std::fs::write(tmp.path().join("a.txt"), "use --files here\nnothing\n").unwrap();

    let tool = SearchTool::new().with_root(tmp.path().to_str().unwrap());
    let result = tool
        .execute(serde_json::json!({"pattern": "--files"}), ctx("search"))
        .await
        .unwrap();
    let Content::Text { text } = &result.content[0] else {
        panic!("expected text")
    };
    assert!(text.contains("use --files here"), "{text}");
    assert!(text.contains("(1 matches)"), "{text}");
}

/// `max_results` caps the total, not the matches per file.
#[tokio::test]
async fn test_search_caps_the_total_number_of_matches() {
    let tmp = tempfile::TempDir::new().unwrap();
    for f in ["a.txt", "b.txt", "c.txt"] {
        std::fs::write(tmp.path().join(f), "hit\nhit\nhit\n").unwrap();
    }
    let mut tool = SearchTool::new().with_root(tmp.path().to_str().unwrap());
    tool.max_results = 4;
    let result = tool
        .execute(serde_json::json!({"pattern": "hit"}), ctx("search"))
        .await
        .unwrap();
    let Content::Text { text } = &result.content[0] else {
        panic!("expected text")
    };
    assert_eq!(
        text.lines().filter(|l| l.contains("hit")).count(),
        4,
        "{text}"
    );
    assert!(
        text.contains("showing the first 4 matches; there are more"),
        "{text}"
    );
    assert_eq!(result.details["truncated"], true);
}

/// Exactly `max_results` matches is the whole result: no "more" note.
#[tokio::test]
async fn test_search_exactly_max_results_is_not_truncated() {
    let tmp = tempfile::TempDir::new().unwrap();
    std::fs::write(tmp.path().join("a.txt"), "hit\nhit\nhit\nhit\n").unwrap();
    let mut tool = SearchTool::new().with_root(tmp.path().to_str().unwrap());
    tool.max_results = 4;
    let result = tool
        .execute(serde_json::json!({"pattern": "hit"}), ctx("search"))
        .await
        .unwrap();
    let Content::Text { text } = &result.content[0] else {
        panic!("expected text")
    };
    assert!(text.ends_with("(4 matches)"), "{text}");
    assert_eq!(result.details["truncated"], false);
}

/// When some files cannot be searched (here: unreadable), rg and grep exit 2
/// even though they found matches elsewhere. Those matches are the answer;
/// the errors come back as warnings, not as a failed search.
#[cfg(unix)]
#[tokio::test]
async fn test_search_returns_matches_despite_unreadable_files() {
    use std::os::unix::fs::PermissionsExt;
    let tmp = tempfile::TempDir::new().unwrap();
    std::fs::write(tmp.path().join("ok.txt"), "needle here\n").unwrap();
    let locked = tmp.path().join("locked.txt");
    std::fs::write(&locked, "needle too\n").unwrap();
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();
    if std::fs::read(&locked).is_ok() {
        // Running as root: nothing is unreadable, so there is nothing to test.
        return;
    }

    let tool = SearchTool::new().with_root(tmp.path().to_str().unwrap());
    let result = tool
        .execute(serde_json::json!({"pattern": "needle"}), ctx("search"))
        .await;
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o644)).unwrap();
    let result = result.expect("matches are returned, not a search error");
    let Content::Text { text } = &result.content[0] else {
        panic!("expected text")
    };
    assert!(text.contains("needle here"), "{text}");
    assert!(text.contains("Warnings:"), "{text}");
    // The warning text itself, not a flag.
    assert!(
        result.details["warnings"]
            .as_str()
            .is_some_and(|w| w.contains("locked.txt")),
        "{}",
        result.details
    );
}

/// With no matches at all, an error is still an error.
#[tokio::test]
async fn test_search_error_without_matches_is_an_error() {
    let tool = SearchTool::new();
    let err = tool
        .execute(
            serde_json::json!({"pattern": "x", "path": "/definitely/not/a/dir"}),
            ctx("search"),
        )
        .await
        .unwrap_err();
    assert!(err.to_string().contains("Search error"), "{err}");
}

// --- Edit tool tests ---

#[tokio::test]
async fn test_edit_file() {
    let tmp = std::env::temp_dir().join("yoagent-test-edit.txt");
    let path = tmp.to_str().unwrap();
    std::fs::write(&tmp, "fn main() {\n    println!(\"hello\");\n}\n").unwrap();

    let tool = EditFileTool::new();
    let result = tool
        .execute(
            serde_json::json!({
                "path": path,
                "old_text": "println!(\"hello\")",
                "new_text": "println!(\"goodbye\")"
            }),
            ctx("edit_file"),
        )
        .await
        .unwrap();

    let text = match &result.content[0] {
        Content::Text { text } => text,
        _ => panic!("expected text"),
    };
    assert!(text.contains("Replaced"));
    let content = std::fs::read_to_string(&tmp).unwrap();
    assert!(content.contains("goodbye"));
    let _ = std::fs::remove_file(tmp);
}

#[tokio::test]
async fn test_edit_file_no_match() {
    let tmp = std::env::temp_dir().join("yoagent-test-edit-nomatch.txt");
    let path = tmp.to_str().unwrap();
    std::fs::write(&tmp, "hello world\n").unwrap();
    let tool = EditFileTool::new();
    let result = tool
        .execute(
            serde_json::json!({"path": path, "old_text": "nonexistent", "new_text": "bar"}),
            ctx("edit_file"),
        )
        .await;
    assert!(result.is_err());
    let _ = std::fs::remove_file(tmp);
}

#[tokio::test]
async fn test_list_files_tool() {
    let tmp_dir = std::env::temp_dir().join("yoagent-test-list2");
    let _ = std::fs::create_dir_all(tmp_dir.join("sub"));
    std::fs::write(tmp_dir.join("a.rs"), "").unwrap();
    std::fs::write(tmp_dir.join("sub/c.rs"), "").unwrap();
    let tool = ListFilesTool::new();
    let result = tool
        .execute(
            serde_json::json!({"path": tmp_dir.to_str().unwrap()}),
            ctx("list_files"),
        )
        .await
        .unwrap();
    let text = match &result.content[0] {
        Content::Text { text } => text,
        _ => panic!("expected text"),
    };
    assert!(text.contains("a.rs"));
    let _ = std::fs::remove_dir_all(tmp_dir);
}

/// An unreadable subdirectory doesn't make a partial listing look complete
/// (#260): the readable files are listed, and `find`'s error is reported as a
/// warning in the text and in `details.warnings`. Control: a fully readable
/// tree has no warnings.
#[cfg(unix)]
#[tokio::test]
async fn list_files_reports_unreadable_subdirectories() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.rs"), "").unwrap();
    let locked = dir.path().join("locked");
    std::fs::create_dir(&locked).unwrap();
    std::fs::write(locked.join("hidden.rs"), "").unwrap();
    let list = |path: std::path::PathBuf| async move {
        ListFilesTool::new()
            .execute(
                serde_json::json!({"path": path.to_str().unwrap()}),
                ctx("list_files"),
            )
            .await
            .unwrap()
    };

    // Control: everything readable, no warnings.
    let ok = list(dir.path().to_path_buf()).await;
    assert!(ok.details["warnings"].is_null(), "{:?}", ok.details);

    // Restores the permissions however the test ends, so the temp dir can be removed.
    struct Unlock(std::path::PathBuf);
    impl Drop for Unlock {
        fn drop(&mut self) {
            let _ = std::fs::set_permissions(&self.0, std::fs::Permissions::from_mode(0o755));
        }
    }
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();
    let _unlock = Unlock(locked.clone());
    if std::fs::read_dir(&locked).is_ok() {
        // Running as root: permissions don't apply, nothing to test.
        eprintln!("skipped: permissions are not enforced for this user");
        return;
    }
    let result = list(dir.path().to_path_buf()).await;
    let text = match &result.content[0] {
        Content::Text { text } => text.clone(),
        _ => panic!("expected text"),
    };
    assert!(text.contains("a.rs"), "{text}");
    assert!(text.contains("Warnings"), "{text}");
    assert!(text.contains("locked"), "{text}");
    assert!(
        result.details["warnings"].is_string(),
        "{:?}",
        result.details
    );
}

#[tokio::test]
async fn test_read_file_line_numbers() {
    let tmp = std::env::temp_dir().join("yoagent-test-lineno2.txt");
    let path = tmp.to_str().unwrap();
    std::fs::write(&tmp, "first\nsecond\nthird\n").unwrap();
    let tool = ReadFileTool::new();
    let result = tool
        .execute(serde_json::json!({"path": path}), ctx("read_file"))
        .await
        .unwrap();
    let text = match &result.content[0] {
        Content::Text { text } => text,
        _ => panic!("expected text"),
    };
    assert!(text.contains("   1 | first"));
    assert!(text.contains("   2 | second"));
    let _ = std::fs::remove_file(tmp);
}

#[tokio::test]
async fn test_bash_blocked_command() {
    let tool = BashTool::new();
    let result = tool
        .execute(serde_json::json!({"command": "rm -rf /"}), ctx("bash"))
        .await;
    assert!(result.is_err());
    assert!(result.unwrap_err().to_string().contains("blocked"));
}

#[tokio::test]
async fn test_default_tools_complete() {
    let tools = yoagent::tools::default_tools();
    let names: Vec<&str> = tools.iter().map(|t| t.name()).collect();
    assert_eq!(names.len(), 6);
    assert!(names.contains(&"bash"));
    assert!(names.contains(&"edit_file"));
    assert!(names.contains(&"list_files"));
}

// --- Image support tests ---

#[tokio::test]
async fn test_read_image_file() {
    // Minimal valid PNG (1x1 pixel, transparent)
    let png_bytes: Vec<u8> = vec![
        0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, // PNG signature
        0x00, 0x00, 0x00, 0x0D, 0x49, 0x48, 0x44, 0x52, // IHDR chunk
        0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, // 1x1
        0x08, 0x02, 0x00, 0x00, 0x00, 0x90, 0x77, 0x53, 0xDE, // 8-bit RGB
        0x00, 0x00, 0x00, 0x0C, 0x49, 0x44, 0x41, 0x54, // IDAT chunk
        0x08, 0xD7, 0x63, 0xF8, 0xCF, 0xC0, 0x00, 0x00, 0x00, 0x02, 0x00, 0x01, 0xE2, 0x21, 0xBC,
        0x33, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4E, 0x44, // IEND chunk
        0xAE, 0x42, 0x60, 0x82,
    ];

    let tmp = std::env::temp_dir().join("yoagent-test-image.png");
    std::fs::write(&tmp, &png_bytes).unwrap();

    let tool = ReadFileTool::new();
    let result = tool
        .execute(
            serde_json::json!({"path": tmp.to_str().unwrap()}),
            ctx("read_file"),
        )
        .await
        .unwrap();

    match &result.content[0] {
        Content::Image { data, mime_type } => {
            assert_eq!(mime_type, "image/png");
            assert!(!data.is_empty());
            // Verify round-trip: decode should match original bytes
            let decoded = base64::engine::general_purpose::STANDARD
                .decode(data)
                .unwrap();
            assert_eq!(decoded, png_bytes);
        }
        _ => panic!("expected Content::Image"),
    }

    let _ = std::fs::remove_file(tmp);
}

#[tokio::test]
async fn test_read_jpeg_file() {
    let tmp = std::env::temp_dir().join("yoagent-test-image.jpg");
    std::fs::write(&tmp, jpeg(4, 3)).unwrap();

    let tool = ReadFileTool::new();
    let result = tool
        .execute(
            serde_json::json!({"path": tmp.to_str().unwrap()}),
            ctx("read_file"),
        )
        .await
        .unwrap();

    match &result.content[0] {
        Content::Image { mime_type, .. } => {
            assert_eq!(mime_type, "image/jpeg");
        }
        _ => panic!("expected Content::Image for .jpg"),
    }

    let _ = std::fs::remove_file(tmp);
}

#[tokio::test]
async fn test_read_text_file_unchanged() {
    // Non-image files should still return Content::Text
    let tmp = std::env::temp_dir().join("yoagent-test-notimage.txt");
    std::fs::write(&tmp, "just text").unwrap();

    let tool = ReadFileTool::new();
    let result = tool
        .execute(
            serde_json::json!({"path": tmp.to_str().unwrap()}),
            ctx("read_file"),
        )
        .await
        .unwrap();

    match &result.content[0] {
        Content::Text { text } => {
            assert!(text.contains("just text"));
        }
        _ => panic!("expected Content::Text for .txt file"),
    }

    let _ = std::fs::remove_file(tmp);
}

#[tokio::test]
async fn test_read_file_pages_long_files_by_default() {
    let tmp = std::env::temp_dir().join("yoagent-test-paging.txt");
    let path = tmp.to_str().unwrap();

    let total = yoagent::tools::DEFAULT_READ_MAX_LINES * 2;
    let content = (1..=total)
        .map(|i| format!("line {}", i))
        .collect::<Vec<_>>()
        .join("\n");
    std::fs::write(&tmp, &content).unwrap();

    let tool = ReadFileTool::new();
    let result = tool
        .execute(serde_json::json!({"path": path}), ctx("read_file"))
        .await
        .unwrap();
    let Content::Text { text } = &result.content[0] else {
        panic!("expected text")
    };

    // One page, and the header states the true total so the agent can page on.
    assert!(text.contains(&format!("of {}", total)));
    assert!(text.contains("offset/limit"));
    assert!(text.contains("line 1\n") || text.contains("| line 1"));
    assert!(!text.contains(&format!("| line {}", total)));
    assert_eq!(
        text.lines().count(),
        yoagent::tools::DEFAULT_READ_MAX_LINES + 1, // + header
    );

    // Paging forward reaches the end.
    let result = tool
        .execute(
            serde_json::json!({"path": path, "offset": yoagent::tools::DEFAULT_READ_MAX_LINES + 1}),
            ctx("read_file"),
        )
        .await
        .unwrap();
    let Content::Text { text } = &result.content[0] else {
        panic!("expected text")
    };
    assert!(text.contains(&format!("| line {}", total)));

    // An explicit limit still wins, and short files are unaffected.
    let result = tool
        .execute(
            serde_json::json!({"path": path, "limit": 3}),
            ctx("read_file"),
        )
        .await
        .unwrap();
    let Content::Text { text } = &result.content[0] else {
        panic!("expected text")
    };
    assert_eq!(text.lines().count(), 4); // header + 3

    let _ = std::fs::remove_file(tmp);
}

#[tokio::test]
async fn test_read_file_unbounded_when_max_lines_disabled() {
    let tmp = std::env::temp_dir().join("yoagent-test-unbounded.txt");
    let path = tmp.to_str().unwrap();
    let total = yoagent::tools::DEFAULT_READ_MAX_LINES + 50;
    let content = (1..=total)
        .map(|i| format!("line {}", i))
        .collect::<Vec<_>>()
        .join("\n");
    std::fs::write(&tmp, &content).unwrap();

    let tool = ReadFileTool {
        max_lines: usize::MAX,
        ..Default::default()
    };
    let result = tool
        .execute(serde_json::json!({"path": path}), ctx("read_file"))
        .await
        .unwrap();
    let Content::Text { text } = &result.content[0] else {
        panic!("expected text")
    };
    assert_eq!(text.lines().count(), total + 1);
    assert!(text.contains(&format!("[{} lines]", total)));

    let _ = std::fs::remove_file(tmp);
}

// ---------------------------------------------------------------------------
// Path sandboxing — allowed_paths must actually be enforced, on every tool
// that takes a path, against the resolved path (not the string).
// ---------------------------------------------------------------------------

#[tokio::test]
async fn read_tool_rejects_paths_outside_allowed_roots() {
    let tmp = std::env::temp_dir().join("yoagent-sandbox-read");
    let ws = tmp.join("workspace");
    std::fs::create_dir_all(&ws).unwrap();
    std::fs::write(ws.join("ok.txt"), "inside").unwrap();
    std::fs::write(tmp.join("secret.txt"), "outside").unwrap();

    let tool = ReadFileTool::new().with_allowed_paths(vec![ws.to_string_lossy().to_string()]);

    // Inside the root: allowed.
    assert!(tool
        .execute(
            serde_json::json!({"path": ws.join("ok.txt").to_str().unwrap()}),
            ctx("read_file")
        )
        .await
        .is_ok());

    // Absolute path outside: rejected.
    assert!(tool
        .execute(
            serde_json::json!({"path": tmp.join("secret.txt").to_str().unwrap()}),
            ctx("read_file")
        )
        .await
        .is_err());

    // Traversal that is lexically "inside" the root: rejected.
    let escape = ws.join("../secret.txt");
    assert!(
        tool.execute(
            serde_json::json!({"path": escape.to_str().unwrap()}),
            ctx("read_file")
        )
        .await
        .is_err(),
        "`..` must not escape the sandbox"
    );

    let _ = std::fs::remove_dir_all(&tmp);
}

/// The `..`-after-a-missing-directory escape, through the tool: it must fail,
/// leave the outside untouched and create nothing. The twin that climbs back
/// inside must write the right file without creating `x` — which only holds
/// while the tool does its I/O on the checked path.
#[tokio::test]
async fn write_file_cannot_climb_out_through_a_missing_directory() {
    let tmp = tempfile::TempDir::new().unwrap();
    let ws = tmp.path().join("workspace");
    std::fs::create_dir_all(&ws).unwrap();
    let write = WriteFileTool::new().with_allowed_paths(vec![ws.to_string_lossy().to_string()]);

    let escape = ws.join("x/../../victim.txt");
    assert!(write
        .execute(
            serde_json::json!({"path": escape.to_str().unwrap(), "content": "pwned"}),
            ctx("write_file")
        )
        .await
        .is_err());
    assert!(!tmp.path().join("victim.txt").exists());
    assert!(!ws.join("x").exists());

    let inside = ws.join("x/../ok.txt");
    write
        .execute(
            serde_json::json!({"path": inside.to_str().unwrap(), "content": "ok"}),
            ctx("write_file"),
        )
        .await
        .unwrap();
    assert_eq!(std::fs::read_to_string(ws.join("ok.txt")).unwrap(), "ok");
    assert!(!ws.join("x").exists(), "the checked path has no `x` in it");
}

/// A dangling symlink to a file outside the root: writing through it would
/// create the target outside.
#[cfg(unix)]
#[tokio::test]
async fn write_file_cannot_write_through_a_dangling_symlink_out_of_the_root() {
    let tmp = tempfile::TempDir::new().unwrap();
    let ws = tmp.path().join("workspace");
    std::fs::create_dir_all(&ws).unwrap();
    std::os::unix::fs::symlink(tmp.path().join("pwned.txt"), ws.join("link")).unwrap();
    let write = WriteFileTool::new().with_allowed_paths(vec![ws.to_string_lossy().to_string()]);

    assert!(write
        .execute(
            serde_json::json!({"path": ws.join("link").to_str().unwrap(), "content": "pwned"}),
            ctx("write_file")
        )
        .await
        .is_err());
    assert!(!tmp.path().join("pwned.txt").exists());
}

#[tokio::test]
async fn write_and_edit_tools_reject_paths_outside_allowed_roots() {
    let tmp = std::env::temp_dir().join("yoagent-sandbox-write");
    let ws = tmp.join("workspace");
    std::fs::create_dir_all(&ws).unwrap();
    std::fs::write(ws.join("edit.txt"), "hello").unwrap();
    let outside = tmp.join("victim.txt");
    std::fs::write(&outside, "original").unwrap();

    let roots = vec![ws.to_string_lossy().to_string()];
    let write = WriteFileTool::new().with_allowed_paths(roots.clone());
    let edit = EditFileTool::new().with_allowed_paths(roots);

    // A write outside the sandbox must fail *and* leave the file untouched.
    assert!(write
        .execute(
            serde_json::json!({"path": outside.to_str().unwrap(), "content": "pwned"}),
            ctx("write_file")
        )
        .await
        .is_err());
    assert_eq!(std::fs::read_to_string(&outside).unwrap(), "original");

    // Writing a not-yet-existing file inside the sandbox still works.
    assert!(write
        .execute(
            serde_json::json!({"path": ws.join("new/deep.txt").to_str().unwrap(), "content": "ok"}),
            ctx("write_file")
        )
        .await
        .is_ok());

    assert!(edit
        .execute(
            serde_json::json!({
                "path": outside.to_str().unwrap(),
                "old_text": "original",
                "new_text": "pwned"
            }),
            ctx("edit_file")
        )
        .await
        .is_err());
    assert_eq!(std::fs::read_to_string(&outside).unwrap(), "original");

    let _ = std::fs::remove_dir_all(&tmp);
}

#[tokio::test]
async fn list_and_search_tools_reject_paths_outside_allowed_roots() {
    let tmp = std::env::temp_dir().join("yoagent-sandbox-scan");
    let ws = tmp.join("workspace");
    std::fs::create_dir_all(&ws).unwrap();
    std::fs::write(tmp.join("secret.txt"), "needle").unwrap();

    let roots = vec![ws.to_string_lossy().to_string()];
    let list = ListFilesTool::default().with_allowed_paths(roots.clone());
    let search = SearchTool::default().with_allowed_paths(roots);

    assert!(list
        .execute(
            serde_json::json!({"path": tmp.to_str().unwrap()}),
            ctx("list_files")
        )
        .await
        .is_err());
    assert!(search
        .execute(
            serde_json::json!({"pattern": "needle", "path": tmp.to_str().unwrap()}),
            ctx("search")
        )
        .await
        .is_err());

    let _ = std::fs::remove_dir_all(&tmp);
}

#[tokio::test]
async fn unrestricted_tools_are_unchanged_by_default() {
    // The default is no sandbox; adding enforcement must not break it.
    let tmp = std::env::temp_dir().join("yoagent-sandbox-default");
    std::fs::create_dir_all(&tmp).unwrap();
    std::fs::write(tmp.join("f.txt"), "content").unwrap();

    let tool = ReadFileTool::new();
    assert!(tool
        .execute(
            serde_json::json!({"path": tmp.join("f.txt").to_str().unwrap()}),
            ctx("read_file")
        )
        .await
        .is_ok());

    let _ = std::fs::remove_dir_all(&tmp);
}

#[tokio::test]
async fn bash_env_allowlist_hides_other_variables() {
    // Model-authored commands inherit the agent's environment by default,
    // including credentials. The allowlist is the mitigation.
    unsafe {
        std::env::set_var("YOAGENT_TEST_SECRET", "leaked-value");
        std::env::set_var("YOAGENT_TEST_KEEP", "kept-value");
    }

    let guarded = BashTool::default().with_env_allowlist(vec!["YOAGENT_TEST_KEEP".to_string()]);
    let result = guarded
        .execute(
            serde_json::json!({"command": "echo \"$YOAGENT_TEST_SECRET|$YOAGENT_TEST_KEEP\""}),
            ctx("bash"),
        )
        .await
        .unwrap();
    let out = match &result.content[0] {
        Content::Text { text } => text.clone(),
        _ => panic!("expected text"),
    };
    assert!(
        !out.contains("leaked-value"),
        "secret reached the command: {out}"
    );
    assert!(out.contains("kept-value"), "allowlisted var missing: {out}");

    // Default behaviour is unchanged: the full environment is inherited.
    let plain = BashTool::default();
    let result = plain
        .execute(
            serde_json::json!({"command": "echo \"$YOAGENT_TEST_SECRET\""}),
            ctx("bash"),
        )
        .await
        .unwrap();
    let out = match &result.content[0] {
        Content::Text { text } => text.clone(),
        _ => panic!("expected text"),
    };
    assert!(
        out.contains("leaked-value"),
        "default should inherit env: {out}"
    );

    unsafe {
        std::env::remove_var("YOAGENT_TEST_SECRET");
        std::env::remove_var("YOAGENT_TEST_KEEP");
    }
}

// ---------------------------------------------------------------------------
// BashTool: what a command started goes with it (#277)
// ---------------------------------------------------------------------------

/// Whether `pid` still runs. A killed process nobody has reaped yet (no init
/// in a container, say) is a zombie: `kill -0` still succeeds on it, so a
/// `Z` state counts as gone.
#[cfg(unix)]
fn alive(pid: &str) -> bool {
    let exists = std::process::Command::new("kill")
        .args(["-0", pid])
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success());
    let zombie = std::process::Command::new("ps")
        .args(["-o", "stat=", "-p", pid])
        .output()
        .is_ok_and(|out| {
            String::from_utf8_lossy(&out.stdout)
                .trim_start()
                .starts_with('Z')
        });
    exists && !zombie
}

/// Waits until the command has written its background job's pid.
#[cfg(unix)]
async fn background_pid(file: &std::path::Path) -> String {
    for _ in 0..200 {
        if let Ok(pid) = std::fs::read_to_string(file) {
            if !pid.trim().is_empty() {
                return pid.trim().to_owned();
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    panic!("the command never wrote its background pid");
}

/// A SIGKILL is delivered asynchronously and the orphan is reaped by init.
#[cfg(unix)]
async fn assert_gone(pid: &str) {
    for _ in 0..200 {
        if !alive(pid) {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    panic!("background job {pid} outlived the command");
}

#[cfg(unix)]
fn backgrounding_command(pid_file: &std::path::Path) -> serde_json::Value {
    serde_json::json!({
        "command": format!("sleep 30 & echo $! > {}; sleep 30 | cat", pid_file.display())
    })
}

#[cfg(unix)]
#[tokio::test]
async fn bash_timeout_kills_background_jobs_and_pipeline_stages() {
    let dir = tempfile::tempdir().unwrap();
    let pid_file = dir.path().join("pid");
    // Long enough that `bash` is up and has written the pid even when other
    // tests load the machine; the pid is read before the timeout fires.
    let run = tokio::spawn({
        let args = backgrounding_command(&pid_file);
        async move {
            BashTool::new()
                .with_timeout(std::time::Duration::from_secs(3))
                .execute(args, ctx("bash"))
                .await
        }
    });
    let pid = background_pid(&pid_file).await;
    assert!(alive(&pid));
    assert!(run
        .await
        .unwrap()
        .unwrap_err()
        .to_string()
        .contains("timed out"));
    assert_gone(&pid).await;
}

#[cfg(unix)]
#[tokio::test]
async fn bash_cancel_kills_background_jobs() {
    let dir = tempfile::tempdir().unwrap();
    let pid_file = dir.path().join("pid");
    let cancel = CancellationToken::new();
    let run = tokio::spawn({
        let (cancel, args) = (cancel.clone(), backgrounding_command(&pid_file));
        async move {
            BashTool::new()
                .execute(args, ctx_with_cancel("bash", cancel))
                .await
        }
    });
    let pid = background_pid(&pid_file).await;
    assert!(alive(&pid));
    cancel.cancel();
    assert!(matches!(run.await.unwrap(), Err(ToolError::Cancelled)));
    assert_gone(&pid).await;
}

#[cfg(unix)]
#[tokio::test]
async fn dropping_the_bash_call_kills_background_jobs() {
    let dir = tempfile::tempdir().unwrap();
    let pid_file = dir.path().join("pid");
    let run = tokio::spawn({
        let args = backgrounding_command(&pid_file);
        async move { BashTool::new().execute(args, ctx("bash")).await }
    });
    let pid = background_pid(&pid_file).await;
    assert!(alive(&pid));
    // The caller gives up: the call's future is dropped mid-run.
    run.abort();
    assert!(run.await.unwrap_err().is_cancelled());
    assert_gone(&pid).await;
}

/// A command that finishes on its own may leave a job running on purpose
/// (a server it started): only a timeout, cancel or drop kills the group.
#[cfg(unix)]
#[tokio::test]
async fn a_finished_command_leaves_its_detached_background_job_alone() {
    let dir = tempfile::tempdir().unwrap();
    let pid_file = dir.path().join("pid");
    let result = BashTool::new()
        .execute(
            serde_json::json!({
                "command": format!("sleep 30 >/dev/null 2>&1 & echo $! > {}", pid_file.display())
            }),
            ctx("bash"),
        )
        .await
        .unwrap();
    assert_eq!(result.details["exit_code"], 0);
    let pid = background_pid(&pid_file).await;
    assert!(alive(&pid), "the job outlives a command that finished");
    std::process::Command::new("kill")
        .arg(&pid)
        .status()
        .unwrap();
}

/// #342: a background job that keeps the output pipes open must not hold
/// the call until the timeout: once `bash` exits, the output is read for a
/// short grace, the job is left running and the result says so.
#[cfg(unix)]
#[tokio::test]
async fn a_background_job_holding_the_pipes_does_not_hold_the_call() {
    let dir = tempfile::tempdir().unwrap();
    let pid_file = dir.path().join("pid");
    let started = std::time::Instant::now();
    let result = BashTool::new()
        .with_timeout(std::time::Duration::from_secs(20))
        .execute(
            serde_json::json!({
                "command": format!("sleep 30 & echo $! > {}; echo hi; echo oops >&2", pid_file.display())
            }),
            ctx("bash"),
        )
        .await
        .unwrap();
    assert!(
        started.elapsed() < std::time::Duration::from_secs(10),
        "the call waited {:?}",
        started.elapsed()
    );
    assert_eq!(result.details["exit_code"], 0);
    let text = match &result.content[0] {
        Content::Text { text } => text.clone(),
        other => panic!("{other:?}"),
    };
    assert!(text.contains("hi") && text.contains("oops"), "{text}");
    assert!(text.contains("still holds the command's output"), "{text}");
    assert_eq!(result.details["output_still_open"], true);
    let pid = background_pid(&pid_file).await;
    assert!(alive(&pid), "the job is left running");
    std::process::Command::new("kill")
        .arg(&pid)
        .status()
        .unwrap();
}

/// #342: what a job prints after `bash` exits but within the grace is
/// captured, and a job that closes the pipes in time gets no note.
#[cfg(unix)]
#[tokio::test]
async fn output_printed_within_the_read_grace_is_captured() {
    let result = BashTool::new()
        .with_timeout(std::time::Duration::from_secs(20))
        .execute(
            serde_json::json!({ "command": "(sleep 0.3; echo late) & echo early" }),
            ctx("bash"),
        )
        .await
        .unwrap();
    let text = match &result.content[0] {
        Content::Text { text } => text.clone(),
        other => panic!("{other:?}"),
    };
    assert!(text.contains("early") && text.contains("late"), "{text}");
    assert!(!text.contains("NOTE:"), "{text}");
    assert_eq!(result.details["output_still_open"], false);
}

/// #342: a job left running keeps working when it prints after the call:
/// the pipes are drained, never closed under it (that would SIGPIPE it).
#[cfg(unix)]
#[tokio::test]
async fn a_background_job_that_keeps_printing_survives_the_call() {
    let dir = tempfile::tempdir().unwrap();
    let pid_file = dir.path().join("pid");
    // Written after the job has put more than a pipe buffer (64 KiB) on each
    // stream: only a pipe that is read lets it get there.
    let done = dir.path().join("done");
    BashTool::new()
        .with_timeout(std::time::Duration::from_secs(20))
        .execute(
            serde_json::json!({
                "command": format!(
                    "(sleep 1.5; head -c 300000 /dev/zero; head -c 300000 /dev/zero >&2; touch {}; \
                     while :; do echo tick; sleep 0.05; done) & echo $! > {}; echo hi",
                    done.display(),
                    pid_file.display()
                )
            }),
            ctx("bash"),
        )
        .await
        .unwrap();
    let pid = background_pid(&pid_file).await;
    let mut waited = 0;
    while !done.exists() {
        assert!(waited < 200, "the job never got its output out");
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        waited += 1;
    }
    // Still printing, past the grace.
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    assert!(alive(&pid), "the job died after the call returned");
    std::process::Command::new("kill")
        .arg(&pid)
        .status()
        .unwrap();
}

/// #342: a timeout that fires during that grace does not turn a finished
/// command into a timed-out one, nor kill the job it left running.
#[cfg(unix)]
#[tokio::test(start_paused = true)]
async fn a_timeout_during_the_read_grace_keeps_a_finished_command() {
    let dir = tempfile::tempdir().unwrap();
    let pid_file = dir.path().join("pid");
    let shell_pid_file = dir.path().join("shell-pid");
    let command = format!(
        "sleep 30 & echo $! > {}; echo $$ > {}; echo hi",
        pid_file.display(),
        shell_pid_file.display()
    );
    let clock = tokio::time::Instant::now();
    let run = tokio::spawn(async move {
        BashTool::new()
            .with_timeout(std::time::Duration::from_millis(900))
            .execute(serde_json::json!({"command": command}), ctx("bash"))
            .await
    });
    struct Cleanup {
        run: tokio::task::AbortHandle,
        pid_file: std::path::PathBuf,
    }
    impl Drop for Cleanup {
        fn drop(&mut self) {
            self.run.abort();
            if let Ok(pid) = std::fs::read_to_string(&self.pid_file) {
                let _ = std::process::Command::new("kill")
                    .arg(pid.trim())
                    .stderr(std::process::Stdio::null())
                    .status();
            }
        }
    }
    let _cleanup = Cleanup {
        run: run.abort_handle(),
        pid_file: pid_file.clone(),
    };
    let started = std::time::Instant::now();
    loop {
        if let Ok(pid) = std::fs::read_to_string(&shell_pid_file) {
            // Require a reaped shell, not just a zombie: execute has observed
            // its status before entering the read grace.
            if !pid.trim().is_empty()
                && !std::process::Command::new("kill")
                    .args(["-0", pid.trim()])
                    .stderr(std::process::Stdio::null())
                    .status()
                    .unwrap()
                    .success()
            {
                break;
            }
        }
        assert!(started.elapsed() < std::time::Duration::from_secs(10));
        // Keep a task runnable so Tokio cannot auto-advance past the timeout
        // while the real process is starting or waiting to be reaped.
        tokio::task::yield_now().await;
    }
    assert_eq!(clock.elapsed(), std::time::Duration::ZERO);
    assert!(!run.is_finished(), "the read grace must still be pending");
    tokio::time::advance(std::time::Duration::from_millis(900)).await;
    let result = run.await.unwrap().unwrap();
    assert_eq!(clock.elapsed(), std::time::Duration::from_millis(900));
    assert_eq!(result.details["exit_code"], 0);
    assert_eq!(result.details["output_still_open"], true);
    let text = match &result.content[0] {
        Content::Text { text } => text.clone(),
        other => panic!("{other:?}"),
    };
    assert!(text.contains("hi") && text.contains("NOTE:"), "{text}");
    let pid = background_pid(&pid_file).await;
    assert!(alive(&pid), "the job is left running");
}

/// #341: a FIFO, a device or a directory is refused before any read, write
/// or edit — a FIFO reports a length of 0, so the size check passed and the
/// read blocked for ever; `/dev/zero` read without end. The bounds report a
/// regression, but cannot end it: the blocked `tokio::fs` thread keeps the
/// runtime from shutting down, so a regression still hangs this test.
#[cfg(unix)]
#[tokio::test]
async fn file_tools_refuse_what_is_not_a_regular_file() {
    use std::time::Duration;
    let dir = tempfile::tempdir().unwrap();
    let fifo = dir.path().join("pipe.txt");
    let fifo_png = dir.path().join("pipe.png");
    for path in [&fifo, &fifo_png] {
        let status = std::process::Command::new("mkfifo")
            .arg(path)
            .status()
            .unwrap();
        assert!(status.success(), "mkfifo");
    }
    let bounded = |fut| tokio::time::timeout(Duration::from_secs(5), fut);
    let refused = |result: Result<ToolResult, ToolError>, kind: &str| match result {
        Err(ToolError::Failed(message)) => assert!(message.contains(kind), "{message}"),
        other => panic!("expected a refusal naming {kind}, got {other:?}"),
    };

    let read = ReadFileTool::new();
    for (path, kind) in [
        (fifo.to_str().unwrap(), "FIFO"),
        (fifo_png.to_str().unwrap(), "FIFO"),
        ("/dev/zero", "character device"),
        (dir.path().to_str().unwrap(), "directory"),
    ] {
        let result = bounded(read.execute(serde_json::json!({ "path": path }), ctx("read_file")))
            .await
            .unwrap_or_else(|_| panic!("read_file on {path} hung"));
        refused(result, kind);
    }

    let edit = EditFileTool::new();
    let result = bounded(edit.execute(
        serde_json::json!({ "path": fifo, "old_text": "a", "new_text": "b" }),
        ctx("edit_file"),
    ))
    .await
    .expect("edit_file on a FIFO hung");
    refused(result, "FIFO");

    let write = WriteFileTool::new();
    let result = bounded(write.execute(
        serde_json::json!({ "path": fifo, "content": "x" }),
        ctx("write_file"),
    ))
    .await
    .expect("write_file on a FIFO hung");
    refused(result, "FIFO");

    for path in [dir.path().to_str().unwrap(), "/dev/null"] {
        let kind = if path == "/dev/null" {
            "character device"
        } else {
            "directory"
        };
        let result = bounded(write.execute(
            serde_json::json!({ "path": path, "content": "x" }),
            ctx("write_file"),
        ))
        .await
        .expect("write_file hung");
        refused(result, kind);
        let result = bounded(edit.execute(
            serde_json::json!({ "path": path, "old_text": "a", "new_text": "b" }),
            ctx("edit_file"),
        ))
        .await
        .expect("edit_file hung");
        refused(result, kind);
    }

    // A symlink to a regular file still reads and writes.
    let real = dir.path().join("real.txt");
    std::fs::write(&real, "hello\n").unwrap();
    let link = dir.path().join("link.txt");
    std::os::unix::fs::symlink(&real, &link).unwrap();
    let result = read
        .execute(serde_json::json!({ "path": link }), ctx("read_file"))
        .await
        .unwrap();
    assert!(
        matches!(&result.content[0], Content::Text { text } if text.contains("hello")),
        "{result:?}"
    );
    write
        .execute(
            serde_json::json!({ "path": link, "content": "bye\n" }),
            ctx("write_file"),
        )
        .await
        .unwrap();
    assert_eq!(std::fs::read_to_string(&real).unwrap(), "bye\n");
}

/// A command killed by a signal names the signal: there is no exit code.
#[cfg(unix)]
#[tokio::test]
async fn test_bash_names_the_signal_that_killed_the_command() {
    let result = BashTool::new()
        .execute(
            serde_json::json!({"command": "echo before; kill -9 $$"}),
            ctx("bash"),
        )
        .await
        .unwrap();
    let Content::Text { text } = &result.content[0] else {
        panic!("expected text")
    };
    assert!(
        text.starts_with("Killed by signal 9 (SIGKILL)\nbefore"),
        "{text}"
    );
    assert!(!text.contains("Exit code"), "{text}");
    assert_eq!(result.details["signal"], 9);
    assert_eq!(result.details["exit_code"], -1);
    assert_eq!(result.details["success"], false);

    // An ordinary exit has no signal.
    let result = BashTool::new()
        .execute(serde_json::json!({"command": "exit 3"}), ctx("bash"))
        .await
        .unwrap();
    assert!(result.details["signal"].is_null());
    assert_eq!(result.details["exit_code"], 3);
}

/// Colour codes and terminal titles never reach the model; stderr too.
#[tokio::test]
async fn test_bash_strips_ansi_escapes() {
    let command =
        r"printf '\033]0;title\007\033[1;32mok\033[0m done\n'; printf '\033[31merr\033[0m\n' >&2";
    let result = BashTool::new()
        .execute(serde_json::json!({ "command": command }), ctx("bash"))
        .await
        .unwrap();
    let Content::Text { text } = &result.content[0] else {
        panic!("expected text")
    };
    assert_eq!(
        text, "Exit code: 0\nSTDOUT:\nok done\n\nSTDERR:\nerr\n",
        "{text:?}"
    );

    // Opted out: kept as printed.
    let result = BashTool::new()
        .with_raw_output(true)
        .execute(serde_json::json!({ "command": command }), ctx("bash"))
        .await
        .unwrap();
    let Content::Text { text } = &result.content[0] else {
        panic!("expected text")
    };
    assert!(text.contains("\u{1b}[1;32mok\u{1b}[0m done"), "{text:?}");
}

/// A sequence the output cap cuts in half is dropped, not left as `[1;3`.
#[tokio::test]
async fn test_bash_strips_an_escape_cut_by_the_cap() {
    let mut tool = BashTool::new();
    // 12 bytes, cap 6: the head keeps "ok" + ESC (the cut sequence), the
    // tail "red".
    tool.max_output_bytes = 6;
    let result = tool
        .execute(
            serde_json::json!({"command": r"printf 'ok\033[1;31mred'"}),
            ctx("bash"),
        )
        .await
        .unwrap();
    let Content::Text { text } = &result.content[0] else {
        panic!("expected text")
    };
    assert_eq!(
        text, "Exit code: 0\nok\n... (output truncated: 6 bytes omitted) ...\nred",
        "{text:?}"
    );
}

/// A model writes `\n`; in a CRLF file the edit still applies, and every
/// line it writes ends `\r\n`.
#[tokio::test]
async fn test_edit_file_crlf_file_with_lf_old_text_keeps_crlf() {
    let dir = tempfile::TempDir::new().unwrap();
    let file = dir.path().join("crlf.txt");
    std::fs::write(&file, b"alpha\r\nbeta\r\ngamma\r\n").unwrap();
    let result = EditFileTool::new()
        .execute(
            serde_json::json!({
                "path": file.to_str().unwrap(),
                "old_text": "alpha\nbeta",
                "new_text": "one\ntwo\nthree",
            }),
            ctx("edit_file"),
        )
        .await
        .unwrap();
    assert_eq!(
        std::fs::read(&file).unwrap(),
        b"one\r\ntwo\r\nthree\r\ngamma\r\n"
    );
    assert_eq!(result.details["crlf"], true);
    assert_eq!(result.details["old_lines"], 2);
    assert_eq!(result.details["new_lines"], 3);
}

/// An exact match wins, and an LF file is never rewritten with CRLF.
#[tokio::test]
async fn test_edit_file_exact_match_is_not_adapted() {
    let dir = tempfile::TempDir::new().unwrap();
    let file = dir.path().join("lf.txt");
    std::fs::write(&file, b"alpha\nbeta\ngamma\n").unwrap();
    let result = EditFileTool::new()
        .execute(
            serde_json::json!({
                "path": file.to_str().unwrap(),
                "old_text": "alpha\nbeta",
                "new_text": "A\nB",
            }),
            ctx("edit_file"),
        )
        .await
        .unwrap();
    assert_eq!(std::fs::read(&file).unwrap(), b"A\nB\ngamma\n");
    assert_eq!(result.details["crlf"], false);
}

/// A minified file's line cannot blow up the "did you mean" error.
#[tokio::test]
async fn test_edit_file_not_found_snippet_is_numbered_and_capped() {
    let dir = tempfile::TempDir::new().unwrap();
    let file = dir.path().join("min.js");
    let minified = format!("var a=1;{}\n", "x+=1;".repeat(100_000));
    std::fs::write(&file, format!("// header\n{minified}")).unwrap();
    let err = EditFileTool::new()
        .execute(
            serde_json::json!({
                "path": file.to_str().unwrap(),
                "old_text": "var a=1;\nnot there",
                "new_text": "",
            }),
            ctx("edit_file"),
        )
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("   2 | var a=1;x+=1;"), "{err}");
    assert!(err.contains("… (line truncated)"), "{err}");
    assert!(err.len() < 2_000, "error is {} bytes", err.len());
}

/// An exact one-line match in a CRLF file: the multi-line new_text is
/// written with `\r\n` too, never bare `\n`.
#[tokio::test]
async fn test_edit_file_exact_match_in_a_crlf_file_writes_crlf() {
    let dir = tempfile::TempDir::new().unwrap();
    let file = dir.path().join("crlf.txt");
    for (old, new, expected) in [
        ("alpha", "A\nB", &b"A\r\nB\r\nbeta\r\n"[..]),
        // An old_text already spelled with `\r\n`.
        ("alpha\r\nbeta", "A\nB\nC", &b"A\r\nB\r\nC\r\n"[..]),
    ] {
        std::fs::write(&file, b"alpha\r\nbeta\r\n").unwrap();
        let result = EditFileTool::new()
            .execute(
                serde_json::json!({
                    "path": file.to_str().unwrap(),
                    "old_text": old,
                    "new_text": new,
                }),
                ctx("edit_file"),
            )
            .await
            .unwrap();
        assert_eq!(std::fs::read(&file).unwrap(), expected, "{old:?}");
        assert_eq!(result.details["crlf"], true, "{old:?}");
    }
}

/// The CRLF spelling of old_text is held to the same uniqueness rule.
#[tokio::test]
async fn test_edit_file_crlf_spelling_matching_twice_is_refused() {
    let dir = tempfile::TempDir::new().unwrap();
    let file = dir.path().join("twice.txt");
    let before = b"a\r\nb\r\na\r\nb\r\n";
    std::fs::write(&file, before).unwrap();
    let err = EditFileTool::new()
        .execute(
            serde_json::json!({
                "path": file.to_str().unwrap(),
                "old_text": "a\nb",
                "new_text": "x",
            }),
            ctx("edit_file"),
        )
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("matches 2 locations"), "{err}");
    assert_eq!(std::fs::read(&file).unwrap(), before);
}

/// A timeout's "output so far" is stripped too, and a command the tool
/// killed is a timeout, not a signal death.
#[tokio::test]
async fn test_bash_timeout_output_is_stripped() {
    let err = BashTool::new()
        .with_timeout(std::time::Duration::from_millis(500))
        .execute(
            serde_json::json!({"command": r"printf '\033[31mred\033[0m\n'; sleep 10"}),
            ctx("bash"),
        )
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("timed out"), "{err}");
    assert!(err.contains("red"), "{err}");
    assert!(!err.contains('\u{1b}'), "{err:?}");
    assert!(!err.contains("Killed by signal"), "{err}");
}

// --- write_file after a partial read_file (ReadLog) -------------------------

/// A file of `n` numbered lines in a fresh temp dir.
fn lines_file(n: usize) -> (tempfile::TempDir, String) {
    let dir = tempfile::TempDir::new().unwrap();
    let file = dir.path().join("big.txt");
    let text: Vec<String> = (1..=n).map(|i| format!("line {i}")).collect();
    std::fs::write(&file, text.join("\n")).unwrap();
    let path = file.to_str().unwrap().to_string();
    (dir, path)
}

fn guarded_pair() -> (ReadFileTool, WriteFileTool) {
    let log = ReadLog::new();
    (
        ReadFileTool::new().with_read_log(log.clone()),
        WriteFileTool::new().with_read_log(log),
    )
}

async fn read(tool: &ReadFileTool, args: serde_json::Value) {
    tool.execute(args, ctx("read_file")).await.unwrap();
}

async fn write(tool: &WriteFileTool, path: &str, content: &str) -> Result<ToolResult, ToolError> {
    tool.execute(
        serde_json::json!({"path": path, "content": content}),
        ctx("write_file"),
    )
    .await
}

#[tokio::test]
async fn write_after_a_partial_read_is_refused_and_names_the_lines_seen() {
    let (_dir, path) = lines_file(2140);
    let (r, w) = guarded_pair();
    read(&r, serde_json::json!({"path": path})).await;

    let err = write(&w, &path, "replaced").await.unwrap_err().to_string();
    assert!(err.contains("only lines 1-500 of its 2140 lines"), "{err}");
    assert!(err.contains(&path) && err.contains("edit_file"), "{err}");
    // Nothing was written.
    let on_disk = std::fs::read_to_string(&path).unwrap();
    assert_eq!(on_disk.lines().count(), 2140);
}

#[tokio::test]
async fn write_after_reading_the_rest_is_allowed() {
    let (_dir, path) = lines_file(1200);
    let (r, w) = guarded_pair();
    read(&r, serde_json::json!({"path": path})).await;
    read(
        &r,
        serde_json::json!({"path": path, "offset": 501, "limit": 300}),
    )
    .await;
    let err = write(&w, &path, "x").await.unwrap_err().to_string();
    assert!(err.contains("only lines 1-800 of its 1200 lines"), "{err}");

    // The last page completes the file.
    read(&r, serde_json::json!({"path": path, "offset": 801})).await;
    write(&w, &path, "x").await.unwrap();
}

#[tokio::test]
async fn write_after_a_partial_then_a_full_read_is_allowed() {
    let (_dir, path) = lines_file(900);
    let (r, w) = guarded_pair();
    read(
        &r,
        serde_json::json!({"path": path, "offset": 10, "limit": 5}),
    )
    .await;
    read(&r, serde_json::json!({"path": path, "limit": 900})).await;
    write(&w, &path, "x").await.unwrap();
}

#[tokio::test]
async fn write_to_a_file_never_read_is_allowed() {
    let (_dir, path) = lines_file(900);
    let (_r, w) = guarded_pair();
    write(&w, &path, "x").await.unwrap();
}

#[tokio::test]
async fn a_deleted_file_is_written_and_then_known() {
    let (_dir, path) = lines_file(900);
    let (r, w) = guarded_pair();
    read(&r, serde_json::json!({"path": path})).await;
    assert!(write(&w, &path, "x").await.is_err());
    // A file deleted since the partial read is written; that write makes the
    // whole file known, so the next write goes ahead too.
    std::fs::remove_file(&path).unwrap();
    write(&w, &path, "first").await.unwrap();
    write(&w, &path, "second").await.unwrap();
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "second");
}

#[tokio::test]
async fn tools_built_without_a_shared_log_overwrite_as_before() {
    let (_dir, path) = lines_file(900);
    let r = ReadFileTool::new();
    let w = WriteFileTool::new();
    read(&r, serde_json::json!({"path": path})).await;
    write(&w, &path, "x").await.unwrap();

    // Two separate logs are not shared either.
    let (_dir, path) = lines_file(900);
    let r = ReadFileTool::new().with_read_log(ReadLog::new());
    let w = WriteFileTool::new().with_read_log(ReadLog::new());
    read(&r, serde_json::json!({"path": path})).await;
    write(&w, &path, "x").await.unwrap();
}

#[tokio::test]
async fn differently_spelled_paths_are_one_file() {
    let (dir, path) = lines_file(900);
    std::fs::create_dir(dir.path().join("sub")).unwrap();
    let dotted = format!("{}/./big.txt", dir.path().display());
    let climbed = format!("{}/sub/../big.txt", dir.path().display());
    let (r, w) = guarded_pair();
    read(&r, serde_json::json!({"path": dotted})).await;
    let err = write(&w, &climbed, "x").await.unwrap_err().to_string();
    assert!(err.contains("only lines 1-500"), "{err}");
    // A full read under a third spelling clears it.
    read(&r, serde_json::json!({"path": path, "limit": 900})).await;
    write(&w, &climbed, "x").await.unwrap();
}

#[tokio::test]
async fn sandboxed_tools_are_guarded_too() {
    let (dir, path) = lines_file(900);
    let roots = vec![dir.path().to_str().unwrap().to_string()];
    let log = ReadLog::new();
    let r = ReadFileTool::new()
        .with_allowed_paths(roots.clone())
        .with_read_log(log.clone());
    let w = WriteFileTool::new()
        .with_allowed_paths(roots)
        .with_read_log(log);
    read(&r, serde_json::json!({"path": path})).await;
    assert!(write(&w, &path, "x").await.is_err());
}

#[tokio::test]
async fn a_file_changed_after_a_partial_read_is_still_refused() {
    let (_dir, path) = lines_file(900);
    let (r, w) = guarded_pair();
    read(&r, serde_json::json!({"path": path})).await;
    // Something else (a bash call) rewrites it.
    let text: Vec<String> = (1..=950).map(|i| format!("row {i}")).collect();
    std::fs::write(&path, text.join("\n")).unwrap();

    let err = write(&w, &path, "x").await.unwrap_err().to_string();
    assert!(err.contains("changed on disk since"), "{err}");

    // Pages of the old version do not add up with pages of the new one.
    read(&r, serde_json::json!({"path": path, "offset": 501})).await;
    let err = write(&w, &path, "x").await.unwrap_err().to_string();
    assert!(err.contains("only lines 501-950 of its 950 lines"), "{err}");
    read(&r, serde_json::json!({"path": path, "limit": 500})).await;
    write(&w, &path, "x").await.unwrap();
}

#[tokio::test]
async fn edit_file_is_not_affected_by_a_partial_read() {
    let (_dir, path) = lines_file(900);
    let (r, _w) = guarded_pair();
    read(&r, serde_json::json!({"path": path})).await;
    EditFileTool::new()
        .execute(
            serde_json::json!({"path": path, "old_text": "line 700\n", "new_text": "LINE 700\n"}),
            ctx("edit_file"),
        )
        .await
        .unwrap();
}

#[tokio::test]
async fn default_tools_refuse_a_write_after_a_partial_read() {
    let (_dir, path) = lines_file(2140);
    let tools = default_tools();
    let tool = |name: &str| tools.iter().find(|t| t.name() == name).unwrap();
    tool("read_file")
        .execute(serde_json::json!({"path": path}), ctx("read_file"))
        .await
        .unwrap();
    let err = tool("write_file")
        .execute(
            serde_json::json!({"path": path, "content": "x"}),
            ctx("write_file"),
        )
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("only lines 1-500 of its 2140 lines"), "{err}");

    // Another default_tools() call (another agent) has its own log.
    let other = default_tools();
    other
        .iter()
        .find(|t| t.name() == "write_file")
        .unwrap()
        .execute(
            serde_json::json!({"path": path, "content": "x"}),
            ctx("write_file"),
        )
        .await
        .unwrap();
}

/// Rewrite `path` as `n` new lines (a different length, so a new version).
fn rewrite_lines(path: &str, n: usize) {
    let text: Vec<String> = (1..=n).map(|i| format!("row {i}")).collect();
    std::fs::write(path, text.join("\n")).unwrap();
}

#[tokio::test]
async fn a_page_after_a_full_read_keeps_the_file_known() {
    let (_dir, path) = lines_file(300);
    let (r, w) = guarded_pair();
    read(&r, serde_json::json!({"path": path})).await;
    read(
        &r,
        serde_json::json!({"path": path, "offset": 40, "limit": 21}),
    )
    .await;
    write(&w, &path, "x").await.unwrap();
}

#[tokio::test]
async fn a_page_of_a_file_changed_after_a_full_read_is_refused() {
    let (_dir, path) = lines_file(300);
    let (r, w) = guarded_pair();
    read(&r, serde_json::json!({"path": path})).await;
    rewrite_lines(&path, 320);
    read(
        &r,
        serde_json::json!({"path": path, "offset": 40, "limit": 21}),
    )
    .await;
    let err = write(&w, &path, "x").await.unwrap_err().to_string();
    assert!(err.contains("only lines 40-60 of its 320 lines"), "{err}");
}

#[tokio::test]
async fn an_empty_page_after_a_full_read_changes_nothing() {
    let (_dir, path) = lines_file(10);
    let (r, w) = guarded_pair();
    read(&r, serde_json::json!({"path": path})).await;
    read(&r, serde_json::json!({"path": path, "offset": 50})).await;
    read(&r, serde_json::json!({"path": path, "limit": 0})).await;
    write(&w, &path, "x").await.unwrap();
}

#[tokio::test]
async fn an_empty_page_keeps_the_partial_read() {
    let (_dir, path) = lines_file(900);
    let (r, w) = guarded_pair();
    read(&r, serde_json::json!({"path": path})).await;
    rewrite_lines(&path, 950);
    // Shows no lines of the new version: what was known stays as it was.
    read(&r, serde_json::json!({"path": path, "limit": 0})).await;
    read(&r, serde_json::json!({"path": path, "offset": 5000})).await;
    let err = write(&w, &path, "x").await.unwrap_err().to_string();
    assert!(
        err.contains("only lines 1-500 of its 900 lines, and it has changed on disk since"),
        "{err}"
    );
}

fn numbered(n: usize) -> String {
    let text: Vec<String> = (1..=n).map(|i| format!("line {i}")).collect();
    text.join("\n")
}

#[tokio::test]
async fn a_same_length_rewrite_with_a_new_mtime_is_a_new_version() {
    let (_dir, path) = lines_file(900);
    let (r, w) = guarded_pair();
    read(&r, serde_json::json!({"path": path})).await;
    // Same bytes count: only the modification time tells the versions apart.
    let upper = std::fs::read_to_string(&path)
        .unwrap()
        .replace("line", "LINE");
    std::fs::write(&path, upper).unwrap();
    std::fs::File::options()
        .write(true)
        .open(&path)
        .unwrap()
        .set_modified(std::time::SystemTime::now() + std::time::Duration::from_secs(10))
        .unwrap();

    let err = write(&w, &path, "x").await.unwrap_err().to_string();
    assert!(err.contains("changed on disk since"), "{err}");
    // A page of the new version starts over rather than adding to 1-500.
    read(&r, serde_json::json!({"path": path, "offset": 501})).await;
    let err = write(&w, &path, "x").await.unwrap_err().to_string();
    assert!(err.contains("only lines 501-900 of its 900 lines"), "{err}");
}

#[tokio::test]
async fn pages_read_out_of_order_add_up() {
    let (_dir, path) = lines_file(900);
    let (r, w) = guarded_pair();
    read(&r, serde_json::json!({"path": path, "offset": 501})).await;
    read(&r, serde_json::json!({"path": path})).await;
    write(&w, &path, "x").await.unwrap();
}

#[tokio::test]
async fn overlapping_pages_merge_and_gaps_are_named() {
    let (_dir, path) = lines_file(900);
    let (r, w) = guarded_pair();
    read(&r, serde_json::json!({"path": path})).await;
    read(
        &r,
        serde_json::json!({"path": path, "offset": 400, "limit": 200}),
    )
    .await;
    let err = write(&w, &path, "x").await.unwrap_err().to_string();
    assert!(err.contains("only lines 1-599 of its 900 lines"), "{err}");

    let (_dir, path) = lines_file(900);
    let (r, w) = guarded_pair();
    read(&r, serde_json::json!({"path": path, "limit": 10})).await;
    read(
        &r,
        serde_json::json!({"path": path, "offset": 50, "limit": 11}),
    )
    .await;
    let err = write(&w, &path, "x").await.unwrap_err().to_string();
    assert!(
        err.contains("only lines 1-10, 50-60 of its 900 lines"),
        "{err}"
    );
}

#[tokio::test]
async fn the_agents_own_write_makes_the_file_known() {
    let dir = tempfile::TempDir::new().unwrap();
    let path = dir.path().join("new.txt");
    let path = path.to_str().unwrap();
    let (r, w) = guarded_pair();
    write(&w, path, &numbered(900)).await.unwrap();
    read(&r, serde_json::json!({"path": path})).await;
    write(&w, path, "x").await.unwrap();
}

#[tokio::test]
async fn a_page_of_a_file_changed_after_the_agents_own_write_is_refused() {
    let dir = tempfile::TempDir::new().unwrap();
    let path = dir.path().join("new.txt");
    let path = path.to_str().unwrap();
    let (r, w) = guarded_pair();
    write(&w, path, &numbered(900)).await.unwrap();
    rewrite_lines(path, 950);
    read(&r, serde_json::json!({"path": path})).await;
    let err = write(&w, path, "x").await.unwrap_err().to_string();
    assert!(err.contains("only lines 1-500 of its 950 lines"), "{err}");
}

#[cfg(unix)]
#[tokio::test]
async fn a_partial_read_through_a_symlink_guards_its_target() {
    let (dir, path) = lines_file(900);
    let link = dir.path().join("link.txt");
    std::os::unix::fs::symlink(&path, &link).unwrap();
    let (r, w) = guarded_pair();
    read(&r, serde_json::json!({"path": link.to_str().unwrap()})).await;
    let err = write(&w, &path, "x").await.unwrap_err().to_string();
    assert!(err.contains("only lines 1-500"), "{err}");
}

#[tokio::test]
async fn a_sandboxed_reader_and_an_unrestricted_writer_share_a_log() {
    let (dir, path) = lines_file(900);
    let log = ReadLog::new();
    let r = ReadFileTool::new()
        .with_allowed_paths(vec![dir.path().to_str().unwrap().to_string()])
        .with_read_log(log.clone());
    let w = WriteFileTool::new().with_read_log(log);
    // The same file, relative to the working directory.
    let ups = std::env::current_dir().unwrap().components().count() - 1;
    let relative = format!("{}{}", "../".repeat(ups), path.trim_start_matches('/'));
    read(&r, serde_json::json!({"path": relative})).await;

    let err = write(&w, &path, "x").await.unwrap_err().to_string();
    assert_eq!(
        err,
        format!(
            "Refusing to overwrite {path}: you have read only lines 1-500 of its 900 lines, \
             so writing the whole file would delete lines you have not seen. Read the rest \
             with read_file (offset/limit), or change it in place with edit_file."
        )
    );
}

#[tokio::test]
async fn an_image_read_then_written_is_allowed() {
    let dir = tempfile::TempDir::new().unwrap();
    let path = dir.path().join("x.png");
    std::fs::write(&path, png(1, 1)).unwrap();
    let path = path.to_str().unwrap();
    let (r, w) = guarded_pair();
    let result = r
        .execute(serde_json::json!({"path": path}), ctx("read_file"))
        .await
        .unwrap();
    assert!(matches!(result.content[0], Content::Image { .. }));
    write(&w, path, "x").await.unwrap();
}

#[tokio::test]
async fn a_custom_max_lines_sets_the_page_the_guard_sees() {
    // Ends with a newline: still 900 lines.
    let dir = tempfile::TempDir::new().unwrap();
    let path = dir.path().join("big.txt");
    std::fs::write(&path, numbered(900) + "\n").unwrap();
    let path = path.to_str().unwrap();

    let log = ReadLog::new();
    let r = ReadFileTool {
        max_lines: 100,
        ..Default::default()
    }
    .with_read_log(log.clone());
    let w = WriteFileTool::new().with_read_log(log);
    read(&r, serde_json::json!({"path": path})).await;
    let err = write(&w, path, &(numbered(900) + "\n"))
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("only lines 1-100 of its 900 lines"), "{err}");

    let log = ReadLog::new();
    let r = ReadFileTool {
        max_lines: usize::MAX,
        ..Default::default()
    }
    .with_read_log(log.clone());
    let w = WriteFileTool::new().with_read_log(log);
    read(&r, serde_json::json!({"path": path})).await;
    write(&w, path, "x").await.unwrap();
}

// --- One file, several calls at once ---

/// The loop runs a response's tool calls concurrently. Two edits of one file
/// both read it, then both write it: without a lock per file the last write
/// wins and the other edit is lost — while both calls report success.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn parallel_edits_of_one_file_all_land() {
    let dir = tempfile::TempDir::new().unwrap();
    let path = dir.path().join("many.txt");
    let lines: String = (0..32).map(|i| format!("<{i}>\n")).collect();
    std::fs::write(&path, &lines).unwrap();
    let path = path.to_str().unwrap().to_owned();
    // The same file spelled another way is the same file.
    std::fs::create_dir(dir.path().join("sub")).unwrap();
    let other = dir.path().join("sub/../many.txt");
    let other = other.to_str().unwrap().to_owned();
    // One tool per call, as separate agents (sub-agents) would have.
    let tools: Vec<EditFileTool> = (0..32).map(|_| EditFileTool::new()).collect();
    let calls = tools.iter().enumerate().map(|(i, tool)| {
        tool.execute(
            serde_json::json!({
                "path": if i % 2 == 0 { &path } else { &other },
                "old_text": format!("<{i}>\n"),
                "new_text": format!("<edited {i}>\n"),
            }),
            ctx("edit_file"),
        )
    });
    for result in futures::future::join_all(calls).await {
        result.unwrap();
    }
    let after = std::fs::read_to_string(&path).unwrap();
    let lost: Vec<usize> = (0..32)
        .filter(|i| !after.contains(&format!("<edited {i}>\n")))
        .collect();
    assert!(lost.is_empty(), "edits lost: {lost:?}\n{after}");
}

/// A write and an edit of one file at once: the edit applies to the file the
/// write produced, or the write replaces the edited file — never an edit of
/// the old content written over the new.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_write_and_an_edit_of_one_file_do_not_interleave() {
    for _ in 0..20 {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("f.txt");
        std::fs::write(&path, "keep <a>\n").unwrap();
        let path = path.to_str().unwrap().to_owned();
        let write = WriteFileTool::new();
        let edit = EditFileTool::new();
        let (w, e) = tokio::join!(
            write.execute(
                serde_json::json!({"path": path, "content": "new <a>\n"}),
                ctx("write_file"),
            ),
            edit.execute(
                serde_json::json!({"path": path, "old_text": "<a>", "new_text": "<b>"}),
                ctx("edit_file"),
            ),
        );
        w.unwrap();
        let after = std::fs::read_to_string(&path).unwrap();
        match e {
            // The edit ran second, on the written file; or first, and the
            // write replaced it.
            Ok(_) => assert!(after == "new <b>\n" || after == "new <a>\n", "{after:?}"),
            Err(e) => panic!("{e}"),
        }
        assert_ne!(
            after, "keep <b>\n",
            "an edit of the old file overwrote the write"
        );
    }
}

// --- Images a provider will take ---
//
// An image block a provider rejects stays in the history and fails every
// later request too, so `read_file` sends only what all of them accept.

/// A PNG of `w`×`h` (the header is all a reader of its size looks at).
fn png(w: u32, h: u32) -> Vec<u8> {
    let mut b = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
    b.extend([0, 0, 0, 13]);
    b.extend(b"IHDR");
    b.extend(w.to_be_bytes());
    b.extend(h.to_be_bytes());
    b.extend([8, 2, 0, 0, 0, 0, 0, 0, 0]);
    b.extend([0, 0, 0, 0]);
    b.extend(b"IEND");
    b.extend([0xAE, 0x42, 0x60, 0x82]);
    b
}

/// A baseline JPEG of `w`×`h`: SOI, a JFIF segment, SOF0, EOI.
fn jpeg(w: u16, h: u16) -> Vec<u8> {
    let mut b = vec![0xFF, 0xD8];
    b.extend([0xFF, 0xE0, 0x00, 0x10]);
    b.extend(b"JFIF\0");
    b.extend([1, 1, 0, 0, 1, 0, 1, 0, 0]);
    b.extend([0xFF, 0xC0, 0x00, 0x11, 0x08]);
    b.extend(h.to_be_bytes());
    b.extend(w.to_be_bytes());
    b.extend([3, 1, 0x22, 0, 2, 0x11, 1, 3, 0x11, 1]);
    b.extend([0xFF, 0xD9]);
    b
}

async fn read_image(name: &str, bytes: &[u8]) -> Result<ToolResult, ToolError> {
    let dir = tempfile::TempDir::new().unwrap();
    let path = dir.path().join(name);
    std::fs::write(&path, bytes).unwrap();
    ReadFileTool::new()
        .execute(
            serde_json::json!({"path": path.to_str().unwrap()}),
            ctx("read_file"),
        )
        .await
}

fn image_type(result: &ToolResult) -> &str {
    match &result.content[0] {
        Content::Image { mime_type, .. } => mime_type,
        other => panic!("expected an image, got {other:?}"),
    }
}

#[tokio::test]
async fn an_image_is_typed_by_its_content_not_its_name() {
    // A JPEG saved as .png: sent as image/png, a provider refuses it.
    let result = read_image("photo.png", &jpeg(10, 10)).await.unwrap();
    assert_eq!(image_type(&result), "image/jpeg");
    let result = read_image("shot.jpg", &png(10, 10)).await.unwrap();
    assert_eq!(image_type(&result), "image/png");
}

/// A WebP of `w`×`h`: `kind` is `VP8 ` (lossy), `VP8L` or `VP8X`.
fn webp(kind: &[u8; 4], w: u32, h: u32) -> Vec<u8> {
    let mut b = b"RIFF\0\0\0\0WEBP".to_vec();
    b.extend(kind);
    b.extend([0; 4]);
    match kind {
        b"VP8 " => {
            b.extend([0, 0, 0, 0x9D, 0x01, 0x2A]);
            b.extend((w as u16).to_le_bytes());
            b.extend((h as u16).to_le_bytes());
        }
        b"VP8L" => {
            b.push(0x2F);
            b.extend(((w - 1) | ((h - 1) << 14)).to_le_bytes());
        }
        _ => {
            b.extend([0; 4]);
            b.extend(&(w - 1).to_le_bytes()[..3]);
            b.extend(&(h - 1).to_le_bytes()[..3]);
        }
    }
    b
}

#[tokio::test]
async fn webp_sizes_are_read_in_all_three_forms() {
    for kind in [b"VP8 ", b"VP8L", b"VP8X"] {
        let ok = read_image("a.webp", &webp(kind, 20, 10)).await.unwrap();
        assert_eq!(image_type(&ok), "image/webp");
        let err = read_image("a.webp", &webp(kind, 9000, 10))
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("9000×10"), "{kind:?}: {err}");
    }
    // The extended form's sizes are 24-bit.
    let err = read_image("a.webp", &webp(b"VP8X", 70000, 10))
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("70000×10"), "{err}");
    // The lossless form stores width - 1: 8001 is refused, 8000 sent.
    assert!(read_image("a.webp", &webp(b"VP8L", 8001, 1)).await.is_err());
    assert!(read_image("a.webp", &webp(b"VP8L", 8000, 1)).await.is_ok());
}

/// A camera JPEG: Exif (APP1), padding, a Huffman table (`FF C4`, which is
/// not a frame), a fill byte and a marker with no length, then the frame
/// (`sof`: SOF2 is progressive).
fn camera_jpeg(sof: u8, precision: u8, w: u16, h: u16) -> Vec<u8> {
    let mut b = vec![0xFF, 0xD8];
    b.extend([0xFF, 0xE1, 0x00, 0x0A]);
    b.extend(b"Exif\0\0\x4D\x4D");
    b.extend([0x00, 0x00]);
    b.extend([0xFF, 0xC4, 0x00, 0x04, 0x00, 0x00]);
    b.extend([0xFF, 0xFF, 0x01]);
    b.extend([0xFF, sof, 0x00, 0x11, precision]);
    b.extend(h.to_be_bytes());
    b.extend(w.to_be_bytes());
    b.extend([3, 1, 0x22, 0, 2, 0x11, 1, 3, 0x11, 1]);
    b.extend([0xFF, 0xD9]);
    b
}

#[tokio::test]
async fn a_camera_jpeg_is_measured_past_its_other_segments() {
    let ok = read_image("p.jpg", &camera_jpeg(0xC2, 8, 4000, 3000))
        .await
        .unwrap();
    assert_eq!(image_type(&ok), "image/jpeg");
    // Width and height are not swapped.
    let err = read_image("p.jpg", &camera_jpeg(0xC2, 8, 9000, 100))
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("9000×100"), "{err}");
}

#[tokio::test]
async fn a_jpeg_decoders_seldom_read_is_refused() {
    // Lossless (SOF3), hierarchical (SOF5), arithmetic-coded (SOF9), and a
    // 12-bit baseline frame.
    for (sof, precision) in [(0xC3, 8), (0xC5, 8), (0xC9, 8), (0xC1, 12)] {
        let err = read_image("p.jpg", &camera_jpeg(sof, precision, 10, 10))
            .await
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("may not decode"),
            "{sof:#x}/{precision}: {err}"
        );
    }
}

#[tokio::test]
async fn the_side_limit_is_8000_px_and_a_zero_side_is_refused() {
    assert!(read_image("a.png", &png(8000, 8000)).await.is_ok());
    let err = read_image("a.png", &png(8001, 1))
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("8001×1"), "{err}");
    let err = read_image("a.png", &png(0, 10))
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("no width or height"), "{err}");
}

#[tokio::test]
async fn an_image_cut_off_before_its_size_is_refused() {
    let err = read_image("a.png", &png(10, 10)[..18])
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("cannot read the image's size"), "{err}");
    let jpeg = camera_jpeg(0xC0, 8, 10, 10);
    let err = read_image("a.jpg", &jpeg[..jpeg.len() - 14])
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("cannot read the image's size"), "{err}");
}

#[tokio::test]
async fn image_extensions_are_matched_in_any_case() {
    assert_eq!(
        image_type(&read_image("A.PNG", &png(4, 4)).await.unwrap()),
        "image/png"
    );
    let err = read_image("A.BMP", b"BM..").await.unwrap_err().to_string();
    assert!(err.contains("BMP images are not sent"), "{err}");
}

#[tokio::test]
async fn a_large_file_that_is_not_an_image_says_so() {
    let mut text = b"not an image at all\n".to_vec();
    text.resize(4 * 1024 * 1024, b'x');
    let err = read_image("big.png", &text).await.unwrap_err().to_string();
    assert!(err.contains("not a PNG, JPEG or WebP image"), "{err}");
}

#[tokio::test]
async fn formats_providers_refuse_are_not_sent() {
    // GIF: Gemini does not list it, OpenAI takes only still ones.
    for name in ["a.gif", "a.bmp", "a.tiff", "a.heic", "a.avif"] {
        let err = read_image(name, b"GIF89a....")
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("PNG, JPEG or WebP"), "{name}: {err}");
    }
}

#[tokio::test]
async fn a_file_named_as_an_image_that_is_not_one_is_refused() {
    let err = read_image("notes.png", b"just some text\n")
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("not a PNG, JPEG or WebP image"), "{err}");
}

#[tokio::test]
async fn an_image_larger_than_providers_take_is_refused() {
    let mut big = png(100, 100);
    big.resize(4 * 1024 * 1024, 0);
    let err = read_image("big.png", &big).await.unwrap_err().to_string();
    assert!(err.contains("too large"), "{err}");

    let wide = png(9000, 100);
    let err = read_image("wide.png", &wide).await.unwrap_err().to_string();
    assert!(err.contains("9000×100"), "{err}");
}

#[tokio::test]
async fn the_image_size_limit_is_a_knob() {
    let dir = tempfile::TempDir::new().unwrap();
    let path = dir.path().join("big.png");
    let mut big = png(100, 100);
    big.resize(4 * 1024 * 1024, 0);
    std::fs::write(&path, &big).unwrap();
    let tool = ReadFileTool::new().with_max_image_bytes(8 * 1024 * 1024);
    let result = tool
        .execute(
            serde_json::json!({"path": path.to_str().unwrap()}),
            ctx("read_file"),
        )
        .await
        .unwrap();
    assert_eq!(image_type(&result), "image/png");
}

#[tokio::test]
async fn an_animated_webp_is_refused() {
    let mut animated = webp(b"VP8X", 20, 10);
    animated[20] |= 0x02;
    let err = read_image("a.webp", &animated)
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("animated WebP"), "{err}");
}

/// A large camera JPEG whose metadata runs past the bytes read of a file too
/// large to send is "too large", not "damaged".
#[tokio::test]
async fn a_large_camera_jpeg_with_long_metadata_is_too_large() {
    let mut b = vec![0xFF, 0xD8];
    for _ in 0..2 {
        b.extend([0xFF, 0xE1, 0xFF, 0xFF]);
        b.extend(std::iter::repeat_n(0u8, 0xFFFF - 2));
    }
    b.extend(&camera_jpeg(0xC0, 8, 4000, 3000)[2..]);
    b.resize(4_200_000, 0);
    let err = read_image("big.jpg", &b).await.unwrap_err().to_string();
    assert!(err.contains("image too large (4.2 MB"), "{err}");
    assert!(err.contains("up to 3.75 MB"), "{err}");
}

#[tokio::test]
async fn a_large_file_of_a_refused_format_says_its_format() {
    let mut gif = b"GIF89a".to_vec();
    gif.resize(4_200_000, 0);
    let err = read_image("big.gif", &gif).await.unwrap_err().to_string();
    assert!(err.contains("GIF images are not sent"), "{err}");
}

#[tokio::test]
async fn an_image_of_exactly_the_limit_is_sent() {
    let dir = tempfile::TempDir::new().unwrap();
    let path = dir.path().join("x.png");
    let mut bytes = png(10, 10);
    bytes.resize(1000, 0);
    std::fs::write(&path, &bytes).unwrap();
    let read = |max| {
        let path = path.to_str().unwrap().to_owned();
        async move {
            ReadFileTool::new()
                .with_max_image_bytes(max)
                .execute(serde_json::json!({"path": path}), ctx("read_file"))
                .await
        }
    };
    assert!(read(1000).await.is_ok());
    let err = read(999).await.unwrap_err().to_string();
    assert!(err.contains("too large"), "{err}");
}

/// A file over `max_bytes` can be read in pages: the size error says to use
/// offset/limit, so offset/limit has to work.
#[tokio::test]
async fn read_file_pages_through_a_file_over_the_size_limit() {
    let tmp = tempfile::TempDir::new().unwrap();
    let path = tmp.path().join("big.txt");
    let body: String = (1..=200_000).map(|i| format!("line {i}\n")).collect();
    std::fs::write(&path, &body).unwrap();
    assert!(body.len() > 1024 * 1024);
    let tool = ReadFileTool::new();
    let path = path.to_str().unwrap();

    let err = tool
        .execute(serde_json::json!({ "path": path }), ctx("read_file"))
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("offset/limit"), "{err}");

    let result = tool
        .execute(
            serde_json::json!({ "path": path, "offset": 150_000, "limit": 3 }),
            ctx("read_file"),
        )
        .await
        .expect("a page of a large file reads");
    let Content::Text { text } = &result.content[0] else {
        panic!("expected text")
    };
    assert!(text.starts_with("[Lines 150000-150002 of 200000"), "{text}");
    assert!(text.contains("150000 | line 150000"), "{text}");
    assert!(text.contains("150002 | line 150002"), "{text}");
    assert!(!text.contains("line 150003"), "{text}");
}

/// A file that is not UTF-8 is not "missing": the error must not steer the
/// model to overwrite it with write_file.
#[tokio::test]
async fn edit_file_on_a_non_utf8_file_does_not_suggest_write_file() {
    let tmp = tempfile::TempDir::new().unwrap();
    let path = tmp.path().join("latin1.txt");
    std::fs::write(&path, b"caf\xe9\n").unwrap();
    let err = EditFileTool::new()
        .execute(
            serde_json::json!({
                "path": path.to_str().unwrap(),
                "old_text": "caf",
                "new_text": "cafe",
            }),
            ctx("edit_file"),
        )
        .await
        .unwrap_err()
        .to_string();
    assert!(!err.contains("write_file"), "{err}");
    assert!(err.contains("UTF-8"), "{err}");
    assert_eq!(std::fs::read(&path).unwrap(), b"caf\xe9\n", "not modified");
}

/// write_file and edit_file replace a file whole (temp file + rename): a
/// reader that opened it before sees the complete old content, never a
/// truncated or half-written one — the same guarantee a crash or a full disk
/// mid-write gets.
#[cfg(unix)]
#[tokio::test]
async fn writes_replace_the_file_whole() {
    use std::io::Read;
    let tmp = tempfile::TempDir::new().unwrap();
    let path = tmp.path().join("doc.txt");
    let old = "old content\n".repeat(1000);
    std::fs::write(&path, &old).unwrap();
    let p = path.to_str().unwrap();

    let mut before = std::fs::File::open(&path).unwrap();
    WriteFileTool::new()
        .execute(
            serde_json::json!({ "path": p, "content": "new\n" }),
            ctx("write_file"),
        )
        .await
        .unwrap();
    let mut seen = String::new();
    before.read_to_string(&mut seen).unwrap();
    assert_eq!(seen, old, "write_file rewrote the file in place");

    let mut before = std::fs::File::open(&path).unwrap();
    EditFileTool::new()
        .execute(
            serde_json::json!({ "path": p, "old_text": "new", "new_text": "newer" }),
            ctx("edit_file"),
        )
        .await
        .unwrap();
    let mut seen = String::new();
    before.read_to_string(&mut seen).unwrap();
    assert_eq!(seen, "new\n", "edit_file rewrote the file in place");
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "newer\n");
}

/// The swap keeps what an in-place write kept: a symlink stays a symlink
/// (its target is replaced), the file's permissions are kept, no temp file
/// is left behind, and a directory that refuses new files still lets an
/// existing writable file be written.
#[cfg(unix)]
#[tokio::test]
async fn atomic_writes_keep_links_permissions_and_work_in_a_locked_directory() {
    use std::os::unix::fs::PermissionsExt;
    let tmp = tempfile::TempDir::new().unwrap();
    let target = tmp.path().join("real.txt");
    let link = tmp.path().join("link.txt");
    std::fs::write(&target, "a\n").unwrap();
    std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o640)).unwrap();
    std::os::unix::fs::symlink(&target, &link).unwrap();

    WriteFileTool::new()
        .execute(
            serde_json::json!({ "path": link.to_str().unwrap(), "content": "b\n" }),
            ctx("write_file"),
        )
        .await
        .unwrap();
    assert!(std::fs::symlink_metadata(&link)
        .unwrap()
        .file_type()
        .is_symlink());
    assert_eq!(std::fs::read_to_string(&target).unwrap(), "b\n");
    let mode = std::fs::metadata(&target).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o640);
    let names: Vec<String> = std::fs::read_dir(tmp.path())
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(names.len(), 2, "a temp file was left behind: {names:?}");

    // A directory where no file can be created (root ignores the bits).
    if unsafe { libc_geteuid() } != 0 {
        std::fs::set_permissions(tmp.path(), std::fs::Permissions::from_mode(0o555)).unwrap();
        let result = WriteFileTool::new()
            .execute(
                serde_json::json!({ "path": target.to_str().unwrap(), "content": "c\n" }),
                ctx("write_file"),
            )
            .await;
        std::fs::set_permissions(tmp.path(), std::fs::Permissions::from_mode(0o755)).unwrap();
        result.expect("an existing writable file in a locked directory");
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "c\n");
    }
}

#[cfg(unix)]
extern "C" {
    #[link_name = "geteuid"]
    fn libc_geteuid() -> u32;
}

/// The atomic swap refuses what an in-place write could not do: a read-only
/// file is not replaced (a rename needs only the directory's permission).
#[cfg(unix)]
#[tokio::test]
async fn atomic_writes_refuse_a_read_only_file() {
    use std::os::unix::fs::PermissionsExt;
    if unsafe { libc_geteuid() } == 0 {
        return; // root writes anything
    }
    let tmp = tempfile::TempDir::new().unwrap();
    let path = tmp.path().join("locked.txt");
    std::fs::write(&path, "keep\n").unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o444)).unwrap();
    let p = path.to_str().unwrap();
    assert!(WriteFileTool::new()
        .execute(
            serde_json::json!({ "path": p, "content": "x\n" }),
            ctx("write_file")
        )
        .await
        .is_err());
    assert!(EditFileTool::new()
        .execute(
            serde_json::json!({ "path": p, "old_text": "keep", "new_text": "x" }),
            ctx("edit_file")
        )
        .await
        .is_err());
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "keep\n");
}

/// A dangling symlink is written through (its target created), not replaced
/// by a regular file; a private file's mode is kept.
#[cfg(unix)]
#[tokio::test]
async fn atomic_writes_follow_a_dangling_link_and_keep_a_private_mode() {
    use std::os::unix::fs::PermissionsExt;
    let tmp = tempfile::TempDir::new().unwrap();
    let missing = tmp.path().join("missing.txt");
    let link = tmp.path().join("dangling.txt");
    std::os::unix::fs::symlink(&missing, &link).unwrap();
    WriteFileTool::new()
        .execute(
            serde_json::json!({ "path": link.to_str().unwrap(), "content": "made\n" }),
            ctx("write_file"),
        )
        .await
        .unwrap();
    assert!(std::fs::symlink_metadata(&link)
        .unwrap()
        .file_type()
        .is_symlink());
    assert_eq!(std::fs::read_to_string(&missing).unwrap(), "made\n");

    let secret = tmp.path().join(".env");
    std::fs::write(&secret, "A=1\n").unwrap();
    std::fs::set_permissions(&secret, std::fs::Permissions::from_mode(0o600)).unwrap();
    WriteFileTool::new()
        .execute(
            serde_json::json!({ "path": secret.to_str().unwrap(), "content": "A=2\n" }),
            ctx("write_file"),
        )
        .await
        .unwrap();
    let mode = std::fs::metadata(&secret).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600);
}

/// A page of a file that is one huge line is bounded: the line is cut at
/// `max_bytes`, not loaded and returned whole.
#[tokio::test]
async fn read_file_bounds_a_page_of_one_huge_line() {
    let tmp = tempfile::TempDir::new().unwrap();
    let path = tmp.path().join("bundle.min.js");
    std::fs::write(&path, "x".repeat(3 * 1024 * 1024)).unwrap();
    let result = ReadFileTool::new()
        .execute(
            serde_json::json!({ "path": path.to_str().unwrap(), "offset": 1, "limit": 5 }),
            ctx("read_file"),
        )
        .await
        .unwrap();
    let Content::Text { text } = &result.content[0] else {
        panic!("expected text")
    };
    assert!(text.len() < 1024 * 1024 + 200, "{} bytes", text.len());
    assert!(text.contains("line cut at"), "{}", &text[..100]);
    assert!(
        text.starts_with("[Lines 1-1 of 1 — line 1 longer than"),
        "{}",
        &text[..80]
    );
}

/// A line shown cut was not read: write_file must not then replace the file
/// whole, whatever pages follow.
#[tokio::test]
async fn a_cut_line_keeps_the_partial_read_guard() {
    let tmp = tempfile::TempDir::new().unwrap();
    let path = tmp.path().join("bundle.min.js");
    std::fs::write(&path, "x".repeat(3 * 1024 * 1024)).unwrap();
    let log = ReadLog::new();
    let read = ReadFileTool::new().with_read_log(log.clone());
    let write = WriteFileTool::new().with_read_log(log);
    let p = path.to_str().unwrap();
    read.execute(
        serde_json::json!({ "path": p, "offset": 1, "limit": 5 }),
        ctx("read_file"),
    )
    .await
    .unwrap();
    let err = write
        .execute(
            serde_json::json!({ "path": p, "content": "x\n" }),
            ctx("write_file"),
        )
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("Refusing to overwrite"), "{err}");
    assert_eq!(std::fs::metadata(&path).unwrap().len(), 3 * 1024 * 1024);
}

/// Many empty lines with a huge limit stay within the page budget: each
/// line's number counts.
#[tokio::test]
async fn a_page_of_empty_lines_is_bounded() {
    let tmp = tempfile::TempDir::new().unwrap();
    let path = tmp.path().join("blank.txt");
    std::fs::write(&path, "\n".repeat(3 * 1024 * 1024)).unwrap();
    let result = ReadFileTool::new()
        .execute(
            serde_json::json!({ "path": path.to_str().unwrap(), "offset": 1, "limit": 1_000_000_000u64 }),
            ctx("read_file"),
        )
        .await
        .unwrap();
    let Content::Text { text } = &result.content[0] else {
        panic!("expected text")
    };
    assert!(text.len() < 2 * 1024 * 1024, "{} bytes", text.len());
    assert!(
        text.contains(&format!("of {}", 3 * 1024 * 1024)),
        "{}",
        &text[..80]
    );
}
