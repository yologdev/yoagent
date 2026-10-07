//! Agent events published on the rutis bus by the extension.

mod common;

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use common::*;
use rutis::{BoxFuture, CordisError, Ctx, EventKey, Listener};
use tokio::sync::Semaphore;
use yoagent::{AgentEvent, SubAgentTool};
use yoagent_rutis::{AgentEventEmitted, PluginCtxExt, RutisBridge};

fn kind(e: &AgentEvent) -> String {
    serde_json::to_value(e).unwrap()["type"]
        .as_str()
        .unwrap()
        .to_string()
}

/// What the agent emits for the mock script `[call act, text "done"]`,
/// pinned independently of the bridge.
const EXPECTED: &[&str] = &[
    "agentStart",
    "turnStart",
    "messageStart",
    "messageEnd",
    "messageStart",
    "messageEnd",
    "toolExecutionStart",
    "toolExecutionEnd",
    "messageStart",
    "messageEnd",
    "turnEnd",
    "turnStart",
    "messageStart",
    "messageUpdate",
    "messageEnd",
    "turnEnd",
    "agentEnd",
];

fn script() -> Vec<yoagent::provider::mock::MockResponse> {
    vec![call("act", serde_json::json!({})), text("done")]
}

/// `(run label, run id, depth, kind)` of every event observed.
type Log = Arc<Mutex<Vec<(Option<String>, String, usize, String)>>>;

fn record(log: &Log, e: &AgentEventEmitted) {
    log.lock().unwrap().push((
        e.label().map(str::to_string),
        e.run_id().to_string(),
        e.depth(),
        kind(e.event()),
    ))
}

/// A plugin recording every event it observes on the bus.
fn observer_plugin(log: Log) -> Setup {
    Setup::new("observer", move |ctx| {
        let log = log.clone();
        ctx.on_agent_event(move |e| record(&log, e)).map(drop)
    })
}

fn kinds(log: &Log, label: Option<&str>) -> Vec<String> {
    log.lock()
        .unwrap()
        .iter()
        .filter(|(l, ..)| l.as_deref() == label)
        .map(|(.., k)| k.clone())
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

fn attached(bridge: &RutisBridge, label: Option<&str>) -> yoagent::Agent {
    let (agent, _) = agent(script());
    let agent = agent
        .with_tools(vec![Box::new(Reply::new("act", "acted"))])
        .with_extension(bridge.extension());
    match label {
        Some(label) => agent.with_run_label(label),
        None => agent,
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_bus_listener_sees_exactly_the_runs_events_in_order() {
    let (root, bridge) = setup();
    let log = Log::default();
    let observer = root.plugin(observer_plugin(log.clone()));
    wait_active(&observer).await;
    let mut agent = attached(&bridge, None);

    let (events, _) = run(&mut agent, "go").await;
    let own: Vec<String> = events.iter().map(kind).collect();
    assert_eq!(own, EXPECTED, "the caller's own consumer gets every event");

    until_count(&log, EXPECTED.len()).await;
    tokio::time::sleep(Duration::from_millis(50)).await; // nothing extra follows
    assert_eq!(kinds(&log, None), EXPECTED);
    let ids: std::collections::HashSet<String> = log
        .lock()
        .unwrap()
        .iter()
        .map(|(_, id, ..)| id.clone())
        .collect();
    assert_eq!(ids.len(), 1, "one run id for the whole run");
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
            record(&self.log, e);
            Ok(None)
        })
    }
}

fn gated_observer(gate: Arc<Semaphore>, log: Log) -> Setup {
    Setup::new("gated-observer", move |ctx| {
        ctx.events()
            .on(
                ctx,
                &EventKey::<AgentEventEmitted>::of(),
                Gated {
                    gate: gate.clone(),
                    log: log.clone(),
                },
            )
            .map(drop)
    })
}

#[tokio::test(flavor = "multi_thread")]
async fn a_blocked_listener_does_not_hold_the_agent_back() {
    let (root, bridge) = setup();
    let gate = Arc::new(Semaphore::new(0));
    let log = Log::default();
    let slow = root.plugin(gated_observer(gate.clone(), log.clone()));
    wait_active(&slow).await;
    let mut agent = attached(&bridge, None);

    tokio::time::timeout(Duration::from_secs(5), run(&mut agent, "go"))
        .await
        .expect("the run completes while the listener is blocked");
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
async fn two_agents_on_one_bridge_are_told_apart_by_label() {
    let (root, bridge) = setup();
    let log = Log::default();
    let observer = root.plugin(observer_plugin(log.clone()));
    wait_active(&observer).await;
    let mut a = attached(&bridge, Some("agent-a"));
    let mut b = attached(&bridge, Some("agent-b"));

    let started = Instant::now();
    tokio::join!(run(&mut a, "go"), run(&mut b, "go"));
    assert!(started.elapsed() < Duration::from_secs(5));

    until_count(&log, 2 * EXPECTED.len()).await;
    assert_eq!(kinds(&log, Some("agent-a")), EXPECTED);
    assert_eq!(kinds(&log, Some("agent-b")), EXPECTED);
    assert!(kinds(&log, None).is_empty());
    root.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_sub_agents_events_carry_its_depth_with_a_tree_extension() {
    let (root, bridge) = setup();
    let log = Log::default();
    let observer = root.plugin(observer_plugin(log.clone()));
    wait_active(&observer).await;
    let (child, _) = recording(vec![text("child done")]);
    let sub = SubAgentTool::from_provider("helper", child, yoagent::provider::ModelConfig::mock());
    let (parent, _) = agent(vec![
        call("helper", serde_json::json!({"task": "help"})),
        text("done"),
    ]);
    let mut parent = parent
        .with_sub_agent(sub)
        .with_run_label("tree")
        .with_tree_extension(bridge.extension());
    run(&mut parent, "delegate").await;
    tokio::time::timeout(Duration::from_secs(5), async {
        while !log
            .lock()
            .unwrap()
            .iter()
            .any(|(_, _, depth, k)| *depth == 0 && k == "agentEnd")
        {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("the parent's run ends on the bus");
    let log = log.lock().unwrap().clone();
    let child: Vec<_> = log.iter().filter(|(_, _, depth, _)| *depth == 1).collect();
    assert!(
        !child.is_empty(),
        "the child's events are published: {log:?}"
    );
    assert!(
        child
            .iter()
            .all(|(label, ..)| label.as_deref() == Some("tree")),
        "a delegated run keeps its parent's label"
    );
    let parent_ids: std::collections::HashSet<_> = log
        .iter()
        .filter(|(_, _, d, _)| *d == 0)
        .map(|(_, id, ..)| id)
        .collect();
    assert!(
        child.iter().all(|(_, id, ..)| !parent_ids.contains(id)),
        "the child's run has its own id"
    );
    root.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn nothing_is_published_after_the_host_stops() {
    let (root, bridge) = setup();
    let log = Log::default();
    let observer = root.plugin(observer_plugin(log.clone()));
    wait_active(&observer).await;
    // Positive control: while the host runs, a run's events reach the
    // listener within the window the negative check waits below, so an
    // empty log after the shutdown is not just slow delivery.
    const WINDOW: Duration = Duration::from_millis(500);
    let mut agent = attached(&bridge, None);
    run(&mut agent, "go").await;
    tokio::time::sleep(WINDOW).await;
    assert_eq!(
        kinds(&log, None),
        EXPECTED,
        "while running, the whole run is published within the window"
    );
    log.lock().unwrap().clear();

    let mut agent = attached(&bridge, None);
    root.shutdown().await.unwrap();
    run(&mut agent, "go").await;
    tokio::time::sleep(WINDOW).await;
    assert!(log.lock().unwrap().is_empty(), "{:?}", log.lock().unwrap());
}
