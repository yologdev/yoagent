//! A custom `McpTransport` and server notifications: one that reports them
//! moves an `McpToolSource`'s generation; one that does not says so.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use yoagent::mcp::types::{JsonRpcRequest, JsonRpcResponse};
use yoagent::mcp::{McpClient, McpError, McpToolSource, McpTransport, ServerNotifications};
use yoagent::*;

/// Offers `load`; calling it adds `extra` and, when the transport was given a
/// handle, records `notifications/tools/list_changed`.
#[derive(Default)]
struct Server {
    loaded: AtomicBool,
    notifications: StdMutex<Option<ServerNotifications>>,
    reports: bool,
}

#[async_trait::async_trait]
impl McpTransport for Server {
    async fn send(&self, request: JsonRpcRequest) -> Result<JsonRpcResponse, McpError> {
        let tool =
            |name: &str| serde_json::json!({ "name": name, "inputSchema": { "type": "object" } });
        let result = match request.method.as_str() {
            "tools/list" => {
                let mut tools = vec![tool("load")];
                if self.loaded.load(Ordering::SeqCst) {
                    tools.push(tool("extra"));
                }
                serde_json::json!({ "tools": tools })
            }
            "tools/call" => {
                self.loaded.store(true, Ordering::SeqCst);
                if let Some(n) = self.notifications.lock().unwrap().as_ref() {
                    n.record("notifications/tools/list_changed");
                }
                serde_json::json!({ "content": [{ "type": "text", "text": "loaded" }] })
            }
            other => panic!("unexpected {other}"),
        };
        Ok(JsonRpcResponse {
            jsonrpc: "2.0".into(),
            id: Some(request.id),
            result: Some(result),
            error: None,
        })
    }

    async fn close(&self) -> Result<(), McpError> {
        Ok(())
    }

    fn report_notifications(&mut self, to: ServerNotifications) -> bool {
        if !self.reports {
            return false;
        }
        *self.notifications.lock().unwrap() = Some(to);
        true
    }
}

/// The source's generation and tool names before and after `load` is called.
async fn around_load(reports: bool) -> (bool, [(u64, Vec<String>); 2]) {
    let client = McpClient::from_transport(Box::new(Server {
        reports,
        ..Server::default()
    }));
    let reported = client.reports_notifications();
    assert_eq!(
        client.exited(),
        None,
        "a custom transport has no exit watch"
    );
    let source = McpToolSource::new(client);
    let snapshot = |tools: Vec<Arc<dyn AgentTool>>| -> Vec<String> {
        tools.iter().map(|t| t.name().to_string()).collect()
    };
    let before = (source.generation(), snapshot(source.tools().await));
    source
        .client()
        .lock()
        .await
        .call_tool("load", serde_json::json!({}))
        .await
        .unwrap();
    let after = (source.generation(), snapshot(source.tools().await));
    (reported, [before, after])
}

#[tokio::test]
async fn a_custom_transport_that_reports_notifications_moves_the_generation() {
    let (reported, [before, after]) = around_load(true).await;
    assert!(reported);
    assert_eq!(before, (0, vec!["load".to_string()]));
    assert_eq!(after, (1, vec!["load".to_string(), "extra".to_string()]));
}

#[tokio::test]
async fn a_custom_transport_by_default_reports_nothing() {
    let (reported, [before, after]) = around_load(false).await;
    assert!(!reported);
    // The server changed its tools, but the source cannot know: generation 0,
    // so a run never asks it again mid-run.
    assert_eq!(before.0, 0);
    assert_eq!(after.0, 0);
}

/// A custom transport with an exit watch: once it says the server went, the
/// client says so and the source withdraws its tools (generation + 1).
#[tokio::test]
async fn a_custom_exit_watch_withdraws_the_tools() {
    use yoagent::mcp::ExitWatch;
    struct Gone(Server, Arc<AtomicBool>);
    #[async_trait::async_trait]
    impl McpTransport for Gone {
        async fn send(&self, request: JsonRpcRequest) -> Result<JsonRpcResponse, McpError> {
            self.0.send(request).await
        }
        async fn close(&self) -> Result<(), McpError> {
            Ok(())
        }
        fn exit_watch(&self) -> Option<ExitWatch> {
            let gone = self.1.clone();
            Some(ExitWatch::new(move || {
                gone.load(Ordering::SeqCst).then(|| "gone".to_string())
            }))
        }
    }
    let gone = Arc::new(AtomicBool::new(false));
    let client = McpClient::from_transport(Box::new(Gone(Server::default(), gone.clone())));
    assert_eq!(client.exited(), None);
    let source = McpToolSource::new(client);
    assert_eq!(source.tools().await.len(), 1);
    assert_eq!(source.generation(), 0);
    gone.store(true, Ordering::SeqCst);
    assert_eq!(source.generation(), 1);
    assert!(source.tools().await.is_empty());
    assert_eq!(
        source.client().lock().await.exited().as_deref(),
        Some("gone")
    );
}
