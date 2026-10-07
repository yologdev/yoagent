//! The edges: host shutdown, the reload window, `require_policy`, timeouts,
//! required mode, and testing a handler through the extension directly.

mod common;

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use common::*;
use rutis::{BoxFuture, CordisError, Ctx, Effect, FiberState, Plugin, PluginFactory, TypeKey};
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;
use yoagent::extension::{InputContext, InputDecision, RunContext, TurnDecision};
use yoagent::{AgentEvent, Extension, Message, RunHooks, ToolCallRequest, ToolDecision};
use yoagent_rutis::{PluginCtxExt, Registry, RutisExtension};

/// Start a run of `extension` outside an agent.
async fn start(extension: &RutisExtension, prompts: &[Message]) -> Box<dyn RunHooks> {
    let cancel = CancellationToken::new();
    extension
        .start_run(&RunContext::new("run-1", prompts, &cancel))
        .await
        .expect("the bridge's extension always starts")
}

async fn judge(extension: &RutisExtension, prompt: &str) -> ToolDecision {
    let prompts = [Message::user(prompt)];
    let hooks = start(extension, &prompts).await;
    let args = serde_json::json!({});
    hooks
        .before_tool(&ToolCallRequest::new("c1", "act", &args).with_run_prompts(&prompts))
        .await
}

#[tokio::test(flavor = "multi_thread")]
async fn after_the_host_shuts_down_calls_are_denied_and_prompts_rejected() {
    let (root, bridge) = setup();
    let policy = root.plugin(plugin(deny_all("deny-all", "nothing may run")));
    wait_active(&policy).await;
    let tool = Reply::new("act", "acted");
    let runs = tool.runs();
    let (agent, seen) = agent(vec![call("act", serde_json::json!({})), text("done")]);
    let mut agent = agent
        .with_tools(vec![Box::new(tool)])
        .with_extension(bridge.extension());

    root.shutdown().await.unwrap();

    // The prompt is rejected before the model sees it...
    let (events, _) = run(&mut agent, "go").await;
    assert!(
        events.iter().any(|e| matches!(
            e,
            AgentEvent::InputRejected { reason } if reason.contains("not running")
        )),
        "{events:?}"
    );
    assert!(seen.lock().unwrap().is_empty());

    // ...and a tool call is denied, though the deny-all handler was removed
    // with its plugin (an empty registry would otherwise allow).
    let decision = judge(&bridge.extension(), "go").await;
    assert!(
        matches!(&decision, ToolDecision::Deny(r) if r.contains("not running")),
        "{decision:?}"
    );
    assert_eq!(runs.load(Ordering::SeqCst), 0);
}

/// A policy whose `apply` waits for a gate before registering its handler,
/// so a test can hold it in `Loading`.
struct GatedPolicyFactory {
    gate: Arc<Notify>,
}

struct GatedPolicy {
    gate: Arc<Notify>,
    first: bool,
    injects: Vec<TypeKey>,
}

impl Plugin for GatedPolicy {
    fn name(&self) -> &str {
        "gated-policy"
    }
    fn injects(&self) -> &[TypeKey] {
        &self.injects
    }
    fn apply<'a>(&'a self, ctx: &'a Ctx) -> BoxFuture<'a, Result<Effect, CordisError>> {
        Box::pin(async move {
            if !self.first {
                self.gate.notified().await;
            }
            ctx.register_handler(handler("gated").with_before_tool(|_| ToolDecision::Allow))?;
            Ok(Effect::Done)
        })
    }
}

impl PluginFactory<u32> for GatedPolicyFactory {
    fn name(&self) -> &str {
        "gated-policy"
    }
    fn build(&self, version: &u32) -> Result<Box<dyn Plugin>, CordisError> {
        Ok(Box::new(GatedPolicy {
            gate: self.gate.clone(),
            first: *version == 1,
            injects: vec![TypeKey::of::<Registry>()],
        }))
    }
}

async fn verdict_during_reload(root: Ctx, extension: RutisExtension) -> ToolDecision {
    let gate = Arc::new(Notify::new());
    let view = root.plugin_with(GatedPolicyFactory { gate: gate.clone() }, 1u32);
    wait_active(&view).await;
    let before = judge(&extension, "go").await;
    assert!(
        matches!(before, ToolDecision::Allow),
        "the loaded policy allows"
    );

    // Update: the old handler is removed, the new generation waits at the
    // gate — no policy is registered now.
    let updating = tokio::spawn({
        let view = view.clone();
        async move { view.update(2u32).await }
    });
    wait_state(&view, FiberState::Loading).await;
    let during = judge(&extension, "go").await;
    gate.notify_one();
    updating.await.unwrap().unwrap();
    wait_active(&view).await;
    let after = judge(&extension, "go").await;
    assert!(
        matches!(after, ToolDecision::Allow),
        "the reloaded policy allows"
    );
    root.shutdown().await.unwrap();
    during
}

#[tokio::test(flavor = "multi_thread")]
async fn by_default_the_reload_window_allows() {
    let (root, bridge) = setup();
    let verdict = verdict_during_reload(root, bridge.extension()).await;
    assert!(matches!(verdict, ToolDecision::Allow), "{verdict:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn require_policy_denies_during_the_reload_window() {
    let (root, bridge) = setup();
    let verdict = verdict_during_reload(root, bridge.extension().require_policy()).await;
    assert!(
        matches!(&verdict, ToolDecision::Deny(r) if r.contains("no plugin policy")),
        "{verdict:?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn require_policy_denies_when_no_policy_was_ever_loaded() {
    let (root, bridge) = setup();
    // A plugin with only a tool is not a policy.
    let tools = root.plugin(plugin(
        handler("tools").with_tool(Reply::new("greet", "hi")),
    ));
    wait_active(&tools).await;
    let tool = Reply::new("act", "acted");
    let runs = tool.runs();
    let (agent, _) = agent(vec![call("act", serde_json::json!({})), text("done")]);
    let mut agent = agent
        .with_tools(vec![Box::new(tool)])
        .with_extension(bridge.extension().require_policy());
    let (_, results) = run(&mut agent, "go").await;
    assert!(results[0].2);
    assert!(results[0].1.contains("no plugin policy"), "{results:?}");
    assert_eq!(runs.load(Ordering::SeqCst), 0);
    root.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_policy_after_a_denier_never_sees_the_call() {
    let (root, bridge) = setup();
    let denier = root.plugin(plugin(handler("denier").with_before_tool(|call| {
        if call.tool == "act" {
            ToolDecision::Deny("no".into())
        } else {
            ToolDecision::Allow
        }
    })));
    wait_active(&denier).await;
    let counted = Arc::new(AtomicUsize::new(0));
    let counter = root.plugin(plugin(handler("rate-counter").with_before_tool({
        let counted = counted.clone();
        move |_| {
            counted.fetch_add(1, Ordering::SeqCst);
            ToolDecision::Allow
        }
    })));
    wait_active(&counter).await;
    let (agent, _) = agent(vec![
        call("act", serde_json::json!({})),
        call("other", serde_json::json!({})),
        text("done"),
    ]);
    let mut agent = agent
        .with_tools(vec![
            Box::new(Reply::new("act", "acted")),
            Box::new(Reply::new("other", "other ran")),
        ])
        .with_extension(bridge.extension());
    let (_, results) = run(&mut agent, "go").await;
    assert!(results[0].2, "act denied");
    assert!(!results[1].2, "other allowed");
    assert_eq!(
        counted.load(Ordering::SeqCst),
        1,
        "only the allowed call reached the later policy"
    );
    root.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_hung_input_handler_rejects_after_its_timeout() {
    let (root, bridge) = setup();
    let hangs = root.plugin(plugin(handler("hanging-input").with_on_input_async(
        |_| async {
            std::future::pending::<()>().await;
            Ok(InputDecision::Pass)
        },
    )));
    wait_active(&hangs).await;
    let (agent, seen) = agent(vec![text("done")]);
    let mut agent = agent.with_extension(
        bridge
            .extension()
            .with_input_timeout(Some(Duration::from_millis(100))),
    );
    let (events, _) = tokio::time::timeout(Duration::from_secs(5), run(&mut agent, "go"))
        .await
        .expect("the input timeout bounds the run");
    assert!(
        events.iter().any(|e| matches!(
            e,
            AgentEvent::InputRejected { reason } if reason.contains("did not answer")
        )),
        "{events:?}"
    );
    assert!(seen.lock().unwrap().is_empty());
    root.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_hung_turn_handler_keeps_the_notes_of_the_others() {
    let (root, bridge) = setup();
    let first =
        root.plugin(plugin(handler("first").with_before_model(|_| {
            TurnDecision::Note("[before the hang]".into())
        })));
    wait_active(&first).await;
    let hangs = root.plugin(plugin(handler("hanging-note").with_before_model_async(
        |_| async {
            std::future::pending::<()>().await;
            Ok(TurnDecision::Continue)
        },
    )));
    wait_active(&hangs).await;
    let (agent, seen) = agent(vec![text("done")]);
    let mut agent = agent.with_extension(
        bridge
            .extension()
            .with_turn_timeout(Some(Duration::from_millis(100))),
    );
    tokio::time::timeout(Duration::from_secs(5), run(&mut agent, "go"))
        .await
        .expect("the turn timeout bounds the request");
    assert_eq!(seen.lock().unwrap()[0].last_user, "go|[before the hang]");
    root.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_failing_turn_handler_fails_a_required_run() {
    let (root, bridge) = setup();
    let p = root
        .plugin(plugin(handler("failing-note").with_before_model(|_| {
            TurnDecision::Fail("note backend down".into())
        })));
    wait_active(&p).await;
    let (agent, seen) = agent(vec![text("done")]);
    let mut agent = agent.with_extension(bridge.extension().required());
    run(&mut agent, "go").await;
    assert!(seen.lock().unwrap().is_empty(), "no request was sent");
    let error = run_error(&agent).expect("the run failed");
    assert!(
        error.contains("failing-note") && error.contains("note backend down"),
        "{error}"
    );
    root.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_failing_tools_hook_fails_a_required_run_before_the_first_request() {
    let (root, bridge) = setup();
    let p = root.plugin(plugin(
        handler("broken-tools").with_tools(|_| panic!("catalogue unavailable")),
    ));
    wait_active(&p).await;
    for required in [false, true] {
        let (agent, seen) = agent(vec![text("done")]);
        let extension = if required {
            bridge.extension().required()
        } else {
            bridge.extension()
        };
        let mut agent = agent.with_extension(extension);
        run(&mut agent, "go").await;
        if required {
            assert!(seen.lock().unwrap().is_empty());
            let error = run_error(&agent).expect("the run failed");
            assert!(error.contains("catalogue unavailable"), "{error}");
        } else {
            assert_eq!(seen.lock().unwrap().len(), 1, "skipped, the run goes on");
            assert_eq!(run_error(&agent), None);
        }
    }
    root.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_policy_can_be_unit_tested_through_the_extension() {
    let (root, bridge) = setup();
    let policy =
        root.plugin(plugin(handler("request-aware").with_before_tool(
            |call| match &call.user_request {
                Some(r) if r.contains("delete") => ToolDecision::Allow,
                _ => ToolDecision::Deny("the user did not ask for a deletion".into()),
            },
        )));
    wait_active(&policy).await;
    let extension = bridge.extension();
    assert!(matches!(
        judge(&extension, "please delete tmp").await,
        ToolDecision::Allow
    ));
    assert!(matches!(
        judge(&extension, "list files").await,
        ToolDecision::Deny(_)
    ));

    // The input hook, too.
    let hooks_input = root.plugin(plugin(handler("input").with_on_input(|input| {
        if input.text.is_empty() {
            InputDecision::Reject("empty".into())
        } else {
            InputDecision::Pass
        }
    })));
    wait_active(&hooks_input).await;
    let mut hooks = start(&extension, &[]).await;
    assert_eq!(
        hooks.on_input(&InputContext::new("", &[])).await,
        InputDecision::Reject("empty".into())
    );
    root.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn notes_are_joined_in_registration_order_and_recomputed_every_turn() {
    let (root, bridge) = setup();
    let n = Arc::new(AtomicUsize::new(0));
    let first = root.plugin(plugin(handler("first").with_before_model({
        let n = n.clone();
        move |_| TurnDecision::Note(format!("[a{}]", n.fetch_add(1, Ordering::SeqCst) + 1))
    })));
    wait_active(&first).await;
    let second = root.plugin(plugin(
        handler("second").with_before_model(|_| TurnDecision::Note("[b]".into())),
    ));
    wait_active(&second).await;
    let (agent, seen) = agent(vec![call("act", serde_json::json!({})), text("done")]);
    let mut agent = agent
        .with_tools(vec![Box::new(Reply::new("act", "acted"))])
        .with_extension(bridge.extension());
    run(&mut agent, "go").await;
    let seen = seen.lock().unwrap().clone();
    assert_eq!(seen[0].last_user, "go|[a1]\n[b]");
    assert_eq!(
        seen[1].last_user, "go|[a2]\n[b]",
        "the second turn's notes replace the first's"
    );
    root.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_bridge_installed_on_a_plugin_context_stops_with_that_plugin() {
    let root = rutis::Ctx::root().unwrap();
    let kept = Arc::new(std::sync::Mutex::new(None));
    let host = root.plugin(
        Setup::new("host-plugin", {
            let kept = kept.clone();
            move |ctx| {
                *kept.lock().unwrap() = Some(yoagent_rutis::RutisBridge::install(ctx)?);
                Ok(())
            }
        })
        .eager(),
    );
    wait_active(&host).await;
    let bridge = kept.lock().unwrap().clone().unwrap();
    assert!(matches!(
        judge(&bridge.extension(), "go").await,
        ToolDecision::Allow
    ));
    host.dispose().await.unwrap();
    let decision = judge(&bridge.extension(), "go").await;
    assert!(
        matches!(&decision, ToolDecision::Deny(r) if r.contains("not running")),
        "{decision:?}"
    );
    root.shutdown().await.unwrap();
}

/// Panics when it sees the assistant message that asks for tools.
fn panics_on_tool_use(name: &str) -> yoagent_rutis::Handler {
    handler(name).with_on_event(|_run, event| {
        if let AgentEvent::MessageEnd {
            message:
                yoagent::AgentMessage::Llm(Message::Assistant {
                    stop_reason: yoagent::StopReason::ToolUse,
                    ..
                }),
        } = event
        {
            panic!("observer bug");
        }
    })
}

#[tokio::test(flavor = "multi_thread")]
async fn a_required_on_event_failure_stops_pending_tool_calls_and_fails_the_run() {
    let (root, bridge) = setup();
    let p = root.plugin(plugin(panics_on_tool_use("fragile-observer")));
    wait_active(&p).await;
    for required in [false, true] {
        let tool = Reply::new("act", "acted");
        let runs = tool.runs();
        let two_calls = yoagent::provider::mock::MockResponse::ToolCalls(
            (0..2)
                .map(|_| yoagent::provider::mock::MockToolCall {
                    provider_metadata: None,
                    name: "act".into(),
                    arguments: serde_json::json!({}),
                })
                .collect(),
        );
        let (agent, _) = agent(vec![two_calls, text("done")]);
        let extension = if required {
            bridge.extension().required()
        } else {
            bridge.extension()
        };
        let mut agent = agent
            .with_tools(vec![Box::new(tool)])
            .with_extension(extension);
        run(&mut agent, "go").await;
        if required {
            assert_eq!(runs.load(Ordering::SeqCst), 0, "no pending call ran");
            let error = run_error(&agent).expect("the run failed");
            assert!(
                error.contains("fragile-observer") && error.contains("observer bug"),
                "{error}"
            );
        } else {
            assert_eq!(runs.load(Ordering::SeqCst), 2, "advisory: logged only");
            assert_eq!(run_error(&agent), None);
        }
    }
    root.shutdown().await.unwrap();
}

fn event_kind(e: &AgentEvent) -> String {
    serde_json::to_value(e).unwrap()["type"]
        .as_str()
        .unwrap()
        .to_string()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_required_on_event_failure_keeps_the_bus_and_other_observers_going() {
    let (root, bridge) = setup();
    let fragile = root.plugin(plugin(panics_on_tool_use("fragile-observer")));
    wait_active(&fragile).await;
    let handled = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    let steady = root.plugin(plugin(handler("steady-observer").with_on_event({
        let handled = handled.clone();
        move |_run, event| handled.lock().unwrap().push(event_kind(event))
    })));
    wait_active(&steady).await;
    let bus = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    let listener = root.plugin(Setup::new("bus-listener", {
        let bus = bus.clone();
        move |ctx| {
            let bus = bus.clone();
            ctx.on_agent_event(move |e| bus.lock().unwrap().push(event_kind(e.event())))
                .map(drop)
        }
    }));
    wait_active(&listener).await;

    let (agent, _) = agent(vec![call("act", serde_json::json!({})), text("done")]);
    let tool = Reply::new("act", "acted");
    let runs = tool.runs();
    let mut agent = agent
        .with_tools(vec![Box::new(tool)])
        .with_extension(bridge.extension().required());
    run(&mut agent, "go").await;

    assert_eq!(
        runs.load(Ordering::SeqCst),
        0,
        "the pending call was denied"
    );
    let error = run_error(&agent).expect("the run failed");
    assert!(
        error.contains("fragile-observer") && error.contains("observer bug"),
        "{error}"
    );
    // The other handler saw the whole run, to its end...
    let handled = handled.lock().unwrap().clone();
    assert_eq!(
        handled.last().map(String::as_str),
        Some("agentEnd"),
        "{handled:?}"
    );
    assert!(
        handled.iter().any(|k| k == "toolExecutionEnd"),
        "{handled:?}"
    );
    // ...and so did the bus (delivered asynchronously).
    tokio::time::timeout(Duration::from_secs(5), async {
        while !bus.lock().unwrap().iter().any(|k| k == "agentEnd") {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("the bus never saw agentEnd: {:?}", bus.lock().unwrap()));
    assert_eq!(*bus.lock().unwrap(), handled, "the bus saw every event too");
    root.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_restarted_root_reads_as_stopped_for_the_bridge_installed_before() {
    let (root, bridge) = setup();
    let policy = root.plugin(plugin(deny_all("deny-all", "nothing may run")));
    wait_active(&policy).await;
    let old = bridge.extension();
    assert!(matches!(judge(&old, "go").await, ToolDecision::Deny(r) if r == "nothing may run"));

    root.root_view()
        .expect("the root's own view")
        .restart()
        .await
        .unwrap();
    // The registry went with the old generation: the old extension denies
    // everything rather than reading an empty registry as "no objection".
    let decision = judge(&old, "go").await;
    assert!(
        matches!(&decision, ToolDecision::Deny(r) if r.contains("not running")),
        "{decision:?}"
    );

    root.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn an_unavailable_handler_rejects_input_and_withholds_output() {
    let (root, bridge) = setup();
    let guard = root.plugin(plugin(
        handler("guard")
            .with_on_input(|_| InputDecision::Pass)
            .with_after_tool(|_, _| Ok(())),
    ));
    wait_active(&guard).await;
    let mut hooks = start(&bridge.extension(), &[]).await;
    // The plugin unloads after the run started: the run still holds its
    // handler, which can no longer answer.
    guard.dispose().await.unwrap();
    let rejected = hooks.on_input(&InputContext::new("hello", &[])).await;
    assert!(
        matches!(&rejected, InputDecision::Reject(r) if r.contains("no longer available")),
        "{rejected:?}"
    );
    let args = serde_json::json!({});
    let mut output = yoagent::extension::ToolOutput::new(
        yoagent::ToolResult {
            content: vec![yoagent::Content::Text {
                text: "secret".into(),
            }],
            details: serde_json::Value::Null,
        },
        false,
    );
    let filtered = hooks
        .after_tool(&ToolCallRequest::new("c1", "read", &args), &mut output)
        .await;
    // Withheld, without failing the run (not even a required one): an
    // unloaded plugin is not a failing one.
    assert!(filtered.is_ok(), "{filtered:?}");
    assert!(output.is_error);
    let text = match &output.result.content[..] {
        [yoagent::Content::Text { text }] => text.clone(),
        other => panic!("{other:?}"),
    };
    assert!(
        text.contains("withheld") && text.contains("no longer available"),
        "{text}"
    );
    assert!(!text.contains("secret"));
    root.shutdown().await.unwrap();
}

/// Unloads a plugin while it runs, then returns a secret.
struct UnloadsThenReplies(std::sync::Mutex<Option<rutis::FiberView>>);

#[async_trait::async_trait]
impl yoagent::AgentTool for UnloadsThenReplies {
    fn name(&self) -> &str {
        "read"
    }
    fn label(&self) -> &str {
        "read"
    }
    fn description(&self) -> &str {
        "unloads a plugin, then replies"
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({"type": "object", "properties": {}})
    }
    async fn execute(
        &self,
        _params: serde_json::Value,
        _ctx: yoagent::ToolContext,
    ) -> Result<yoagent::ToolResult, yoagent::ToolError> {
        let view = self.0.lock().unwrap().take();
        if let Some(view) = view {
            view.dispose().await.unwrap();
        }
        Ok(yoagent::ToolResult {
            content: vec![yoagent::Content::Text {
                text: "secret".into(),
            }],
            details: serde_json::Value::Null,
        })
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_redactor_unloading_mid_run_withholds_without_failing_a_required_run() {
    let (root, bridge) = setup();
    let redactor = root.plugin(plugin(handler("redactor").with_after_tool(|_, _| Ok(()))));
    wait_active(&redactor).await;
    let (agent, seen) = agent(vec![call("read", serde_json::json!({})), text("done")]);
    let mut agent = agent
        .with_tools(vec![Box::new(UnloadsThenReplies(std::sync::Mutex::new(
            Some(redactor),
        )))])
        .with_extension(bridge.extension().required());
    let (_, results) = run(&mut agent, "go").await;
    assert_eq!(results.len(), 1, "{results:?}");
    let (_, text, is_error) = &results[0];
    assert!(*is_error && text.contains("withheld"), "{text}");
    assert!(!text.contains("secret"), "{text}");
    assert_eq!(seen.lock().unwrap().len(), 2, "the run went on");
    assert_eq!(run_error(&agent), None);
    root.shutdown().await.unwrap();
}
