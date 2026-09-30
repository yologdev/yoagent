//! The edges of the policy / input / turn chains: host shutdown, empty
//! chains during reloads, short-circuits, timeouts, and what listeners see.

mod common;

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use common::*;
use rutis::{
    BoxFuture, CordisError, Ctx, Effect, EventKey, FiberState, Listener, Next, Plugin,
    PluginFactory, TypeKey, WaterfallListener,
};
use tokio::sync::Notify;
use yoagent::AgentEvent;
use yoagent_rutis::{
    AgentPlugin, AgentRutisExt, InputEvent, PluginCtxExt, RutisBridge, ToolCallEvent, ToolRegistry,
    ToolVerdict, TurnEvent,
};

fn deny_all() -> AgentPlugin {
    AgentPlugin::new("deny-all").with_policy(|_| ToolVerdict::deny("nothing may run"))
}

#[tokio::test(flavor = "multi_thread")]
async fn after_the_host_shuts_down_calls_are_denied_and_prompts_rejected() {
    let root = Ctx::root().unwrap();
    let bridge = RutisBridge::install(&root).unwrap();
    let policy = root.plugin(deny_all());
    wait_active(&policy).await;
    let tool = Reply::new("act", "acted");
    let runs = tool.runs();
    let (agent, seen) = agent(vec![call("act", serde_json::json!({})), text("done")]);
    let mut agent = agent.with_tools(vec![Box::new(tool)]).with_rutis(&bridge);

    root.shutdown().await.unwrap();

    // The prompt is rejected before the model sees it...
    let (events, _) = run(&mut agent, "go").await;
    assert!(
        events.iter().any(|e| matches!(
            e,
            AgentEvent::InputRejected { reason } if reason.contains("shut down")
        )),
        "{events:?}"
    );
    assert!(seen.lock().unwrap().is_empty());

    // ...and a tool call is denied, even though the deny-all listener was
    // drained by the shutdown (an empty chain would otherwise allow).
    let middleware = bridge.tool_middleware();
    let event = ToolCallEvent::new("c1", "act", serde_json::json!({}));
    let verdict = middleware.judge(&event).await;
    assert!(
        matches!(&verdict, ToolVerdict::Deny(r) if r.contains("shut down")),
        "{verdict:?}"
    );
    assert_eq!(runs.load(Ordering::SeqCst), 0);
}

/// A policy whose `apply` waits for a gate before registering its listener,
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
            ctx.on_tool_call(|_| ToolVerdict::Allow)?;
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
            injects: vec![TypeKey::of::<ToolRegistry>()],
        }))
    }
}

async fn verdict_during_reload(bridge: RutisBridge) -> ToolVerdict {
    let root = bridge.ctx().clone();
    let gate = Arc::new(Notify::new());
    let view = root.plugin_with(GatedPolicyFactory { gate: gate.clone() }, 1u32);
    wait_active(&view).await;
    let middleware = bridge.tool_middleware();
    let before = middleware
        .judge(&ToolCallEvent::new("c0", "act", serde_json::json!({})))
        .await;
    assert_eq!(before, ToolVerdict::Allow, "the loaded policy allows");

    // Update: the old listener is drained, the new generation waits at the
    // gate — the chain is empty now.
    let updating = tokio::spawn({
        let view = view.clone();
        async move { view.update(2u32).await }
    });
    wait_state(&view, FiberState::Loading).await;
    let during = middleware
        .judge(&ToolCallEvent::new("c1", "act", serde_json::json!({})))
        .await;
    gate.notify_one();
    updating.await.unwrap().unwrap();
    wait_active(&view).await;
    let after = middleware
        .judge(&ToolCallEvent::new("c2", "act", serde_json::json!({})))
        .await;
    assert_eq!(after, ToolVerdict::Allow, "the reloaded policy allows");
    root.shutdown().await.unwrap();
    during
}

#[tokio::test(flavor = "multi_thread")]
async fn by_default_the_reload_window_allows() {
    let root = Ctx::root().unwrap();
    let bridge = RutisBridge::install(&root).unwrap();
    assert_eq!(verdict_during_reload(bridge).await, ToolVerdict::Allow);
}

#[tokio::test(flavor = "multi_thread")]
async fn require_policy_denies_during_the_reload_window() {
    let root = Ctx::root().unwrap();
    let bridge = RutisBridge::install(&root).unwrap().require_policy();
    let verdict = verdict_during_reload(bridge).await;
    assert!(
        matches!(&verdict, ToolVerdict::Deny(r) if r.contains("no plugin policy")),
        "{verdict:?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn require_policy_denies_when_no_policy_was_ever_loaded() {
    let root = Ctx::root().unwrap();
    let bridge = RutisBridge::install(&root).unwrap().require_policy();
    let tool = Reply::new("act", "acted");
    let runs = tool.runs();
    let (agent, _) = agent(vec![call("act", serde_json::json!({})), text("done")]);
    let mut agent = agent.with_tools(vec![Box::new(tool)]).with_rutis(&bridge);
    let (_, results) = run(&mut agent, "go").await;
    assert!(results[0].2);
    assert!(results[0].1.contains("no plugin policy"), "{results:?}");
    assert_eq!(runs.load(Ordering::SeqCst), 0);
    root.shutdown().await.unwrap();
}

/// A raw waterfall listener that returns `Allow` without calling `next`.
struct ShortCircuit;

impl WaterfallListener<ToolCallEvent> for ShortCircuit {
    fn call<'a>(
        &'a self,
        _ctx: &'a Ctx,
        e: &'a ToolCallEvent,
        _next: Next<'a, ToolCallEvent>,
    ) -> BoxFuture<'a, Result<ToolVerdict, CordisError>> {
        e.mark_judged();
        Box::pin(async { Ok(ToolVerdict::Allow) })
    }
}

struct ShortCircuitPlugin;

impl Plugin for ShortCircuitPlugin {
    fn name(&self) -> &str {
        "short-circuit"
    }
    fn apply<'a>(&'a self, ctx: &'a Ctx) -> BoxFuture<'a, Result<Effect, CordisError>> {
        Box::pin(async move {
            ctx.events()
                .on_waterfall(ctx, &EventKey::<ToolCallEvent>::of(), ShortCircuit)?;
            Ok(Effect::Done)
        })
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn an_allow_that_skips_later_policies_is_denied() {
    let root = Ctx::root().unwrap();
    let bridge = RutisBridge::install(&root).unwrap();
    let short = root.plugin(ShortCircuitPlugin);
    wait_active(&short).await;
    let later = root.plugin(deny_all());
    wait_active(&later).await;
    let tool = Reply::new("act", "acted");
    let runs = tool.runs();
    let (agent, _) = agent(vec![call("act", serde_json::json!({})), text("done")]);
    let mut agent = agent.with_tools(vec![Box::new(tool)]).with_rutis(&bridge);
    let (_, results) = run(&mut agent, "go").await;
    assert_eq!(
        runs.load(Ordering::SeqCst),
        0,
        "the skipped deny-all still wins"
    );
    assert!(results[0].2);
    assert!(
        results[0].1.contains("without passing it on"),
        "{results:?}"
    );
    root.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_policy_after_a_denier_never_sees_the_call() {
    let root = Ctx::root().unwrap();
    let bridge = RutisBridge::install(&root).unwrap();
    let denier = root.plugin(AgentPlugin::new("denier").with_policy(|call| {
        if call.tool_name() == "act" {
            ToolVerdict::deny("no")
        } else {
            ToolVerdict::Allow
        }
    }));
    wait_active(&denier).await;
    let counted = Arc::new(AtomicUsize::new(0));
    let counter = root.plugin(AgentPlugin::new("rate-counter").with_policy({
        let counted = counted.clone();
        move |_| {
            counted.fetch_add(1, Ordering::SeqCst);
            ToolVerdict::Allow
        }
    }));
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
        .with_rutis(&bridge);
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

/// Never answers.
struct HangingInput;

impl Listener<InputEvent> for HangingInput {
    fn call<'a>(
        &'a self,
        _ctx: &'a Ctx,
        _e: &'a InputEvent,
    ) -> BoxFuture<'a, Result<Option<String>, CordisError>> {
        Box::pin(std::future::pending())
    }
}

/// Adds a note, then hangs instead of finishing the chain.
struct HangingNote;

impl WaterfallListener<TurnEvent> for HangingNote {
    fn call<'a>(
        &'a self,
        _ctx: &'a Ctx,
        e: &'a TurnEvent,
        _next: Next<'a, TurnEvent>,
    ) -> BoxFuture<'a, Result<(), CordisError>> {
        Box::pin(async move {
            e.add_note("[before the hang]");
            std::future::pending().await
        })
    }
}

struct Hangs;

impl Plugin for Hangs {
    fn name(&self) -> &str {
        "hangs"
    }
    fn apply<'a>(&'a self, ctx: &'a Ctx) -> BoxFuture<'a, Result<Effect, CordisError>> {
        Box::pin(async move {
            ctx.events()
                .on(ctx, &EventKey::<InputEvent>::of(), HangingInput)?;
            ctx.events()
                .on_waterfall(ctx, &EventKey::<TurnEvent>::of(), HangingNote)?;
            Ok(Effect::Done)
        })
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_hung_input_filter_rejects_after_its_timeout() {
    let root = Ctx::root().unwrap();
    let bridge = RutisBridge::install(&root)
        .unwrap()
        .with_input_timeout(Some(Duration::from_millis(100)));
    let hangs = root.plugin(Hangs);
    wait_active(&hangs).await;
    let (agent, seen) = agent(vec![text("done")]);
    let mut agent = agent.with_rutis(&bridge);
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
async fn a_hung_turn_chain_keeps_the_notes_added_before_its_timeout() {
    let root = Ctx::root().unwrap();
    let bridge = RutisBridge::install(&root)
        .unwrap()
        .with_input_timeout(None) // the input half of `Hangs` is not under test
        .with_turn_timeout(Some(Duration::from_millis(100)));
    // Only the turn half is wanted: install the hanging note directly.
    struct NoteOnly;
    impl Plugin for NoteOnly {
        fn name(&self) -> &str {
            "note-only"
        }
        fn apply<'a>(&'a self, ctx: &'a Ctx) -> BoxFuture<'a, Result<Effect, CordisError>> {
            Box::pin(async move {
                ctx.events()
                    .on_waterfall(ctx, &EventKey::<TurnEvent>::of(), HangingNote)?;
                Ok(Effect::Done)
            })
        }
    }
    let p = root.plugin(NoteOnly);
    wait_active(&p).await;
    let (agent, seen) = agent(vec![text("done")]);
    let mut agent = agent.with_rutis(&bridge);
    tokio::time::timeout(Duration::from_secs(5), run(&mut agent, "go"))
        .await
        .expect("the turn timeout bounds the request");
    assert_eq!(seen.lock().unwrap()[0].last_user, "go|[before the hang]");
    root.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn listeners_see_the_users_request_and_the_offered_tools() {
    let root = Ctx::root().unwrap();
    let bridge = RutisBridge::install(&root).unwrap();
    type Seen = Arc<Mutex<Vec<(Option<String>, Option<String>)>>>;
    let calls: Seen = Arc::default();
    let turns: Arc<Mutex<Vec<Vec<String>>>> = Arc::default();
    let observer = root.plugin(
        AgentPlugin::new("observer")
            .with_policy({
                let calls = calls.clone();
                move |call| {
                    calls.lock().unwrap().push((
                        call.user_request().map(str::to_string),
                        call.latest_user_text().map(str::to_string),
                    ));
                    ToolVerdict::Allow
                }
            })
            .with_turn_note({
                let turns = turns.clone();
                move |turn| {
                    turns.lock().unwrap().push(turn.tool_names().to_vec());
                    None
                }
            }),
    );
    wait_active(&observer).await;
    let (agent, _) = agent(vec![call("act", serde_json::json!({})), text("done")]);
    let mut agent = agent
        .with_tools(vec![Box::new(Reply::new("act", "acted"))])
        .with_rutis(&bridge);
    run(&mut agent, "please act now").await;
    assert_eq!(
        calls.lock().unwrap().clone(),
        vec![(
            Some("please act now".to_string()),
            Some("please act now".to_string())
        )]
    );
    assert_eq!(
        turns.lock().unwrap().clone(),
        vec![vec!["act".to_string()], vec!["act".to_string()]]
    );
    root.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn notes_are_joined_in_listener_order_and_recomputed_every_turn() {
    let root = Ctx::root().unwrap();
    let bridge = RutisBridge::install(&root).unwrap();
    let n = Arc::new(AtomicUsize::new(0));
    let first = root.plugin(AgentPlugin::new("first").with_turn_note({
        let n = n.clone();
        move |_| Some(format!("[a{}]", n.fetch_add(1, Ordering::SeqCst) + 1))
    }));
    wait_active(&first).await;
    let second = root.plugin(AgentPlugin::new("second").with_turn_note(|_| Some("[b]".into())));
    wait_active(&second).await;
    let (agent, seen) = agent(vec![call("act", serde_json::json!({})), text("done")]);
    let mut agent = agent
        .with_tools(vec![Box::new(Reply::new("act", "acted"))])
        .with_rutis(&bridge);
    run(&mut agent, "go").await;
    let seen = seen.lock().unwrap().clone();
    assert_eq!(seen[0].last_user, "go|[a1]\n[b]");
    assert_eq!(
        seen[1].last_user, "go|[a2]\n[b]",
        "the second turn's notes replace the first's"
    );
    root.shutdown().await.unwrap();
}
