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
        let results: Vec<_> = run
            .history
            .iter()
            .filter_map(result_text)
            .filter(|(call, ..)| call == id)
            .collect();
        assert_eq!(results.len(), 1, "{id}: {results:?}");
        let (_, text, is_error) = results[0];
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
    let before = |m: &AgentMessage| serde_json::to_value(m).unwrap();
    assert_eq!(
        run.history[..4].iter().map(before).collect::<Vec<_>>(),
        history.iter().map(before).collect::<Vec<_>>()
    );
    assert_eq!(
        run.history.iter().filter_map(result_text).count(),
        1,
        "{:#?}",
        run.history
    );
    assert!(violation(&run.requests[0]).is_none());
}

/// Parallel calls, one answered, then more turns: the missing result goes
/// after the answered one, before the next message.
#[tokio::test]
async fn a_partly_answered_turn_earlier_in_the_history_is_completed_in_place() {
    let run = run(
        vec![
            user("start"),
            said("ok"),
            user("write both"),
            calls(&["c1", "c2"]),
            answered("c1"),
            user("never mind"),
            said("fine"),
        ],
        Some("next"),
    )
    .await;
    assert_answered_honestly(&run, &["c2"]);
    let ids: Vec<_> = run.history[4..6]
        .iter()
        .filter_map(result_text)
        .map(|(id, ..)| id)
        .collect();
    assert_eq!(ids, ["c1", "c2"], "{:#?}", run.history);
}

/// A custom message (it never reaches the model) between a call and the
/// end of the history does not count as the turn moving on.
#[tokio::test]
async fn a_custom_message_after_an_unanswered_call_is_kept_in_place() {
    let note = AgentMessage::Custom(CustomMessage::new("note", serde_json::json!({"x": 1})));
    let run = run(vec![user("write"), calls(&["c1"]), note], Some("next")).await;
    assert_answered_honestly(&run, &["c1"]);
    assert!(
        matches!(run.history[2], AgentMessage::Custom(_)),
        "{:#?}",
        run.history
    );
    assert!(result_text(&run.history[3]).is_some(), "{:#?}", run.history);
}

/// The results come first among the run's events: after `TurnStart`,
/// before the prompt and the answer.
#[tokio::test]
async fn closed_calls_are_reported_before_the_prompt() {
    let run = run(vec![user("write"), calls(&["c1"])], Some("next")).await;
    let order: Vec<String> = run
        .events
        .iter()
        .filter_map(|e| match e {
            AgentEvent::TurnStart => Some("turn".into()),
            AgentEvent::MessageEnd { message } => Some(match message {
                AgentMessage::Llm(Message::ToolResult { .. }) => "result".into(),
                AgentMessage::Llm(Message::User { .. }) => "prompt".into(),
                AgentMessage::Llm(Message::Assistant { .. }) => "answer".into(),
                _ => "other".into(),
            }),
            _ => None,
        })
        .collect();
    assert_eq!(
        order[..4],
        ["turn", "result", "prompt", "answer"],
        "{order:?}"
    );
}

/// The free function returns the results it added, as the run's first new
/// messages.
#[tokio::test]
async fn agent_loop_continue_returns_the_closed_calls() {
    let provider = Arc::new(MockProvider::texts(vec!["done"]));
    let config = yoagent::agent_loop::AgentLoopConfig::new(provider, "mock");
    let mut context = AgentContext {
        system_prompt: String::new(),
        messages: vec![user("write"), calls(&["c1"])],
        tools: vec![],
    };
    let (tx, _rx) = mpsc::unbounded_channel();
    let new = yoagent::agent_loop::agent_loop_continue(
        &mut context,
        &config,
        tx,
        CancellationToken::new(),
    )
    .await;
    assert_eq!(result_text(&new[0]).map(|(id, ..)| id), Some("c1"));
    assert!(matches!(
        new.last(),
        Some(AgentMessage::Llm(Message::Assistant {
            stop_reason: StopReason::Stop,
            ..
        }))
    ));
}

/// A response that ended in an error kept its calls, and the loop ran none:
/// they are answered "not run", not "may have run".
#[tokio::test]
async fn calls_of_a_failed_response_are_answered_not_run() {
    let failed = AgentMessage::Llm(Message::assistant(
        vec![Content::tool_call(
            "c1",
            "write_file",
            serde_json::json!({"path": "r.md"}),
        )],
        StopReason::Error,
        "mock",
        "mock",
        Usage::default(),
    ));
    let run = run(vec![user("write"), failed], Some("try again")).await;
    assert!(violation(&run.requests[0]).is_none());
    let texts: Vec<_> = run
        .history
        .iter()
        .filter_map(result_text)
        .filter(|(id, ..)| *id == "c1")
        .map(|(_, text, _)| text.to_owned())
        .collect();
    assert_eq!(texts.len(), 1, "{texts:?}");
    assert!(texts[0].starts_with("Not run"), "{}", texts[0]);
    assert_eq!(run.tool_runs, 0);
}

/// A call answered later in a hand-built history is not answered twice (two
/// results for one call are refused too).
#[tokio::test]
async fn a_call_answered_later_is_not_answered_again() {
    let run = run(
        vec![user("write"), calls(&["c1"]), user("hm"), answered("c1")],
        None,
    )
    .await;
    let c1 = run
        .history
        .iter()
        .filter_map(result_text)
        .filter(|(id, ..)| *id == "c1")
        .count();
    assert_eq!(c1, 1, "{:#?}", run.history);
}

fn ended(stop: StopReason, ids: &[&str]) -> AgentMessage {
    AgentMessage::Llm(Message::assistant(
        ids.iter()
            .map(|id| Content::tool_call(*id, "write_file", serde_json::json!({"path": "r.md"})))
            .collect(),
        stop,
        "mock",
        "mock",
        Usage::default(),
    ))
}

fn texts_for<'a>(run: &'a Run, id: &str) -> Vec<&'a str> {
    run.history
        .iter()
        .filter_map(result_text)
        .filter(|(call, ..)| *call == id)
        .map(|(_, text, _)| text)
        .collect()
}

/// A cancelled response, or a refused one, ran none of its calls either.
#[tokio::test]
async fn calls_of_a_cancelled_or_refused_response_are_answered_not_run() {
    for stop in [StopReason::Aborted, StopReason::Refusal] {
        let run = run(
            vec![user("write"), ended(stop.clone(), &["c1"])],
            Some("again"),
        )
        .await;
        assert!(violation(&run.requests[0]).is_none());
        let texts = texts_for(&run, "c1");
        assert_eq!(texts.len(), 1, "{stop:?}: {texts:?}");
        assert!(texts[0].starts_with("Not run"), "{stop:?}: {}", texts[0]);
    }
}

/// Each response's calls get the text for how that response ended.
#[tokio::test]
async fn each_turn_is_answered_by_how_its_response_ended() {
    let run = run(
        vec![
            user("write"),
            ended(StopReason::Error, &["c1"]),
            user("retry"),
            calls(&["c2"]),
        ],
        Some("well?"),
    )
    .await;
    assert!(violation(&run.requests[0]).is_none());
    assert!(texts_for(&run, "c1")[0].starts_with("Not run"));
    assert!(texts_for(&run, "c2")[0].starts_with("No result was recorded"));
    assert!(result_text(&run.history[2]).is_some(), "{:#?}", run.history);
}

/// `continue_loop` after a failed response with calls.
#[tokio::test]
async fn continuing_after_a_failed_response_answers_its_calls_not_run() {
    let run = run(vec![user("write"), ended(StopReason::Error, &["c1"])], None).await;
    assert!(violation(&run.requests[0]).is_none());
    assert!(texts_for(&run, "c1")[0].starts_with("Not run"));
    assert_eq!(run.tool_runs, 0);
}

/// Gemini makes call ids up from their position (`google-fc-0`), so one id
/// recurs turn after turn: an earlier turn's result answers only its own
/// call, in either order.
#[tokio::test]
async fn a_reused_call_id_is_answered_per_turn() {
    // Answered first, left open later.
    let run1 = run(
        vec![
            user("one"),
            calls(&["google-fc-0"]),
            answered("google-fc-0"),
            said("done"),
            user("two"),
            ended(StopReason::Aborted, &["google-fc-0"]),
        ],
        None,
    )
    .await;
    assert!(
        violation(&run1.requests[0]).is_none(),
        "{:#?}",
        run1.requests[0]
    );
    assert_eq!(texts_for(&run1, "google-fc-0").len(), 2);
    // Left open first, answered later.
    let run2 = run(
        vec![
            user("one"),
            calls(&["google-fc-0"]),
            user("two"),
            calls(&["google-fc-0"]),
            answered("google-fc-0"),
            said("done"),
        ],
        Some("three"),
    )
    .await;
    assert!(
        violation(&run2.requests[0]).is_none(),
        "{:#?}",
        run2.requests[0]
    );
    assert!(
        result_text(&run2.history[2]).is_some(),
        "{:#?}",
        run2.history
    );
}

/// On the continue path the results come right after `TurnStart`, before
/// the answer.
#[tokio::test]
async fn closed_calls_are_reported_first_when_continuing() {
    let run = run(vec![user("write"), calls(&["c1"])], None).await;
    let order: Vec<&str> = run
        .events
        .iter()
        .filter_map(|e| match e {
            AgentEvent::TurnStart => Some("turn"),
            AgentEvent::MessageEnd { message } => Some(match message {
                AgentMessage::Llm(Message::ToolResult { .. }) => "result",
                _ => "other",
            }),
            _ => None,
        })
        .collect();
    assert_eq!(order[..3], ["turn", "result", "other"], "{order:?}");
}
