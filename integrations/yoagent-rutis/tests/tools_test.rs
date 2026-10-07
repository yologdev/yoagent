//! Plugin tools: they appear and disappear with the plugin that provides
//! them, at run boundaries; sub-agents get plugin hooks through the
//! extension.

mod common;

use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};

use common::*;
use rutis::{Ctx, FiberState, FiberView, TypeKey};
use yoagent::extension::InputDecision;
use yoagent::provider::mock::{MockResponse, MockToolCall};
use yoagent::{AgentTool, SubAgentTool, ToolContext, ToolDecision, ToolError, ToolResult};
use yoagent_rutis::{PluginCtxExt, RutisBridge};

#[tokio::test(flavor = "multi_thread")]
async fn a_plugin_tool_is_offered_while_the_plugin_is_loaded() {
    let (root, bridge) = setup();
    let (agent, seen) = agent(vec![
        call("greet", serde_json::json!({})),
        text("done"),
        call("greet", serde_json::json!({})),
        text("done"),
    ]);
    let mut agent = agent.with_extension(bridge.extension());

    let greet = Reply::new("greet", "hello from the plugin");
    let runs = greet.runs();
    let view = root.plugin(plugin(handler("greeter").with_tool(greet)));
    wait_active(&view).await;

    let (_, results) = run(&mut agent, "first").await;
    assert_eq!(
        results,
        vec![("greet".into(), "hello from the plugin".into(), false)]
    );
    assert_eq!(runs.load(Ordering::SeqCst), 1);
    assert_eq!(seen.lock().unwrap()[0].tools, vec!["greet".to_string()]);

    view.dispose().await.unwrap();
    assert!(
        bridge.registry().tool_names().is_empty(),
        "unload removed it"
    );
    assert!(bridge.registry().handlers().is_empty());

    let (_, results) = run(&mut agent, "second").await;
    let requests = seen.lock().unwrap().clone();
    assert!(
        requests[2..].iter().all(|r| r.tools.is_empty()),
        "the next run no longer offers it: {requests:?}"
    );
    assert_eq!(results.len(), 1);
    assert!(results[0].2, "a stale call is an error result");
    assert!(results[0].1.contains("not found"), "{results:?}");
    assert_eq!(runs.load(Ordering::SeqCst), 1, "the tool did not run again");
    root.shutdown().await.unwrap();
}

/// Disposes a plugin from inside a run (a static tool the model calls).
struct Unload(FiberView);

#[async_trait::async_trait]
impl AgentTool for Unload {
    fn name(&self) -> &str {
        "unload"
    }
    fn label(&self) -> &str {
        "unload"
    }
    fn description(&self) -> &str {
        "unloads the greeter plugin"
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({"type": "object", "properties": {}})
    }
    async fn execute(
        &self,
        _params: serde_json::Value,
        _ctx: ToolContext,
    ) -> Result<ToolResult, ToolError> {
        self.0
            .dispose()
            .await
            .map_err(|e| ToolError::Failed(e.to_string()))?;
        Ok(ToolResult {
            content: vec![],
            details: serde_json::Value::Null,
        })
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_tool_whose_plugin_unloads_mid_run_fails_cleanly() {
    let (root, bridge) = setup();
    let greet = Reply::new("greet", "hello");
    let runs = greet.runs();
    let view = root.plugin(plugin(handler("greeter").with_tool(greet)));
    wait_active(&view).await;

    let (agent, seen) = agent(vec![
        call("unload", serde_json::json!({})),
        call("greet", serde_json::json!({})),
        text("done"),
    ]);
    let mut agent = agent
        .with_tools(vec![Box::new(Unload(view.clone()))])
        .with_extension(bridge.extension());

    let (_, results) = run(&mut agent, "go").await;
    // Per run: still offered on the turn after the unload...
    assert!(seen.lock().unwrap()[1].tools.contains(&"greet".to_string()));
    // ...but calling it no longer reaches the plugin.
    assert_eq!(results[1].0, "greet");
    assert!(results[1].2);
    assert!(results[1].1.contains("no longer available"), "{results:?}");
    assert_eq!(runs.load(Ordering::SeqCst), 0);
    root.shutdown().await.unwrap();
}

fn calls(names: &[&str]) -> MockResponse {
    MockResponse::ToolCalls(
        names
            .iter()
            .map(|name| MockToolCall {
                provider_metadata: None,
                name: (*name).into(),
                arguments: serde_json::json!({}),
            })
            .collect(),
    )
}

#[tokio::test(flavor = "multi_thread")]
async fn two_plugins_contribute_and_a_taken_name_is_refused() {
    let (root, bridge) = setup();
    let a = root.plugin(plugin(
        handler("a").with_tool(Reply::new("alpha", "from a")),
    ));
    wait_active(&a).await;
    let b = root.plugin(plugin(handler("b").with_tool(Reply::new("beta", "from b"))));
    wait_active(&b).await;
    // A third plugin wants a tool name `a` holds: refused, so it fails to load.
    let c = root.plugin(plugin(
        handler("c").with_tool(Reply::new("alpha", "from c")),
    ));
    wait_state(&c, FiberState::Failed).await;
    let failure = c.state().error.expect("failed with an error").to_string();
    assert!(failure.contains("alpha"), "{failure}");

    let (agent, seen) = agent(vec![calls(&["alpha", "beta", "own"]), text("done")]);
    // Plugin `d` offers a tool named like the agent's own `own`: the registry
    // accepts it (no other plugin holds the name), and at run time the
    // agent's own tool wins.
    let d = root.plugin(plugin(handler("d").with_tool(Reply::new("own", "from d"))));
    wait_active(&d).await;
    let mut agent = agent
        .with_tools(vec![Box::new(Reply::new("own", "static wins"))])
        .with_extension(bridge.extension());

    let (_, results) = run(&mut agent, "go").await;
    assert_eq!(
        seen.lock().unwrap()[0].tools,
        vec!["own".to_string(), "alpha".into(), "beta".into()]
    );
    assert_eq!(
        results,
        vec![
            ("alpha".into(), "from a".into(), false),
            ("beta".into(), "from b".into(), false),
            ("own".into(), "static wins".into(), false),
        ]
    );
    root.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_taken_handler_name_is_refused() {
    let (root, bridge) = setup();
    let first = root.plugin(Setup::new("first", |ctx| {
        ctx.register_handler(handler("policy")).map(drop)
    }));
    wait_active(&first).await;
    let second = root.plugin(Setup::new("second", |ctx| {
        ctx.register_handler(handler("policy")).map(drop)
    }));
    wait_state(&second, FiberState::Failed).await;
    let failure = second.state().error.unwrap().to_string();
    assert!(
        failure.contains("policy") && failure.contains("first"),
        "names the handler and its holder: {failure}"
    );
    let handlers = bridge.registry().handlers();
    assert_eq!(handlers.len(), 1);
    assert_eq!(handlers[0].owner(), "first");
    root.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_tools_hook_offers_tools_per_run_and_earlier_handlers_win_a_clash() {
    let (root, bridge) = setup();
    let generation = Arc::new(Mutex::new("v1".to_string()));
    let dynamic = root.plugin(plugin(handler("dynamic").with_tools({
        let generation = generation.clone();
        move |_run| {
            let reply = generation.lock().unwrap().clone();
            vec![Arc::new(Reply::new("probe", &reply)) as Arc<dyn AgentTool>]
        }
    })));
    wait_active(&dynamic).await;
    // A later handler offering the same name loses (no registration check
    // for per-run tools).
    let later = root.plugin(plugin(
        handler("later").with_tools(|_| vec![Arc::new(Reply::new("probe", "later")) as _]),
    ));
    wait_active(&later).await;

    let (agent, seen) = agent(vec![
        call("probe", serde_json::json!({})),
        text("done"),
        call("probe", serde_json::json!({})),
        text("done"),
    ]);
    let mut agent = agent.with_extension(bridge.extension());
    let (_, results) = run(&mut agent, "first").await;
    assert_eq!(results, vec![("probe".into(), "v1".into(), false)]);
    assert_eq!(seen.lock().unwrap()[0].tools, vec!["probe".to_string()]);

    *generation.lock().unwrap() = "v2".into();
    let (_, results) = run(&mut agent, "second").await;
    assert_eq!(results, vec![("probe".into(), "v2".into(), false)]);
    root.shutdown().await.unwrap();
}

/// A service plugin B depends on.
struct Backend(String);

fn backend(version: &'static str) -> Setup {
    Setup::new("backend", move |ctx| {
        ctx.provide(Backend(version.into())).map(drop)
    })
}

/// Provides a tool that reports the backend it was loaded against.
fn consumer() -> Setup {
    Setup::new("consumer", |ctx| {
        let backend = ctx.require::<Backend>()?;
        ctx.provide_tool(Reply::new("query", &format!("answered by {}", backend.0)))
            .map(drop)
    })
    .with_inject(TypeKey::of::<Backend>())
}

#[tokio::test(flavor = "multi_thread")]
async fn a_dependency_going_away_takes_the_dependents_tools_with_it() {
    let (root, bridge) = setup();
    let consumer = root.plugin(consumer());
    (&consumer).await.unwrap();
    assert_eq!(consumer.state().state, FiberState::Pending);
    assert!(bridge.registry().tool_names().is_empty());

    let v1 = root.plugin(backend("v1"));
    wait_active(&consumer).await;
    assert_eq!(bridge.registry().tool_names(), vec!["query".to_string()]);

    // Unloading the provider evicts the consumer, and its tool with it.
    v1.dispose().await.unwrap();
    wait_state(&consumer, FiberState::Pending).await;
    assert!(bridge.registry().tool_names().is_empty());

    let (agent, seen) = agent(vec![
        text("nothing to do"),
        call("query", serde_json::json!({})),
        text("done"),
    ]);
    let mut agent = agent.with_extension(bridge.extension());
    run(&mut agent, "while the backend is down").await;
    assert!(seen.lock().unwrap()[0].tools.is_empty());

    // A new provider brings the consumer — and its tool — back.
    let _v2 = root.plugin(backend("v2"));
    wait_active(&consumer).await;
    let (_, results) = run(&mut agent, "and now").await;
    assert_eq!(
        results,
        vec![("query".into(), "answered by v2".into(), false)]
    );
    root.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_config_update_changes_the_tool_between_runs() {
    let (root, bridge) = setup();
    let view = root.plugin_with(GreeterFactory, "hello".to_string());
    wait_active(&view).await;

    let (agent, _) = agent(vec![
        call("greet", serde_json::json!({})),
        text("done"),
        call("greet", serde_json::json!({})),
        text("done"),
    ]);
    let mut agent = agent.with_extension(bridge.extension());
    let (_, results) = run(&mut agent, "first").await;
    assert_eq!(results, vec![("greet".into(), "hello".into(), false)]);

    view.update("bonjour".to_string()).await.unwrap();
    wait_active(&view).await;
    let (_, results) = run(&mut agent, "second").await;
    assert_eq!(results, vec![("greet".into(), "bonjour".into(), false)]);
    root.shutdown().await.unwrap();
}

/// The plugin used by the sub-agent tests: two tools, a policy denying one
/// of them (and the child's own `act`), an input check.
fn child_plugin(greet: Reply, secret: Reply) -> yoagent_rutis::AgentPlugin {
    plugin(
        handler("child-plugin")
            .with_tool(greet)
            .with_tool(secret)
            .with_before_tool(|call| match call.tool.as_str() {
                "secret" | "act" => ToolDecision::Deny(format!("{} is off limits", call.tool)),
                _ => ToolDecision::Allow,
            })
            .with_on_input(|input| {
                if input.text.contains("forbidden") {
                    InputDecision::Reject("forbidden task".into())
                } else {
                    InputDecision::Pass
                }
            }),
    )
}

#[tokio::test(flavor = "multi_thread")]
async fn a_sub_agent_with_the_extension_gets_tools_policy_and_input_checks() {
    let (root, bridge) = setup();
    let greet = Reply::new("greet", "hi");
    let greet_runs = greet.runs();
    let secret = Reply::new("secret", "leaked");
    let secret_runs = secret.runs();
    let view = root.plugin(child_plugin(greet, secret));
    wait_active(&view).await;

    let (child, child_seen) = recording(vec![
        call("greet", serde_json::json!({})),
        call("secret", serde_json::json!({})),
        text("child done"),
    ]);
    let sub = SubAgentTool::from_provider("helper", child, yoagent::provider::ModelConfig::mock())
        .with_extension(bridge.extension());
    let (parent, _) = agent(vec![
        call("helper", serde_json::json!({"task": "greet"})),
        call(
            "helper",
            serde_json::json!({"task": "do the forbidden thing"}),
        ),
        text("done"),
    ]);
    let mut parent = parent.with_sub_agent(sub);
    let (_, results) = run(&mut parent, "delegate").await;

    let child_seen = child_seen.lock().unwrap().clone();
    assert_eq!(
        child_seen[0].tools,
        vec!["greet".to_string(), "secret".into()]
    );
    assert_eq!(greet_runs.load(Ordering::SeqCst), 1, "the allowed tool ran");
    assert_eq!(
        secret_runs.load(Ordering::SeqCst),
        0,
        "the policy denied inside the child"
    );
    assert_eq!(
        child_seen.len(),
        3,
        "the rejected task never reached the child model"
    );
    assert!(!results[0].2, "{results:?}");
    assert!(results[1].2, "the rejected delegation fails: {results:?}");
    assert!(results[1].1.contains("forbidden task"), "{results:?}");
    root.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_tree_extension_carries_plugin_policy_into_sub_agents_without_its_tools() {
    let (root, bridge) = setup();
    let greet = Reply::new("greet", "hi");
    let secret = Reply::new("secret", "leaked");
    let view = root.plugin(child_plugin(greet, secret));
    wait_active(&view).await;

    // The child has a tool of its own, `act`, which the plugin denies; the
    // parent installs the bridge for its whole tree, the child nothing.
    let act = Reply::new("act", "acted");
    let act_runs = act.runs();
    let (child, child_seen) =
        recording(vec![call("act", serde_json::json!({})), text("child done")]);
    let sub = SubAgentTool::from_provider("helper", child, yoagent::provider::ModelConfig::mock())
        .with_tools(vec![Arc::new(act)]);
    let (parent, parent_seen) = agent(vec![
        call("helper", serde_json::json!({"task": "act"})),
        call("helper", serde_json::json!({"task": "the forbidden one"})),
        text("done"),
    ]);
    let mut parent = parent
        .with_sub_agent(sub)
        .with_tree_extension(bridge.extension());
    let (_, results) = run(&mut parent, "delegate").await;

    assert!(
        parent_seen.lock().unwrap()[0]
            .tools
            .contains(&"greet".to_string()),
        "the parent's run is offered the plugin tools"
    );
    let child_seen = child_seen.lock().unwrap().clone();
    assert_eq!(
        child_seen[0].tools,
        vec!["act".to_string()],
        "a tree extension's tools are not offered to child runs"
    );
    assert_eq!(act_runs.load(Ordering::SeqCst), 0, "denied in the child");
    assert!(
        !results[0].2,
        "the first delegation itself succeeds: {results:?}"
    );
    assert!(results[1].2, "the child's input was rejected: {results:?}");
    assert!(results[1].1.contains("forbidden task"), "{results:?}");
    root.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn installing_twice_shares_one_registry_and_registering_without_a_bridge_fails() {
    let root = Ctx::root().unwrap();
    // No bridge yet: a plugin that does not wait for it fails to load.
    let eager = root.plugin(
        Setup::new("eager", |ctx| {
            ctx.provide_tool(Reply::new("x", "x")).map(drop)
        })
        .eager(),
    );
    wait_state(&eager, FiberState::Failed).await;

    let first = RutisBridge::install(&root).unwrap();
    let second = RutisBridge::install(&root).unwrap();
    assert!(Arc::ptr_eq(first.registry(), second.registry()));
    root.shutdown().await.unwrap();
}
