//! The policy-engine hooks: conversation context on `ToolCallRequest`, async
//! input filters, and per-turn hooks that add a transient note to the latest
//! user turn.

use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;
use yoagent::agent_loop::{agent_loop, AgentLoopConfig};
use yoagent::provider::mock::*;
use yoagent::provider::{
    MockProvider, ModelConfig, ProviderError, StreamConfig, StreamEvent, StreamProvider,
    TurnHookProvider,
};
use yoagent::*;

/// The level-3 compaction marker (crate-private; `is_loop_injected` knows it).
const COMPACTION_MARKER_TEXT: &str =
    "[Context compacted: earlier messages removed to fit the context window]";

/// Records the system prompt and the latest user turn's text blocks (joined
/// by `|`) of every request, then delegates.
struct Recording {
    inner: MockProvider,
    seen: Seen,
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
        self.seen
            .lock()
            .unwrap()
            .push((config.system_prompt.clone(), last_user));
        self.inner.stream(config, tx, cancel).await
    }
}

type Seen = Arc<Mutex<Vec<(String, String)>>>;

fn recording(inner: MockProvider) -> (Recording, Seen) {
    let seen: Seen = Arc::new(Mutex::new(Vec::new()));
    (
        Recording {
            inner,
            seen: seen.clone(),
        },
        seen,
    )
}

struct EchoTool;

#[async_trait::async_trait]
impl AgentTool for EchoTool {
    fn name(&self) -> &str {
        "echo"
    }
    fn label(&self) -> &str {
        "Echo"
    }
    fn description(&self) -> &str {
        "Echoes"
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({"type": "object"})
    }
    async fn execute(
        &self,
        _params: serde_json::Value,
        _ctx: ToolContext,
    ) -> Result<ToolResult, ToolError> {
        Ok(ToolResult {
            content: vec![Content::Text { text: "ok".into() }],
            details: serde_json::Value::Null,
        })
    }
}

fn tool_then_text() -> MockProvider {
    MockProvider::new(vec![
        MockResponse::ToolCalls(vec![MockToolCall {
            provider_metadata: None,
            name: "echo".into(),
            arguments: serde_json::json!({"x": 1}),
        }]),
        MockResponse::Text("done".into()),
    ])
}

async fn run(mut agent: Agent, prompt: &str) -> (Agent, Vec<AgentEvent>) {
    let mut rx = agent.prompt(prompt).await;
    let mut events = Vec::new();
    while let Some(e) = rx.recv().await {
        events.push(e);
    }
    agent.finish().await;
    (agent, events)
}

// ---------------------------------------------------------------------------
// ToolCallRequest carries the conversation
// ---------------------------------------------------------------------------

/// (latest user text, message count) per call.
type HistoryLog = Arc<Mutex<Vec<(Option<String>, usize)>>>;

struct SeesHistory(HistoryLog);

#[async_trait::async_trait]
impl ToolMiddleware for SeesHistory {
    async fn before_tool(&self, call: &ToolCallRequest<'_>) -> ToolDecision {
        self.0
            .lock()
            .unwrap()
            .push((call.latest_user_text(), call.messages.len()));
        ToolDecision::Allow
    }
}

#[tokio::test]
async fn middleware_sees_the_conversation_and_latest_user_text() {
    let log = Arc::new(Mutex::new(Vec::new()));
    let agent = Agent::from_provider(tool_then_text(), ModelConfig::mock())
        .with_tools(vec![Box::new(EchoTool)])
        .with_tool_middleware(SeesHistory(log.clone()));
    let (_agent, _) = run(agent, "delete the temp file").await;
    let log = log.lock().unwrap();
    assert_eq!(log.len(), 1);
    assert_eq!(log[0].0.as_deref(), Some("delete the temp file"));
    // The user prompt and the assistant message carrying the call.
    assert_eq!(log[0].1, 2);
}

// ---------------------------------------------------------------------------
// Async input filters
// ---------------------------------------------------------------------------

struct SlowModeration;

#[async_trait::async_trait]
impl AsyncInputFilter for SlowModeration {
    async fn filter(&self, text: &str) -> FilterResult {
        tokio::task::yield_now().await;
        if text.contains("forbidden") {
            FilterResult::Reject("moderation said no".into())
        } else if text.contains("iffy") {
            FilterResult::Warn("flagged by moderation".into())
        } else {
            FilterResult::Pass
        }
    }
}

#[tokio::test]
async fn async_input_filter_rejects_before_the_model_is_called() {
    let (provider, seen) = recording(MockProvider::text("should not run"));
    let agent =
        Agent::from_provider(provider, ModelConfig::mock()).with_async_input_filter(SlowModeration);
    let (agent, events) = run(agent, "this is forbidden").await;
    assert!(events.iter().any(
        |e| matches!(e, AgentEvent::InputRejected { reason } if reason == "moderation said no")
    ));
    assert!(seen.lock().unwrap().is_empty(), "no LLM request");
    assert!(agent.messages().is_empty());
}

#[tokio::test]
async fn async_input_filter_warns_and_passes() {
    let agent = Agent::from_provider(MockProvider::text("fine"), ModelConfig::mock())
        .with_async_input_filter(SlowModeration);
    let (agent, _) = run(agent, "an iffy request").await;
    let Some(AgentMessage::Llm(Message::User { content, .. })) = agent.messages().first() else {
        panic!("user message first");
    };
    assert!(content.iter().any(
        |c| matches!(c, Content::Text { text } if text == "[Warning: flagged by moderation]")
    ));
    // Positive control: a clean prompt carries no warning.
    let agent = Agent::from_provider(MockProvider::text("fine"), ModelConfig::mock())
        .with_async_input_filter(SlowModeration);
    let (agent, _) = run(agent, "a clean request").await;
    let Some(AgentMessage::Llm(Message::User { content, .. })) = agent.messages().first() else {
        panic!("user message first");
    };
    assert_eq!(content.len(), 1);
}

struct PassSync;
impl InputFilter for PassSync {
    fn filter(&self, _text: &str) -> FilterResult {
        FilterResult::Warn("sync ran".into())
    }
}

#[tokio::test]
async fn sync_and_async_filters_share_one_ordered_list() {
    let agent = Agent::from_provider(MockProvider::text("fine"), ModelConfig::mock())
        .with_input_filter(PassSync)
        .with_async_input_filter(SlowModeration);
    let (agent, _) = run(agent, "an iffy request").await;
    let Some(AgentMessage::Llm(Message::User { content, .. })) = agent.messages().first() else {
        panic!("user message first");
    };
    let Content::Text { text } = content.last().unwrap() else {
        panic!("text")
    };
    assert_eq!(
        text,
        "[Warning: sync ran]\n[Warning: flagged by moderation]"
    );
}

#[test]
fn an_async_filter_called_synchronously_fails_closed() {
    let f = AsyncFilter::new(SlowModeration);
    assert!(matches!(
        InputFilter::filter(&f, "anything"),
        FilterResult::Reject(_)
    ));
    assert!(f.as_async().is_some());
    assert!(PassSync.as_async().is_none());
}

// ---------------------------------------------------------------------------
// Turn hooks
// ---------------------------------------------------------------------------

struct Line(Option<&'static str>, Arc<Mutex<Vec<Option<String>>>>);

#[async_trait::async_trait]
impl TurnHook for Line {
    async fn before_turn(&self, turn: &TurnContext<'_>) -> Option<String> {
        self.1.lock().unwrap().push(turn.latest_user_text());
        self.0.map(str::to_string)
    }
}

struct Panics;

#[async_trait::async_trait]
impl TurnHook for Panics {
    async fn before_turn(&self, _turn: &TurnContext<'_>) -> Option<String> {
        panic!("hook bug")
    }
}

#[tokio::test]
async fn turn_hook_adds_a_transient_note_to_the_latest_user_turn() {
    let (provider, seen) = recording(tool_then_text());
    let calls = Arc::new(Mutex::new(Vec::new()));
    let agent = Agent::from_provider(provider, ModelConfig::mock())
        .with_system_prompt("Base prompt.")
        .with_tools(vec![Box::new(EchoTool)])
        .with_turn_hook(Line(Some("Hint A."), calls.clone()))
        .with_turn_hook(Panics)
        .with_turn_hook(Line(None, calls.clone()))
        .with_turn_hook(Line(Some("Hint B."), calls.clone()));
    let (agent, _) = run(agent, "hello").await;

    let seen = seen.lock().unwrap().clone();
    assert_eq!(seen.len(), 2, "two turns");
    for (prompt, user) in seen.iter() {
        assert_eq!(prompt, "Base prompt.", "the system prompt is untouched");
        assert_eq!(user, "hello|Hint A.\nHint B.");
    }
    // Each hook ran once per turn and saw the user's text.
    let calls = calls.lock().unwrap();
    assert_eq!(calls.len(), 6);
    assert!(calls.iter().all(|c| c.as_deref() == Some("hello")));
    // Transient: never stored.
    assert_eq!(agent.system_prompt, "Base prompt.");
    for m in agent.messages() {
        let json = serde_json::to_string(m).unwrap();
        assert!(!json.contains("Hint A."), "{json}");
    }
}

#[tokio::test]
async fn a_hook_returning_none_leaves_the_request_unchanged() {
    let (provider, seen) = recording(MockProvider::text("hi"));
    let calls = Arc::new(Mutex::new(Vec::new()));
    let agent = Agent::from_provider(provider, ModelConfig::mock())
        .with_system_prompt("Base prompt.")
        .with_turn_hook(Line(None, calls.clone()));
    run(agent, "hello").await;
    assert_eq!(
        seen.lock().unwrap()[0],
        ("Base prompt.".to_string(), "hello".to_string())
    );
    assert_eq!(calls.lock().unwrap().len(), 1, "the hook did run");
}

#[tokio::test]
async fn raw_loop_callers_wrap_the_provider() {
    let (provider, seen) = recording(MockProvider::text("hi"));
    let calls = Arc::new(Mutex::new(Vec::new()));
    let hooked = TurnHookProvider::new(
        Arc::new(provider),
        vec![Arc::new(Line(Some("Hint."), calls.clone()))],
    );
    let mut config = AgentLoopConfig::new(Arc::new(hooked), "mock");
    config.input_filters = vec![Arc::new(AsyncFilter::new(SlowModeration))];
    let mut context = AgentContext {
        system_prompt: String::new(),
        messages: vec![],
        tools: vec![],
    };
    let (tx, _rx) = mpsc::unbounded_channel();
    agent_loop(
        vec![AgentMessage::Llm(Message::user("hello"))],
        &mut context,
        &config,
        tx,
        tokio_util::sync::CancellationToken::new(),
    )
    .await;
    assert_eq!(seen.lock().unwrap()[0].1, "hello|Hint.");
    assert!(context.system_prompt.is_empty());
    // Not stored.
    let Some(AgentMessage::Llm(Message::User { content, .. })) = context.messages.first() else {
        panic!("user message first");
    };
    assert_eq!(content.len(), 1);
}

// ---------------------------------------------------------------------------
// Async filter panics are contained
// ---------------------------------------------------------------------------

struct PanicsOnBoom;

#[async_trait::async_trait]
impl AsyncInputFilter for PanicsOnBoom {
    async fn filter(&self, text: &str) -> FilterResult {
        if text.contains("boom") {
            panic!("filter bug");
        }
        FilterResult::Pass
    }
}

struct Ran(Arc<Mutex<u32>>);

#[async_trait::async_trait]
impl AgentTool for Ran {
    fn name(&self) -> &str {
        "echo"
    }
    fn label(&self) -> &str {
        "Echo"
    }
    fn description(&self) -> &str {
        "Counts runs"
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({"type": "object"})
    }
    async fn execute(
        &self,
        _params: serde_json::Value,
        _ctx: ToolContext,
    ) -> Result<ToolResult, ToolError> {
        *self.0.lock().unwrap() += 1;
        Ok(ToolResult {
            content: vec![Content::Text { text: "ok".into() }],
            details: serde_json::Value::Null,
        })
    }
}

#[tokio::test]
async fn a_panicking_async_filter_rejects_and_the_agent_keeps_its_tools() {
    let count = Arc::new(Mutex::new(0));
    let agent = Agent::from_provider(tool_then_text(), ModelConfig::mock())
        .with_tools(vec![Box::new(Ran(count.clone()))])
        .with_async_input_filter(PanicsOnBoom);
    let (agent, events) = run(agent, "boom").await;
    assert!(events.iter().any(
        |e| matches!(e, AgentEvent::InputRejected { reason } if reason == "input filter panicked")
    ));
    assert!(agent.messages().is_empty());
    // Positive control: the same agent still runs a prompt and its tool.
    let (agent, _) = run(agent, "go").await;
    assert_eq!(*count.lock().unwrap(), 1, "tools survived the panic");
    assert!(!agent.messages().is_empty());
}

// ---------------------------------------------------------------------------
// What the user asked, read out of a conversation
// ---------------------------------------------------------------------------

fn assistant(text: &str) -> Message {
    Message::assistant(
        vec![Content::Text { text: text.into() }],
        StopReason::Stop,
        "mock",
        "mock",
        Usage::default(),
    )
}

fn user_request(messages: &[Message]) -> Option<String> {
    TurnContext::new("", messages, &[], "m").user_request()
}

#[test]
fn a_short_reply_carries_the_exchange_it_answers() {
    let messages = vec![
        Message::user("clean up the workspace"),
        assistant("I found /tmp/scratch.txt. Should I delete it?"),
        Message::user("yes, go ahead"),
    ];
    assert_eq!(
        user_request(&messages).unwrap(),
        "Earlier user request: clean up the workspace\n\n\
         Assistant: I found /tmp/scratch.txt. Should I delete it?\n\n\
         Latest user message: yes, go ahead"
    );
    // Positive control: a self-contained request stands alone.
    let long = "please refactor the parser module so that errors carry spans";
    let messages = vec![
        Message::user("clean up the workspace"),
        assistant("Done."),
        Message::user(long),
    ];
    assert_eq!(user_request(&messages).unwrap(), long);
}

#[test]
fn loop_injected_messages_are_not_the_user_request() {
    let prompt = "tidy up the temp directory, then report what you removed";
    let nudge = format!(
        "{}rm 3 times with identical arguments.]",
        "[You have called "
    );
    let messages = vec![
        Message::user(prompt),
        Message::user(nudge.as_str()),
        Message::user(format!(
            "{} max turns]",
            yoagent::agent_loop::AGENT_STOPPED_PREFIX
        )),
    ];
    assert_eq!(user_request(&messages).unwrap(), prompt);
    let ctx = TurnContext::new("", &messages, &[], "m");
    assert_eq!(ctx.latest_user_text().unwrap(), prompt);
    // Positive control: without the prefixes these would be "the latest".
    let plain = vec![Message::user(prompt), Message::user("rm 3 times")];
    assert_eq!(
        TurnContext::new("", &plain, &[], "m")
            .latest_user_text()
            .unwrap(),
        "rm 3 times"
    );
}

#[test]
fn compacted_history_does_not_yield_summary_text() {
    use yoagent::context::{compact_messages, ContextConfig};
    // Real compaction output: old assistant turns become `[Summary]` user
    // messages.
    let mut history: Vec<AgentMessage> = vec![AgentMessage::Llm(Message::user(
        "migrate the database to the new schema",
    ))];
    for i in 0..40 {
        history.push(AgentMessage::Llm(assistant(&format!(
            "step {i}: {}",
            "detail ".repeat(200)
        ))));
    }
    let config = ContextConfig {
        max_context_tokens: 4_000,
        ..ContextConfig::default()
    };
    let compacted: Vec<Message> = compact_messages(history, &config)
        .into_iter()
        .filter_map(|m| m.as_llm().cloned())
        .collect();
    let has_injected = compacted.iter().any(|m| match m {
        Message::User { content, .. } => content
            .iter()
            .any(|c| matches!(c, Content::Text { text } if is_loop_injected(text))),
        _ => false,
    });
    assert!(
        has_injected,
        "positive control: compaction injected messages"
    );
    let request = user_request(&compacted).unwrap_or_default();
    assert!(!request.contains("[Summary]"), "{request}");
    assert!(!request.contains("[Context compacted"), "{request}");
    // The public predicate knows every marker the loop writes.
    for marker in [
        "[Summary] earlier turn",
        "[Context compacted: earlier messages removed to fit the context window]",
        yoagent::llm_compaction::SUMMARY_MARKER,
        yoagent::agent_loop::AGENT_STOPPED_PREFIX,
        yoagent::agent_loop::LOOP_ABORT_PREFIX,
        "[You have called rm 3 times with identical arguments.]",
    ] {
        assert!(is_loop_injected(marker), "{marker}");
    }
    assert!(!is_loop_injected("please [Summary] this"), "prefix only");
}

#[test]
fn a_short_new_command_stands_alone() {
    let messages = vec![
        Message::user("clean up the workspace"),
        assistant("Done. I removed three temporary files."),
        Message::user("cut the release"),
    ];
    assert_eq!(user_request(&messages).unwrap(), "cut the release");
}

#[test]
fn a_question_mark_inside_code_or_urls_is_not_a_question() {
    let not_questions = [
        "Here is the fix:\n```rust\nlet n = s.parse::<u32>()?;\n```",
        "Docs: https://example.com/search?q=rust",
        "Use the regex `colou?r` to match both spellings",
        "Propagate the error with `?`",
        "Fixed. Should I also update the tests? I went ahead and did it.",
    ];
    for asked in not_questions {
        let messages = vec![
            Message::user("fix the parser"),
            assistant(asked),
            Message::user("ok, thanks"),
        ];
        assert_eq!(user_request(&messages).unwrap(), "ok, thanks", "{asked:?}");
    }
    // Positive controls: a real closing question is carried, markup and
    // trailing whitespace notwithstanding.
    for asked in [
        "Should I delete /tmp/scratch.txt?",
        "Found it.\n\n**Shall I apply the patch?**  \n",
        "The fix uses `?`:\n```rust\nfoo()?;\n```\nDo you want me to commit it?",
    ] {
        let messages = vec![
            Message::user("fix the parser"),
            assistant(asked),
            Message::user("yes"),
        ];
        let request = user_request(&messages).unwrap();
        assert!(
            request.contains("Earlier user request: fix the parser"),
            "{request}"
        );
        assert!(request.ends_with("Latest user message: yes"), "{request}");
    }
}

#[test]
fn a_compaction_boundary_is_never_crossed() {
    let head = Message::user("delete every file in /srv/prod");
    for marker in [
        COMPACTION_MARKER_TEXT.to_string(),
        format!(
            "{}\n\nThe user asked to list files.",
            yoagent::llm_compaction::SUMMARY_MARKER
        ),
        "[Summary] assistant listed /tmp".to_string(),
    ] {
        let messages = vec![
            head.clone(),
            Message::user(marker.as_str()),
            assistant("Working."),
        ];
        let ctx = TurnContext::new("", &messages, &[], "m");
        assert_eq!(ctx.user_request(), None, "{marker}: never the head");
        assert_eq!(ctx.latest_user_text(), None, "{marker}");
        // The run's prompts are the fallback compaction cannot remove.
        let prompts = vec![Message::user("list the files in /tmp")];
        let ctx = ctx.with_run_prompts(&prompts);
        assert_eq!(ctx.user_request().unwrap(), "list the files in /tmp");
        // Positive control: a user message after the boundary wins.
        let mut after = messages.clone();
        after.push(Message::user("now archive /tmp"));
        let ctx = TurnContext::new("", &after, &[], "m").with_run_prompts(&prompts);
        assert_eq!(ctx.user_request().unwrap(), "now archive /tmp");
    }
}

fn image_only() -> Message {
    Message::User {
        content: vec![Content::Image {
            data: "iVBORw0KGgo=".into(),
            mime_type: "image/png".into(),
        }],
        timestamp: 0,
    }
}

#[test]
fn an_image_only_latest_message_stops_the_search() {
    let messages = vec![
        Message::user("delete /tmp/scratch.txt"),
        assistant("Done."),
        image_only(),
    ];
    let ctx = TurnContext::new("", &messages, &[], "m");
    assert_eq!(ctx.user_request(), None, "never the earlier run's request");
    assert_eq!(ctx.latest_user_text(), None);
    // This run's text prompts are the fallback.
    let prompts = vec![Message::user("describe this screenshot")];
    let ctx = ctx.with_run_prompts(&prompts);
    assert_eq!(ctx.user_request().unwrap(), "describe this screenshot");
    // An image-only run prompt gives nothing to fall back on.
    let only_image = vec![image_only()];
    assert_eq!(
        TurnContext::new("", &messages, &[], "m")
            .with_run_prompts(&only_image)
            .user_request(),
        None
    );
    // Positive control: an image with text uses the text.
    let mut with_text = messages.clone();
    with_text.push(Message::User {
        content: vec![
            Content::Image {
                data: "iVBORw0KGgo=".into(),
                mime_type: "image/png".into(),
            },
            Content::Text {
                text: "what is this?".into(),
            },
        ],
        timestamp: 0,
    });
    assert_eq!(
        TurnContext::new("", &with_text, &[], "m")
            .user_request()
            .unwrap(),
        "what is this?"
    );
}

#[test]
fn several_run_prompts_are_labelled() {
    let messages = vec![Message::user(
        "[Context compacted: earlier messages removed to fit the context window]",
    )];
    let prompts = vec![
        Message::user("look around /srv"),
        image_only(),
        Message::user("then tidy it up"),
    ];
    assert_eq!(
        TurnContext::new("", &messages, &[], "m")
            .with_run_prompts(&prompts)
            .user_request()
            .unwrap(),
        "User: look around /srv\n\nUser: then tidy it up"
    );
}

// ---------------------------------------------------------------------------
// User request parts, and ToolCallRequest outside the loop
// ---------------------------------------------------------------------------

#[test]
fn user_request_parts_match_the_prose() {
    // A short reply to a question: every part set.
    let messages = vec![
        Message::user("clean up the workspace"),
        assistant("I found /tmp/scratch.txt. Should I delete it?"),
        Message::user("yes, go ahead"),
    ];
    let turn = TurnContext::new("", &messages, &[], "m");
    let parts = turn.user_request_parts().unwrap();
    assert_eq!(parts.latest, "yes, go ahead");
    assert_eq!(parts.source, UserRequestSource::Conversation);
    assert!(parts.run_prompts.is_empty());
    let reply = parts.reply.clone().unwrap();
    assert_eq!(
        reply.question,
        "I found /tmp/scratch.txt. Should I delete it?"
    );
    assert_eq!(
        reply.earlier_request.as_deref(),
        Some("clean up the workspace")
    );
    let prose = turn.user_request().unwrap();
    for part in [parts.latest, reply.question, reply.earlier_request.unwrap()] {
        assert!(prose.contains(&part), "{prose}");
    }

    // A self-contained request: only `latest`.
    let messages = vec![Message::user(
        "please refactor the parser module so errors carry spans",
    )];
    let parts = TurnContext::new("", &messages, &[], "m")
        .user_request_parts()
        .unwrap();
    assert!(parts.reply.is_none());

    // Compacted away: the run's prompts, each kept separately.
    let messages = vec![Message::user(COMPACTION_MARKER_TEXT)];
    let prompts = vec![
        Message::user("look around /srv"),
        Message::user("tidy /srv"),
    ];
    let turn = TurnContext::new("", &messages, &[], "m").with_run_prompts(&prompts);
    let parts = turn.user_request_parts().unwrap();
    assert_eq!(parts.source, UserRequestSource::RunPrompts);
    assert_eq!(parts.latest, "tidy /srv");
    assert_eq!(parts.run_prompts, ["look around /srv", "tidy /srv"]);
    assert!(parts.reply.is_none());
    let one = [Message::user("tidy /srv")];
    let turn = TurnContext::new("", &messages, &[], "m").with_run_prompts(&one);
    assert_eq!(turn.user_request().as_deref(), Some("tidy /srv"));

    // Nothing at all.
    assert!(TurnContext::new("", &messages, &[], "m")
        .user_request_parts()
        .is_none());
}

#[test]
fn a_tool_call_request_can_be_built_outside_the_loop() {
    let args = serde_json::json!({"path": "/tmp/x"});
    let bare = ToolCallRequest::new("call-1", "rm", &args);
    assert_eq!(bare.tool_call_id, "call-1");
    assert_eq!(bare.tool_name, "rm");
    assert_eq!(bare.args, &args);
    assert!(bare.messages.is_empty() && bare.run_prompts.is_empty());
    assert!(bare.user_request().is_none());

    let history = vec![
        AgentMessage::Llm(Message::user("clean up the workspace")),
        AgentMessage::Llm(assistant("Delete /tmp/x?")),
        AgentMessage::Llm(Message::user("yes")),
    ];
    let call = ToolCallRequest::new("call-1", "rm", &args).with_messages(&history);
    assert_eq!(call.latest_user_text().as_deref(), Some("yes"));
    let reply = call.user_request_parts().unwrap().reply.unwrap();
    assert_eq!(reply.question, "Delete /tmp/x?");
    assert_eq!(
        reply.earlier_request.as_deref(),
        Some("clean up the workspace")
    );

    let prompts = vec![Message::user("remove /tmp/x")];
    let call = ToolCallRequest::new("call-1", "rm", &args).with_run_prompts(&prompts);
    assert_eq!(call.user_request().as_deref(), Some("remove /tmp/x"));
    assert_eq!(
        call.user_request_parts().unwrap().source,
        UserRequestSource::RunPrompts
    );
}

// ---------------------------------------------------------------------------
// SubAgentTool mirrors: turn hooks and async input filters
// ---------------------------------------------------------------------------

fn sub_agent(provider: Recording) -> SubAgentTool {
    SubAgentTool::from_provider("helper", Arc::new(provider), ModelConfig::mock())
}

fn tool_text(result: &ToolResult) -> String {
    match &result.content[0] {
        Content::Text { text } => text.clone(),
        _ => panic!("text"),
    }
}

#[tokio::test]
async fn a_sub_agent_turn_hook_notes_its_own_requests() {
    let (provider, seen) = recording(MockProvider::text("done"));
    let calls = Arc::new(Mutex::new(Vec::new()));
    let tool = sub_agent(provider).with_turn_hook(Line(Some("Sub hint."), calls.clone()));
    let result = tool
        .execute(
            serde_json::json!({"task": "summarize"}),
            ToolContext::new("tc-1", "helper"),
        )
        .await
        .unwrap();
    assert_eq!(tool_text(&result), "done");
    let seen = seen.lock().unwrap().clone();
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].1, "summarize|Sub hint.");
    assert_eq!(
        calls.lock().unwrap().as_slice(),
        [Some("summarize".to_string())]
    );
}

#[tokio::test]
async fn a_sub_agent_async_filter_can_reject_or_warn() {
    // Rejected: the sub-agent never calls its model, and the tool call fails
    // with the reason.
    let (provider, seen) = recording(MockProvider::text("should not run"));
    let tool = sub_agent(provider).with_async_input_filter(SlowModeration);
    let err = tool
        .execute(
            serde_json::json!({"task": "do the forbidden thing"}),
            ToolContext::new("tc-1", "helper"),
        )
        .await
        .unwrap_err();
    let text = err.to_string();
    assert!(text.contains("rejected its task"), "{text}");
    assert!(text.contains("moderation said no"), "{text}");
    assert!(seen.lock().unwrap().is_empty(), "no LLM request");

    // The same with event forwarding on (the other code path).
    let (provider, _) = recording(MockProvider::text("should not run"));
    let tool = sub_agent(provider).with_async_input_filter(SlowModeration);
    let ctx = ToolContext::new("tc-1", "helper").with_on_progress(Arc::new(|_| {}));
    let err = tool
        .execute(serde_json::json!({"task": "forbidden"}), ctx)
        .await
        .unwrap_err();
    assert!(err.to_string().contains("moderation said no"));

    // Warned: runs, with the warning appended to the task.
    let (provider, seen) = recording(MockProvider::text("ok"));
    let tool = sub_agent(provider).with_async_input_filter(SlowModeration);
    tool.execute(
        serde_json::json!({"task": "an iffy task"}),
        ToolContext::new("tc-1", "helper"),
    )
    .await
    .unwrap();
    assert_eq!(
        seen.lock().unwrap()[0].1,
        "an iffy task|[Warning: flagged by moderation]"
    );

    // Positive control: a clean task runs untouched.
    let (provider, seen) = recording(MockProvider::text("ok"));
    let tool = sub_agent(provider).with_async_input_filter(SlowModeration);
    tool.execute(
        serde_json::json!({"task": "a clean task"}),
        ToolContext::new("tc-1", "helper"),
    )
    .await
    .unwrap();
    assert_eq!(seen.lock().unwrap()[0].1, "a clean task");
}
