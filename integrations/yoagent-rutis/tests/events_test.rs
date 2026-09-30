//! Agent events forwarded onto the rutis bus.

mod common;

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use common::*;
use rutis::{BoxFuture, CordisError, Ctx, Effect, EventKey, Listener, Plugin};
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

type Log = Arc<Mutex<Vec<&'static str>>>;

struct Observer(Log);

impl Plugin for Observer {
    fn name(&self) -> &str {
        "observer"
    }
    fn apply<'a>(&'a self, ctx: &'a Ctx) -> BoxFuture<'a, Result<Effect, CordisError>> {
        let log = self.0.clone();
        Box::pin(async move {
            ctx.on_agent_event(move |e| log.lock().unwrap().push(kind(e)))?;
            Ok(Effect::Done)
        })
    }
}

async fn until(log: &Log, what: &str) {
    tokio::time::timeout(Duration::from_secs(5), async {
        while !log.lock().unwrap().contains(&what) {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("{what} never arrived: {:?}", log.lock().unwrap()));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_bus_listener_sees_the_agents_events_in_order() {
    let root = Ctx::root().unwrap();
    let bridge = RutisBridge::install(&root).unwrap();
    let log = Log::default();
    let observer = root.plugin(Observer(log.clone()));
    wait_active(&observer).await;

    let (agent, _) = agent(vec![call("act", serde_json::json!({})), text("done")]);
    let mut agent = agent
        .with_tools(vec![Box::new(Reply::new("act", "acted"))])
        .with_rutis(&bridge);

    // Tee: the caller's own consumer receives every event too.
    let (ui_tx, mut ui_rx) = tokio::sync::mpsc::unbounded_channel();
    let (tx, forwarder) = bridge.event_sender(Some(ui_tx));
    agent.prompt_with_sender("go", tx).await;
    forwarder.await.unwrap();
    let mut ui = Vec::new();
    while let Ok(e) = ui_rx.try_recv() {
        ui.push(kind(&e));
    }

    until(&log, "AgentEnd").await;
    let seen = log.lock().unwrap().clone();
    assert_eq!(
        seen, ui,
        "the bus sees exactly what the caller saw, in order"
    );
    assert_eq!(seen.first(), Some(&"AgentStart"));
    assert_eq!(seen.last(), Some(&"AgentEnd"));
    let tool_end = seen.iter().position(|k| *k == "ToolExecutionEnd").unwrap();
    let tool_start = seen
        .iter()
        .position(|k| *k == "ToolExecutionStart")
        .unwrap();
    assert!(tool_start < tool_end);
    root.shutdown().await.unwrap();
}

/// Sleeps on every event.
struct Slow(Log);

impl Listener<AgentEventEmitted> for Slow {
    fn call<'a>(
        &'a self,
        _ctx: &'a Ctx,
        e: &'a AgentEventEmitted,
    ) -> BoxFuture<'a, Result<Option<()>, CordisError>> {
        Box::pin(async move {
            tokio::time::sleep(Duration::from_millis(100)).await;
            self.0.lock().unwrap().push(kind(e.event()));
            Ok(None)
        })
    }
}

struct SlowObserver(Log);

impl Plugin for SlowObserver {
    fn name(&self) -> &str {
        "slow-observer"
    }
    fn apply<'a>(&'a self, ctx: &'a Ctx) -> BoxFuture<'a, Result<Effect, CordisError>> {
        Box::pin(async move {
            ctx.events().on(
                ctx,
                &EventKey::<AgentEventEmitted>::of(),
                Slow(self.0.clone()),
            )?;
            Ok(Effect::Done)
        })
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_slow_listener_does_not_slow_the_agent() {
    let root = Ctx::root().unwrap();
    let bridge = RutisBridge::install(&root).unwrap();
    let log = Log::default();
    let slow = root.plugin(SlowObserver(log.clone()));
    wait_active(&slow).await;

    let (agent, _) = agent(vec![call("act", serde_json::json!({})), text("done")]);
    let mut agent = agent
        .with_tools(vec![Box::new(Reply::new("act", "acted"))])
        .with_rutis(&bridge);

    let (tx, forwarder) = bridge.event_sender(None);
    let started = Instant::now();
    agent.prompt_with_sender("go", tx).await;
    forwarder.await.unwrap();
    let elapsed = started.elapsed();
    // A dozen-plus events at 100 ms each would take well over a second if
    // the agent waited for the listener.
    assert!(
        elapsed < Duration::from_millis(500),
        "the run took {elapsed:?}; it waited for the listener"
    );
    assert!(
        log.lock().unwrap().len() < 3,
        "the listener is still behind"
    );

    // It still gets everything, in order, eventually.
    tokio::time::timeout(Duration::from_secs(10), async {
        while !log.lock().unwrap().contains(&"AgentEnd") {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the slow listener catches up");
    let seen = log.lock().unwrap().clone();
    assert_eq!(seen.first(), Some(&"AgentStart"));
    assert_eq!(seen.last(), Some(&"AgentEnd"));
    root.shutdown().await.unwrap();
}
