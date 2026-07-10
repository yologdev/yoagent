//! **yoagent** — the agent runtime for Rust.
//!
//! A simple, effective agent loop with tool execution and event streaming:
//! `Prompt → LLM stream → tool execution → loop`. The loop is the product;
//! everything else is optional layers on top of it.
//!
//! # Quick start
//!
//! ```no_run
//! use yoagent::{Agent, provider::ModelConfig, tools};
//!
//! # #[tokio::main]
//! # async fn main() {
//! // Provider is selected from the config's protocol; the API key is read
//! // from ANTHROPIC_API_KEY. Call `.with_api_key(...)` to override.
//! let mut agent = Agent::from_config(ModelConfig::anthropic("claude-sonnet-5", "Sonnet 5"))
//!     .with_system_prompt("You are a helpful coding assistant.")
//!     .with_tools(tools::default_tools());
//!
//! let mut events = agent.prompt("List the files in the current directory").await;
//! while let Some(event) = events.recv().await {
//!     // stream text deltas, tool calls, usage — render however you like
//! }
//! agent.finish().await;
//! # }
//! ```
//!
//! # What's in the box
//!
//! - **The loop** ([`agent_loop()`](agent_loop())) — a stateless free function; [`Agent`] is an
//!   optional stateful wrapper (history, tool registry, steering queues).
//! - **7 provider protocols, 20+ providers** ([`provider`]) — Anthropic,
//!   OpenAI (Completions + Responses), Azure, Gemini, Vertex, Bedrock, plus
//!   OpenAI-compatible gateways (Groq, DeepSeek, xAI, OpenCode, Ollama, ...)
//!   with per-provider quirk flags.
//! - **Tools** ([`tools`]) — bash, read/write/edit file, search; add your own
//!   via the [`AgentTool`] trait. [MCP](mcp) servers and
//!   [OpenAPI specs](https://docs.rs/yoagent/latest/yoagent/openapi/index.html)
//!   (feature `openapi`) become tools transparently.
//! - **Steering** — inject guidance into a running agent ([`Agent::steer`]);
//!   picked up between tool executions (per batch under the default parallel
//!   strategy). Queue follow-ups, inspect/edit the queues.
//! - **Structured outputs** ([`Agent::prompt_structured`]) — typed,
//!   schema-validated replies, enforced natively per provider (forced tool
//!   call / `json_schema` / `responseSchema`).
//! - **Permissions** ([`ToolMiddleware`]) — async approve/deny/modify hooks
//!   gating every tool call; the mechanism behind approval prompts and
//!   policy engines (yoagent ships no policy — you install it).
//! - **Sub-agents** ([`SubAgentTool`]) — delegation with per-sub-agent models
//!   and [`SharedState`] for passing artifacts by reference.
//! - **Context management** ([`context`]) — token tracking and tiered
//!   compaction so long sessions keep running.
//! - **Skills** ([`skills`]) — load `SKILL.md` files per the
//!   [AgentSkills](https://agentskills.io) standard.
//!
//! The [book](https://yologdev.github.io/yoagent/) covers concepts and
//! provider-specific guides.

pub mod agent;
pub mod agent_loop;
pub mod context;
pub mod mcp;
pub mod provider;
pub mod retry;
pub mod shared_state;
pub mod skills;
pub mod sub_agent;
pub mod tools;
pub mod types;

#[cfg(feature = "openapi")]
pub mod openapi;

pub use agent::{Agent, AgentBuildError, StructuredPromptError};
pub use agent_loop::{agent_loop, agent_loop_continue};
pub use context::{CompactionStrategy, DefaultCompaction};
pub use retry::RetryConfig;
pub use shared_state::SharedState;
pub use skills::SkillSet;
pub use sub_agent::SubAgentTool;
pub use types::*;
