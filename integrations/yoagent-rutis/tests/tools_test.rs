//! Plugin tools: they appear and disappear with the plugin that provides
//! them, at run boundaries.

mod common;

use std::sync::atomic::Ordering;
use std::sync::Arc;

use common::*;
use rutis::{
    BoxFuture, CordisError, Ctx, Effect, FiberState, FiberView, Plugin, PluginFactory, TypeKey,
};
use yoagent::provider::mock::MockResponse;
use yoagent::{AgentTool, SubAgentTool, ToolContext, ToolError, ToolResult};
use yoagent_rutis::{
    AgentPlugin, AgentRutisExt, PluginCtxExt, RutisBridge, ToolRegistry, ToolVerdict,
};

#[tokio::test(flavor = "multi_thread")]
async fn a_plugin_tool_is_offered_while_the_plugin_is_loaded() {
    let root = Ctx::root().unwrap();
    let bridge = RutisBridge::install(&root).unwrap();
    let (agent, seen) = agent(vec![
        call("greet", serde_json::json!({})),
        text("done"),
        call("greet", serde_json::json!({})),
        text("done"),
    ]);
    let mut agent = agent.with_rutis(&bridge);

    let greet = Reply::new("greet", "hello from the plugin");
    let runs = greet.runs();
    let view = root.plugin(AgentPlugin::new("greeter").with_tool(greet));
    wait_active(&view).await;

    let (_, results) = run(&mut agent, "first").await;
    assert_eq!(
        results,
        vec![("greet".into(), "hello from the plugin".into(), false)]
    );
    assert_eq!(runs.load(Ordering::SeqCst), 1);
    assert_eq!(seen.lock().unwrap()[0].tools, vec!["greet".to_string()]);

    view.dispose().await.unwrap();
    assert!(bridge.registry().names().is_empty(), "unload removed it");

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
    let root = Ctx::root().unwrap();
    let bridge = RutisBridge::install(&root).unwrap();
    let greet = Reply::new("greet", "hello");
    let runs = greet.runs();
    let view = root.plugin(AgentPlugin::new("greeter").with_tool(greet));
    wait_active(&view).await;

    let (agent, seen) = agent(vec![
        call("unload", serde_json::json!({})),
        call("greet", serde_json::json!({})),
        text("done"),
    ]);
    let mut agent = agent
        .with_tools(vec![Box::new(Unload(view.clone()))])
        .with_rutis(&bridge);

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

#[tokio::test(flavor = "multi_thread")]
async fn two_plugins_contribute_and_a_taken_name_is_refused() {
    let root = Ctx::root().unwrap();
    let bridge = RutisBridge::install(&root).unwrap();
    let a = root.plugin(AgentPlugin::new("a").with_tool(Reply::new("alpha", "from a")));
    wait_active(&a).await;
    let b = root.plugin(AgentPlugin::new("b").with_tool(Reply::new("beta", "from b")));
    wait_active(&b).await;
    // A third plugin wants a name `a` holds: refused, so it fails to load.
    let c = root.plugin(AgentPlugin::new("c").with_tool(Reply::new("alpha", "from c")));
    wait_state(&c, FiberState::Failed).await;
    let failure = c.state().error.expect("failed with an error").to_string();
    assert!(failure.contains("alpha"), "{failure}");

    let (agent, seen) = agent(vec![
        MockResponse::ToolCalls(vec![
            yoagent::provider::mock::MockToolCall {
                provider_metadata: None,
                name: "alpha".into(),
                arguments: serde_json::json!({}),
            },
            yoagent::provider::mock::MockToolCall {
                provider_metadata: None,
                name: "beta".into(),
                arguments: serde_json::json!({}),
            },
            yoagent::provider::mock::MockToolCall {
                provider_metadata: None,
                name: "own".into(),
                arguments: serde_json::json!({}),
            },
        ]),
        text("done"),
    ]);
    // Plugin `d` offers a tool named like the agent's own `own`: the registry
    // accepts it (no other plugin holds the name), and at run time the
    // agent's own tool wins.
    let d = root.plugin(AgentPlugin::new("d").with_tool(Reply::new("own", "from d")));
    wait_active(&d).await;
    let mut agent = agent
        .with_tools(vec![Box::new(Reply::new("own", "static wins"))])
        .with_rutis(&bridge);

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

/// A service plugin B depends on.
struct Backend(String);

struct BackendPlugin(&'static str);

impl Plugin for BackendPlugin {
    fn name(&self) -> &str {
        "backend"
    }
    fn apply<'a>(&'a self, ctx: &'a Ctx) -> BoxFuture<'a, Result<Effect, CordisError>> {
        Box::pin(async move {
            ctx.provide(Backend(self.0.into()))?;
            Ok(Effect::Done)
        })
    }
}

/// Provides a tool that reports the backend it was loaded against.
struct Consumer {
    injects: Vec<TypeKey>,
}

impl Plugin for Consumer {
    fn name(&self) -> &str {
        "consumer"
    }
    fn injects(&self) -> &[TypeKey] {
        &self.injects
    }
    fn apply<'a>(&'a self, ctx: &'a Ctx) -> BoxFuture<'a, Result<Effect, CordisError>> {
        Box::pin(async move {
            let backend = ctx.require::<Backend>()?;
            ctx.provide_tool(Reply::new("query", &format!("answered by {}", backend.0)))?;
            Ok(Effect::Done)
        })
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_dependency_going_away_takes_the_dependents_tools_with_it() {
    let root = Ctx::root().unwrap();
    let bridge = RutisBridge::install(&root).unwrap();
    let consumer = root.plugin(Consumer {
        injects: vec![TypeKey::of::<ToolRegistry>(), TypeKey::of::<Backend>()],
    });
    (&consumer).await.unwrap();
    assert_eq!(consumer.state().state, FiberState::Pending);
    assert!(bridge.registry().names().is_empty());

    let v1 = root.plugin(BackendPlugin("v1"));
    wait_active(&consumer).await;
    assert_eq!(bridge.registry().names(), vec!["query".to_string()]);

    // Unloading the provider evicts the consumer, and its tool with it.
    v1.dispose().await.unwrap();
    wait_state(&consumer, FiberState::Pending).await;
    assert!(bridge.registry().names().is_empty());

    let (agent, seen) = agent(vec![
        text("nothing to do"),
        call("query", serde_json::json!({})),
        text("done"),
    ]);
    let mut agent = agent.with_rutis(&bridge);
    run(&mut agent, "while the backend is down").await;
    assert!(seen.lock().unwrap()[0].tools.is_empty());

    // A new provider brings the consumer — and its tool — back.
    let _v2 = root.plugin(BackendPlugin("v2"));
    wait_active(&consumer).await;
    let (_, results) = run(&mut agent, "and now").await;
    assert_eq!(
        results,
        vec![("query".into(), "answered by v2".into(), false)]
    );
    root.shutdown().await.unwrap();
}

/// Builds a plugin whose tool replies with the current config.
struct GreeterFactory;

impl PluginFactory<String> for GreeterFactory {
    fn name(&self) -> &str {
        "configurable-greeter"
    }
    fn injects(&self) -> &[TypeKey] {
        static KEYS: std::sync::OnceLock<Vec<TypeKey>> = std::sync::OnceLock::new();
        KEYS.get_or_init(|| vec![TypeKey::of::<ToolRegistry>()])
    }
    fn build(&self, greeting: &String) -> Result<Box<dyn Plugin>, CordisError> {
        Ok(Box::new(
            AgentPlugin::new("configurable-greeter").with_tool(Reply::new("greet", greeting)),
        ))
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_config_update_changes_the_tool_between_runs() {
    let root = Ctx::root().unwrap();
    let bridge = RutisBridge::install(&root).unwrap();
    let view = root.plugin_with(GreeterFactory, "hello".to_string());
    wait_active(&view).await;

    let (agent, _) = agent(vec![
        call("greet", serde_json::json!({})),
        text("done"),
        call("greet", serde_json::json!({})),
        text("done"),
    ]);
    let mut agent = agent.with_rutis(&bridge);
    let (_, results) = run(&mut agent, "first").await;
    assert_eq!(results, vec![("greet".into(), "hello".into(), false)]);

    view.update("bonjour".to_string()).await.unwrap();
    wait_active(&view).await;
    let (_, results) = run(&mut agent, "second").await;
    assert_eq!(results, vec![("greet".into(), "bonjour".into(), false)]);
    root.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_sub_agent_attached_to_the_bridge_gets_tools_policy_and_input_filter() {
    let root = Ctx::root().unwrap();
    let bridge = RutisBridge::install(&root).unwrap();
    let greet = Reply::new("greet", "hi");
    let greet_runs = greet.runs();
    let secret = Reply::new("secret", "leaked");
    let secret_runs = secret.runs();
    let view = root.plugin(
        AgentPlugin::new("child-plugin")
            .with_tool(greet)
            .with_tool(secret)
            .with_policy(|call| match call.tool_name() {
                "secret" => ToolVerdict::deny("secret is off limits"),
                _ => ToolVerdict::Allow,
            })
            .with_input_check(|input| {
                input
                    .text()
                    .contains("forbidden")
                    .then(|| "forbidden task".to_string())
            }),
    );
    wait_active(&view).await;

    let (child, child_seen) = recording(vec![
        call("greet", serde_json::json!({})),
        call("secret", serde_json::json!({})),
        text("child done"),
    ]);
    let sub = SubAgentTool::from_provider("helper", child, yoagent::provider::ModelConfig::mock())
        .with_rutis(&bridge);
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
    assert_eq!(
        greet_runs.load(Ordering::SeqCst),
        1,
        "the allowed plugin tool ran"
    );
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
async fn installing_twice_shares_one_registry_and_provide_tool_without_a_bridge_fails() {
    let root = Ctx::root().unwrap();
    // No bridge yet: a plugin that does not wait for it fails to load.
    struct Eager;
    impl Plugin for Eager {
        fn name(&self) -> &str {
            "eager"
        }
        fn apply<'a>(&'a self, ctx: &'a Ctx) -> BoxFuture<'a, Result<Effect, CordisError>> {
            Box::pin(async move {
                ctx.provide_tool(Reply::new("x", "x"))?;
                Ok(Effect::Done)
            })
        }
    }
    let eager = root.plugin(Eager);
    wait_state(&eager, FiberState::Failed).await;

    let first = RutisBridge::install(&root).unwrap();
    let second = RutisBridge::install(&root).unwrap();
    assert!(Arc::ptr_eq(first.registry(), second.registry()));
    root.shutdown().await.unwrap();
}
