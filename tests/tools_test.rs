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
    tool.max_output_bytes = 4; // "日" is 3 bytes: the cut lands inside "本"
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
    assert!(text.contains("(output truncated)"), "{text}");
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
    assert!(text.contains("(output truncated)"));
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
    assert!(text.ends_with("(output truncated)"), "stderr is cut");
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
    assert!(err.contains("(output truncated)"), "{err}");
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
    std::fs::write(&tmp, b"fake-jpeg-data").unwrap();

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
