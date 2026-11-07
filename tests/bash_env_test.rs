//! `BashTool`'s environment allow-list with a variable that is not valid
//! Unicode in the process environment. Its own binary: the variable would
//! make `std::env::vars()` panic in any test sharing the process.
#![cfg(all(unix, feature = "native"))]

use std::os::unix::ffi::OsStrExt;
use yoagent::tools::BashTool;
use yoagent::*;

#[tokio::test]
async fn the_allow_list_survives_a_non_unicode_variable() {
    let bad = std::ffi::OsStr::from_bytes(b"\xff\xfe-not-utf8");
    // SAFETY: set before any thread of this test binary reads the
    // environment; this binary holds only this test.
    unsafe { std::env::set_var("YOAGENT_TEST_NOT_UTF8", bad) };
    unsafe { std::env::set_var("YOAGENT_TEST_KEPT", "kept") };
    let tool = BashTool::new().with_env_allowlist(vec!["YOAGENT_TEST_KEPT".into()]);
    let ctx = ToolContext::new("t", "bash");
    let result = tool
        .execute(
            serde_json::json!({ "command": "echo \"$YOAGENT_TEST_KEPT\"" }),
            ctx,
        )
        .await
        .expect("a non-Unicode variable must not fail the call");
    let Content::Text { text } = &result.content[0] else {
        panic!("expected text")
    };
    assert!(text.contains("kept"), "{text}");
}
