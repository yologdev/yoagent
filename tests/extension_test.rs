//! The `Extension` contract (#241): every hook, the advisory/required
//! failure rules, isolation between runs, partial-output withholding, and
//! tree extensions across sub-agents.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use yoagent::extension::*;
use yoagent::provider::mock::{MockResponse, MockToolCall};
use yoagent::provider::{
    MockProvider, ModelConfig, ProviderError, StreamConfig, StreamEvent, StreamProvider,
};
use yoagent::*;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Wraps a provider and records each request's messages.
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

fn call(name: &str, args: serde_json::Value) -> MockResponse {
    MockResponse::ToolCalls(vec![MockToolCall {
        name: name.into(),
        arguments: args,
        provider_metadata: None,
    }])
}

fn text(t: &str) -> MockResponse {
    MockResponse::Text(t.into())
}

/// An agent on scripted responses that also returns the recorded requests.
fn scripted(responses: Vec<MockResponse>) -> (Agent, Arc<Mutex<Vec<Vec<Message>>>>) {
    let requests = Arc::new(Mutex::new(Vec::new()));
    let provider = Recording {
        inner: MockProvider::new(responses),
        requests: requests.clone(),
    };
    let agent =
        Agent::from_provider(provider, ModelConfig::mock()).with_tools(vec![Box::new(Echo)]);
    (agent, requests)
}

async fn run(agent: &mut Agent, prompt: &str) -> Vec<AgentEvent> {
    let (tx, mut rx) = mpsc::unbounded_channel();
    agent.prompt_with_sender(prompt, tx).await;
    let mut events = Vec::new();
    while let Ok(e) = rx.try_recv() {
        events.push(e);
    }
    events
}

fn texts(content: &[Content]) -> String {
    content
        .iter()
        .filter_map(|c| match c {
            Content::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join(" | ")
}

fn last_user_text(messages: &[Message]) -> String {
    messages
        .iter()
        .rev()
        .find_map(|m| match m {
            Message::User { content, .. } => Some(texts(content)),
            _ => None,
        })
        .unwrap_or_default()
}

fn final_assistant(events: &[AgentEvent]) -> (StopReason, Option<String>) {
    let Some(AgentEvent::AgentEnd { messages, .. }) = events.last() else {
        panic!("the run must end with AgentEnd");
    };
    messages
        .iter()
        .rev()
        .find_map(|m| match m {
            AgentMessage::Llm(Message::Assistant {
                stop_reason,
                error_message,
                ..
            }) => Some((stop_reason.clone(), error_message.clone())),
            _ => None,
        })
        .expect("an assistant message")
}

fn end_messages(events: &[AgentEvent]) -> Vec<AgentMessage> {
    match events.last() {
        Some(AgentEvent::AgentEnd { messages, .. }) => messages.clone(),
        _ => panic!("the run must end with AgentEnd"),
    }
}

fn tool_results(events: &[AgentEvent]) -> Vec<(String, bool)> {
    events
        .iter()
        .filter_map(|e| match e {
            AgentEvent::ToolExecutionEnd {
                result, is_error, ..
            } => Some((texts(&result.content), *is_error)),
            _ => None,
        })
        .collect()
}

fn turns_paired(events: &[AgentEvent]) -> bool {
    let starts = events
        .iter()
        .filter(|e| matches!(e, AgentEvent::TurnStart))
        .count();
    let ends = events
        .iter()
        .filter(|e| matches!(e, AgentEvent::TurnEnd { .. }))
        .count();
    starts == ends
}

/// A tool that echoes its `text` argument, and can stream partial output.
struct Echo;

#[async_trait::async_trait]
impl AgentTool for Echo {
    fn name(&self) -> &str {
        "echo"
    }
    fn label(&self) -> &str {
        "Echo"
    }
    fn description(&self) -> &str {
        "Echo the text"
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({"type": "object", "properties": {"text": {"type": "string"}}})
    }
    async fn execute(
        &self,
        params: serde_json::Value,
        ctx: ToolContext,
    ) -> Result<ToolResult, ToolError> {
        let text = params["text"].as_str().unwrap_or_default().to_string();
        if let Some(update) = &ctx.on_update {
            update(ToolResult {
                content: vec![Content::Text {
                    text: format!("partial {text}"),
                }],
                details: serde_json::Value::Null,
            });
        }
        if let Some(progress) = &ctx.on_progress {
            progress(format!("progress {text}"));
        }
        Ok(ToolResult {
            content: vec![Content::Text {
                text: format!("echo {text}"),
            }],
            details: serde_json::Value::Null,
        })
    }
}

/// A configurable extension: each field is what the hook does.
#[derive(Clone, Default)]
struct Hooks {
    tools: Vec<&'static str>,
    reject: Option<&'static str>,
    note: Option<&'static str>,
    stop: Option<&'static str>,
    fail_model: Option<&'static str>,
    deny: Option<&'static str>,
    modify: Option<serde_json::Value>,
    redact: Option<(&'static str, &'static str)>,
    fail_after_tool: bool,
    continue_with: Option<&'static str>,
    fail_stop: Option<&'static str>,
    panic_in: Option<&'static str>,
    log: Option<Arc<Mutex<Vec<String>>>>,
    /// Every event `on_event` sees, as Debug text.
    audit: Option<Arc<Mutex<Vec<String>>>>,
    /// `on_event` panics on events this matches.
    panic_on_event: Option<fn(&AgentEvent) -> bool>,
}

impl Hooks {
    fn record(&self, entry: String) {
        if let Some(log) = &self.log {
            log.lock().unwrap().push(entry);
        }
    }
    fn maybe_panic(&self, hook: &str) {
        if self.panic_in == Some(hook) {
            panic!("{hook} exploded");
        }
    }
}

#[async_trait::async_trait]
impl RunHooks for Hooks {
    async fn tools(&mut self, _run: &RunContext<'_>) -> Vec<Arc<dyn AgentTool>> {
        self.maybe_panic("tools");
        self.tools
            .iter()
            .map(|name| Arc::new(Named(name)) as Arc<dyn AgentTool>)
            .collect()
    }
    async fn on_input(&mut self, input: &InputContext<'_>) -> InputDecision {
        self.maybe_panic("on_input");
        self.record(format!("on_input {}", input.text));
        match self.reject {
            Some(reason) => InputDecision::Reject(reason.into()),
            None => InputDecision::Pass,
        }
    }
    async fn before_model(&mut self, _turn: &TurnContext<'_>) -> TurnDecision {
        self.maybe_panic("before_model");
        self.record("before_model".into());
        if let Some(reason) = self.fail_model {
            return TurnDecision::Fail(reason.into());
        }
        if let Some(reason) = self.stop {
            return TurnDecision::Stop(reason.into());
        }
        match self.note {
            Some(note) => TurnDecision::Note(note.into()),
            None => TurnDecision::Continue,
        }
    }
    async fn before_tool(&self, call: &ToolCallRequest<'_>) -> ToolDecision {
        self.maybe_panic("before_tool");
        self.record(format!("before_tool {} {}", call.tool_name, call.args));
        if let Some(forbidden) = self.deny {
            if call.args.to_string().contains(forbidden) {
                return ToolDecision::Deny(format!("{forbidden} is not allowed"));
            }
        }
        match &self.modify {
            Some(args) => ToolDecision::Modify(args.clone()),
            None => ToolDecision::Allow,
        }
    }
    async fn after_tool(
        &self,
        _call: &ToolCallRequest<'_>,
        output: &mut ToolOutput,
    ) -> Result<(), ExtensionError> {
        self.maybe_panic("after_tool");
        if self.fail_after_tool {
            return Err("cannot process".into());
        }
        if let Some((secret, mask)) = self.redact {
            for c in &mut output.result.content {
                if let Content::Text { text } = c {
                    *text = text.replace(secret, mask);
                }
            }
        }
        Ok(())
    }
    async fn on_stop(&mut self, stop: &StopContext<'_>) -> StopDecision {
        self.maybe_panic("on_stop");
        self.record(format!("on_stop continues={}", stop.continues));
        if let Some(reason) = self.fail_stop {
            return StopDecision::Fail(reason.into());
        }
        match self.continue_with {
            Some(message) => StopDecision::Continue(message.into()),
            None => StopDecision::Accept,
        }
    }
    fn on_event(&self, event: &AgentEvent) {
        if let Some(audit) = &self.audit {
            audit.lock().unwrap().push(format!("{event:?}"));
        }
        if self.panic_on_event.is_some_and(|matches| matches(event)) {
            panic!("event sink down");
        }
    }
    async fn finish(&mut self, outcome: &RunOutcome) {
        self.record(format!(
            "finish {:?} stop={:?}",
            outcome.end(),
            outcome.stop_reason()
        ));
    }
}

/// A tool that only reports its name.
struct Named(&'static str);

#[async_trait::async_trait]
impl AgentTool for Named {
    fn name(&self) -> &str {
        self.0
    }
    fn label(&self) -> &str {
        self.0
    }
    fn description(&self) -> &str {
        "named"
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({"type": "object", "properties": {}})
    }
    async fn execute(&self, _: serde_json::Value, _: ToolContext) -> Result<ToolResult, ToolError> {
        Ok(ToolResult {
            content: vec![Content::Text {
                text: format!("ran {}", self.0),
            }],
            details: serde_json::Value::Null,
        })
    }
}

fn ext(name: &str, hooks: Hooks) -> ClonedHooks<Hooks> {
    ClonedHooks::new(name, hooks)
}

// ---------------------------------------------------------------------------
// Hooks
// ---------------------------------------------------------------------------

/// Offers a tool named `extra` whose description says which run made it,
/// and one named `echo`, which collides with the agent's own.
struct PerRunTools(AtomicUsize);

struct PerRunHooks(usize);

#[async_trait::async_trait]
impl RunHooks for PerRunHooks {
    async fn tools(&mut self, _: &RunContext<'_>) -> Vec<Arc<dyn AgentTool>> {
        vec![
            Arc::new(Described("extra", format!("run {}", self.0))),
            Arc::new(Described("echo", "an impostor".into())),
        ]
    }
}

#[async_trait::async_trait]
impl Extension for PerRunTools {
    fn name(&self) -> &str {
        "tools"
    }
    async fn start_run(&self, _: &RunContext<'_>) -> Result<Box<dyn RunHooks>, ExtensionError> {
        Ok(Box::new(PerRunHooks(
            self.0.fetch_add(1, Ordering::SeqCst) + 1,
        )))
    }
}

struct Described(&'static str, String);

#[async_trait::async_trait]
impl AgentTool for Described {
    fn name(&self) -> &str {
        self.0
    }
    fn label(&self) -> &str {
        self.0
    }
    fn description(&self) -> &str {
        &self.1
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({"type": "object", "properties": {}})
    }
    async fn execute(&self, _: serde_json::Value, _: ToolContext) -> Result<ToolResult, ToolError> {
        Ok(ToolResult {
            content: vec![Content::Text {
                text: format!("ran {}", self.1),
            }],
            details: serde_json::Value::Null,
        })
    }
}

/// Records each request's tool definitions as `name: description`.
struct ToolsSeen {
    inner: MockProvider,
    seen: Arc<Mutex<Vec<Vec<String>>>>,
}

#[async_trait::async_trait]
impl StreamProvider for ToolsSeen {
    async fn stream(
        &self,
        config: StreamConfig,
        tx: mpsc::UnboundedSender<StreamEvent>,
        cancel: CancellationToken,
    ) -> Result<Message, ProviderError> {
        self.seen.lock().unwrap().push(
            config
                .tools
                .iter()
                .map(|t| format!("{}: {}", t.name, t.description))
                .collect(),
        );
        self.inner.stream(config, tx, cancel).await
    }
}

#[tokio::test]
async fn tools_are_offered_for_the_run_only_and_static_tools_win() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let provider = ToolsSeen {
        inner: MockProvider::new(vec![
            call("extra", serde_json::json!({})),
            text("one"),
            text("two"),
        ]),
        seen: seen.clone(),
    };
    let mut agent = Agent::from_provider(provider, ModelConfig::mock())
        .with_tools(vec![Box::new(Echo)])
        .with_extension(PerRunTools(AtomicUsize::new(0)));

    let events = run(&mut agent, "first").await;
    assert_eq!(tool_results(&events), vec![("ran run 1".into(), false)]);
    let _ = run(&mut agent, "second").await;

    let seen = seen.lock().unwrap();
    // The agent's `echo` wins over the extension's; each run sees only its
    // own `extra` (the first run's was removed when it ended).
    assert_eq!(seen[0], vec!["echo: Echo the text", "extra: run 1"]);
    assert_eq!(seen[1], vec!["echo: Echo the text", "extra: run 1"]);
    assert_eq!(seen[2], vec!["echo: Echo the text", "extra: run 2"]);
}

#[tokio::test]
async fn on_input_rejects_and_finish_reports_it() {
    let log = Arc::new(Mutex::new(Vec::new()));
    let (agent, _) = scripted(vec![text("never")]);
    let mut agent = agent.with_extension(ext(
        "input",
        Hooks {
            reject: Some("not today"),
            log: Some(log.clone()),
            ..Default::default()
        },
    ));
    let events = run(&mut agent, "hello").await;
    assert!(events
        .iter()
        .any(|e| matches!(e, AgentEvent::InputRejected { reason } if reason == "not today")));
    assert_eq!(
        *log.lock().unwrap(),
        vec![
            "on_input hello".to_string(),
            r#"finish Rejected { reason: "not today" } stop=None"#.to_string()
        ]
    );
}

#[tokio::test]
async fn before_model_notes_land_on_the_latest_user_turn_and_are_not_stored() {
    let (agent, requests) = scripted(vec![
        call("echo", serde_json::json!({"text": "x"})),
        text("done"),
    ]);
    let mut agent = agent.with_extension(ext(
        "notes",
        Hooks {
            note: Some("remember X"),
            ..Default::default()
        },
    ));
    let events = run(&mut agent, "go").await;
    let requests = requests.lock().unwrap();
    assert_eq!(requests.len(), 2);
    for request in requests.iter() {
        assert_eq!(last_user_text(request), "go | remember X");
    }
    // Not stored: the history holds the prompt as sent by the user.
    let history = end_messages(&events);
    let AgentMessage::Llm(Message::User { content, .. }) = &history[0] else {
        panic!("first message is the prompt");
    };
    assert_eq!(texts(content), "go");
}

#[tokio::test]
async fn before_model_stop_ends_the_run_like_a_limit() {
    let (agent, requests) = scripted(vec![text("never")]);
    let mut agent = agent.with_extension(ext(
        "budget",
        Hooks {
            stop: Some("budget spent"),
            ..Default::default()
        },
    ));
    let events = run(&mut agent, "go").await;
    assert!(requests.lock().unwrap().is_empty(), "no request was sent");
    let last = end_messages(&events).last().cloned().unwrap();
    let AgentMessage::Llm(Message::User { content, .. }) = last else {
        panic!("ends with the stop marker");
    };
    assert_eq!(texts(&content), "[Agent stopped: budget spent]");
    assert!(turns_paired(&events));
}

#[tokio::test]
async fn a_required_extension_failing_fails_the_run() {
    let errors = Arc::new(Mutex::new(Vec::new()));
    let seen = errors.clone();
    let (agent, requests) = scripted(vec![text("never")]);
    let mut agent = agent
        .with_extension(
            ext(
                "verifier",
                Hooks {
                    fail_model: Some("cannot verify"),
                    ..Default::default()
                },
            )
            .required(),
        )
        .on_error(move |e| seen.lock().unwrap().push(e.to_string()));
    let events = run(&mut agent, "go").await;
    assert!(requests.lock().unwrap().is_empty());
    let (stop, error) = final_assistant(&events);
    assert_eq!(stop, StopReason::Error);
    let error = error.unwrap();
    assert!(error.starts_with(EXTENSION_FAILED_PREFIX), "{error}");
    assert!(
        error.contains("verifier") && error.contains("cannot verify"),
        "{error}"
    );
    assert_eq!(errors.lock().unwrap().len(), 1);
    assert!(turns_paired(&events));
}

#[tokio::test]
async fn an_advisory_extension_failing_is_skipped() {
    let (agent, _) = scripted(vec![text("done")]);
    let mut agent = agent.with_extension(ext(
        "advice",
        Hooks {
            fail_model: Some("no advice"),
            ..Default::default()
        },
    ));
    let events = run(&mut agent, "go").await;
    assert_eq!(final_assistant(&events).0, StopReason::Stop);
}

#[tokio::test]
async fn panics_are_contained_by_mode() {
    for hook in ["before_model", "on_stop", "tools"] {
        // Advisory: the run completes.
        let (agent, _) = scripted(vec![text("done")]);
        let mut a = agent.with_extension(ext(
            "p",
            Hooks {
                panic_in: Some(hook),
                ..Default::default()
            },
        ));
        let events = run(&mut a, "go").await;
        assert_eq!(
            final_assistant(&events).0,
            StopReason::Stop,
            "{hook}, advisory"
        );

        // Required: the run fails, naming the extension.
        let (agent, _) = scripted(vec![text("done")]);
        let mut r = agent.with_extension(
            ext(
                "p",
                Hooks {
                    panic_in: Some(hook),
                    ..Default::default()
                },
            )
            .required(),
        );
        let events = run(&mut r, "go").await;
        let (stop, error) = final_assistant(&events);
        assert_eq!(stop, StopReason::Error, "{hook}, required");
        assert!(
            error.unwrap().starts_with(EXTENSION_FAILED_PREFIX),
            "{hook}"
        );
    }
}

#[tokio::test]
async fn before_tool_denies_modifies_and_a_panic_denies() {
    // Deny.
    let (agent, _) = scripted(vec![
        call("echo", serde_json::json!({"text": "rm -rf"})),
        text("done"),
    ]);
    let mut a = agent.with_extension(ext(
        "policy",
        Hooks {
            deny: Some("rm"),
            ..Default::default()
        },
    ));
    let events = run(&mut a, "go").await;
    let results = tool_results(&events);
    assert_eq!(results.len(), 1);
    assert!(
        results[0].1 && results[0].0.contains("rm is not allowed"),
        "{results:?}"
    );

    // Modify.
    let (agent, _) = scripted(vec![
        call("echo", serde_json::json!({"text": "a"})),
        text("done"),
    ]);
    let mut m = agent.with_extension(ext(
        "rewrite",
        Hooks {
            modify: Some(serde_json::json!({"text": "b"})),
            ..Default::default()
        },
    ));
    let events = run(&mut m, "go").await;
    assert_eq!(tool_results(&events), vec![("echo b".into(), false)]);

    // A panic denies (fail closed).
    let (agent, _) = scripted(vec![
        call("echo", serde_json::json!({"text": "a"})),
        text("done"),
    ]);
    let mut p = agent.with_extension(ext(
        "broken",
        Hooks {
            panic_in: Some("before_tool"),
            ..Default::default()
        },
    ));
    let events = run(&mut p, "go").await;
    let results = tool_results(&events);
    assert!(
        results[0].1 && results[0].0.contains("broken"),
        "{results:?}"
    );
}

/// A policy that judged a call is asked again when a later extension rewrote
/// the arguments, so the rewrite cannot slip a forbidden call past it.
struct Policy;

#[async_trait::async_trait]
impl Extension for Policy {
    fn name(&self) -> &str {
        "policy"
    }
    fn rechecks_modified_calls(&self) -> bool {
        true
    }
    async fn start_run(&self, _: &RunContext<'_>) -> Result<Box<dyn RunHooks>, ExtensionError> {
        Ok(Box::new(Hooks {
            deny: Some("secret"),
            ..Default::default()
        }))
    }
}

#[tokio::test]
async fn a_rewrite_is_judged_again_by_a_rechecking_policy() {
    let (agent, _) = scripted(vec![
        call("echo", serde_json::json!({"text": "fine"})),
        text("done"),
    ]);
    let mut agent = agent.with_extension(Policy).with_extension(ext(
        "sneaky",
        Hooks {
            modify: Some(serde_json::json!({"text": "secret"})),
            ..Default::default()
        },
    ));
    let events = run(&mut agent, "go").await;
    let results = tool_results(&events);
    assert!(
        results[0].1 && results[0].0.contains("secret is not allowed"),
        "{results:?}"
    );
}

#[tokio::test]
async fn after_tool_edits_the_result_everywhere_and_withholds_partial_output() {
    struct Redactor;
    #[async_trait::async_trait]
    impl Extension for Redactor {
        fn name(&self) -> &str {
            "redact"
        }
        fn filters_tool_output(&self) -> bool {
            true
        }
        async fn start_run(&self, _: &RunContext<'_>) -> Result<Box<dyn RunHooks>, ExtensionError> {
            Ok(Box::new(Hooks {
                redact: Some(("hunter2", "[redacted]")),
                ..Default::default()
            }))
        }
    }
    let (agent, requests) = scripted(vec![
        call("echo", serde_json::json!({"text": "hunter2"})),
        text("done"),
    ]);
    let mut agent = agent.with_extension(Redactor);
    let events = run(&mut agent, "go").await;

    // No partial output was sent, and the final result is redacted.
    assert!(!events.iter().any(|e| matches!(
        e,
        AgentEvent::ToolExecutionUpdate { .. } | AgentEvent::ProgressMessage { .. }
    )));
    assert_eq!(
        tool_results(&events),
        vec![("echo [redacted]".into(), false)]
    );
    // The history and the next request got the redacted result too. (The
    // tool's arguments are not filtered: the model wrote them.)
    let history = format!("{:?}", end_messages(&events));
    let results_only: Vec<_> = end_messages(&events)
        .into_iter()
        .filter(|m| m.role() == "toolResult")
        .collect();
    assert!(
        !format!("{results_only:?}").contains("hunter2"),
        "{history}"
    );
    let second = format!("{:?}", requests.lock().unwrap()[1]);
    assert!(second.contains("echo [redacted]"));
}

#[tokio::test]
async fn without_a_filtering_extension_partial_output_is_sent() {
    let (agent, _) = scripted(vec![
        call("echo", serde_json::json!({"text": "x"})),
        text("done"),
    ]);
    let mut agent = agent.with_extension(ext("plain", Hooks::default()));
    let events = run(&mut agent, "go").await;
    assert!(events
        .iter()
        .any(|e| matches!(e, AgentEvent::ToolExecutionUpdate { .. })));
    assert!(events
        .iter()
        .any(|e| matches!(e, AgentEvent::ProgressMessage { .. })));
}

#[tokio::test]
async fn a_failing_after_tool_withholds_the_result_and_a_required_one_fails_the_run() {
    let (agent, _) = scripted(vec![
        call("echo", serde_json::json!({"text": "x"})),
        text("done"),
    ]);
    let mut a = agent.with_extension(ext(
        "post",
        Hooks {
            fail_after_tool: true,
            ..Default::default()
        },
    ));
    let events = run(&mut a, "go").await;
    let results = tool_results(&events);
    assert!(
        results[0].1 && results[0].0.contains("withheld"),
        "{results:?}"
    );
    assert_eq!(
        final_assistant(&events).0,
        StopReason::Stop,
        "advisory: the run goes on"
    );

    let (agent, requests) = scripted(vec![
        call("echo", serde_json::json!({"text": "x"})),
        text("done"),
    ]);
    let mut r = agent.with_extension(
        ext(
            "post",
            Hooks {
                fail_after_tool: true,
                ..Default::default()
            },
        )
        .required(),
    );
    let events = run(&mut r, "go").await;
    let (stop, error) = final_assistant(&events);
    assert_eq!(stop, StopReason::Error);
    assert!(error.unwrap().contains("after_tool failed"));
    assert_eq!(requests.lock().unwrap().len(), 1, "no second request");
    assert!(turns_paired(&events));
}

#[tokio::test]
async fn on_stop_continues_with_a_loop_injected_message_up_to_the_cap() {
    let log = Arc::new(Mutex::new(Vec::new()));
    let (agent, requests) = scripted(vec![text("a"), text("b"), text("c")]);
    let mut agent = agent.with_max_stop_continues(2).with_extension(ext(
        "verify",
        Hooks {
            continue_with: Some("check again"),
            log: Some(log.clone()),
            ..Default::default()
        },
    ));
    let events = run(&mut agent, "go").await;
    // Two continues, then the cap: three requests, and the run ends
    // normally (advisory).
    assert_eq!(requests.lock().unwrap().len(), 3);
    let second = &requests.lock().unwrap()[1];
    let injected = last_user_text(second);
    assert_eq!(
        injected,
        format!("{EXTENSION_MESSAGE_PREFIX}verify] check again")
    );
    assert!(is_loop_injected(&injected));
    assert_eq!(final_assistant(&events).0, StopReason::Stop);
    let stops: Vec<_> = log
        .lock()
        .unwrap()
        .iter()
        .filter(|l| l.starts_with("on_stop"))
        .cloned()
        .collect();
    assert_eq!(
        stops,
        vec![
            "on_stop continues=0",
            "on_stop continues=1",
            "on_stop continues=2"
        ]
    );
    assert!(turns_paired(&events));
}

#[tokio::test]
async fn a_required_verifier_that_never_accepts_fails_the_run() {
    let (agent, _) = scripted(vec![text("a"), text("b")]);
    let mut agent = agent.with_max_stop_continues(1).with_extension(
        ext(
            "verify",
            Hooks {
                continue_with: Some("check again"),
                ..Default::default()
            },
        )
        .required(),
    );
    let events = run(&mut agent, "go").await;
    let (stop, error) = final_assistant(&events);
    assert_eq!(stop, StopReason::Error);
    assert!(error
        .unwrap()
        .contains("still not accepted after 1 continues"));
}

#[tokio::test]
async fn on_stop_fail_fails_a_required_run() {
    let (agent, _) = scripted(vec![text("wrong")]);
    let mut agent = agent.with_extension(
        ext(
            "verify",
            Hooks {
                fail_stop: Some("the answer is wrong"),
                ..Default::default()
            },
        )
        .required(),
    );
    let events = run(&mut agent, "go").await;
    let (stop, error) = final_assistant(&events);
    assert_eq!(stop, StopReason::Error);
    assert!(error.unwrap().contains("the answer is wrong"));
}

#[tokio::test]
async fn on_event_sees_every_event_before_the_consumer() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let (agent, _) = scripted(vec![
        call("echo", serde_json::json!({"text": "x"})),
        text("done"),
    ]);
    let mut agent = agent.with_extension(ext(
        "audit",
        Hooks {
            audit: Some(seen.clone()),
            ..Default::default()
        },
    ));
    let events = run(&mut agent, "go").await;
    let consumer: Vec<String> = events.iter().map(|e| format!("{e:?}")).collect();
    assert_eq!(*seen.lock().unwrap(), consumer);
}

// ---------------------------------------------------------------------------
// Runs: isolation, finish, start failures, continue_loop
// ---------------------------------------------------------------------------

/// Counts turns per run in its hooks, and keeps every run's total.
struct Counter {
    runs: Arc<Mutex<Vec<usize>>>,
    started: AtomicUsize,
}

struct CounterHooks {
    turns: usize,
    runs: Arc<Mutex<Vec<usize>>>,
}

#[async_trait::async_trait]
impl RunHooks for CounterHooks {
    async fn before_model(&mut self, _: &TurnContext<'_>) -> TurnDecision {
        self.turns += 1;
        // Give a concurrent run the chance to interleave.
        tokio::task::yield_now().await;
        TurnDecision::Continue
    }
    async fn finish(&mut self, _: &RunOutcome) {
        self.runs.lock().unwrap().push(self.turns);
    }
}

#[async_trait::async_trait]
impl Extension for Counter {
    fn name(&self) -> &str {
        "counter"
    }
    async fn start_run(&self, _: &RunContext<'_>) -> Result<Box<dyn RunHooks>, ExtensionError> {
        self.started.fetch_add(1, Ordering::SeqCst);
        Ok(Box::new(CounterHooks {
            turns: 0,
            runs: self.runs.clone(),
        }))
    }
}

struct Shared(Arc<Counter>);

#[async_trait::async_trait]
impl Extension for Shared {
    fn name(&self) -> &str {
        self.0.name()
    }
    async fn start_run(&self, run: &RunContext<'_>) -> Result<Box<dyn RunHooks>, ExtensionError> {
        self.0.start_run(run).await
    }
}

#[tokio::test]
async fn each_run_gets_fresh_hooks_and_finish_runs_once_per_run() {
    let counter = Arc::new(Counter {
        runs: Arc::default(),
        started: AtomicUsize::new(0),
    });
    // Two agents sharing one extension, running concurrently: one run of two
    // turns, one of one.
    let (a, _) = scripted(vec![
        call("echo", serde_json::json!({"text": "x"})),
        text("done"),
    ]);
    let (b, _) = scripted(vec![text("done")]);
    let mut a = a.with_extension(Shared(counter.clone()));
    let mut b = b.with_extension(Shared(counter.clone()));
    let (ea, eb) = tokio::join!(run(&mut a, "one"), run(&mut b, "two"));
    assert!(matches!(ea.last(), Some(AgentEvent::AgentEnd { .. })));
    assert!(matches!(eb.last(), Some(AgentEvent::AgentEnd { .. })));
    // And a second, sequential run on `b`.
    let _ = run(&mut b, "three").await;

    assert_eq!(counter.started.load(Ordering::SeqCst), 3);
    let mut runs = counter.runs.lock().unwrap().clone();
    runs.sort();
    assert_eq!(runs, vec![1, 1, 2], "per-run state is isolated");
}

#[tokio::test]
async fn a_start_failure_sits_out_an_advisory_run_and_fails_a_required_one() {
    struct NoStart(ExtensionMode);
    #[async_trait::async_trait]
    impl Extension for NoStart {
        fn name(&self) -> &str {
            "nostart"
        }
        fn mode(&self) -> ExtensionMode {
            self.0
        }
        async fn start_run(&self, _: &RunContext<'_>) -> Result<Box<dyn RunHooks>, ExtensionError> {
            Err("backend down".into())
        }
    }
    let (agent, _) = scripted(vec![text("done")]);
    let mut a = agent.with_extension(NoStart(ExtensionMode::Advisory));
    let events = run(&mut a, "go").await;
    assert_eq!(final_assistant(&events).0, StopReason::Stop);

    let (agent, requests) = scripted(vec![text("done")]);
    let mut r = agent.with_extension(NoStart(ExtensionMode::Required));
    let events = run(&mut r, "go").await;
    let (stop, error) = final_assistant(&events);
    assert_eq!(stop, StopReason::Error);
    assert!(error.unwrap().contains("could not start: backend down"));
    assert!(requests.lock().unwrap().is_empty());
    assert!(turns_paired(&events));
}

#[tokio::test]
async fn continue_loop_runs_no_on_input() {
    let log = Arc::new(Mutex::new(Vec::new()));
    let (agent, _) = scripted(vec![text("done")]);
    let mut agent = agent
        .with_messages(vec![AgentMessage::Llm(Message::user("seeded"))])
        .with_extension(ext(
            "input",
            Hooks {
                log: Some(log.clone()),
                ..Default::default()
            },
        ));
    let (tx, _rx) = mpsc::unbounded_channel();
    agent.continue_loop_with_sender(tx).await;
    let log = log.lock().unwrap();
    assert!(!log.iter().any(|l| l.starts_with("on_input")), "{log:?}");
    assert!(log.iter().any(|l| l == "before_model"));
}

#[tokio::test]
async fn run_context_carries_the_label_prompts_and_depth() {
    struct Probe(Arc<Mutex<Vec<String>>>);
    #[async_trait::async_trait]
    impl Extension for Probe {
        fn name(&self) -> &str {
            "probe"
        }
        async fn start_run(
            &self,
            run: &RunContext<'_>,
        ) -> Result<Box<dyn RunHooks>, ExtensionError> {
            self.0.lock().unwrap().push(format!(
                "label={:?} prompts={} depth={} delegation={} has_id={}",
                run.label,
                run.prompts.len(),
                run.depth,
                run.is_delegation(),
                !run.run_id.is_empty()
            ));
            Ok(Box::new(Hooks::default()))
        }
    }
    let seen = Arc::new(Mutex::new(Vec::new()));
    let (agent, _) = scripted(vec![text("done")]);
    let mut agent = agent
        .with_run_label("session-7")
        .with_extension(Probe(seen.clone()));
    let _ = run(&mut agent, "go").await;
    assert_eq!(
        *seen.lock().unwrap(),
        vec!["label=Some(\"session-7\") prompts=1 depth=0 delegation=false has_id=true"]
    );
}

// ---------------------------------------------------------------------------
// Sub-agents: tree extensions
// ---------------------------------------------------------------------------

/// A sub-agent named `name` whose model calls `echo` with `text`, then
/// answers; optionally delegating once more to `child`.
fn sub_agent(name: &str, script: Vec<MockResponse>, child: Option<SubAgentTool>) -> SubAgentTool {
    let mut tools: Vec<Arc<dyn AgentTool>> = vec![Arc::new(Echo)];
    if let Some(child) = child {
        tools.push(Arc::new(child));
    }
    SubAgentTool::from_provider(
        name,
        Arc::new(MockProvider::new(script)),
        ModelConfig::mock(),
    )
    .with_tools(tools)
}

fn delegate(to: &str, task: &str) -> MockResponse {
    call(to, serde_json::json!({"task": task}))
}

#[tokio::test]
async fn a_tree_policy_reaches_children_and_grandchildren_but_an_ordinary_one_does_not() {
    let log = Arc::new(Mutex::new(Vec::new()));
    let grandchild = sub_agent(
        "grandchild",
        vec![
            call("echo", serde_json::json!({"text": "secret 3"})),
            text("gc done"),
        ],
        None,
    );
    let child = sub_agent(
        "child",
        vec![
            call("echo", serde_json::json!({"text": "secret 2"})),
            delegate("grandchild", "go deeper"),
            text("child done"),
        ],
        Some(grandchild),
    );
    let (agent, _) = scripted(vec![
        call("echo", serde_json::json!({"text": "secret 1"})),
        delegate("child", "go"),
        text("parent done"),
    ]);
    let mut agent = agent
        .with_tools(vec![Box::new(Echo), Box::new(child)])
        .with_tree_extension(ext(
            "policy",
            Hooks {
                deny: Some("secret"),
                ..Default::default()
            },
        ))
        .with_extension(ext(
            "parent-only",
            Hooks {
                log: Some(log.clone()),
                ..Default::default()
            },
        ));
    let events = run(&mut agent, "start").await;

    // The parent's own call was denied.
    let parent_results = tool_results(&events);
    assert!(parent_results[0].1 && parent_results[0].0.contains("secret is not allowed"));
    // The child (and through it the grandchild) ran to the end; the denials
    // at those depths are checked in `tree_policy_judges_every_depth_...`.
    let child_result = &parent_results[1];
    assert!(
        !child_result.1,
        "the delegation itself succeeded: {child_result:?}"
    );
    assert!(child_result.0.contains("child done"), "{child_result:?}");

    // The ordinary extension saw only the parent's calls, and not the denied
    // one: the tree policy runs first and its denial ends the chain.
    let before_tool: Vec<_> = log
        .lock()
        .unwrap()
        .iter()
        .filter(|l| l.starts_with("before_tool"))
        .cloned()
        .collect();
    assert_eq!(before_tool, vec![r#"before_tool child {"task":"go"}"#]);
}

/// Records, per run depth, what the tree policy decided.
struct DepthPolicy(Arc<Mutex<Vec<(usize, String)>>>);

struct DepthHooks {
    depth: usize,
    seen: Arc<Mutex<Vec<(usize, String)>>>,
}

#[async_trait::async_trait]
impl RunHooks for DepthHooks {
    async fn tools(&mut self, _: &RunContext<'_>) -> Vec<Arc<dyn AgentTool>> {
        vec![Arc::new(Named("policy_tool"))]
    }
    async fn before_tool(&self, call: &ToolCallRequest<'_>) -> ToolDecision {
        self.seen
            .lock()
            .unwrap()
            .push((self.depth, format!("{} {}", call.tool_name, call.args)));
        if call.args.to_string().contains("secret") {
            ToolDecision::Deny("secret is not allowed".into())
        } else {
            ToolDecision::Allow
        }
    }
}

#[async_trait::async_trait]
impl Extension for DepthPolicy {
    fn name(&self) -> &str {
        "depth-policy"
    }
    async fn start_run(&self, run: &RunContext<'_>) -> Result<Box<dyn RunHooks>, ExtensionError> {
        Ok(Box::new(DepthHooks {
            depth: run.depth,
            seen: self.0.clone(),
        }))
    }
}

#[tokio::test]
async fn tree_policy_judges_every_depth_and_its_tools_stay_with_the_parent() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let grandchild = sub_agent(
        "grandchild",
        vec![
            call("echo", serde_json::json!({"text": "secret 3"})),
            call("policy_tool", serde_json::json!({})),
            text("gc done"),
        ],
        None,
    );
    let child = sub_agent(
        "child",
        vec![
            call("echo", serde_json::json!({"text": "secret 2"})),
            delegate("grandchild", "deeper"),
            text("child done"),
        ],
        Some(grandchild),
    );
    let (agent, _) = scripted(vec![delegate("child", "go"), text("parent done")]);
    let mut agent = agent
        .with_tools(vec![Box::new(Echo), Box::new(child)])
        .with_tree_extension(DepthPolicy(seen.clone()));
    let _ = run(&mut agent, "start").await;

    let seen = seen.lock().unwrap().clone();
    let depths: Vec<usize> = seen.iter().map(|(d, _)| *d).collect();
    assert_eq!(depths, vec![0, 1, 1, 2, 2], "{seen:?}");
    // The grandchild's call to the policy's own tool reached the policy,
    // which shows the tool was not offered there (the model asked for a tool
    // it did not have; the loop answers "not found" after the policy).
    assert!(seen
        .iter()
        .any(|(d, c)| *d == 2 && c.starts_with("policy_tool")));
}

/// A budget shared across the tree: every run's hooks count turns into the
/// extension's total, and stop the run past the limit.
struct TreeBudget {
    limit: usize,
    spent: Arc<AtomicUsize>,
}

struct BudgetHooks {
    limit: usize,
    spent: Arc<AtomicUsize>,
}

#[async_trait::async_trait]
impl RunHooks for BudgetHooks {
    async fn before_model(&mut self, _: &TurnContext<'_>) -> TurnDecision {
        if self.spent.fetch_add(1, Ordering::SeqCst) >= self.limit {
            TurnDecision::Stop("tree budget spent".into())
        } else {
            TurnDecision::Continue
        }
    }
}

#[async_trait::async_trait]
impl Extension for TreeBudget {
    fn name(&self) -> &str {
        "budget"
    }
    async fn start_run(&self, _: &RunContext<'_>) -> Result<Box<dyn RunHooks>, ExtensionError> {
        Ok(Box::new(BudgetHooks {
            limit: self.limit,
            spent: self.spent.clone(),
        }))
    }
}

#[tokio::test]
async fn a_tree_budget_stops_a_child_whose_spend_crosses_the_total() {
    let spent = Arc::new(AtomicUsize::new(0));
    let child = sub_agent(
        "child",
        vec![
            call("echo", serde_json::json!({"text": "1"})),
            call("echo", serde_json::json!({"text": "2"})),
            call("echo", serde_json::json!({"text": "3"})),
            text("child done"),
        ],
        None,
    );
    let (agent, _) = scripted(vec![delegate("child", "work"), text("parent done")]);
    let mut agent = agent
        .with_tools(vec![Box::new(Echo), Box::new(child)])
        .with_tree_extension(TreeBudget {
            limit: 3,
            spent: spent.clone(),
        });
    let events = run(&mut agent, "start").await;
    // Parent turn 1 + child turns 1 and 2 = 3; the child's third turn is
    // stopped by the shared total, and the parent's next turn too.
    let results = tool_results(&events);
    assert!(
        results[0].0.contains("tree budget spent"),
        "the child stopped on the shared budget: {results:?}"
    );
    let last = end_messages(&events).last().cloned().unwrap();
    assert!(format!("{last:?}").contains("[Agent stopped: tree budget spent]"));
}

// ---------------------------------------------------------------------------
// Review follow-ups: failures are never lost, finish, transcripts, children
// ---------------------------------------------------------------------------

fn assert_failed_by(events: &[AgentEvent], name: &str, reason: &str) {
    let (stop, error) = final_assistant(events);
    assert_eq!(stop, StopReason::Error);
    let error = error.unwrap();
    assert!(
        error.starts_with(&format!("{EXTENSION_FAILED_PREFIX} {name}]")) && error.contains(reason),
        "{error}"
    );
    assert!(turns_paired(events));
}

#[tokio::test]
async fn a_required_failure_is_not_lost_when_a_limit_ends_the_run() {
    let (agent, _) = scripted(vec![
        call("echo", serde_json::json!({"text": "x"})),
        text("done"),
    ]);
    let mut agent = agent
        .with_execution_limits(yoagent::context::ExecutionLimits::default().with_max_turns(1))
        .with_extension(
            ext(
                "post",
                Hooks {
                    fail_after_tool: true,
                    ..Default::default()
                },
            )
            .required(),
        );
    let events = run(&mut agent, "go").await;
    assert_failed_by(&events, "post", "after_tool failed");
}

/// A required audit that panics observing `AgentStart`, the run's first
/// event.
#[allow(non_snake_case)]
fn PanickyAudit() -> ClonedHooks<Hooks> {
    ext(
        "audit",
        Hooks {
            panic_on_event: Some(|e| matches!(e, AgentEvent::AgentStart)),
            ..Default::default()
        },
    )
    .required()
}

#[tokio::test]
async fn a_required_on_event_failure_fails_the_run_deterministically() {
    for _ in 0..20 {
        let (agent, requests) = scripted(vec![text("never")]);
        let mut agent = agent.with_extension(PanickyAudit());
        let events = run(&mut agent, "go").await;
        assert_failed_by(&events, "audit", "on_event failed");
        assert!(
            requests.lock().unwrap().is_empty(),
            "no request after the failure"
        );
    }
}

#[tokio::test]
async fn a_required_verifier_is_not_outvoted_by_an_advisory_continue() {
    let (agent, _) = scripted(vec![text("a"), text("b")]);
    let mut agent = agent
        .with_max_stop_continues(1)
        .with_extension(ext(
            "nudge",
            Hooks {
                continue_with: Some("try harder"),
                ..Default::default()
            },
        ))
        .with_extension(
            ext(
                "verify",
                Hooks {
                    continue_with: Some("not verified"),
                    ..Default::default()
                },
            )
            .required(),
        );
    let events = run(&mut agent, "go").await;
    assert_failed_by(&events, "verify", "still not accepted");
}

#[tokio::test]
async fn a_redactor_that_cannot_start_fails_the_run_whatever_its_mode() {
    struct BrokenRedactor;
    #[async_trait::async_trait]
    impl Extension for BrokenRedactor {
        fn name(&self) -> &str {
            "redact"
        }
        fn filters_tool_output(&self) -> bool {
            true
        }
        async fn start_run(&self, _: &RunContext<'_>) -> Result<Box<dyn RunHooks>, ExtensionError> {
            Err("no key".into())
        }
    }
    let (agent, requests) = scripted(vec![
        call("echo", serde_json::json!({"text": "hunter2"})),
        text("done"),
    ]);
    let mut agent = agent.with_extension(BrokenRedactor);
    let events = run(&mut agent, "go").await;
    assert_failed_by(&events, "redact", "could not start: no key");
    assert!(requests.lock().unwrap().is_empty());
}

/// Records each `finish` outcome, and can cancel the run from `before_tool`.
#[derive(Clone)]
struct Outcomes {
    seen: Arc<Mutex<Vec<String>>>,
    cancel_in_before_tool: bool,
}

struct OutcomeHooks {
    seen: Arc<Mutex<Vec<String>>>,
    cancel: Option<CancellationToken>,
}

#[async_trait::async_trait]
impl RunHooks for OutcomeHooks {
    async fn before_tool(&self, _: &ToolCallRequest<'_>) -> ToolDecision {
        if let Some(cancel) = &self.cancel {
            cancel.cancel();
        }
        ToolDecision::Allow
    }
    async fn finish(&mut self, outcome: &RunOutcome) {
        let end = match outcome.end() {
            RunEnd::Failed { extension, .. } => format!("Failed by {extension:?}"),
            RunEnd::Stopped { reason } => format!("Stopped {reason:?}"),
            other => format!("{other:?}"),
        };
        self.seen
            .lock()
            .unwrap()
            .push(format!("{end} stop={:?}", outcome.stop_reason()));
    }
}

#[async_trait::async_trait]
impl Extension for Outcomes {
    fn name(&self) -> &str {
        "outcomes"
    }
    async fn start_run(&self, run: &RunContext<'_>) -> Result<Box<dyn RunHooks>, ExtensionError> {
        Ok(Box::new(OutcomeHooks {
            seen: self.seen.clone(),
            cancel: self.cancel_in_before_tool.then(|| run.cancel.clone()),
        }))
    }
}

#[tokio::test]
async fn finish_reports_each_ending_once() {
    async fn finished(agent: Agent, cancel: bool, prompt: &str) -> Vec<String> {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let mut agent = agent.with_extension(Outcomes {
            seen: seen.clone(),
            cancel_in_before_tool: cancel,
        });
        let _ = run(&mut agent, prompt).await;
        let seen = seen.lock().unwrap().clone();
        seen
    }
    // Completed.
    let (a, _) = scripted(vec![text("done")]);
    assert_eq!(
        finished(a, false, "go").await,
        vec!["Completed stop=Some(Stop)"]
    );
    // Stopped by another extension's before_model.
    let (a, _) = scripted(vec![text("never")]);
    let a = a.with_extension(ext(
        "budget",
        Hooks {
            stop: Some("spent"),
            ..Default::default()
        },
    ));
    assert_eq!(
        finished(a, false, "go").await,
        vec![r#"Stopped "[Agent stopped: spent]" stop=None"#]
    );
    // Failed by a required extension.
    let (a, _) = scripted(vec![text("never")]);
    let a = a.with_extension(
        ext(
            "verify",
            Hooks {
                fail_model: Some("broken"),
                ..Default::default()
            },
        )
        .required(),
    );
    assert_eq!(
        finished(a, false, "go").await,
        vec![r#"Failed by Some("verify") stop=Some(Error)"#]
    );
    // Cancelled while its tool call was being judged: the call does not run.
    let (a, _) = scripted(vec![
        call("echo", serde_json::json!({"text": "x"})),
        text("never"),
    ]);
    assert_eq!(
        finished(a, true, "go").await,
        vec!["Cancelled stop=Some(ToolUse)"]
    );
    // Stopped by a turn limit.
    let (a, _) = scripted(vec![
        call("echo", serde_json::json!({"text": "x"})),
        text("never"),
    ]);
    let a = a.with_execution_limits(yoagent::context::ExecutionLimits::default().with_max_turns(1));
    assert_eq!(
        finished(a, false, "go").await,
        vec![r#"Stopped "[Agent stopped: Max turns reached (1/1)]" stop=Some(ToolUse)"#]
    );
}

/// Stops the first run's second turn; later runs are left alone.
struct StopOnce(AtomicUsize);

struct StopOnceHooks {
    run: usize,
    turns: usize,
}

#[async_trait::async_trait]
impl RunHooks for StopOnceHooks {
    async fn before_model(&mut self, _: &TurnContext<'_>) -> TurnDecision {
        self.turns += 1;
        if self.run == 0 && self.turns == 2 {
            TurnDecision::Stop("budget spent".into())
        } else {
            TurnDecision::Continue
        }
    }
}

#[async_trait::async_trait]
impl Extension for StopOnce {
    fn name(&self) -> &str {
        "stop-once"
    }
    async fn start_run(&self, _: &RunContext<'_>) -> Result<Box<dyn RunHooks>, ExtensionError> {
        Ok(Box::new(StopOnceHooks {
            run: self.0.fetch_add(1, Ordering::SeqCst),
            turns: 0,
        }))
    }
}

#[tokio::test]
async fn a_stop_after_a_tool_turn_keeps_the_transcript_valid_for_the_next_prompt() {
    // MockProvider rejects (panics on) a transcript a real provider would.
    let (agent, requests) = scripted(vec![
        call("echo", serde_json::json!({"text": "x"})),
        text("second prompt answered"),
    ]);
    let mut agent = agent.with_extension(StopOnce(AtomicUsize::new(0)));
    let first = run(&mut agent, "one").await;
    assert!(turns_paired(&first));
    let second = run(&mut agent, "two").await;
    assert_eq!(final_assistant(&second).0, StopReason::Stop);
    assert_eq!(requests.lock().unwrap().len(), 2);
}

type Seen<T> = Arc<Mutex<Vec<T>>>;

/// A sub-agent on a recording provider.
fn recorded_sub_agent(
    name: &str,
    script: Vec<MockResponse>,
    tools: Vec<Arc<dyn AgentTool>>,
) -> (SubAgentTool, Seen<Vec<String>>, Seen<Vec<Message>>) {
    struct Both {
        inner: MockProvider,
        tools: Arc<Mutex<Vec<Vec<String>>>>,
        requests: Arc<Mutex<Vec<Vec<Message>>>>,
    }
    #[async_trait::async_trait]
    impl StreamProvider for Both {
        async fn stream(
            &self,
            config: StreamConfig,
            tx: mpsc::UnboundedSender<StreamEvent>,
            cancel: CancellationToken,
        ) -> Result<Message, ProviderError> {
            self.tools
                .lock()
                .unwrap()
                .push(config.tools.iter().map(|t| t.name.clone()).collect());
            self.requests.lock().unwrap().push(config.messages.clone());
            self.inner.stream(config, tx, cancel).await
        }
    }
    let seen_tools = Arc::new(Mutex::new(Vec::new()));
    let seen_requests = Arc::new(Mutex::new(Vec::new()));
    let provider = Both {
        inner: MockProvider::new(script),
        tools: seen_tools.clone(),
        requests: seen_requests.clone(),
    };
    let tool = SubAgentTool::from_provider(name, Arc::new(provider), ModelConfig::mock())
        .with_tools(tools);
    (tool, seen_tools, seen_requests)
}

#[tokio::test]
async fn a_tree_policy_denial_is_honoured_in_the_child_and_its_tools_stay_with_the_parent() {
    let (child, child_tools, child_requests) = recorded_sub_agent(
        "child",
        vec![
            call("echo", serde_json::json!({"text": "secret"})),
            text("child done"),
        ],
        vec![Arc::new(Echo)],
    );
    let (agent, parent_requests) = scripted(vec![delegate("child", "go"), text("parent done")]);
    let mut agent = agent
        .with_tools(vec![Box::new(Echo), Box::new(child)])
        .with_tree_extension(DepthPolicy(Arc::default()));
    let _ = run(&mut agent, "start").await;

    // The child's call was denied: its next request carries the denial.
    let child_requests = child_requests.lock().unwrap();
    let after_call = format!("{:?}", child_requests[1]);
    assert!(after_call.contains("secret is not allowed"), "{after_call}");
    // The policy's own tool was offered to the parent, never to the child.
    drop(parent_requests);
    for tools in child_tools.lock().unwrap().iter() {
        assert!(!tools.contains(&"policy_tool".to_string()), "{tools:?}");
    }
}

#[tokio::test]
async fn a_required_failure_in_a_child_fails_the_delegation() {
    let (child, _, _) = recorded_sub_agent("child", vec![text("never")], vec![]);
    let child = child.with_extension(
        ext(
            "verify",
            Hooks {
                fail_model: Some("cannot verify"),
                ..Default::default()
            },
        )
        .required(),
    );
    let (agent, _) = scripted(vec![delegate("child", "go"), text("parent done")]);
    let mut agent = agent.with_tools(vec![Box::new(child)]);
    let events = run(&mut agent, "start").await;
    let results = tool_results(&events);
    assert!(results[0].1, "the delegation failed: {results:?}");
    assert!(
        results[0].0.contains(EXTENSION_FAILED_PREFIX),
        "{results:?}"
    );
}

#[tokio::test]
async fn a_custom_delegation_tool_sees_the_tree_extensions_depth_and_label() {
    struct Delegator(Arc<Mutex<Vec<String>>>);
    #[async_trait::async_trait]
    impl AgentTool for Delegator {
        fn name(&self) -> &str {
            "delegate"
        }
        fn label(&self) -> &str {
            "delegate"
        }
        fn description(&self) -> &str {
            "delegate"
        }
        fn parameters_schema(&self) -> serde_json::Value {
            serde_json::json!({"type": "object", "properties": {}})
        }
        async fn execute(
            &self,
            _: serde_json::Value,
            ctx: ToolContext,
        ) -> Result<ToolResult, ToolError> {
            self.0.lock().unwrap().push(format!(
                "tree={} depth={} label={:?}",
                ctx.tree_extensions()
                    .iter()
                    .map(|e| e.name().to_string())
                    .collect::<Vec<_>>()
                    .join(","),
                ctx.delegation_depth(),
                ctx.run_label()
            ));
            Ok(ToolResult {
                content: vec![Content::Text { text: "ok".into() }],
                details: serde_json::Value::Null,
            })
        }
    }
    let seen = Arc::new(Mutex::new(Vec::new()));
    let (agent, _) = scripted(vec![call("delegate", serde_json::json!({})), text("done")]);
    let mut agent = agent
        .with_tools(vec![Box::new(Delegator(seen.clone()))])
        .with_run_label("session-9")
        .with_tree_extension(ext("policy", Hooks::default()))
        .with_extension(ext("local", Hooks::default()));
    let _ = run(&mut agent, "go").await;
    assert_eq!(
        *seen.lock().unwrap(),
        vec![r#"tree=policy depth=1 label=Some("session-9")"#]
    );
}

#[tokio::test]
async fn runs_sharing_an_extension_really_run_concurrently_and_stay_isolated() {
    // Each run's first `before_model` waits for the other run to get there:
    // this deadlocks (and times out) unless both runs are in flight at once.
    struct Rendezvous {
        barrier: Arc<tokio::sync::Barrier>,
        runs: Arc<Mutex<Vec<usize>>>,
    }
    struct RendezvousHooks {
        barrier: Option<Arc<tokio::sync::Barrier>>,
        turns: usize,
        runs: Arc<Mutex<Vec<usize>>>,
    }
    #[async_trait::async_trait]
    impl RunHooks for RendezvousHooks {
        async fn before_model(&mut self, _: &TurnContext<'_>) -> TurnDecision {
            if let Some(barrier) = self.barrier.take() {
                barrier.wait().await;
            }
            self.turns += 1;
            TurnDecision::Continue
        }
        async fn finish(&mut self, _: &RunOutcome) {
            self.runs.lock().unwrap().push(self.turns);
        }
    }
    #[async_trait::async_trait]
    impl Extension for Rendezvous {
        fn name(&self) -> &str {
            "rendezvous"
        }
        async fn start_run(&self, _: &RunContext<'_>) -> Result<Box<dyn RunHooks>, ExtensionError> {
            Ok(Box::new(RendezvousHooks {
                barrier: Some(self.barrier.clone()),
                turns: 0,
                runs: self.runs.clone(),
            }))
        }
    }
    let shared = Arc::new(Rendezvous {
        barrier: Arc::new(tokio::sync::Barrier::new(2)),
        runs: Arc::default(),
    });
    struct ByRef(Arc<Rendezvous>);
    #[async_trait::async_trait]
    impl Extension for ByRef {
        fn name(&self) -> &str {
            "rendezvous"
        }
        async fn start_run(
            &self,
            run: &RunContext<'_>,
        ) -> Result<Box<dyn RunHooks>, ExtensionError> {
            self.0.start_run(run).await
        }
    }
    let (a, _) = scripted(vec![
        call("echo", serde_json::json!({"text": "x"})),
        text("done"),
    ]);
    let (b, _) = scripted(vec![text("done")]);
    let mut a = a.with_extension(ByRef(shared.clone()));
    let mut b = b.with_extension(ByRef(shared.clone()));
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        tokio::join!(run(&mut a, "one"), run(&mut b, "two"))
    })
    .await
    .expect("both runs were in flight together");
    let mut runs = shared.runs.lock().unwrap().clone();
    runs.sort();
    assert_eq!(runs, vec![1, 2]);
}

#[tokio::test]
async fn after_tool_sees_whether_the_call_failed() {
    struct Failing;
    #[async_trait::async_trait]
    impl AgentTool for Failing {
        fn name(&self) -> &str {
            "failing"
        }
        fn label(&self) -> &str {
            "failing"
        }
        fn description(&self) -> &str {
            "fails"
        }
        fn parameters_schema(&self) -> serde_json::Value {
            serde_json::json!({"type": "object", "properties": {}})
        }
        async fn execute(
            &self,
            _: serde_json::Value,
            _: ToolContext,
        ) -> Result<ToolResult, ToolError> {
            Err(ToolError::Failed("disk full".into()))
        }
    }
    #[derive(Clone)]
    struct SeesErrors(Arc<Mutex<Vec<bool>>>);
    #[async_trait::async_trait]
    impl RunHooks for SeesErrors {
        async fn after_tool(
            &self,
            _: &ToolCallRequest<'_>,
            output: &mut ToolOutput,
        ) -> Result<(), ExtensionError> {
            self.0.lock().unwrap().push(output.is_error);
            Ok(())
        }
    }
    let seen = Arc::new(Mutex::new(Vec::new()));
    let (agent, _) = scripted(vec![
        call("failing", serde_json::json!({})),
        call("echo", serde_json::json!({"text": "x"})),
        text("done"),
    ]);
    let mut agent = agent
        .with_tools(vec![Box::new(Echo), Box::new(Failing)])
        .with_extension(ClonedHooks::new("errors", SeesErrors(seen.clone())));
    let _ = run(&mut agent, "go").await;
    assert_eq!(*seen.lock().unwrap(), vec![true, false]);
}

/// A required sink that panics observing every `MessageEnd`: including the
/// events of the failure report itself.
#[allow(non_snake_case)]
fn BrokenSink() -> ClonedHooks<Hooks> {
    ext(
        "sink",
        Hooks {
            panic_on_event: Some(|e| matches!(e, AgentEvent::MessageEnd { .. })),
            ..Default::default()
        },
    )
    .required()
}

#[tokio::test]
async fn a_run_fails_once_even_when_its_failure_report_fails_again() {
    let errors = Arc::new(Mutex::new(Vec::new()));
    let seen = errors.clone();
    let (agent, _) = scripted(vec![text("done")]);
    let mut agent = agent
        .with_extension(BrokenSink())
        .on_error(move |e| seen.lock().unwrap().push(e.to_string()));
    let events = run(&mut agent, "go").await;
    assert_failed_by(&events, "sink", "on_event failed");
    let failures = end_messages(&events)
        .iter()
        .filter(|m| {
            matches!(m, AgentMessage::Llm(Message::Assistant { error_message: Some(e), .. })
                if e.starts_with(EXTENSION_FAILED_PREFIX))
        })
        .count();
    assert_eq!(failures, 1, "one failure message");
    assert_eq!(errors.lock().unwrap().len(), 1, "one on_error");
}

#[tokio::test]
async fn every_extension_that_continues_is_heard() {
    let (agent, requests) = scripted(vec![text("a"), text("b")]);
    let mut agent = agent
        .with_max_stop_continues(1)
        .with_extension(ext(
            "nudge",
            Hooks {
                continue_with: Some("try harder"),
                ..Default::default()
            },
        ))
        .with_extension(
            ext(
                "verify",
                Hooks {
                    continue_with: Some("tests still fail"),
                    ..Default::default()
                },
            )
            .required(),
        );
    let _ = run(&mut agent, "go").await;
    let injected = last_user_text(&requests.lock().unwrap()[1]);
    assert_eq!(
        injected,
        format!(
            "{p}nudge] try harder\n{p}verify] tests still fail",
            p = EXTENSION_MESSAGE_PREFIX
        )
    );
}

/// Parallel tool calls are judged concurrently: each `before_tool` waits for
/// the other, which deadlocks (and times out) if they ran one at a time.
#[tokio::test]
async fn parallel_calls_are_judged_concurrently() {
    #[derive(Clone)]
    struct Slow(Arc<tokio::sync::Barrier>);
    #[async_trait::async_trait]
    impl RunHooks for Slow {
        async fn before_tool(&self, _: &ToolCallRequest<'_>) -> ToolDecision {
            self.0.wait().await;
            ToolDecision::Allow
        }
    }
    let (agent, _) = scripted(vec![
        MockResponse::ToolCalls(vec![
            MockToolCall {
                name: "echo".into(),
                arguments: serde_json::json!({"text": "a"}),
                provider_metadata: None,
            },
            MockToolCall {
                name: "echo".into(),
                arguments: serde_json::json!({"text": "b"}),
                provider_metadata: None,
            },
        ]),
        text("done"),
    ]);
    let mut agent = agent.with_extension(ClonedHooks::new(
        "slow-policy",
        Slow(Arc::new(tokio::sync::Barrier::new(2))),
    ));
    let events = tokio::time::timeout(std::time::Duration::from_secs(5), run(&mut agent, "go"))
        .await
        .expect("both calls were judged at once");
    assert_eq!(tool_results(&events).len(), 2);
}

// ---------------------------------------------------------------------------
// Holistic review follow-ups
// ---------------------------------------------------------------------------

/// A delegation tool written by hand: runs an `Agent` as the child, made a
/// delegated run with `Agent::delegated_from`.
struct HandDelegator {
    child_tools: Seen<Vec<String>>,
    child_requests: Seen<Vec<Message>>,
}

#[async_trait::async_trait]
impl AgentTool for HandDelegator {
    fn name(&self) -> &str {
        "hand_delegate"
    }
    fn label(&self) -> &str {
        "hand_delegate"
    }
    fn description(&self) -> &str {
        "runs a child agent"
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({"type": "object", "properties": {}})
    }
    async fn execute(
        &self,
        _: serde_json::Value,
        ctx: ToolContext,
    ) -> Result<ToolResult, ToolError> {
        struct Both {
            inner: MockProvider,
            tools: Seen<Vec<String>>,
            requests: Seen<Vec<Message>>,
        }
        #[async_trait::async_trait]
        impl StreamProvider for Both {
            async fn stream(
                &self,
                config: StreamConfig,
                tx: mpsc::UnboundedSender<StreamEvent>,
                cancel: CancellationToken,
            ) -> Result<Message, ProviderError> {
                self.tools
                    .lock()
                    .unwrap()
                    .push(config.tools.iter().map(|t| t.name.clone()).collect());
                self.requests.lock().unwrap().push(config.messages.clone());
                self.inner.stream(config, tx, cancel).await
            }
        }
        let provider = Both {
            inner: MockProvider::new(vec![
                call("echo", serde_json::json!({"text": "secret"})),
                text("child done"),
            ]),
            tools: self.child_tools.clone(),
            requests: self.child_requests.clone(),
        };
        let mut child = Agent::from_provider(provider, ModelConfig::mock())
            .with_tools(vec![Box::new(Echo)])
            .delegated_from(&ctx);
        let (tx, _rx) = mpsc::unbounded_channel();
        child.prompt_with_sender("work", tx).await;
        Ok(ToolResult {
            content: vec![Content::Text {
                text: "delegated".into(),
            }],
            details: serde_json::Value::Null,
        })
    }
}

#[tokio::test]
async fn a_hand_written_delegation_tool_runs_its_child_under_the_tree() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let child_tools = Arc::new(Mutex::new(Vec::new()));
    let child_requests = Arc::new(Mutex::new(Vec::new()));
    let (agent, _) = scripted(vec![
        call("hand_delegate", serde_json::json!({})),
        text("done"),
    ]);
    // The tree policy is installed through an `Arc`, as a host sharing it
    // would.
    let policy: Arc<dyn Extension> = Arc::new(DepthPolicy(seen.clone()));
    let mut agent = agent
        .with_tools(vec![Box::new(HandDelegator {
            child_tools: child_tools.clone(),
            child_requests: child_requests.clone(),
        })])
        .with_tree_extension(policy);
    let _ = run(&mut agent, "start").await;

    // The child's call was judged at depth 1 and denied.
    let seen = seen.lock().unwrap().clone();
    assert!(
        seen.iter().any(|(d, c)| *d == 1 && c.contains("secret")),
        "{seen:?}"
    );
    let after_call = format!("{:?}", child_requests.lock().unwrap()[1]);
    assert!(after_call.contains("secret is not allowed"), "{after_call}");
    // The policy's own tool stayed with the parent.
    for tools in child_tools.lock().unwrap().iter() {
        assert!(!tools.contains(&"policy_tool".to_string()), "{tools:?}");
    }
}

#[tokio::test]
async fn a_required_failure_on_a_response_stops_its_tool_calls() {
    let (agent, _) = scripted(vec![
        call("echo", serde_json::json!({"text": "x"})),
        text("never"),
    ]);
    let mut agent = agent.with_extension(
        ext(
            "audit",
            Hooks {
                // Fails observing the tool-calling response itself.
                panic_on_event: Some(|e| {
                    matches!(
                        e,
                        AgentEvent::MessageEnd {
                            message: AgentMessage::Llm(Message::Assistant {
                                stop_reason: StopReason::ToolUse,
                                ..
                            })
                        }
                    )
                }),
                ..Default::default()
            },
        )
        .required(),
    );
    let events = run(&mut agent, "go").await;
    let results = tool_results(&events);
    assert_eq!(results.len(), 1);
    assert!(
        results[0].1 && results[0].0.contains("a required extension failed the run"),
        "the call did not run: {results:?}"
    );
    assert_failed_by(&events, "audit", "on_event failed");
}

#[tokio::test]
async fn a_required_after_tool_failure_stops_the_next_sequential_call() {
    let (agent, _) = scripted(vec![
        MockResponse::ToolCalls(vec![
            MockToolCall {
                name: "echo".into(),
                arguments: serde_json::json!({"text": "a"}),
                provider_metadata: None,
            },
            MockToolCall {
                name: "echo".into(),
                arguments: serde_json::json!({"text": "b"}),
                provider_metadata: None,
            },
        ]),
        text("never"),
    ]);
    let mut agent = agent
        .with_tool_execution(ToolExecutionStrategy::Sequential)
        .with_extension(
            ext(
                "post",
                Hooks {
                    fail_after_tool: true,
                    ..Default::default()
                },
            )
            .required(),
        );
    let events = run(&mut agent, "go").await;
    let results = tool_results(&events);
    assert!(results[0].0.contains("withheld"), "{results:?}");
    assert!(
        results[1].0.contains("a required extension failed the run"),
        "{results:?}"
    );
    assert_failed_by(&events, "post", "after_tool failed");
}

/// Cancels its run as soon as the run starts (from `tools`, which runs after
/// the first `TurnStart`).
struct CancelAtStart;

struct CancelAtStartHooks(CancellationToken);

#[async_trait::async_trait]
impl RunHooks for CancelAtStartHooks {
    async fn tools(&mut self, _: &RunContext<'_>) -> Vec<Arc<dyn AgentTool>> {
        self.0.cancel();
        Vec::new()
    }
}

#[async_trait::async_trait]
impl Extension for CancelAtStart {
    fn name(&self) -> &str {
        "cancel-at-start"
    }
    async fn start_run(&self, run: &RunContext<'_>) -> Result<Box<dyn RunHooks>, ExtensionError> {
        Ok(Box::new(CancelAtStartHooks(run.cancel.clone())))
    }
}

#[tokio::test]
async fn a_run_cancelled_before_its_first_turn_still_closes_it() {
    let (agent, requests) = scripted(vec![text("never")]);
    let mut agent = agent.with_extension(CancelAtStart);
    let events = run(&mut agent, "go").await;
    assert!(requests.lock().unwrap().is_empty());
    assert!(turns_paired(&events));
}

/// A `before_model` that never returns, and a run cancelled from outside.
#[tokio::test]
async fn a_hung_hook_does_not_hang_a_cancelled_run() {
    struct Hang(Arc<Mutex<Option<CancellationToken>>>);
    struct HangHooks;
    #[async_trait::async_trait]
    impl RunHooks for HangHooks {
        async fn before_model(&mut self, _: &TurnContext<'_>) -> TurnDecision {
            futures::future::pending::<()>().await;
            TurnDecision::Continue
        }
    }
    #[async_trait::async_trait]
    impl Extension for Hang {
        fn name(&self) -> &str {
            "hang"
        }
        async fn start_run(
            &self,
            run: &RunContext<'_>,
        ) -> Result<Box<dyn RunHooks>, ExtensionError> {
            *self.0.lock().unwrap() = Some(run.cancel.clone());
            Ok(Box::new(HangHooks))
        }
    }
    let token: Arc<Mutex<Option<CancellationToken>>> = Arc::new(Mutex::new(None));
    let canceller = token.clone();
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        if let Some(t) = canceller.lock().unwrap().as_ref() {
            t.cancel();
        }
    });
    let (agent, _) = scripted(vec![text("never")]);
    let mut agent = agent.with_extension(Hang(token));
    let events = tokio::time::timeout(std::time::Duration::from_secs(5), run(&mut agent, "go"))
        .await
        .expect("the cancel ended the run");
    assert_eq!(final_assistant(&events).0, StopReason::Aborted);
    assert!(turns_paired(&events));
}

#[test]
fn only_extension_messages_count_as_loop_injected() {
    assert!(is_loop_injected(&format!(
        "{EXTENSION_MESSAGE_PREFIX}verify] again"
    )));
    assert!(!is_loop_injected("[Extension foo] my own words"));
}

#[tokio::test]
async fn an_advisory_on_event_that_panics_is_not_called_again() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let (agent, _) = scripted(vec![text("done")]);
    let mut agent = agent.with_extension(ext(
        "flaky-audit",
        Hooks {
            audit: Some(seen.clone()),
            panic_on_event: Some(|_| true),
            ..Default::default()
        },
    ));
    let events = run(&mut agent, "go").await;
    assert_eq!(final_assistant(&events).0, StopReason::Stop);
    assert_eq!(
        seen.lock().unwrap().len(),
        1,
        "called once, then switched off"
    );
}

#[tokio::test]
async fn on_input_sees_the_input_without_the_filters_warnings() {
    struct Warns;
    impl InputFilter for Warns {
        fn filter(&self, _: &str) -> FilterResult {
            FilterResult::Warn("careful".into())
        }
    }
    let log = Arc::new(Mutex::new(Vec::new()));
    let (agent, _) = scripted(vec![text("done")]);
    let mut agent = agent.with_input_filter(Warns).with_extension(ext(
        "input",
        Hooks {
            log: Some(log.clone()),
            ..Default::default()
        },
    ));
    let _ = run(&mut agent, "hello").await;
    assert!(log.lock().unwrap().contains(&"on_input hello".to_string()));
}

#[tokio::test]
async fn cloned_hooks_can_declare_that_they_filter_tool_output() {
    let (agent, _) = scripted(vec![
        call("echo", serde_json::json!({"text": "hunter2"})),
        text("done"),
    ]);
    let mut agent = agent.with_extension(
        ext(
            "redact",
            Hooks {
                redact: Some(("hunter2", "[redacted]")),
                ..Default::default()
            },
        )
        .filters_tool_output(),
    );
    let events = run(&mut agent, "go").await;
    assert!(!events.iter().any(|e| matches!(
        e,
        AgentEvent::ToolExecutionUpdate { .. } | AgentEvent::ProgressMessage { .. }
    )));
    assert_eq!(
        tool_results(&events),
        vec![("echo [redacted]".into(), false)]
    );
}

#[tokio::test]
async fn a_tree_redactor_keeps_a_child_s_secret_from_the_parent_and_the_child() {
    let (child, _, child_requests) = recorded_sub_agent(
        "child",
        vec![
            call("echo", serde_json::json!({"text": "hunter2"})),
            text("child done"),
        ],
        vec![Arc::new(Echo)],
    );
    let (agent, _) = scripted(vec![delegate("child", "go"), text("parent done")]);
    let mut agent = agent.with_tools(vec![Box::new(child)]).with_tree_extension(
        ext(
            "redact",
            Hooks {
                redact: Some(("hunter2", "[redacted]")),
                ..Default::default()
            },
        )
        .filters_tool_output(),
    );
    let events = run(&mut agent, "start").await;
    // Nothing the parent's consumer saw carries the child's raw output.
    for e in &events {
        if let AgentEvent::ToolExecutionUpdate { .. } | AgentEvent::ToolExecutionEnd { .. } = e {
            assert!(!format!("{e:?}").contains("echo hunter2"), "{e:?}");
        }
    }
    // The child's own next request got the redacted result.
    let after_call = format!("{:?}", child_requests.lock().unwrap()[1]);
    assert!(after_call.contains("echo [redacted]"), "{after_call}");
    assert!(!after_call.contains("echo hunter2"), "{after_call}");
}

/// A tool whose output is long enough to be truncated and stashed, with a
/// secret in the part that is cut.
struct LongSecret;

#[async_trait::async_trait]
impl AgentTool for LongSecret {
    fn name(&self) -> &str {
        "long"
    }
    fn label(&self) -> &str {
        "long"
    }
    fn description(&self) -> &str {
        "a long output"
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({"type": "object", "properties": {}})
    }
    async fn execute(&self, _: serde_json::Value, _: ToolContext) -> Result<ToolResult, ToolError> {
        let text = (0..500)
            .map(|i| {
                if i == 250 {
                    "token hunter2".to_string()
                } else {
                    format!("line {i}")
                }
            })
            .collect::<Vec<_>>()
            .join("\n");
        Ok(ToolResult {
            content: vec![Content::Text { text }],
            details: serde_json::Value::Null,
        })
    }
}

#[tokio::test]
async fn the_output_store_gets_only_the_filtered_result() {
    let state = yoagent::shared_state::SharedState::new();
    let (agent, _) = scripted(vec![call("long", serde_json::json!({})), text("done")]);
    let mut agent = agent
        .with_tools(vec![Box::new(LongSecret)])
        .with_shared_state(state.clone())
        .with_context_config(yoagent::context::ContextConfig {
            tool_output_max_lines: 20,
            ..Default::default()
        })
        .with_extension(
            ext(
                "redact",
                Hooks {
                    redact: Some(("hunter2", "[redacted]")),
                    ..Default::default()
                },
            )
            .filters_tool_output(),
        );
    let _ = run(&mut agent, "go").await;
    let keys = state.keys().await;
    assert!(!keys.is_empty(), "the output was stashed");
    for key in keys {
        let value = state.get(&key).await.unwrap_or_default();
        assert!(
            !value.contains("hunter2"),
            "{key}: the store holds the secret"
        );
        if value.contains("line 250") || value.contains("token") {
            assert!(value.contains("[redacted]"));
        }
    }
}

#[tokio::test]
async fn required_panics_in_the_per_call_hooks_and_finish() {
    // on_input: rejects, in both modes.
    for required in [false, true] {
        let (agent, requests) = scripted(vec![text("never")]);
        let e = ext(
            "input",
            Hooks {
                panic_in: Some("on_input"),
                ..Default::default()
            },
        );
        let mut agent = agent.with_extension(if required { e.required() } else { e });
        let events = run(&mut agent, "go").await;
        assert!(events
            .iter()
            .any(|e| matches!(e, AgentEvent::InputRejected { .. })));
        assert!(requests.lock().unwrap().is_empty());
    }
    // after_tool panic: withheld; required also fails the run.
    let (agent, _) = scripted(vec![
        call("echo", serde_json::json!({"text": "x"})),
        text("never"),
    ]);
    let mut agent = agent.with_extension(
        ext(
            "post",
            Hooks {
                panic_in: Some("after_tool"),
                ..Default::default()
            },
        )
        .required(),
    );
    let events = run(&mut agent, "go").await;
    assert!(tool_results(&events)[0].0.contains("withheld"));
    assert_failed_by(&events, "post", "after_tool failed");
    // finish panic: too late to change the outcome.
    #[derive(Clone)]
    struct BadFinish;
    #[async_trait::async_trait]
    impl RunHooks for BadFinish {
        async fn finish(&mut self, _: &RunOutcome) {
            panic!("finish exploded");
        }
    }
    let (agent, _) = scripted(vec![text("done")]);
    let mut agent = agent.with_extension(ClonedHooks::new("bad-finish", BadFinish).required());
    let events = run(&mut agent, "go").await;
    assert_eq!(final_assistant(&events).0, StopReason::Stop);
}

/// Counts its `before_tool` calls.
#[derive(Clone)]
struct CountCalls(
    Arc<std::sync::atomic::AtomicUsize>,
    Option<serde_json::Value>,
);

#[async_trait::async_trait]
impl RunHooks for CountCalls {
    async fn before_tool(&self, _: &ToolCallRequest<'_>) -> ToolDecision {
        self.0.fetch_add(1, Ordering::SeqCst);
        match &self.1 {
            Some(args) => ToolDecision::Modify(args.clone()),
            None => ToolDecision::Allow,
        }
    }
}

#[tokio::test]
async fn only_rechecking_extensions_before_the_rewriter_are_asked_again() {
    let counter = || Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let (early_rechecker, early_plain, rewriter, late_rechecker) =
        (counter(), counter(), counter(), counter());
    let (agent, _) = scripted(vec![
        call("echo", serde_json::json!({"text": "a"})),
        text("done"),
    ]);
    let mut agent = agent
        .with_extension(
            ClonedHooks::new("early-rechecker", CountCalls(early_rechecker.clone(), None))
                .rechecks_modified_calls(),
        )
        .with_extension(ClonedHooks::new(
            "early-plain",
            CountCalls(early_plain.clone(), None),
        ))
        .with_extension(ClonedHooks::new(
            "rewriter",
            CountCalls(rewriter.clone(), Some(serde_json::json!({"text": "b"}))),
        ))
        .with_extension(
            ClonedHooks::new("late-rechecker", CountCalls(late_rechecker.clone(), None))
                .rechecks_modified_calls(),
        );
    let events = run(&mut agent, "go").await;
    assert_eq!(tool_results(&events), vec![("echo b".into(), false)]);
    assert_eq!(early_rechecker.load(Ordering::SeqCst), 2);
    assert_eq!(early_plain.load(Ordering::SeqCst), 1);
    assert_eq!(rewriter.load(Ordering::SeqCst), 1);
    assert_eq!(late_rechecker.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn the_older_hooks_run_before_the_extensions_at_each_point() {
    let order = Arc::new(Mutex::new(Vec::<String>::new()));
    struct Filter(Arc<Mutex<Vec<String>>>);
    impl InputFilter for Filter {
        fn filter(&self, _: &str) -> FilterResult {
            self.0.lock().unwrap().push("input filter".into());
            FilterResult::Pass
        }
    }
    struct Middleware(Arc<Mutex<Vec<String>>>);
    #[async_trait::async_trait]
    impl ToolMiddleware for Middleware {
        async fn before_tool(&self, _: &ToolCallRequest<'_>) -> ToolDecision {
            self.0.lock().unwrap().push("middleware".into());
            ToolDecision::Allow
        }
    }
    struct Turn;
    #[async_trait::async_trait]
    impl TurnHook for Turn {
        async fn before_turn(&self, _: &TurnContext<'_>) -> Option<String> {
            Some("hook note".into())
        }
    }
    #[derive(Clone)]
    struct Ext(Arc<Mutex<Vec<String>>>);
    #[async_trait::async_trait]
    impl RunHooks for Ext {
        async fn on_input(&mut self, _: &InputContext<'_>) -> InputDecision {
            self.0.lock().unwrap().push("on_input".into());
            InputDecision::Pass
        }
        async fn before_model(&mut self, _: &TurnContext<'_>) -> TurnDecision {
            TurnDecision::Note("ext note".into())
        }
        async fn before_tool(&self, _: &ToolCallRequest<'_>) -> ToolDecision {
            self.0.lock().unwrap().push("before_tool".into());
            ToolDecision::Allow
        }
    }
    let (agent, requests) = scripted(vec![
        call("echo", serde_json::json!({"text": "x"})),
        text("done"),
    ]);
    let mut agent = agent
        .with_input_filter(Filter(order.clone()))
        .with_tool_middleware(Middleware(order.clone()))
        .with_turn_hook(Turn)
        .with_extension(ClonedHooks::new("ext", Ext(order.clone())));
    let _ = run(&mut agent, "go").await;
    assert_eq!(
        *order.lock().unwrap(),
        vec!["input filter", "on_input", "middleware", "before_tool"]
    );
    // Notes: the extension's first, then the turn hook's (it runs inside the
    // provider call).
    assert_eq!(
        last_user_text(&requests.lock().unwrap()[0]),
        "go | ext note | hook note"
    );
}

#[tokio::test]
async fn on_stop_waits_for_follow_ups_and_continues_count_against_limits() {
    // A queued follow-up is answered before the answer is judged.
    let log = Arc::new(Mutex::new(Vec::new()));
    let (agent, requests) = scripted(vec![text("first"), text("second")]);
    let mut agent = agent.with_extension(ext(
        "verify",
        Hooks {
            log: Some(log.clone()),
            ..Default::default()
        },
    ));
    agent.follow_up(AgentMessage::Llm(Message::user("and one more thing")));
    let _ = run(&mut agent, "go").await;
    assert_eq!(requests.lock().unwrap().len(), 2);
    let stops = log
        .lock()
        .unwrap()
        .iter()
        .filter(|l| l.starts_with("on_stop"))
        .count();
    assert_eq!(stops, 1, "judged once, after the follow-up");

    // An extension that always continues still ends at the turn limit.
    let (agent, _) = scripted(vec![text("a"), text("b"), text("c"), text("d")]);
    let mut agent = agent
        .with_max_stop_continues(10)
        .with_execution_limits(yoagent::context::ExecutionLimits::default().with_max_turns(2))
        .with_extension(ext(
            "nag",
            Hooks {
                continue_with: Some("again"),
                ..Default::default()
            },
        ));
    let events = run(&mut agent, "go").await;
    let last = end_messages(&events).last().cloned().unwrap();
    assert!(
        format!("{last:?}").contains("Max turns reached"),
        "{last:?}"
    );
}

#[tokio::test]
async fn finish_sees_every_event_before_agent_end_observed() {
    #[derive(Clone)]
    struct CountAtFinish(Arc<Mutex<(usize, usize)>>);
    #[async_trait::async_trait]
    impl RunHooks for CountAtFinish {
        fn on_event(&self, _: &AgentEvent) {
            self.0.lock().unwrap().0 += 1;
        }
        async fn finish(&mut self, _: &RunOutcome) {
            let mut counts = self.0.lock().unwrap();
            counts.1 = counts.0;
        }
    }
    let counts = Arc::new(Mutex::new((0, 0)));
    let (agent, _) = scripted(vec![
        call("echo", serde_json::json!({"text": "x"})),
        text("done"),
    ]);
    let mut agent = agent.with_extension(ClonedHooks::new("count", CountAtFinish(counts.clone())));
    let events = run(&mut agent, "go").await;
    let at_finish = counts.lock().unwrap().1;
    // Everything but `AgentEnd` itself, which comes after `finish`.
    assert_eq!(at_finish, events.len() - 1);
}

#[tokio::test]
async fn after_tool_is_not_called_for_denied_or_unknown_calls() {
    #[derive(Clone)]
    struct Post(Arc<std::sync::atomic::AtomicUsize>);
    #[async_trait::async_trait]
    impl RunHooks for Post {
        async fn before_tool(&self, call: &ToolCallRequest<'_>) -> ToolDecision {
            if call.args.to_string().contains("deny") {
                ToolDecision::Deny("no".into())
            } else {
                ToolDecision::Allow
            }
        }
        async fn after_tool(
            &self,
            _: &ToolCallRequest<'_>,
            _: &mut ToolOutput,
        ) -> Result<(), ExtensionError> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
    }
    let after = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let (agent, _) = scripted(vec![
        call("echo", serde_json::json!({"text": "deny"})),
        call("missing_tool", serde_json::json!({})),
        text("done"),
    ]);
    let mut agent = agent.with_extension(ClonedHooks::new("post", Post(after.clone())));
    let _ = run(&mut agent, "go").await;
    assert_eq!(after.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn finish_reports_a_provider_error() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let provider = MockProvider::new(vec![MockResponse::ErrorWithUsage(
        "upstream down".into(),
        Usage::default(),
    )]);
    let mut agent = Agent::from_provider(provider, ModelConfig::mock()).with_extension(Outcomes {
        seen: seen.clone(),
        cancel_in_before_tool: false,
    });
    let _ = run(&mut agent, "go").await;
    assert_eq!(
        *seen.lock().unwrap(),
        vec!["Failed by None stop=Some(Error)"]
    );
}

#[tokio::test]
async fn on_event_runs_before_the_consumer_receives_the_event() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let (agent, _) = scripted(vec![
        call("echo", serde_json::json!({"text": "x"})),
        text("done"),
    ]);
    let mut agent = agent.with_extension(ext(
        "audit",
        Hooks {
            audit: Some(seen.clone()),
            ..Default::default()
        },
    ));
    let mut rx = agent.prompt("go").await;
    let mut received = 0;
    while let Some(_event) = rx.recv().await {
        received += 1;
        assert!(seen.lock().unwrap().len() >= received, "observed first");
    }
    agent.finish().await;
    assert!(received > 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn multi_thread_on_event_matches_the_consumer() {
    for _ in 0..10 {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let (agent, _) = scripted(vec![
            call("echo", serde_json::json!({"text": "x"})),
            text("done"),
        ]);
        let mut agent = agent.with_extension(ext(
            "audit",
            Hooks {
                audit: Some(seen.clone()),
                ..Default::default()
            },
        ));
        let events = run(&mut agent, "go").await;
        let consumer: Vec<String> = events.iter().map(|e| format!("{e:?}")).collect();
        assert_eq!(*seen.lock().unwrap(), consumer);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn multi_thread_a_required_on_event_failure_fails_the_run() {
    for _ in 0..10 {
        let (agent, requests) = scripted(vec![text("never")]);
        let mut agent = agent.with_extension(PanickyAudit());
        let events = run(&mut agent, "go").await;
        assert_failed_by(&events, "audit", "on_event failed");
        assert!(requests.lock().unwrap().is_empty());
    }
}

/// Cancels its run from `start_run`, so the input stage sees a cancelled
/// run; records how the run ended.
struct CancelBeforeInput(Arc<Mutex<Vec<String>>>);

struct CancelBeforeInputHooks(Arc<Mutex<Vec<String>>>);

#[async_trait::async_trait]
impl RunHooks for CancelBeforeInputHooks {
    async fn finish(&mut self, outcome: &RunOutcome) {
        self.0.lock().unwrap().push(format!("{:?}", outcome.end()));
    }
}

#[async_trait::async_trait]
impl Extension for CancelBeforeInput {
    fn name(&self) -> &str {
        "cancel-before-input"
    }
    async fn start_run(&self, run: &RunContext<'_>) -> Result<Box<dyn RunHooks>, ExtensionError> {
        run.cancel.cancel();
        Ok(Box::new(CancelBeforeInputHooks(self.0.clone())))
    }
}

#[tokio::test]
async fn a_cancel_before_the_input_check_ends_the_run_cancelled_not_rejected() {
    let ends = Arc::new(Mutex::new(Vec::new()));
    let (agent, requests) = scripted(vec![text("never")]);
    let mut agent = agent.with_extension(CancelBeforeInput(ends.clone()));
    let events = run(&mut agent, "go").await;
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, AgentEvent::InputRejected { .. })),
        "a cancel is not a rejection"
    );
    assert!(requests.lock().unwrap().is_empty());
    assert!(turns_paired(&events));
    assert_eq!(*ends.lock().unwrap(), vec!["Cancelled"]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn on_event_sees_each_event_before_the_consumer_has_it() {
    #[derive(Clone)]
    struct Strict {
        observed: Arc<std::sync::atomic::AtomicUsize>,
        received: Arc<std::sync::atomic::AtomicUsize>,
        violations: Arc<std::sync::atomic::AtomicUsize>,
    }
    #[async_trait::async_trait]
    impl RunHooks for Strict {
        fn on_event(&self, _: &AgentEvent) {
            let index = self.observed.fetch_add(1, Ordering::SeqCst);
            // Give the consumer every chance to overtake: were the event
            // forwarded before this call, the consumer would have it by now.
            std::thread::sleep(std::time::Duration::from_millis(2));
            // The consumer must not have this event (index) yet.
            if self.received.load(Ordering::SeqCst) > index {
                self.violations.fetch_add(1, Ordering::SeqCst);
            }
        }
    }
    let hooks = Strict {
        observed: Arc::default(),
        received: Arc::default(),
        violations: Arc::default(),
    };
    let (agent, _) = scripted(vec![
        call("echo", serde_json::json!({"text": "x"})),
        text("done"),
    ]);
    let mut agent = agent.with_extension(ClonedHooks::new("strict", hooks.clone()));
    let mut rx = agent.prompt("go").await;
    while rx.recv().await.is_some() {
        hooks.received.fetch_add(1, Ordering::SeqCst);
    }
    agent.finish().await;
    assert!(hooks.received.load(Ordering::SeqCst) > 0);
    assert_eq!(hooks.violations.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn a_sub_agent_cancelled_before_it_answers_is_a_cancelled_delegation() {
    let child = SubAgentTool::from_provider(
        "child",
        Arc::new(MockProvider::new(vec![text("never")])),
        ModelConfig::mock(),
    )
    .with_extension(CancelBeforeInput(Arc::default()));
    let (agent, _) = scripted(vec![delegate("child", "go"), text("parent done")]);
    let mut agent = agent.with_tools(vec![Box::new(child)]);
    let events = run(&mut agent, "start").await;
    let results = tool_results(&events);
    assert!(results[0].1, "the delegation failed: {results:?}");
    assert!(
        results[0].0.to_lowercase().contains("cancel"),
        "{results:?}"
    );
}

/// Records a failure from `on_event` (on the event `on`) without panicking,
/// and hands it over through `take_failure`.
#[derive(Clone)]
struct Recorder {
    on: fn(&AgentEvent) -> bool,
    failure: Arc<std::sync::Mutex<Option<String>>>,
}

impl Recorder {
    fn new(on: fn(&AgentEvent) -> bool) -> Self {
        Self {
            on,
            failure: Arc::default(),
        }
    }
}

#[async_trait::async_trait]
impl RunHooks for Recorder {
    fn on_event(&self, event: &AgentEvent) {
        if (self.on)(event) {
            self.failure
                .lock()
                .unwrap()
                .get_or_insert_with(|| "audit sink down".into());
        }
    }
    fn take_failure(&self) -> Option<String> {
        self.failure.lock().unwrap().take()
    }
}

/// A failure recorded through `take_failure` fails a required run, also
/// when the run ends without another hook (a plain final answer here), and
/// a tool call not started yet is not run.
#[tokio::test]
async fn a_failure_handed_over_by_take_failure_fails_a_required_run() {
    // On the final answer: no later hook asks, only the loop's own check.
    let (agent, _) = scripted(vec![text("done")]);
    let mut agent = agent.with_extension(
        ClonedHooks::new(
            "audit",
            Recorder::new(|e| matches!(e, AgentEvent::MessageEnd { .. })),
        )
        .required(),
    );
    let events = run(&mut agent, "go").await;
    assert_failed_by(&events, "audit", "audit sink down");

    // On the tool call's turn: the call is not run.
    let (agent, _) = scripted(vec![
        call("echo", serde_json::json!({"text": "x"})),
        text("done"),
    ]);
    let mut agent = agent.with_extension(
        ClonedHooks::new(
            "audit",
            Recorder::new(|e| matches!(e, AgentEvent::MessageEnd { .. })),
        )
        .required(),
    );
    let events = run(&mut agent, "go").await;
    assert_failed_by(&events, "audit", "audit sink down");
    assert!(
        !events.iter().any(|e| matches!(
            e,
            AgentEvent::ToolExecutionEnd {
                is_error: false,
                ..
            }
        )),
        "the echo call did not run"
    );

    // Positive control: advisory, the same failure is only logged.
    let (agent, _) = scripted(vec![text("done")]);
    let mut agent = agent.with_extension(ClonedHooks::new(
        "audit",
        Recorder::new(|e| matches!(e, AgentEvent::MessageEnd { .. })),
    ));
    let events = run(&mut agent, "go").await;
    assert!(!events.iter().any(|e| matches!(
        e,
        AgentEvent::MessageEnd {
            message: AgentMessage::Llm(Message::Assistant {
                stop_reason: StopReason::Error,
                ..
            }),
            ..
        }
    )));
}

// --- `take_failure` on every ending its docs promise ----------------------

fn is_tool_result_end(e: &AgentEvent) -> bool {
    matches!(
        e,
        AgentEvent::MessageEnd {
            message: AgentMessage::Llm(Message::ToolResult { .. })
        }
    )
}

fn is_assistant_end(e: &AgentEvent) -> bool {
    matches!(
        e,
        AgentEvent::MessageEnd {
            message: AgentMessage::Llm(Message::Assistant { .. })
        }
    )
}

/// A failure recorded on the last turn's events, when `max_turns` ends the
/// run, still fails a required run.
#[tokio::test]
async fn take_failure_is_asked_when_max_turns_ends_the_run() {
    let (agent, _) = scripted(vec![
        call("echo", serde_json::json!({"text": "x"})),
        text("never"),
    ]);
    let mut agent = agent
        .with_execution_limits(yoagent::context::ExecutionLimits::default().with_max_turns(1))
        .with_extension(ClonedHooks::new("audit", Recorder::new(is_tool_result_end)).required());
    let events = run(&mut agent, "go").await;
    assert_failed_by(&events, "audit", "audit sink down");
}

/// Records a failure on the event `on` and cancels the run there.
struct CancelOn(fn(&AgentEvent) -> bool);

struct CancelOnHooks {
    on: fn(&AgentEvent) -> bool,
    cancel: CancellationToken,
    failure: Mutex<Option<String>>,
}

#[async_trait::async_trait]
impl RunHooks for CancelOnHooks {
    fn on_event(&self, event: &AgentEvent) {
        if (self.on)(event) {
            self.failure
                .lock()
                .unwrap()
                .get_or_insert_with(|| "audit sink down".into());
            self.cancel.cancel();
        }
    }
    fn take_failure(&self) -> Option<String> {
        self.failure.lock().unwrap().take()
    }
}

#[async_trait::async_trait]
impl Extension for CancelOn {
    fn name(&self) -> &str {
        "audit"
    }
    fn mode(&self) -> ExtensionMode {
        ExtensionMode::Required
    }
    async fn start_run(&self, run: &RunContext<'_>) -> Result<Box<dyn RunHooks>, ExtensionError> {
        Ok(Box::new(CancelOnHooks {
            on: self.0,
            cancel: run.cancel.clone(),
            failure: Mutex::new(None),
        }))
    }
}

/// A failure recorded as the run is cancelled still fails a required run:
/// on the tool results (the cancel is seen between turns) and on the final
/// answer (seen as the run ends).
#[tokio::test]
async fn take_failure_is_asked_when_a_cancel_ends_the_run() {
    let (agent, _) = scripted(vec![
        call("echo", serde_json::json!({"text": "x"})),
        text("never"),
    ]);
    let mut agent = agent.with_extension(CancelOn(is_tool_result_end));
    let events = run(&mut agent, "go").await;
    assert_failed_by(&events, "audit", "audit sink down");

    let (agent, _) = scripted(vec![text("done")]);
    let mut agent = agent.with_extension(CancelOn(is_assistant_end));
    let events = run(&mut agent, "go").await;
    assert_failed_by(&events, "audit", "audit sink down");
}

/// Answers every request with text cut off at the output limit.
struct CutOff;

#[async_trait::async_trait]
impl StreamProvider for CutOff {
    async fn stream(
        &self,
        _config: StreamConfig,
        _tx: mpsc::UnboundedSender<StreamEvent>,
        _cancel: CancellationToken,
    ) -> Result<Message, ProviderError> {
        Ok(Message::assistant(
            vec![Content::Text {
                text: "half an ans".into(),
            }],
            StopReason::Length,
            "mock",
            "mock",
            Usage::default(),
        ))
    }
}

/// `on_stop` is not called for an answer cut off at the output limit; a
/// failure recorded on it still fails a required run.
#[tokio::test]
async fn take_failure_is_asked_when_the_final_answer_is_cut_off() {
    let mut agent = Agent::from_provider(CutOff, ModelConfig::mock())
        .with_extension(ClonedHooks::new("audit", Recorder::new(is_assistant_end)).required());
    let events = run(&mut agent, "go").await;
    assert_failed_by(&events, "audit", "audit sink down");

    // Positive control: without the failure the run ends on the cut-off
    // answer.
    let mut agent = Agent::from_provider(CutOff, ModelConfig::mock())
        .with_extension(ClonedHooks::new("audit", Recorder::new(|_| false)).required());
    let events = run(&mut agent, "go").await;
    assert_eq!(final_assistant(&events).0, StopReason::Length);
}

/// `take_failure` itself panics, every time it is asked.
#[derive(Clone)]
struct PanickyTake;

#[async_trait::async_trait]
impl RunHooks for PanickyTake {
    fn take_failure(&self) -> Option<String> {
        panic!("ledger poisoned")
    }
}

/// A panicking `take_failure` is contained: it fails a required run (once),
/// is only logged for an advisory one, and the loop carries on either way.
#[tokio::test]
async fn a_panicking_take_failure_is_contained() {
    let (agent, _) = scripted(vec![
        call("echo", serde_json::json!({"text": "x"})),
        text("done"),
    ]);
    let mut agent = agent.with_extension(ClonedHooks::new("audit", PanickyTake).required());
    let events = run(&mut agent, "go").await;
    assert_failed_by(&events, "audit", "ledger poisoned");
    let failures = end_messages(&events)
        .iter()
        .filter(|m| {
            matches!(
                m,
                AgentMessage::Llm(Message::Assistant {
                    stop_reason: StopReason::Error,
                    ..
                })
            )
        })
        .count();
    assert_eq!(failures, 1, "the run fails once");

    let (agent, _) = scripted(vec![
        call("echo", serde_json::json!({"text": "x"})),
        text("done"),
    ]);
    let mut agent = agent.with_extension(ClonedHooks::new("audit", PanickyTake));
    let events = run(&mut agent, "go").await;
    assert_eq!(final_assistant(&events).0, StopReason::Stop);
    let results = tool_results(&events);
    assert_eq!(results.len(), 1);
    assert!(!results[0].1, "the call ran: {results:?}");
    assert!(turns_paired(&events));
}
