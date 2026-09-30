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
        vec!["own".to_string(), "shared".into(), "extra".into()],
        "one tool per name, static first, then sources in order"
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
