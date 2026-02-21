//! Stateful Agent struct — wraps the agent loop with state management,
//! steering/follow-up queues, and abort support.

use crate::agent_loop::{
    agent_loop, agent_loop_continue, AfterTurnFn, AgentLoopConfig, BeforeTurnFn, OnErrorFn,
};
use crate::context::{ContextConfig, ExecutionLimits};
use crate::mcp::{McpClient, McpError, McpToolAdapter};
use crate::provider::StreamProvider;
use crate::types::*;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

/// Queue mode for steering and follow-up messages
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueueMode {
    /// Deliver one message per turn
    OneAtATime,
    /// Deliver all queued messages at once
    All,
}

/// The main Agent. Owns state, tools, and provider.
pub struct Agent {
    // State
    pub system_prompt: String,
    pub model: String,
    pub api_key: String,
    pub thinking_level: ThinkingLevel,
    pub max_tokens: Option<u32>,
    pub temperature: Option<f32>,
    messages: Vec<AgentMessage>,
    tools: Vec<Box<dyn AgentTool>>,
    provider: Box<dyn StreamProvider>,

    // Queues (shared with the loop via Arc<Mutex>)
    steering_queue: Arc<Mutex<Vec<AgentMessage>>>,
    follow_up_queue: Arc<Mutex<Vec<AgentMessage>>>,
    steering_mode: QueueMode,
    follow_up_mode: QueueMode,

    // Context, limits & caching
    pub context_config: Option<ContextConfig>,
    pub execution_limits: Option<ExecutionLimits>,
    pub cache_config: CacheConfig,
    pub tool_execution: ToolExecutionStrategy,
    pub retry_config: crate::retry::RetryConfig,

    // Lifecycle callbacks
    before_turn: Option<BeforeTurnFn>,
    after_turn: Option<AfterTurnFn>,
    on_error: Option<OnErrorFn>,

    // Control
    cancel: Option<CancellationToken>,
    is_streaming: bool,
}

impl Agent {
    pub fn new(provider: impl StreamProvider + 'static) -> Self {
        Self {
            system_prompt: String::new(),
            model: String::new(),
            api_key: String::new(),
            thinking_level: ThinkingLevel::Off,
            max_tokens: None,
            temperature: None,
            messages: Vec::new(),
            tools: Vec::new(),
            provider: Box::new(provider),
            steering_queue: Arc::new(Mutex::new(Vec::new())),
            follow_up_queue: Arc::new(Mutex::new(Vec::new())),
            steering_mode: QueueMode::OneAtATime,
            follow_up_mode: QueueMode::OneAtATime,
            context_config: Some(ContextConfig::default()),
            execution_limits: Some(ExecutionLimits::default()),
            cache_config: CacheConfig::default(),
            tool_execution: ToolExecutionStrategy::default(),
            retry_config: crate::retry::RetryConfig::default(),
            before_turn: None,
            after_turn: None,
            on_error: None,
            cancel: None,
            is_streaming: false,
        }
    }

    // -- Builder-style setters --

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

    pub fn with_thinking(mut self, level: ThinkingLevel) -> Self {
        self.thinking_level = level;
        self
    }

    pub fn with_tools(mut self, tools: Vec<Box<dyn AgentTool>>) -> Self {
        self.tools = tools;
        self
    }

    pub fn with_max_tokens(mut self, max: u32) -> Self {
        self.max_tokens = Some(max);
        self
    }

    pub fn with_context_config(mut self, config: ContextConfig) -> Self {
        self.context_config = Some(config);
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

    /// Load skills and append their index to the system prompt.
    ///
    /// The skills index is appended as XML per the [AgentSkills standard](https://agentskills.io).
    /// The agent can then read individual SKILL.md files using the `read_file` tool
    /// when it decides a skill is relevant.
    pub fn with_skills(mut self, skills: crate::skills::SkillSet) -> Self {
        let prompt_fragment = skills.format_for_prompt();
        if !prompt_fragment.is_empty() {
            if self.system_prompt.is_empty() {
                self.system_prompt = prompt_fragment;
            } else {
                self.system_prompt = format!("{}\n\n{}", self.system_prompt, prompt_fragment);
            }
        }
        self
    }

    pub fn with_execution_limits(mut self, limits: ExecutionLimits) -> Self {
        self.execution_limits = Some(limits);
        self
    }

    pub fn with_messages(mut self, msgs: Vec<AgentMessage>) -> Self {
        self.messages = msgs;
        self
    }

    pub fn on_before_turn(
        mut self,
        f: impl Fn(&[AgentMessage], usize) -> bool + Send + Sync + 'static,
    ) -> Self {
        self.before_turn = Some(Arc::new(f));
        self
    }

    pub fn on_after_turn(
        mut self,
        f: impl Fn(&[AgentMessage], &Usage) + Send + Sync + 'static,
    ) -> Self {
        self.after_turn = Some(Arc::new(f));
        self
    }

    pub fn on_error(mut self, f: impl Fn(&str) + Send + Sync + 'static) -> Self {
        self.on_error = Some(Arc::new(f));
        self
    }

    /// Add a sub-agent tool. The sub-agent runs its own `agent_loop()` when invoked.
    pub fn with_sub_agent(mut self, sub: crate::sub_agent::SubAgentTool) -> Self {
        self.tools.push(Box::new(sub));
        self
    }

    /// Disable automatic context compaction
    pub fn without_context_management(mut self) -> Self {
        self.context_config = None;
        self.execution_limits = None;
        self
    }

    // -- MCP integration --

    /// Connect to an MCP server via stdio and add its tools to the agent.
    pub async fn with_mcp_server_stdio(
        mut self,
        command: &str,
        args: &[&str],
        env: Option<HashMap<String, String>>,
    ) -> Result<Self, McpError> {
        let client = McpClient::connect_stdio(command, args, env).await?;
        let client = Arc::new(tokio::sync::Mutex::new(client));
        let adapters = McpToolAdapter::from_client(client).await?;
        for adapter in adapters {
            self.tools.push(Box::new(adapter));
        }
        Ok(self)
    }

    /// Connect to an MCP server via HTTP and add its tools to the agent.
    pub async fn with_mcp_server_http(mut self, url: &str) -> Result<Self, McpError> {
        let client = McpClient::connect_http(url).await?;
        let client = Arc::new(tokio::sync::Mutex::new(client));
        let adapters = McpToolAdapter::from_client(client).await?;
        for adapter in adapters {
            self.tools.push(Box::new(adapter));
        }
        Ok(self)
    }

    // -- State access --

    pub fn messages(&self) -> &[AgentMessage] {
        &self.messages
    }

    pub fn is_streaming(&self) -> bool {
        self.is_streaming
    }

    pub fn set_tools(&mut self, tools: Vec<Box<dyn AgentTool>>) {
        self.tools = tools;
    }

    pub fn clear_messages(&mut self) {
        self.messages.clear();
    }

    pub fn append_message(&mut self, msg: AgentMessage) {
        self.messages.push(msg);
    }

    pub fn replace_messages(&mut self, msgs: Vec<AgentMessage>) {
        self.messages = msgs;
    }

    pub fn save_messages(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string(&self.messages)
    }

    pub fn restore_messages(&mut self, json: &str) -> Result<(), serde_json::Error> {
        let msgs: Vec<AgentMessage> = serde_json::from_str(json)?;
        self.messages = msgs;
        Ok(())
    }

    // -- Queue management --

    /// Queue a steering message (interrupts agent mid-tool-execution)
    pub fn steer(&self, msg: AgentMessage) {
        self.steering_queue.lock().unwrap().push(msg);
    }

    /// Queue a follow-up message (processed after agent finishes)
    pub fn follow_up(&self, msg: AgentMessage) {
        self.follow_up_queue.lock().unwrap().push(msg);
    }

    pub fn clear_steering_queue(&self) {
        self.steering_queue.lock().unwrap().clear();
    }

    pub fn clear_follow_up_queue(&self) {
        self.follow_up_queue.lock().unwrap().clear();
    }

    pub fn clear_all_queues(&self) {
        self.clear_steering_queue();
        self.clear_follow_up_queue();
    }

    pub fn set_steering_mode(&mut self, mode: QueueMode) {
        self.steering_mode = mode;
    }

    pub fn set_follow_up_mode(&mut self, mode: QueueMode) {
        self.follow_up_mode = mode;
    }

    // -- Control --

    pub fn abort(&self) {
        if let Some(ref cancel) = self.cancel {
            cancel.cancel();
        }
    }

    pub fn reset(&mut self) {
        self.messages.clear();
        self.clear_all_queues();
        self.is_streaming = false;
        self.cancel = None;
    }

    // -- Prompting --

    /// Send a text prompt. Returns a stream of AgentEvents.
    pub async fn prompt(&mut self, text: impl Into<String>) -> mpsc::UnboundedReceiver<AgentEvent> {
        let msg = AgentMessage::Llm(Message::user(text));
        self.prompt_messages(vec![msg]).await
    }

    /// Send messages as a prompt.
    pub async fn prompt_messages(
        &mut self,
        messages: Vec<AgentMessage>,
    ) -> mpsc::UnboundedReceiver<AgentEvent> {
        assert!(
            !self.is_streaming,
            "Agent is already streaming. Use steer() or follow_up()."
        );

        let (tx, rx) = mpsc::unbounded_channel();
        let cancel = CancellationToken::new();
        self.cancel = Some(cancel.clone());
        self.is_streaming = true;

        // Build context
        let mut context = AgentContext {
            system_prompt: self.system_prompt.clone(),
            messages: self.messages.clone(),
            tools: Vec::new(), // Tools stay on Agent, referenced via config
        };

        // Move tools temporarily
        let tools = std::mem::take(&mut self.tools);
        context.tools = tools;

        let config = self.build_config();

        let _new_messages = agent_loop(messages, &mut context, &config, tx.clone(), cancel).await;

        // Restore tools and update state
        self.tools = context.tools;
        self.messages = context.messages;
        self.is_streaming = false;
        self.cancel = None;

        rx
    }

    /// Continue from current context (for retries after errors).
    pub async fn continue_loop(&mut self) -> mpsc::UnboundedReceiver<AgentEvent> {
        assert!(!self.is_streaming, "Agent is already streaming.");
        assert!(!self.messages.is_empty(), "No messages to continue from.");

        let (tx, rx) = mpsc::unbounded_channel();
        let cancel = CancellationToken::new();
        self.cancel = Some(cancel.clone());
        self.is_streaming = true;

        let mut context = AgentContext {
            system_prompt: self.system_prompt.clone(),
            messages: self.messages.clone(),
            tools: std::mem::take(&mut self.tools),
        };

        let config = self.build_config();

        let _new_messages = agent_loop_continue(&mut context, &config, tx.clone(), cancel).await;

        self.tools = context.tools;
        self.messages = context.messages;
        self.is_streaming = false;
        self.cancel = None;

        rx
    }

    // -- Internal --

    fn build_config(&self) -> AgentLoopConfig<'_> {
        let steering_queue = self.steering_queue.clone();
        let steering_mode = self.steering_mode;

        let follow_up_queue = self.follow_up_queue.clone();
        let follow_up_mode = self.follow_up_mode;

        AgentLoopConfig {
            provider: &*self.provider,
            model: self.model.clone(),
            api_key: self.api_key.clone(),
            thinking_level: self.thinking_level,
            max_tokens: self.max_tokens,
            temperature: self.temperature,
            convert_to_llm: None,
            transform_context: None,
            get_steering_messages: Some(Box::new(move || {
                let mut queue = steering_queue.lock().unwrap();
                match steering_mode {
                    QueueMode::OneAtATime => {
                        if queue.is_empty() {
                            vec![]
                        } else {
                            vec![queue.remove(0)]
                        }
                    }
                    QueueMode::All => queue.drain(..).collect(),
                }
            })),
            context_config: self.context_config.clone(),
            execution_limits: self.execution_limits.clone(),
            cache_config: self.cache_config.clone(),
            tool_execution: self.tool_execution.clone(),
            retry_config: self.retry_config.clone(),
            get_follow_up_messages: Some(Box::new(move || {
                let mut queue = follow_up_queue.lock().unwrap();
                match follow_up_mode {
                    QueueMode::OneAtATime => {
                        if queue.is_empty() {
                            vec![]
                        } else {
                            vec![queue.remove(0)]
                        }
                    }
                    QueueMode::All => queue.drain(..).collect(),
                }
            })),
            before_turn: self.before_turn.clone(),
            after_turn: self.after_turn.clone(),
            on_error: self.on_error.clone(),
        }
    }
}
