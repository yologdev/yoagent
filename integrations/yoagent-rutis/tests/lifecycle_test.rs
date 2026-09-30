//! Plugin tools across unloads that race a run: in-flight calls, failed
//! registrations, config updates mid-run.

mod common;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use common::*;
use rutis::{
    BoxFuture, CordisError, Ctx, Effect, FiberState, FiberView, Plugin, PluginFactory, TypeKey,
};
use tokio::sync::Notify;
use yoagent::{AgentTool, ToolContext, ToolError, ToolResult};
use yoagent_rutis::{AgentPlugin, AgentRutisExt, PluginCtxExt, RutisBridge, ToolRegistry};

/// Blocks for a long time; signals when it started.
struct Blocking {
    started: Arc<Notify>,
    finished: Arc<AtomicBool>,
}

#[async_trait::async_trait]
impl AgentTool for Blocking {
    fn name(&self) -> &str {
        "slow"
    }
    fn label(&self) -> &str {
        "slow"
    }
    fn description(&self) -> &str {
        "takes forever"
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({"type": "object", "properties": {}})
    }
    async fn execute(
        &self,
        _params: serde_json::Value,
        _ctx: ToolContext,
    ) -> Result<ToolResult, ToolError> {
        self.started.notify_one();
        tokio::time::sleep(Duration::from_secs(30)).await;
        self.finished.store(true, Ordering::SeqCst);
        Ok(ToolResult {
            content: vec![],
            details: serde_json::Value::Null,
        })
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_call_in_flight_when_its_plugin_unloads_fails() {
    let root = Ctx::root().unwrap();
    let bridge = RutisBridge::install(&root).unwrap();
    let started = Arc::new(Notify::new());
    let finished = Arc::new(AtomicBool::new(false));
    let view = root.plugin(AgentPlugin::new("slow").with_tool(Blocking {
        started: started.clone(),
        finished: finished.clone(),
    }));
    wait_active(&view).await;
    let (agent, _) = agent(vec![call("slow", serde_json::json!({})), text("done")]);
    let mut agent = agent.with_rutis(&bridge);

    let unload = tokio::spawn({
        let view = view.clone();
        async move {
            started.notified().await;
            view.dispose().await.unwrap();
        }
    });
    let (_, results) = tokio::time::timeout(Duration::from_secs(10), run(&mut agent, "go"))
        .await
        .expect("the unload ends the call");
    unload.await.unwrap();
    assert_eq!(results.len(), 1);
    assert!(results[0].2, "an error result, not a success: {results:?}");
    assert!(
        results[0].1.contains("unloaded during the call"),
        "{results:?}"
    );
    assert!(!finished.load(Ordering::SeqCst));
    root.shutdown().await.unwrap();
}

/// Keeps a copy of its apply context, to register through it later.
struct Keeper {
    kept: Arc<Mutex<Option<Ctx>>>,
    injects: Vec<TypeKey>,
}

impl Plugin for Keeper {
    fn name(&self) -> &str {
        "keeper"
    }
    fn injects(&self) -> &[TypeKey] {
        &self.injects
    }
    fn apply<'a>(&'a self, ctx: &'a Ctx) -> BoxFuture<'a, Result<Effect, CordisError>> {
        Box::pin(async move {
            *self.kept.lock().unwrap() = Some(ctx.clone());
            Ok(Effect::Done)
        })
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_failed_registration_leaves_no_name_locked() {
    let root = Ctx::root().unwrap();
    let bridge = RutisBridge::install(&root).unwrap();

    // 1. Through a context whose plugin has unloaded: refused, not kept.
    let kept = Arc::new(Mutex::new(None));
    let keeper = root.plugin(Keeper {
        kept: kept.clone(),
        injects: vec![TypeKey::of::<ToolRegistry>()],
    });
    wait_active(&keeper).await;
    keeper.dispose().await.unwrap();
    let stale = kept.lock().unwrap().clone().unwrap();
    assert!(stale.provide_tool(Reply::new("gamma", "stale")).is_err());
    assert!(
        bridge.registry().names().is_empty(),
        "nothing stays registered"
    );

    // 2. A plugin that registers `delta`, then fails on a taken name: its
    //    `delta` goes with it.
    let holder = root.plugin(AgentPlugin::new("holder").with_tool(Reply::new("alpha", "held")));
    wait_active(&holder).await;
    let loser = root.plugin(
        AgentPlugin::new("loser")
            .with_tool(Reply::new("delta", "from loser"))
            .with_tool(Reply::new("alpha", "clash")),
    );
    wait_state(&loser, FiberState::Failed).await;
    assert_eq!(bridge.registry().names(), vec!["alpha".to_string()]);

    // Both names are free for someone else.
    let other = root.plugin(
        AgentPlugin::new("other")
            .with_tool(Reply::new("gamma", "g"))
            .with_tool(Reply::new("delta", "d")),
    );
    wait_active(&other).await;
    assert_eq!(
        bridge.registry().names(),
        vec!["alpha".to_string(), "gamma".into(), "delta".into()]
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

/// Updates a factory plugin's config from inside a run.
struct Update(FiberView);

#[async_trait::async_trait]
impl AgentTool for Update {
    fn name(&self) -> &str {
        "update"
    }
    fn label(&self) -> &str {
        "update"
    }
    fn description(&self) -> &str {
        "reconfigures the greeter"
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
            .update("bonjour".to_string())
            .await
            .map_err(|e| ToolError::Failed(e.to_string()))?;
        Ok(ToolResult {
            content: vec![],
            details: serde_json::Value::Null,
        })
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_config_update_mid_run_does_not_rebind_the_runs_tool() {
    let root = Ctx::root().unwrap();
    let bridge = RutisBridge::install(&root).unwrap();
    let view = root.plugin_with(GreeterFactory, "hello".to_string());
    wait_active(&view).await;
    let (agent, _) = agent(vec![
        call("update", serde_json::json!({})),
        call("greet", serde_json::json!({})),
        text("done"),
        call("greet", serde_json::json!({})),
        text("done"),
    ]);
    let mut agent = agent
        .with_tools(vec![Box::new(Update(view.clone()))])
        .with_rutis(&bridge);
    let (_, results) = run(&mut agent, "first").await;
    assert!(!results[0].2, "the update ran: {results:?}");
    // The run holds the old generation's tool: it errors rather than
    // silently calling the new generation (whose schema may differ).
    assert!(results[1].2);
    assert!(results[1].1.contains("no longer available"), "{results:?}");
    wait_active(&view).await;
    let (_, results) = run(&mut agent, "second").await;
    assert_eq!(results, vec![("greet".into(), "bonjour".into(), false)]);
    root.shutdown().await.unwrap();
}
