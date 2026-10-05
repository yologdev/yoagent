//! `StdioTransport` against a scripted server that behaves like real ones:
//! interleaved notifications and server requests, a stale response, a noisy
//! stderr, a content kind this crate does not model, and a call it never
//! answers. Unix only (the server is a bash script).
#![cfg(all(feature = "native", unix))]

use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;
use yoagent::mcp::{McpClient, McpToolAdapter};
use yoagent::types::*;

const SERVER: &str = r#"
id_of() { printf '%s' "$1" | sed -n 's/.*"id":\([0-9][0-9]*\).*/\1/p'; }

# initialize: a notification and a ping arrive before the answer.
read -r line; id=$(id_of "$line")
echo '{"jsonrpc":"2.0","method":"notifications/message","params":{"level":"info","data":"starting"}}'
echo '{"jsonrpc":"2.0","id":"srv-1","method":"ping"}'
read -r pong
case "$pong" in *'"id":"srv-1"'*'"result"'*) ;; *) echo "bad ping reply: $pong" >&2; exit 1;; esac
echo "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":{\"protocolVersion\":\"2024-11-05\",\"capabilities\":{\"tools\":{}},\"serverInfo\":{\"name\":\"fake\",\"version\":\"1\"}}}"

# notifications/initialized: no id, and no reply is sent.
read -r note
case "$note" in *'"id"'*) echo "the notification carried an id: $note" >&2; exit 1;; esac

# tools/list: a stale response for another id comes first.
read -r line; id=$(id_of "$line")
echo '{"jsonrpc":"2.0","id":999999,"result":{"tools":[]}}'
echo "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":{\"tools\":[{\"name\":\"fetch\",\"description\":\"Fetch\",\"inputSchema\":{\"type\":\"object\"}}]}}"

# tools/call: 200 KB of stderr first (more than a pipe holds), then a text
# block and a resource block.
read -r line; id=$(id_of "$line")
head -c 200000 /dev/zero | tr '\0' x >&2
echo "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":{\"content\":[{\"type\":\"text\",\"text\":\"fetched\"},{\"type\":\"resource\",\"resource\":{\"uri\":\"file:///a.txt\",\"text\":\"body\"}}]}}"

# Every later call: never answered.
while read -r line; do :; done
"#;

fn ctx() -> ToolContext {
    ToolContext::new("call-1", "fetch")
}

#[tokio::test]
async fn a_realistic_stdio_server_works_end_to_end() {
    let run = async {
        let client = McpClient::connect_stdio("bash", &["-c", SERVER], None)
            .await
            .expect("the handshake completes (initialized is not awaited)");
        let client = Arc::new(Mutex::new(client));
        let tools = McpToolAdapter::from_client(client.clone())
            .await
            .expect("tools/list past the stale response");
        assert_eq!(tools.len(), 1);
        let tool = tools.into_iter().next().unwrap();

        let result = tool
            .execute(serde_json::json!({"url": "x"}), ctx())
            .await
            .expect("the call completes despite the stderr flood");
        assert!(matches!(&result.content[0], Content::Text { text } if text == "fetched"));
        assert!(
            matches!(&result.content[1], Content::Text { text } if text.contains("file:///a.txt")),
            "an unmodelled block arrives as JSON text: {:?}",
            result.content[1]
        );

        // The server never answers again: a timeout ends the call...
        let tool = tool.with_call_timeout(Some(Duration::from_millis(300)));
        let err = tool
            .execute(serde_json::json!({}), ctx())
            .await
            .unwrap_err();
        assert!(err.to_string().contains("timed out"), "{err}");

        // ...and so does the run's cancellation.
        let cancel = CancellationToken::new();
        let trigger = cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(100)).await;
            trigger.cancel();
        });
        let tool = tool.with_call_timeout(None);
        let err = tool
            .execute(serde_json::json!({}), ctx().with_cancel(cancel))
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::Cancelled), "{err:?}");
    };
    tokio::time::timeout(Duration::from_secs(20), run)
        .await
        .expect("nothing hangs");
}
