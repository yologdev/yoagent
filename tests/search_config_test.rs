//! A user's ripgrep config (`RIPGREP_CONFIG_PATH`) must not reach the
//! `search` tool (#315). Its own binary: it sets a process-wide variable.
#![cfg(all(feature = "native", unix))]

use yoagent::tools::SearchTool;
use yoagent::types::*;

fn ctx() -> ToolContext {
    ToolContext::new("c1", "search")
}

fn rg_installed() -> bool {
    std::process::Command::new("rg")
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

#[tokio::test]
async fn a_ripgrep_config_cannot_change_the_output_or_escape_allowed_paths() {
    if !rg_installed() {
        // CI's main test jobs install ripgrep and set this; elsewhere optional.
        assert!(
            std::env::var_os("YOAGENT_REQUIRE_RG").is_none(),
            "YOAGENT_REQUIRE_RG is set but ripgrep is not installed"
        );
        eprintln!("skipped: ripgrep is not installed (tests/search_engine_test.rs covers grep)");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let (ok, deny) = (root.join("ok"), root.join("deny"));
    std::fs::create_dir_all(&ok).unwrap();
    std::fs::create_dir_all(&deny).unwrap();
    std::fs::write(ok.join("a.txt"), "TOKEN=allowed\n").unwrap();
    std::fs::write(deny.join("s.txt"), "TOKEN=secret\n").unwrap();
    std::os::unix::fs::symlink(&deny, ok.join("link")).unwrap();
    // Strips file names from the output and follows symlinks out of `ok/`.
    let rc = root.join("ripgreprc");
    std::fs::write(&rc, "--no-filename\n--follow\n").unwrap();
    // Canary: the config really bites, so a clean result below means the
    // tool kept it out, not that this ripgrep ignores it.
    let raw = std::process::Command::new("rg")
        .args(["--line-number", "--no-heading", "--regexp=TOKEN", "--"])
        .arg(&ok)
        .env("RIPGREP_CONFIG_PATH", &rc)
        .output()
        .unwrap();
    let raw = String::from_utf8_lossy(&raw.stdout);
    assert!(
        raw.contains("1:TOKEN=secret"),
        "the fixture no longer bites: {raw}"
    );

    // SAFETY: this binary holds one test; nothing else reads the environment.
    unsafe { std::env::set_var("RIPGREP_CONFIG_PATH", &rc) };

    let tool = SearchTool::new()
        .with_root(ok.to_str().unwrap())
        .with_allowed_paths(vec![ok.to_str().unwrap().to_string()]);
    let result = tool
        .execute(serde_json::json!({"pattern": "TOKEN"}), ctx())
        .await
        .unwrap();
    assert_eq!(result.details["engine"], "rg", "the search ran grep");
    let Content::Text { text } = &result.content[0] else {
        panic!("expected text")
    };
    assert!(text.contains("TOKEN=allowed"), "{text}");
    assert!(
        !text.contains("TOKEN=secret"),
        "escaped allowed_paths: {text}"
    );
    // Every match keeps its `path:line:` prefix.
    let line = text.lines().find(|l| l.contains("TOKEN=allowed")).unwrap();
    assert!(
        line.contains(&format!("{}:1:", ok.join("a.txt").display())),
        "{line}"
    );
}
