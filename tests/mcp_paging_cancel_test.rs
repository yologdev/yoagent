//! MCP `tools/list` follows `nextCursor`, and a call dropped before its
//! answer (cancelled, or timed out) sends `notifications/cancelled`.

use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;
use yoagent::mcp::types::{JsonRpcNotification, JsonRpcRequest, JsonRpcResponse};
use yoagent::mcp::{McpClient, McpError, McpTransport};

/// Pages its tools by cursor (`cycle`: always answers with the same cursor);
/// `tools/call` never answers. Records every request and notification.
#[derive(Default)]
struct Server {
    cycle: bool,
    requests: Arc<StdMutex<Vec<JsonRpcRequest>>>,
    notifications: Arc<StdMutex<Vec<JsonRpcNotification>>>,
}

fn tool(name: &str) -> serde_json::Value {
    serde_json::json!({ "name": name, "inputSchema": { "type": "object" } })
}

#[async_trait::async_trait]
impl McpTransport for Server {
    async fn send(&self, request: JsonRpcRequest) -> Result<JsonRpcResponse, McpError> {
        self.requests.lock().unwrap().push(request.clone());
        let cursor = request
            .params
            .as_ref()
            .and_then(|p| p.get("cursor"))
            .and_then(|c| c.as_str())
            .map(str::to_owned);
        let result = match request.method.as_str() {
            "tools/list" if self.cycle => {
                serde_json::json!({ "tools": [tool("again")], "nextCursor": "same" })
            }
            "tools/list" => match cursor.as_deref() {
                None => serde_json::json!({ "tools": [tool("a")], "nextCursor": "page-2" }),
                Some("page-2") => {
                    serde_json::json!({ "tools": [tool("b")], "nextCursor": "page-3" })
                }
                Some("page-3") => serde_json::json!({ "tools": [tool("c")] }),
                Some(other) => panic!("unknown cursor {other}"),
            },
            "tools/call"
                if request.params.as_ref().and_then(|p| p.get("name"))
                    == Some(&serde_json::json!("fast")) =>
            {
                serde_json::json!({ "content": [{ "type": "text", "text": "done" }] })
            }
            "tools/call" => std::future::pending().await,
            other => panic!("unexpected {other}"),
        };
        Ok(JsonRpcResponse {
            jsonrpc: "2.0".into(),
            id: Some(request.id),
            result: Some(result),
            error: None,
        })
    }

    async fn notify(&self, notification: JsonRpcNotification) -> Result<(), McpError> {
        self.notifications.lock().unwrap().push(notification);
        Ok(())
    }

    async fn close(&self) -> Result<(), McpError> {
        Ok(())
    }
}

#[tokio::test]
async fn tools_list_follows_every_page() {
    let server = Server::default();
    let requests = server.requests.clone();
    let client = McpClient::from_transport(Box::new(server));
    let names: Vec<String> = client
        .list_tools()
        .await
        .unwrap()
        .into_iter()
        .map(|t| t.name)
        .collect();
    assert_eq!(names, ["a", "b", "c"]);
    let cursors: Vec<Option<String>> = requests
        .lock()
        .unwrap()
        .iter()
        .map(|r| {
            r.params
                .as_ref()
                .and_then(|p| p.get("cursor"))
                .and_then(|c| c.as_str())
                .map(str::to_owned)
        })
        .collect();
    assert_eq!(
        cursors,
        [None, Some("page-2".into()), Some("page-3".into())]
    );
}

#[tokio::test]
async fn a_repeated_cursor_is_an_error_not_a_loop() {
    let client = McpClient::from_transport(Box::new(Server {
        cycle: true,
        ..Server::default()
    }));
    let err = client.list_tools().await.unwrap_err();
    assert!(err.to_string().contains("cursor"), "{err}");
}

#[tokio::test]
async fn a_dropped_call_tells_the_server_it_was_cancelled() {
    let server = Server::default();
    let (requests, notifications) = (server.requests.clone(), server.notifications.clone());
    let client = McpClient::from_transport(Box::new(server));
    let call = client.call_tool("slow", serde_json::json!({}));
    assert!(tokio::time::timeout(Duration::from_millis(50), call)
        .await
        .is_err());
    let id = requests
        .lock()
        .unwrap()
        .iter()
        .find(|r| r.method == "tools/call")
        .unwrap()
        .id;
    // The notice is sent on a spawned task.
    let mut sent = Vec::new();
    for _ in 0..100 {
        sent = notifications.lock().unwrap().clone();
        if !sent.is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(sent.len(), 1, "{sent:?}");
    assert_eq!(sent[0].method, "notifications/cancelled");
    assert_eq!(sent[0].params.as_ref().unwrap()["requestId"], id);
}

#[tokio::test]
async fn an_answered_call_sends_no_cancellation() {
    let server = Server::default();
    let notifications = server.notifications.clone();
    let client = McpClient::from_transport(Box::new(server));
    client
        .call_tool("fast", serde_json::json!({}))
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(notifications.lock().unwrap().is_empty());
}
