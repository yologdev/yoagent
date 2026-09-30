//! Tool policy, turn notes and input filtering from plugins.

mod common;

use std::sync::atomic::Ordering;
use std::time::Duration;

use common::*;
use rutis::{BoxFuture, CordisError, Ctx, Effect, Plugin, TypeKey};
use yoagent::AgentEvent;
use yoagent_rutis::{
    AgentPlugin, AgentRutisExt, PluginCtxExt, RutisBridge, ToolCallEvent, ToolPolicy, ToolRegistry,
    ToolVerdict,
};

async fn setup() -> (Ctx, RutisBridge) {
    let root = Ctx::root().unwrap();
    let bridge = RutisBridge::install(&root).unwrap();
    (root, bridge)
}

#[tokio::test(flavor = "multi_thread")]
async fn no_policy_listener_allows_the_call() {
    let (root, bridge) = setup().await;
    let tool = Reply::new("act", "acted");
    let runs = tool.runs();
    let (agent, _) = agent(vec![call("act", serde_json::json!({})), text("done")]);
    let mut agent = agent.with_tools(vec![Box::new(tool)]).with_rutis(&bridge);
    let (_, results) = run(&mut agent, "go").await;
    assert_eq!(results, vec![("act".into(), "acted".into(), false)]);
    assert_eq!(runs.load(Ordering::SeqCst), 1);
    root.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_policy_plugin_denies_a_call_and_the_model_gets_the_reason() {
    let (root, bridge) = setup().await;
    let policy = root.plugin(AgentPlugin::new("no-act").with_policy(|call| {
        if call.tool_name() == "act" {
            ToolVerdict::deny("act is forbidden by policy")
        } else {
            ToolVerdict::Allow
        }
    }));
    wait_active(&policy).await;

    let tool = Reply::new("act", "acted");
    let runs = tool.runs();
    let (agent, _) = agent(vec![
        call("act", serde_json::json!({})),
        text("ok"),
        call("act", serde_json::json!({})),
        text("ok"),
    ]);
    let mut agent = agent.with_tools(vec![Box::new(tool)]).with_rutis(&bridge);
    let (_, results) = run(&mut agent, "go").await;
    assert_eq!(runs.load(Ordering::SeqCst), 0, "a denied tool does not run");
    assert!(results[0].2);
    assert!(
        results[0].1.contains("act is forbidden by policy"),
        "{results:?}"
    );

    // Unloading the policy plugin lifts the denial on the next call.
    policy.dispose().await.unwrap();
    let (_, results) = run(&mut agent, "again").await;
    assert_eq!(results, vec![("act".into(), "acted".into(), false)]);
    root.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_policy_can_rewrite_arguments_and_later_listeners_see_them() {
    let (root, bridge) = setup().await;
    let sandbox = root.plugin(AgentPlugin::new("sandbox").with_policy(|call| {
        let mut args = call.args();
        if let Some(path) = args.get("path").and_then(|p| p.as_str()) {
            args["path"] = format!("/sandbox{path}").into();
            call.set_args(args);
        }
        ToolVerdict::Allow
    }));
    wait_active(&sandbox).await;
    // Registered after: judges the rewritten path.
    let guard =
        root.plugin(AgentPlugin::new("guard").with_policy(
            |call| match call.args()["path"].as_str() {
                Some(p) if p.starts_with("/sandbox/") => ToolVerdict::Allow,
                other => ToolVerdict::deny(format!("unsandboxed path {other:?}")),
            },
        ));
    wait_active(&guard).await;

    let (agent, _) = agent(vec![
        call("echo_args", serde_json::json!({"path": "/etc/passwd"})),
        text("done"),
    ]);
    let mut agent = agent
        .with_tools(vec![Box::new(EchoArgs)])
        .with_rutis(&bridge);
    let (events, results) = run(&mut agent, "go").await;
    assert_eq!(results[0].1, r#"{"path":"/sandbox/etc/passwd"}"#);
    assert!(!results[0].2);
    let started = events.iter().find_map(|e| match e {
        AgentEvent::ToolExecutionStart { args, .. } => Some(args.clone()),
        _ => None,
    });
    assert_eq!(
        started,
        Some(serde_json::json!({"path": "/sandbox/etc/passwd"})),
        "the tool runs with the rewritten arguments"
    );
    root.shutdown().await.unwrap();
}

struct Failing;

#[async_trait::async_trait]
impl ToolPolicy for Failing {
    async fn check(&self, _call: &ToolCallEvent) -> Result<ToolVerdict, CordisError> {
        Err(CordisError::PluginFailed(
            "policy backend unreachable".into(),
        ))
    }
}

struct FailingPolicyPlugin;

impl Plugin for FailingPolicyPlugin {
    fn name(&self) -> &str {
        "failing-policy"
    }
    fn apply<'a>(&'a self, ctx: &'a Ctx) -> BoxFuture<'a, Result<Effect, CordisError>> {
        Box::pin(async move {
            ctx.on_tool_call_async(Failing)?;
            Ok(Effect::Done)
        })
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn an_erroring_policy_denies_fail_closed() {
    let (root, bridge) = setup().await;
    let failing = root.plugin(FailingPolicyPlugin);
    wait_active(&failing).await;
    let tool = Reply::new("act", "acted");
    let runs = tool.runs();
    let (agent, _) = agent(vec![call("act", serde_json::json!({})), text("done")]);
    let mut agent = agent.with_tools(vec![Box::new(tool)]).with_rutis(&bridge);
    let (_, results) = run(&mut agent, "go").await;
    assert_eq!(runs.load(Ordering::SeqCst), 0);
    assert!(results[0].2);
    assert!(
        results[0].1.contains("policy backend unreachable"),
        "{results:?}"
    );
    root.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_panicking_policy_denies_fail_closed() {
    let (root, bridge) = setup().await;
    let panicking = root.plugin(AgentPlugin::new("panicky").with_policy(|_| panic!("policy bug")));
    wait_active(&panicking).await;
    let tool = Reply::new("act", "acted");
    let runs = tool.runs();
    let (agent, _) = agent(vec![call("act", serde_json::json!({})), text("done")]);
    let mut agent = agent.with_tools(vec![Box::new(tool)]).with_rutis(&bridge);
    let (_, results) = run(&mut agent, "go").await;
    assert_eq!(runs.load(Ordering::SeqCst), 0);
    assert!(results[0].2);
    assert!(results[0].1.contains("panicked"), "{results:?}");
    root.shutdown().await.unwrap();
}

/// Never answers.
struct Hanging;

#[async_trait::async_trait]
impl ToolPolicy for Hanging {
    async fn check(&self, _call: &ToolCallEvent) -> Result<ToolVerdict, CordisError> {
        std::future::pending().await
    }
}

struct HangingPolicyPlugin;

impl Plugin for HangingPolicyPlugin {
    fn name(&self) -> &str {
        "hanging-policy"
    }
    fn apply<'a>(&'a self, ctx: &'a Ctx) -> BoxFuture<'a, Result<Effect, CordisError>> {
        Box::pin(async move {
            ctx.on_tool_call_async(Hanging)?;
            Ok(Effect::Done)
        })
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_policy_that_times_out_denies() {
    let (root, bridge) = setup().await;
    let bridge = bridge.with_timeout(Duration::from_millis(100));
    let hanging = root.plugin(HangingPolicyPlugin);
    wait_active(&hanging).await;
    let (agent, _) = agent(vec![call("act", serde_json::json!({})), text("done")]);
    let mut agent = agent
        .with_tools(vec![Box::new(Reply::new("act", "acted"))])
        .with_rutis(&bridge);
    let (_, results) = tokio::time::timeout(Duration::from_secs(5), run(&mut agent, "go"))
        .await
        .expect("the timeout bounds the run");
    assert!(results[0].2);
    assert!(results[0].1.contains("did not answer"), "{results:?}");
    // The timed-out dispatch was dropped with the middleware's future, so
    // nothing holds the root open.
    drop(hanging);
    root.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_turn_note_reaches_the_request_but_not_history() {
    let (root, bridge) = setup().await;
    let noter = root.plugin(
        AgentPlugin::new("noter")
            .with_turn_note(|turn| Some(format!("[note for {}]", turn.model())))
            .with_turn_note(|turn| {
                turn.user_request()
                    .filter(|r| r.contains("deploy"))
                    .map(|_| "[deploys need approval]".to_string())
            }),
    );
    wait_active(&noter).await;
    let (agent, seen) = agent(vec![text("done")]);
    let mut agent = agent.with_rutis(&bridge);
    run(&mut agent, "please deploy").await;
    let last_user = seen.lock().unwrap()[0].last_user.clone();
    assert!(last_user.starts_with("please deploy|"), "{last_user}");
    assert!(last_user.contains("[note for mock]"), "{last_user}");
    assert!(last_user.contains("[deploys need approval]"), "{last_user}");
    let stored = format!("{:?}", agent.messages());
    assert!(!stored.contains("deploys need approval"), "never stored");
    root.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_panicking_turn_listener_fails_open() {
    let (root, bridge) = setup().await;
    let first = root.plugin(AgentPlugin::new("first").with_turn_note(|_| Some("[kept]".into())));
    wait_active(&first).await;
    let second = root.plugin(AgentPlugin::new("second").with_turn_note(|_| panic!("note bug")));
    wait_active(&second).await;
    let (agent, seen) = agent(vec![text("done")]);
    let mut agent = agent.with_rutis(&bridge);
    run(&mut agent, "hi").await;
    let seen = seen.lock().unwrap().clone();
    assert_eq!(seen.len(), 1, "the request still went out");
    assert_eq!(seen[0].last_user, "hi|[kept]", "the earlier note survives");
    root.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn an_input_plugin_rejects_a_prompt() {
    let (root, bridge) = setup().await;
    let filter = root.plugin(AgentPlugin::new("filter").with_input_check(|input| {
        input
            .text()
            .contains("rm -rf")
            .then(|| "destructive request refused".to_string())
    }));
    wait_active(&filter).await;
    let (agent, seen) = agent(vec![text("fine")]);
    let mut agent = agent.with_rutis(&bridge);

    let (events, _) = run(&mut agent, "please rm -rf /").await;
    assert!(events.iter().any(|e| matches!(
        e,
        AgentEvent::InputRejected { reason } if reason == "destructive request refused"
    )));
    assert!(seen.lock().unwrap().is_empty(), "the model never saw it");

    let (events, _) = run(&mut agent, "list files").await;
    assert!(!events
        .iter()
        .any(|e| matches!(e, AgentEvent::InputRejected { .. })));
    assert_eq!(seen.lock().unwrap().len(), 1);
    root.shutdown().await.unwrap();
}

struct PanickingInput {
    injects: Vec<TypeKey>,
}

impl Plugin for PanickingInput {
    fn name(&self) -> &str {
        "panicking-input"
    }
    fn injects(&self) -> &[TypeKey] {
        &self.injects
    }
    fn apply<'a>(&'a self, ctx: &'a Ctx) -> BoxFuture<'a, Result<Effect, CordisError>> {
        Box::pin(async move {
            ctx.on_input(|_| panic!("filter bug"))?;
            Ok(Effect::Done)
        })
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_panicking_input_listener_rejects_fail_closed() {
    let (root, bridge) = setup().await;
    let p = root.plugin(PanickingInput {
        injects: vec![TypeKey::of::<ToolRegistry>()],
    });
    wait_active(&p).await;
    let (agent, seen) = agent(vec![text("fine")]);
    let mut agent = agent.with_rutis(&bridge);
    let (events, _) = run(&mut agent, "hello").await;
    assert!(events.iter().any(|e| matches!(
        e,
        AgentEvent::InputRejected { reason } if reason.contains("input filter failed")
    )));
    assert!(seen.lock().unwrap().is_empty());
    root.shutdown().await.unwrap();
}
