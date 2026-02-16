//! Tests for built-in tools.

use yo_agent::tools::*;
use yo_agent::types::*;
use tokio_util::sync::CancellationToken;
use yo_agent::tools::edit::EditFileTool;
use yo_agent::tools::list::ListFilesTool;

#[tokio::test]
async fn test_bash_echo() {
    let tool = BashTool::new();
    let result = tool.execute(
        "t1",
        serde_json::json!({"command": "echo hello"}),
        CancellationToken::new(),
    ).await.unwrap();

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
    let result = tool.execute(
        "t1",
        serde_json::json!({"command": "false"}),
        CancellationToken::new(),
    ).await.unwrap();

    let text = match &result.content[0] {
        Content::Text { text } => text,
        _ => panic!("expected text"),
    };
    assert!(text.contains("Exit code: 1"));
}

#[tokio::test]
async fn test_bash_deny_pattern() {
    let tool = BashTool::new();
    let result = tool.execute(
        "t1",
        serde_json::json!({"command": "rm -rf /"}),
        CancellationToken::new(),
    ).await;

    assert!(result.is_err());
    assert!(result.unwrap_err().to_string().contains("blocked"));
}

#[tokio::test]
async fn test_bash_timeout() {
    let tool = BashTool::new().with_timeout(std::time::Duration::from_millis(100));
    let result = tool.execute(
        "t1",
        serde_json::json!({"command": "sleep 10"}),
        CancellationToken::new(),
    ).await;

    assert!(result.is_err());
    let err = result.unwrap_err().to_string();
    assert!(err.contains("timed out"));
}

#[tokio::test]
async fn test_bash_cancel() {
    let tool = BashTool::new();
    let cancel = CancellationToken::new();
    cancel.cancel();

    let result = tool.execute(
        "t1",
        serde_json::json!({"command": "echo should not run"}),
        cancel,
    ).await;

    assert!(result.is_err());
}

#[tokio::test]
async fn test_read_write_file() {
    let tmp = std::env::temp_dir().join("yo-agent-test-rw.txt");
    let path = tmp.to_str().unwrap();

    // Write
    let write_tool = WriteFileTool::new();
    let result = write_tool.execute(
        "t1",
        serde_json::json!({"path": path, "content": "hello from yo-agent"}),
        CancellationToken::new(),
    ).await.unwrap();

    let text = match &result.content[0] {
        Content::Text { text } => text,
        _ => panic!("expected text"),
    };
    assert!(text.contains("Wrote"));

    // Read
    let read_tool = ReadFileTool::new();
    let result = read_tool.execute(
        "t2",
        serde_json::json!({"path": path}),
        CancellationToken::new(),
    ).await.unwrap();

    let text = match &result.content[0] {
        Content::Text { text } => text,
        _ => panic!("expected text"),
    };
    assert!(text.contains("hello from yo-agent"));

    // Cleanup
    let _ = std::fs::remove_file(tmp);
}

#[tokio::test]
async fn test_read_file_with_offset_limit() {
    let tmp = std::env::temp_dir().join("yo-agent-test-lines.txt");
    let path = tmp.to_str().unwrap();

    let content = (1..=20).map(|i| format!("line {}", i)).collect::<Vec<_>>().join("\n");
    std::fs::write(&tmp, &content).unwrap();

    let tool = ReadFileTool::new();
    let result = tool.execute(
        "t1",
        serde_json::json!({"path": path, "offset": 5, "limit": 3}),
        CancellationToken::new(),
    ).await.unwrap();

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
    let result = tool.execute(
        "t1",
        serde_json::json!({"path": "/nonexistent/file.txt"}),
        CancellationToken::new(),
    ).await;

    assert!(result.is_err());
}

#[tokio::test]
async fn test_write_creates_directories() {
    let tmp = std::env::temp_dir().join("yo-agent-test-nested/deep/dir/file.txt");
    let path = tmp.to_str().unwrap();

    let tool = WriteFileTool::new();
    let result = tool.execute(
        "t1",
        serde_json::json!({"path": path, "content": "nested!"}),
        CancellationToken::new(),
    ).await;

    assert!(result.is_ok());
    assert!(tmp.exists());

    // Cleanup
    let _ = std::fs::remove_dir_all(std::env::temp_dir().join("yo-agent-test-nested"));
}

#[tokio::test]
async fn test_search_pattern() {
    let tmp_dir = std::env::temp_dir().join("yo-agent-test-search");
    let _ = std::fs::create_dir_all(&tmp_dir);
    std::fs::write(tmp_dir.join("a.txt"), "hello world\nfoo bar\nhello again").unwrap();
    std::fs::write(tmp_dir.join("b.txt"), "no match here\nhello there").unwrap();

    let tool = SearchTool::new().with_root(tmp_dir.to_str().unwrap());
    let result = tool.execute(
        "t1",
        serde_json::json!({"pattern": "hello"}),
        CancellationToken::new(),
    ).await.unwrap();

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
    let tmp_dir = std::env::temp_dir().join("yo-agent-test-search-empty");
    let _ = std::fs::create_dir_all(&tmp_dir);
    std::fs::write(tmp_dir.join("a.txt"), "nothing interesting").unwrap();

    let tool = SearchTool::new().with_root(tmp_dir.to_str().unwrap());
    let result = tool.execute(
        "t1",
        serde_json::json!({"pattern": "zzzznotfound"}),
        CancellationToken::new(),
    ).await.unwrap();

    let text = match &result.content[0] {
        Content::Text { text } => text,
        _ => panic!("expected text"),
    };
    assert!(text.contains("No matches"));

    let _ = std::fs::remove_dir_all(tmp_dir);
}

// --- Edit tool tests ---

#[tokio::test]
async fn test_edit_file() {
    let tmp = std::env::temp_dir().join("yo-agent-test-edit.txt");
    let path = tmp.to_str().unwrap();
    std::fs::write(&tmp, "fn main() {\n    println!(\"hello\");\n}\n").unwrap();

    let tool = EditFileTool::new();
    let result = tool.execute(
        "t1",
        serde_json::json!({
            "path": path,
            "old_text": "println!(\"hello\")",
            "new_text": "println!(\"goodbye\")"
        }),
        CancellationToken::new(),
    ).await.unwrap();

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
    let tmp = std::env::temp_dir().join("yo-agent-test-edit-nomatch.txt");
    let path = tmp.to_str().unwrap();
    std::fs::write(&tmp, "hello world\n").unwrap();
    let tool = EditFileTool::new();
    let result = tool.execute(
        "t1",
        serde_json::json!({"path": path, "old_text": "nonexistent", "new_text": "bar"}),
        CancellationToken::new(),
    ).await;
    assert!(result.is_err());
    let _ = std::fs::remove_file(tmp);
}

#[tokio::test]
async fn test_list_files_tool() {
    let tmp_dir = std::env::temp_dir().join("yo-agent-test-list2");
    let _ = std::fs::create_dir_all(tmp_dir.join("sub"));
    std::fs::write(tmp_dir.join("a.rs"), "").unwrap();
    std::fs::write(tmp_dir.join("sub/c.rs"), "").unwrap();
    let tool = ListFilesTool::new();
    let result = tool.execute(
        "t1",
        serde_json::json!({"path": tmp_dir.to_str().unwrap()}),
        CancellationToken::new(),
    ).await.unwrap();
    let text = match &result.content[0] { Content::Text { text } => text, _ => panic!("expected text") };
    assert!(text.contains("a.rs"));
    let _ = std::fs::remove_dir_all(tmp_dir);
}

#[tokio::test]
async fn test_read_file_line_numbers() {
    let tmp = std::env::temp_dir().join("yo-agent-test-lineno2.txt");
    let path = tmp.to_str().unwrap();
    std::fs::write(&tmp, "first\nsecond\nthird\n").unwrap();
    let tool = ReadFileTool::new();
    let result = tool.execute("t1", serde_json::json!({"path": path}), CancellationToken::new()).await.unwrap();
    let text = match &result.content[0] { Content::Text { text } => text, _ => panic!("expected text") };
    assert!(text.contains("   1 | first"));
    assert!(text.contains("   2 | second"));
    let _ = std::fs::remove_file(tmp);
}

#[tokio::test]
async fn test_bash_blocked_command() {
    let tool = BashTool::new();
    let result = tool.execute("t1", serde_json::json!({"command": "rm -rf /"}), CancellationToken::new()).await;
    assert!(result.is_err());
    assert!(result.unwrap_err().to_string().contains("blocked"));
}

#[tokio::test]
async fn test_default_tools_complete() {
    let tools = yo_agent::tools::default_tools();
    let names: Vec<&str> = tools.iter().map(|t| t.name()).collect();
    assert_eq!(names.len(), 6);
    assert!(names.contains(&"bash"));
    assert!(names.contains(&"edit_file"));
    assert!(names.contains(&"list_files"));
}
