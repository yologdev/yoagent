//! `StdioTransport` against scripted servers that behave like real ones:
//! interleaved notifications and server requests, a stale response, a noisy
//! (and not always UTF-8) stderr, a content kind this crate does not model,
//! late answers, servers that die. Unix only (the servers are bash scripts).
#![cfg(all(feature = "native", unix))]

use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;
use yoagent::mcp::{McpClient, McpToolAdapter};
use yoagent::types::*;

/// Shared by the scripts: the numeric id of a JSON-RPC line.
const ID_OF: &str = r#"id_of() { printf '%s' "$1" | sed -n 's/.*"id":\([0-9][0-9]*\).*/\1/p'; }"#;

const SERVER: &str = r#"
# initialize: a banner line, a notification, a ping and a sampling request
# arrive before the answer.
read -r line; id=$(id_of "$line")
echo 'fake-server v1 starting (a banner on stdout)'
echo '{"jsonrpc":"2.0","method":"notifications/message","params":{"level":"info","data":"starting"}}'
echo '{"jsonrpc":"2.0","id":"srv-1","method":"ping"}'
read -r pong
case "$pong" in *'"id":"srv-1"'*'"result"'*) ;; *) echo "bad ping reply: $pong" >&2; exit 1;; esac
echo '{"jsonrpc":"2.0","id":"srv-2","method":"sampling/createMessage","params":{}}'
read -r declined
case "$declined" in *'"error"'*'-32601'*'"id":"srv-2"'*) ;; *) echo "sampling not declined: $declined" >&2; exit 1;; esac
echo "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":{\"protocolVersion\":\"2024-11-05\",\"capabilities\":{\"tools\":{}},\"serverInfo\":{\"name\":\"fake\",\"version\":\"1\"}}}"

# notifications/initialized: no id, and no reply is sent.
read -r note
case "$note" in *'"id"'*) echo "the notification carried an id: $note" >&2; exit 1;; esac

# tools/list: a stale response for another id comes first.
read -r line; id=$(id_of "$line")
echo '{"jsonrpc":"2.0","id":999999,"result":{"tools":[]}}'
echo "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":{\"tools\":[{\"name\":\"fetch\",\"description\":\"Fetch\",\"inputSchema\":{\"type\":\"object\"}}]}}"

# Call 1: stderr gets a non-UTF-8 byte, then 200 KB (more than a pipe
# holds), then the answer: a text block and a resource block.
read -r line; id=$(id_of "$line")
printf '\377 not utf-8\n' >&2
head -c 200000 /dev/zero | tr '\0' x >&2
echo "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":{\"content\":[{\"type\":\"text\",\"text\":\"fetched\"},{\"type\":\"resource\",\"resource\":{\"uri\":\"file:///a.txt\",\"text\":\"body\"}}]}}"

# Call 2: not answered in time (the client times out)...
read -r line; late=$(id_of "$line")
# Call 3: ...its answer arrives late, ahead of call 3's own.
read -r line; id=$(id_of "$line")
echo "{\"jsonrpc\":\"2.0\",\"id\":$late,\"result\":{\"content\":[{\"type\":\"text\",\"text\":\"too late\"}]}}"
echo "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":{\"content\":[{\"type\":\"text\",\"text\":\"third\"}]}}"

# Every later call: never answered.
while read -r line; do :; done
"#;

fn ctx() -> ToolContext {
    ToolContext::new("call-1", "fetch")
}

fn text_of(result: &ToolResult, i: usize) -> &str {
    match &result.content[i] {
        Content::Text { text } => text,
        other => panic!("expected text, got {other:?}"),
    }
}

async fn connect(script: &str) -> Result<McpClient, yoagent::mcp::McpError> {
    let script = format!("{ID_OF}\n{script}");
    McpClient::connect_stdio("bash", &["-c", &script], None).await
}

#[tokio::test]
async fn a_realistic_stdio_server_works_end_to_end() {
    let run = async {
        let client = connect(SERVER)
            .await
            .expect("the handshake completes despite the banner and server requests");
        assert!(client.call_timeout().is_some(), "stdio calls are bounded");
        let client = Arc::new(Mutex::new(client));
        let tools = McpToolAdapter::from_client(client.clone())
            .await
            .expect("tools/list past the stale response");
        assert_eq!(tools.len(), 1);
        let tool = tools.into_iter().next().unwrap();

        // Call 1.
        let result = tool
            .execute(serde_json::json!({"url": "x"}), ctx())
            .await
            .expect("the call completes despite a non-UTF-8, flooded stderr");
        assert_eq!(text_of(&result, 0), "fetched");
        assert_eq!(text_of(&result, 1), "[resource file:///a.txt]\nbody");

        // Call 2 times out...
        let short = tool.with_call_timeout(Some(Duration::from_millis(300)));
        let err = short
            .execute(serde_json::json!({}), ctx())
            .await
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("no answer within 300ms") && err.contains("may still carry it out"),
            "{err}"
        );

        // ...and call 3 gets its own answer, not call 2's late one.
        let tool = short.with_call_timeout(Some(Duration::from_secs(10)));
        let result = tool.execute(serde_json::json!({}), ctx()).await.unwrap();
        assert_eq!(text_of(&result, 0), "third");

        // A call the run cancels ends at once.
        let cancel = CancellationToken::new();
        let trigger = cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(100)).await;
            trigger.cancel();
        });
        let err = tool
            .with_call_timeout(None)
            .execute(serde_json::json!({}), ctx().with_cancel(cancel))
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::Cancelled), "{err:?}");
    };
    tokio::time::timeout(Duration::from_secs(20), run)
        .await
        .expect("nothing hangs");
}

/// A server that dies at startup explains itself on stderr; the error says
/// so, with its exit status, instead of a bare "Connection closed".
#[tokio::test]
async fn a_server_that_dies_reports_its_stderr() {
    let err = connect(
        r#"read -r line
echo "error: OPENAI_API_KEY is not set" >&2
exit 3"#,
    )
    .await
    .err()
    .expect("the connect fails")
    .to_string();
    assert!(err.contains("Connection closed"), "{err}");
    assert!(err.contains("exit status: 3"), "{err}");
    assert!(err.contains("OPENAI_API_KEY is not set"), "{err}");
}

/// Dropping the client kills the server, even one that ignores EOF on its
/// stdin.
#[tokio::test]
async fn dropping_the_client_kills_the_server() {
    let tmp = tempfile::TempDir::new().unwrap();
    let pid_file = tmp.path().join("pid");
    let script = format!(
        r#"echo $$ > '{}'
read -r line; id=$(id_of "$line")
echo "{{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":{{\"protocolVersion\":\"2024-11-05\",\"capabilities\":{{}},\"serverInfo\":{{\"name\":\"stubborn\",\"version\":\"1\"}}}}}}"
read -r note
trap '' HUP TERM
while :; do sleep 1; done"#,
        pid_file.display()
    );
    let client = connect(&script).await.expect("connects");
    let pid = std::fs::read_to_string(&pid_file)
        .unwrap()
        .trim()
        .to_string();
    let alive = || {
        std::process::Command::new("kill")
            .args(["-0", &pid])
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    };
    assert!(alive(), "the server runs while the client lives");
    drop(client);
    for _ in 0..50 {
        if !alive() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("the server outlived its client");
}
