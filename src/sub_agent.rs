//! Sub-agent tool — delegates tasks to a child agent loop.
//!
//! The `SubAgentTool` implements `AgentTool` and internally runs `agent_loop()`
//! with its own system prompt, tools, and provider. The parent LLM invokes it
//! like any other tool, passing a natural-language `task` string.
//!
//! # Design
//!
//! - **Context isolation**: each invocation starts a fresh conversation
//! - **Depth limiting**: sub-agents are not given other SubAgentTools (static, no runtime counter)
//! - **Cancellation propagation**: the parent's cancel token is forwarded
//! - **Event forwarding**: sub-agent events stream to the parent via `on_update`
//!
//! # Example
//!
//! ```rust,no_run
//! use yoagent::sub_agent::SubAgentTool;
//! use yoagent::provider::AnthropicProvider;
//! use std::sync::Arc;
//!
//! let researcher = SubAgentTool::new("researcher", Arc::new(AnthropicProvider))
//!     .with_description("Searches codebases and documents")
//!     .with_system_prompt("You are a research assistant.")
//!     .with_model("claude-sonnet-4-20250514")
//!     .with_api_key("sk-...");
//! ```

use crate::agent_loop::{agent_loop, AgentLoopConfig};
use crate::context::ExecutionLimits;
use crate::provider::StreamProvider;
use crate::types::*;
use std::sync::Arc;
use tokio::sync::mpsc;

/// Default max turns for sub-agents (prevents runaway execution).
const DEFAULT_MAX_TURNS: usize = 10;

/// A tool that delegates work to a child agent loop.
///
/// When the parent LLM calls this tool, it spawns a fresh `agent_loop()` with
/// its own system prompt, tools, and provider. The sub-agent runs to completion
/// and its final text output is returned as the tool result.
pub struct SubAgentTool {
    tool_name: String,
    tool_description: String,
    system_prompt: String,
    model: String,
    api_key: String,
    provider: Arc<dyn StreamProvider>,
    tools: Vec<Arc<dyn AgentTool>>,
    thinking_level: ThinkingLevel,
    max_tokens: Option<u32>,
    cache_config: CacheConfig,
    tool_execution: ToolExecutionStrategy,
    retry_config: crate::retry::RetryConfig,
    max_turns: usize,
}

impl SubAgentTool {
    /// Create a new sub-agent tool with a name and provider.
    pub fn new(name: impl Into<String>, provider: Arc<dyn StreamProvider>) -> Self {
        let name = name.into();
        Self {
            tool_description: format!("Delegate a task to the '{}' sub-agent", name),
            tool_name: name,
            system_prompt: String::new(),
            model: String::new(),
            api_key: String::new(),
            provider,
            tools: Vec::new(),
            thinking_level: ThinkingLevel::Off,
            max_tokens: None,
            cache_config: CacheConfig::default(),
            tool_execution: ToolExecutionStrategy::default(),
            retry_config: crate::retry::RetryConfig::default(),
            max_turns: DEFAULT_MAX_TURNS,
        }
    }

    pub fn with_description(mut self, desc: impl Into<String>) -> Self {
        self.tool_description = desc.into();
        self
    }

    pub fn with_system_prompt(mut self, prompt: impl Into<String>) -> Self {
        self.system_prompt = prompt.into();
        self
    }

    pub fn with_model(mut self, model: impl Into<String>) -> Self {
        self.model = model.into();
        self
    }

    pub fn with_api_key(mut self, key: impl Into<String>) -> Self {
        self.api_key = key.into();
        self
    }

    pub fn with_tools(mut self, tools: Vec<Arc<dyn AgentTool>>) -> Self {
        self.tools = tools;
        self
    }

    pub fn with_thinking(mut self, level: ThinkingLevel) -> Self {
        self.thinking_level = level;
        self
    }

    pub fn with_max_tokens(mut self, max: u32) -> Self {
        self.max_tokens = Some(max);
        self
    }

    pub fn with_cache_config(mut self, config: CacheConfig) -> Self {
        self.cache_config = config;
        self
    }

    pub fn with_tool_execution(mut self, strategy: ToolExecutionStrategy) -> Self {
        self.tool_execution = strategy;
        self
    }

    pub fn with_retry_config(mut self, config: crate::retry::RetryConfig) -> Self {
        self.retry_config = config;
        self
    }

    pub fn with_max_turns(mut self, max: usize) -> Self {
        self.max_turns = max;
        self
    }
}

/// Thin adapter: wraps `Arc<dyn AgentTool>` so it can be placed in a
/// `Vec<Box<dyn AgentTool>>` (required by `AgentContext`).
struct ArcToolWrapper(Arc<dyn AgentTool>);

#[async_trait::async_trait]
impl AgentTool for ArcToolWrapper {
    fn name(&self) -> &str {
        self.0.name()
    }
    fn label(&self) -> &str {
        self.0.label()
    }
    fn description(&self) -> &str {
        self.0.description()
    }
    fn parameters_schema(&self) -> serde_json::Value {
        self.0.parameters_schema()
    }
    async fn execute(
        &self,
        tool_call_id: &str,
        params: serde_json::Value,
        cancel: tokio_util::sync::CancellationToken,
        on_update: Option<ToolUpdateFn>,
    ) -> Result<ToolResult, ToolError> {
        self.0
            .execute(tool_call_id, params, cancel, on_update)
            .await
    }
}

#[async_trait::async_trait]
impl AgentTool for SubAgentTool {
    fn name(&self) -> &str {
        &self.tool_name
    }

    fn label(&self) -> &str {
        &self.tool_name
    }

    fn description(&self) -> &str {
        &self.tool_description
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "task": {
                    "type": "string",
                    "description": "The task to delegate to this sub-agent"
                }
            },
            "required": ["task"]
        })
    }

    async fn execute(
        &self,
        _tool_call_id: &str,
        params: serde_json::Value,
        cancel: tokio_util::sync::CancellationToken,
        on_update: Option<ToolUpdateFn>,
    ) -> Result<ToolResult, ToolError> {
        // Extract the task parameter
        let task = params
            .get("task")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ToolError::InvalidArgs("Missing required 'task' parameter".into()))?
            .to_string();

        // Build tool list from Arc wrappers
        let tools: Vec<Box<dyn AgentTool>> = self
            .tools
            .iter()
            .map(|t| Box::new(ArcToolWrapper(Arc::clone(t))) as Box<dyn AgentTool>)
            .collect();

        // Fresh context for the sub-agent
        let mut context = AgentContext {
            system_prompt: self.system_prompt.clone(),
            messages: Vec::new(),
            tools,
        };

        // Config referencing the Arc'd provider
        let config = AgentLoopConfig {
            provider: &*self.provider,
            model: self.model.clone(),
            api_key: self.api_key.clone(),
            thinking_level: self.thinking_level,
            max_tokens: self.max_tokens,
            temperature: None,
            convert_to_llm: None,
            transform_context: None,
            get_steering_messages: None,
            get_follow_up_messages: None,
            context_config: None,
            execution_limits: Some(ExecutionLimits {
                max_turns: self.max_turns,
                // Generous token/duration limits — turn limit is the primary guard
                max_total_tokens: 1_000_000,
                max_duration: std::time::Duration::from_secs(300),
            }),
            cache_config: self.cache_config.clone(),
            tool_execution: self.tool_execution.clone(),
            retry_config: self.retry_config.clone(),
            before_turn: None,
            after_turn: None,
            on_error: None,
        };

        // Channel for sub-agent events
        let (tx, mut rx) = mpsc::unbounded_channel();

        // Forward sub-agent events to parent via on_update callback
        let forward_handle = if let Some(on_update) = on_update {
            let tool_name = self.tool_name.clone();
            Some(tokio::spawn(async move {
                while let Some(event) = rx.recv().await {
                    // Convert interesting events to ToolResult updates for the parent
                    let update_text = match &event {
                        AgentEvent::MessageUpdate {
                            delta: StreamDelta::Text { delta },
                            ..
                        } => Some(delta.clone()),
                        AgentEvent::ToolExecutionStart { tool_name, .. } => {
                            Some(format!("[sub-agent calling tool: {}]", tool_name))
                        }
                        _ => None,
                    };

                    if let Some(text) = update_text {
                        on_update(ToolResult {
                            content: vec![Content::Text { text }],
                            details: serde_json::json!({ "sub_agent": tool_name }),
                        });
                    }
                }
            }))
        } else {
            None
        };

        // Run the sub-agent loop
        let prompt = AgentMessage::Llm(Message::user(task));
        let new_messages = agent_loop(vec![prompt], &mut context, &config, tx, cancel).await;

        // Wait for event forwarding to complete
        if let Some(handle) = forward_handle {
            let _ = handle.await;
        }

        // Extract final assistant text from the returned messages
        let result_text = extract_final_text(&new_messages);

        // Include full sub-agent conversation in details for debugging
        let details = serde_json::json!({
            "sub_agent": self.tool_name,
            "turns": new_messages.len(),
        });

        Ok(ToolResult {
            content: vec![Content::Text { text: result_text }],
            details,
        })
    }
}

/// Extract the final assistant text from agent messages.
/// Collects text from the last assistant message, or returns a fallback.
fn extract_final_text(messages: &[AgentMessage]) -> String {
    for msg in messages.iter().rev() {
        if let AgentMessage::Llm(Message::Assistant { content, .. }) = msg {
            let texts: Vec<&str> = content
                .iter()
                .filter_map(|c| match c {
                    Content::Text { text } => Some(text.as_str()),
                    _ => None,
                })
                .collect();
            if !texts.is_empty() {
                return texts.join("\n");
            }
        }
    }
    "(sub-agent produced no text output)".to_string()
}
