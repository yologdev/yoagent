//! MCP (Model Context Protocol) JSON-RPC 2.0 types.

use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicU64, Ordering};

static REQUEST_ID_COUNTER: AtomicU64 = AtomicU64::new(1);

/// Generate a unique JSON-RPC request ID.
pub fn next_request_id() -> u64 {
    REQUEST_ID_COUNTER.fetch_add(1, Ordering::Relaxed)
}

// ---------------------------------------------------------------------------
// JSON-RPC 2.0
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonRpcRequest {
    pub jsonrpc: String,
    pub id: u64,
    pub method: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub params: Option<serde_json::Value>,
}

impl JsonRpcRequest {
    pub fn new(method: impl Into<String>, params: Option<serde_json::Value>) -> Self {
        Self {
            jsonrpc: "2.0".into(),
            id: next_request_id(),
            method: method.into(),
            params,
        }
    }
}

/// A JSON-RPC notification: no `id`, and no response is expected (or sent).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonRpcNotification {
    pub jsonrpc: String,
    pub method: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub params: Option<serde_json::Value>,
}

impl JsonRpcNotification {
    pub fn new(method: impl Into<String>, params: Option<serde_json::Value>) -> Self {
        Self {
            jsonrpc: "2.0".into(),
            method: method.into(),
            params,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonRpcResponse {
    pub jsonrpc: String,
    pub id: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<JsonRpcError>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonRpcError {
    pub code: i64,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<serde_json::Value>,
}

// ---------------------------------------------------------------------------
// MCP Protocol types
// ---------------------------------------------------------------------------

/// Client info sent during initialization.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClientInfo {
    pub name: String,
    pub version: String,
}

impl Default for ClientInfo {
    fn default() -> Self {
        Self {
            name: "yoagent".into(),
            version: env!("CARGO_PKG_VERSION").into(),
        }
    }
}

/// Server info received during initialization.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ServerInfo {
    pub name: String,
    #[serde(default)]
    pub version: String,
}

/// Server capabilities received during initialization.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ServerCapabilities {
    #[serde(default)]
    pub tools: Option<serde_json::Value>,
    #[serde(default)]
    pub resources: Option<serde_json::Value>,
    #[serde(default)]
    pub prompts: Option<serde_json::Value>,
}

/// Initialize result from the server.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InitializeResult {
    pub protocol_version: String,
    pub capabilities: ServerCapabilities,
    pub server_info: ServerInfo,
}

/// MCP tool as returned by tools/list.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct McpToolInfo {
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub input_schema: serde_json::Value,
}

/// tools/list result.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolsListResult {
    pub tools: Vec<McpToolInfo>,
}

/// Content item in a tool call result.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum McpContent {
    #[serde(rename = "text")]
    Text { text: String },
    #[serde(rename = "image")]
    Image {
        data: String,
        #[serde(rename = "mimeType")]
        mime_type: String,
    },
}

/// tools/call result.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct McpToolCallResult {
    /// Content kinds this crate does not model (`resource`, `resource_link`,
    /// `audio`, anything newer) arrive as [`McpContent::Text`] holding the
    /// block's JSON, rather than failing the whole call.
    #[serde(deserialize_with = "lenient_content")]
    pub content: Vec<McpContent>,
    #[serde(default)]
    pub is_error: bool,
}

/// Read each content block, keeping one this crate does not model as text
/// the model can use: a `resource`'s own text, or the block's JSON with any
/// large `data` / `blob` payload replaced by its size (base64 audio would
/// otherwise go into the context verbatim).
fn lenient_content<'de, D>(deserializer: D) -> Result<Vec<McpContent>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let blocks = Vec::<serde_json::Value>::deserialize(deserializer)?;
    Ok(blocks
        .into_iter()
        .map(
            |block| match serde_json::from_value::<McpContent>(block.clone()) {
                Ok(content) => content,
                Err(e) => {
                    let kind = block
                        .get("type")
                        .and_then(|t| t.as_str())
                        .unwrap_or("(none)")
                        .to_string();
                    warn_unmodelled(&kind, &e);
                    McpContent::Text {
                        text: unmodelled_text(block),
                    }
                }
            },
        )
        .collect())
}

/// Warn once per content type that had to be passed through as text.
fn warn_unmodelled(kind: &str, error: &serde_json::Error) {
    use std::collections::HashSet;
    use std::sync::{Mutex, OnceLock};
    static SEEN: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
    let first = SEEN
        .get_or_init(|| Mutex::new(HashSet::new()))
        .lock()
        .map(|mut seen| seen.insert(kind.to_string()))
        .unwrap_or(true);
    if first {
        tracing::warn!(
            "MCP content of type '{kind}' is not modelled ({error}); passing it to the model as text"
        );
    }
}

/// Payloads longer than this are replaced by a note of their size.
const MAX_INLINE_PAYLOAD: usize = 1024;

fn unmodelled_text(mut block: serde_json::Value) -> String {
    // A text resource: its own text is what the model wants.
    if let Some(resource) = block.get("resource") {
        if let Some(text) = resource.get("text").and_then(|t| t.as_str()) {
            let uri = resource.get("uri").and_then(|u| u.as_str()).unwrap_or("");
            return format!("[resource {uri}]\n{text}");
        }
    }
    shrink_payloads(&mut block);
    block.to_string()
}

fn shrink_payloads(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::Object(map) => {
            for (key, v) in map.iter_mut() {
                match v {
                    serde_json::Value::String(s)
                        if (key == "data" || key == "blob") && s.len() > MAX_INLINE_PAYLOAD =>
                    {
                        *v = serde_json::Value::String(format!(
                            "[{} bytes of base64 omitted]",
                            s.len()
                        ));
                    }
                    other => shrink_payloads(other),
                }
            }
        }
        serde_json::Value::Array(items) => items.iter_mut().for_each(shrink_payloads),
        _ => {}
    }
}

// ---------------------------------------------------------------------------
// MCP Error
// ---------------------------------------------------------------------------

#[derive(Debug, thiserror::Error)]
pub enum McpError {
    #[error("Transport error: {0}")]
    Transport(String),
    #[error("Protocol error: {0}")]
    Protocol(String),
    #[error("JSON-RPC error {code}: {message}")]
    JsonRpc { code: i64, message: String },
    #[error("Serialization error: {0}")]
    Serialization(#[from] serde_json::Error),
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    #[error("Connection closed")]
    ConnectionClosed,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_json_rpc_request_serialization() {
        let req = JsonRpcRequest {
            jsonrpc: "2.0".into(),
            id: 1,
            method: "initialize".into(),
            params: Some(serde_json::json!({"protocolVersion": "2024-11-05"})),
        };
        let json = serde_json::to_string(&req).unwrap();
        assert!(json.contains("\"jsonrpc\":\"2.0\""));
        assert!(json.contains("\"method\":\"initialize\""));

        let parsed: JsonRpcRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.id, 1);
        assert_eq!(parsed.method, "initialize");
    }

    #[test]
    fn test_json_rpc_response_deserialization() {
        let json = r#"{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2024-11-05","capabilities":{},"serverInfo":{"name":"test","version":"1.0"}}}"#;
        let resp: JsonRpcResponse = serde_json::from_str(json).unwrap();
        assert_eq!(resp.id, Some(1));
        assert!(resp.result.is_some());
        assert!(resp.error.is_none());
    }

    #[test]
    fn test_json_rpc_error_response() {
        let json =
            r#"{"jsonrpc":"2.0","id":1,"error":{"code":-32601,"message":"Method not found"}}"#;
        let resp: JsonRpcResponse = serde_json::from_str(json).unwrap();
        assert!(resp.error.is_some());
        let err = resp.error.unwrap();
        assert_eq!(err.code, -32601);
    }

    #[test]
    fn test_initialize_result_deserialization() {
        let json = r#"{"protocolVersion":"2024-11-05","capabilities":{"tools":{}},"serverInfo":{"name":"test-server","version":"0.1.0"}}"#;
        let result: InitializeResult = serde_json::from_str(json).unwrap();
        assert_eq!(result.server_info.name, "test-server");
        assert!(result.capabilities.tools.is_some());
    }

    #[test]
    fn test_mcp_tool_info_deserialization() {
        let json = r#"{"name":"read_file","description":"Read a file","inputSchema":{"type":"object","properties":{"path":{"type":"string"}}}}"#;
        let tool: McpToolInfo = serde_json::from_str(json).unwrap();
        assert_eq!(tool.name, "read_file");
        assert_eq!(tool.description.as_deref(), Some("Read a file"));
    }

    #[test]
    fn test_mcp_tool_call_result() {
        let json = r#"{"content":[{"type":"text","text":"file contents here"}],"isError":false}"#;
        let result: McpToolCallResult = serde_json::from_str(json).unwrap();
        assert_eq!(result.content.len(), 1);
        assert!(!result.is_error);
    }

    #[test]
    fn unknown_content_blocks_are_kept_as_json_text() {
        let json = r#"{"content":[
            {"type":"text","text":"hi"},
            {"type":"resource","resource":{"uri":"file:///a.txt","text":"body"}},
            {"type":"audio","data":"AAAA","mimeType":"audio/wav"}
        ]}"#;
        let result: McpToolCallResult = serde_json::from_str(json).unwrap();
        assert_eq!(result.content.len(), 3);
        assert!(matches!(&result.content[0], McpContent::Text { text } if text == "hi"));
        assert!(
            matches!(&result.content[1], McpContent::Text { text } if text == "[resource file:///a.txt]\nbody")
        );
        assert!(matches!(&result.content[2], McpContent::Text { text } if text.contains("audio")));
    }

    #[test]
    fn large_unmodelled_payloads_are_replaced_by_their_size() {
        let blob = "A".repeat(5000);
        let json = format!(
            r#"{{"content":[{{"type":"audio","data":"{blob}","mimeType":"audio/wav"}},{{"type":"resource","resource":{{"uri":"file:///b.bin","blob":"{blob}"}}}}]}}"#
        );
        let result: McpToolCallResult = serde_json::from_str(&json).unwrap();
        for content in &result.content {
            let McpContent::Text { text } = content else {
                panic!("expected text")
            };
            assert!(text.contains("[5000 bytes of base64 omitted]"), "{text}");
            assert!(text.len() < 500, "{} bytes kept", text.len());
        }
    }

    #[test]
    fn a_notification_has_no_id() {
        let json =
            serde_json::to_string(&JsonRpcNotification::new("notifications/initialized", None))
                .unwrap();
        assert_eq!(
            json,
            r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#
        );
    }

    #[test]
    fn test_unique_request_ids() {
        let id1 = next_request_id();
        let id2 = next_request_id();
        assert_ne!(id1, id2);
    }
}
