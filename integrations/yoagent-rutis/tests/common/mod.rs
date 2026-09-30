//! Shared test doubles: a recording provider, simple tools, fiber waits.
#![allow(dead_code)]

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rutis::{FiberState, FiberView};
use tokio::sync::mpsc;
use yoagent::provider::mock::*;
use yoagent::provider::{
    MockProvider, ModelConfig, ProviderError, StreamConfig, StreamEvent, StreamProvider,
};
use yoagent::*;

/// One request as the provider received it.
#[derive(Debug, Clone)]
pub struct Seen {
    pub tools: Vec<String>,
    /// Text blocks of the latest user message, joined by `|`.
    pub last_user: String,
}

pub type SeenLog = Arc<Mutex<Vec<Seen>>>;

pub struct Recording {
    inner: MockProvider,
    seen: SeenLog,
}

#[async_trait::async_trait]
impl StreamProvider for Recording {
    async fn stream(
        &self,
        config: StreamConfig,
        tx: mpsc::UnboundedSender<StreamEvent>,
        cancel: tokio_util::sync::CancellationToken,
    ) -> Result<Message, ProviderError> {
        let last_user = config
            .messages
            .iter()
            .rev()
            .find_map(|m| match m {
                Message::User { content, .. } => Some(
                    content
                        .iter()
                        .filter_map(|c| match c {
                            Content::Text { text } => Some(text.as_str()),
                            _ => None,
                        })
                        .collect::<Vec<_>>()
                        .join("|"),
                ),
                _ => None,
            })
            .unwrap_or_default();
        self.seen.lock().unwrap().push(Seen {
            tools: config.tools.iter().map(|t| t.name.clone()).collect(),
            last_user,
        });
        self.inner.stream(config, tx, cancel).await
    }
}

/// An agent over a recording mock provider.
pub fn agent(responses: Vec<MockResponse>) -> (Agent, SeenLog) {
    let seen = SeenLog::default();
    let provider = Recording {
        inner: MockProvider::new(responses),
        seen: seen.clone(),
    };
    (Agent::from_provider(provider, ModelConfig::mock()), seen)
}

pub fn recording(responses: Vec<MockResponse>) -> (Arc<dyn StreamProvider>, SeenLog) {
    let seen = SeenLog::default();
    (
        Arc::new(Recording {
            inner: MockProvider::new(responses),
            seen: seen.clone(),
        }),
        seen,
    )
}

pub fn call(name: &str, args: serde_json::Value) -> MockResponse {
    MockResponse::ToolCalls(vec![MockToolCall {
        provider_metadata: None,
        name: name.into(),
        arguments: args,
    }])
}

pub fn text(t: &str) -> MockResponse {
    MockResponse::Text(t.into())
}

/// Replies with a fixed text and counts its runs.
pub struct Reply {
    pub name: String,
    pub reply: String,
    pub runs: Arc<AtomicUsize>,
}

impl Reply {
    pub fn new(name: &str, reply: &str) -> Self {
        Self {
            name: name.into(),
            reply: reply.into(),
            runs: Arc::default(),
        }
    }

    pub fn runs(&self) -> Arc<AtomicUsize> {
        self.runs.clone()
    }
}

#[async_trait::async_trait]
impl AgentTool for Reply {
    fn name(&self) -> &str {
        &self.name
    }
    fn label(&self) -> &str {
        &self.name
    }
    fn description(&self) -> &str {
        "replies with a fixed text"
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({"type": "object", "properties": {}})
    }
    async fn execute(
        &self,
        _params: serde_json::Value,
        _ctx: ToolContext,
    ) -> Result<ToolResult, ToolError> {
        self.runs.fetch_add(1, Ordering::SeqCst);
        Ok(ToolResult {
            content: vec![Content::Text {
                text: self.reply.clone(),
            }],
            details: serde_json::Value::Null,
        })
    }
}

/// Replies with its arguments, serialized.
pub struct EchoArgs;

#[async_trait::async_trait]
impl AgentTool for EchoArgs {
    fn name(&self) -> &str {
        "echo_args"
    }
    fn label(&self) -> &str {
        "echo_args"
    }
    fn description(&self) -> &str {
        "echoes its arguments"
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({"type": "object", "properties": {"path": {"type": "string"}}})
    }
    async fn execute(
        &self,
        params: serde_json::Value,
        _ctx: ToolContext,
    ) -> Result<ToolResult, ToolError> {
        Ok(ToolResult {
            content: vec![Content::Text {
                text: params.to_string(),
            }],
            details: serde_json::Value::Null,
        })
    }
}

/// Every tool result of `messages` as `(tool_name, text, is_error)`.
pub fn tool_results(messages: &[AgentMessage]) -> Vec<(String, String, bool)> {
    messages
        .iter()
        .filter_map(|m| match m {
            AgentMessage::Llm(Message::ToolResult {
                tool_name,
                content,
                is_error,
                ..
            }) => Some((
                tool_name.clone(),
                content
                    .iter()
                    .filter_map(|c| match c {
                        Content::Text { text } => Some(text.as_str()),
                        _ => None,
                    })
                    .collect::<String>(),
                *is_error,
            )),
            _ => None,
        })
        .collect()
}

/// Run one prompt to completion; returns the events and this run's tool
/// results.
pub async fn run(
    agent: &mut Agent,
    prompt: &str,
) -> (Vec<AgentEvent>, Vec<(String, String, bool)>) {
    let before = agent.messages().len();
    let (tx, mut rx) = mpsc::unbounded_channel();
    let collect = tokio::spawn(async move {
        let mut events = Vec::new();
        while let Some(e) = rx.recv().await {
            events.push(e);
        }
        events
    });
    agent.prompt_with_sender(prompt, tx).await;
    let events = collect.await.unwrap();
    let results = tool_results(&agent.messages()[before.min(agent.messages().len())..]);
    (events, results)
}

pub async fn wait_state(view: &FiberView, want: FiberState) {
    let mut rx = view.watch();
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if rx.borrow_and_update().state == want {
                return;
            }
            rx.changed().await.expect("fiber driver alive");
        }
    })
    .await
    .unwrap_or_else(|_| panic!("{} never reached {want:?}: {:?}", view.name(), view.state()));
}

pub async fn wait_active(view: &FiberView) {
    wait_state(view, FiberState::Active).await
}
