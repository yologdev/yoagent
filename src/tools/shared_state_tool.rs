//! Tool that exposes `SharedState` to sub-agents.
//!
//! Injected automatically by `SubAgentTool` when `.with_shared_state()` is used.
//! Provides get/set/list/remove actions against the shared key-value store.

use crate::shared_state::SharedState;
use crate::types::*;

/// A tool that lets an LLM read/write a [`SharedState`] store.
pub struct SharedStateTool {
    state: SharedState,
}

impl SharedStateTool {
    pub fn new(state: SharedState) -> Self {
        Self { state }
    }
}

#[async_trait::async_trait]
impl AgentTool for SharedStateTool {
    fn name(&self) -> &str {
        "shared_state"
    }

    fn label(&self) -> &str {
        "Shared State"
    }

    fn description(&self) -> &str {
        "Read and write named variables in a shared store. Variables persist across tool calls within this session."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "enum": ["get", "set", "list", "remove"],
                    "description": "Action to perform"
                },
                "key": {
                    "type": "string",
                    "description": "Variable name (required for get/set/remove)"
                },
                "value": {
                    "type": "string",
                    "description": "Value to store (required for set)"
                }
            },
            "required": ["action"]
        })
    }

    async fn execute(
        &self,
        params: serde_json::Value,
        _ctx: ToolContext,
    ) -> Result<ToolResult, ToolError> {
        let action = params
            .get("action")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ToolError::InvalidArgs("Missing required 'action' parameter".into()))?;

        match action {
            "get" => {
                let key = require_key(&params)?;
                match self.state.get(&key).await {
                    Some(value) => Ok(ToolResult {
                        content: vec![Content::Text { text: value }],
                        details: serde_json::json!({"action": "get", "key": key}),
                    }),
                    None => Err(ToolError::Failed(format!(
                        "Key '{}' not found in shared state",
                        key
                    ))),
                }
            }
            "set" => {
                let key = require_key(&params)?;
                let value = params
                    .get("value")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| {
                        ToolError::InvalidArgs("Missing required 'value' parameter for set".into())
                    })?
                    .to_string();

                let bytes = value.len();
                self.state
                    .set(&key, value)
                    .await
                    .map_err(|e| ToolError::Failed(e.to_string()))?;

                Ok(ToolResult {
                    content: vec![Content::Text {
                        text: format!("Stored '{}' ({} bytes)", key, bytes),
                    }],
                    details: serde_json::json!({"action": "set", "key": key, "bytes": bytes}),
                })
            }
            "list" => {
                let summary = self.state.summary().await;
                Ok(ToolResult {
                    content: vec![Content::Text { text: summary }],
                    details: serde_json::json!({"action": "list"}),
                })
            }
            "remove" => {
                let key = require_key(&params)?;
                let existed = self.state.remove(&key).await;
                let text = if existed {
                    format!("Removed '{}'", key)
                } else {
                    format!("Key '{}' not found", key)
                };
                Ok(ToolResult {
                    content: vec![Content::Text { text }],
                    details: serde_json::json!({"action": "remove", "key": key, "existed": existed}),
                })
            }
            other => Err(ToolError::InvalidArgs(format!(
                "Unknown action '{}'. Use get, set, list, or remove.",
                other
            ))),
        }
    }
}

fn require_key(params: &serde_json::Value) -> Result<String, ToolError> {
    params
        .get("key")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .ok_or_else(|| ToolError::InvalidArgs("Missing required 'key' parameter".into()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio_util::sync::CancellationToken;

    fn ctx() -> ToolContext {
        ToolContext {
            tool_call_id: "test".into(),
            tool_name: "shared_state".into(),
            cancel: CancellationToken::new(),
            on_update: None,
            on_progress: None,
        }
    }

    fn text_of(result: &ToolResult) -> &str {
        match &result.content[0] {
            Content::Text { text } => text,
            _ => panic!("expected Text content"),
        }
    }

    #[tokio::test]
    async fn test_set_and_get() {
        let state = SharedState::new();
        let tool = SharedStateTool::new(state);

        let result = tool
            .execute(
                serde_json::json!({"action": "set", "key": "x", "value": "hello"}),
                ctx(),
            )
            .await
            .unwrap();
        assert!(text_of(&result).contains("Stored"));

        let result = tool
            .execute(serde_json::json!({"action": "get", "key": "x"}), ctx())
            .await
            .unwrap();
        assert_eq!(text_of(&result), "hello");
    }

    #[tokio::test]
    async fn test_get_missing_key() {
        let tool = SharedStateTool::new(SharedState::new());
        let err = tool
            .execute(serde_json::json!({"action": "get", "key": "nope"}), ctx())
            .await;
        assert!(matches!(err, Err(ToolError::Failed(_))));
    }

    #[tokio::test]
    async fn test_list() {
        let state = SharedState::new();
        state.set("a", "1".into()).await.unwrap();
        let tool = SharedStateTool::new(state);

        let result = tool
            .execute(serde_json::json!({"action": "list"}), ctx())
            .await
            .unwrap();
        assert!(text_of(&result).contains("a"));
    }

    #[tokio::test]
    async fn test_remove() {
        let state = SharedState::new();
        state.set("k", "v".into()).await.unwrap();
        let tool = SharedStateTool::new(state);

        let result = tool
            .execute(serde_json::json!({"action": "remove", "key": "k"}), ctx())
            .await
            .unwrap();
        assert!(text_of(&result).contains("Removed"));

        let result = tool
            .execute(serde_json::json!({"action": "remove", "key": "k"}), ctx())
            .await
            .unwrap();
        assert!(text_of(&result).contains("not found"));
    }

    #[tokio::test]
    async fn test_invalid_action() {
        let tool = SharedStateTool::new(SharedState::new());
        let err = tool
            .execute(serde_json::json!({"action": "explode"}), ctx())
            .await;
        assert!(matches!(err, Err(ToolError::InvalidArgs(_))));
    }

    #[tokio::test]
    async fn test_missing_params() {
        let tool = SharedStateTool::new(SharedState::new());

        // Missing action
        let err = tool.execute(serde_json::json!({}), ctx()).await;
        assert!(matches!(err, Err(ToolError::InvalidArgs(_))));

        // Missing key for get
        let err = tool
            .execute(serde_json::json!({"action": "get"}), ctx())
            .await;
        assert!(matches!(err, Err(ToolError::InvalidArgs(_))));

        // Missing value for set
        let err = tool
            .execute(serde_json::json!({"action": "set", "key": "k"}), ctx())
            .await;
        assert!(matches!(err, Err(ToolError::InvalidArgs(_))));
    }
}
