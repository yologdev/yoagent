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
    async fn before_tool(&mut self, call: &ToolCallRequest<'_>) -> ToolDecision {
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
        &mut self,
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
    async fn finish(&mut self, outcome: &RunOutcome) {
        self.record(format!(
            "finish stop={:?} rejected={} cancelled={}",
            outcome.stop_reason, outcome.rejected, outcome.cancelled
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

fn ext(name: &str, hooks: Hooks) -> Stateless<Hooks> {
    Stateless::new(name, hooks)
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
            "finish stop=None rejected=true cancelled=false".to_string()
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
    struct Audit(Arc<Mutex<Vec<String>>>);
    #[async_trait::async_trait]
    impl Extension for Audit {
        fn name(&self) -> &str {
            "audit"
        }
        async fn start_run(&self, _: &RunContext<'_>) -> Result<Box<dyn RunHooks>, ExtensionError> {
            Ok(Box::new(Hooks::default()))
        }
        fn on_event(&self, _run_id: &str, event: &AgentEvent) {
            self.0.lock().unwrap().push(format!("{event:?}"));
        }
    }
    let seen = Arc::new(Mutex::new(Vec::new()));
    let (agent, _) = scripted(vec![
        call("echo", serde_json::json!({"text": "x"})),
        text("done"),
    ]);
    let mut agent = agent.with_extension(Audit(seen.clone()));
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
    async fn before_tool(&mut self, call: &ToolCallRequest<'_>) -> ToolDecision {
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

/// Panics observing `AgentStart`, the run's first event.
struct PanickyAudit;

#[async_trait::async_trait]
impl Extension for PanickyAudit {
    fn name(&self) -> &str {
        "audit"
    }
    fn mode(&self) -> ExtensionMode {
        ExtensionMode::Required
    }
    async fn start_run(&self, _: &RunContext<'_>) -> Result<Box<dyn RunHooks>, ExtensionError> {
        Ok(Box::new(Hooks::default()))
    }
    fn on_event(&self, _: &str, event: &AgentEvent) {
        if matches!(event, AgentEvent::AgentStart) {
            panic!("audit log unavailable");
        }
    }
}

#[tokio::test]
async fn a_required_on_event_failure_fails_the_run_deterministically() {
    for _ in 0..20 {
        let (agent, requests) = scripted(vec![text("never")]);
        let mut agent = agent.with_extension(PanickyAudit);
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
    async fn before_tool(&mut self, _: &ToolCallRequest<'_>) -> ToolDecision {
        if let Some(cancel) = &self.cancel {
            cancel.cancel();
        }
        ToolDecision::Allow
    }
    async fn finish(&mut self, outcome: &RunOutcome) {
        self.seen.lock().unwrap().push(format!(
            "stop={:?} rejected={} cancelled={} error={}",
            outcome.stop_reason,
            outcome.rejected,
            outcome.cancelled,
            outcome
                .error
                .as_deref()
                .is_some_and(|e| e.starts_with(EXTENSION_FAILED_PREFIX))
        ));
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
        vec!["stop=Some(Stop) rejected=false cancelled=false error=false"]
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
        vec!["stop=None rejected=false cancelled=false error=false"]
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
        vec!["stop=Some(Error) rejected=false cancelled=false error=true"]
    );
    // Cancelled while its tool call was being judged: the call does not run.
    let (a, _) = scripted(vec![
        call("echo", serde_json::json!({"text": "x"})),
        text("never"),
    ]);
    assert_eq!(
        finished(a, true, "go").await,
        vec!["stop=Some(ToolUse) rejected=false cancelled=true error=false"]
    );
    // Stopped by a turn limit.
    let (a, _) = scripted(vec![
        call("echo", serde_json::json!({"text": "x"})),
        text("never"),
    ]);
    let a = a.with_execution_limits(yoagent::context::ExecutionLimits::default().with_max_turns(1));
    assert_eq!(
        finished(a, false, "go").await,
        vec!["stop=Some(ToolUse) rejected=false cancelled=false error=false"]
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
            &mut self,
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
        .with_extension(Stateless::new("errors", SeesErrors(seen.clone())));
    let _ = run(&mut agent, "go").await;
    assert_eq!(*seen.lock().unwrap(), vec![true, false]);
}
