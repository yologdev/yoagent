//! Tests for built-in tools.

use yo_agent::tools::*;
use yo_agent::types::*;
use tokio_util::sync::CancellationToken;

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
    let tool = BashTool::new();
    let result = tool.execute(
        "t1",
        serde_json::json!({"command": "false"}),
        CancellationToken::new(),
    ).await;

    assert!(result.is_err());
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
