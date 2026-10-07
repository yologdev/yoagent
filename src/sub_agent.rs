//! Sub-agent tool — delegates tasks to a child agent loop.
//!
//! The `SubAgentTool` implements `AgentTool` and internally runs `agent_loop()`
//! with its own system prompt, tools, and provider. The parent LLM invokes it
//! like any other tool, passing a natural-language `task` string.
//!
//! # Design
//!
//! - **Context isolation**: each invocation starts a fresh conversation
//! - **Nesting supported**: sub-agents can contain other SubAgentTools for recursive delegation (use `with_max_turns()` to bound depth)
//! - **Cancellation propagation**: the parent's cancel token is forwarded
//! - **Event forwarding**: sub-agent events stream to the parent via `on_update`
//! - **Spend reporting**: the sub-agent's [`SessionStats`] — its own usage and,
//!   recursively, its own sub-agents' — reach the parent loop, which keeps them
//!   in a separate [`SessionStats::sub_agents`] bucket. Failed runs included.
//!
//! # Example
//!
//! ```rust,no_run
//! use yoagent::sub_agent::SubAgentTool;
//! use yoagent::provider::ModelConfig;
//!
//! // Provider selected from the config's protocol; key from ANTHROPIC_API_KEY.
//! let researcher = SubAgentTool::from_config(
//!     "researcher",
//!     ModelConfig::anthropic("claude-sonnet-5", "Sonnet 5"),
//! )
//! .with_description("Searches codebases and documents")
//! .with_system_prompt("You are a research assistant.");
//! ```

use crate::agent_loop::{agent_loop_with_stats, AgentLoopConfig};
use crate::context::ExecutionLimits;
use crate::provider::model::ModelConfig;
use crate::provider::StreamProvider;
use crate::shared_state::SharedState;
use crate::tool_source::{ArcTool, ToolSource};
use crate::tools::shared_state_tool::SharedStateTool;
use crate::types::*;
use std::sync::Arc;
use tokio::sync::mpsc;

/// Default max turns for sub-agents (prevents runaway execution).
const DEFAULT_MAX_TURNS: usize = 10;

/// The limits a delegation runs under unless [`SubAgentTool::with_execution_limits`]
/// says otherwise: 10 turns, 1M tokens, 5 minutes, loop detection on.
fn default_execution_limits() -> ExecutionLimits {
    ExecutionLimits::default()
        .with_max_turns(DEFAULT_MAX_TURNS)
        .with_max_total_tokens(1_000_000)
        .with_max_duration(std::time::Duration::from_secs(300))
}

/// A tool that delegates work to a child agent loop.
///
/// When the parent LLM calls this tool, it spawns a fresh `agent_loop()` with
/// its own system prompt, tools, and provider. The sub-agent runs to completion
/// and its final text output is returned as the tool result.
pub struct SubAgentTool {
    tool_name: String,
    tool_description: String,
    system_prompt: String,
    skills_prompt: String,
    model: String,
    api_key: String,
    provider: Arc<dyn StreamProvider>,
    tools: Vec<Arc<dyn AgentTool>>,
    tool_sources: Vec<Arc<dyn ToolSource>>,
    thinking_level: ThinkingLevel,
    max_tokens: Option<u32>,
    temperature: Option<f32>,
    cache_config: CacheConfig,
    tool_execution: ToolExecutionStrategy,
    retry_config: crate::retry::RetryConfig,
    execution_limits: ExecutionLimits,
    shared_state: Option<SharedState>,
    context_config: Option<crate::context::ContextConfig>,
    turn_delay: Option<std::time::Duration>,
    model_config: Option<ModelConfig>,
    tool_middleware: Vec<Arc<dyn ToolMiddleware>>,
    extensions: Vec<Arc<dyn crate::Extension>>,
    tree_extensions: Vec<Arc<dyn crate::Extension>>,
    max_stop_continues: usize,
    turn_hooks: Vec<Arc<dyn TurnHook>>,
    input_filters: Vec<Arc<dyn InputFilter>>,
    #[cfg(feature = "decision")]
    skills: crate::skills::SkillSet,
    #[cfg(feature = "decision")]
    decision: Option<crate::decision::Advisory>,
    #[cfg(feature = "decision")]
    tool_gate: Option<crate::decision::ToolGate>,
}

impl SubAgentTool {
    /// Create a new sub-agent tool with a name and provider.
    #[doc(hidden)]
    #[deprecated(
        since = "0.10.0",
        note = "use SubAgentTool::from_config(name, config) — provider + env key \
                resolved automatically — or SubAgentTool::from_provider(name, provider, config) \
                for a custom provider; will be removed in 1.0"
    )]
    pub fn new(name: impl Into<String>, provider: Arc<dyn StreamProvider>) -> Self {
        Self::build(name, provider)
    }

    /// Internal constructor shared by `new` and the `from_*` builders (not
    /// deprecated, so the builders don't trip the deprecation lint).
    fn build(name: impl Into<String>, provider: Arc<dyn StreamProvider>) -> Self {
        let name = name.into();
        Self {
            tool_description: format!("Delegate a task to the '{}' sub-agent", name),
            tool_name: name,
            system_prompt: String::new(),
            skills_prompt: String::new(),
            model: String::new(),
            api_key: String::new(),
            provider,
            tools: Vec::new(),
            tool_sources: Vec::new(),
            thinking_level: ThinkingLevel::Off,
            max_tokens: None,
            temperature: None,
            cache_config: CacheConfig::default(),
            tool_execution: ToolExecutionStrategy::default(),
            retry_config: crate::retry::RetryConfig::default(),
            execution_limits: default_execution_limits(),
            shared_state: None,
            context_config: None,
            turn_delay: None,
            model_config: None,
            tool_middleware: Vec::new(),
            extensions: Vec::new(),
            tree_extensions: Vec::new(),
            max_stop_continues: crate::extension::DEFAULT_MAX_STOP_CONTINUES,
            turn_hooks: Vec::new(),
            input_filters: Vec::new(),
            #[cfg(feature = "decision")]
            skills: crate::skills::SkillSet::empty(),
            #[cfg(feature = "decision")]
            decision: None,
            #[cfg(feature = "decision")]
            tool_gate: None,
        }
    }

    /// Create a sub-agent from a name and [`ModelConfig`], selecting the
    /// built-in provider for the config's protocol.
    ///
    /// Mirrors [`Agent::from_config`](crate::Agent::from_config): the model
    /// id, provider, and pricing come from one config, and the API key is
    /// resolved from the provider-conventional env var unless set explicitly
    /// with [`with_api_key`](Self::with_api_key).
    ///
    /// # Panics
    ///
    /// Never panics — the default registry covers every [`ApiProtocol`]
    /// variant. Use [`from_config_with`](Self::from_config_with) with a custom
    /// registry when a protocol may be unregistered and you want a `Result`.
    ///
    /// [`ApiProtocol`]: crate::provider::ApiProtocol
    pub fn from_config(name: impl Into<String>, config: ModelConfig) -> Self {
        Self::from_config_with(&crate::provider::ProviderRegistry::default(), name, config)
            .expect("default registry covers all built-in protocols")
    }

    /// Like [`from_config`](Self::from_config) but resolves the provider from a
    /// caller-supplied registry, returning an error if the config's protocol
    /// isn't registered. Mirrors
    /// [`Agent::from_config_with`](crate::Agent::from_config_with).
    pub fn from_config_with(
        registry: &crate::provider::ProviderRegistry,
        name: impl Into<String>,
        config: ModelConfig,
    ) -> Result<Self, crate::AgentBuildError> {
        let provider = registry
            .resolve(&config.api)
            .ok_or(crate::AgentBuildError::NoProviderForProtocol(config.api))?;
        Ok(Self::build(name, provider).configured_for(config))
    }

    /// Create a sub-agent from a name, explicit provider, and [`ModelConfig`].
    ///
    /// The escape hatch for custom providers and test doubles (pair with
    /// [`ModelConfig::mock`](crate::provider::ModelConfig::mock)). Mirrors
    /// [`Agent::from_provider`](crate::Agent::from_provider).
    pub fn from_provider(
        name: impl Into<String>,
        provider: Arc<dyn StreamProvider>,
        config: ModelConfig,
    ) -> Self {
        Self::build(name, provider).configured_for(config)
    }

    /// Set the model id and stash the config on a freshly-constructed
    /// sub-agent (provider already wired).
    fn configured_for(mut self, config: ModelConfig) -> Self {
        self.model = config.id.clone();
        self.model_config = Some(config);
        self
    }

    /// Re-price the sub-agent's model config against the process-wide price
    /// table now — [`ModelConfig::reprice`] on the stored config. Builder
    /// style: call it before handing the tool to an agent, because a
    /// registered tool is shared and immutable. Constructors resolve prices
    /// when they run, so either install prices
    /// ([`global::install_override`](crate::provider::prices::global::install_override),
    /// [`global::install_fetched`](crate::provider::prices::global::install_fetched))
    /// before building the config, or call this afterwards. Same rules as
    /// [`ModelConfig::reprice`].
    pub fn reprice(mut self) -> Self {
        if let Some(config) = self.model_config.take() {
            self.model_config = Some(config.reprice());
        }
        self
    }

    pub fn with_description(mut self, desc: impl Into<String>) -> Self {
        self.tool_description = desc.into();
        self
    }

    pub fn with_system_prompt(mut self, prompt: impl Into<String>) -> Self {
        self.system_prompt = prompt.into();
        self
    }

    /// Attach a skill set so the sub-agent sees the skills index.
    ///
    /// Mirrors [`Agent::with_skills`](crate::agent::Agent::with_skills): the skills
    /// index is formatted as XML per the [AgentSkills standard](https://agentskills.io)
    /// and appended to the sub-agent's system prompt at dispatch time. The sub-agent
    /// can then read individual SKILL.md files via the `read_file` tool when it
    /// decides a skill is relevant (make sure the sub-agent has such a tool).
    pub fn with_skills(mut self, skills: crate::skills::SkillSet) -> Self {
        self.skills_prompt = skills.format_for_prompt();
        #[cfg(feature = "decision")]
        {
            self.skills = skills;
        }
        self
    }

    #[doc(hidden)]
    #[deprecated(
        since = "0.10.0",
        note = "the model id now comes from the ModelConfig passed to \
                SubAgentTool::from_config / from_provider; will be removed in 1.0"
    )]
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

    /// Add a [`ToolSource`], consulted once per delegation (each delegation
    /// is one run of the sub-agent). Mirrors
    /// [`Agent::with_tool_source`](crate::Agent::with_tool_source): the
    /// sub-agent's own tools win a name collision, then earlier sources.
    pub fn with_tool_source(mut self, source: impl ToolSource + 'static) -> Self {
        self.tool_sources.push(Arc::new(source));
        self
    }

    /// Add a tool middleware for the sub-agent's own tool calls. Mirrors
    /// [`Agent::with_tool_middleware`](crate::Agent::with_tool_middleware).
    pub fn with_tool_middleware(mut self, middleware: impl ToolMiddleware + 'static) -> Self {
        self.tool_middleware.push(Arc::new(middleware));
        self
    }

    /// Add an [`Extension`](crate::Extension) for the sub-agent's runs (see
    /// [`crate::extension`]). Extensions run in installation order, after the
    /// tree extensions and after the older hooks (middleware, filters).
    pub fn with_extension(mut self, extension: impl crate::Extension + 'static) -> Self {
        self.extensions.push(Arc::new(extension));
        self
    }

    /// Add an [`Extension`](crate::Extension) for the sub-agent's runs **and
    /// every run they delegate to**, at any depth: host policy (permissions,
    /// deny rules, redaction, audit, a budget across the tree). A child runs
    /// it ahead of its own extensions and cannot remove it. Its tools are not
    /// offered to child runs.
    pub fn with_tree_extension(mut self, extension: impl crate::Extension + 'static) -> Self {
        self.tree_extensions.push(Arc::new(extension));
        self
    }

    /// How many times per run an extension's `on_stop` may continue it
    /// (default [`DEFAULT_MAX_STOP_CONTINUES`](crate::extension::DEFAULT_MAX_STOP_CONTINUES)).
    pub fn with_max_stop_continues(mut self, max: usize) -> Self {
        self.max_stop_continues = max;
        self
    }

    /// Add a [`TurnHook`] for the sub-agent's own LLM requests. Mirrors
    /// [`Agent::with_turn_hook`](crate::Agent::with_turn_hook): awaited
    /// before every request, it may append one note to that request's
    /// latest user turn. Hooks run in installation order, before the
    /// decision advisory's.
    pub fn with_turn_hook(mut self, hook: impl TurnHook + 'static) -> Self {
        self.turn_hooks.push(Arc::new(hook));
        self
    }

    /// Add an async input filter for the task the parent model hands this
    /// sub-agent. Mirrors
    /// [`Agent::with_async_input_filter`](crate::Agent::with_async_input_filter).
    /// When a filter rejects the task, the sub-agent does not run and the
    /// tool call fails with the reason (the parent's model sees it); a
    /// `Warn` is appended to the task.
    pub fn with_async_input_filter(mut self, filter: impl AsyncInputFilter + 'static) -> Self {
        self.input_filters.push(Arc::new(AsyncFilter::new(filter)));
        self
    }

    /// Screen the task the parent model hands this sub-agent with a
    /// decision model. Mirrors
    /// [`Agent::with_input_guard`](crate::Agent::with_input_guard) — fails
    /// closed — except that the "input" is the task text the parent model
    /// wrote, not the human's words. A rejected task fails the tool call
    /// with the guard's reason. Panics when the guard has no checks.
    #[cfg(feature = "decision")]
    #[cfg_attr(docsrs, doc(cfg(feature = "decision")))]
    pub fn with_input_guard(self, guard: crate::decision::InputGuard) -> Self {
        guard.assert_has_checks();
        self.with_extension(guard)
    }

    /// Attach a decision model for the sub-agent's own turns: advisory skill
    /// and tool hints only. Mirrors
    /// [`Agent::with_decision_model`](crate::Agent::with_decision_model).
    #[cfg(feature = "decision")]
    #[cfg_attr(docsrs, doc(cfg(feature = "decision")))]
    pub fn with_decision_model(self, model: crate::decision::DecisionModel) -> Self {
        self.with_decision_advisory(crate::decision::Advisory::new(model))
    }

    /// Mirrors [`Agent::with_decision_advisory`](crate::Agent::with_decision_advisory).
    #[cfg(feature = "decision")]
    #[cfg_attr(docsrs, doc(cfg(feature = "decision")))]
    pub fn with_decision_advisory(mut self, advisory: crate::decision::Advisory) -> Self {
        self.decision = Some(advisory);
        self
    }

    /// Gate the sub-agent's own tool calls. Mirrors
    /// [`Agent::with_tool_gate`](crate::Agent::with_tool_gate), fail-closed
    /// included. A parent's gate does not cover these calls; and here the
    /// gate's `user_request` is the task text the parent model wrote.
    #[cfg(feature = "decision")]
    #[cfg_attr(docsrs, doc(cfg(feature = "decision")))]
    pub fn with_tool_gate(mut self, gate: crate::decision::ToolGate) -> Self {
        self.tool_gate = Some(gate);
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

    /// Set the sampling temperature for the sub-agent. Note: the newest
    /// reasoning models (e.g. Claude Fable 5 / Opus 4.7+) reject sampling
    /// parameters — leave unset for those.
    pub fn with_temperature(mut self, temperature: f32) -> Self {
        self.temperature = Some(temperature);
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

    /// Give the sub-agent its own context management.
    ///
    /// Sub-agents ran without one, so `truncate_tool_output_on_append` never
    /// fired for them and a sub-agent's oversized tool output entered its
    /// context whole — and, once #133 wired a stash sink here, that sink was
    /// unreachable because the loop gates truncation and stashing together on
    /// this being `Some`.
    ///
    /// This enables the **whole** context pipeline for the child loop, not just
    /// truncation: compaction summarizes and drops old turns once the sub-agent
    /// exceeds `max_context_tokens`. `max_turns` remains the guard on a
    /// sub-agent's *length*; this adds guards on any single tool result and on
    /// total history.
    ///
    /// The gate is this being `Some` **and** `truncate_tool_output_on_append`,
    /// which defaults to true — passing a config with it false still yields no
    /// truncation and no stash. Note also that the budget is line-based, so a
    /// single-line result (minified JSON, base64) passes through whole.
    ///
    /// Not inherited by nested sub-agents; each needs its own call.
    pub fn with_context_config(mut self, config: crate::context::ContextConfig) -> Self {
        self.context_config = Some(config);
        self
    }

    /// Cap on turns per delegation (default 10). Changes only the turn
    /// count; see [`with_execution_limits`](Self::with_execution_limits) for
    /// the rest.
    pub fn with_max_turns(mut self, max: usize) -> Self {
        self.execution_limits = self.execution_limits.with_max_turns(max);
        self
    }

    /// The limits each delegation runs under: turns, tokens, wall-clock time
    /// and loop detection, all per delegation. The default is 10 turns, 1M
    /// tokens and 5 minutes. The token count includes every turn's whole
    /// prompt, so a delegation working over a large context reaches 1M in a
    /// few turns (see
    /// [`ExecutionLimits::max_total_tokens`](crate::context::ExecutionLimits::max_total_tokens));
    /// raise it here for long-running sub-agents. Replaces every limit,
    /// including one set with [`with_max_turns`](Self::with_max_turns)
    /// earlier.
    pub fn with_execution_limits(mut self, limits: ExecutionLimits) -> Self {
        self.execution_limits = limits;
        self
    }

    /// Attach a shared key-value store. Sub-agents get a `shared_state` tool
    /// to read/write variables. The parent can also read/write programmatically
    /// via the `SharedState` handle.
    pub fn with_shared_state(mut self, state: SharedState) -> Self {
        self.shared_state = Some(state);
        self
    }

    /// Attach a shared store restricted to this sub-agent's own namespace.
    ///
    /// Equivalent to [`with_shared_state(state.scoped(name))`](Self::with_shared_state),
    /// spelled out because the isolating variant is easy to forget. The
    /// sub-agent cannot read, overwrite, or even enumerate keys outside its
    /// scope, while the parent's unscoped handle still sees everything it
    /// writes.
    ///
    /// Use when sub-agents should not see each other's data; keep
    /// `with_shared_state` when sharing is the point.
    pub fn with_scoped_shared_state(mut self, state: SharedState, scope: impl AsRef<str>) -> Self {
        self.shared_state = Some(state.scoped(scope));
        self
    }

    /// Add an inter-turn delay to throttle API requests.
    /// Useful when using OAuth tokens or providers with low rate limits.
    /// The delay is applied before each turn except the first.
    pub fn with_turn_delay(mut self, delay: std::time::Duration) -> Self {
        self.turn_delay = Some(delay);
        self
    }

    /// Set the model configuration for multi-provider support.
    /// Required for non-Anthropic providers (OpenAI-compat, Google, etc.)
    /// to specify base URL, compat flags, and other provider-specific settings.
    #[doc(hidden)]
    #[deprecated(
        since = "0.10.0",
        note = "pass the ModelConfig to SubAgentTool::from_config(name, config) or \
                from_provider(name, provider, config) instead; will be removed in 1.0"
    )]
    pub fn with_model_config(mut self, config: ModelConfig) -> Self {
        self.model_config = Some(config);
        self
    }
}

#[cfg_attr(target_arch = "wasm32", async_trait::async_trait(?Send))]
#[cfg_attr(not(target_arch = "wasm32"), async_trait::async_trait)]
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
        params: serde_json::Value,
        ctx: ToolContext,
    ) -> Result<ToolResult, ToolError> {
        let cancel = ctx.cancel.clone();
        let on_update = ctx.on_update.clone();
        let on_progress = ctx.on_progress.clone();
        // Extract the task parameter
        let task = params
            .get("task")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ToolError::InvalidArgs("Missing required 'task' parameter".into()))?
            .to_string();

        // Build tool list from Arc wrappers
        let mut tools: Vec<Box<dyn AgentTool>> = self
            .tools
            .iter()
            .map(|t| Box::new(ArcTool(Arc::clone(t))) as Box<dyn AgentTool>)
            .collect();

        // Append the skills index (if any) so the sub-agent can discover skills.
        let mut system_prompt = self.system_prompt.clone();
        if !self.skills_prompt.is_empty() {
            if system_prompt.is_empty() {
                system_prompt = self.skills_prompt.clone();
            } else {
                system_prompt = format!("{}\n\n{}", system_prompt, self.skills_prompt);
            }
        }

        // Inject SharedStateTool when shared state is configured
        if let Some(ref state) = self.shared_state {
            tools.push(Box::new(SharedStateTool::new(state.clone())));
            let summary = state.prompt_summary().await;
            system_prompt.push_str(&format!(
                "\n\n## Shared State\nYou have access to a shared variable store via the `shared_state` tool.\nAvailable: {}",
                summary
            ));
        }

        // Tools resolved for this run come after the static ones (which win
        // a name collision).
        if !self.tool_sources.is_empty() {
            // A hung source must not outlive the parent's abort.
            let sourced = tokio::select! {
                sourced = crate::tool_source::collect(&self.tool_sources) => sourced,
                _ = cancel.cancelled() => return Err(ToolError::Cancelled),
            };
            crate::tool_source::merge(&mut tools, sourced);
        }

        // Fresh context for the sub-agent
        let mut context = AgentContext {
            system_prompt,
            messages: Vec::new(),
            tools,
        };

        #[cfg(feature = "decision")]
        let decision_extensions = crate::decision::wire(
            self.decision.as_ref(),
            self.tool_gate.as_ref(),
            &self.skills,
        );
        let (provider, tool_middleware) = (
            with_turn_hooks(&self.provider, self.turn_hooks.clone()),
            self.tool_middleware.clone(),
        );

        // Config with Arc'd provider
        let mut config = AgentLoopConfig::new(provider, self.model.clone());
        config.api_key = if self.api_key.is_empty() {
            crate::provider::resolve_api_key_or_warn(self.model_config.as_ref())
        } else {
            self.api_key.clone()
        };
        config.thinking_level = self.thinking_level;
        config.max_tokens = self.max_tokens;
        config.temperature = self.temperature;
        config.model_config = self.model_config.clone();
        config.context_config = self.context_config.clone();
        config.execution_limits = Some(self.execution_limits.clone());
        config.cache_config = self.cache_config.clone();
        config.tool_output_sink = self.shared_state.clone();
        config.tool_execution = self.tool_execution.clone();
        config.retry_config = self.retry_config.clone();
        config.input_filters = self.input_filters.clone();
        config.tool_middleware = tool_middleware;
        config.turn_delay = self.turn_delay;
        config.extensions = self.extensions.clone();
        #[cfg(feature = "decision")]
        config.extensions.extend(decision_extensions);
        config.tree_extensions = self.tree_extensions.clone();
        config.max_stop_continues = self.max_stop_continues;
        // The caller's tree extensions (host policy) apply here too, ahead of
        // this sub-agent's own, at this delegation's depth and label.
        config.delegated_from(&ctx);

        // Channel for sub-agent events
        let (tx, rx) = mpsc::unbounded_channel();
        let mut unforwarded = Some(rx);
        // Why an input filter rejected the task, if one did.
        let rejected: Arc<std::sync::Mutex<Option<String>>> = Arc::default();

        // Forward sub-agent events to parent via on_update and on_progress callbacks
        let forward_handle = if on_update.is_some() || on_progress.is_some() {
            let tool_name = self.tool_name.clone();
            let rejected = rejected.clone();
            let mut rx = unforwarded.take().expect("receiver not taken yet");
            Some(crate::rt::spawn(async move {
                while let Some(event) = rx.recv().await {
                    if let AgentEvent::InputRejected { reason } = &event {
                        *rejected.lock().unwrap_or_else(|e| e.into_inner()) = Some(reason.clone());
                    }
                    // Forward progress messages via on_progress
                    if let AgentEvent::ProgressMessage { text, .. } = &event {
                        if let Some(ref cb) = on_progress {
                            cb(text.clone());
                        }
                    }

                    // Convert interesting events to ToolResult updates for the parent
                    if let Some(ref on_update) = on_update {
                        let update_text = match &event {
                            AgentEvent::MessageUpdate {
                                delta: StreamDelta::Text { delta },
                                ..
                            } => Some(delta.clone()),
                            AgentEvent::ToolExecutionStart { tool_name, .. } => {
                                Some(format!("[sub-agent calling tool: {}]", tool_name))
                            }
                            // The failed attempt's text was forwarded above;
                            // mark where the retry's text starts.
                            AgentEvent::ProviderRetry { error, .. } => {
                                Some(format!("[sub-agent retrying after: {}]", error))
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
                }
            }))
        } else {
            None
        };

        // Run the sub-agent loop
        let prompt = AgentMessage::Llm(Message::user(task));
        let run_cancel = cancel.clone();
        let (new_messages, run_stats) =
            agent_loop_with_stats(vec![prompt], &mut context, &config, tx, cancel).await;

        // Wait for event forwarding to complete. A forwarder that failed may
        // have missed a rejection: the delegation then fails rather than
        // passing for an empty success.
        let mut forward_failed = None;
        if let Some(handle) = forward_handle {
            if let Err(e) = handle.await {
                tracing::error!(tool = %self.tool_name, "sub-agent event forwarder failed: {e}");
                forward_failed = Some(e.to_string());
            }
        } else if let Some(mut rx) = unforwarded {
            while let Ok(event) = rx.try_recv() {
                if let AgentEvent::InputRejected { reason } = event {
                    *rejected.lock().unwrap_or_else(|e| e.into_inner()) = Some(reason);
                }
            }
        }

        // Report before deciding success or failure: a failed delegation
        // still spent tokens, and `Err(ToolError)` has nowhere to carry them.
        // `run_stats` covers this run's own turns and, recursively, whatever
        // its own sub-agents reported to it.
        ctx.report_delegated_run(run_stats.clone());
        if let Some(e) = forward_failed {
            return Err(ToolError::Failed(format!(
                "sub-agent '{}' could not be followed: its event forwarder failed ({e})",
                self.tool_name
            )));
        }

        // An input filter rejected the task: nothing ran.
        let rejection = rejected.lock().unwrap_or_else(|e| e.into_inner()).take();
        if let Some(reason) = rejection {
            return Err(ToolError::Failed(format!(
                "Sub-agent '{}' rejected its task: {}",
                self.tool_name, reason
            )));
        }

        // Cancelled before the model answered at all: not an empty success.
        let answered = new_messages
            .iter()
            .any(|m| matches!(m, AgentMessage::Llm(Message::Assistant { .. })));
        if run_cancel.is_cancelled() && !answered {
            return Err(ToolError::Cancelled);
        }

        // Check if the last message was an error
        if let Some(error_msg) = extract_error(&new_messages) {
            return Err(ToolError::Failed(format!(
                "Sub-agent '{}' failed: {}",
                self.tool_name, error_msg
            )));
        }

        // Extract final assistant text from the returned messages
        let result_text = extract_final_text(&new_messages);

        // Include full sub-agent conversation in details for debugging
        let mut details = serde_json::json!({
            "sub_agent": self.tool_name,
            "turns": new_messages.len(),
        });
        // Read with `SessionStats::from_sub_agent_result`.
        details[SUB_AGENT_STATS_KEY] = serde_json::to_value(&run_stats).unwrap_or_default();

        Ok(ToolResult {
            content: vec![Content::Text { text: result_text }],
            details,
        })
    }
}

/// `provider` behind the turn hooks, or as is when there are none.
fn with_turn_hooks(
    provider: &Arc<dyn StreamProvider>,
    hooks: Vec<Arc<dyn TurnHook>>,
) -> Arc<dyn StreamProvider> {
    if hooks.is_empty() {
        provider.clone()
    } else {
        Arc::new(crate::provider::TurnHookProvider::new(
            provider.clone(),
            hooks,
        ))
    }
}

/// Check if the last assistant message was an error, return the error message.
fn extract_error(messages: &[AgentMessage]) -> Option<String> {
    // A loop abort ends with a marker user message while the last *assistant*
    // message still carries `ToolUse`, so the scan below returned `None` and
    // the delegation reported as a clean success with "(sub-agent produced no
    // text output)". A sub-agent that burned its entire budget looping looked
    // like one that simply had nothing to say.
    //
    // Only loop aborts and cancels. Hitting `max_turns` is a bound, not a failure — the
    // work was cut short but what it produced is real, so it is returned, with
    // the stop notice appended by `extract_final_text` so the parent's model
    // knows the answer is partial.
    if let Some(AgentMessage::Llm(Message::User { content, .. })) = messages.last() {
        if let Some(Content::Text { text }) = content.first() {
            // A cancel between provider calls did not finish either.
            if text.starts_with(crate::agent_loop::LOOP_ABORT_PREFIX)
                || text == crate::agent_loop::CANCELLED_MARKER
            {
                return Some(text.clone());
            }
        }
    }

    for msg in messages.iter().rev() {
        if let AgentMessage::Llm(Message::Assistant {
            stop_reason,
            error_message,
            ..
        }) = msg
        {
            // A cancelled run ends as `Aborted`. Its message is empty, so
            // falling through would hand back an earlier turn's text as if
            // the delegation had finished.
            if *stop_reason == StopReason::Error || *stop_reason == StopReason::Aborted {
                return Some(error_message.clone().unwrap_or_else(|| {
                    if *stop_reason == StopReason::Aborted {
                        "Cancelled".into()
                    } else {
                        "Unknown error".into()
                    }
                }));
            }
        }
    }
    None
}

/// Extract the final assistant text from agent messages.
/// Collects text from the last assistant message, or returns a fallback.
/// The loop's own stop marker, if the run ended on one.
fn stopped_notice(messages: &[AgentMessage]) -> Option<String> {
    if let Some(AgentMessage::Llm(Message::User { content, .. })) = messages.last() {
        if let Some(Content::Text { text }) = content.first() {
            if text.starts_with(crate::agent_loop::AGENT_STOPPED_PREFIX) {
                return Some(text.clone());
            }
        }
    }
    None
}

fn extract_final_text(messages: &[AgentMessage]) -> String {
    let mut out = None;
    for msg in messages.iter().rev() {
        if let AgentMessage::Llm(Message::Assistant { content, .. }) = msg {
            let texts: Vec<&str> = content
                .iter()
                .filter_map(|c| match c {
                    Content::Text { text } if !text.is_empty() => Some(text.as_str()),
                    _ => None,
                })
                .collect();
            if !texts.is_empty() {
                out = Some(texts.join("\n"));
                break;
            }
        }
    }

    // Tell the parent the answer is partial, whether or not there was text to
    // return. A turn-limited run whose last assistant message held only tool
    // calls has no text at all, and reporting that as "produced no text output"
    // reads as an empty success rather than a run cut short.
    match (out, stopped_notice(messages)) {
        (Some(text), Some(stop)) => format!("{text}\n\n{stop}"),
        (Some(text), None) => text,
        (None, Some(stop)) => stop,
        (None, None) => "(sub-agent produced no text output)".to_string(),
    }
}
