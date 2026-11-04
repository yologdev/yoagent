//! A history with a tool call nobody answered (a run killed while its tools
//! ran, a session saved mid-turn). Every provider refuses a request with an
//! unanswered call, so the next run answers it first, honestly, and never
//! runs the call again.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use yoagent::provider::mock::MockProvider;
use yoagent::provider::{ModelConfig, ProviderError, StreamConfig, StreamEvent, StreamProvider};
use yoagent::*;

/// Records each request's messages.
struct Recording {
    inner: MockProvider,
    requests: Arc<Mutex<Vec<Vec<Message>>>>,
}

#[async_trait::async_trait]
impl StreamProvider for Recording {
    async fn stream(
        &self,
        config: StreamConfig,
        tx: mpsc::UnboundedSender<StreamEvent>,
        cancel: CancellationToken,
    ) -> Result<Message, ProviderError> {
        self.requests.lock().unwrap().push(config.messages.clone());
        self.inner.stream(config, tx, cancel).await
    }
}

/// `write_file`, counting how often it runs.
struct Counted(Arc<AtomicUsize>);

#[async_trait::async_trait]
impl AgentTool for Counted {
    fn name(&self) -> &str {
        "write_file"
    }
    fn label(&self) -> &str {
        "Write"
    }
    fn description(&self) -> &str {
        "write"
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({"type": "object"})
    }
    async fn execute(&self, _: serde_json::Value, _: ToolContext) -> Result<ToolResult, ToolError> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(ToolResult {
            content: vec![Content::Text { text: "ok".into() }],
            details: serde_json::Value::Null,
        })
    }
}

fn user(text: &str) -> AgentMessage {
    AgentMessage::Llm(Message::user(text))
}

fn calls(ids: &[&str]) -> AgentMessage {
    AgentMessage::Llm(Message::assistant(
        ids.iter()
            .map(|id| Content::tool_call(*id, "write_file", serde_json::json!({"path": "r.md"})))
            .collect(),
        StopReason::ToolUse,
        "mock",
        "mock",
        Usage::default(),
    ))
}

fn answered(id: &str) -> AgentMessage {
    AgentMessage::Llm(Message::ToolResult {
        tool_call_id: id.into(),
        tool_name: "write_file".into(),
        content: vec![Content::Text { text: "ok".into() }],
        is_error: false,
        timestamp: 0,
    })
}

fn said(text: &str) -> AgentMessage {
    AgentMessage::Llm(Message::assistant(
        vec![Content::Text { text: text.into() }],
        StopReason::Stop,
        "mock",
        "mock",
        Usage::default(),
    ))
}

/// Why a provider would refuse `messages`, if it would.
fn violation(messages: &[Message]) -> Option<String> {
    let mut pending: Vec<String> = Vec::new();
    for (i, m) in messages.iter().enumerate() {
        match m {
            Message::ToolResult { tool_call_id, .. } => {
                let Some(at) = pending.iter().position(|p| p == tool_call_id) else {
                    return Some(format!("[{i}] answers no open call"));
                };
                pending.remove(at);
            }
            _ if !pending.is_empty() => return Some(format!("[{i}] while {pending:?} open")),
            Message::Assistant { content, .. } => {
                pending = content
                    .iter()
                    .filter_map(|c| match c {
                        Content::ToolCall { id, .. } => Some(id.clone()),
                        _ => None,
                    })
                    .collect();
            }
            Message::User { .. } => {}
        }
    }
    (!pending.is_empty()).then(|| format!("ends with {pending:?} open"))
}

struct Run {
    requests: Vec<Vec<Message>>,
    events: Vec<AgentEvent>,
    history: Vec<AgentMessage>,
    tool_runs: usize,
}

async fn run(history: Vec<AgentMessage>, prompt: Option<&str>) -> Run {
    let requests = Arc::new(Mutex::new(Vec::new()));
    let ran = Arc::new(AtomicUsize::new(0));
    let provider = Recording {
        inner: MockProvider::texts(vec!["done"]),
        requests: requests.clone(),
    };
    let mut agent = Agent::from_provider(provider, ModelConfig::mock())
        .with_tools(vec![Box::new(Counted(ran.clone()))])
        .with_messages(history);
    let mut rx = match prompt {
        Some(p) => agent.prompt(p).await,
        None => agent.continue_loop().await,
    };
    let mut events = Vec::new();
    while let Some(e) = rx.recv().await {
        events.push(e);
    }
    agent.finish().await;
    let requests = requests.lock().unwrap().clone();
    Run {
        requests,
        events,
        history: agent.messages().to_vec(),
        tool_runs: ran.load(Ordering::SeqCst),
    }
}

fn result_text(m: &AgentMessage) -> Option<(&str, &str, bool)> {
    match m {
        AgentMessage::Llm(Message::ToolResult {
            tool_call_id,
            content,
            is_error,
            ..
        }) => match &content[0] {
            Content::Text { text } => Some((tool_call_id.as_str(), text.as_str(), *is_error)),
            _ => None,
        },
        _ => None,
    }
}

fn assert_answered_honestly(run: &Run, ids: &[&str]) {
    assert_eq!(run.requests.len(), 1, "one request, the run finished");
    if let Some(why) = violation(&run.requests[0]) {
        panic!(
            "the request would be refused: {why}\n{:#?}",
            run.requests[0]
        );
    }
    for id in ids {
        let (_, text, is_error) = run
            .history
            .iter()
            .filter_map(result_text)
            .find(|(call, ..)| call == id)
            .unwrap_or_else(|| panic!("{id} has no result"));
        assert!(text.contains("No result was recorded"), "{text}");
        assert!(text.contains("check the current state"), "{text}");
        assert!(is_error);
    }
    assert_eq!(run.tool_runs, 0, "the call is not run again");
    assert!(matches!(
        run.history.last(),
        Some(AgentMessage::Llm(Message::Assistant {
            stop_reason: StopReason::Stop,
            ..
        }))
    ));
}

#[tokio::test]
async fn continuing_answers_an_unanswered_call_first() {
    let run = run(vec![user("write the report"), calls(&["c1", "c2"])], None).await;
    assert_answered_honestly(&run, &["c1", "c2"]);
    // Reported like any message of the run.
    let ended: Vec<_> = run
        .events
        .iter()
        .filter_map(|e| match e {
            AgentEvent::MessageEnd { message } => {
                result_text(message).map(|(id, ..)| id.to_owned())
            }
            _ => None,
        })
        .collect();
    assert_eq!(ended, ["c1", "c2"]);
}

#[tokio::test]
async fn prompting_answers_an_unanswered_call_before_the_prompt() {
    let run = run(
        vec![
            user("write the report"),
            calls(&["c1", "c2"]),
            answered("c1"),
        ],
        Some("are you done?"),
    )
    .await;
    assert_answered_honestly(&run, &["c2"]);
    // c1 keeps its own result.
    let c1: Vec<_> = run
        .history
        .iter()
        .filter_map(result_text)
        .filter(|(id, ..)| *id == "c1")
        .collect();
    assert_eq!(c1, [("c1", "ok", false)]);
}

/// A history repaired earlier on (a call left open, then more turns): the
/// result goes right after the call's turn.
#[tokio::test]
async fn an_unanswered_call_earlier_in_the_history_is_answered_in_place() {
    let run = run(
        vec![
            user("write the report"),
            calls(&["c1"]),
            user("never mind"),
            said("ok"),
        ],
        Some("next"),
    )
    .await;
    assert_answered_honestly(&run, &["c1"]);
    assert!(result_text(&run.history[2]).is_some(), "{:#?}", run.history);
}

#[tokio::test]
async fn a_complete_history_is_left_as_it_is() {
    let history = vec![user("write"), calls(&["c1"]), answered("c1"), said("done")];
    let run = run(history.clone(), Some("thanks")).await;
    assert_eq!(run.history[..4].len(), history.len());
    assert_eq!(
        run.history.iter().filter_map(result_text).count(),
        1,
        "{:#?}",
        run.history
    );
    assert!(violation(&run.requests[0]).is_none());
}
