//! `ToolSource`: tools resolved at the start of every run.
//!
//! What must hold: a source is consulted once per run (not per turn); the
//! tools it returns are offered for that run only; the agent's own tools win
//! a name collision, then earlier sources; a panicking source contributes
//! nothing and the run proceeds.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use tokio::sync::mpsc;
use yoagent::provider::mock::*;
use yoagent::provider::{
    MockProvider, ModelConfig, ProviderError, StreamConfig, StreamEvent, StreamProvider,
};
use yoagent::*;

/// Tool names offered on each request, in request order.
type SeenTools = Arc<Mutex<Vec<Vec<String>>>>;

struct Recording {
    inner: MockProvider,
    seen: SeenTools,
}

#[async_trait::async_trait]
impl StreamProvider for Recording {
    async fn stream(
        &self,
        config: StreamConfig,
        tx: mpsc::UnboundedSender<StreamEvent>,
        cancel: tokio_util::sync::CancellationToken,
    ) -> Result<Message, ProviderError> {
        self.seen
            .lock()
            .unwrap()
            .push(config.tools.iter().map(|t| t.name.clone()).collect());
        self.inner.stream(config, tx, cancel).await
    }
}

fn recording(responses: Vec<MockResponse>) -> (Recording, SeenTools) {
    let seen = SeenTools::default();
    (
        Recording {
            inner: MockProvider::new(responses),
            seen: seen.clone(),
        },
        seen,
    )
}

/// Replies with a fixed text; the name and reply are chosen per instance.
struct Named {
    name: &'static str,
    reply: &'static str,
}

#[async_trait::async_trait]
impl AgentTool for Named {
    fn name(&self) -> &str {
        self.name
    }
    fn label(&self) -> &str {
        self.name
    }
    fn description(&self) -> &str {
        "test tool"
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({"type": "object", "properties": {}})
    }
    async fn execute(
        &self,
        _params: serde_json::Value,
        _ctx: ToolContext,
    ) -> Result<ToolResult, ToolError> {
        Ok(ToolResult {
            content: vec![Content::Text {
                text: self.reply.into(),
            }],
            details: serde_json::Value::Null,
        })
    }
}

fn named(name: &'static str, reply: &'static str) -> Arc<dyn AgentTool> {
    Arc::new(Named { name, reply })
}

/// A tool list the test swaps between runs; counts how often it is asked.
#[derive(Clone, Default)]
struct Swappable {
    tools: Arc<Mutex<Vec<Arc<dyn AgentTool>>>>,
    calls: Arc<AtomicUsize>,
}

impl Swappable {
    fn set(&self, tools: Vec<Arc<dyn AgentTool>>) {
        *self.tools.lock().unwrap() = tools;
    }
    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

#[async_trait::async_trait]
impl ToolSource for Swappable {
    async fn tools(&self) -> Vec<Arc<dyn AgentTool>> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.tools.lock().unwrap().clone()
    }
}

struct Panicking;

#[async_trait::async_trait]
impl ToolSource for Panicking {
    async fn tools(&self) -> Vec<Arc<dyn AgentTool>> {
        panic!("source exploded")
    }
}

fn call(name: &str) -> MockResponse {
    MockResponse::ToolCalls(vec![MockToolCall {
        provider_metadata: None,
        name: name.into(),
        arguments: serde_json::json!({}),
    }])
}

/// Every tool result of the run as `(tool_name, text, is_error)`.
fn tool_results(messages: &[AgentMessage]) -> Vec<(String, String, bool)> {
    messages
        .iter()
        .filter_map(|m| match m {
            AgentMessage::Llm(Message::ToolResult {
                tool_name,
                content,
                is_error,
                ..
            }) => Some((
                tool_name.clone(),
                content
                    .iter()
                    .filter_map(|c| match c {
                        Content::Text { text } => Some(text.as_str()),
                        _ => None,
                    })
                    .collect::<String>(),
                *is_error,
            )),
            _ => None,
        })
        .collect()
}

async fn run(agent: &mut Agent, text: &str) {
    let (tx, mut rx) = mpsc::unbounded_channel();
    let drain = tokio::spawn(async move { while rx.recv().await.is_some() {} });
    agent.prompt_with_sender(text, tx).await;
    drain.await.unwrap();
}

#[tokio::test]
async fn a_source_whose_tools_change_is_seen_by_the_next_run() {
    let source = Swappable::default();
    source.set(vec![named("alpha", "alpha ran")]);
    let (provider, seen) = recording(vec![
        call("alpha"),
        MockResponse::Text("done".into()),
        // Second run: alpha is gone; a stale call to it must be an error
        // result, not a panic.
        call("alpha"),
        call("beta"),
        MockResponse::Text("done".into()),
    ]);
    let mut agent = Agent::from_provider(provider, ModelConfig::mock())
        .with_tools(vec![Box::new(Named {
            name: "own",
            reply: "own ran",
        })])
        .with_tool_source(source.clone());

    run(&mut agent, "first").await;
    let first = tool_results(agent.messages());
    assert_eq!(first, vec![("alpha".into(), "alpha ran".into(), false)]);

    source.set(vec![named("beta", "beta ran")]);
    let before = agent.messages().len();
    run(&mut agent, "second").await;
    let second = tool_results(&agent.messages()[before..]);
    assert_eq!(second.len(), 2);
    assert_eq!(second[0].0, "alpha");
    assert!(second[0].2, "a call to a withdrawn tool is an error result");
    assert!(second[0].1.contains("not found"), "{:?}", second[0]);
    assert_eq!(second[1], ("beta".into(), "beta ran".into(), false));

    let seen = seen.lock().unwrap();
    // Run 1: two requests offering own + alpha; run 2: three offering own + beta.
    assert_eq!(seen.len(), 5);
    for tools in &seen[..2] {
        assert_eq!(tools, &vec!["own".to_string(), "alpha".into()]);
    }
    for tools in &seen[2..] {
        assert_eq!(tools, &vec!["own".to_string(), "beta".into()]);
    }
}

#[tokio::test]
async fn a_source_is_consulted_once_per_run_not_per_turn() {
    let source = Swappable::default();
    source.set(vec![named("alpha", "alpha ran")]);
    let (provider, seen) = recording(vec![
        call("alpha"),
        call("alpha"),
        call("alpha"),
        MockResponse::Text("done".into()),
        MockResponse::Text("continued".into()),
    ]);
    let mut agent =
        Agent::from_provider(provider, ModelConfig::mock()).with_tool_source(source.clone());

    run(&mut agent, "go").await;
    assert_eq!(seen.lock().unwrap().len(), 4, "four requests in the run");
    assert_eq!(source.calls(), 1, "one run, one consultation");

    // A continue is a run too: it consults the source again, and a change
    // made in between is visible.
    source.set(vec![named("gamma", "gamma ran")]);
    agent.append_message(AgentMessage::Llm(Message::user("and now?")));
    let mut rx = agent.continue_loop().await;
    while rx.recv().await.is_some() {}
    agent.finish().await;
    assert_eq!(source.calls(), 2);
    assert_eq!(
        seen.lock().unwrap().last().unwrap(),
        &vec!["gamma".to_string()]
    );
}

#[tokio::test]
async fn the_receiver_returning_prompt_also_resolves_and_drops_sourced_tools() {
    let source = Swappable::default();
    source.set(vec![named("alpha", "alpha ran")]);
    let (provider, seen) = recording(vec![
        call("alpha"),
        MockResponse::Text("done".into()),
        MockResponse::Text("done again".into()),
    ]);
    let mut agent =
        Agent::from_provider(provider, ModelConfig::mock()).with_tool_source(source.clone());

    let mut rx = agent.prompt("one").await;
    while rx.recv().await.is_some() {}
    agent.finish().await;
    assert_eq!(
        tool_results(agent.messages()),
        vec![("alpha".into(), "alpha ran".into(), false)]
    );

    source.set(Vec::new());
    let mut rx = agent.prompt("two").await;
    while rx.recv().await.is_some() {}
    agent.finish().await;
    assert_eq!(
        seen.lock().unwrap().last().unwrap(),
        &Vec::<String>::new(),
        "the previous run's sourced tool must not have been kept"
    );
}

#[tokio::test]
async fn static_tools_win_a_collision_then_earlier_sources() {
    let first = Swappable::default();
    first.set(vec![
        named("own", "shadowed by the static tool"),
        named("shared", "from the first source"),
        named("shared", "duplicate within the first source"),
    ]);
    let second = Swappable::default();
    second.set(vec![
        named("shared", "from the second source"),
        named("extra", "extra ran"),
    ]);
    let (provider, seen) = recording(vec![
        MockResponse::ToolCalls(vec![
            MockToolCall {
                provider_metadata: None,
                name: "own".into(),
                arguments: serde_json::json!({}),
            },
            MockToolCall {
                provider_metadata: None,
                name: "shared".into(),
                arguments: serde_json::json!({}),
            },
        ]),
        MockResponse::Text("done".into()),
    ]);
    let mut agent = Agent::from_provider(provider, ModelConfig::mock())
        .with_tools(vec![Box::new(Named {
            name: "own",
            reply: "own ran",
        })])
        .with_tool_source(first)
        .with_tool_source(second);

    run(&mut agent, "go").await;
    assert_eq!(
        seen.lock().unwrap()[0],
        vec!["own".to_string(), "extra".into(), "shared".into()],
        "one tool per name: static first, then the surviving sourced tools by name"
    );
    assert_eq!(
        tool_results(agent.messages()),
        vec![
            ("own".into(), "own ran".into(), false),
            ("shared".into(), "from the first source".into(), false),
        ]
    );
}

#[tokio::test]
async fn a_panicking_source_contributes_nothing_and_the_run_proceeds() {
    let good = Swappable::default();
    good.set(vec![named("alpha", "alpha ran")]);
    let (provider, seen) = recording(vec![call("alpha"), MockResponse::Text("done".into())]);
    let mut agent = Agent::from_provider(provider, ModelConfig::mock())
        .with_tool_source(Panicking)
        .with_tool_source(good);

    run(&mut agent, "go").await;
    assert_eq!(seen.lock().unwrap()[0], vec!["alpha".to_string()]);
    assert_eq!(
        tool_results(agent.messages()),
        vec![("alpha".into(), "alpha ran".into(), false)]
    );
}

#[tokio::test]
async fn a_sub_agent_consults_its_source_per_delegation() {
    let source = Swappable::default();
    source.set(vec![named("alpha", "alpha ran")]);
    let (child, child_seen) = recording(vec![
        call("alpha"),
        MockResponse::Text("child done".into()),
        MockResponse::Text("child done again".into()),
    ]);
    let sub = SubAgentTool::from_provider("helper", Arc::new(child), ModelConfig::mock())
        .with_tool_source(source.clone());
    let delegate = || {
        MockResponse::ToolCalls(vec![MockToolCall {
            provider_metadata: None,
            name: "helper".into(),
            arguments: serde_json::json!({"task": "do it"}),
        }])
    };
    let parent = MockProvider::new(vec![
        delegate(),
        MockResponse::Text("parent done".into()),
        delegate(),
        MockResponse::Text("parent done".into()),
    ]);
    let mut agent = Agent::from_provider(parent, ModelConfig::mock()).with_sub_agent(sub);

    run(&mut agent, "first").await;
    source.set(Vec::new());
    run(&mut agent, "second").await;

    assert_eq!(source.calls(), 2, "one consultation per delegation");
    let child_seen = child_seen.lock().unwrap();
    assert_eq!(child_seen[0], vec!["alpha".to_string()]);
    assert_eq!(child_seen.last().unwrap(), &Vec::<String>::new());
}

/// Returns its tools in a different order on every call.
#[derive(Clone, Default)]
struct Rotating {
    calls: Arc<AtomicUsize>,
}

#[async_trait::async_trait]
impl ToolSource for Rotating {
    async fn tools(&self) -> Vec<Arc<dyn AgentTool>> {
        let n = self.calls.fetch_add(1, Ordering::SeqCst);
        let mut tools = vec![named("zeta", "z"), named("alpha", "a"), named("mid", "m")];
        tools.rotate_left(n % 3);
        if n % 2 == 1 {
            tools.reverse();
        }
        tools
    }
}

#[tokio::test]
async fn the_same_set_in_another_order_offers_an_identical_list() {
    let (provider, seen) = recording(vec![
        MockResponse::Text("1".into()),
        MockResponse::Text("2".into()),
        MockResponse::Text("3".into()),
    ]);
    let mut agent = Agent::from_provider(provider, ModelConfig::mock())
        .with_tools(vec![
            Box::new(Named {
                name: "own_b",
                reply: "",
            }),
            Box::new(Named {
                name: "own_a",
                reply: "",
            }),
        ])
        .with_tool_source(Rotating::default());
    for prompt in ["one", "two", "three"] {
        run(&mut agent, prompt).await;
    }
    let seen = seen.lock().unwrap();
    let expected: Vec<String> = ["own_b", "own_a", "alpha", "mid", "zeta"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    assert_eq!(seen.len(), 3);
    for tools in seen.iter() {
        assert_eq!(
            tools, &expected,
            "own tools keep their order; sourced ones are sorted"
        );
    }
}

/// Panics while *building* its future, before any `async` body runs.
struct PanicsBeforeTheFuture;

impl ToolSource for PanicsBeforeTheFuture {
    fn tools<'life0, 'async_trait>(
        &'life0 self,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Vec<Arc<dyn AgentTool>>> + Send + 'async_trait>,
    >
    where
        'life0: 'async_trait,
        Self: 'async_trait,
    {
        panic!("sync panic in tools()")
    }
}

#[tokio::test]
async fn a_source_panicking_before_returning_its_future_is_contained() {
    let good = Swappable::default();
    good.set(vec![named("alpha", "alpha ran")]);
    let (provider, seen) = recording(vec![MockResponse::Text("done".into())]);
    let mut agent = Agent::from_provider(provider, ModelConfig::mock())
        .with_tool_source(PanicsBeforeTheFuture)
        .with_tool_source(good);
    run(&mut agent, "go").await;
    assert_eq!(seen.lock().unwrap()[0], vec!["alpha".to_string()]);
}

#[tokio::test]
async fn continue_loop_with_sender_consults_the_source_and_drops_its_tools() {
    let source = Swappable::default();
    source.set(vec![named("alpha", "alpha ran")]);
    let (provider, seen) = recording(vec![
        MockResponse::Text("first".into()),
        call("alpha"),
        MockResponse::Text("continued".into()),
        MockResponse::Text("after".into()),
    ]);
    let mut agent = Agent::from_provider(provider, ModelConfig::mock())
        .with_tools(vec![Box::new(Named {
            name: "own",
            reply: "own ran",
        })])
        .with_tool_source(source.clone());
    run(&mut agent, "go").await;
    assert_eq!(source.calls(), 1);

    agent.append_message(AgentMessage::Llm(Message::user("more")));
    let (tx, mut rx) = mpsc::unbounded_channel();
    let drain = tokio::spawn(async move { while rx.recv().await.is_some() {} });
    agent.continue_loop_with_sender(tx).await;
    drain.await.unwrap();
    assert_eq!(source.calls(), 2, "a continue is a run: consulted again");
    assert_eq!(
        tool_results(agent.messages()).last().unwrap(),
        &("alpha".to_string(), "alpha ran".to_string(), false)
    );

    // Withdraw the tool: the next run offers only the agent's own tool, so
    // the continue's sourced tool was not kept.
    source.set(Vec::new());
    run(&mut agent, "again").await;
    assert_eq!(
        seen.lock().unwrap().last().unwrap(),
        &vec!["own".to_string()]
    );
}

#[tokio::test]
async fn a_run_after_continue_loop_does_not_keep_the_sourced_tools() {
    let source = Swappable::default();
    source.set(vec![named("alpha", "alpha ran")]);
    let (provider, seen) = recording(vec![
        MockResponse::Text("first".into()),
        MockResponse::Text("continued".into()),
        MockResponse::Text("after".into()),
    ]);
    let mut agent =
        Agent::from_provider(provider, ModelConfig::mock()).with_tool_source(source.clone());
    run(&mut agent, "go").await;
    agent.append_message(AgentMessage::Llm(Message::user("more")));
    let mut rx = agent.continue_loop().await;
    while rx.recv().await.is_some() {}
    agent.finish().await;

    source.set(Vec::new());
    run(&mut agent, "again").await;
    let seen = seen.lock().unwrap();
    assert_eq!(seen[1], vec!["alpha".to_string()]);
    assert_eq!(seen[2], Vec::<String>::new());
}

#[tokio::test]
async fn the_injected_shared_state_tool_wins_a_collision_and_survives() {
    let source = Swappable::default();
    source.set(vec![named("shared_state", "an impostor")]);
    let (provider, seen) = recording(vec![
        MockResponse::ToolCalls(vec![MockToolCall {
            provider_metadata: None,
            name: "shared_state".into(),
            arguments: serde_json::json!({"action": "list"}),
        }]),
        MockResponse::Text("done".into()),
        MockResponse::Text("second".into()),
    ]);
    let mut agent = Agent::from_provider(provider, ModelConfig::mock())
        .with_shared_state(SharedState::default())
        .with_tool_source(source.clone());
    run(&mut agent, "go").await;
    let results = tool_results(agent.messages());
    assert_eq!(results.len(), 1);
    assert_ne!(
        results[0].1, "an impostor",
        "the real shared_state tool ran"
    );
    assert!(!results[0].2);

    source.set(Vec::new());
    run(&mut agent, "again").await;
    let seen = seen.lock().unwrap();
    assert_eq!(seen[0], vec!["shared_state".to_string()]);
    assert_eq!(
        seen.last().unwrap(),
        &vec!["shared_state".to_string()],
        "the injected tool survives the cut-back, exactly once"
    );
}

/// Never answers.
struct Hung;

#[async_trait::async_trait]
impl ToolSource for Hung {
    async fn tools(&self) -> Vec<Arc<dyn AgentTool>> {
        std::future::pending().await
    }
}

#[tokio::test]
async fn a_parent_abort_interrupts_a_sub_agents_hung_source() {
    let (child, _) = recording(vec![MockResponse::Text("never".into())]);
    let sub = SubAgentTool::from_provider("helper", Arc::new(child), ModelConfig::mock())
        .with_tool_source(Hung);
    let cancel = tokio_util::sync::CancellationToken::new();
    let ctx = ToolContext::new("call-1", "helper").with_cancel(cancel.clone());
    let task =
        tokio::spawn(async move { sub.execute(serde_json::json!({"task": "x"}), ctx).await });
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    cancel.cancel();
    let outcome = tokio::time::timeout(std::time::Duration::from_secs(5), task)
        .await
        .expect("the abort unsticks the delegation")
        .unwrap();
    assert!(matches!(outcome, Err(ToolError::Cancelled)), "{outcome:?}");
}

// ---------------------------------------------------------------------------
// Generation: a source asked again between turns when it changed
// ---------------------------------------------------------------------------

type Tools = Vec<Arc<dyn AgentTool>>;

/// A source with a generation that moves when its tools change; counts how
/// often it is asked.
#[derive(Clone, Default)]
struct Generational {
    tools: Arc<Mutex<Tools>>,
    generation: Arc<std::sync::atomic::AtomicU64>,
    calls: Arc<AtomicUsize>,
    panics: Arc<std::sync::atomic::AtomicBool>,
}

impl Generational {
    fn new(tools: Tools) -> Self {
        let source = Self::default();
        *source.tools.lock().unwrap() = tools;
        source
    }
    /// Replace the tools and move the generation, as a plugin load would.
    fn change(&self, tools: Tools) {
        *self.tools.lock().unwrap() = tools;
        self.generation.fetch_add(1, Ordering::SeqCst);
    }
    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

#[async_trait::async_trait]
impl ToolSource for Generational {
    async fn tools(&self) -> Vec<Arc<dyn AgentTool>> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        if self.panics.load(Ordering::SeqCst) {
            panic!("source exploded when asked again");
        }
        self.tools.lock().unwrap().clone()
    }
    fn generation(&self) -> u64 {
        self.generation.load(Ordering::SeqCst)
    }
}

/// A static tool `load`: when called, it runs `then` (changing a source).
struct Load(Mutex<Option<Box<dyn FnOnce() + Send>>>);

fn load(then: impl FnOnce() + Send + 'static) -> Box<dyn AgentTool> {
    Box::new(Load(Mutex::new(Some(Box::new(then)))))
}

#[async_trait::async_trait]
impl AgentTool for Load {
    fn name(&self) -> &str {
        "load"
    }
    fn label(&self) -> &str {
        "load"
    }
    fn description(&self) -> &str {
        "loads more tools"
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({"type": "object", "properties": {}})
    }
    async fn execute(&self, _: serde_json::Value, _: ToolContext) -> Result<ToolResult, ToolError> {
        if let Some(then) = self.0.lock().unwrap().take() {
            then();
        }
        Ok(ToolResult {
            content: vec![Content::Text {
                text: "loaded".into(),
            }],
            details: serde_json::Value::Null,
        })
    }
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

/// A tool loaded mid-run is offered from the next request — after every call
/// of the turn that loaded it has returned — and only the source that changed
/// is asked again; a source whose generation stays 0 is asked once per run.
#[tokio::test]
async fn a_source_that_changes_mid_run_is_asked_again_at_the_next_turn() {
    let source = Generational::new(vec![named("alpha", "alpha ran")]);
    let steady = Swappable::default();
    steady.set(vec![named("steady", "steady ran")]);
    let changes = source.clone();
    let (provider, seen) = recording(vec![
        // One turn: `load` and `alpha` together; `beta` exists only after.
        calls(&["load", "alpha"]),
        call("beta"),
        MockResponse::Text("done".into()),
        MockResponse::Text("again".into()),
    ]);
    let mut agent = Agent::from_provider(provider, ModelConfig::mock())
        .with_tools(vec![load(move || {
            changes.change(vec![named("alpha", "alpha ran"), named("beta", "beta ran")])
        })])
        .with_tool_source(source.clone())
        .with_tool_source(steady.clone());
    run(&mut agent, "go").await;

    assert_eq!(
        tool_results(agent.messages()),
        vec![
            ("load".into(), "loaded".into(), false),
            ("alpha".into(), "alpha ran".into(), false),
            ("beta".into(), "beta ran".into(), false),
        ]
    );
    {
        let seen = seen.lock().unwrap();
        assert_eq!(seen[0], vec!["load", "alpha", "steady"], "{seen:?}");
        assert_eq!(seen[1], vec!["load", "alpha", "beta", "steady"], "{seen:?}");
        assert_eq!(seen[2], seen[1]);
    }
    assert_eq!(
        source.calls(),
        2,
        "asked at the start, then once after it changed"
    );
    assert_eq!(
        steady.calls(),
        1,
        "a source whose generation stays 0 is asked once"
    );

    // The next run starts from the source's current list, asked once more.
    run(&mut agent, "again").await;
    assert_eq!(
        seen.lock().unwrap()[3],
        vec!["load", "alpha", "beta", "steady"]
    );
    assert_eq!(source.calls(), 3);
    assert_eq!(steady.calls(), 2);
}

/// A source's new list replaces its old one even when shorter (a server that
/// went away): a later call to a withdrawn tool is answered as an unknown tool.
#[tokio::test]
async fn a_shorter_list_withdraws_tools_mid_run() {
    let source = Generational::new(vec![named("alpha", "alpha ran"), named("beta", "beta ran")]);
    let changes = source.clone();
    let (provider, seen) = recording(vec![
        call("load"),
        call("beta"),
        MockResponse::Text("done".into()),
    ]);
    let mut agent = Agent::from_provider(provider, ModelConfig::mock())
        .with_tools(vec![load(move || {
            changes.change(vec![named("alpha", "alpha ran")])
        })])
        .with_tool_source(source.clone());
    run(&mut agent, "go").await;
    let seen = seen.lock().unwrap().clone();
    assert_eq!(seen[0], vec!["load", "alpha", "beta"]);
    assert_eq!(seen[1], vec!["load", "alpha"], "{seen:?}");
    let results = tool_results(agent.messages());
    assert!(
        results[1].2 && results[1].1.contains("not found"),
        "{results:?}"
    );
}

/// The collision rules are applied again over the whole list: a changed source
/// cannot shadow the agent's own tool, nor an earlier source's.
#[tokio::test]
async fn collisions_are_resolved_again_after_a_change() {
    let first = Swappable::default();
    first.set(vec![named("shared", "first's shared")]);
    let source = Generational::new(vec![]);
    let changes = source.clone();
    let (provider, seen) = recording(vec![
        call("load"),
        calls(&["load", "shared"]),
        MockResponse::Text("done".into()),
    ]);
    let mut agent = Agent::from_provider(provider, ModelConfig::mock())
        .with_tools(vec![load(move || {
            changes.change(vec![
                named("load", "an impostor"),
                named("shared", "an impostor"),
                named("gamma", "gamma ran"),
            ])
        })])
        .with_tool_source(first)
        .with_tool_source(source);
    run(&mut agent, "go").await;
    let seen = seen.lock().unwrap().clone();
    // Sourced tools are sorted by name; each name appears once.
    assert_eq!(seen[1], vec!["load", "gamma", "shared"], "{seen:?}");
    let results = tool_results(agent.messages());
    // The agent's own `load` and the first source's `shared` answer.
    assert_eq!(results[1], ("load".into(), "loaded".into(), false));
    assert_eq!(
        results[2],
        ("shared".into(), "first's shared".into(), false)
    );
}

/// Extensions' per-run tools are not asked again mid-run, and stay offered
/// after the sources' tools are rebuilt.
#[tokio::test]
async fn extension_tools_are_not_asked_again_mid_run() {
    use yoagent::extension::*;

    struct Offers(Arc<AtomicUsize>);
    #[async_trait::async_trait]
    impl RunHooks for Offers {
        async fn tools(&mut self, _: &RunContext<'_>) -> Vec<Arc<dyn AgentTool>> {
            self.0.fetch_add(1, Ordering::SeqCst);
            vec![
                named("ext_tool", "ext ran"),
                named("delta", "the extension's delta"),
            ]
        }
    }
    struct Ext(Arc<AtomicUsize>);
    #[async_trait::async_trait]
    impl Extension for Ext {
        fn name(&self) -> &str {
            "offers"
        }
        async fn start_run(&self, _: &RunContext<'_>) -> Result<Box<dyn RunHooks>, ExtensionError> {
            Ok(Box::new(Offers(self.0.clone())))
        }
    }

    let asked = Arc::new(AtomicUsize::new(0));
    let source = Generational::new(vec![]);
    let changes = source.clone();
    let (provider, seen) = recording(vec![
        call("load"),
        call("delta"),
        MockResponse::Text("done".into()),
    ]);
    let mut agent = Agent::from_provider(provider, ModelConfig::mock())
        .with_tools(vec![load(move || {
            changes.change(vec![named("delta", "the source's delta")])
        })])
        .with_tool_source(source)
        .with_extension(Ext(asked.clone()));
    run(&mut agent, "go").await;
    let seen = seen.lock().unwrap().clone();
    assert_eq!(seen[0], vec!["load", "delta", "ext_tool"], "{seen:?}");
    assert_eq!(seen[1], vec!["load", "delta", "ext_tool"], "{seen:?}");
    assert_eq!(asked.load(Ordering::SeqCst), 1, "per-run tools: asked once");
    // A source comes before the extensions: its `delta` wins after the change.
    let results = tool_results(agent.messages());
    assert_eq!(
        results[1],
        ("delta".into(), "the source's delta".into(), false)
    );
}

/// A source that panics when asked again gives no tools from then on, and is
/// not asked again until its generation moves once more.
#[tokio::test]
async fn a_source_that_panics_when_asked_again_gives_nothing() {
    let source = Generational::new(vec![named("alpha", "alpha ran")]);
    let changes = source.clone();
    let (provider, seen) = recording(vec![
        call("load"),
        call("load"),
        MockResponse::Text("done".into()),
    ]);
    let mut agent = Agent::from_provider(provider, ModelConfig::mock())
        .with_tools(vec![load(move || {
            changes.panics.store(true, Ordering::SeqCst);
            changes.change(vec![]);
        })])
        .with_tool_source(source.clone());
    run(&mut agent, "go").await;
    let seen = seen.lock().unwrap().clone();
    assert_eq!(seen[1], vec!["load"], "{seen:?}");
    assert_eq!(seen[2], vec!["load"], "{seen:?}");
    assert_eq!(
        source.calls(),
        2,
        "asked at the start and once after the change"
    );
}
