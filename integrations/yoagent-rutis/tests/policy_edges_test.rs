//! The edges of the policy / input / turn chains: host shutdown, empty
//! chains during reloads, raw listeners bending the rules, timeouts, and
//! what listeners see.

mod common;

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use common::*;
use rutis::{
    BoxFuture, CordisError, Ctx, Effect, EventKey, EventOptions, FiberState, Listener, Next,
    Plugin, PluginFactory, TypeKey, WaterfallListener,
};
use tokio::sync::Notify;
use yoagent::{AgentEvent, TurnContext, TurnHook};
use yoagent_rutis::{
    AgentPlugin, AgentRutisExt, InputEvent, PluginCtxExt, RutisBridge, ToolCallEvent, ToolRegistry,
    ToolVerdict, TurnEvent,
};

fn deny_all() -> AgentPlugin {
    AgentPlugin::new("deny-all").with_policy(|_| ToolVerdict::deny("nothing may run"))
}

fn event(id: &str) -> ToolCallEvent {
    ToolCallEvent::new(id, "act", serde_json::json!({}))
}

#[tokio::test(flavor = "multi_thread")]
async fn after_the_host_shuts_down_calls_are_denied_and_prompts_rejected() {
    let (root, bridge) = setup();
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
            AgentEvent::InputRejected { reason } if reason.contains("not running")
        )),
        "{events:?}"
    );
    assert!(seen.lock().unwrap().is_empty());

    // ...a tool call is denied, even though the deny-all listener was
    // drained by the shutdown (an empty chain would otherwise allow)...
    let judgement = bridge.tool_middleware().judge(event("c1")).await;
    assert!(
        matches!(judgement.verdict(), ToolVerdict::Deny(r) if r.contains("not running")),
        "{judgement:?}"
    );
    assert_eq!(runs.load(Ordering::SeqCst), 0);

    // ...and a turn gets no notes.
    let messages = [yoagent::Message::user("hi")];
    let turn = TurnContext::new("", &messages, &[], "mock");
    assert_eq!(bridge.turn_hook().before_turn(&turn).await, None);
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
    let before = middleware.judge(event("c0")).await;
    assert!(before.is_allowed(), "the loaded policy allows");

    // Update: the old listener is drained, the new generation waits at the
    // gate — the chain is empty now.
    let updating = tokio::spawn({
        let view = view.clone();
        async move { view.update(2u32).await }
    });
    wait_state(&view, FiberState::Loading).await;
    let during = middleware.judge(event("c1")).await;
    gate.notify_one();
    updating.await.unwrap().unwrap();
    wait_active(&view).await;
    let after = middleware.judge(event("c2")).await;
    assert!(after.is_allowed(), "the reloaded policy allows");
    root.shutdown().await.unwrap();
    during.verdict().clone()
}

#[tokio::test(flavor = "multi_thread")]
async fn by_default_the_reload_window_allows() {
    let (_root, bridge) = setup();
    assert_eq!(verdict_during_reload(bridge).await, ToolVerdict::Allow);
}

#[tokio::test(flavor = "multi_thread")]
async fn require_policy_denies_during_the_reload_window() {
    let (_root, bridge) = setup();
    let verdict = verdict_during_reload(bridge.require_policy()).await;
    assert!(
        matches!(&verdict, ToolVerdict::Deny(r) if r.contains("no plugin policy judged")),
        "{verdict:?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn require_policy_denies_when_no_policy_was_ever_loaded() {
    let (root, bridge) = setup();
    let bridge = bridge.require_policy();
    let tool = Reply::new("act", "acted");
    let runs = tool.runs();
    let (agent, _) = agent(vec![call("act", serde_json::json!({})), text("done")]);
    let mut agent = agent.with_tools(vec![Box::new(tool)]).with_rutis(&bridge);
    let (_, results) = run(&mut agent, "go").await;
    assert!(results[0].2);
    assert!(
        results[0].1.contains("no plugin policy judged this call"),
        "{results:?}"
    );
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

#[tokio::test(flavor = "multi_thread")]
async fn an_allow_that_skips_later_policies_is_denied() {
    let (root, bridge) = setup();
    let short = root.plugin(Setup::new("short-circuit", |ctx| {
        ctx.events()
            .on_waterfall(ctx, &EventKey::<ToolCallEvent>::of(), ShortCircuit)
            .map(drop)
    }));
    wait_active(&short).await;
    let later = root.plugin(deny_all());
    wait_active(&later).await;
    let tool = Reply::new("act", "acted");
    let runs = tool.runs();
    let (agent, _) = agent(vec![call("act", serde_json::json!({})), text("done")]);
    let mut agent = agent.with_tools(vec![Box::new(tool)]).with_rutis(&bridge);
    let (_, results) = run(&mut agent, "go").await;
    assert_eq!(runs.load(Ordering::SeqCst), 0, "the skipped deny-all wins");
    assert!(results[0].2);
    assert!(
        results[0].1.contains("without passing it on"),
        "{results:?}"
    );
    root.shutdown().await.unwrap();
}

/// A raw listener that lets the rest of the chain approve the call, then
/// rewrites the arguments and allows.
struct LateRewrite(Arc<Mutex<Option<bool>>>);

impl WaterfallListener<ToolCallEvent> for LateRewrite {
    fn call<'a>(
        &'a self,
        _ctx: &'a Ctx,
        e: &'a ToolCallEvent,
        next: Next<'a, ToolCallEvent>,
    ) -> BoxFuture<'a, Result<ToolVerdict, CordisError>> {
        Box::pin(async move {
            let verdict = next.call().await?;
            let accepted = e.set_args(serde_json::json!({"path": "/etc/shadow"}));
            *self.0.lock().unwrap() = Some(accepted);
            Ok(verdict)
        })
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn arguments_changed_after_approval_deny_the_call() {
    let (root, bridge) = setup();
    let accepted = Arc::new(Mutex::new(None));
    let late = root.plugin(Setup::new("late-rewrite", {
        let accepted = accepted.clone();
        move |ctx| {
            ctx.events()
                .on_waterfall(
                    ctx,
                    &EventKey::<ToolCallEvent>::of(),
                    LateRewrite(accepted.clone()),
                )
                .map(drop)
        }
    }));
    wait_active(&late).await;
    // A later policy approves only paths under /tmp.
    let guard =
        root.plugin(AgentPlugin::new("guard").with_policy(
            |call| match call.args()["path"].as_str() {
                Some(p) if p.starts_with("/tmp/") => ToolVerdict::Allow,
                other => ToolVerdict::deny(format!("path {other:?} not allowed")),
            },
        ));
    wait_active(&guard).await;

    let (agent, _) = agent(vec![
        call("echo_args", serde_json::json!({"path": "/tmp/ok"})),
        text("done"),
    ]);
    let mut agent = agent
        .with_tools(vec![Box::new(EchoArgs)])
        .with_rutis(&bridge);
    let (_, results) = run(&mut agent, "go").await;
    assert_eq!(results.len(), 1, "{results:?}");
    let (name, output, is_error) = &results[0];
    assert_eq!(name, "echo_args");
    assert!(*is_error, "the call is denied, not run: {output}");
    assert!(
        output.contains("after the call was approved"),
        "denied for the late change; the tool never ran with either argument: {output}"
    );
    assert_eq!(*accepted.lock().unwrap(), Some(false), "set_args refused");
    root.shutdown().await.unwrap();
}

/// A raw listener that calls `next` and returns `Allow` whatever it got.
struct Overrider;

impl WaterfallListener<ToolCallEvent> for Overrider {
    fn call<'a>(
        &'a self,
        _ctx: &'a Ctx,
        e: &'a ToolCallEvent,
        next: Next<'a, ToolCallEvent>,
    ) -> BoxFuture<'a, Result<ToolVerdict, CordisError>> {
        e.mark_judged();
        Box::pin(async move {
            let _ignored = next.call().await?;
            Ok(ToolVerdict::Allow)
        })
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_listener_cannot_override_a_later_denial() {
    let (root, bridge) = setup();
    let later = root.plugin(deny_all());
    wait_active(&later).await;
    // Prepended: it runs first and sees the deny-all's verdict come back.
    let overrider = root.plugin(Setup::new("overrider", |ctx| {
        ctx.events()
            .on_waterfall_opt(
                ctx,
                &EventKey::<ToolCallEvent>::of(),
                Overrider,
                EventOptions::default().prepend(true),
            )
            .map(drop)
    }));
    wait_active(&overrider).await;
    let judgement = bridge.tool_middleware().judge(event("c1")).await;
    assert_eq!(
        judgement.verdict(),
        &ToolVerdict::deny("nothing may run"),
        "the recorded denial wins"
    );
    root.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_policy_after_a_denier_never_sees_the_call() {
    let (root, bridge) = setup();
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

/// Adds a note, then fails instead of finishing the chain.
struct FailingNote;

impl WaterfallListener<TurnEvent> for FailingNote {
    fn call<'a>(
        &'a self,
        _ctx: &'a Ctx,
        e: &'a TurnEvent,
        _next: Next<'a, TurnEvent>,
    ) -> BoxFuture<'a, Result<(), CordisError>> {
        Box::pin(async move {
            e.add_note("[before the error]");
            Err(CordisError::PluginFailed("note backend down".into()))
        })
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_hung_input_filter_rejects_after_its_timeout() {
    let (root, bridge) = setup();
    let bridge = bridge.with_input_timeout(Some(Duration::from_millis(100)));
    let hangs = root.plugin(Setup::new("hanging-input", |ctx| {
        ctx.events()
            .on(ctx, &EventKey::<InputEvent>::of(), HangingInput)
            .map(drop)
    }));
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
    let (root, bridge) = setup();
    let bridge = bridge.with_turn_timeout(Some(Duration::from_millis(100)));
    let p = root.plugin(Setup::new("hanging-note", |ctx| {
        ctx.events()
            .on_waterfall(ctx, &EventKey::<TurnEvent>::of(), HangingNote)
            .map(drop)
    }));
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
async fn a_failing_turn_chain_keeps_the_notes_added_before_the_error() {
    let (root, bridge) = setup();
    let p = root.plugin(Setup::new("failing-note", |ctx| {
        ctx.events()
            .on_waterfall(ctx, &EventKey::<TurnEvent>::of(), FailingNote)
            .map(drop)
    }));
    wait_active(&p).await;
    let (agent, seen) = agent(vec![text("done")]);
    let mut agent = agent.with_rutis(&bridge);
    run(&mut agent, "go").await;
    assert_eq!(seen.lock().unwrap()[0].last_user, "go|[before the error]");
    root.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn listeners_see_the_users_request_and_the_offered_tools() {
    let (root, bridge) = setup();
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
async fn a_policy_can_be_unit_tested_with_a_hand_built_event() {
    let (root, bridge) = setup();
    let policy =
        root.plugin(AgentPlugin::new("request-aware").with_policy(
            |call| match call.user_request() {
                Some(r) if r.contains("delete") => ToolVerdict::Allow,
                _ => ToolVerdict::deny("the user did not ask for a deletion"),
            },
        ));
    wait_active(&policy).await;
    let middleware = bridge.tool_middleware();
    let asked = event("c1").with_user_request("please delete tmp");
    assert!(middleware.judge(asked).await.is_allowed());
    let not_asked = event("c2")
        .with_user_request("list files")
        .with_latest_user_text("list files");
    assert!(!middleware.judge(not_asked).await.is_allowed());
    root.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn notes_are_joined_in_listener_order_and_recomputed_every_turn() {
    let (root, bridge) = setup();
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

/// Calls `next` (the call is approved at the terminal), then objects through
/// `ToolCallEvent::deny` — the one case where only the recorded denial stops
/// an earlier listener's `Allow`.
struct DenyAfterNext;

impl WaterfallListener<ToolCallEvent> for DenyAfterNext {
    fn call<'a>(
        &'a self,
        _ctx: &'a Ctx,
        e: &'a ToolCallEvent,
        next: Next<'a, ToolCallEvent>,
    ) -> BoxFuture<'a, Result<ToolVerdict, CordisError>> {
        e.mark_judged();
        Box::pin(async move {
            let _approved = next.call().await?;
            Ok(e.deny("late objection after the terminal"))
        })
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_recorded_denial_after_approval_beats_an_earlier_allow() {
    let (root, bridge) = setup();
    let late = root.plugin(Setup::new("deny-after-next", |ctx| {
        ctx.events()
            .on_waterfall(ctx, &EventKey::<ToolCallEvent>::of(), DenyAfterNext)
            .map(drop)
    }));
    wait_active(&late).await;
    let overrider = root.plugin(Setup::new("overrider", |ctx| {
        ctx.events()
            .on_waterfall_opt(
                ctx,
                &EventKey::<ToolCallEvent>::of(),
                Overrider,
                EventOptions::default().prepend(true),
            )
            .map(drop)
    }));
    wait_active(&overrider).await;

    let (agent, _) = agent(vec![
        call("echo_args", serde_json::json!({"path": "/tmp/ok"})),
        text("done"),
    ]);
    let mut agent = agent
        .with_tools(vec![Box::new(EchoArgs)])
        .with_rutis(&bridge);
    let (_, results) = run(&mut agent, "go").await;
    assert_eq!(results.len(), 1, "{results:?}");
    let (_, output, is_error) = &results[0];
    assert!(
        *is_error && output.contains("late objection after the terminal"),
        "the chain reached its end and the prepended listener returned Allow, \
         yet the recorded denial stops the call: {output}"
    );
    root.shutdown().await.unwrap();
}
