//! Agent events forwarded onto the rutis bus.

mod common;

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use common::*;
use rutis::{BoxFuture, CordisError, Ctx, Effect, EventKey, Listener, Plugin};
use tokio::sync::{mpsc, Semaphore};
use yoagent::AgentEvent;
use yoagent_rutis::{AgentEventEmitted, AgentRutisExt, PluginCtxExt, RutisBridge};

fn kind(e: &AgentEvent) -> &'static str {
    match e {
        AgentEvent::AgentStart => "AgentStart",
        AgentEvent::AgentEnd { .. } => "AgentEnd",
        AgentEvent::TurnStart => "TurnStart",
        AgentEvent::TurnEnd { .. } => "TurnEnd",
        AgentEvent::MessageStart { .. } => "MessageStart",
        AgentEvent::MessageUpdate { .. } => "MessageUpdate",
        AgentEvent::MessageEnd { .. } => "MessageEnd",
        AgentEvent::ToolExecutionStart { .. } => "ToolExecutionStart",
        AgentEvent::ToolExecutionUpdate { .. } => "ToolExecutionUpdate",
        AgentEvent::ToolExecutionEnd { .. } => "ToolExecutionEnd",
        _ => "other",
    }
}

/// What the agent emits for the mock script `[call act, text "done"]`,
/// pinned independently of the bridge.
const EXPECTED: &[&str] = &[
    "AgentStart",
    "TurnStart",
    "MessageStart",
    "MessageEnd",
    "MessageStart",
    "MessageEnd",
    "ToolExecutionStart",
    "ToolExecutionEnd",
    "MessageStart",
    "MessageEnd",
    "TurnEnd",
    "TurnStart",
    "MessageStart",
    "MessageUpdate",
    "MessageEnd",
    "TurnEnd",
    "AgentEnd",
];

fn script() -> Vec<yoagent::provider::mock::MockResponse> {
    vec![call("act", serde_json::json!({})), text("done")]
}

type Log = Arc<Mutex<Vec<(Option<String>, &'static str)>>>;

struct Observer(Log);

impl Plugin for Observer {
    fn name(&self) -> &str {
        "observer"
    }
    fn apply<'a>(&'a self, ctx: &'a Ctx) -> BoxFuture<'a, Result<Effect, CordisError>> {
        let log = self.0.clone();
        Box::pin(async move {
            ctx.on_agent_event(move |e| {
                log.lock()
                    .unwrap()
                    .push((e.label().map(str::to_string), kind(e.event())))
            })?;
            Ok(Effect::Done)
        })
    }
}

fn kinds(log: &Log, label: Option<&str>) -> Vec<&'static str> {
    log.lock()
        .unwrap()
        .iter()
        .filter(|(l, _)| l.as_deref() == label)
        .map(|(_, k)| *k)
        .collect()
}

async fn until_count(log: &Log, n: usize) {
    tokio::time::timeout(Duration::from_secs(5), async {
        while log.lock().unwrap().len() < n {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("only {:?} arrived", log.lock().unwrap()));
}

async fn attached(bridge: &RutisBridge) -> yoagent::Agent {
    let (agent, _) = agent(script());
    agent
        .with_tools(vec![Box::new(Reply::new("act", "acted"))])
        .with_rutis(bridge)
}

#[tokio::test(flavor = "multi_thread")]
async fn a_bus_listener_sees_exactly_the_agents_events_in_order() {
    let root = Ctx::root().unwrap();
    let bridge = RutisBridge::install(&root).unwrap();
    let log = Log::default();
    let observer = root.plugin(Observer(log.clone()));
    wait_active(&observer).await;
    let mut agent = attached(&bridge).await;

    let (ui_tx, mut ui_rx) = mpsc::unbounded_channel();
    let (tx, forwarder) = bridge.event_sender(Some(ui_tx));
    agent.prompt_with_sender("go", tx).await;
    forwarder.await.unwrap();
    let mut ui = Vec::new();
    while let Ok(e) = ui_rx.try_recv() {
        ui.push(kind(&e));
    }
    assert_eq!(ui, EXPECTED, "the caller's own consumer gets every event");

    until_count(&log, EXPECTED.len()).await;
    tokio::time::sleep(Duration::from_millis(50)).await; // nothing extra follows
    assert_eq!(kinds(&log, None), EXPECTED);
    root.shutdown().await.unwrap();
}

/// Waits on a gate before recording each event.
struct Gated {
    gate: Arc<Semaphore>,
    log: Log,
}

impl Listener<AgentEventEmitted> for Gated {
    fn call<'a>(
        &'a self,
        _ctx: &'a Ctx,
        e: &'a AgentEventEmitted,
    ) -> BoxFuture<'a, Result<Option<()>, CordisError>> {
        Box::pin(async move {
            self.gate.acquire().await.unwrap().forget();
            self.log.lock().unwrap().push((None, kind(e.event())));
            Ok(None)
        })
    }
}

struct GatedObserver(Arc<Semaphore>, Log);

impl Plugin for GatedObserver {
    fn name(&self) -> &str {
        "gated-observer"
    }
    fn apply<'a>(&'a self, ctx: &'a Ctx) -> BoxFuture<'a, Result<Effect, CordisError>> {
        Box::pin(async move {
            ctx.events().on(
                ctx,
                &EventKey::<AgentEventEmitted>::of(),
                Gated {
                    gate: self.0.clone(),
                    log: self.1.clone(),
                },
            )?;
            Ok(Effect::Done)
        })
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_blocked_listener_does_not_hold_the_agent_back() {
    let root = Ctx::root().unwrap();
    let bridge = RutisBridge::install(&root).unwrap();
    let gate = Arc::new(Semaphore::new(0));
    let log = Log::default();
    let slow = root.plugin(GatedObserver(gate.clone(), log.clone()));
    wait_active(&slow).await;
    let mut agent = attached(&bridge).await;

    let (tx, forwarder) = bridge.event_sender(None);
    tokio::time::timeout(Duration::from_secs(5), agent.prompt_with_sender("go", tx))
        .await
        .expect("the run completes while the listener is blocked");
    forwarder.await.unwrap();
    assert!(
        log.lock().unwrap().is_empty(),
        "the listener has not handled a single event, yet the run is done"
    );

    gate.add_permits(EXPECTED.len());
    until_count(&log, EXPECTED.len()).await;
    assert_eq!(
        kinds(&log, None),
        EXPECTED,
        "then it gets everything, in order"
    );
    root.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_closed_forward_receiver_does_not_stop_publishing() {
    let root = Ctx::root().unwrap();
    let bridge = RutisBridge::install(&root).unwrap();
    let log = Log::default();
    let observer = root.plugin(Observer(log.clone()));
    wait_active(&observer).await;
    let mut agent = attached(&bridge).await;

    let (ui_tx, ui_rx) = mpsc::unbounded_channel();
    drop(ui_rx);
    let (tx, forwarder) = bridge.event_sender(Some(ui_tx));
    agent.prompt_with_sender("go", tx).await;
    forwarder.await.unwrap();
    until_count(&log, EXPECTED.len()).await;
    assert_eq!(kinds(&log, None), EXPECTED);
    root.shutdown().await.unwrap();
}

/// Records, for each event it receives from the bus, whether the caller's
/// forward channel already held that event.
struct ForwardFirst {
    forwarded: Arc<Mutex<mpsc::UnboundedReceiver<AgentEvent>>>,
    ahead: Arc<Mutex<Vec<bool>>>,
}

impl Listener<AgentEventEmitted> for ForwardFirst {
    fn call<'a>(
        &'a self,
        _ctx: &'a Ctx,
        e: &'a AgentEventEmitted,
    ) -> BoxFuture<'a, Result<Option<()>, CordisError>> {
        Box::pin(async move {
            let got = self.forwarded.lock().unwrap().try_recv();
            let ok = matches!(&got, Ok(f) if kind(f) == kind(e.event()));
            self.ahead.lock().unwrap().push(ok);
            Ok(None)
        })
    }
}

struct ForwardFirstPlugin(
    Arc<Mutex<mpsc::UnboundedReceiver<AgentEvent>>>,
    Arc<Mutex<Vec<bool>>>,
);

impl Plugin for ForwardFirstPlugin {
    fn name(&self) -> &str {
        "forward-first"
    }
    fn apply<'a>(&'a self, ctx: &'a Ctx) -> BoxFuture<'a, Result<Effect, CordisError>> {
        Box::pin(async move {
            ctx.events().on(
                ctx,
                &EventKey::<AgentEventEmitted>::of(),
                ForwardFirst {
                    forwarded: self.0.clone(),
                    ahead: self.1.clone(),
                },
            )?;
            Ok(Effect::Done)
        })
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn the_callers_consumer_is_never_behind_the_bus() {
    let root = Ctx::root().unwrap();
    let bridge = RutisBridge::install(&root).unwrap();
    let (ui_tx, ui_rx) = mpsc::unbounded_channel();
    let forwarded = Arc::new(Mutex::new(ui_rx));
    let ahead = Arc::new(Mutex::new(Vec::new()));
    let p = root.plugin(ForwardFirstPlugin(forwarded, ahead.clone()));
    wait_active(&p).await;
    let mut agent = attached(&bridge).await;
    let (tx, forwarder) = bridge.event_sender(Some(ui_tx));
    agent.prompt_with_sender("go", tx).await;
    forwarder.await.unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        while ahead.lock().unwrap().len() < EXPECTED.len() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    assert!(
        ahead.lock().unwrap().iter().all(|ok| *ok),
        "every event was forwarded before the bus saw it: {:?}",
        ahead.lock().unwrap()
    );
    root.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn two_agents_on_one_bridge_are_told_apart_by_label() {
    let root = Ctx::root().unwrap();
    let bridge = RutisBridge::install(&root).unwrap();
    let log = Log::default();
    let observer = root.plugin(Observer(log.clone()));
    wait_active(&observer).await;
    let mut a = attached(&bridge).await;
    let mut b = attached(&bridge).await;

    let (tx_a, fwd_a) = bridge.event_sender_labeled("agent-a", None);
    let (tx_b, fwd_b) = bridge.event_sender_labeled("agent-b", None);
    let started = Instant::now();
    tokio::join!(
        a.prompt_with_sender("go", tx_a),
        b.prompt_with_sender("go", tx_b)
    );
    fwd_a.await.unwrap();
    fwd_b.await.unwrap();
    assert!(started.elapsed() < Duration::from_secs(5));

    until_count(&log, 2 * EXPECTED.len()).await;
    assert_eq!(kinds(&log, Some("agent-a")), EXPECTED);
    assert_eq!(kinds(&log, Some("agent-b")), EXPECTED);
    assert!(kinds(&log, None).is_empty());
    root.shutdown().await.unwrap();
}
