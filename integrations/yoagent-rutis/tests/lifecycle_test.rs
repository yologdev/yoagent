//! Handlers across unloads and reloads that race a run: in-flight calls,
//! failed registrations, config updates, and the snapshot each run takes.

mod common;

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use common::*;
use rutis::{
    BoxFuture, CordisError, Ctx, Effect, FiberState, FiberView, Plugin, PluginFactory, TypeKey,
};
use tokio::sync::Notify;
use yoagent::{AgentTool, ToolContext, ToolDecision, ToolError, ToolResult};
use yoagent_rutis::{PluginCtxExt, Registry};

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
    let (root, bridge) = setup();
    let started = Arc::new(Notify::new());
    let finished = Arc::new(AtomicBool::new(false));
    let view = root.plugin(plugin(handler("slow").with_tool(Blocking {
        started: started.clone(),
        finished: finished.clone(),
    })));
    wait_active(&view).await;
    let (agent, _) = agent(vec![call("slow", serde_json::json!({})), text("done")]);
    let mut agent = agent.with_extension(bridge.extension());

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
    let (root, bridge) = setup();

    // 1. Through a context whose plugin has unloaded: refused, not kept.
    let kept = Arc::new(Mutex::new(None));
    let keeper = root.plugin(Keeper {
        kept: kept.clone(),
        injects: vec![TypeKey::of::<Registry>()],
    });
    wait_active(&keeper).await;
    keeper.dispose().await.unwrap();
    let stale = kept.lock().unwrap().clone().unwrap();
    assert!(stale.provide_tool(Reply::new("gamma", "stale")).is_err());
    assert!(
        bridge.registry().handlers().is_empty(),
        "nothing stays registered"
    );

    // 2. A plugin that registers `delta`, then fails on a taken name: its
    //    `delta` goes with it.
    let holder = root.plugin(plugin(
        handler("holder").with_tool(Reply::new("alpha", "held")),
    ));
    wait_active(&holder).await;
    let loser = root.plugin(Setup::new("loser", |ctx| {
        ctx.provide_tool(Reply::new("delta", "from loser"))?;
        ctx.provide_tool(Reply::new("alpha", "clash"))?;
        Ok(())
    }));
    wait_state(&loser, FiberState::Failed).await;
    assert_eq!(bridge.registry().tool_names(), vec!["alpha".to_string()]);

    // Both names are free for someone else.
    let other = root.plugin(plugin(
        handler("other")
            .with_tool(Reply::new("gamma", "g"))
            .with_tool(Reply::new("delta", "d")),
    ));
    wait_active(&other).await;
    assert_eq!(
        bridge.registry().tool_names(),
        vec!["alpha".to_string(), "gamma".into(), "delta".into()]
    );
    root.shutdown().await.unwrap();
}

/// Updates a factory plugin's config from inside a run.
struct Update<C: Clone + Send + Sync + 'static>(FiberView, C);

#[async_trait::async_trait]
impl<C: Clone + Send + Sync + 'static> AgentTool for Update<C> {
    fn name(&self) -> &str {
        "update"
    }
    fn label(&self) -> &str {
        "update"
    }
    fn description(&self) -> &str {
        "reconfigures a plugin"
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
            .update(self.1.clone())
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
    let (root, bridge) = setup();
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
        .with_tools(vec![Box::new(Update(view.clone(), "bonjour".to_string()))])
        .with_extension(bridge.extension());
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

#[tokio::test(flavor = "multi_thread")]
async fn a_run_starting_while_a_plugin_unloads_does_not_get_its_handler() {
    let (root, bridge) = setup();
    let gate = Arc::new(Notify::new());
    // The handler is registered first, so its cleanup runs last (LIFO):
    // while the gated cleanup blocks, the plugin is unloading — its
    // generation is cancelled — yet its entry is still in the registry.
    let view = root.plugin(Setup::new("slow-unload", {
        let gate = gate.clone();
        move |ctx| {
            ctx.register_handler(
                deny_all("slow-unload", "denied by an unloading plugin")
                    .with_tool(Reply::new("greet", "hi")),
            )?;
            let gate = gate.clone();
            ctx.effect(move || {
                Effect::AsyncDisposer(Box::new(move || {
                    Box::pin(async move {
                        gate.notified().await;
                        Ok(())
                    })
                }))
            })
            .map(drop)
        }
    }));
    wait_active(&view).await;
    assert_eq!(bridge.registry().tool_names(), vec!["greet".to_string()]);

    let disposing = tokio::spawn({
        let view = view.clone();
        async move { view.dispose().await }
    });
    wait_state(&view, FiberState::Unloading).await;
    let (agent, seen) = agent(vec![call("act", serde_json::json!({})), text("done")]);
    let mut agent = agent
        .with_tools(vec![Box::new(Reply::new("act", "acted"))])
        .with_extension(bridge.extension());
    let (_, results) = run(&mut agent, "go").await;
    assert!(
        seen.lock().unwrap()[0].tools == vec!["act".to_string()],
        "an unloading plugin's tools are not offered"
    );
    assert_eq!(
        results,
        vec![("act".into(), "acted".into(), false)],
        "nor does its policy judge"
    );

    gate.notify_one();
    disposing.await.unwrap().unwrap();
    root.shutdown().await.unwrap();
}

/// A policy whose verdict comes from its config: `true` allows, `false`
/// denies.
struct SwitchFactory;

impl PluginFactory<bool> for SwitchFactory {
    fn name(&self) -> &str {
        "switch"
    }
    fn injects(&self) -> &[TypeKey] {
        static KEYS: std::sync::OnceLock<Vec<TypeKey>> = std::sync::OnceLock::new();
        KEYS.get_or_init(|| vec![TypeKey::of::<Registry>()])
    }
    fn build(&self, allow: &bool) -> Result<Box<dyn Plugin>, CordisError> {
        let allow = *allow;
        Ok(Box::new(plugin(handler("switch").with_before_tool(
            move |_| {
                if allow {
                    ToolDecision::Allow
                } else {
                    ToolDecision::Deny("switched off".into())
                }
            },
        ))))
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_reload_between_runs_takes_effect_on_the_next_run() {
    let (root, bridge) = setup();
    let view = root.plugin_with(SwitchFactory, true);
    wait_active(&view).await;
    let tool = Reply::new("act", "acted");
    let runs = tool.runs();
    let (agent, _) = agent(vec![
        call("act", serde_json::json!({})),
        text("done"),
        call("act", serde_json::json!({})),
        text("done"),
        call("act", serde_json::json!({})),
        text("done"),
    ]);
    let mut agent = agent
        .with_tools(vec![Box::new(tool)])
        .with_extension(bridge.extension());

    let (_, results) = run(&mut agent, "one").await;
    assert!(!results[0].2, "{results:?}");

    view.update(false).await.unwrap();
    wait_active(&view).await;
    let (_, results) = run(&mut agent, "two").await;
    assert!(
        results[0].2 && results[0].1.contains("switched off"),
        "{results:?}"
    );

    view.update(true).await.unwrap();
    wait_active(&view).await;
    let (_, results) = run(&mut agent, "three").await;
    assert!(!results[0].2, "{results:?}");
    assert_eq!(runs.load(Ordering::SeqCst), 2);
    root.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_policy_reloaded_mid_run_is_unavailable_for_the_rest_of_that_run() {
    let (root, bridge) = setup();
    let view = root.plugin_with(SwitchFactory, true);
    wait_active(&view).await;
    let tool = Reply::new("act", "acted");
    let runs = tool.runs();
    let (agent, _) = agent(vec![
        call("update", serde_json::json!({})),
        call("act", serde_json::json!({})),
        text("done"),
        call("act", serde_json::json!({})),
        text("done"),
    ]);
    let mut agent = agent
        .with_tools(vec![Box::new(tool), Box::new(Update(view.clone(), false))])
        .with_extension(bridge.extension());

    // The update reloads the policy: the run holds the old generation's
    // handler, which is now unavailable — so its calls are denied, never
    // judged by a policy generation the run did not start with.
    let (_, results) = run(&mut agent, "go").await;
    assert!(!results[0].2, "the update itself was allowed: {results:?}");
    assert!(results[1].2, "{results:?}");
    assert!(results[1].1.contains("no longer available"), "{results:?}");
    assert_eq!(runs.load(Ordering::SeqCst), 0);

    // The next run snapshots the new generation, which denies.
    wait_active(&view).await;
    let (_, results) = run(&mut agent, "again").await;
    assert!(
        results[0].2 && results[0].1.contains("switched off"),
        "{results:?}"
    );
    root.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_policy_in_flight_when_its_plugin_unloads_denies() {
    let (root, bridge) = setup();
    let entered = Arc::new(Notify::new());
    let checks = Arc::new(AtomicUsize::new(0));
    let view = root.plugin(plugin(handler("slow-policy").with_before_tool_async({
        let entered = entered.clone();
        let checks = checks.clone();
        move |_| {
            let entered = entered.clone();
            checks.fetch_add(1, Ordering::SeqCst);
            async move {
                entered.notify_one();
                std::future::pending::<()>().await;
                Ok(ToolDecision::Allow)
            }
        }
    })));
    wait_active(&view).await;
    let tool = Reply::new("act", "acted");
    let runs = tool.runs();
    let (agent, _) = agent(vec![call("act", serde_json::json!({})), text("done")]);
    let mut agent = agent
        .with_tools(vec![Box::new(tool)])
        .with_extension(bridge.extension());
    let unload = tokio::spawn({
        let view = view.clone();
        async move {
            entered.notified().await;
            view.dispose().await.unwrap();
        }
    });
    let (_, results) = tokio::time::timeout(Duration::from_secs(10), run(&mut agent, "go"))
        .await
        .expect("the unload ends the policy call");
    unload.await.unwrap();
    assert_eq!(checks.load(Ordering::SeqCst), 1);
    assert!(results[0].2, "{results:?}");
    assert!(results[0].1.contains("unloaded during"), "{results:?}");
    assert_eq!(runs.load(Ordering::SeqCst), 0);
    root.shutdown().await.unwrap();
}
